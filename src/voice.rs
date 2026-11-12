//! Voice receive: the voice gateway handshake, UDP discovery, RTP decryption,
//! Opus decoding and playback. Transmit is a later step; this module only
//! listens.
//!
//! Flow: op 8 hello -> op 0 identify -> op 2 ready (ssrc/ip/port) -> UDP IP
//! discovery -> op 1 select protocol -> op 4 session description (secret key)
//! -> RTP audio, with op 3 heartbeats throughout.

use std::collections::HashMap;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use opus_decoder::OpusDecoder;
use serde_json::{json, Value};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::watch;
use tokio::time;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use xsalsa20poly1305::aead::{Aead, KeyInit};
use xsalsa20poly1305::{Nonce, XSalsa20Poly1305};

use crate::audio::Player;
use crate::gateway::EventTx;
use crate::messages::ToApp;

/// Everything the voice gateway needs from the main gateway's op 4 answer.
pub(crate) struct VoiceConnect {
    /// `VOICE_SERVER_UPDATE.endpoint`, host[:port] with no scheme.
    pub(crate) endpoint: String,
    /// `VOICE_SERVER_UPDATE.token`.
    pub(crate) token: String,
    /// `VOICE_STATE_UPDATE.session_id` for our own user.
    pub(crate) session_id: String,
    /// Guild id: `server_id` in the identify payload.
    pub(crate) guild_id: String,
    pub(crate) user_id: String,
}

type VoiceSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Runs one voice session until `stop` is set or the socket fails. Errors are
/// reported to the app, which shows them in the voice bar.
pub(crate) async fn run(conn: VoiceConnect, event_tx: EventTx, stop: watch::Receiver<bool>) {
    if let Err(reason) = run_inner(conn, &event_tx, stop).await {
        let _ = event_tx.send(ToApp::VoiceAudio {
            ready: false,
            reason: Some(reason),
        });
    }
}

async fn run_inner(
    conn: VoiceConnect,
    event_tx: &EventTx,
    mut stop: watch::Receiver<bool>,
) -> Result<(), String> {
    let url = voice_url(&conn.endpoint);
    let (mut ws, _) = connect_async(url.as_str())
        .await
        .map_err(|e| format!("voice gateway: {}", e))?;

    let hello = read_json(&mut ws).await?;
    if hello["op"].as_i64() != Some(8) {
        return Err(format!("voice hello: unexpected op {}", hello["op"]));
    }
    let interval = hello["d"]["heartbeat_interval"]
        .as_u64()
        .unwrap_or(13_750)
        .max(1_000);
    let identify = json!({
        "op": 0,
        "d": {
            "server_id": conn.guild_id,
            "user_id": conn.user_id,
            "session_id": conn.session_id,
            "token": conn.token,
            "video": false,
        }
    });
    send_json(&mut ws, &identify).await?;

    let mut heartbeat = time::interval(Duration::from_millis(interval));
    heartbeat.tick().await;
    let mut nonce: u64 = 0;
    let mut ready = None;
    // The server sends op 2 Ready with the UDP endpoint, then (after our
    // select protocol) op 4 with the secret key.
    while ready.is_none() {
        tokio::select! {
            _ = heartbeat.tick() => send_heartbeat(&mut ws, &mut nonce).await?,
            _ = stop.changed() => return Ok(()),
            msg = read_json(&mut ws) => {
                let v = msg?;
                match v["op"].as_i64() {
                    Some(2) => ready = Some(v["d"].clone()),
                    Some(4) => return Err("voice: session description before ready".into()),
                    Some(6) => {}
                    _ => {}
                }
            }
        }
    }
    let ready = ready.unwrap_or_default();
    let ssrc = ready["ssrc"]
        .as_u64()
        .ok_or("voice ready: no ssrc")? as u32;
    let address = ready["ip"].as_str().ok_or("voice ready: no ip")?;
    let port = ready["port"].as_u64().ok_or("voice ready: no port")? as u16;

    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| format!("voice udp bind: {}", e))?;
    socket
        .connect((address, port))
        .await
        .map_err(|e| format!("voice udp connect: {}", e))?;

    // IP discovery: the voice server needs our public address to send audio
    // back. Retry a few times; the first packet is sometimes lost.
    let mut public = None;
    let mut reply = [0u8; 256];
    for _ in 0..10 {
        socket
            .send(&discovery_packet(ssrc))
            .await
            .map_err(|e| format!("voice udp discovery: {}", e))?;
        match time::timeout(Duration::from_millis(500), socket.recv(&mut reply)).await {
            Ok(Ok(n)) => {
                if let Some(found) = parse_discovery_reply(&reply[..n]) {
                    public = Some(found);
                    break;
                }
            }
            Ok(Err(e)) => return Err(format!("voice udp discovery: {}", e)),
            // Timeout: resend.
            Err(_) => {}
        }
    }
    let (public_ip, public_port) = public.ok_or("voice udp discovery timed out")?;

    let select = json!({
        "op": 1,
        "d": {
            "protocol": "udp",
            "data": {
                "address": public_ip,
                "port": public_port,
                "mode": "xsalsa20_poly1305",
            }
        }
    });
    send_json(&mut ws, &select).await?;

    let key = loop {
        tokio::select! {
            _ = heartbeat.tick() => send_heartbeat(&mut ws, &mut nonce).await?,
            _ = stop.changed() => return Ok(()),
            msg = read_json(&mut ws) => {
                let v = msg?;
                match v["op"].as_i64() {
                    Some(4) => {
                        let mode = v["d"]["mode"].as_str().unwrap_or("");
                        if !mode.is_empty() && mode != "xsalsa20_poly1305" {
                            return Err(format!("voice: unsupported encryption mode {}", mode));
                        }
                        let bytes: Vec<u8> = v["d"]["secret_key"]
                            .as_array()
                            .map(|a| a.iter().filter_map(|x| x.as_u64().map(|n| n as u8)).collect())
                            .unwrap_or_default();
                        if bytes.len() != 32 {
                            return Err(format!("voice: secret key has {} bytes, need 32", bytes.len()));
                        }
                        break bytes;
                    }
                    Some(5) | Some(6) => {}
                    _ => {}
                }
            }
        }
    };

    // Audio output is opened only once the session is negotiated: a failed
    // handshake must not leave a stream running.
    let player = Player::start().map_err(|e| format!("audio output: {}", e))?;
    let _ = event_tx.send(ToApp::VoiceAudio {
        ready: true,
        reason: None,
    });

    receive_loop(&mut ws, &socket, &key, &player, event_tx, &mut stop, &mut heartbeat, &mut nonce).await
}

#[allow(clippy::too_many_arguments)]
async fn receive_loop(
    ws: &mut VoiceSocket,
    socket: &UdpSocket,
    key: &[u8],
    player: &Player,
    event_tx: &EventTx,
    stop: &mut watch::Receiver<bool>,
    heartbeat: &mut time::Interval,
    nonce: &mut u64,
) -> Result<(), String> {
    let mut decoders: HashMap<u32, OpusDecoder> = HashMap::new();
    let mut speakers: HashMap<u32, String> = HashMap::new();
    // Per-ssrc RTP timestamp anchor: (mixer frame, timestamp).
    let mut anchors: HashMap<u32, (i64, u32)> = HashMap::new();
    let mut pcm = vec![0i16; OpusDecoder::MAX_FRAME_SIZE_48K * 2];
    let mut udp_buf = [0u8; 2048];

    loop {
        tokio::select! {
            _ = heartbeat.tick() => send_heartbeat(ws, nonce).await?,
            _ = stop.changed() => return Ok(()),
            received = socket.recv(&mut udp_buf) => {
                let n = received.map_err(|e| format!("voice udp: {}", e))?;
                let packet = &udp_buf[..n];
                let Some(header) = parse_rtp(packet) else { continue };
                let Some(plain) = decrypt_rtp(&packet[..12], &packet[header.len..], key) else {
                    continue;
                };
                if plain.is_empty() {
                    continue;
                }
                let decoder = match decoders.entry(header.ssrc) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => match OpusDecoder::new(48_000, 2) {
                        Ok(d) => e.insert(d),
                        // A codec failure must not kill the session.
                        Err(_) => continue,
                    },
                };
                // 20 ms frame at 48 kHz; the buffer always has room for a
                // full 120 ms frame.
                let Ok(samples) = decoder.decode(&plain, &mut pcm, false) else {
                    continue;
                };
                let used = samples * 2;
                if used > 0 && used <= pcm.len() {
                    let start = anchor_frame(
                        &mut anchors,
                        player.position(),
                        header.ssrc,
                        header.timestamp,
                    );
                    player.push_at(start, &pcm[..used]);
                }
            }
            msg = read_json(ws) => {
                let v = msg?;
                match v["op"].as_i64() {
                    Some(5) => {
                        // Speaking state: the only place that maps an RTP
                        // ssrc to a user id. Some events omit the id, so the
                        // earlier mapping is the fallback.
                        let d = &v["d"];
                        let speaking = d["speaking"].as_u64().unwrap_or(0) != 0;
                        let ssrc = d["ssrc"].as_u64().map(|s| s as u32);
                        let mut user_id = d["user_id"].as_str().unwrap_or("").to_string();
                        if let (Some(ssrc), false) = (ssrc, user_id.is_empty()) {
                            speakers.insert(ssrc, user_id.clone());
                        } else if let Some(ssrc) = ssrc {
                            user_id = speakers.get(&ssrc).cloned().unwrap_or_default();
                        }
                        if !user_id.is_empty() {
                            let _ = event_tx.send(ToApp::VoiceSpeaking { user_id, speaking });
                        }
                    }
                    Some(6) => {}
                    _ => {}
                }
            }
        }
    }
}

/// Mixer frame for this packet's first sample, derived from its RTP timestamp.
/// The first packet of each ssrc is anchored 100 ms ahead of playback; later
/// packets keep that offset, which is what aligns two speakers in the ring.
pub(crate) fn anchor_frame(
    anchors: &mut HashMap<u32, (i64, u32)>,
    now: i64,
    ssrc: u32,
    timestamp: u32,
) -> i64 {
    let (base, base_ts) = match anchors.get(&ssrc).copied() {
        Some((base, base_ts)) => {
            let delta = timestamp.wrapping_sub(base_ts) as i32 as i64;
            let start = base + delta;
            // A long pause (or a counter reset) leaves the old anchor hours
            // away; join the current timeline instead of dropping forever.
            if start < now - crate::audio::RING_FRAMES as i64
                || start > now + crate::audio::RING_FRAMES as i64
            {
                (now + crate::audio::PREBUFFER_FRAMES as i64, timestamp)
            } else {
                (base, base_ts)
            }
        }
        None => (now + crate::audio::PREBUFFER_FRAMES as i64, timestamp),
    };
    anchors.insert(ssrc, (base, base_ts));
    base + timestamp.wrapping_sub(base_ts) as i32 as i64
}

async fn send_heartbeat(ws: &mut VoiceSocket, nonce: &mut u64) -> Result<(), String> {    *nonce = nonce.wrapping_add(1);
    send_json(ws, &json!({ "op": 3, "d": *nonce })).await
}

async fn send_json(ws: &mut VoiceSocket, payload: &Value) -> Result<(), String> {
    ws.send(WsMessage::Text(payload.to_string().into()))
        .await
        .map_err(|e| format!("voice gateway: {}", e))
}

async fn read_json(ws: &mut VoiceSocket) -> Result<Value, String> {
    loop {
        match ws.next().await {
            Some(Ok(WsMessage::Text(text))) => {
                return serde_json::from_str(&text).map_err(|e| format!("voice gateway: {}", e));
            }
            Some(Ok(WsMessage::Close(frame))) => {
                let code = frame.map(|f| u16::from(f.code)).unwrap_or(0);
                return Err(format!("voice gateway closed: {}", code));
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(format!("voice gateway: {}", e)),
            None => return Err("voice gateway closed".to_string()),
        }
    }
}

/// Voice gateway URL from `VOICE_SERVER_UPDATE.endpoint`, which carries no
/// scheme and usually no port.
pub(crate) fn voice_url(endpoint: &str) -> String {
    let trimmed = endpoint.trim().trim_end_matches('/');
    if trimmed.starts_with("wss://") || trimmed.starts_with("ws://") {
        format!("{}/?v=8", trimmed)
    } else {
        format!("wss://{}/?v=8", trimmed)
    }
}

/// UDP IP discovery request: type 1, payload length 70, our ssrc, padding.
pub(crate) fn discovery_packet(ssrc: u32) -> [u8; 74] {
    let mut packet = [0u8; 74];
    packet[0..2].copy_from_slice(&1u16.to_be_bytes());
    packet[2..4].copy_from_slice(&70u16.to_be_bytes());
    packet[4..8].copy_from_slice(&ssrc.to_be_bytes());
    packet
}

/// Parses the discovery answer: type 2, then our public address (null
/// terminated) and port.
pub(crate) fn parse_discovery_reply(buf: &[u8]) -> Option<(String, u16)> {
    if buf.len() < 74 {
        return None;
    }
    if u16::from_be_bytes([buf[0], buf[1]]) != 2 {
        return None;
    }
    let address_bytes = &buf[8..72];
    let end = address_bytes
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(address_bytes.len());
    let address = std::str::from_utf8(&address_bytes[..end]).ok()?;
    if address.is_empty() {
        return None;
    }
    let port = u16::from_be_bytes([buf[72], buf[73]]);
    Some((address.to_string(), port))
}

/// One parsed RTP packet header.
pub(crate) struct RtpHeader {
    /// Length of the header including CSRCs and the extension.
    pub(crate) len: usize,
    pub(crate) ssrc: u32,
    /// 48 kHz sample clock; places the packet on the mixer timeline.
    pub(crate) timestamp: u32,
}

/// Parses a voice RTP packet. Returns `None` for anything that is not a
/// version 2 packet with a full authentication tag present.
pub(crate) fn parse_rtp(packet: &[u8]) -> Option<RtpHeader> {
    if packet.len() < 12 || packet[0] >> 6 != 2 {
        return None;
    }
    let csrc_count = (packet[0] & 0x0f) as usize;
    let mut len = 12 + csrc_count * 4;
    if packet[0] & 0x10 != 0 {
        // One extension: 16-bit profile, 16-bit length in 32-bit words.
        if packet.len() < len + 4 {
            return None;
        }
        let words = u16::from_be_bytes([packet[len + 2], packet[len + 3]]) as usize;
        len += 4 + words * 4;
    }
    // Ciphertext plus the 16-byte Poly1305 tag.
    if packet.len() < len + 16 {
        return None;
    }
    let ssrc = u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]);
    let timestamp = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
    Some(RtpHeader { len, ssrc, timestamp })
}

/// Decrypts an `xsalsa20_poly1305` RTP body. The nonce is the first 12 header
/// bytes followed by 12 zeros, per the Discord voice protocol.
pub(crate) fn decrypt_rtp(header: &[u8], body: &[u8], key: &[u8]) -> Option<Vec<u8>> {
    if header.len() < 12 {
        return None;
    }
    let cipher = XSalsa20Poly1305::new_from_slice(key).ok()?;
    let mut nonce = [0u8; 24];
    nonce[..12].copy_from_slice(&header[..12]);
    cipher.decrypt(Nonce::from_slice(&nonce), body).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_url_adds_scheme_and_version() {
        assert_eq!(
            voice_url("c-ams.discord.media:443"),
            "wss://c-ams.discord.media:443/?v=8",
            "эндпоинт должен получить схему и версию"
        );
        assert_eq!(voice_url(" wss://x/ "), "wss://x/?v=8", "схема не должна дублироваться");
    }

    #[test]
    fn discovery_packet_has_type_length_and_ssrc() {
        let packet = discovery_packet(0xDEAD_BEEF);
        assert_eq!(&packet[..2], &[0, 1], "тип запроса должен быть 1");
        assert_eq!(&packet[2..4], &[0, 70], "длина полезной части должна быть 70");
        assert_eq!(&packet[4..8], &[0xDE, 0xAD, 0xBE, 0xEF], "ssrc потерялся");
        assert!(packet[8..].iter().all(|&b| b == 0), "хвост должен быть нулевым");
    }

    #[test]
    fn discovery_reply_reads_address_and_port() {
        let mut reply = [0u8; 74];
        reply[0..2].copy_from_slice(&2u16.to_be_bytes());
        reply[2..4].copy_from_slice(&70u16.to_be_bytes());
        reply[8..19].copy_from_slice(b"203.0.113.7");
        reply[72..74].copy_from_slice(&50000u16.to_be_bytes());
        let (address, port) = parse_discovery_reply(&reply).expect("ответ должен разобраться");
        assert_eq!(address, "203.0.113.7", "адрес прочитан неверно");
        assert_eq!(port, 50000, "порт прочитан неверно");

        assert!(parse_discovery_reply(&reply[..70]).is_none(), "короткий ответ не должен приниматься");
        let mut wrong = reply;
        wrong[0] = 9;
        assert!(parse_discovery_reply(&wrong).is_none(), "чужой тип должен отбрасываться");
    }

    #[test]
    fn rtp_header_reads_ssrc_and_length() {
        let mut packet = vec![0u8; 12 + 16 + 4];
        packet[0] = 0x80; // version 2, no csrc
        packet[1] = 0x78;
        packet[8..12].copy_from_slice(&0x0102_0304u32.to_be_bytes());
        let header = parse_rtp(&packet).expect("заголовок должен разобраться");
        assert_eq!(header.len, 12, "длина заголовка без CSRC");
        assert_eq!(header.ssrc, 0x0102_0304, "ssrc прочитан неверно");

        packet[0] = 0x40; // version 1
        assert!(parse_rtp(&packet).is_none(), "версия 1 не RTP");
    }

    #[test]
    fn rtp_header_skips_csrc_and_extension() {
        let mut packet = vec![0u8; 24 + 16];
        packet[0] = 0x80 | 0x01 | 0x10; // one CSRC, extension
        packet[12 + 4 + 2..12 + 4 + 4].copy_from_slice(&1u16.to_be_bytes());
        let header = parse_rtp(&packet).expect("расширение должно учитываться");
        assert_eq!(header.len, 12 + 4 + 4 + 4, "длина с CSRC и расширением");
    }

    #[test]
    fn rtp_decrypt_roundtrip_and_tamper() {
        let mut header = [0u8; 12];
        header[0] = 0x80;
        header[8..12].copy_from_slice(&7u32.to_be_bytes());
        let key = [0x42u8; 32];
        let cipher = XSalsa20Poly1305::new_from_slice(&key).unwrap();
        let mut nonce = [0u8; 24];
        nonce[..12].copy_from_slice(&header);
        let body = cipher
            .encrypt(Nonce::from_slice(&nonce), b"opus-frame".as_slice())
            .unwrap();
        let plain = decrypt_rtp(&header, &body, &key).expect("расшифровка должна пройти");
        assert_eq!(plain, b"opus-frame", "данные повреждены");

        let mut tampered = body.clone();
        tampered[0] ^= 0xff;
        assert!(decrypt_rtp(&header, &tampered, &key).is_none(), "подмена должна ломать тег");
    }

    #[test]
    fn opus_decoder_handles_packet_loss() {
        // An empty packet is Opus packet-loss concealment; it must not error
        // even before the first real frame.
        let mut decoder = OpusDecoder::new(48_000, 2).expect("декодер должен создаться");
        let mut pcm = vec![0i16; OpusDecoder::MAX_FRAME_SIZE_48K * 2];
        assert!(decoder.decode(&[], &mut pcm, false).is_ok(), "PLC не должен падать");
    }
}
