//! Native VLESS client (VLESS over TLS, with the WebSocket or raw TCP
//! transport).
//!
//! VLESS itself does no encryption — confidentiality comes from the TLS layer —
//! so the client half is just a request header followed by the raw payload. We
//! support the common `security=tls` setups (`type=tcp` and `type=ws`); Reality
//! and XTLS flows are rejected with a clear message, since they need their own
//! TLS ClientHello handling.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{client_async, MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The transport used to carry the VLESS stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Transport {
    /// WebSocket with the given path and (optional) Host override.
    Ws { path: String, host: String },
    /// Raw TCP over TLS.
    Tcp,
}

/// A parsed `vless://` endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VlessConfig {
    pub(crate) uuid: [u8; 16],
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) transport: Transport,
    pub(crate) tls: bool,
    pub(crate) sni: String,
}

/// Parses a UUID string into its 16 raw bytes.
fn parse_uuid(s: &str) -> Result<[u8; 16], String> {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return Err("vless: неверный uuid".into());
    }
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|_| "vless: uuid не hex".to_string())?;
    }
    Ok(out)
}

/// Parses a `vless://` URL (`security=tls`, transport `tcp` or `ws`).
pub(crate) fn parse_url(url: &str) -> Result<VlessConfig, String> {
    let trimmed = url.trim();
    let rest = trimmed.strip_prefix("vless://").ok_or("vless: не ссылка vless://")?;
    let rest = rest.split('#').next().unwrap_or(rest);
    let (authority, query) = match rest.split_once('?') {
        Some((a, q)) => (a, q),
        None => (rest, ""),
    };
    let (uuid_s, hostport) = authority.rsplit_once('@').ok_or("vless: нет uuid@host")?;
    let (host, port_s) = hostport.rsplit_once(':').ok_or("vless: нет порта")?;
    let port: u16 = port_s.parse().map_err(|_| "vless: неверный порт")?;
    if host.is_empty() {
        return Err("vless: пустой host".into());
    }
    let uuid = parse_uuid(uuid_s)?;

    let mut params: HashMap<String, String> = HashMap::new();
    for kv in query.split('&') {
        if kv.is_empty() {
            continue;
        }
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        params.insert(k.to_string(), crate::ss::percent_decode(v));
    }
    let get = |k: &str| params.get(k).map(String::as_str).unwrap_or("");

    let tls = match get("security") {
        "" | "none" => false,
        "tls" => true,
        "reality" => return Err("vless: Reality пока не поддержан".into()),
        other => return Err(format!("vless: security '{other}' не поддержан")),
    };
    let flow = get("flow");
    if !flow.is_empty() && flow != "none" {
        return Err(format!("vless: flow '{flow}' не поддержан"));
    }
    let sni = match get("sni") {
        "" => host.to_string(),
        s => s.to_string(),
    };
    let ws_host = get("host").to_string();
    let path = match get("path") {
        "" => "/".to_string(),
        p => p.to_string(),
    };
    let transport = match get("type") {
        "" | "tcp" => Transport::Tcp,
        "ws" => Transport::Ws { path, host: ws_host },
        other => return Err(format!("vless: type '{other}' не поддержан")),
    };
    Ok(VlessConfig { uuid, host: host.to_string(), port, transport, tls, sni })
}

/// Builds the VLESS request header for a SOCKS5-encoded `target`.
fn build_request(cfg: &VlessConfig, target: &[u8]) -> io::Result<Vec<u8>> {
    let bad = || io::Error::new(io::ErrorKind::InvalidInput, "vless: bad target");
    if target.len() < 7 {
        return Err(bad());
    }
    let tail = target.len() - 2;
    let mut h = Vec::with_capacity(target.len() + 20);
    h.push(0); // protocol version
    h.extend_from_slice(&cfg.uuid);
    h.push(0); // addon length (none)
    h.push(1); // command: TCP
    h.extend_from_slice(&target[tail..]); // destination port (big-endian)
    match target[0] {
        1 => {
            h.push(1); // IPv4
            h.extend_from_slice(&target[1..5]);
        }
        4 => {
            h.push(3); // IPv6
            h.extend_from_slice(&target[1..17]);
        }
        3 => {
            h.push(2); // domain (length-prefixed, already encoded)
            h.extend_from_slice(&target[1..tail]);
        }
        _ => return Err(bad()),
    }
    Ok(h)
}

/// Rewrites a SOCKS5-encoded domain target to an IP target, resolving locally.
///
/// Some VLESS servers have no working outbound DNS, so a domain target hangs;
/// sending the resolved IP avoids that. On any failure the domain is kept and
/// the server is left to resolve it.
async fn resolve_target_addr(target: &[u8]) -> Vec<u8> {
    if target.first() != Some(&3) || target.len() < 4 {
        return target.to_vec();
    }
    let len = target[1] as usize;
    if 2 + len + 2 != target.len() {
        return target.to_vec();
    }
    let Ok(host) = std::str::from_utf8(&target[2..2 + len]) else {
        return target.to_vec();
    };
    let port = u16::from_be_bytes([target[target.len() - 2], target[target.len() - 1]]);
    let Ok(addrs) = tokio::net::lookup_host((host, port)).await else {
        return target.to_vec();
    };
    let mut v4 = None;
    let mut v6 = None;
    for a in addrs {
        match a {
            SocketAddr::V4(_) if v4.is_none() => v4 = Some(a),
            SocketAddr::V6(_) if v6.is_none() => v6 = Some(a),
            _ => {}
        }
    }
    match v4.or(v6) {
        Some(SocketAddr::V4(sa)) => {
            let mut t = vec![1];
            t.extend_from_slice(&sa.ip().octets());
            t.extend_from_slice(&port.to_be_bytes());
            t
        }
        Some(SocketAddr::V6(sa)) => {
            let mut t = vec![4];
            t.extend_from_slice(&sa.ip().octets());
            t.extend_from_slice(&port.to_be_bytes());
            t
        }
        None => target.to_vec(),
    }
}

enum Inner {
    Ws(WsStream),
    Raw(MaybeTlsStream<TcpStream>),
}

/// An established VLESS tunnel; [`VlessTunnel::pump`] splices it with a client.
pub(crate) struct VlessTunnel {
    inner: Inner,
}

/// Opens a VLESS connection to `server` and sends the request for `target`
/// (a SOCKS5 address), returning the tunnel.
///
/// The server's 2-byte (plus addon) response header is read lazily by the pump:
/// many servers only send it once the target produces data, so awaiting it here
/// would deadlock the client handshake.
pub(crate) async fn connect(
    server: SocketAddr,
    cfg: &VlessConfig,
    target: &[u8],
) -> io::Result<VlessTunnel> {
    let tcp = TcpStream::connect(server).await?;
    let stream = if cfg.tls {
        let connector = tokio_native_tls::TlsConnector::from(
            native_tls::TlsConnector::new().map_err(other)?,
        );
        let tls = connector
            .connect(&cfg.sni, tcp)
            .await
            .map_err(other)?;
        MaybeTlsStream::NativeTls(tls)
    } else {
        MaybeTlsStream::Plain(tcp)
    };
    let target = resolve_target_addr(target).await;
    let header = build_request(cfg, &target)?;

    match &cfg.transport {
        Transport::Ws { path, host } => {
            let url = format!("wss://{}:{}{}", cfg.host, cfg.port, path);
            let mut req = url.as_str().into_client_request().map_err(other)?;
            if !host.is_empty() {
                req.headers_mut()
                    .insert("host", HeaderValue::from_str(host).map_err(other)?);
            }
            let (mut ws, _resp) = client_async(req, stream).await.map_err(other)?;
            ws.send(Message::Binary(header.into())).await.map_err(other)?;
            Ok(VlessTunnel { inner: Inner::Ws(ws) })
        }
        Transport::Tcp => {
            let mut stream = stream;
            stream.write_all(&header).await?;
            Ok(VlessTunnel { inner: Inner::Raw(stream) })
        }
    }
}

fn other<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}

/// Drops the leading VLESS response header (version byte, addon length, addon
/// bytes) from the server->client byte stream.
#[derive(Default)]
struct StripState {
    stage: u8,
    addon: usize,
    skipped: usize,
}

impl StripState {
    /// Discards the header prefix of `data` and returns how many leading bytes
    /// the caller must not forward.
    fn consume(&mut self, data: &[u8]) -> usize {
        let mut i = 0;
        while i < data.len() {
            match self.stage {
                0 => {
                    i += 1;
                    self.stage = 1;
                }
                1 => {
                    self.addon = data[i] as usize;
                    i += 1;
                    self.stage = if self.addon == 0 { 3 } else { 2 };
                }
                2 => {
                    let take = (self.addon - self.skipped).min(data.len() - i);
                    self.skipped += take;
                    i += take;
                    if self.skipped == self.addon {
                        self.stage = 3;
                    }
                }
                _ => break,
            }
        }
        i
    }
}

impl VlessTunnel {
    /// Splices `client` with the tunnel until either side closes.
    pub(crate) async fn pump(self, client: TcpStream) -> io::Result<()> {
        match self.inner {
            Inner::Ws(ws) => pump_ws(client, ws).await,
            Inner::Raw(stream) => pump_raw(client, stream).await,
        }
    }
}

async fn pump_ws(client: TcpStream, ws: WsStream) -> io::Result<()> {
    let (mut cr, mut cw) = tokio::io::split(client);
    let (mut sink, mut rd) = ws.split();
    let c2s = async {
        let mut buf = vec![0u8; 16384];
        loop {
            let n = cr.read(&mut buf).await?;
            if n == 0 {
                let _ = sink.close().await;
                return Ok::<(), io::Error>(());
            }
            sink.send(Message::Binary(buf[..n].to_vec().into()))
                .await
                .map_err(other)?;
        }
    };
    let s2c = async {
        let mut strip = StripState::default();
        loop {
            match rd.next().await {
                Some(Ok(Message::Binary(b))) => {
                    let n = strip.consume(b.as_ref());
                    if n < b.len() {
                        cw.write_all(&b[n..]).await?;
                    }
                }
                Some(Ok(Message::Text(t))) => {
                    let b = t.as_str().as_bytes();
                    let n = strip.consume(b);
                    if n < b.len() {
                        cw.write_all(&b[n..]).await?;
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(other(e)),
            }
        }
        Ok::<(), io::Error>(())
    };
    tokio::try_join!(c2s, s2c)?;
    Ok(())
}

async fn pump_raw(client: TcpStream, stream: MaybeTlsStream<TcpStream>) -> io::Result<()> {
    let (mut cr, mut cw) = tokio::io::split(client);
    let (mut sr, mut sw) = tokio::io::split(stream);
    let c2s = async {
        let mut buf = vec![0u8; 16384];
        loop {
            let n = cr.read(&mut buf).await?;
            if n == 0 {
                sw.shutdown().await?;
                return Ok::<(), io::Error>(());
            }
            sw.write_all(&buf[..n]).await?;
        }
    };
    let s2c = async {
        let mut strip = StripState::default();
        let mut buf = vec![0u8; 16384];
        loop {
            let n = sr.read(&mut buf).await?;
            if n == 0 {
                return Ok::<(), io::Error>(());
            }
            let c = strip.consume(&buf[..n]);
            if c < n {
                cw.write_all(&buf[c..n]).await?;
            }
        }
    };
    tokio::try_join!(c2s, s2c)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link() -> &'static str {
        "vless://e579c385-15fd-4900-8e6c-e0a55fb14d19@uk5.taskflowhub.org:8443?alpn=h2%2Chttp%2F1.1&encryption=none&fp=chrome&host=&path=%2Frealtime&security=tls&sni=&type=ws#tag"
    }

    #[test]
    fn parses_tls_ws_link() {
        let c = parse_url(link()).unwrap();
        assert_eq!(c.host, "uk5.taskflowhub.org");
        assert_eq!(c.port, 8443);
        assert!(c.tls);
        assert_eq!(c.sni, "uk5.taskflowhub.org");
        assert_eq!(c.transport, Transport::Ws { path: "/realtime".into(), host: String::new() });
        assert_eq!(c.uuid[0], 0xe5);
        assert_eq!(c.uuid[15], 0x19);
    }

    #[test]
    fn parses_tcp_without_tls() {
        let c = parse_url("vless://e579c385-15fd-4900-8e6c-e0a55fb14d19@1.2.3.4:443?type=tcp&security=none").unwrap();
        assert!(!c.tls);
        assert_eq!(c.transport, Transport::Tcp);
    }

    #[test]
    fn rejects_reality_and_unknown_transport() {
        assert!(parse_url("vless://e579c385-15fd-4900-8e6c-e0a55fb14d19@h:443?security=reality").is_err());
        assert!(parse_url("vless://e579c385-15fd-4900-8e6c-e0a55fb14d19@h:443?type=grpc").is_err());
    }

    #[test]
    fn builds_domain_request_header() {
        let cfg = VlessConfig {
            uuid: [0xAB; 16],
            host: "h".into(),
            port: 443,
            transport: Transport::Tcp,
            tls: true,
            sni: "h".into(),
        };
        // SOCKS5 target: domain "example.com" (11), port 80.
        let mut target = vec![3, 11];
        target.extend_from_slice(b"example.com");
        target.extend_from_slice(&80u16.to_be_bytes());
        let h = build_request(&cfg, &target).unwrap();
        assert_eq!(h[0], 0);
        assert_eq!(&h[1..17], &[0xAB; 16]);
        assert_eq!(h[17], 0); // addon len
        assert_eq!(h[18], 1); // command TCP
        assert_eq!(&h[19..21], &80u16.to_be_bytes()); // port first
        assert_eq!(h[21], 2); // domain
        assert_eq!(h[22], 11); // domain length
        assert_eq!(&h[23..34], b"example.com");
    }

    /// End-to-end check against a real server: run with
    /// `WYVERN_VLESS_URL='vless://...' cargo test live_vless_proxy_smoke -- --ignored`.
    #[test]
    #[ignore = "needs network and a real vless:// server via WYVERN_VLESS_URL"]
    fn live_vless_proxy_smoke() {
        let Ok(url) = std::env::var("WYVERN_VLESS_URL") else {
            return;
        };
        let proxy = crate::proxybridge::resolve_proxy(&url);
        assert!(proxy.starts_with("socks5h://"), "bridge not started: {proxy}");
        let client = reqwest::blocking::Client::builder()
            .proxy(reqwest::Proxy::all(&proxy).unwrap())
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(25))
            .build()
            .unwrap();
        // `one.one.one.one` resolves to 1.1.1.1, so this checks domain ->
        // local resolution -> tunnel as well as the IP path.
        let resp = client.get("http://one.one.one.one/").send().unwrap();
        assert!(
            resp.status().is_redirection() || resp.status().is_success(),
            "unexpected status: {}",
            resp.status()
        );
        crate::proxybridge::stop();
    }

    #[test]
    fn strips_response_header_lazily() {
        // Header split across two chunks, addon length 0.
        let mut s = StripState::default();
        assert_eq!(s.consume(&[0x00]), 1);
        assert_eq!(s.consume(&[0x00, b'H', b'I']), 1);
        assert_eq!(s.consume(b"data"), 0);
        // Non-zero addon: version, len=2, two addon bytes, then payload.
        let mut s = StripState::default();
        assert_eq!(s.consume(&[0x00, 0x02, 0xAA, 0xBB, b'X']), 4);
        // Header fully consumed in one chunk, payload kept.
        let mut s = StripState::default();
        assert_eq!(s.consume(&[0x00, 0x00, b'o', b'k']), 2);
    }
}
