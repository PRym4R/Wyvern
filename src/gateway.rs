use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use eframe::egui;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::messages::{ToApp, ToGateway};
use crate::models::{ChatChannel, Guild};
use crate::util::{client_properties, super_props};

mod parse;
mod rest;

use self::parse::parse_message_value;
use self::rest::{api_client, fetch_history_page, fetch_relationships, open_dm, send_message};

// Re-exported so the `crate::gateway::...` paths used by the test modules keep
// resolving without changes.
#[cfg(test)]
pub(crate) use self::parse::{parse_history_page, parse_history_page_lenient};
#[cfg(test)]
pub(crate) use self::rest::{
    client_with_timeout, history_url, more_history_available, next_before_id, send_failure_reason,
    send_message_to, API_TIMEOUT, HISTORY_PAGE,
};

const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
const API_BASE: &str = "https://discord.com/api/v10";

/// Connection counter: each gateway run gets its own generation. Needed on
/// account switch: a stale thread can reconnect and IDENTIFY with the old
/// token, so events from an outdated generation are ignored.
#[derive(Default)]
pub(crate) struct Generation(AtomicU64);

impl Generation {
    /// Generation for a new connection.
    pub(crate) fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }
    /// Whether this generation is still current.
    pub(crate) fn is_current(&self, mine: u64) -> bool {
        self.0.load(Ordering::SeqCst) == mine
    }
}

/// Event sender that goes silent once the app has switched accounts.
#[derive(Clone)]
pub(crate) struct EventTx {
    tx: mpsc::UnboundedSender<ToApp>,
    /// Generation this gateway belongs to.
    mine: u64,
    current: Arc<Generation>,
    /// App window: egui only repaints on demand, so a new event must wake it.
    wake: Option<egui::Context>,
}

impl EventTx {
    pub(crate) fn new(tx: mpsc::UnboundedSender<ToApp>, mine: u64, current: Arc<Generation>) -> Self {
        Self { tx, mine, current, wake: None }
    }
    /// Attach the window to wake on every event.
    pub(crate) fn with_wake(mut self, ctx: egui::Context) -> Self {
        self.wake = Some(ctx);
        self
    }
    /// Event reaches the app only if this gateway is still current; stale
    /// events would otherwise clobber the new account's state.
    pub(crate) fn send(&self, ev: ToApp) {
        if !self.current.is_current(self.mine) {
            return;
        }
        let _ = self.tx.send(ev);
        // Wake the window: another thread's event won't trigger a frame itself.
        if let Some(ctx) = &self.wake {
            ctx.request_repaint();
        }
    }
    /// Whether this gateway may still work.
    pub(crate) fn alive(&self) -> bool {
        self.current.is_current(self.mine)
    }
}

pub(crate) async fn run_gateway(
    mut cmd_rx: mpsc::UnboundedReceiver<ToGateway>,
    event_tx: EventTx,
    token: String,
) {
    let _ = event_tx.send(ToApp::Debug("Gateway thread started".into()));
    let mut session = SessionState::default();
    // Consecutive failed attempts; drives the backoff.
    let mut attempt: u32 = 0;
    loop {
        // The app switched accounts while we slept between attempts.
        if !event_tx.alive() {
            let _ = event_tx.send(ToApp::Debug("Gateway superseded, stopping".into()));
            return;
        }
        let use_resume = session.session_id.is_some();
        let started = std::time::Instant::now();
        match gw_inner(&mut cmd_rx, event_tx.clone(), &token, &mut session, use_resume).await {
            Ok(()) => {
                let _ = event_tx.send(ToApp::Debug("Gateway disconnected cleanly".into()));
                let _ = event_tx.send(ToApp::Status("Disconnected".into()));
                break;
            }
            Err(e) => {
                let fatal = e.downcast_ref::<GwClosed>().is_some_and(|c| c.fatal);
                let msg = e.to_string();
                let _ = event_tx.send(ToApp::Debug(format!("Gateway error: {}", msg)));
                if fatal {
                    // Discord rejected the token itself; retrying won't help.
                    // Return to the login screen with the reason.
                    let _ = event_tx.send(ToApp::AuthFailed { reason: msg });
                    break;
                }
                let _ = event_tx.send(ToApp::Status(format!("Reconnecting: {}", msg)));
                // The connection lived long, so treat the failure as isolated
                // and reset the backoff.
                if started.elapsed() >= Duration::from_secs(60) {
                    attempt = 0;
                }
                let delay = reconnect_delay_with_jitter(attempt, rand::random::<f64>());
                let _ = event_tx.send(ToApp::Debug(format!(
                    "Reconnect in {:?} (attempt {})",
                    delay,
                    attempt + 1
                )));
                attempt = attempt.saturating_add(1);
                // Sleep in small steps and check the generation so an account
                // switch stops the old gateway immediately.
                let mut left = delay;
                while !left.is_zero() {
                    if !event_tx.alive() {
                        let _ = event_tx.send(ToApp::Debug("Gateway superseded while waiting, stopping".into()));
                        return;
                    }
                    let step = left.min(Duration::from_millis(100));
                    time::sleep(step).await;
                    left -= step;
                }
            }
        }
    }
}

/// Gateway close that is pointless to retry: Discord rejected the token or
/// intents. Distinguishes it from a normal drop that should reconnect.
#[derive(Debug)]
struct GwClosed {
    message: String,
    fatal: bool,
}

impl std::fmt::Display for GwClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for GwClosed {}

/// Unanswered heartbeats allowed before closing the connection. One is
/// enough: the next heartbeat would go to a dead socket.
const HEARTBEAT_ACK_LIMIT: u32 = 1;

/// Heartbeat accounting: sent count without ACK; ACK resets it.
#[derive(Default)]
struct HeartbeatBook {
    unanswered: u32,
}

impl HeartbeatBook {
    /// Send the next heartbeat. `Err` means the previous one went unanswered,
    /// so the connection should be closed.
    fn tick(&mut self) -> Result<(), &'static str> {
        if self.unanswered >= HEARTBEAT_ACK_LIMIT {
            return Err("heartbeat остался без ACK");
        }
        self.unanswered += 1;
        Ok(())
    }

    /// ACK received — reset the counter.
    fn ack(&mut self) {
        self.unanswered = 0;
    }
}

/// Base backoff before attempt `attempt` (zero-based): 1, 2, 4, … capped at
/// 60 seconds.
fn reconnect_delay(attempt: u32) -> Duration {
    let secs = 1u64.checked_shl(attempt.min(6)).unwrap_or(64);
    Duration::from_secs(secs.min(60))
}

/// Backoff plus jitter up to a quarter of the base so clients don't
/// reconnect in lockstep. `roll` is 0.0–1.0.
fn reconnect_delay_with_jitter(attempt: u32, roll: f64) -> Duration {
    let base = reconnect_delay(attempt);
    let extra = base.as_secs_f64() * 0.25 * roll.clamp(0.0, 1.0);
    base + Duration::from_secs_f64(extra)
}

/// Meaning of a WebSocket close code: `Some` means fatal, do not retry.
fn close_fatal_reason(code: u16) -> Option<&'static str> {
    match code {
        4004 => Some("токен отклонён Discord: он недействителен"),
        4007 => Some("токен отозван"),
        4013 => Some("набор подписок (intents) неверный — Discord его не принимает"),
        4014 => Some("эти подписки (intents) запрещены для этого аккаунта"),
        _ => None,
    }
}

/// Extract the close code from the read task's marker message.
fn close_code_of(raw: &str) -> Option<u16> {
    raw.strip_prefix(CLOSE_MARK)?.parse().ok()
}

/// Markers from the WebSocket read task: it sees close and end-of-stream,
/// which the gateway loop otherwise only learns about via failed heartbeats.
const CLOSE_MARK: &str = "__CLOSE__";
const WS_ERROR_MARK: &str = "__WS_ERROR__";
/// Server closed the connection silently, without a close frame.
const EOF_MARK: &str = "__EOF__";

/// What the read task delivered: a Discord event or a marker.
enum RawFrame {
    /// Discord event — parsed as JSON.
    Event,
    /// Closed with a code (0 = none provided).
    Closed(u16),
    /// Stream ended without a close frame: ordinary drop, reconnect.
    Eof,
    /// Read error.
    WsError(String),
}

fn classify_raw(raw: &str) -> RawFrame {
    if raw == EOF_MARK {
        return RawFrame::Eof;
    }
    if let Some(code) = close_code_of(raw) {
        return RawFrame::Closed(code);
    }
    if let Some(e) = raw.strip_prefix(WS_ERROR_MARK) {
        return RawFrame::WsError(e.to_string());
    }
    RawFrame::Event
}

#[derive(Default)]
struct SessionState {
    session_id: Option<String>,
    seq: Option<i64>,
    /// "Channel, page" pairs with a history request in flight. Lives with the
    /// session, not one connection, so duplicate protection survives a reconnect.
    history_inflight: HistoryInflight,
}

/// Set of in-flight history requests, keyed by channel and page (`before`).
type HistoryInflight =
    std::sync::Arc<tokio::sync::Mutex<std::collections::HashSet<(String, Option<String>)>>>;

/// Claim a (channel, page) pair for a history request. `false` means one is
/// already in flight, so don't send a duplicate.
async fn claim_history(inflight: &HistoryInflight, key: (String, Option<String>)) -> bool {
    inflight.lock().await.insert(key)
}

/// Release the pair once the request finishes.
async fn release_history(inflight: &HistoryInflight, key: &(String, Option<String>)) {
    inflight.lock().await.remove(key);
}

/// IDENTIFY (op 2) handshake body. Uses the shared `client_properties` so the
/// WebSocket fingerprint matches `X-Super-Properties`.
fn identify_payload(token: &str) -> serde_json::Value {
    json!({
        "op": 2,
        "d": {
            "token": token,
            "properties": client_properties(),
            "intents": 327679,
            "presence": {
                "status": "online",
                "since": null,
                "activities": [],
                "afk": false
            }
        }
    })
}

async fn gw_inner(
    cmd_rx: &mut mpsc::UnboundedReceiver<ToGateway>,
    event_tx: EventTx,
    token: &str,
    session: &mut SessionState,
    use_resume: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let _ = event_tx.send(ToApp::Status("Connecting...".into()));
    if use_resume {
        let _ = event_tx.send(ToApp::Debug("Connecting to gateway (resume)...".into()));
    } else {
        let _ = event_tx.send(ToApp::Debug("Connecting to gateway...".into()));
    }

    let (ws_stream, _) = connect_async(GATEWAY_URL).await?;
    let (mut write, mut read) = ws_stream.split();

    let hello = read.next().await.ok_or("no hello")??;
    let hello: Value = match hello {
        WsMessage::Text(t) => serde_json::from_str(&t)?,
        _ => return Err("unexpected first message".into()),
    };
    let op = hello["op"].as_i64().unwrap_or(-1);
    if op != 10 {
        return Err(format!("expected op 10 hello, got op {}", op).into());
    }
    let interval = hello["d"]["heartbeat_interval"].as_u64().ok_or("no heartbeat_interval")?;
    let _ = event_tx.send(ToApp::Debug(format!("Hello received, interval={}ms", interval)));

    let identify = identify_payload(token);
    let first_payload = if use_resume {
        Some(json!({
            "op": 6,
            "d": {
                "token": token,
                "session_id": session.session_id.clone().unwrap_or_default(),
                "seq": session.seq,
            }
        }))
    } else {
        None
    };
    if let Some(payload) = &first_payload {
        write.send(WsMessage::Text(serde_json::to_string(payload)?.into())).await?;
        let _ = event_tx.send(ToApp::Debug("Resume sent".into()));
    } else {
        write.send(WsMessage::Text(serde_json::to_string(&identify)?.into())).await?;
        let _ = event_tx.send(ToApp::Debug("Identify sent".into()));
    }

    let (ws_tx, mut ws_rx) = mpsc::unbounded_channel::<WsMessage>();
    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<String>();

    tokio::spawn(async move {
        while let Some(msg) = read.next().await {
            match msg {
                Ok(WsMessage::Text(t)) => { let _ = raw_tx.send(t.to_string()); }
                Ok(WsMessage::Ping(d)) => { let _ = ws_tx.send(WsMessage::Pong(d)); }
                Ok(WsMessage::Close(c)) => {
                    // Send the close code itself, not the debug-rendered
                    // struct: `close_fatal_reason` decides whether to retry.
                    let code = c.map(|f| u16::from(f.code)).unwrap_or(0);
                    let _ = raw_tx.send(format!("__CLOSE__{}", code));
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    let _ = raw_tx.send(format!("{}{}", WS_ERROR_MARK, e));
                    break;
                }
            }
        }
        // Server stream ended. A silent end (no close frame) must be reported
        // immediately so the gateway doesn't keep heartbeating a dead socket.
        let _ = raw_tx.send(EOF_MARK.to_string());
    });

    // The client must have a timeout, or a stalled POST stops the gateway
    // forever (see `api_client`).
    let http = api_client()?;
    let tkn = token.to_string();
    let mut heartbeat = time::interval(Duration::from_millis(interval));
    heartbeat.tick().await;
    let mut seq: Option<i64> = session.seq;
    let mut hb = HeartbeatBook::default();

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                // Previous heartbeat went unanswered: close now rather than
                // wait through more failures.
                if let Err(why) = hb.tick() {
                    let _ = event_tx.send(ToApp::Debug(why.into()));
                    return Err("heartbeat ACK timeout".into());
                }
                let p = json!({ "op": 1, "d": seq });
                if write.send(WsMessage::Text(serde_json::to_string(&p).unwrap().into())).await.is_err() {
                    let _ = event_tx.send(ToApp::Debug("WebSocket write failed during heartbeat".into()));
                    return Err("heartbeat write failed".into());
                }
            }
            Some(ws_msg) = ws_rx.recv() => {
                let _ = write.send(ws_msg).await;
            }
            Some(raw) = raw_rx.recv() => {
                match classify_raw(&raw) {
                    RawFrame::Closed(code) => match close_fatal_reason(code) {
                        // Token rejected: mark fatal so `run_gateway` returns
                        // to the login screen instead of retrying forever.
                        Some(reason) => {
                            let _ = event_tx.send(ToApp::Debug(format!("WebSocket closed {}: {}", code, reason)));
                            return Err(Box::new(GwClosed {
                                message: format!("{} ({})", reason, code),
                                fatal: true,
                            }));
                        }
                        // Ordinary drop (network, sleep, Discord-side
                        // reconnect): retry.
                        None => {
                            let _ = event_tx.send(ToApp::Debug(format!("WebSocket closed: {}", code)));
                            return Err("websocket closed".into());
                        }
                    },
                    // Stream ended silently: reconnect now instead of
                    // heartbeating a dead socket.
                    RawFrame::Eof => {
                        let _ = event_tx.send(ToApp::Debug("WebSocket stream ended".into()));
                        return Err("websocket closed".into());
                    }
                    RawFrame::WsError(e) => {
                        let _ = event_tx.send(ToApp::Debug(format!("WebSocket error: {}", e)));
                        return Err("websocket error".into());
                    }
                    RawFrame::Event => {}
                }

                let v: Value = match serde_json::from_str(&raw) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                if let Some(s) = v["s"].as_i64() {
                    seq = Some(s);
                    session.seq = Some(s);
                }

                let op = v["op"].as_i64().unwrap_or(-1);
                match op {
                    0 => {
                        let t = v["t"].as_str().unwrap_or("");
                        match t {
                            "READY" => {
                                let d = &v["d"];
                                let u = d["user"]["username"].as_str().unwrap_or("?").to_string();
                                let uid = d["user"]["id"].as_str().unwrap_or("").to_string();
                                let avatar = d["user"]["avatar"].as_str().map(|s| s.to_string());
                                session.session_id = d["session_id"].as_str().map(|s| s.to_string());
                                session.seq = seq;
                                let _ = event_tx.send(ToApp::Ready { username: u, user_id: uid, avatar });

                                if let Some(guilds) = d["guilds"].as_array() {
                                    let _ = event_tx.send(ToApp::Debug(format!("READY: {} guilds", guilds.len())));
                                    for g in guilds {
                                        let gid = g["id"].as_str().unwrap_or("").to_string();
                                        if gid.is_empty() { continue; }
                                        let gname = g["name"].as_str().unwrap_or("Unknown").to_string();
                                        let gicon = g["icon"].as_str().map(|s| s.to_string());
                                        let _ = event_tx.send(ToApp::Guild(Guild {
                                            id: gid.clone(),
                                            name: gname.clone(),
                                            icon: gicon,
                                        }));
                                        let _ = event_tx.send(ToApp::Debug(format!("Will load channels for '{}'", &gname)));
                                    }

                                    let guilds = d["guilds"].as_array().cloned().unwrap_or_default();
                                    let egoods = event_tx.clone();
                                    let httpc = http.clone();
                                    let tkc = tkn.clone();
                                    tokio::spawn(async move {
                                        const CONCURRENCY: usize = 8;

                                        async fn fetch_one(
                                            httpc: reqwest::Client,
                                            tkc: String,
                                            egoods: EventTx,
                                            gid: String,
                                            gname: String,
                                        ) {
                                            for _attempt in 0..3 {
                                                let url = format!("{}/guilds/{}/channels", API_BASE, gid);
                                                let req = httpc.get(&url)
                                                    .header("Authorization", &*tkc)
                                                    .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36")
                                                    .header("X-Super-Properties", &super_props())
                                                    .header("X-Discord-Locale", "en-US")
                                                    .header("X-Discord-Timezone", "Europe/Moscow");
                                                match req.send().await {
                                                    Ok(resp) => {
                                                        let status = resp.status();
                                                        if status == 429 {
                                                            let retry = resp.headers()
                                                                .get("retry-after")
                                                                .and_then(|v| v.to_str().ok())
                                                                .and_then(|s| s.parse::<u64>().ok())
                                                                .unwrap_or(2);
                                                            let _ = egoods.send(ToApp::Debug(format!("429 for '{}', retrying in {}s", gname, retry)));
                                                            tokio::time::sleep(Duration::from_secs(retry)).await;
                                                            continue;
                                                        }
                                                        if status.is_success() {
                                                            match resp.text().await {
                                                                Ok(body) => {
                                                                    if let Ok(arr) = serde_json::from_str::<Vec<Value>>(&body) {
                                                                        let mut channels: Vec<ChatChannel> = Vec::new();
                                                                        for c in arr {
                                                                            let ctype = c["type"].as_i64().unwrap_or(0);
                                                                            if ctype == 0 || ctype == 11 || ctype == 12 {
                                                                                channels.push(ChatChannel {
                                                                                    id: c["id"].as_str().unwrap_or("").to_string(),
                                                                                    name: c["name"].as_str().unwrap_or("unknown").to_string(),
                                                                                    guild_id: Some(gid.clone()),
                                                                                    channel_type: ctype,
                                                                                    topic: c["topic"].as_str().map(|s| s.to_string()),
                                                                                    position: c["position"].as_i64().unwrap_or(0) as i32,
                                                                                });
                                                                            }
                                                                        }
                                                                        channels.sort_by_key(|c| c.position);
                                                                        let _ = egoods.send(ToApp::GuildChannels {
                                                                            guild_id: gid.clone(),
                                                                            channels,
                                                                        });
                                                                    }
                                                                }
                                                                Err(_) => {}
                                                            }
                                                            break;
                                                        } else if status == 403 {
                                                            let _ = egoods.send(ToApp::Debug(format!("No access to '{}', skipping", gname)));
                                                            break;
                                                        } else {
                                                            let _ = egoods.send(ToApp::Debug(format!("Guild '{}' channels error {}", gname, status)));
                                                            break;
                                                        }
                                                    }
                                                    Err(e) => {
                                                        let _ = egoods.send(ToApp::Debug(format!("Guild '{}' channels request error: {}", gname, e)));
                                                        break;
                                                    }
                                                }
                                            }
                                        }

                                        let mut guilds: Vec<(String, String)> = guilds.into_iter()
                                            .filter_map(|g| {
                                                let gid = g["id"].as_str().unwrap_or("").to_string();
                                                if gid.is_empty() { return None; }
                                                let gname = g["name"].as_str().unwrap_or("Unknown").to_string();
                                                Some((gid, gname))
                                            })
                                            .collect();

                                        while !guilds.is_empty() {
                                            let batch = guilds.drain(..guilds.len().min(CONCURRENCY)).collect::<Vec<_>>();
                                            let mut handles = Vec::new();
                                            for (gid, gname) in batch {
                                                handles.push(tokio::spawn(fetch_one(
                                                    httpc.clone(),
                                                    tkc.clone(),
                                                    egoods.clone(),
                                                    gid,
                                                    gname,
                                                )));
                                            }
                                            for h in handles {
                                                let _ = h.await;
                                            }
                                            tokio::time::sleep(Duration::from_millis(250)).await;
                                        }
                                    });
                                }

                                {
                                    let egoods = event_tx.clone();
                                    let httpc = http.clone();
                                    let tkc = tkn.clone();
                                    tokio::spawn(async move {
                                        let url = format!("{}/users/@me/relationships", API_BASE);
                                        match fetch_relationships(httpc, tkc, url).await {
                                            Ok(friends) => {
                                                let _ = egoods.send(ToApp::Friends(friends));
                                            }
                                            Err(e) => {
                                                let _ = egoods.send(ToApp::Debug(format!("Friends fetch failed: {}", e)));
                                            }
                                        }
                                    });
                                }

                                let priv_channels = d["private_channels"].as_array()
                                    .cloned()
                                    .or_else(|| d["channels"].as_array().cloned())
                                    .unwrap_or_default();
                                let _ = event_tx.send(ToApp::Debug(format!("READY: {} private channels", priv_channels.len())));
                                for ch in &priv_channels {
                                    let ctype = ch["type"].as_i64().unwrap_or(0);
                                    if ctype == 1 || ctype == 3 {
                                        let name = ch["name"].as_str()
                                            .filter(|s| !s.is_empty())
                                            .map(|s| s.to_string())
                                            .or_else(|| {
                                                ch["recipients"].as_array().map(|r| {
                                                    r.iter()
                                                        .filter_map(|u| u["username"].as_str().map(|s| s.to_string()))
                                                        .collect::<Vec<_>>()
                                                        .join(", ")
                                                }).filter(|s| !s.is_empty())
                                            })
                                            .unwrap_or_else(|| "DM".to_string());
                                        let _ = event_tx.send(ToApp::Channel(ChatChannel {
                                            id: ch["id"].as_str().unwrap_or("").to_string(),
                                            name,
                                            guild_id: None,
                                            channel_type: ctype,
                                            topic: None,
                                            position: 0,
                                        }));
                                    }
                                }
                            }
                            "RESUMED" => {
                                let _ = event_tx.send(ToApp::Debug("Session resumed".into()));
                                let _ = event_tx.send(ToApp::Status("Resumed".into()));
                            }
                            "MESSAGE_CREATE" => {
                                // Same parse as a history page, reusing the
                                // already-built event tree.
                                if let Some(msg) = parse_message_value(&v["d"], "") {
                                    let _ = event_tx.send(ToApp::Message(msg));
                                }
                            }
                            "MESSAGE_UPDATE" => {
                                // An edit arrives as the full message object;
                                // parse like create.
                                if let Some(msg) = parse_message_value(&v["d"], "") {
                                    let _ = event_tx.send(ToApp::MessageUpdated(msg));
                                }
                            }
                            "MESSAGE_DELETE" => {
                                let d = &v["d"];
                                let _ = event_tx.send(ToApp::MessageDeleted {
                                    channel_id: d["channel_id"].as_str().unwrap_or("").to_string(),
                                    message_id: d["id"].as_str().unwrap_or("").to_string(),
                                });
                            }
                            "MESSAGE_DELETE_BULK" => {
                                let d = &v["d"];
                                let ids: Vec<String> = d["ids"]
                                    .as_array()
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                let _ = event_tx.send(ToApp::MessageDeletedBulk {
                                    channel_id: d["channel_id"].as_str().unwrap_or("").to_string(),
                                    message_ids: ids,
                                });
                            }
                            "CHANNEL_UPDATE" => {
                                let d = &v["d"];
                                let _ = event_tx.send(ToApp::ChannelUpdated {
                                    channel_id: d["id"].as_str().unwrap_or("").to_string(),
                                    name: d["name"].as_str().map(|s| s.to_string()),
                                    topic: d["topic"].as_str().map(|s| s.to_string()),
                                });
                            }
                            "GUILD_CREATE" => {
                                let d = &v["d"];
                                let guild = Guild {
                                    id: d["id"].as_str().unwrap_or("").to_string(),
                                    name: d["name"].as_str().unwrap_or("Unknown").to_string(),
                                    icon: d["icon"].as_str().map(|s| s.to_string()),
                                };
                                let _ = event_tx.send(ToApp::Guild(guild));

                                if let Some(chs) = d["channels"].as_array() {
                                    for ch in chs {
                                        let cid = ch["id"].as_str().unwrap_or("").to_string();
                                        let cname = ch["name"].as_str().unwrap_or("unknown").to_string();
                                        let ctype = ch["type"].as_i64().unwrap_or(0);
                                        let topic = ch["topic"].as_str().map(|s| s.to_string());
                                        let pos = ch["position"].as_i64().unwrap_or(0) as i32;
                                        let _ = event_tx.send(ToApp::Channel(ChatChannel {
                                            id: cid,
                                            name: cname,
                                            guild_id: Some(d["id"].as_str().unwrap_or("").to_string()),
                                            channel_type: ctype,
                                            topic,
                                            position: pos,
                                        }));
                                    }
                                }
                            }
                            "CHANNEL_CREATE" | "DM_CHANNEL_CREATE" => {
                                let d = &v["d"];
                                let ctype = d["type"].as_i64().unwrap_or(0);
                                if ctype == 1 || ctype == 3 {
                                    let recipient = d["recipients"].as_array()
                                        .and_then(|r| r.first())
                                        .and_then(|r| r["username"].as_str())
                                        .unwrap_or("Unknown")
                                        .to_string();
                                    let _ = event_tx.send(ToApp::Channel(ChatChannel {
                                        id: d["id"].as_str().unwrap_or("").to_string(),
                                        name: recipient,
                                        guild_id: None,
                                        channel_type: ctype,
                                        topic: None,
                                        position: 0,
                                    }));
                                } else {
                                    let _ = event_tx.send(ToApp::Channel(ChatChannel {
                                        id: d["id"].as_str().unwrap_or("").to_string(),
                                        name: d["name"].as_str().unwrap_or("unknown").to_string(),
                                        guild_id: d["guild_id"].as_str().map(|s| s.to_string()),
                                        channel_type: ctype,
                                        topic: d["topic"].as_str().map(|s| s.to_string()),
                                        position: d["position"].as_i64().unwrap_or(0) as i32,
                                    }));
                                }
                            }
                            _ => {}
                        }
                    }
                    1 => {
                        let _ = event_tx.send(ToApp::Debug("Gateway requested heartbeat".into()));
                        // Server requested a heartbeat: send it now and restart
                        // the ACK wait.
                        hb.ack();
                        let _ = hb.tick();
                        let p = json!({ "op": 1, "d": seq });
                        if write.send(WsMessage::Text(serde_json::to_string(&p).unwrap().into())).await.is_err() {
                            break;
                        }
                    }
                    9 => {
                        let d = v["d"].as_bool().unwrap_or(false);
                        let _ = event_tx.send(ToApp::Debug(format!("Invalid session (resumable={})", d)));
                        if !d {
                            session.session_id = None;
                            session.seq = None;
                            return Err("Invalid session, re-identifying".into());
                        }
                        session.seq = seq;
                        return Err("Invalid session, keeping session for resume".into());
                    }
                    7 => {
                        let _ = event_tx.send(ToApp::Debug("Reconnect requested by gateway".into()));
                        return Err("Reconnect requested".into());
                    }
                    11 => {
                        hb.ack();
                    }
                    _ => {}
                }
            }
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    ToGateway::Send { channel_id, content, local_id, reply_to } => {
                        // Send in a separate task so the gateway keeps reading
                        // events and commands during the POST.
                        let httpc = http.clone();
                        let tkc = tkn.clone();
                        let ev = event_tx.clone();
                        tokio::spawn(async move {
                            send_message(httpc, tkc, ev, channel_id, content, local_id, reply_to).await;
                        });
                    }
                    ToGateway::FetchHistory { channel_id, before } => {
                        // Fetch history in a separate task; a request can make
                        // up to three API calls with backoff and would otherwise
                        // block the loop. Duplicates are skipped to avoid 429s.
                        let inflight = session.history_inflight.clone();
                        let httpc = http.clone();
                        let tkc = tkn.clone();
                        let ev = event_tx.clone();
                        let cid = channel_id.clone();
                        // Key is (channel, page), not just channel: loading
                        // older messages and reopening the channel are different
                        // pages and must not dedupe each other.
                        let key = (cid.clone(), before.clone());
                        let cid_short: String = cid.chars().take(14).collect();
                        tokio::spawn(async move {
                            if !claim_history(&inflight, key.clone()).await {
                                let _ = ev.send(ToApp::Debug(format!(
                                    "History for {} already in flight, skipping",
                                    cid_short
                                )));
                                return;
                            }
                            fetch_history_page(httpc, tkc, ev.clone(), cid.clone(), before).await;
                            release_history(&inflight, &key).await;
                        });
                    }
                    ToGateway::OpenDM { user_id } => {
                        // Same as sending: the request must not freeze the
                        // gateway loop.
                        let httpc = http.clone();
                        let tkc = tkn.clone();
                        let ev = event_tx.clone();
                        tokio::spawn(async move {
                            open_dm(httpc, tkc, ev, user_id).await;
                        });
                    }
                    ToGateway::Shutdown => {
                        let _ = event_tx.send(ToApp::Debug("Shutdown requested".into()));
                        return Ok(());
                    }
                }
            }
            else => break,
        }
    }

    Ok(())

}

#[cfg(test)]
include!("gateway_tests.rs");
