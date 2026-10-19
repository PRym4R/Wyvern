//! Native Shadowsocks (SIP002 / legacy AEAD) client.
//!
//! We implement the client half of the Shadowsocks AEAD protocol in Rust so no
//! external core binary is needed: a `ss://` proxy is terminated locally by
//! [`crate::ssbridge`], which speaks SOCKS5 to the rest of the app and forwards
//! each connection through [`connect`] here.
//!
//! Supported ciphers (the ones free providers actually hand out):
//! `aes-128-gcm`, `aes-256-gcm`, `chacha20-ietf-poly1305`.

use std::io;
use std::net::SocketAddr;

use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::{Aes128Gcm, Aes256Gcm};
use chacha20poly1305::ChaCha20Poly1305;
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;

/// Largest plaintext payload we put into one AEAD record (protocol limit).
const MAX_CHUNK: usize = 0x3FFF;
/// AEAD authentication tag length (all supported ciphers use 16 bytes).
const TAG_LEN: usize = 16;

/// A parsed `ss://` endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SsServer {
    pub(crate) method: String,
    pub(crate) password: String,
    pub(crate) host: String,
    pub(crate) port: u16,
}

/// Key/salt length for a cipher, or `None` if we don't support it.
pub(crate) fn key_len(method: &str) -> Option<usize> {
    Some(match method {
        "aes-128-gcm" => 16,
        "aes-256-gcm" | "chacha20-ietf-poly1305" => 32,
        _ => return None,
    })
}

/// Decodes base64 with any of the paddings/alphabets found in the wild.
fn b64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose as gp;
    use base64::Engine as _;
    gp::URL_SAFE_NO_PAD
        .decode(s)
        .ok()
        .or_else(|| gp::URL_SAFE.decode(s).ok())
        .or_else(|| gp::STANDARD_NO_PAD.decode(s).ok())
        .or_else(|| gp::STANDARD.decode(s).ok())
}

/// Percent-decodes `%XX` sequences; invalid escapes are kept verbatim.
fn percent_decode(s: &str) -> String {
    let hex = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    };
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Splits a SIP002 `userinfo` into `(method, password)`. The part is either
/// plain `method:password` or base64 of the same (`ss://base64(...)@host`).
fn split_userinfo(info: &str) -> (String, String) {
    if let Some((m, p)) = info.split_once(':') {
        return (percent_decode(m), percent_decode(p));
    }
    if let Some(dec) = b64_decode(info) {
        let dec = String::from_utf8_lossy(&dec).into_owned();
        let (m, p) = dec.split_once(':').unwrap_or((dec.as_str(), ""));
        return (m.to_string(), p.to_string());
    }
    (info.to_string(), String::new())
}

/// Parses a `ss://` URL in SIP002 (`…@host:port`) or legacy whole-base64 form.
pub(crate) fn parse_url(url: &str) -> Result<SsServer, String> {
    let trimmed = url.trim();
    let rest = trimmed
        .strip_prefix("ss://")
        .or_else(|| trimmed.strip_prefix("SS://"))
        .ok_or("ss: не ссылка ss://")?;
    // Drop the tag (`#name`) and any plugin query (`?plugin=…`).
    let rest = rest.split('#').next().unwrap_or(rest);
    let rest = rest.split('?').next().unwrap_or(rest);

    let (method, password, hostport) = if let Some((userinfo, hp)) = rest.rsplit_once('@') {
        let (m, p) = split_userinfo(userinfo);
        (m, p, hp.to_string())
    } else {
        let decoded = b64_decode(rest).ok_or("ss: не удалось декодировать base64")?;
        let decoded = String::from_utf8(decoded).map_err(|_| "ss: base64 не UTF-8")?;
        let (userinfo, hp) = decoded.rsplit_once('@').ok_or("ss: в ссылке нет '@'")?;
        let (m, p) = split_userinfo(userinfo);
        (m, p, hp.to_string())
    };

    let (host, port) = hostport.rsplit_once(':').ok_or("ss: нет порта")?;
    let port: u16 = port.parse().map_err(|_| "ss: неверный порт")?;
    if host.is_empty() {
        return Err("ss: пустой host".into());
    }
    if method.is_empty() || key_len(&method).is_none() {
        return Err(format!("ss: неподдерживаемый метод '{method}'"));
    }
    Ok(SsServer { method, password, host: host.to_string(), port })
}

/// OpenSSL's `EVP_BytesToKey` with MD5, used by Shadowsocks to stretch the
/// password into the master key.
fn master_key(password: &str, key_len: usize) -> Vec<u8> {
    use md5::{Digest, Md5};
    let mut out = Vec::with_capacity(key_len + 16);
    let mut prev: Vec<u8> = Vec::new();
    while out.len() < key_len {
        let mut md = Md5::new();
        md.update(&prev);
        md.update(password.as_bytes());
        prev = md.finalize().to_vec();
        out.extend_from_slice(&prev);
    }
    out.truncate(key_len);
    out
}

/// Per-connection subkey: `HKDF-SHA1(master, salt, "ss-subkey")`.
fn subkey(master: &[u8], salt: &[u8], key_len: usize) -> Vec<u8> {
    use hkdf::Hkdf;
    use sha1::Sha1;
    let hk = Hkdf::<Sha1>::new(Some(salt), master);
    let mut okm = vec![0u8; key_len];
    hk.expand(b"ss-subkey", &mut okm).expect("hkdf expand fits");
    okm
}

/// Little-endian increment of a 12-byte AEAD nonce.
fn inc_nonce(nonce: &mut [u8; 12]) {
    for b in nonce.iter_mut() {
        *b = b.wrapping_add(1);
        if *b != 0 {
            break;
        }
    }
}

/// One AEAD cipher instance for a direction.
enum Cipher {
    Aes128(Box<Aes128Gcm>),
    Aes256(Box<Aes256Gcm>),
    Chacha(Box<ChaCha20Poly1305>),
}

impl Cipher {
    fn new(method: &str, key: &[u8]) -> Result<Self, String> {
        let bad = || format!("ss: неверная длина ключа для {method}");
        match method {
            "aes-128-gcm" => Aes128Gcm::new_from_slice(key)
                .map(|c| Cipher::Aes128(Box::new(c)))
                .map_err(|_| bad()),
            "aes-256-gcm" => Aes256Gcm::new_from_slice(key)
                .map(|c| Cipher::Aes256(Box::new(c)))
                .map_err(|_| bad()),
            "chacha20-ietf-poly1305" => ChaCha20Poly1305::new_from_slice(key)
                .map(|c| Cipher::Chacha(Box::new(c)))
                .map_err(|_| bad()),
            other => Err(format!("ss: неподдерживаемый метод '{other}'")),
        }
    }

    /// Encrypts `buf` in place, appending the 16-byte tag.
    fn encrypt(&self, nonce: &[u8; 12], buf: &mut Vec<u8>) -> io::Result<()> {
        let n = GenericArray::from_slice(nonce);
        let r = match self {
            Cipher::Aes128(c) => c.encrypt_in_place(n, b"", buf),
            Cipher::Aes256(c) => c.encrypt_in_place(n, b"", buf),
            Cipher::Chacha(c) => c.encrypt_in_place(n, b"", buf),
        };
        r.map_err(|_| io::Error::new(io::ErrorKind::Other, "ss: encrypt failed"))
    }

    /// Verifies and decrypts `buf` in place (tag is the trailing 16 bytes),
    /// truncating it to the plaintext.
    fn decrypt(&self, nonce: &[u8; 12], buf: &mut Vec<u8>) -> io::Result<()> {
        let n = GenericArray::from_slice(nonce);
        let r = match self {
            Cipher::Aes128(c) => c.decrypt_in_place(n, b"", buf),
            Cipher::Aes256(c) => c.decrypt_in_place(n, b"", buf),
            Cipher::Chacha(c) => c.decrypt_in_place(n, b"", buf),
        };
        r.map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "ss: decrypt failed"))
    }
}

/// Encrypts one `(length, payload)` record pair into `out`.
fn seal_record(cipher: &Cipher, nonce: &mut [u8; 12], payload: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
    let mut len = (payload.len() as u16).to_be_bytes().to_vec();
    cipher.encrypt(nonce, &mut len)?;
    inc_nonce(nonce);
    out.extend_from_slice(&len);
    let mut body = payload.to_vec();
    cipher.encrypt(nonce, &mut body)?;
    inc_nonce(nonce);
    out.extend_from_slice(&body);
    Ok(())
}

/// Write half of a Shadowsocks connection.
pub(crate) struct SsWriter {
    inner: WriteHalf<TcpStream>,
    cipher: Cipher,
    nonce: [u8; 12],
}

impl SsWriter {
    /// Encrypts `data` into records and writes them.
    pub(crate) async fn write_data(&mut self, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let mut out = Vec::with_capacity(data.len() + 64);
        for chunk in data.chunks(MAX_CHUNK) {
            seal_record(&self.cipher, &mut self.nonce, chunk, &mut out)?;
        }
        self.inner.write_all(&out).await
    }

    /// Half-closes the underlying TCP stream (Shadowsocks signals EOF with FIN).
    pub(crate) async fn shutdown(&mut self) -> io::Result<()> {
        self.inner.shutdown().await
    }
}

/// Read half of a Shadowsocks connection. Decrypts lazily so the server salt is
/// consumed on the first read, not at handshake time.
pub(crate) struct SsReader {
    inner: ReadHalf<TcpStream>,
    method: String,
    master: Vec<u8>,
    key_len: usize,
    cipher: Option<Cipher>,
    nonce: [u8; 12],
    body: Vec<u8>,
    pos: usize,
}

impl SsReader {
    /// Reads the server salt and derives the read subkey. Idempotent.
    pub(crate) async fn init(&mut self) -> io::Result<()> {
        if self.cipher.is_some() {
            return Ok(());
        }
        let mut salt = vec![0u8; self.key_len];
        self.inner.read_exact(&mut salt).await?;
        let sk = subkey(&self.master, &salt, self.key_len);
        let cipher = Cipher::new(&self.method, &sk).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        self.cipher = Some(cipher);
        Ok(())
    }

    /// Reads the next block of plaintext; `0` means the server closed the stream.
    pub(crate) async fn read_data(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos < self.body.len() {
            let n = (self.body.len() - self.pos).min(out.len());
            out[..n].copy_from_slice(&self.body[self.pos..self.pos + n]);
            self.pos += n;
            if self.pos == self.body.len() {
                self.body.clear();
                self.pos = 0;
            }
            return Ok(n);
        }
        self.init().await?;
        // Records carry a 2-byte length; an empty record is a no-op keep-alive.
        loop {
            let mut lenbuf = vec![0u8; 2 + TAG_LEN];
            match self.inner.read_exact(&mut lenbuf).await {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(0),
                Err(e) => return Err(e),
            }
            let cipher = self.cipher.as_ref().expect("cipher set by init");
            cipher.decrypt(&self.nonce, &mut lenbuf)?;
            inc_nonce(&mut self.nonce);
            let len = u16::from_be_bytes([lenbuf[0], lenbuf[1]]) as usize;
            if len == 0 {
                continue;
            }
            let mut payload = vec![0u8; len + TAG_LEN];
            self.inner.read_exact(&mut payload).await?;
            cipher.decrypt(&self.nonce, &mut payload)?;
            inc_nonce(&mut self.nonce);
            let n = payload.len().min(out.len());
            out[..n].copy_from_slice(&payload[..n]);
            if n < payload.len() {
                self.body = payload;
                self.pos = n;
            }
            return Ok(n);
        }
    }
}

/// Opens a Shadowsocks connection to `server` and sends `target` (a SOCKS5
/// address, as the protocol expects) as the first payload record.
pub(crate) async fn connect(
    server: SocketAddr,
    method: &str,
    password: &str,
    target: &[u8],
) -> io::Result<(SsReader, SsWriter)> {
    let key_len = key_len(method)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "ss: unsupported method"))?;
    let master = master_key(password, key_len);

    let mut stream = TcpStream::connect(server).await?;
    // Client salt + first record (the target address) go out before we split.
    let mut salt = vec![0u8; key_len];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut salt);
    stream.write_all(&salt).await?;
    let tx_key = subkey(&master, &salt, key_len);
    let tx_cipher = Cipher::new(method, &tx_key).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    let mut tx_nonce = [0u8; 12];
    let mut first = Vec::with_capacity(target.len() + 32);
    seal_record(&tx_cipher, &mut tx_nonce, target, &mut first)?;
    stream.write_all(&first).await?;

    let (rh, wh) = tokio::io::split(stream);
    let reader = SsReader {
        inner: rh,
        method: method.to_string(),
        master,
        key_len,
        cipher: None,
        nonce: [0u8; 12],
        body: Vec::new(),
        pos: 0,
    };
    let writer = SsWriter { inner: wh, cipher: tx_cipher, nonce: tx_nonce };
    Ok((reader, writer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sip002_with_base64_userinfo() {
        let url = "ss://Y2hhY2hhMjAtaWV0Zi1wb2x5MTMwNTo1MkpxN2hGQWdZeUxySHAxR20xb1ha@creativeboxfin.zanity.net:5223#Helsinki";
        let s = parse_url(url).unwrap();
        assert_eq!(s.method, "chacha20-ietf-poly1305");
        assert_eq!(s.password, "52Jq7hFAgYyLrHp1Gm1oXZ");
        assert_eq!(s.host, "creativeboxfin.zanity.net");
        assert_eq!(s.port, 5223);
    }

    #[test]
    fn parses_sip002_with_plain_userinfo_and_escapes() {
        let s = parse_url("ss://aes-256-gcm:p%40ss%3Aword@1.2.3.4:8388").unwrap();
        assert_eq!(s.method, "aes-256-gcm");
        assert_eq!(s.password, "p@ss:word");
        assert_eq!(s.host, "1.2.3.4");
        assert_eq!(s.port, 8388);
    }

    #[test]
    fn parses_legacy_whole_base64() {
        // base64 of "aes-128-gcm:secret@example.com:443"
        let url = "ss://YWVzLTEyOC1nY206c2VjcmV0QGV4YW1wbGUuY29tOjQ0Mw==";
        let s = parse_url(url).unwrap();
        assert_eq!(s.method, "aes-128-gcm");
        assert_eq!(s.password, "secret");
        assert_eq!(s.host, "example.com");
        assert_eq!(s.port, 443);
    }

    #[test]
    fn rejects_unsupported_method() {
        assert!(parse_url("ss://rc4-md5:pw@h:1").is_err());
        assert!(parse_url("http://127.0.0.1:8080").is_err());
    }

    #[tokio::test]
    async fn round_trips_through_a_local_echo() {
        // A fake Shadowsocks server built from the same primitives; verifies the
        // record framing and nonce handling end to end.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let kl = key_len("chacha20-ietf-poly1305").unwrap();
            let master = master_key("pw", kl);
            let mut csalt = vec![0u8; kl];
            tcp.read_exact(&mut csalt).await.unwrap();
            let ckey = subkey(&master, &csalt, kl);
            let ccipher = Cipher::new("chacha20-ietf-poly1305", &ckey).unwrap();
            let mut cnonce = [0u8; 12];
            // Read the target record, then one data record.
            let mut got = Vec::new();
            for _ in 0..2 {
                let mut lb = vec![0u8; 2 + TAG_LEN];
                tcp.read_exact(&mut lb).await.unwrap();
                ccipher.decrypt(&cnonce, &mut lb).unwrap();
                inc_nonce(&mut cnonce);
                let n = u16::from_be_bytes([lb[0], lb[1]]) as usize;
                let mut pb = vec![0u8; n + TAG_LEN];
                tcp.read_exact(&mut pb).await.unwrap();
                ccipher.decrypt(&cnonce, &mut pb).unwrap();
                inc_nonce(&mut cnonce);
                got.extend_from_slice(&pb);
            }
            // Echo back (excluding the address prefix) with our own salt.
            let ssalt = vec![7u8; kl];
            tcp.write_all(&ssalt).await.unwrap();
            let skey = subkey(&master, &ssalt, kl);
            let scipher = Cipher::new("chacha20-ietf-poly1305", &skey).unwrap();
            let mut snonce = [0u8; 12];
            let reply = &got[7..]; // skip the encoded 1.2.3.4:80 target (7 bytes)
            let mut out = Vec::new();
            seal_record(&scipher, &mut snonce, reply, &mut out).unwrap();
            tcp.write_all(&out).await.unwrap();
        });

        let target = [0x01, 1, 2, 3, 4, 0x00, 0x50]; // 1.2.3.4:80
        let (mut r, mut w) = connect(addr, "chacha20-ietf-poly1305", "pw", &target).await.unwrap();
        w.write_data(b"hello world").await.unwrap();
        let mut buf = vec![0u8; 64];
        let n = r.read_data(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"hello world");
        server.await.unwrap();
    }

    /// End-to-end check against a real server: run with
    /// `WYVERN_SS_URL='ss://...' cargo test live_shadowsocks_proxy_smoke -- --ignored`.
    #[test]
    #[ignore = "needs network and a real ss:// server via WYVERN_SS_URL"]
    fn live_shadowsocks_proxy_smoke() {
        let Ok(url) = std::env::var("WYVERN_SS_URL") else {
            return;
        };
        let proxy = crate::ssbridge::resolve_proxy(&url);
        assert!(proxy.starts_with("socks5h://"), "bridge not started: {proxy}");
        let client = reqwest::blocking::Client::builder()
            .proxy(reqwest::Proxy::all(&proxy).unwrap())
            .timeout(std::time::Duration::from_secs(25))
            .build()
            .unwrap();
        let body = client.get("http://example.com/").send().unwrap().text().unwrap();
        assert!(
            body.to_lowercase().contains("example"),
            "unexpected body: {:.120}",
            body
        );
        crate::ssbridge::stop();
    }
}
