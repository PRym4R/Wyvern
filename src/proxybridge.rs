//! Local SOCKS5 bridge that terminates an `ss://` or `vless://` proxy in-process.
//!
//! Everything else in the client (reqwest, the gateway socket) only knows how
//! to speak HTTP or SOCKS5 proxying. To reuse that, we run a tiny SOCKS5 server
//! on loopback and point the global proxy at it; each accepted connection is
//! forwarded through the matching native client ([`crate::ss`] or
//! [`crate::vless`]).
//!
//! The bridge is keyed by the source URL and lives for as long as that
//! configuration is active; changing the URL tears the old one down.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::ss;
use crate::vless::{self, VlessConfig};

/// Which native protocol backs the bridge.
enum Backend {
    Ss { server: SocketAddr, method: String, password: String },
    Vless { server: SocketAddr, cfg: VlessConfig },
}

/// A running bridge: its source URL, loopback port and owning runtime.
struct Bridge {
    source: String,
    port: u16,
    /// Dropping the runtime aborts the accept loop and all live tunnels.
    _rt: tokio::runtime::Runtime,
}

fn slot() -> &'static Mutex<Option<Bridge>> {
    static SLOT: OnceLock<Mutex<Option<Bridge>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Loopback SOCKS5 URL handed to the rest of the client.
fn local_url(port: u16) -> String {
    format!("socks5h://127.0.0.1:{port}")
}

fn has_scheme(url: &str, scheme: &str) -> bool {
    url.get(..scheme.len()).is_some_and(|s| s.eq_ignore_ascii_case(scheme))
}

/// Resolves `url` into the effective proxy URL to install globally.
///
/// * supported URLs (`ss://`, `vless://`) start/reuse a loopback bridge and
///   return its SOCKS5 URL;
/// * anything else is returned unchanged (stopping any bridge).
///
/// On failure the original URL is returned so the mistake stays visible rather
/// than silently falling back to a direct connection.
pub(crate) fn resolve_proxy(url: &str) -> String {
    let trimmed = url.trim();
    if !has_scheme(trimmed, "ss://") && !has_scheme(trimmed, "vless://") {
        stop();
        return trimmed.to_string();
    }
    let mut guard = slot().lock().unwrap();
    if let Some(b) = guard.as_ref() {
        if b.source == trimmed {
            return local_url(b.port);
        }
    }
    // Replace any previous bridge (drops its runtime/task).
    *guard = None;
    match start(trimmed) {
        Ok(b) => {
            let url = local_url(b.port);
            *guard = Some(b);
            url
        }
        Err(e) => {
            eprintln!("proxy: не удалось поднять мост: {e}");
            trimmed.to_string()
        }
    }
}

/// Stops the active bridge, if any.
pub(crate) fn stop() {
    let mut guard = slot().lock().unwrap();
    *guard = None;
}

fn start(url: &str) -> Result<Bridge, String> {
    let backend = if has_scheme(url, "ss://") {
        let s = ss::parse_url(url)?;
        let server = resolve_blocking(&s.host, s.port)?;
        Backend::Ss { server, method: s.method, password: s.password }
    } else {
        let cfg = vless::parse_url(url)?;
        let server = resolve_blocking(&cfg.host, cfg.port)?;
        Backend::Vless { server, cfg }
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    let listener = rt
        .block_on(async { TcpListener::bind("127.0.0.1:0").await })
        .map_err(|e| format!("bind: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("local_addr: {e}"))?
        .port();
    rt.spawn(accept_loop(listener, Arc::new(backend)));
    Ok(Bridge { source: url.to_string(), port, _rt: rt })
}

/// Resolves the proxy server host to a socket address using a throwaway runtime.
fn resolve_blocking(host: &str, port: u16) -> Result<SocketAddr, String> {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(resolve_target(host, port))
}

/// Resolves a proxy server host to a socket address.
async fn resolve_target(host: &str, port: u16) -> Result<SocketAddr, String> {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    let mut addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("dns {host}: {e}"))?;
    addrs.next().ok_or_else(|| format!("dns {host}: пусто"))
}

async fn accept_loop(listener: TcpListener, backend: Arc<Backend>) {
    loop {
        let (client, _) = match listener.accept().await {
            Ok(v) => v,
            Err(_) => continue,
        };
        let backend = backend.clone();
        tokio::spawn(async move {
            let _ = handle(client, backend).await;
        });
    }
}

/// SOCKS5 reply with the given reply code and a zero BND.ADDR (IPv4).
async fn reply(client: &mut TcpStream, code: u8) -> std::io::Result<()> {
    client.write_all(&[0x05, code, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await
}

/// An established tunnel ready to be spliced with the client.
enum Established {
    Ss(ss::SsReader, ss::SsWriter),
    Vless(vless::VlessTunnel),
}

impl Established {
    async fn pump(self, client: TcpStream) -> std::io::Result<()> {
        match self {
            Established::Ss(reader, writer) => ss_pump(client, reader, writer).await,
            Established::Vless(tunnel) => tunnel.pump(client).await,
        }
    }
}

/// Handles one SOCKS5 client: greeting, CONNECT, then bidirectional pump.
async fn handle(mut client: TcpStream, backend: Arc<Backend>) -> std::io::Result<()> {
    let ver = client.read_u8().await?;
    let nmethods = client.read_u8().await? as usize;
    let mut methods = vec![0u8; nmethods];
    client.read_exact(&mut methods).await?;
    if ver != 0x05 || !methods.contains(&0x00) {
        client.write_all(&[0x05, 0xFF]).await?;
        return Ok(());
    }
    client.write_all(&[0x05, 0x00]).await?;

    let mut head = [0u8; 4];
    client.read_exact(&mut head).await?;
    if head[0] != 0x05 || head[1] != 0x01 {
        // Only CONNECT is meaningful for a TCP proxy.
        return reply(&mut client, 0x07).await;
    }
    let target = match read_target(&mut client, head[3]).await {
        Ok(t) => t,
        Err(_) => return reply(&mut client, 0x08).await,
    };

    let established = match &*backend {
        Backend::Ss { server, method, password } => ss::connect(*server, method, password, &target)
            .await
            .map(|(r, w)| Established::Ss(r, w)),
        Backend::Vless { server, cfg } => vless::connect(*server, cfg, &target)
            .await
            .map(Established::Vless),
    };
    match established {
        Ok(e) => {
            reply(&mut client, 0x00).await?;
            e.pump(client).await
        }
        Err(_) => reply(&mut client, 0x01).await,
    }
}

/// Pumps a Shadowsocks tunnel against the client.
async fn ss_pump(
    client: TcpStream,
    mut reader: ss::SsReader,
    mut writer: ss::SsWriter,
) -> std::io::Result<()> {
    let (mut cr, mut cw) = tokio::io::split(client);
    let c2s = async {
        let mut buf = vec![0u8; 16384];
        loop {
            let n = cr.read(&mut buf).await?;
            if n == 0 {
                writer.shutdown().await?;
                return Ok::<(), std::io::Error>(());
            }
            writer.write_data(&buf[..n]).await?;
        }
    };
    let s2c = async {
        reader.init().await?;
        let mut buf = vec![0u8; 16384];
        loop {
            let n = reader.read_data(&mut buf).await?;
            if n == 0 {
                return Ok::<(), std::io::Error>(());
            }
            cw.write_all(&buf[..n]).await?;
        }
    };
    let _ = tokio::try_join!(c2s, s2c);
    Ok(())
}

/// Reads a SOCKS5 address into the wire encoding the protocols expect as target.
async fn read_target(client: &mut TcpStream, atyp: u8) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(259);
    out.push(atyp);
    match atyp {
        0x01 => {
            let mut b = [0u8; 4];
            client.read_exact(&mut b).await?;
            out.extend_from_slice(&b);
        }
        0x04 => {
            let mut b = [0u8; 16];
            client.read_exact(&mut b).await?;
            out.extend_from_slice(&b);
        }
        0x03 => {
            let len = client.read_u8().await? as usize;
            out.push(len as u8);
            let mut b = vec![0u8; len];
            client.read_exact(&mut b).await?;
            out.extend_from_slice(&b);
        }
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "socks5: bad atyp",
            ))
        }
    }
    let mut port = [0u8; 2];
    client.read_exact(&mut port).await?;
    out.extend_from_slice(&port);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kept as one test so the shared global bridge is exercised sequentially.
    #[test]
    fn resolve_proxy_dispatches_backends() {
        // Plain proxies pass through untouched.
        assert_eq!(resolve_proxy(""), "");
        assert_eq!(resolve_proxy("http://127.0.0.1:8080"), "http://127.0.0.1:8080");
        assert_eq!(resolve_proxy("socks5://1.2.3.4:1080"), "socks5://1.2.3.4:1080");

        // Unsupported method/type: bridge can't start, URL is returned as-is.
        let bad_ss = "ss://rc4-md5:pw@127.0.0.1:1";
        assert_eq!(resolve_proxy(bad_ss), bad_ss);
        let bad_vless = "vless://e579c385-15fd-4900-8e6c-e0a55fb14d19@127.0.0.1:1?security=reality";
        assert_eq!(resolve_proxy(bad_vless), bad_vless);

        // Shadowsocks bridge starts and is reused for the same URL.
        let ss = "ss://chacha20-ietf-poly1305:pw@127.0.0.1:1";
        let ss_port = resolve_proxy(ss);
        assert!(ss_port.starts_with("socks5h://127.0.0.1:"), "got {ss_port}");
        assert_eq!(resolve_proxy(ss), ss_port);

        // VLESS bridge starts and is reused for the same URL.
        let vless = "vless://e579c385-15fd-4900-8e6c-e0a55fb14d19@127.0.0.1:1?type=ws&security=tls&path=/x";
        let vless_port = resolve_proxy(vless);
        assert!(vless_port.starts_with("socks5h://127.0.0.1:"), "got {vless_port}");
        assert_eq!(resolve_proxy(vless), vless_port);

        // Switching away stops any bridge.
        assert_eq!(resolve_proxy("http://1.2.3.4:8080"), "http://1.2.3.4:8080");
    }
}
