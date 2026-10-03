use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use eframe::egui;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::messages::{ToApp, ToGateway};
use crate::models::{image_size_of, size_from, Attachment, ChatChannel, ChatMessage, Embed, Guild, UserProfile};
use crate::util::{client_properties, super_props};

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

/// Messages per history page. Discord allows up to 100, but loading three
/// pages at once shows an empty channel; 50, then more on scroll.
pub(crate) const HISTORY_PAGE: usize = 50;

/// Send a message to a channel.
///
/// Runs as a separate task so the gateway loop keeps processing events and
/// commands during the POST. The client timeout is mandatory.
async fn send_message(
    httpc: reqwest::Client,
    tkn: String,
    event_tx: EventTx,
    channel_id: String,
    content: String,
    local_id: String,
) {
    let url = format!("{}/channels/{}/messages", API_BASE, channel_id);
    send_message_to(httpc, tkn, event_tx, url, content, local_id, channel_id).await;
}

/// Map a send failure to user-facing words. Raw codes and response bodies are
/// unusable, so only the actionable meaning is shown.
fn send_failure_reason(status: u16) -> &'static str {
    match status {
        403 => "в этот канал писать нельзя",
        404 => "канал не найден — возможно, прав на него нет",
        429 => "слишком много сообщений подряд, Discord просит подождать",
        // 413 = too large, 400 = e.g. over 2000 characters.
        400 | 413 => "Discord отклонил текст (скорее всего, длиннее 2000 символов)",
        _ => "Discord отклонил сообщение",
    }
}

/// Timeout for Discord API requests. Without it a stalled request leaves the
/// gateway unable to reconnect or accept commands.
const API_TIMEOUT: Duration = Duration::from_secs(20);

fn api_client() -> Result<reqwest::Client, String> {
    client_with_timeout(API_TIMEOUT)
}

fn client_with_timeout(timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("не собрать HTTP-клиент: {}", e))
}

/// Send a message to a ready URL. The URL is a parameter so tests can point
/// at a local socket.
async fn send_message_to(
    httpc: reqwest::Client,
    tkn: String,
    event_tx: EventTx,
    url: String,
    content: String,
    local_id: String,
    channel_id: String,
) {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_default();
    let req = httpc
        .post(&url)
        .header("Authorization", &*tkn)
        .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36")
        .header("X-Super-Properties", &super_props())
        .header("X-Discord-Locale", "en-US")
        .header("X-Discord-Timezone", "Europe/Moscow")
        .json(&json!({ "content": content, "nonce": nonce }));
    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                let _ = event_tx.send(ToApp::Debug(format!("Send failed {}: {}", status, body)));
                // Show the failure and drop the echo: otherwise it looks sent
                // forever.
                let _ = event_tx.send(ToApp::SendFailed {
                    channel_id,
                    local_id,
                    reason: send_failure_reason(status.as_u16()).to_string(),
                });
            } else {
                let _ = event_tx.send(ToApp::Debug("Message sent".into()));
            }
        }
        Err(e) => {
            let _ = event_tx.send(ToApp::Debug(format!("Send error: {}", e)));
            let _ = event_tx.send(ToApp::SendFailed {
                channel_id,
                local_id,
                reason: "не удалось отправить: нет связи с Discord".to_string(),
            });
        }
    }
}

/// Open a DM with a user. Separate task, like `send_message`, so the network
/// request doesn't stall the gateway loop.
async fn open_dm(
    httpc: reqwest::Client,
    tkn: String,
    event_tx: EventTx,
    user_id: String,
) {
    let url = format!("{}/users/@me/channels", API_BASE);
    let req = httpc
        .post(&url)
        .header("Authorization", &*tkn)
        .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36")
        .header("X-Super-Properties", &super_props())
        .header("X-Discord-Locale", "en-US")
        .header("X-Discord-Timezone", "Europe/Moscow")
        .json(&json!({ "recipient_id": user_id }));
    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                let _ = event_tx.send(ToApp::Debug(format!("Open DM failed {}", status)));
                return;
            }
            let Ok(body) = resp.text().await else { return };
            let Ok(d) = serde_json::from_str::<Value>(&body) else { return };
            let recipient = d["recipients"].as_array()
                .and_then(|r| r.first())
                .and_then(|r| r["username"].as_str())
                .unwrap_or("DM")
                .to_string();
            let _ = event_tx.send(ToApp::DMChannel(ChatChannel {
                id: d["id"].as_str().unwrap_or("").to_string(),
                name: recipient,
                guild_id: None,
                channel_type: 1,
                topic: None,
                position: 0,
            }));
        }
        Err(e) => {
            let _ = event_tx.send(ToApp::Debug(format!("Open DM error: {}", e)));
        }
    }
}

/// Fetch the friends list, retrying on 429 via `retry-after` because it races
/// the guild-channel requests. URL is a parameter for tests.
async fn fetch_relationships(
    httpc: reqwest::Client,
    tkn: String,
    url: String,
) -> Result<Vec<UserProfile>, String> {
    for _attempt in 0..3 {
        let req = httpc
            .get(&url)
            .header("Authorization", &*tkn)
            .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36")
            .header("X-Super-Properties", &super_props())
            .header("X-Discord-Locale", "en-US")
            .header("X-Discord-Timezone", "Europe/Moscow");
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status == 429 {
                    let retry = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(2);
                    tokio::time::sleep(Duration::from_secs(retry)).await;
                    continue;
                }
                if !status.is_success() {
                    return Err(format!("status {}", status));
                }
                let body = resp.text().await.map_err(|e| format!("body: {}", e))?;
                return parse_relationships(&body);
            }
            Err(e) => return Err(format!("request: {}", e)),
        }
    }
    Err("rate limited after 3 attempts".into())
}

/// Keep only accepted friends (`type` 1) from `/users/@me/relationships`:
/// the array also holds requests and blocks.
fn parse_relationships(body: &str) -> Result<Vec<UserProfile>, String> {
    let arr: Vec<Value> = serde_json::from_str(body).map_err(|e| format!("parse: {}", e))?;
    Ok(arr
        .into_iter()
        .filter_map(|r| {
            if r["type"].as_i64()? != 1 {
                return None;
            }
            let u = &r["user"];
            Some(UserProfile {
                id: u["id"].as_str().unwrap_or("").to_string(),
                username: u["username"].as_str().unwrap_or("?").to_string(),
            })
        })
        .collect())
}

/// Load one history page and send it to the UI. `before` is the oldest id on
/// screen; `None` loads the first page.
async fn fetch_history_page(
    httpc: reqwest::Client,
    tkn: String,
    event_tx: EventTx,
    channel_id: String,
    before: Option<String>,
) {
    let url = history_url(&channel_id, before.as_deref());

    let mut page: Vec<ChatMessage> = Vec::new();
    let mut got_page = false;
    let mut failed = false;
    let mut reason = String::new();
    let mut attempt = 0u32;
    while !got_page && attempt < 3 && !failed {
        attempt += 1;
        let req = httpc
            .get(&url)
            .header("Authorization", &*tkn)
            .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36")
            .header("X-Super-Properties", &super_props())
            .header("X-Discord-Locale", "en-US")
            .header("X-Discord-Timezone", "Europe/Moscow");
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                let _ = event_tx.send(ToApp::Debug(format!("History response: {}", status)));
                if status == 429 {
                    let retry = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(2);
                    let _ = event_tx.send(ToApp::Debug(format!("History 429, retrying in {}s", retry)));
                    reason = "Discord просит подождать (лимит запросов)".to_string();
                    time::sleep(Duration::from_secs(retry)).await;
                    continue;
                }
                if !status.is_success() {
                    let _ = event_tx.send(ToApp::Debug(format!("History error {}", status)));
                    // 403 = no channel access; retrying won't help, and the
                    // user sees an empty channel.
                    reason = if status == 403 {
                        "нет прав на канал".to_string()
                    } else {
                        format!("сервер ответил {}", status)
                    };
                    failed = true;
                    break;
                }
                match resp.text().await {
                    Ok(body) => {
                        let mut warn = |m: String| {
                            let _ = event_tx.send(ToApp::Debug(m));
                        };
                        page = parse_history_page_lenient(&body, &channel_id, &mut warn);
                        got_page = true;
                    }
                    Err(e) => {
                        let _ = event_tx.send(ToApp::Debug(format!("History body error: {}", e)));
                        reason = "не удалось прочитать ответ".to_string();
                    }
                }
            }
            Err(e) => {
                let _ = event_tx.send(ToApp::Debug(format!("History request error: {}", e)));
                reason = "нет связи с Discord".to_string();
                time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    if failed || !got_page {
        // Nothing arrived: report failure so the loading spinner clears
        // instead of waiting forever.
        if reason.is_empty() {
            reason = "история не пришла".to_string();
        }
        let _ = event_tx.send(ToApp::HistoryFailed { channel_id, before, reason });
        return;
    }

    let got = page.len();
    let more = more_history_available(got) && next_before_id(&page, before.as_deref()).is_some();
    let _ = event_tx.send(ToApp::Debug(format!(
        "History page: {} messages, more={} (before={})",
        got,
        more,
        before.as_deref().map(|b| &b[..b.len().min(14)]).unwrap_or("-")
    )));
    let event = if before.is_some() {
        ToApp::HistoryMore { channel_id, messages: page, more }
    } else {
        ToApp::History { channel_id, messages: page, more }
    };
    let _ = event_tx.send(event);
}

/// History page URL: no `before` for the first page, else from the oldest
/// received id.
pub(crate) fn history_url(channel_id: &str, before: Option<&str>) -> String {
    match before {
        Some(b) => format!(
            "{}/channels/{}/messages?limit={}&before={}",
            API_BASE, channel_id, HISTORY_PAGE, b
        ),
        None => format!("{}/channels/{}/messages?limit={}", API_BASE, channel_id, HISTORY_PAGE),
    }
}

/// Id of the oldest message on the page, used as the next `before`. `None` if
/// empty or repeating (paging looped).
pub(crate) fn next_before_id(page: &[ChatMessage], current: Option<&str>) -> Option<String> {
    let id = page.last()?.id.clone();
    if id.is_empty() || Some(id.as_str()) == current {
        return None;
    }
    Some(id)
}

/// Whether older messages can be loaded: a short page means history is
/// exhausted.
pub(crate) fn more_history_available(got: usize) -> bool {
    got >= HISTORY_PAGE
}

/// Deserialize a field that may not be a string (e.g. `content` can be a
/// number) by stringifying it, instead of dropping the whole page.
fn de_text<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<Value>::deserialize(d)?
        .map(|v| match v {
            Value::String(s) => s,
            other => other.to_string(),
        })
        .unwrap_or_default())
}

/// Like above, but a missing field or non-string yields `None`.
fn de_opt_text<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(Option::<Value>::deserialize(d)?.and_then(|v| match v {
        Value::String(s) => Some(s),
        _ => None,
    }))
}

/// A number that may arrive as a number or a string; a bad value must not
/// break message parsing.
fn de_opt_u32<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    Ok(Option::<Value>::deserialize(d)?.and_then(|v| match v {
        Value::Number(n) => n.as_u64().map(|x| x.min(u64::from(u32::MAX)) as u32),
        Value::String(s) => s.trim().parse::<u32>().ok(),
        _ => None,
    }))
}

/// Message author as the client renders it.
#[derive(serde::Deserialize)]
struct RawAuthor {
    #[serde(default, deserialize_with = "de_text")]
    id: String,
    #[serde(default, deserialize_with = "de_opt_text")]
    username: Option<String>,
    #[serde(default, deserialize_with = "de_opt_text")]
    avatar: Option<String>,
}

/// Attachment: only what is needed for display. `content_type` decides whether
/// to load the image; width/height reserve its space.
#[derive(serde::Deserialize)]
struct RawAttachment {
    #[serde(default, deserialize_with = "de_opt_text")]
    url: Option<String>,
    #[serde(default, deserialize_with = "de_opt_text")]
    content_type: Option<String>,
    #[serde(default, deserialize_with = "de_opt_text")]
    description: Option<String>,
    /// Image size in pixels, from `width`/`height` (the attachment `size`
    /// field is file size in bytes).
    #[serde(default, deserialize_with = "de_opt_u32")]
    width: Option<u32>,
    #[serde(default, deserialize_with = "de_opt_u32")]
    height: Option<u32>,
}

#[derive(serde::Deserialize)]
struct RawImage {
    #[serde(default, deserialize_with = "de_opt_text")]
    url: Option<String>,
    /// As with attachments: pixels come as `width`/`height`.
    #[serde(default, deserialize_with = "de_opt_u32")]
    width: Option<u32>,
    #[serde(default, deserialize_with = "de_opt_u32")]
    height: Option<u32>,
}

/// Embed: only fields the client shows. serde skips unknown fields without
/// allocating for them.
#[derive(serde::Deserialize)]
struct RawEmbed {
    #[serde(default, deserialize_with = "de_opt_text")]
    description: Option<String>,
    #[serde(default)]
    image: Option<RawImage>,
    #[serde(default)]
    thumbnail: Option<RawImage>,
    #[serde(default)]
    video: Option<RawImage>,
}

impl RawEmbed {
    fn into_embed(self) -> Option<Embed> {
        let image = [self.image, self.thumbnail, self.video]
            .into_iter()
            .flatten()
            .find(|i| i.url.as_deref().is_some_and(|u| !u.is_empty()));
        let image_url = image.as_ref().and_then(|i| i.url.clone());
        let image_size = image.and_then(|i| size_from(i.width, i.height));
        let description = match self.description {
            Some(s) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    None
                } else if trimmed.len() == s.len() {
                    // No trimming needed; keep the existing string.
                    Some(s)
                } else {
                    Some(trimmed.to_string())
                }
            }
            None => None,
        };
        if image_url.is_none() && description.is_none() {
            return None;
        }
        Some(Embed { image_url, image_size, description })
    }
}

/// Message as stored by the client.
#[derive(serde::Deserialize)]
struct RawMessage {
    #[serde(default, deserialize_with = "de_text")]
    id: String,
    #[serde(default, deserialize_with = "de_text")]
    content: String,
    #[serde(default, deserialize_with = "de_text")]
    timestamp: String,
    #[serde(default)]
    author: Option<RawAuthor>,
    #[serde(default)]
    attachments: Vec<RawAttachment>,
    #[serde(default)]
    embeds: Vec<RawEmbed>,
}

impl RawMessage {
    /// `None` for an authorless message, which is not shown.
    fn into_message(self, channel_id: &str) -> Option<ChatMessage> {
        let author = self.author?;
        Some(ChatMessage {
            id: self.id,
            channel_id: channel_id.to_string(),
            author_id: author.id,
            author_name: author.username.unwrap_or_else(|| "?".into()),
            author_avatar: author.avatar,
            nickname: None,
            content: self.content,
            timestamp: self.timestamp,
            attachments: self
                .attachments
                .into_iter()
                .filter_map(|a| {
                    Some(Attachment {
                        url: a.url?,
                        content_type: a.content_type,
                        description: a.description,
                        size: size_from(a.width, a.height),
                    })
                })
                .collect(),
            // Store embeds trimmed: the full JSON costs far more than the two
            // fields used.
            embeds: self.embeds.into_iter().filter_map(RawEmbed::into_embed).collect(),
            is_own: false,
        })
    }
}

/// Parse a history page from Discord JSON into messages.
///
/// Deserializes straight into the target structs rather than building a full
/// `Value` tree. Falls back to a lenient pass on type errors (see
/// [`parse_message_value`]) instead of losing the page.
pub(crate) fn parse_history_page_lenient(body: &str, channel_id: &str, warn: &mut dyn FnMut(String)) -> Vec<ChatMessage> {
    match parse_history_page(body, channel_id) {
        Ok(msgs) => msgs,
        Err(e) => match serde_json::from_str::<Vec<Value>>(body) {
            Ok(arr) => {
                warn(format!("History strict parse failed ({}), fallback used", e));
                arr.iter().filter_map(|m| parse_message_value(m, channel_id)).collect()
            }
            Err(e2) => {
                warn(format!("History parse error: {} / {}", e, e2));
                Vec::new()
            }
        },
    }
}

/// Strict page parse: format errors are returned to the caller.
pub(crate) fn parse_history_page(body: &str, channel_id: &str) -> Result<Vec<ChatMessage>, serde_json::Error> {
    let raw: Vec<RawMessage> = serde_json::from_str(body)?;
    Ok(raw.into_iter().filter_map(|m| m.into_message(channel_id)).collect())
}

/// Parse one message from an existing `Value` tree (live gateway events).
/// `fallback_channel` is used when the event has no `channel_id`.
pub(crate) fn parse_message_value(m: &Value, fallback_channel: &str) -> Option<ChatMessage> {
    let author = m.get("author")?;
    Some(ChatMessage {
        id: m["id"].as_str().unwrap_or("").to_string(),
        channel_id: m["channel_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(fallback_channel)
            .to_string(),
        author_id: author["id"].as_str().unwrap_or("").to_string(),
        author_name: author["username"].as_str().unwrap_or("?").to_string(),
        author_avatar: author["avatar"].as_str().map(|s| s.to_string()),
        nickname: None,
        content: m["content"].as_str().unwrap_or("").to_string(),
        timestamp: m["timestamp"].as_str().unwrap_or("").to_string(),
        attachments: m["attachments"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| {
                        Some(Attachment {
                            url: a["url"].as_str()?.to_string(),
                            content_type: a["content_type"].as_str().map(|s| s.to_string()),
                            description: a["description"].as_str().map(|s| s.to_string()),
                            size: image_size_of(a),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        embeds: m["embeds"]
            .as_array()
            .map(|arr| arr.iter().filter_map(Embed::from_json).collect())
            .unwrap_or_default(),
        is_own: false,
    })
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
                    ToGateway::Send { channel_id, content, local_id } => {
                        // Send in a separate task so the gateway keeps reading
                        // events and commands during the POST.
                        let httpc = http.clone();
                        let tkc = tkn.clone();
                        let ev = event_tx.clone();
                        tokio::spawn(async move {
                            send_message(httpc, tkc, ev, channel_id, content, local_id).await;
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
mod http_tests {
    use super::{
        api_client, client_with_timeout, fetch_relationships, send_failure_reason, send_message_to,
        EventTx, Generation, API_TIMEOUT,
    };
    use crate::messages::ToApp;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// The gateway client must have a sane timeout; a stalled request would
    /// otherwise block reconnection.
    #[test]
    fn api_client_has_a_sane_timeout() {
        assert!(API_TIMEOUT > Duration::from_secs(1), "слишком часто обрывать");
        assert!(
            API_TIMEOUT < Duration::from_secs(60),
            "настоящий ответ Discord столько не ждёт, а гейтвей столько молчит"
        );
        assert!(api_client().is_ok(), "клиент должен собираться");
    }

    /// Friends are fetched alongside guild channels and hit 429; verify on a
    /// real socket that the 429 is retried and only friends are kept.
    #[test]
    fn relationships_fetch_retries_after_429() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for attempt in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                if attempt == 0 {
                    let _ = stream.write_all(
                        b"HTTP/1.1 429 Too Many Requests\r\nretry-after: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                } else {
                    let body = r#"[{"id":"1","type":1,"user":{"id":"42","username":"friend"}},{"id":"2","type":2,"user":{"id":"43","username":"blocked"}}]"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes());
                }
                let _ = stream.flush();
            }
        });

        let client = api_client().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let friends = rt
            .block_on(fetch_relationships(
                client,
                "токен".into(),
                format!("http://{}/users/@me/relationships", addr),
            ))
            .expect("после 429 друзья должны догрузиться");
        assert_eq!(friends.len(), 1, "на экран идут только друзья: {friends:?}");
        assert_eq!(friends[0].username, "friend");
    }

    /// A send must not hang on a server that accepts the connection then goes
    /// silent; verified against a real unanswered socket.
    #[test]
    fn stalled_post_gives_up_instead_of_hanging() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Server accepts and never responds; keep the socket open, or the
        // client would see a disconnect and the test would pass on broken code.
        std::thread::spawn(move || {
            if let Ok((_stream, _)) = listener.accept() {
                std::thread::sleep(Duration::from_secs(10));
            }
        });

        let (tx, mut rx) = mpsc::unbounded_channel();
        // Wrap the receiver in an EventTx with a single generation so events
        // go straight through.
        let gen = Arc::new(Generation::default());
        let event_tx = EventTx::new(tx, gen.next(), gen.clone());
        // Shortened timeout so the test doesn't wait twenty seconds.
        let client = client_with_timeout(Duration::from_millis(300)).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let started = std::time::Instant::now();
        rt.block_on(send_message_to(
            client,
            "токен".into(),
            event_tx,
            format!("http://{}/channels/1/messages", addr),
            "привет".into(),
            "local:7".into(),
            "c1".into(),
        ));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "зависший POST должен обрываться, а не ждать: {:?}",
            started.elapsed()
        );
        // Connection loss must surface to the user, not just the debug log.
        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            seen.push(ev);
        }
        assert!(
            seen.iter().any(|e| matches!(e, ToApp::SendFailed { reason, .. } if reason.contains("нет связи"))),
            "пользователь должен узнать об отказе, получено: {seen:?}"
        );
    }

    /// Send failures are explained in words, not status codes.
    #[test]
    fn send_failure_reasons_are_readable() {
        for (status, must_contain) in [
            (403u16, "писать нельзя"),
            (404, "прав"),
            (429, "подождать"),
            (400, "2000"),
            (413, "2000"),
            (500, "отклонил"),
        ] {
            let reason = send_failure_reason(status);
            assert!(
                reason.contains(must_contain),
                "код {status}: должно быть про {must_contain:?}, а написано {reason:?}"
            );
            assert!(!reason.contains(&status.to_string()), "код не должен попадать в текст: {reason:?}");
        }
    }
}

#[cfg(test)]
mod generation_tests {
    use super::{EventTx, Generation};
    use crate::messages::ToApp;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    /// Switching accounts must silence the old gateway: it can otherwise
    /// reconnect with the old token and create two sessions.
    #[test]
    fn superseded_gateway_goes_silent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let gen = Arc::new(Generation::default());
        let old = EventTx::new(tx.clone(), gen.next(), gen.clone());
        // Account switched.
        let new = EventTx::new(tx, gen.next(), gen.clone());

        assert!(!old.alive(), "прежний гейтвей должен понять, что он больше не нужен");
        assert!(new.alive());
        old.send(ToApp::Debug("старое имя пользователя".into()));
        assert!(
            rx.try_recv().is_err(),
            "событие устаревшего гейтвея перетирало бы состояние нового"
        );

        new.send(ToApp::Debug("новое".into()));
        assert!(matches!(rx.try_recv(), Ok(ToApp::Debug(_))), "новый гейтвей должен говорить");
    }

    /// Generations must increase monotonically, or a new gateway would think
    /// it is stale and go silent.
    #[test]
    fn generations_increase_monotonically() {
        let gen = Generation::default();
        let first = gen.next();
        let second = gen.next();
        assert!(second > first, "поколения должны расти: {first} → {second}");
        assert!(gen.is_current(second));
        assert!(!gen.is_current(first), "прежнее поколение больше не актуально");
    }
}

#[cfg(test)]
mod session_tests {
    use super::{claim_history, release_history, SessionState};

    /// Duplicate-history protection must survive a reconnect: `run_gateway`
    /// passes one `SessionState` (and its `Arc`) into every `gw_inner`.
    #[tokio::test]
    async fn history_inflight_survives_a_reconnect() {
        let session = SessionState::default();
        let key = ("c1".to_string(), None);

        assert!(
            claim_history(&session.history_inflight, key.clone()).await,
            "первый запрос истории должен пройти"
        );
        // "Reconnect" is the same SessionState in the next gw_inner.
        assert!(
            !claim_history(&session.history_inflight, key.clone()).await,
            "после реконнекта защита от дублей пропала: запрос уйдёт второй раз"
        );

        // Request finished: release the pair so the next click passes.
        release_history(&session.history_inflight, &key).await;
        assert!(
            claim_history(&session.history_inflight, key).await,
            "после завершения запроса канал снова должен открываться"
        );
    }
}

#[cfg(test)]
mod identify_tests {
    use super::identify_payload;

    /// IDENTIFY must carry the real client fingerprint, the same object as
    /// `X-Super-Properties`.
    #[test]
    fn identify_carries_the_real_client_fingerprint() {
        let p = identify_payload("tok");
        assert_eq!(p["op"].as_i64(), Some(2), "это должен быть IDENTIFY");
        assert_eq!(
            p["d"]["properties"]["os"],
            std::env::consts::OS,
            "в IDENTIFY ушла чужая система"
        );
        assert_eq!(
            p["d"]["properties"],
            crate::util::client_properties(),
            "IDENTIFY и X-Super-Properties должны нести один отпечаток"
        );
    }
}

#[cfg(test)]
mod close_tests {
    use super::{close_code_of, close_fatal_reason, classify_raw, RawFrame, EOF_MARK, WS_ERROR_MARK};

    /// Token/intent refusal codes mean reconnecting changes nothing; they must
    /// be distinguished from ordinary drops.
    #[test]
    fn token_refusal_codes_are_fatal() {
        for code in [4004u16, 4007, 4013, 4014] {
            let reason = close_fatal_reason(code)
                .unwrap_or_else(|| panic!("код {code} должен быть фатальным"));
            assert!(!reason.is_empty(), "причина должна показываться пользователю");
        }
        // 4004 is the most common: invalid token.
        assert!(close_fatal_reason(4004).unwrap().contains("токен"));
    }

    /// Ordinary drops (network, sleep, Discord-side reconnect) must still
    /// reconnect.
    #[test]
    fn ordinary_close_codes_are_not_fatal() {
        for code in [0u16, 1000, 1001, 1006, 1011, 1012, 1013, 4000, 4008, 4011] {
            assert_eq!(close_fatal_reason(code), None, "код {code} — обычный обрыв");
        }
    }

    /// The read task reports the close code as a marker; an unparseable one is
    /// treated as an ordinary drop.
    #[test]
    fn close_code_is_taken_from_the_read_task() {
        assert_eq!(close_code_of("__CLOSE__4004"), Some(4004));
        assert_eq!(close_code_of("__CLOSE__1000"), Some(1000));
        assert_eq!(close_code_of("__WS_ERROR__broken pipe"), None);
        assert_eq!(close_code_of("{\"op\":0}"), None);
        // Garbage instead of a code must not become "fatal".
        assert_eq!(close_code_of("__CLOSE__мусор"), None);
    }

    /// A silent end of stream (no close frame) must be distinguished from an
    /// event so the gateway reconnects immediately.
    #[test]
    fn silent_end_of_stream_is_recognised() {
        assert!(matches!(classify_raw(EOF_MARK), RawFrame::Eof));
        // The marker must match what the read task sends; a typo would silently
        // disable reconnection.
        assert!(matches!(classify_raw(super::EOF_MARK), RawFrame::Eof));
    }

    /// The other markers must not be confused with Discord events.
    #[test]
    fn other_frames_stay_distinct() {
        assert!(matches!(classify_raw("__CLOSE__4004"), RawFrame::Closed(4004)));
        assert!(matches!(classify_raw("__CLOSE__1000"), RawFrame::Closed(1000)));
        assert!(matches!(classify_raw(&format!("{}broken pipe", WS_ERROR_MARK)), RawFrame::WsError(_)));
        // A real event stays an event even if its data contains "__CLOSE__".
        assert!(matches!(classify_raw("{\"op\":0,\"t\":\"__CLOSE__4004\"}"), RawFrame::Event));
        assert!(matches!(classify_raw("{\"op\":11,\"d\":null}"), RawFrame::Event));
    }
}

#[cfg(test)]
mod parse_tests {
    use super::{parse_history_page, parse_history_page_lenient, parse_message_value};

    /// A message as Discord sends it, with many fields the client ignores.
    const REAL: &str = r#"[{
        "id": "1200000000000000001",
        "channel_id": "900000000000000000",
        "content": "привет",
        "timestamp": "2026-09-26T12:00:00.000000+00:00",
        "type": 0,
        "pinned": false,
        "mention_everyone": false,
        "edited_timestamp": null,
        "flags": 0,
        "author": {
            "id": "800000000000000000",
            "username": "vasya",
            "discriminator": "0",
            "avatar": "abc123",
            "global_name": "Вася",
            "bot": false
        },
        "attachments": [{
            "id": "1100000000000000000",
            "filename": "photo.png",
            "size": 123456,
            "width": 1600,
            "height": 1200,
            "content_type": "image/png",
            "description": "схема из чата",
            "url": "https://cdn.discordapp.com/attachments/1/photo.png",
            "proxy_url": "https://media.discordapp.net/attachments/1/photo.png"
        }],
        "embeds": [{
            "type": "rich",
            "title": "заголовок",
            "author": {"name": "Кто-то", "url": "https://example.com"},
            "footer": {"text": "подпись"},
            "provider": {"name": " twitch"},
            "image": {"url": "https://cdn.discordapp.com/embeds/1/picture.png", "width": 800, "height": 600},
            "fields": [{"name": "a", "value": "b", "inline": true}]
        }],
        "reaction_counts": [{"count": 1, "me": false}],
        "mentions": []
    }]"#;

    fn parse_one_in(body: &str, channel_id: &str) -> crate::models::ChatMessage {
        let mut msgs = parse_history_page(body, channel_id).expect("разбор не должен падать");
        assert_eq!(msgs.len(), 1, "ожидалось одно сообщение");
        msgs.pop().unwrap()
    }

    fn parse_one(body: &str) -> crate::models::ChatMessage {
        parse_one_in(body, "fallback")
    }

    #[test]
    fn history_page_maps_all_used_fields() {
        let m = parse_one(REAL);
        assert_eq!(m.id, "1200000000000000001");
        assert_eq!(m.channel_id, "fallback", "канал страницы важнее поля в сообщении");
        assert_eq!(m.author_id, "800000000000000000");
        assert_eq!(m.author_name, "vasya");
        assert_eq!(m.author_avatar.as_deref(), Some("abc123"));
        assert_eq!(m.content, "привет");
        assert_eq!(m.timestamp, "2026-09-26T12:00:00.000000+00:00");
        assert!(!m.is_own);
        assert!(m.nickname.is_none());

        assert_eq!(m.attachments.len(), 1);
        let a = &m.attachments[0];
        assert_eq!(a.url, "https://cdn.discordapp.com/attachments/1/photo.png");
        assert_eq!(a.content_type.as_deref(), Some("image/png"));
        assert_eq!(a.description.as_deref(), Some("схема из чата"));
        // Pixels come from `width`/`height`; `size` is file size in bytes.
        assert_eq!(a.size, Some([1600, 1200]), "размер картинки должен доходить из истории");

        assert_eq!(m.embeds.len(), 1);
        assert_eq!(
            m.embeds[0].image_url.as_deref(),
            Some("https://cdn.discordapp.com/embeds/1/picture.png")
        );
        assert_eq!(m.embeds[0].image_size, Some([800, 600]), "размер картинки эмбеда — тоже");
        assert_eq!(m.embeds[0].description, None, "у эмбеда нет description — выкидываем пустое");
    }

    /// History and live messages must parse identically, except the channel:
    /// a page takes the requested channel, a live message its own `channel_id`.
    #[test]
    fn history_and_live_parse_agree() {
        let from_history = parse_one_in(REAL, "900000000000000000");
        let from_live = parse_message_value(
            &serde_json::from_str::<serde_json::Value>(REAL).unwrap()[0],
            "900000000000000000",
        )
        .expect("живое сообщение должно разобраться");
        assert_eq!(from_history.id, from_live.id);
        assert_eq!(from_history.channel_id, from_live.channel_id);
        assert_eq!(from_history.author_id, from_live.author_id);
        assert_eq!(from_history.author_name, from_live.author_name);
        assert_eq!(from_history.author_avatar, from_live.author_avatar);
        assert_eq!(from_history.content, from_live.content);
        assert_eq!(from_history.timestamp, from_live.timestamp);
        assert_eq!(from_history.attachments.len(), from_live.attachments.len());
        assert_eq!(from_history.attachments[0].url, from_live.attachments[0].url);
        assert_eq!(
            from_history.attachments[0].description, from_live.attachments[0].description
        );
        // Image size must match too; the two paths used to diverge here.
        assert_eq!(from_history.attachments[0].size, from_live.attachments[0].size);
        assert_eq!(from_history.attachments[0].size, Some([1600, 1200]));
        assert_eq!(from_history.embeds.len(), from_live.embeds.len());
        assert_eq!(from_history.embeds[0].image_url, from_live.embeds[0].image_url);
        assert_eq!(from_history.embeds[0].image_size, from_live.embeds[0].image_size);
    }

    /// A live message uses its own channel; fall back if the field is absent.
    #[test]
    fn live_message_uses_own_channel_then_fallback() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"id":"1","channel_id":"chan-42","content":"x","author":{"id":"u","username":"n"}}"#).unwrap();
        let m = parse_message_value(&v, "fallback").unwrap();
        assert_eq!(m.channel_id, "chan-42");
        let v2: serde_json::Value =
            serde_json::from_str(r#"{"id":"1","content":"x","author":{"id":"u","username":"n"}}"#).unwrap();
        assert_eq!(parse_message_value(&v2, "fallback").unwrap().channel_id, "fallback");
    }

    /// A message without an author is not shown.
    #[test]
    fn message_without_author_is_skipped() {
        let body = r#"[{"id":"1","content":"системное","author":{"id":"u","username":"n"}},{"id":"2","content":"без автора"}]"#;
        let msgs = parse_history_page(body, "c").unwrap();
        assert_eq!(msgs.len(), 1, "сообщение без author пропускается");
        assert_eq!(msgs[0].id, "1");
    }

    /// A bad value in one field must not drop the whole page.
    #[test]
    fn odd_field_types_do_not_break_the_page() {
        let body = r#"[{"id":7,"content":12345,"timestamp":null,
                        "author":{"id":"u","username":null,"avatar":null}},
                       {"id":"8","content":"ок","author":{"id":"u2","username":"n"}}]"#;
        let msgs = parse_history_page(body, "c").unwrap();
        assert_eq!(msgs.len(), 2, "оба сообщения должны остаться");
        assert_eq!(msgs[0].id, "7");
        assert_eq!(msgs[0].content, "12345");
        assert_eq!(msgs[0].timestamp, "");
        assert_eq!(msgs[0].author_name, "?", "нет username — как раньше показываем «?»");
        assert!(msgs[0].author_avatar.is_none());
    }

    /// A completely empty object must not break the page.
    #[test]
    fn empty_and_missing_fields_survive() {
        let msgs = parse_history_page(r#"[{}]"#, "c").unwrap();
        assert!(msgs.is_empty(), "без автора сообщение не показываем");
        let msgs = parse_history_page(r#"[]"#, "c").unwrap();
        assert!(msgs.is_empty());
        assert_eq!(parse_history_page("не json", "c").is_err(), true, "битый JSON — ошибка разбора");
    }

    /// Embed descriptions are trimmed; untrimmed ones are kept as-is.
    #[test]
    fn embed_description_is_trimmed() {
        let body = r#"[{"id":"1","content":"","author":{"id":"u","username":"n"},
                        "embeds":[{"description":"  текст  "},{"description":"   "}]}]"#;
        let msgs = parse_history_page(body, "c").unwrap();
        assert_eq!(msgs[0].embeds.len(), 1, "эмбед из одних пробелов выкидываем");
        assert_eq!(msgs[0].embeds[0].description.as_deref(), Some("текст"));
    }

    /// An attachment without a url is dropped, not a page failure.
    #[test]
    fn attachment_without_url_is_dropped() {
        let body = r#"[{"id":"1","content":"","author":{"id":"u","username":"n"},
                        "attachments":[{"filename":"x.png"},{"url":"https://cdn.discordapp.com/a/1.png"}]}]"#;
        let msgs = parse_history_page(body, "c").unwrap();
        assert_eq!(msgs[0].attachments.len(), 1);
        assert_eq!(msgs[0].attachments[0].url, "https://cdn.discordapp.com/a/1.png");
    }

    /// Fallback parse: an unexpected shape (e.g. non-object `author`) still
    /// shows the page; the strict pass fails but the page must not vanish.
    #[test]
    fn lenient_parse_survives_unexpected_shape() {
        let body = r#"[{"id":"1","content":"строка вместо объекта","author":"bob"},
                       {"id":"2","content":"нормальное","author":{"id":"u","username":"n"}}]"#;
        assert!(
            parse_history_page(body, "c").is_err(),
            "строгий разбор на этом должен ругаться — иначе тест бессмыслен"
        );
        let mut warns = Vec::new();
        let msgs = parse_history_page_lenient(body, "c", &mut |m| warns.push(m));
        assert_eq!(msgs.len(), 2, "оба сообщения должны показаться");
        assert_eq!(msgs[0].id, "1");
        assert_eq!(msgs[0].content, "строка вместо объекта");
        assert_eq!(msgs[1].author_name, "n");
        assert_eq!(warns.len(), 1, "о разборе запасным путём пишем в лог");
        assert!(warns[0].contains("fallback"), "лог должен говорить, что это запасной путь: {}", warns[0]);
    }

    /// Unreadable input must not panic: an empty list and a clear log line.
    #[test]
    fn lenient_parse_reports_garbage() {
        let mut warns = Vec::new();
        let msgs = parse_history_page_lenient("не json", "c", &mut |m| warns.push(m));
        assert!(msgs.is_empty());
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("History parse error"), "{}", warns[0]);
    }
}

#[cfg(test)]
mod heartbeat_tests {
    use super::{reconnect_delay, reconnect_delay_with_jitter, HeartbeatBook};
    use std::time::Duration;

    /// One unanswered heartbeat is enough to drop the connection.
    #[test]
    fn one_missed_heartbeat_is_enough_to_reconnect() {
        let mut hb = HeartbeatBook::default();
        assert!(hb.tick().is_ok(), "первый heartbeat уходит");
        assert!(hb.tick().is_err(), "без ACK второй уже рвёт соединение");
        // ACK clears the wait.
        hb.ack();
        assert!(hb.tick().is_ok());
        assert!(hb.tick().is_err());
    }

    /// Backoff grows exponentially and is capped.
    #[test]
    fn reconnect_backoff_grows_and_is_capped() {
        assert_eq!(reconnect_delay(0), Duration::from_secs(1));
        assert_eq!(reconnect_delay(1), Duration::from_secs(2));
        assert_eq!(reconnect_delay(2), Duration::from_secs(4));
        assert_eq!(reconnect_delay(5), Duration::from_secs(32));
        assert_eq!(reconnect_delay(6), Duration::from_secs(60));
        // No growth or overflow beyond this.
        assert_eq!(reconnect_delay(50), Duration::from_secs(60));
    }

    /// Jitter is at most a quarter of the base and clamps at the edges.
    #[test]
    fn reconnect_jitter_stays_within_a_quarter() {
        let base = reconnect_delay(3); // 8 s
        assert_eq!(reconnect_delay_with_jitter(3, 0.0), base);
        assert_eq!(reconnect_delay_with_jitter(3, 1.0), base + Duration::from_secs(2));
        assert_eq!(reconnect_delay_with_jitter(3, -5.0), base);
        assert_eq!(reconnect_delay_with_jitter(3, 5.0), base + Duration::from_secs(2));
    }
}
