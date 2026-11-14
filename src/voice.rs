//! Voice receive: the voice gateway handshake, UDP discovery, RTP decryption,
//! Opus decoding and playback. Transmit is a later step; this module only
//! listens.
//!
//! Flow: op 8 hello -> op 0 identify -> op 2 ready (ssrc/ip/port) -> UDP IP
//! discovery -> op 1 select protocol -> op 4 session description (secret key)
//! -> RTP audio, with op 3 heartbeats throughout.

use std::collections::HashMap;
use std::num::NonZeroU16;
use std::time::Duration;

use davey::{DaveSession, MediaType, ProposalsOperationType, DAVE_PROTOCOL_VERSION};
use futures_util::{SinkExt, StreamExt};
use opus::{Channels, Decoder};
use serde_json::{json, Value};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::watch;
use tokio::time;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use aes_gcm::Aes256Gcm;
use chacha20poly1305::aead::Payload;
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use xsalsa20poly1305::aead::{Aead, KeyInit};
use xsalsa20poly1305::{Nonce as XsalsaNonce, XSalsa20Poly1305};

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
    /// Channel id: the DAVE MLS group id and v9 `channel_id`.
    pub(crate) channel_id: String,
    pub(crate) user_id: String,
}

type VoiceSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Voice transport encryption. Discord stopped accepting plain
/// `xsalsa20_poly1305` on new connections (close 4016), so the modern
/// `*_rtpsize` AEAD modes are preferred; the choice itself comes from the
/// `modes` list in op 2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EncryptionMode {
    Xsalsa20Poly1305,
    AeadXChaCha20Poly1305Rtpsize,
    AeadAes256GcmRtpsize,
}

impl EncryptionMode {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Xsalsa20Poly1305 => "xsalsa20_poly1305",
            Self::AeadXChaCha20Poly1305Rtpsize => "aead_xchacha20_poly1305_rtpsize",
            Self::AeadAes256GcmRtpsize => "aead_aes256_gcm_rtpsize",
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "xsalsa20_poly1305" => Some(Self::Xsalsa20Poly1305),
            "aead_xchacha20_poly1305_rtpsize" => Some(Self::AeadXChaCha20Poly1305Rtpsize),
            "aead_aes256_gcm_rtpsize" => Some(Self::AeadAes256GcmRtpsize),
            _ => None,
        }
    }
}

/// Picks the best transport encryption from op 2's `modes` list. Unknown and
/// unsupported names (the `_lite`/`_suffix` xsalsa variants) are skipped;
/// without a list the server is old enough for header-nonce xsalsa.
pub(crate) fn pick_mode(modes: &[Value]) -> EncryptionMode {
    for wanted in [
        EncryptionMode::AeadXChaCha20Poly1305Rtpsize,
        EncryptionMode::AeadAes256GcmRtpsize,
        EncryptionMode::Xsalsa20Poly1305,
    ] {
        if modes.iter().any(|mode| mode.as_str() == Some(wanted.name())) {
            return wanted;
        }
    }
    EncryptionMode::Xsalsa20Poly1305
}

/// One event from the voice gateway. DAVE opcodes arrive as binary frames on
/// gateway v8+, with a 2-byte sequence number in front of the opcode.
enum VoiceEvent {
    Json(Value),
    Binary {
        /// v8 sequence number; heartbeats must acknowledge the last one.
        seq: u16,
        op: u8,
        payload: Vec<u8>,
    },
}

/// Raw dump of everything that reached the mixer, for diagnosing "the sound
/// is wrong" reports. `WYVERN_VOICE_DUMP=<dir>` writes one
/// `ssrc-<n>.pcm` file per sender: 48 kHz stereo, little-endian 16-bit, in
/// playback order, plus `packets-<n>.bin` with the exact Opus payloads
/// (4-byte little-endian length, then the bytes) so a decoder can be compared
/// offline. Two minutes at most, so a forgotten variable cannot fill the disk.
struct VoiceDump {
    dir: Option<std::path::PathBuf>,
    files: HashMap<u32, std::fs::File>,
    written: u64,
    packets: HashMap<u32, std::fs::File>,
    packet_bytes: u64,
}

impl VoiceDump {
    const LIMIT: u64 = 48_000 * 2 * 120;
    const PACKET_LIMIT: u64 = 16 * 1024 * 1024;

    fn new() -> Self {
        let dir = std::env::var("WYVERN_VOICE_DUMP")
            .ok()
            .filter(|d| !d.is_empty())
            .map(std::path::PathBuf::from);
        if let Some(dir) = dir.as_ref() {
            let _ = std::fs::create_dir_all(dir);
        }
        Self {
            dir,
            files: HashMap::new(),
            written: 0,
            packets: HashMap::new(),
            packet_bytes: 0,
        }
    }

    /// Saves a decoded frame, exactly as the mixer will hear it.
    fn push(&mut self, ssrc: u32, pcm: &[i16]) {
        let Some(dir) = self.dir.clone() else {
            return;
        };
        if self.written >= Self::LIMIT {
            return;
        }
        let file = match self.files.entry(ssrc) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let Ok(file) = std::fs::File::create(dir.join(format!("ssrc-{}.pcm", ssrc)))
                else {
                    return;
                };
                e.insert(file)
            }
        };
        use std::io::Write as _;
        let mut bytes = Vec::with_capacity(pcm.len() * 2);
        for sample in pcm {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        if file.write_all(&bytes).is_ok() {
            self.written += pcm.len() as u64;
        }
    }

    /// Saves an Opus payload before it is decoded: the input that decides
    /// whether a glitch comes from the network, the decoder or the mixer.
    fn push_packet(&mut self, ssrc: u32, payload: &[u8]) {
        let Some(dir) = self.dir.clone() else {
            return;
        };
        if self.packet_bytes >= Self::PACKET_LIMIT {
            return;
        }
        let file = match self.packets.entry(ssrc) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let Ok(file) = std::fs::File::create(dir.join(format!("packets-{}.bin", ssrc)))
                else {
                    return;
                };
                e.insert(file)
            }
        };
        use std::io::Write as _;
        let mut bytes = Vec::with_capacity(payload.len() + 4);
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(payload);
        if file.write_all(&bytes).is_ok() {
            self.packet_bytes += bytes.len() as u64;
        }
    }
}

/// A DAVE message to send: JSON for transition bookkeeping, binary for MLS.
enum DaveSend {
    Json(Value),
    Binary { op: u8, payload: Vec<u8> },
}

/// DAVE receive state. The MLS group itself lives in `davey`; this wrapper
/// only tracks the handshake pieces that arrive as separate gateway messages.
struct DaveState {
    session: DaveSession,
    user_id: u64,
    channel_id: u64,
    version: NonZeroU16,
    /// External sender package from op 25; no group can be created without it.
    external: Option<Vec<u8>>,
    /// Set when a fresh key package must go out (connect, epoch reset, recovery).
    key_package_pending: bool,
    /// False while the gateway selected protocol version 0; an upgrade can
    /// still arrive later as op 24 with epoch 1.
    active: bool,
}

impl DaveState {
    fn new(version: u16, user_id: u64, channel_id: u64, active: bool) -> Option<Self> {
        let version = NonZeroU16::new(version)?;
        let session = DaveSession::new(version, user_id, channel_id, None)
            .map_err(|e| eprintln!("[dave] session: {:?}", e))
            .ok()?;
        Some(Self {
            session,
            user_id,
            channel_id,
            version,
            external: None,
            key_package_pending: false,
            active,
        })
    }

    /// Applies op 25. Returns false once we are in a group: the external
    /// sender is fixed for the lifetime of that group.
    fn apply_external(&mut self, data: &[u8]) -> bool {
        if self.session.is_ready() {
            return false;
        }
        self.external = Some(data.to_vec());
        match self.session.set_external_sender(data) {
            Ok(()) => {
                self.key_package_pending = true;
                true
            }
            Err(e) => {
                eprintln!("[dave] external sender: {:?}", e);
                false
            }
        }
    }

    /// Enables E2EE after a transport-only start (op 24 upgrade).
    fn enable(&mut self) {
        self.active = true;
        if self.external.is_some() && !self.session.is_ready() {
            self.key_package_pending = true;
        }
    }

    /// Creates the next one-time key package, if one is due.
    fn take_key_package(&mut self) -> Option<Vec<u8>> {
        if !self.active || !self.key_package_pending {
            return None;
        }
        match self.session.create_key_package() {
            Ok(bytes) => {
                self.key_package_pending = false;
                Some(bytes)
            }
            Err(e) => {
                eprintln!("[dave] key package: {:?}", e);
                None
            }
        }
    }

    fn key_package(&mut self) -> Option<DaveSend> {
        self.take_key_package()
            .map(|payload| DaveSend::Binary { op: 26, payload })
    }

    /// Drops the MLS group and starts a new pending one (op 24 epoch = 1).
    fn reset(&mut self) {
        match DaveSession::new(self.version, self.user_id, self.channel_id, None) {
            Ok(session) => self.session = session,
            Err(e) => {
                eprintln!("[dave] reset: {:?}", e);
                return;
            }
        }
        if let Some(external) = self.external.clone() {
            if let Err(e) = self.session.set_external_sender(&external) {
                eprintln!("[dave] external sender after reset: {:?}", e);
                return;
            }
            self.key_package_pending = true;
        }
    }

    /// Handles one server-sent DAVE binary opcode (payload after the opcode).
    fn on_binary(&mut self, op: u8, payload: &[u8]) -> Vec<DaveSend> {
        match op {
            25 => {
                if self.apply_external(payload) {
                    self.key_package().into_iter().collect()
                } else {
                    Vec::new()
                }
            }
            27 => {
                if payload.is_empty() {
                    return Vec::new();
                }
                let operation = match payload[0] {
                    0 => ProposalsOperationType::APPEND,
                    1 => ProposalsOperationType::REVOKE,
                    other => {
                        eprintln!("[dave] unknown proposal operation {}", other);
                        return Vec::new();
                    }
                };
                match self
                    .session
                    .process_proposals(operation, &payload[1..], None)
                {
                    Ok(Some(commit)) => {
                        // Commit and welcome are serialized back to back; the
                        // welcome is absent unless the commit added members.
                        let mut body = commit.commit;
                        body.extend_from_slice(&commit.welcome.unwrap_or_default());
                        vec![DaveSend::Binary { op: 28, payload: body }]
                    }
                    Ok(None) => Vec::new(),
                    Err(e) => {
                        eprintln!("[dave] proposals: {:?}", e);
                        Vec::new()
                    }
                }
            }
            29 => {
                let Some(transition_id) = payload
                    .get(..2)
                    .map(|b| u16::from_be_bytes([b[0], b[1]]))
                else {
                    return Vec::new();
                };
                match self.session.process_commit(&payload[2..]) {
                    Ok(()) => vec![ready(transition_id)],
                    Err(e) => {
                        eprintln!("[dave] commit: {:?}", e);
                        self.invalid(transition_id)
                    }
                }
            }
            30 => {
                let Some(transition_id) = payload
                    .get(..2)
                    .map(|b| u16::from_be_bytes([b[0], b[1]]))
                else {
                    return Vec::new();
                };
                match self.session.process_welcome(&payload[2..]) {
                    Ok(()) => vec![ready(transition_id)],
                    Err(e) => {
                        eprintln!("[dave] welcome: {:?}", e);
                        self.invalid(transition_id)
                    }
                }
            }
            _ => Vec::new(),
        }
    }

    /// Handles the JSON DAVE opcodes that can arrive during a session.
    fn on_json(&mut self, op: i64, d: &Value) -> Vec<DaveSend> {
        match op {
            21 => {
                // Prepare transition. Version 0 is a downgrade to
                // transport-only encryption: plaintext frames must flow again
                // as soon as it is announced. A non-zero version is an
                // upgrade, and passthrough must stay off.
                if d["protocol_version"].as_u64().unwrap_or(0) == 0 {
                    self.session.set_passthrough_mode(true, None);
                } else {
                    self.enable();
                }
                let transition_id = d["transition_id"].as_u64().unwrap_or(0) as u16;
                // Transition 0 is the sole-member reset and executes at once.
                if transition_id == 0 {
                    Vec::new()
                } else {
                    vec![ready(transition_id)]
                }
            }
            24 => {
                if d["epoch"].as_u64() == Some(1) {
                    // A new group is being created: this is also the signal
                    // that an upgrade from transport-only encryption started.
                    if d["protocol_version"].as_u64().unwrap_or(0) > 0 {
                        self.enable();
                    }
                    self.reset();
                    self.key_package().into_iter().collect()
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    /// Reports an unprocessable commit/welcome and rebuilds our key package.
    fn invalid(&mut self, transition_id: u16) -> Vec<DaveSend> {
        let mut out = vec![DaveSend::Json(json!({
            "op": 31,
            "d": { "transition_id": transition_id }
        }))];
        self.reset();
        out.extend(self.key_package());
        out
    }

    /// Decrypts one DAVE frame from `user_id`. `None` means the frame must be
    /// dropped: a protocol frame we cannot authenticate is noise to Opus.
    fn decrypt(&mut self, user_id: u64, packet: &[u8]) -> Option<Vec<u8>> {
        self.session
            .decrypt(user_id, MediaType::AUDIO, packet)
            .ok()
    }

    /// Decrypts a frame whose sender is `known` if possible, otherwise tries
    /// every member of the MLS group.
    ///
    /// Every sender has its own key ratchet, so a frame can only be opened
    /// with its sender's id. Trying the group is safe: a wrong id fails the
    /// AEAD check and, because davey only advances a ratchet on success, it
    /// does not disturb that sender's stream. The id that authenticates the
    /// frame is the real owner of the ssrc, which is how the map is filled
    /// for streams that were already running when we joined — the speaking
    /// event is only sent when someone starts talking.
    fn decrypt_best(&mut self, known: Option<u64>, packet: &[u8]) -> Option<(u64, Vec<u8>)> {
        if let Some(user_id) = known {
            if let Some(frame) = self.decrypt(user_id, packet) {
                return Some((user_id, frame));
            }
        }
        for user_id in self.user_ids() {
            if Some(user_id) == known {
                continue;
            }
            if let Some(frame) = self.decrypt(user_id, packet) {
                return Some((user_id, frame));
            }
        }
        None
    }

    /// Members of the current MLS group: everyone who can send audio.
    fn user_ids(&self) -> Vec<u64> {
        self.session.get_user_ids().unwrap_or_default()
    }

    /// Whether the group is joined and frames can be opened.
    fn is_ready(&self) -> bool {
        self.session.is_ready()
    }
}

fn ready(transition_id: u16) -> DaveSend {
    DaveSend::Json(json!({ "op": 23, "d": { "transition_id": transition_id } }))
}

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

    // Op 25 (external sender) may arrive before the session description, so
    // keep it for the DAVE session that op 4 creates.
    let mut external_sender: Option<Vec<u8>> = None;
    // Last numbered binary message, echoed in every heartbeat (v8).
    let mut last_seq: Option<u16> = None;
    let hello = loop {
        match read_event(&mut ws).await? {
            VoiceEvent::Json(value) => break value,
            VoiceEvent::Binary { seq, op: 25, payload } => {
                last_seq = Some(seq);
                external_sender = Some(payload);
            }
            VoiceEvent::Binary { seq, .. } => last_seq = Some(seq),
        }
    };
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
            "channel_id": conn.channel_id,
            "user_id": conn.user_id,
            "session_id": conn.session_id,
            "token": conn.token,
            "video": false,
            // Without this Discord closes the socket with 4017 on every
            // non-stage voice channel since March 2026.
            "max_dave_protocol_version": DAVE_PROTOCOL_VERSION,
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
            _ = heartbeat.tick() => send_heartbeat(&mut ws, &mut nonce, last_seq).await?,
            _ = stop.changed() => return Ok(()),
            event = read_event(&mut ws) => {
                match event? {
                    VoiceEvent::Binary { seq, op: 25, payload } => {
                        last_seq = Some(seq);
                        external_sender = Some(payload);
                    }
                    VoiceEvent::Binary { seq, .. } => last_seq = Some(seq),
                    VoiceEvent::Json(v) => match v["op"].as_i64() {
                        Some(2) => ready = Some(v["d"].clone()),
                        Some(4) => return Err("voice: session description before ready".into()),
                        _ => {}
                    },
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

    // Take the best transport encryption the server lists. Sending a mode it
    // no longer supports gets the socket closed with 4016 before any audio.
    let modes = ready["modes"].as_array().cloned().unwrap_or_default();
    let requested = pick_mode(&modes);
    let select = json!({
        "op": 1,
        "d": {
            "protocol": "udp",
            "data": {
                "address": public_ip,
                "port": public_port,
                "mode": requested.name(),
            }
        }
    });
    send_json(&mut ws, &select).await?;

    // The answer repeats the accepted mode; use it rather than assuming the
    // server took our preference.
    let selected: EncryptionMode;
    let dave_version;
    let key = loop {
        tokio::select! {
            _ = heartbeat.tick() => send_heartbeat(&mut ws, &mut nonce, last_seq).await?,
            _ = stop.changed() => return Ok(()),
            event = read_event(&mut ws) => {
                match event? {
                    VoiceEvent::Binary { seq, op: 25, payload } => {
                        last_seq = Some(seq);
                        external_sender = Some(payload);
                    }
                    VoiceEvent::Binary { seq, .. } => last_seq = Some(seq),
                    VoiceEvent::Json(v) => match v["op"].as_i64() {
                        Some(4) => {
                            let mode = v["d"]["mode"].as_str().unwrap_or("");
                            selected = if mode.is_empty() {
                                requested
                            } else {
                                EncryptionMode::from_name(mode).ok_or_else(|| {
                                    format!("voice: unsupported encryption mode {}", mode)
                                })?
                            };
                            let bytes: Vec<u8> = v["d"]["secret_key"]
                                .as_array()
                                .map(|a| a.iter().filter_map(|x| x.as_u64().map(|n| n as u8)).collect())
                                .unwrap_or_default();
                            if bytes.len() != 32 {
                                return Err(format!("voice: secret key has {} bytes, need 32", bytes.len()));
                            }
                            dave_version = v["d"]["dave_protocol_version"].as_u64().unwrap_or(0) as u16;
                            break bytes;
                        }
                        Some(5) | Some(6) => {}
                        _ => {}
                    },
                }
            }
        }
    };

    // The session is prepared even for protocol version 0: the gateway can
    // upgrade a transport-only call later through op 24 with epoch 1.
    let mut dave = match (
        conn.user_id.parse::<u64>(),
        conn.channel_id.parse::<u64>(),
    ) {
        (Ok(user_id), Ok(channel_id)) => {
            DaveState::new(DAVE_PROTOCOL_VERSION, user_id, channel_id, dave_version > 0)
        }
        _ => None,
    };
    if let Some(state) = dave.as_mut() {
        if let Some(external) = external_sender.as_deref() {
            if state.apply_external(external) {
                if let Some(message) = state.key_package() {
                    send_dave(&mut ws, message).await?;
                }
            }
        }
    }

    // Audio output is opened only once the session is negotiated: a failed
    // handshake must not leave a stream running.
    let player = Player::start().map_err(|e| format!("audio output: {}", e))?;
    let _ = event_tx.send(ToApp::VoiceAudio {
        ready: true,
        reason: None,
    });

    receive_loop(
        &mut ws,
        &socket,
        &key,
        selected,
        &player,
        &mut dave,
        event_tx,
        &mut stop,
        &mut heartbeat,
        &mut nonce,
        &mut last_seq,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn receive_loop(
    ws: &mut VoiceSocket,
    socket: &UdpSocket,
    key: &[u8],
    mode: EncryptionMode,
    player: &Player,
    dave: &mut Option<DaveState>,
    event_tx: &EventTx,
    stop: &mut watch::Receiver<bool>,
    heartbeat: &mut time::Interval,
    nonce: &mut u64,
    last_seq: &mut Option<u16>,
) -> Result<(), String> {
    let mut decoders: HashMap<u32, Decoder> = HashMap::new();
    let mut speakers: HashMap<u32, String> = HashMap::new();
    // Per-ssrc RTP timestamp anchor: (mixer frame, timestamp).
    let mut anchors: HashMap<u32, (i64, u32)> = HashMap::new();
    // Per-ssrc RTP timestamp the next packet should carry; a difference is a
    // hole that has to be concealed so the decoder stays in step.
    let mut next_ts: HashMap<u32, u32> = HashMap::new();
    let mut dump = VoiceDump::new();
    let mut pcm = vec![0i16; MAX_FRAME_SAMPLES];
    // Mixer input is interleaved stereo, the codec output is mono.
    let mut stereo = vec![0i16; MAX_FRAME_SAMPLES * 2];
    let mut udp_buf = [0u8; 2048];
    // Diagnostics: a garbled call must say which stage loses frames. Counters
    // are reported to the app log every few seconds while audio arrives.
    let mut packets: u64 = 0;
    let mut transport_fail: u64 = 0;
    let mut transport_logged = false;
    let mut no_user: u64 = 0;
    let mut dave_ok: u64 = 0;
    let mut dave_fail: u64 = 0;
    let mut plain_frames: u64 = 0;
    let mut opus_fail: u64 = 0;
    // Payloads that are neither marked E2EE nor a parseable Opus packet.
    let mut malformed: u64 = 0;
    let mut pushed: u64 = 0;
    // Frames the decoder turned into a saturated tone instead of speech.
    let mut blowups: u64 = 0;
    let mut concealed: u64 = 0;
    let mut gap_jumps: u64 = 0;
    let mut ssrc_learned: u64 = 0;
    let mut speaking_events: u64 = 0;
    let mut speaking_with_id: u64 = 0;
    let mut op5_logged = false;
    let mut decode_us: u128 = 0;
    let mut last_toc: u8 = 0;
    let mut report = time::interval(Duration::from_secs(5));

    loop {
        tokio::select! {
            _ = heartbeat.tick() => send_heartbeat(ws, nonce, *last_seq).await?,
            _ = stop.changed() => return Ok(()),
            _ = report.tick() => {
                if packets > 0 {
                    let (stalls, resets, late, ahead) = player.stalls();
                    let (clipped, peak) = player.levels();
                    let (dave_ready, dave_group) = dave
                        .as_ref()
                        .map(|s| (s.is_ready(), s.user_ids().len()))
                        .unwrap_or((false, 0));
                    let _ = event_tx.send(ToApp::Debug(format!(
                        "Голос: режим={} пакетов={} транспорт_ошибок={} без_пользователя={} dave_ok={} dave_fail={} dave_ready={} группа={} выучено={} оп5={} оп5_с_id={} ssrc_известно={} plain={} мусор={} opus_fail={} отправлено={} скрыто={} скачков={} буфер={}мс toc={:#04x} провалов={} сбросов={} опоздавших={} вперёд={} источников={} декод={}мкс колбэк={}мс клипов={} пик={} глитчей={}",
                        mode.name(),
                        packets,
                        transport_fail,
                        no_user,
                        dave_ok,
                        dave_fail,
                        dave_ready,
                        dave_group,
                        ssrc_learned,
                        speaking_events,
                        speaking_with_id,
                        speakers.len(),
                        plain_frames,
                        malformed,
                        opus_fail,
                        pushed,
                        concealed,
                        gap_jumps,
                        player.buffered() * 1000 / crate::audio::SAMPLE_RATE as i64,
                        last_toc,
                        stalls,
                        resets,
                        late,
                        ahead,
                        decoders.len(),
                        decode_us / u128::from(pushed.max(1)),
                        player.callback_frames() * 1000 / crate::audio::SAMPLE_RATE as i64,
                        clipped,
                        peak,
                        blowups,
                    )));
                }
            }
            received = socket.recv(&mut udp_buf) => {
                let n = received.map_err(|e| format!("voice udp: {}", e))?;
                let packet = &udp_buf[..n];
                packets += 1;
                let Some(header) = parse_rtp(packet) else { continue };
                let Some(plain) = decrypt_rtp(packet, key, mode) else {
                    transport_fail += 1;
                    // One sample is enough to tell a stray non-voice packet
                    // from a real key mismatch.
                    if !transport_logged {
                        transport_logged = true;
                        use std::fmt::Write as _;
                        let mut hex = String::new();
                        for byte in &packet[..packet.len().min(16)] {
                            let _ = write!(hex, "{:02x}", byte);
                        }
                        let _ = event_tx.send(ToApp::Debug(format!(
                            "Голос: не расшифровался пакет len={} pt={:#04x} ssrc={} нач={}",
                            packet.len(),
                            packet[1],
                            header.ssrc,
                            hex,
                        )));
                    }
                    continue;
                };
                if plain.is_empty() {
                    continue;
                }
                // E2EE frames are decrypted per sender before Opus. A plain
                // frame has no magic marker and passes through unchanged, as
                // during the transport-only and downgrade phases.
                let mut dave_frame = is_dave_frame(&plain);
                let mut frame = plain;
                let mut opened = false;
                if !dave_frame && !valid_opus_packet(&frame) {
                    // Not marked as E2EE and not a well-formed Opus payload.
                    // Opus has no checksum, so ciphertext handed to the
                    // decoder is not an error but a loud burst of noise —
                    // exactly the "loud low-quality sound" this guards
                    // against. One last try at the ratchets: a sender may
                    // encrypt without leaving the marker where we look for it.
                    let known = speakers
                        .get(&header.ssrc)
                        .and_then(|id| id.parse::<u64>().ok());
                    if let Some((user_id, plaintext)) = dave
                        .as_mut()
                        .and_then(|state| state.decrypt_best(known, &frame))
                    {
                        if valid_opus_packet(&plaintext) {
                            if known.is_none() {
                                speakers.insert(header.ssrc, user_id.to_string());
                                ssrc_learned += 1;
                            }
                            frame = plaintext;
                            dave_frame = true;
                            opened = true;
                            dave_ok += 1;
                        }
                    }
                    if !opened {
                        malformed += 1;
                        continue;
                    }
                }
                let plain = if dave_frame && !opened {
                    let known = speakers
                        .get(&header.ssrc)
                        .and_then(|id| id.parse::<u64>().ok());
                    let Some(state) = dave.as_mut() else {
                        dave_fail += 1;
                        continue;
                    };
                    let Some((user_id, plaintext)) = state.decrypt_best(known, &frame) else {
                        if known.is_none() {
                            no_user += 1;
                        }
                        dave_fail += 1;
                        continue;
                    };
                    if known.is_none() {
                        // Learned straight from the ratchet that opened the
                        // frame; no speaking event was needed.
                        speakers.insert(header.ssrc, user_id.to_string());
                        ssrc_learned += 1;
                    }
                    dave_ok += 1;
                    plaintext
                } else {
                    plain_frames += 1;
                    frame
                };
                last_toc = plain[0];
                dump.push_packet(header.ssrc, &plain);
                let gain = crate::audio::others_volume();
                let now = player.position();
                let (start, jumped) = anchor_frame(
                    &mut anchors,
                    now,
                    player.end(),
                    header.ssrc,
                    header.timestamp,
                );
                if jumped {
                    gap_jumps += 1;
                    // After a pause the decoder still holds the history of a
                    // stream that was already played; feeding it the first
                    // frame of a new spurt makes the synthesis filter
                    // overshoot.
                    decoders.remove(&header.ssrc);
                }
                let decoder = match decoders.entry(header.ssrc) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        match Decoder::new(48_000, Channels::Mono) {
                            Ok(d) => e.insert(d),
                            // A codec failure must not kill the session.
                            Err(_) => continue,
                        }
                    }
                };
                // Packets the network lost must be replaced, not skipped: an
                // Opus stream is stateful, and a gap leaves the decoder
                // predicting from stale history, which is heard as crackle
                // and "wobbly" speech after every loss. Concealing up to
                // 120 ms keeps both the state and the timeline continuous; a
                // longer hole is a pause and was re-anchored above. The hole
                // must also still lie in the future: filling one that was
                // already played as silence would push this stream further
                // ahead of playback on every pause, which is how the delay
                // grew to half a second.
                if let Some(expected) = next_ts.get(&header.ssrc).copied() {
                    let delta = header.timestamp.wrapping_sub(expected) as i32 as i64;
                    if delta > 0 && delta <= MAX_CONCEAL_FRAMES && start - delta >= now {
                        let mut cursor = start - delta;
                        while cursor < start {
                            let Ok(n) = decoder.decode(&[], &mut pcm, false) else {
                                break;
                            };
                            if n == 0 {
                                break;
                            }
                            let room = (start - cursor).min(n as i64) as usize;
                            let used = dup_mono(&pcm[..room], &mut stereo);
                            player.push_at_gain(cursor, &stereo[..used], gain);
                            dump.push(header.ssrc, &stereo[..used]);
                            cursor += room as i64;
                        }
                        concealed += 1;
                    }
                }
                // 20 ms frame at 48 kHz; the buffer always has room for a
                // full 120 ms frame.
                let started = std::time::Instant::now();
                let decoded = decoder.decode(&plain, &mut pcm, false);
                decode_us += started.elapsed().as_micros();
                let Ok(samples) = decoded else {
                    opus_fail += 1;
                    // Opus has no integrity check: a corrupt payload can still
                    // "decode" into a short burst of noise, so a rejected one
                    // is worth showing verbatim.
                    if opus_fail <= 3 {
                        use std::fmt::Write as _;
                        let mut hex = String::new();
                        for byte in &plain[..plain.len().min(16)] {
                            let _ = write!(hex, "{:02x}", byte);
                        }
                        let _ = event_tx.send(ToApp::Debug(format!(
                            "Голос: Opus не принял кадр ssrc={} len={} dave={} нач={}",
                            header.ssrc,
                            plain.len(),
                            dave_frame,
                            hex,
                        )));
                    }
                    continue;
                };
                next_ts.insert(
                    header.ssrc,
                    header.timestamp.wrapping_add(samples as u32),
                );
                // Opus has no checksum, so a payload that is not really Opus
                // can decode into a saturated tone. Nothing that comes out of
                // the decoder sounding like that is speech, and pushing it
                // would be heard as a loud glitch; drop the frame and throw the
                // poisoned decoder away so the next one starts clean.
                if samples > 0 && is_decoder_blowup(&pcm[..samples.min(pcm.len())]) {
                    blowups += 1;
                    decoders.remove(&header.ssrc);
                    continue;
                }
                if samples > 0 && samples <= pcm.len() {
                    let used = dup_mono(&pcm[..samples], &mut stereo);
                    player.push_at_gain(start, &stereo[..used], gain);
                    dump.push(header.ssrc, &stereo[..used]);
                    pushed += 1;
                }
            }
            event = read_event(ws) => {
                match event? {
                    VoiceEvent::Json(v) => {
                        let op = v["op"].as_i64().unwrap_or(0);
                        match op {
                            5 => {
                                // Speaking state: the only place that maps an
                                // RTP ssrc to a user id. Some events omit the
                                // id, so the earlier mapping is the fallback.
                                let d = &v["d"];
                                // The first event is logged verbatim: whether
                                // it carries a user id at all decides how an
                                // ssrc is attributed, and servers have changed
                                // between a string and a number here.
                                if !op5_logged {
                                    op5_logged = true;
                                    let raw = v.to_string();
                                    let _ = event_tx.send(ToApp::Debug(format!(
                                        "Голос: первый op5 {}",
                                        raw.chars().take(300).collect::<String>()
                                    )));
                                }
                                speaking_events += 1;
                                let speaking = d["speaking"].as_u64().unwrap_or(0) != 0;
                                let ssrc = d["ssrc"].as_u64().map(|s| s as u32);
                                let mut user_id = d["user_id"]
                                    .as_str()
                                    .map(str::to_string)
                                    .or_else(|| d["user_id"].as_u64().map(|id| id.to_string()))
                                    .unwrap_or_default();
                                if let (Some(ssrc), false) = (ssrc, user_id.is_empty()) {
                                    speaking_with_id += 1;
                                    speakers.insert(ssrc, user_id.clone());
                                } else if let Some(ssrc) = ssrc {
                                    user_id = speakers.get(&ssrc).cloned().unwrap_or_default();
                                }
                                if !user_id.is_empty() {
                                    let _ = event_tx.send(ToApp::VoiceSpeaking { user_id, speaking });
                                }
                            }
                            21 | 24 => {
                                if let Some(state) = dave.as_mut() {
                                    let outgoing = state.on_json(op, &v["d"]);
                                    for message in outgoing {
                                        send_dave(ws, message).await?;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    VoiceEvent::Binary { seq, op, payload } => {
                        *last_seq = Some(seq);
                        if let Some(state) = dave.as_mut() {
                            let outgoing = state.on_binary(op, &payload);
                            for message in outgoing {
                                send_dave(ws, message).await?;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Longest hole that is filled with concealing frames. Anything longer is a
/// pause, not packet loss: the sender's RTP clock keeps running while nobody
/// talks, and pretending to conceal a whole second of silence would only add
/// delay.
const MAX_CONCEAL_FRAMES: i64 = 5_760;
/// Samples in the longest Opus frame: 120 ms at 48 kHz.
const MAX_FRAME_SAMPLES: usize = 5_760;
/// How far a sender's timeline may jump before the stream is re-anchored to
/// the end of the queue. Equal to the conceal limit so a hole is either
/// concealed or skipped, never both.
const ANCHOR_GAP_FRAMES: i64 = MAX_CONCEAL_FRAMES;

/// Mixer frame for this packet's first sample, derived from its RTP timestamp.
///
/// The first packet of each ssrc is anchored 100 ms ahead of playback (or at
/// the end of the queue if other people are already playing). Later packets
/// keep that offset, which is what aligns two speakers in the ring. The
/// returned flag is set when the sender's clock jumped past the queue and the
/// stream had to be re-anchored: RTP timestamps keep counting while someone
/// is silent, so a pause would otherwise leave a hole in the timeline that is
/// heard as a delay followed by a hiccup.
pub(crate) fn anchor_frame(
    anchors: &mut HashMap<u32, (i64, u32)>,
    now: i64,
    queue_end: i64,
    ssrc: u32,
    timestamp: u32,
) -> (i64, bool) {
    // Join whatever is already queued; only an empty queue needs the
    // pre-buffer lead. Adding the lead on top of a queue would leave a hole.
    let resume = if queue_end > now {
        queue_end
    } else {
        now + crate::audio::PREBUFFER_FRAMES as i64
    };
    let (base, base_ts, jumped) = match anchors.get(&ssrc).copied() {
        Some((base, base_ts)) => {
            let start = base + timestamp.wrapping_sub(base_ts) as i32 as i64;
            if start + (crate::audio::RING_FRAMES as i64) < now {
                // Counter reset or a stream restarted behind us.
                (resume, timestamp, true)
            } else if start > queue_end + ANCHOR_GAP_FRAMES {
                // The sender paused and its clock moved on without us.
                (resume, timestamp, true)
            } else if start < now {
                // The stream fell behind playback — a pause we already
                // covered with silence, or a burst that arrived late. Audio
                // cannot be played in the past; joining the queue now keeps
                // the delay from growing with every pause.
                (resume, timestamp, true)
            } else {
                (base, base_ts, false)
            }
        }
        None => (resume, timestamp, false),
    };
    anchors.insert(ssrc, (base, base_ts));
    (base + timestamp.wrapping_sub(base_ts) as i32 as i64, jumped)
}

/// Heartbeat body. v8 changed `d` from a bare `t` to an object and made
/// `seq_ack` mandatory once a numbered binary message has arrived: it is what
/// stops DAVE commits and welcomes from being retransmitted.
pub(crate) fn heartbeat_payload(t: u64, seq_ack: Option<u16>) -> Value {
    match seq_ack {
        Some(seq) => json!({ "op": 3, "d": { "t": t, "seq_ack": seq } }),
        None => json!({ "op": 3, "d": { "t": t } }),
    }
}

async fn send_heartbeat(
    ws: &mut VoiceSocket,
    nonce: &mut u64,
    seq_ack: Option<u16>,
) -> Result<(), String> {
    *nonce = nonce.wrapping_add(1);
    send_json(ws, &heartbeat_payload(*nonce, seq_ack)).await
}

async fn send_json(ws: &mut VoiceSocket, payload: &Value) -> Result<(), String> {
    ws.send(WsMessage::Text(payload.to_string().into()))
        .await
        .map_err(|e| format!("voice gateway: {}", e))
}

async fn send_dave(ws: &mut VoiceSocket, message: DaveSend) -> Result<(), String> {
    match message {
        DaveSend::Json(payload) => send_json(ws, &payload).await,
        DaveSend::Binary { op, payload } => {
            // Client-to-server binary messages carry no sequence number.
            let mut frame = Vec::with_capacity(payload.len() + 1);
            frame.push(op);
            frame.extend_from_slice(&payload);
            ws.send(WsMessage::Binary(frame.into()))
                .await
                .map_err(|e| format!("voice gateway: {}", e))
        }
    }
}

async fn read_event(ws: &mut VoiceSocket) -> Result<VoiceEvent, String> {
    loop {
        match ws.next().await {
            Some(Ok(WsMessage::Text(text))) => {
                let value =
                    serde_json::from_str(&text).map_err(|e| format!("voice gateway: {}", e))?;
                return Ok(VoiceEvent::Json(value));
            }
            Some(Ok(WsMessage::Binary(frame))) => {
                let (seq, op, payload) =
                    parse_binary(&frame).ok_or("voice gateway: short binary frame")?;
                return Ok(VoiceEvent::Binary { seq, op, payload });
            }
            Some(Ok(WsMessage::Close(frame))) => {
                let code = frame.map(|f| u16::from(f.code)).unwrap_or(0);
                return Err(close_reason(code));
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(format!("voice gateway: {}", e)),
            None => return Err("voice gateway closed".to_string()),
        }
    }
}

/// Splits a server binary frame. Gateway v8 prefixes every binary message
/// with a 2-byte big-endian sequence number; the opcode follows it.
pub(crate) fn parse_binary(frame: &[u8]) -> Option<(u16, u8, Vec<u8>)> {
    if frame.len() < 3 {
        return None;
    }
    let seq = u16::from_be_bytes([frame[0], frame[1]]);
    Some((seq, frame[2], frame[3..].to_vec()))
}

/// DAVE protocol frames end with the 0xFAFA magic marker. Anything else is a
/// plain encoded frame and passes through unchanged during transitions.
pub(crate) fn is_dave_frame(packet: &[u8]) -> bool {
    packet.len() >= 12 && packet[packet.len() - 2..] == [0xfa, 0xfa]
}

/// Turns a voice close code into the sentence shown in the voice bar.
pub(crate) fn close_reason(code: u16) -> String {
    let text = match code {
        4001 => "unknown voice opcode",
        4002 => "malformed voice identify",
        4004 => "invalid voice token",
        4006 => "voice session expired",
        4011 => "voice server not found",
        4012 => "unknown voice protocol",
        4014 => "disconnected by Discord",
        4015 => "voice server crashed",
        4016 => "unsupported voice encryption mode",
        4017 => "Discord requires DAVE end-to-end encryption",
        4020 => "malformed voice request",
        4021 => "voice rate limit",
        4022 => "call ended",
        _ => return format!("voice gateway closed: {}", code),
    };
    format!("voice: {} ({})", text, code)
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

/// Copies a mono frame into interleaved stereo, returning the number of
/// samples written.
///
/// Discord voice is mono, but Opus has no checksum: the decoder accepts
/// almost any payload and, when the stream and the decoder disagree about the
/// channel count, occasionally emits a full-scale anti-phase burst instead of
/// speech. Decoding one channel and copying it is both cheaper and immune.
pub(crate) fn dup_mono(src: &[i16], dst: &mut [i16]) -> usize {
    let n = src.len().min(dst.len() / 2);
    for (i, sample) in src[..n].iter().enumerate() {
        dst[2 * i] = *sample;
        dst[2 * i + 1] = *sample;
    }
    n * 2
}

/// Whether a decoded frame looks like the decoder blew up rather than like a
/// loud voice.
///
/// A payload that is not really Opus — ciphertext that slipped through, a
/// packet from another stream — leaves an unstable synthesis filter behind:
/// the output ramps up to full scale and stays there, which is heard as a
/// short loud tone. Speech has a much lower crest factor even when it clips,
/// so a frame that is both saturated and nearly square is never voice. The
/// artefact also starts on exact silence: the first samples are zero and the
/// end of the same 20 ms frame is already pinned to full scale, which no
/// microphone attack produces.
pub(crate) fn is_decoder_blowup(pcm: &[i16]) -> bool {
    if pcm.len() < 64 {
        return false;
    }
    let mut peak = 0i32;
    let mut sum = 0f64;
    for sample in pcm {
        let magnitude = (*sample as i32).abs();
        peak = peak.max(magnitude);
        sum += (*sample as f64) * (*sample as f64);
    }
    // Too quiet to be the artefact, and loud speech must never be dropped.
    if peak < 32_000 {
        return false;
    }
    let rms = (sum / pcm.len() as f64).sqrt();
    if rms > peak as f64 * 0.85 {
        return true;
    }
    let onset = pcm.len() / 8;
    let tail = &pcm[pcm.len() / 4 * 3..];
    pcm[..onset].iter().all(|s| (*s as i32).abs() < 200)
        && tail.iter().any(|s| (*s as i32).abs() >= 31_000)
}

/// Reads a frame length as RFC 6716 section 3.2 encodes it: one byte below
/// 252, otherwise a two-byte value.
fn frame_length(body: &[u8], pos: &mut usize) -> Option<usize> {
    let first = *body.get(*pos)? as usize;
    *pos += 1;
    if first < 252 {
        return Some(first);
    }
    let second = *body.get(*pos)? as usize;
    *pos += 1;
    Some(first + 4 * second)
}

/// Whether `payload` is a syntactically valid Opus packet (RFC 6716 section
/// 3). A payload that does not parse is ciphertext or a foreign protocol
/// packet, and handing it to the decoder produces noise rather than an error,
/// so the caller drops it instead.
pub(crate) fn valid_opus_packet(payload: &[u8]) -> bool {
    let Some(&toc) = payload.first() else {
        return false;
    };
    let body = &payload[1..];
    match toc & 0x03 {
        // One frame of any size; zero bytes is a DTX frame.
        0 => true,
        // Two frames of equal size.
        1 => body.len() % 2 == 0,
        // Two frames, the first with an explicit length.
        2 => {
            let mut pos = 0;
            let Some(first) = frame_length(body, &mut pos) else {
                return false;
            };
            match body.len().checked_sub(pos) {
                Some(rest) => first > 0 && rest > first,
                None => false,
            }
        }
        // Arbitrary framing: optional padding, then one to 48 frames whose
        // lengths must add up to exactly what is left.
        _ => {
            let Some(&head) = body.first() else {
                return false;
            };
            let count = (head & 0x3f) as usize;
            if count == 0 {
                return false;
            }
            let mut pos = 1;
            let mut padding = 0usize;
            if head & 0x40 != 0 {
                loop {
                    let Some(&byte) = body.get(pos) else {
                        return false;
                    };
                    pos += 1;
                    if byte == 255 {
                        padding += 254;
                    } else {
                        padding += byte as usize;
                        break;
                    }
                }
            }
            let Some(mut rest) = body.len().checked_sub(pos) else {
                return false;
            };
            if rest <= padding {
                return false;
            }
            rest -= padding;
            if head & 0x80 != 0 {
                let mut total = 0usize;
                for _ in 1..count {
                    let Some(length) = frame_length(body, &mut pos) else {
                        return false;
                    };
                    if length == 0 {
                        return false;
                    }
                    total += length;
                }
                let Some(rest) = body.len().checked_sub(pos) else {
                    return false;
                };
                if rest <= padding {
                    return false;
                }
                rest - padding > total
            } else {
                rest >= count
            }
        }
    }
}

/// One parsed RTP packet header.
pub(crate) struct RtpHeader {
    /// Full header length including CSRCs and the extension payload.
    pub(crate) len: usize,
    /// Fixed header + CSRCs + extension header: the part AEAD authenticates
    /// without encrypting.
    pub(crate) aad_len: usize,
    /// Extension payload size in 32-bit words; AEAD encrypts it with the frame.
    pub(crate) extension_words: usize,
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
    // RTCP shares the UDP port and its packet type goes into the same byte,
    // but without RTP's marker bit: 200 (sender report) and up. Encrypted
    // voice never uses a payload type that high, so reports and keepalives
    // must not be counted as undecryptable audio.
    if packet[1] >= 200 {
        return None;
    }
    let csrc_count = (packet[0] & 0x0f) as usize;
    let mut len = 12 + csrc_count * 4;
    let mut aad_len = len;
    let mut extension_words = 0;
    if packet[0] & 0x10 != 0 {
        // One extension: 16-bit profile, 16-bit length in 32-bit words. The
        // extension payload itself is encrypted in the AEAD modes.
        if packet.len() < len + 4 {
            return None;
        }
        extension_words = u16::from_be_bytes([packet[len + 2], packet[len + 3]]) as usize;
        aad_len = len + 4;
        len += 4 + extension_words * 4;
    }
    // Ciphertext plus the 16-byte Poly1305 tag.
    if packet.len() < len + 16 {
        return None;
    }
    let ssrc = u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]);
    let timestamp = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
    Some(RtpHeader {
        len,
        aad_len,
        extension_words,
        ssrc,
        timestamp,
    })
}

/// Decrypts one transport-encrypted RTP packet according to `mode`.
///
/// `xsalsa20_poly1305` takes the first 12 header bytes as the nonce and leaves
/// the extension payload in clear text. The `*_rtpsize` AEAD modes keep a
/// 4-byte counter at the very end of the packet (the nonce is that counter
/// followed by zeros), authenticate the plain header as additional data and
/// encrypt the extension payload together with the Opus frame.
pub(crate) fn decrypt_rtp(packet: &[u8], key: &[u8], mode: EncryptionMode) -> Option<Vec<u8>> {
    let header = parse_rtp(packet)?;
    match mode {
        EncryptionMode::Xsalsa20Poly1305 => {
            let cipher = XSalsa20Poly1305::new_from_slice(key).ok()?;
            let mut nonce = [0u8; 24];
            nonce[..12].copy_from_slice(&packet[..12]);
            cipher
                .decrypt(XsalsaNonce::from_slice(&nonce), &packet[header.len..])
                .ok()
        }
        EncryptionMode::AeadXChaCha20Poly1305Rtpsize
        | EncryptionMode::AeadAes256GcmRtpsize => {
            if packet.len() < header.aad_len + 16 + 4 {
                return None;
            }
            // Ciphertext (tag included) up to the trailing 4-byte nonce.
            let split = packet.len() - 4;
            let ciphertext = &packet[header.aad_len..split];
            let nonce_bytes = &packet[split..];
            let aad = &packet[..header.aad_len];
            let plain = if mode == EncryptionMode::AeadXChaCha20Poly1305Rtpsize {
                let cipher = XChaCha20Poly1305::new_from_slice(key).ok()?;
                let mut nonce = [0u8; 24];
                nonce[..4].copy_from_slice(nonce_bytes);
                cipher
                    .decrypt(
                        XNonce::from_slice(&nonce),
                        Payload {
                            msg: ciphertext,
                            aad,
                        },
                    )
                    .ok()?
            } else {
                let cipher = Aes256Gcm::new_from_slice(key).ok()?;
                let mut nonce = [0u8; 12];
                nonce[..4].copy_from_slice(nonce_bytes);
                cipher
                    .decrypt(
                        aes_gcm::Nonce::from_slice(&nonce),
                        Payload {
                            msg: ciphertext,
                            aad,
                        },
                    )
                    .ok()?
            };
            // Drop the decrypted extension payload; Opus starts after it.
            let skip = header.extension_words * 4;
            if plain.len() < skip {
                return None;
            }
            Some(plain[skip..].to_vec())
        }
    }
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
    fn rtcp_reports_are_not_mistaken_for_voice() {
        // 0x81 0xC8: version 2, one report block, sender report (PT 200).
        let mut packet = vec![0u8; 32];
        packet[0] = 0x81;
        packet[1] = 0xC8;
        assert!(
            parse_rtp(&packet).is_none(),
            "RTCP-отчёт не должен считаться голосовым пакетом"
        );
    }

    #[test]
    fn anchor_joins_the_end_of_the_queue() {
        let mut anchors = HashMap::new();
        let (start, jumped) = anchor_frame(&mut anchors, 0, 0, 7, 1_000);
        assert_eq!(start, 4_800, "пустая очередь — пакет через пребуфер");
        assert!(!jumped, "первый пакет источника — не скачок");

        // Someone is already playing: the new stream must follow it directly
        // instead of opening a hole in the timeline.
        let mut anchors = HashMap::new();
        let (start, jumped) = anchor_frame(&mut anchors, 500, 1_000, 9, 0);
        assert_eq!(start, 1_000, "новый источник встаёт в конец очереди");
        assert!(!jumped);
    }

    #[test]
    fn anchor_keeps_the_clock_of_a_running_stream() {
        let mut anchors = HashMap::new();
        anchor_frame(&mut anchors, 0, 0, 7, 1_000);
        let (start, jumped) = anchor_frame(&mut anchors, 480, 5_760, 7, 1_960);
        assert_eq!(start, 5_760, "кадры подряд должны идти вплотную");
        assert!(!jumped);
    }

    #[test]
    fn anchor_skips_a_pause_instead_of_leaving_a_hole() {
        let mut anchors = HashMap::new();
        anchor_frame(&mut anchors, 0, 0, 7, 0);
        // Half a second of silence: the RTP clock kept running.
        let (start, jumped) = anchor_frame(&mut anchors, 9_000, 10_000, 7, 24_000);
        assert!(jumped, "пауза говорящего — это скачок таймлайна");
        assert_eq!(start, 10_000, "после паузы звук идёт сразу за очередью");
    }

    #[test]
    fn anchor_never_schedules_audio_in_the_past() {
        // The stream fell behind the playback position (a pause that was
        // already heard as silence, or a late burst). Playing it in the past
        // is impossible; it has to join the queue instead of stretching the
        // delay by the length of every pause.
        let mut anchors = HashMap::new();
        anchor_frame(&mut anchors, 0, 4_800, 7, 1_000);
        let (start, jumped) = anchor_frame(&mut anchors, 12_000, 12_500, 7, 2_000);
        assert!(jumped, "отставший источник должен переставиться");
        assert_eq!(start, 12_500, "звук встаёт в конец очереди");
    }

    #[test]
    fn mono_frame_is_copied_into_both_channels() {
        let src = [1i16, -2, 3];
        let mut dst = [0i16; 6];
        let used = dup_mono(&src, &mut dst);
        assert_eq!(used, 6, "моно-кадр занимает вдвое больше сэмплов");
        assert_eq!(dst, [1, 1, -2, -2, 3, 3], "оба канала должны совпадать");
    }

    #[test]
    fn opus_framing_rejects_ciphertext() {
        // Payloads that reached the decoder in a real call instead of audio.
        assert!(
            !valid_opus_packet(&[
                0x71, 0xc4, 0x63, 0x7a, 0x0c, 0xd2, 0x4c, 0x27, 0x21, 0xc2, 0x08, 0x89, 0xb3,
                0x54, 0x5f, 0xad
            ]),
            "кадр с длиной больше остатка — не Opus"
        );
        assert!(!valid_opus_packet(&[]), "пустой кадр — не Opus");
        assert!(
            !valid_opus_packet(&[0x03, 0x62, 0x05]),
            "произвольная упаковка с неверной длиной — не Opus"
        );
    }

    #[test]
    fn opus_framing_accepts_normal_frames() {
        // Code 0 (single frame), code 1 (two equal), code 2 (explicit length)
        // and code 3 with a frame count.
        let single = [0x78u8, 0x11, 0x22, 0x33];
        assert!(valid_opus_packet(&single), "одиночный кадр должен проходить");
        let two_equal = [0x79u8, 0x11, 0x22, 0x33, 0x44];
        assert!(valid_opus_packet(&two_equal), "два равных кадра");
        let explicit = [0x7au8, 0x01, 0x11, 0x22];
        assert!(valid_opus_packet(&explicit), "кадр с явной длиной");
        let counted = [0x7bu8, 0x02, 0x01, 0x11, 0x22];
        assert!(valid_opus_packet(&counted), "произвольная упаковка");
        // Silence/DTX frame Discord sends between words.
        assert!(valid_opus_packet(&[0xF8, 0xFF, 0xFE]), "DTX-кадр");
    }

    #[test]
    fn saturated_square_frame_is_a_decoder_blowup() {
        // The glitch heard in a call: a tone pinned to full scale. Speech
        // never has RMS that close to the peak.
        let blowup: Vec<i16> = (0..960)
            .map(|i| if (i / 20) % 2 == 0 { 32_700 } else { -32_700 })
            .collect();
        assert!(is_decoder_blowup(&blowup), "прямоугольная волна — глитч");

        // The same artefact's fade-in: exact silence, then full scale before
        // the frame ends.
        let ramp: Vec<i16> = (0..960)
            .map(|i| {
                if i < 600 {
                    0
                } else if i % 2 == 0 {
                    32_767
                } else {
                    -32_767
                }
            })
            .collect();
        assert!(is_decoder_blowup(&ramp), "нарастание из тишины — глитч");

        // Loud speech: lower crest factor even at full scale, must survive.
        let speech: Vec<i16> = (0..960)
            .map(|i| (((i as f32) * 0.11).sin() * 32_767.0) as i16)
            .collect();
        assert!(
            !is_decoder_blowup(&speech),
            "громкая речь не должна отбрасываться"
        );
        assert!(!is_decoder_blowup(&[0i16; 8]), "тишина — не глитч");
        assert!(
            !is_decoder_blowup(&[32_000i16, -32_000, 32_000, -32_000]),
            "слишком короткий фрагмент не проверяется"
        );
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
        assert_eq!(header.aad_len, 12 + 4 + 4, "AAD включает заголовок расширения");
        assert_eq!(header.extension_words, 1, "длина расширения потерялась");
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
            .encrypt(XsalsaNonce::from_slice(&nonce), b"opus-frame".as_slice())
            .unwrap();
        let mut packet = header.to_vec();
        packet.extend_from_slice(&body);
        let plain = decrypt_rtp(&packet, &key, EncryptionMode::Xsalsa20Poly1305)
            .expect("расшифровка должна пройти");
        assert_eq!(plain, b"opus-frame", "данные повреждены");

        let mut tampered = packet.clone();
        tampered[12] ^= 0xff;
        assert!(
            decrypt_rtp(&tampered, &key, EncryptionMode::Xsalsa20Poly1305).is_none(),
            "подмена должна ломать тег"
        );
    }

    /// Builds an AEAD rtpsize packet: header + ciphertext||tag + 4-byte nonce.
    fn rtpsize_packet(mode: EncryptionMode, header: &[u8], frame: &[u8], counter: [u8; 4]) -> Vec<u8> {
        let key = [0x24u8; 32];
        let sealed = match mode {
            EncryptionMode::AeadXChaCha20Poly1305Rtpsize => {
                let mut nonce = [0u8; 24];
                nonce[..4].copy_from_slice(&counter);
                XChaCha20Poly1305::new_from_slice(&key)
                    .unwrap()
                    .encrypt(XNonce::from_slice(&nonce), Payload { msg: frame, aad: header })
                    .unwrap()
            }
            EncryptionMode::AeadAes256GcmRtpsize => {
                let mut nonce = [0u8; 12];
                nonce[..4].copy_from_slice(&counter);
                Aes256Gcm::new_from_slice(&key)
                    .unwrap()
                    .encrypt(aes_gcm::Nonce::from_slice(&nonce), Payload { msg: frame, aad: header })
                    .unwrap()
            }
            other => panic!("не AEAD-режим: {other:?}"),
        };
        let mut packet = header.to_vec();
        packet.extend_from_slice(&sealed);
        packet.extend_from_slice(&counter);
        packet
    }

    #[test]
    fn rtpsize_nonce_comes_from_the_packet_tail() {
        let mut header = [0u8; 12];
        header[0] = 0x80;
        header[8..12].copy_from_slice(&9u32.to_be_bytes());
        let key = [0x24u8; 32];
        let packet = rtpsize_packet(
            EncryptionMode::AeadXChaCha20Poly1305Rtpsize,
            &header,
            b"opus-frame",
            [0, 0, 0, 7],
        );
        let plain = decrypt_rtp(&packet, &key, EncryptionMode::AeadXChaCha20Poly1305Rtpsize)
            .expect("xchacha rtpsize должен расшифровываться");
        assert_eq!(plain, b"opus-frame", "данные повреждены");

        let mut tampered = packet.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0xff;
        assert!(
            decrypt_rtp(&tampered, &key, EncryptionMode::AeadXChaCha20Poly1305Rtpsize).is_none(),
            "чужой счётчик nonce должен ломать тег"
        );
    }

    #[test]
    fn aes_gcm_rtpsize_roundtrip() {
        let mut header = [0u8; 12];
        header[0] = 0x80;
        header[8..12].copy_from_slice(&11u32.to_be_bytes());
        let key = [0x24u8; 32];
        let packet = rtpsize_packet(
            EncryptionMode::AeadAes256GcmRtpsize,
            &header,
            b"opus-frame",
            [0, 0, 1, 0],
        );
        let plain = decrypt_rtp(&packet, &key, EncryptionMode::AeadAes256GcmRtpsize)
            .expect("aes-gcm rtpsize должен расшифровываться");
        assert_eq!(plain, b"opus-frame", "данные повреждены");
    }

    #[test]
    fn mode_choice_prefers_modern_rtpsize() {
        let modes = json!([
            "xsalsa20_poly1305",
            "aead_aes256_gcm_rtpsize",
            "aead_xchacha20_poly1305_rtpsize"
        ]);
        let list = modes.as_array().unwrap().clone();
        assert_eq!(
            pick_mode(&list),
            EncryptionMode::AeadXChaCha20Poly1305Rtpsize,
            "предпочтителен xchacha rtpsize"
        );

        let aes = json!(["xsalsa20_poly1305", "aead_aes256_gcm_rtpsize"]);
        assert_eq!(
            pick_mode(aes.as_array().unwrap()),
            EncryptionMode::AeadAes256GcmRtpsize,
            "без xchacha берём AES-GCM"
        );

        let old = json!(["xsalsa20_poly1305_lite", "xsalsa20_poly1305"]);
        assert_eq!(
            pick_mode(old.as_array().unwrap()),
            EncryptionMode::Xsalsa20Poly1305,
            "старый сервер без AEAD"
        );

        assert_eq!(
            pick_mode(&[]),
            EncryptionMode::Xsalsa20Poly1305,
            "без списка режимов — xsalsa"
        );
    }

    #[test]
    fn binary_frames_drop_the_sequence_number() {
        let frame = [0x00, 0x2a, 25, 1, 2, 3];
        let (seq, op, payload) = parse_binary(&frame).expect("бинарный кадр должен разобраться");
        assert_eq!(seq, 42, "номер последовательности потерялся");
        assert_eq!(op, 25, "опкод должен идти после номера последовательности");
        assert_eq!(payload, vec![1, 2, 3], "полезная часть потерялась");
        assert!(parse_binary(&[0, 1]).is_none(), "короткий кадр не должен приниматься");
    }

    #[test]
    fn heartbeat_acknowledges_the_last_binary_frame() {
        let acked = heartbeat_payload(7, Some(42));
        assert_eq!(acked["op"], 3, "это должен быть heartbeat");
        assert_eq!(acked["d"]["t"], 7, "счётчик не попал в t");
        assert_eq!(acked["d"]["seq_ack"], 42, "номер последнего кадра не подтверждён");

        let plain = heartbeat_payload(8, None);
        assert_eq!(plain["d"]["t"], 8, "t нужен всегда");
        assert!(
            plain["d"].get("seq_ack").is_none(),
            "без бинарных кадров seq_ack не отправляется"
        );
    }

    #[test]
    fn dave_frame_detection_uses_the_magic_marker() {
        let mut packet = vec![7u8; 12];
        packet[10] = 0xfa;
        packet[11] = 0xfa;
        assert!(is_dave_frame(&packet), "маркер 0xFAFA должен распознаваться");

        packet[11] = 0;
        assert!(!is_dave_frame(&packet), "без маркера это обычный Opus-кадр");
        assert!(!is_dave_frame(&[0xfa, 0xfa]), "слишком короткий кадр не считается DAVE");
    }

    #[test]
    fn voice_close_codes_are_explained() {
        let reason = close_reason(4017);
        assert!(reason.contains("DAVE"), "4017 должен объяснять требование DAVE: {reason}");
        assert!(reason.contains("4017"), "код должен оставаться в тексте: {reason}");
        assert_eq!(close_reason(4999), "voice gateway closed: 4999", "неизвестный код как есть");
    }

    #[test]
    fn opus_decoder_handles_packet_loss() {
        // An empty packet is Opus packet-loss concealment; it must not error
        // even before the first real frame.
        let mut decoder = Decoder::new(48_000, Channels::Stereo).expect("декодер должен создаться");
        let mut pcm = vec![0i16; MAX_FRAME_SAMPLES * 2];
        assert!(decoder.decode(&[], &mut pcm, false).is_ok(), "PLC не должен падать");
    }
}
