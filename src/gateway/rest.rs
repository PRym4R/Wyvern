//! Discord REST/HTTP calls: sending messages, DMs, friends, and history pages.

use std::time::Duration;

use serde_json::{json, Value};
use tokio::time;

use crate::messages::ToApp;
use crate::models::{ChatChannel, ChatMessage, UserProfile};
use crate::util::super_props;

use super::parse::{parse_history_page_lenient, parse_search_results};
use super::ratelimit;
use super::{EventTx, API_BASE};

/// Messages per history page. Discord allows up to 100, but loading three
/// pages at once shows an empty channel; 50, then more on scroll.
pub(crate) const HISTORY_PAGE: usize = 50;

/// Results per guild search answer: enough for a list, cheap on the limit.
pub(crate) const SEARCH_PAGE: usize = 25;

/// Messages fetched around a jumped-to hit, the same order of magnitude as a
/// history page so the context above and below is readable.
pub(crate) const AROUND_PAGE: usize = 25;

/// Browser fingerprint Discord expects on every REST call. A user token
/// without it gets flagged, so the headers live in one place: a new endpoint
/// cannot forget one.
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

fn browser_headers(req: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
    req.header("Authorization", token)
        .header("User-Agent", USER_AGENT)
        .header("X-Super-Properties", super_props())
        .header("X-Discord-Locale", "en-US")
        .header("X-Discord-Timezone", "Europe/Moscow")
}

/// Send a message to a channel.
///
/// Runs as a separate task so the gateway loop keeps processing events and
/// commands during the POST. The client timeout is mandatory.
pub(super) async fn send_message(
    httpc: reqwest::Client,
    tkn: String,
    event_tx: EventTx,
    channel_id: String,
    content: String,
    local_id: String,
    reply_to: Option<String>,
) {
    let url = format!("{}/channels/{}/messages", API_BASE, channel_id);
    send_message_to(
        httpc, tkn, event_tx, url, content, local_id, channel_id, reply_to,
    )
    .await;
}

/// Map a send failure to user-facing words. Raw codes and response bodies are
/// unusable, so only the actionable meaning is shown.
pub(crate) fn send_failure_reason(status: u16) -> &'static str {
    match status {
        403 => "you can't write in this channel",
        404 => "channel not found — you may not have access to it",
        429 => "too many messages in a row, Discord asks you to wait",
        // 413 = too large, 400 = e.g. over 2000 characters.
        400 | 413 => "Discord rejected the text (most likely longer than 2000 characters)",
        _ => "Discord rejected the message",
    }
}

/// Timeout for Discord API requests. Without it a stalled request leaves the
/// gateway unable to reconnect or accept commands.
pub(crate) const API_TIMEOUT: Duration = Duration::from_secs(20);

pub(super) fn api_client() -> Result<reqwest::Client, String> {
    client_with_timeout(API_TIMEOUT)
}

pub(crate) fn client_with_timeout(timeout: Duration) -> Result<reqwest::Client, String> {
    crate::net::with_proxy(reqwest::Client::builder())
        .timeout(timeout)
        .build()
        .map_err(|e| format!("failed to build HTTP client: {}", e))
}

/// Attempts per send before the composer gets the text back. Three covers a
/// rate limit or a blip; more would leave the echo hanging for too long.
const SEND_ATTEMPTS: u32 = 3;

/// Whether a status is worth another attempt: Discord throttling us or a
/// temporary outage. A 400/403/404 means the message itself was refused, and
/// retrying the same text would only annoy the user.
pub(crate) fn send_retryable(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

/// Backoff between attempts, doubling from half a second and capped at four.
/// The rate limiter already waits when Discord says how long; this covers a
/// blind `5xx`.
pub(crate) fn send_retry_delay(attempt: u32) -> Duration {
    let shifted = attempt.saturating_sub(1).min(3);
    Duration::from_millis(500u64 << shifted)
}

/// Send a message to a ready URL. The URL is a parameter so tests can point
/// at a local socket.
pub(crate) async fn send_message_to(
    httpc: reqwest::Client,
    tkn: String,
    event_tx: EventTx,
    url: String,
    content: String,
    local_id: String,
    channel_id: String,
    reply_to: Option<String>,
) {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_default();
    // A reply adds `message_reference`; Discord needs the replied id and the
    // channel it lives in. Ordinary sends keep the old two-field body.
    let mut body = json!({ "content": content, "nonce": nonce });
    if let Some(reference) = reply_to.as_deref() {
        body["message_reference"] = json!({
            "message_id": reference,
            "channel_id": channel_id.as_str(),
        });
    }
    let route = format!("messages:{}", channel_id);
    // The text stays in the echo until an attempt finally fails: a `429` or a
    // dropped connection is retried instead of handing the draft back.
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        ratelimit::wait(&route).await;
        let req = browser_headers(httpc.post(&url), &tkn).json(&body);
        match req.send().await {
            Ok(resp) => {
                let limit = ratelimit::note(&route, resp.headers());
                let status = resp.status();
                if status.is_success() {
                    let _ = event_tx.send(ToApp::Debug("Message sent".into()));
                    return;
                }
                let body = resp.text().await.unwrap_or_default();
                let _ = event_tx.send(ToApp::Debug(format!("Send failed {}: {}", status, body)));
                if send_retryable(status.as_u16()) && attempt < SEND_ATTEMPTS {
                    // A rate limit is already remembered by the limiter; only a
                    // blind `5xx` needs a backoff of our own.
                    if limit.is_none() {
                        tokio::time::sleep(send_retry_delay(attempt)).await;
                    }
                    continue;
                }
                // Show the failure and drop the echo: otherwise it looks sent
                // forever.
                let _ = event_tx.send(ToApp::SendFailed {
                    channel_id,
                    local_id,
                    reason: send_failure_reason(status.as_u16()).to_string(),
                });
                return;
            }
            Err(e) => {
                let _ = event_tx.send(ToApp::Debug(format!("Send error: {}", e)));
                if attempt < SEND_ATTEMPTS {
                    tokio::time::sleep(send_retry_delay(attempt)).await;
                    continue;
                }
                let _ = event_tx.send(ToApp::SendFailed {
                    channel_id,
                    local_id,
                    reason: "couldn't send: no connection to Discord".to_string(),
                });
                return;
            }
        }
    }
}

/// URL of one message, used by edit and delete.
pub(crate) fn message_url(channel_id: &str, message_id: &str) -> String {
    format!(
        "{}/channels/{}/messages/{}",
        API_BASE, channel_id, message_id
    )
}

/// Why an edit or delete was refused, in words; raw statuses never reach the
/// user.
pub(crate) fn message_action_reason(status: u16) -> &'static str {
    match status {
        403 => "no permission for this message",
        404 => "message is already deleted",
        429 => "Discord asks you to wait (rate limit)",
        _ => "Discord rejected the request",
    }
}

/// Edit a message's text. The new text is applied only by the MESSAGE_UPDATE
/// event, so a rejected edit leaves the row untouched.
pub(super) async fn edit_message(
    httpc: reqwest::Client,
    tkn: String,
    event_tx: EventTx,
    channel_id: String,
    message_id: String,
    content: String,
) {
    let url = message_url(&channel_id, &message_id);
    edit_message_to(httpc, tkn, event_tx, url, channel_id, message_id, content).await;
}

/// Edit against a ready URL; the URL is a parameter so tests can use a local
/// socket.
pub(crate) async fn edit_message_to(
    httpc: reqwest::Client,
    tkn: String,
    event_tx: EventTx,
    url: String,
    channel_id: String,
    message_id: String,
    content: String,
) {
    let route = format!("edit:{}", channel_id);
    ratelimit::wait(&route).await;
    let req = browser_headers(httpc.patch(&url), &tkn).json(&json!({ "content": content }));
    match req.send().await {
        Ok(resp) => {
            let _ = ratelimit::note(&route, resp.headers());
            let status = resp.status();
            if status.is_success() {
                let _ = event_tx.send(ToApp::Debug("Message edited".into()));
            } else {
                let body = resp.text().await.unwrap_or_default();
                let _ = event_tx.send(ToApp::Debug(format!("Edit failed {}: {}", status, body)));
                let _ = event_tx.send(ToApp::EditFailed {
                    channel_id,
                    message_id,
                    content,
                    reason: message_action_reason(status.as_u16()).to_string(),
                });
            }
        }
        Err(e) => {
            let _ = event_tx.send(ToApp::Debug(format!("Edit error: {}", e)));
            let _ = event_tx.send(ToApp::EditFailed {
                channel_id,
                message_id,
                content,
                reason: "couldn't edit: no connection to Discord".to_string(),
            });
        }
    }
}

/// URL for one reaction on one message. `emoji` is already the API key
/// (`name` or `name:id`), percent-encoded for the path.
pub(crate) fn reaction_url(channel_id: &str, message_id: &str, emoji: &str) -> String {
    format!(
        "{}/channels/{}/messages/{}/reactions/{}/@me",
        API_BASE,
        channel_id,
        message_id,
        encode_path_segment(emoji)
    )
}

/// Percent-encode one path segment. A unicode emoji is multi-byte UTF-8 and
/// would otherwise produce an invalid URL; `:` must survive so a custom emoji
/// stays `name:id`.
pub(crate) fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b':' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

/// Add (`add`) or remove our own reaction. The UI has already toggled the pill
/// optimistically; a rejection comes back as `ReactionFailed` so it can undo.
pub(super) async fn set_reaction(
    httpc: reqwest::Client,
    tkn: String,
    event_tx: EventTx,
    channel_id: String,
    message_id: String,
    emoji: String,
    add: bool,
) {
    let url = reaction_url(&channel_id, &message_id, &emoji);
    let route = format!("reactions:{}", channel_id);
    ratelimit::wait(&route).await;
    let req = browser_headers(httpc.request(if add { reqwest::Method::PUT } else { reqwest::Method::DELETE }, &url), &tkn);
    let result = req.send().await;
    match result {
        Ok(resp) => {
            let _ = ratelimit::note(&route, resp.headers());
            let status = resp.status();
            if !status.is_success() {
                let _ = event_tx.send(ToApp::Debug(format!("Reaction failed {}", status)));
                let _ = event_tx.send(ToApp::ReactionFailed {
                    channel_id,
                    message_id,
                    emoji,
                    add,
                });
            }
        }
        Err(e) => {
            let _ = event_tx.send(ToApp::Debug(format!("Reaction error: {}", e)));
            let _ = event_tx.send(ToApp::ReactionFailed {
                channel_id,
                message_id,
                emoji,
                add,
            });
        }
    }
}

/// Delete a message. The row disappears on MESSAGE_DELETE, not here.
pub(super) async fn delete_message(
    httpc: reqwest::Client,
    tkn: String,
    event_tx: EventTx,
    channel_id: String,
    message_id: String,
) {
    let url = message_url(&channel_id, &message_id);
    delete_message_to(httpc, tkn, event_tx, url, channel_id).await;
}

/// Delete against a ready URL; the URL is a parameter so tests can use a local
/// socket.
pub(crate) async fn delete_message_to(
    httpc: reqwest::Client,
    tkn: String,
    event_tx: EventTx,
    url: String,
    channel_id: String,
) {
    let route = format!("delete:{}", channel_id);
    ratelimit::wait(&route).await;
    let req = browser_headers(httpc.delete(&url), &tkn);
    match req.send().await {
        Ok(resp) => {
            let _ = ratelimit::note(&route, resp.headers());
            let status = resp.status();
            if status.is_success() {
                let _ = event_tx.send(ToApp::Debug("Message deleted".into()));
            } else {
                let body = resp.text().await.unwrap_or_default();
                let _ = event_tx.send(ToApp::Debug(format!("Delete failed {}: {}", status, body)));
                let _ = event_tx.send(ToApp::DeleteFailed {
                    channel_id,
                    reason: message_action_reason(status.as_u16()).to_string(),
                });
            }
        }
        Err(e) => {
            let _ = event_tx.send(ToApp::Debug(format!("Delete error: {}", e)));
            let _ = event_tx.send(ToApp::DeleteFailed {
                channel_id,
                reason: "couldn't delete: no connection to Discord".to_string(),
            });
        }
    }
}

/// Mark a channel read up to a message. Best-effort: the format of user-token
/// read state has changed before, so a rejection is only logged, never shown.
pub(super) async fn ack_message(
    httpc: reqwest::Client,
    tkn: String,
    event_tx: EventTx,
    channel_id: String,
    message_id: String,
) {
    let url = format!(
        "{}/channels/{}/messages/{}/ack",
        API_BASE, channel_id, message_id
    );
    let route = format!("ack:{}", channel_id);
    ratelimit::wait(&route).await;
    let req = browser_headers(httpc.post(&url), &tkn).json(&json!({ "token": null }));
    match req.send().await {
        Ok(resp) => {
            let _ = ratelimit::note(&route, resp.headers());
            if resp.status().is_success() {
                let _ = event_tx.send(ToApp::Debug("Channel acked".into()));
            } else {
                let status = resp.status();
                let _ = event_tx.send(ToApp::Debug(format!("Ack failed {}", status)));
            }
        }
        Err(e) => {
            let _ = event_tx.send(ToApp::Debug(format!("Ack error: {}", e)));
        }
    }
}

/// Open a DM with a user. Separate task, like `send_message`, so the network
/// request doesn't stall the gateway loop.
pub(super) async fn open_dm(
    httpc: reqwest::Client,
    tkn: String,
    event_tx: EventTx,
    user_id: String,
) {
    let url = format!("{}/users/@me/channels", API_BASE);
    let route = "dm";
    ratelimit::wait(route).await;
    let req = browser_headers(httpc.post(&url), &tkn).json(&json!({ "recipient_id": user_id }));
    match req.send().await {
        Ok(resp) => {
            let _ = ratelimit::note(route, resp.headers());
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
                parent_id: None,
                last_message_id: None,
            }));
        }
        Err(e) => {
            let _ = event_tx.send(ToApp::Debug(format!("Open DM error: {}", e)));
        }
    }
}

/// Fetch the friends list, retrying on 429 via `retry-after` because it races
/// the guild-channel requests. URL is a parameter for tests.
pub(super) async fn fetch_relationships(
    httpc: reqwest::Client,
    tkn: String,
    url: String,
) -> Result<Vec<UserProfile>, String> {
    let route = "relationships";
    for _attempt in 0..3 {
        ratelimit::wait(route).await;
        let req = browser_headers(httpc.get(&url), &tkn);
        match req.send().await {
            Ok(resp) => {
                let limit = ratelimit::note(route, resp.headers());
                let status = resp.status();
                if status == 429 {
                    // `retry-after` is what Discord asks for; the fixed two
                    // seconds only covers a response that names no delay.
                    let wait = limit
                        .map(|l| l.wait)
                        .unwrap_or(Duration::from_secs(2));
                    tokio::time::sleep(wait).await;
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
/// GET one page with the retry policy every REST page shares: three attempts,
/// honoring 429's `retry-after` and giving up on any other refusal. `label`
/// shows up only in the debug log and in the final reason.
async fn get_page(
    httpc: &reqwest::Client,
    tkn: &str,
    event_tx: &EventTx,
    url: &str,
    route: &str,
    label: &str,
) -> Result<String, String> {
    let mut reason = String::new();
    let mut attempt = 0u32;
    while attempt < 3 {
        attempt += 1;
        ratelimit::wait(route).await;
        let req = browser_headers(httpc.get(url), tkn);
        match req.send().await {
            Ok(resp) => {
                let limit = ratelimit::note(route, resp.headers());
                let status = resp.status();
                let _ = event_tx.send(ToApp::Debug(format!("{label} response: {status}")));
                if status == 429 {
                    let wait = limit.map(|l| l.wait).unwrap_or(Duration::from_secs(2));
                    let _ = event_tx.send(ToApp::Debug(format!(
                        "{label} 429, retrying in {wait:?}"
                    )));
                    reason = "Discord asks you to wait (rate limit)".to_string();
                    time::sleep(wait).await;
                    continue;
                }
                if !status.is_success() {
                    let _ = event_tx.send(ToApp::Debug(format!("{label} error {status}")));
                    return Err(if status == 403 {
                        "no permission (403)".to_string()
                    } else {
                        format!("server responded {status}")
                    });
                }
                match resp.text().await {
                    Ok(body) => return Ok(body),
                    Err(e) => {
                        let _ = event_tx.send(ToApp::Debug(format!("{label} body error: {e}")));
                        reason = "couldn't read the response".to_string();
                    }
                }
            }
            Err(e) => {
                let _ = event_tx.send(ToApp::Debug(format!("{label} request error: {e}")));
                reason = "no connection to Discord".to_string();
                time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    if reason.is_empty() {
        reason = format!("{label} didn't arrive");
    }
    Err(reason)
}

pub(super) async fn fetch_history_page(
    httpc: reqwest::Client,
    tkn: String,
    event_tx: EventTx,
    channel_id: String,
    before: Option<String>,
) {
    let url = history_url(&channel_id, before.as_deref());
    let route = format!("history:{}", channel_id);
    let body = match get_page(&httpc, &tkn, &event_tx, &url, &route, "History").await {
        Ok(body) => body,
        Err(reason) => {
            // Nothing arrived: report failure so the loading spinner clears
            // instead of waiting forever.
            let _ = event_tx.send(ToApp::HistoryFailed { channel_id, before, reason });
            return;
        }
    };
    let mut warn = |m: String| {
        let _ = event_tx.send(ToApp::Debug(m));
    };
    let page = parse_history_page_lenient(&body, &channel_id, &mut warn);

    let got = page.len();
    let more = more_history_available(got) && next_before_id(&page, before.as_deref()).is_some();
    let _ = event_tx.send(ToApp::Debug(format!(
        "History page: {} messages, more={} (before={})",
        got,
        more,
        before.as_deref().map(|b| b.chars().take(14).collect::<String>()).unwrap_or_else(|| "-".to_string())
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

/// Guild search URL. Built through `reqwest::Url` so `content` is percent-
/// encoded: it is user text and may hold spaces, `#` or Cyrillic.
pub(crate) fn search_url(guild_id: &str, query: &str) -> String {
    let base = format!("{}/guilds/{}/messages/search", API_BASE, guild_id);
    match reqwest::Url::parse(&base) {
        Ok(mut url) => {
            url.query_pairs_mut()
                .append_pair("content", query)
                .append_pair("limit", &SEARCH_PAGE.to_string());
            url.into()
        }
        // The base is a constant, so this is unreachable in practice; a plain
        // URL is still better than a panic.
        Err(_) => base,
    }
}

/// Page around one message, used to land on a search hit that is far from the
/// newest page.
pub(crate) fn around_url(channel_id: &str, message_id: &str) -> String {
    format!(
        "{}/channels/{}/messages?limit={}&around={}",
        API_BASE, channel_id, AROUND_PAGE, message_id
    )
}

/// Guild-wide search. The query is echoed back with the answer so the app can
/// drop results for text the user has already typed over.
pub(super) async fn fetch_guild_search(
    httpc: reqwest::Client,
    tkn: String,
    event_tx: EventTx,
    guild_id: String,
    query: String,
) {
    let url = search_url(&guild_id, &query);
    let route = format!("search:{}", guild_id);
    let body = match get_page(&httpc, &tkn, &event_tx, &url, &route, "Search").await {
        Ok(body) => body,
        Err(reason) => {
            let _ = event_tx.send(ToApp::SearchFailed { query, reason });
            return;
        }
    };
    let mut warn = |m: String| {
        let _ = event_tx.send(ToApp::Debug(m));
    };
    let messages = parse_search_results(&body, &mut warn);
    let _ = event_tx.send(ToApp::Debug(format!(
        "Search: {} results for {:?}",
        messages.len(),
        query
    )));
    let _ = event_tx.send(ToApp::SearchResults { query, messages });
}

/// Messages around one id, so a hit can be shown with its context.
pub(super) async fn fetch_around(
    httpc: reqwest::Client,
    tkn: String,
    event_tx: EventTx,
    channel_id: String,
    message_id: String,
) {
    let url = around_url(&channel_id, &message_id);
    let route = format!("history:{}", channel_id);
    let body = match get_page(&httpc, &tkn, &event_tx, &url, &route, "Around").await {
        Ok(body) => body,
        Err(reason) => {
            // Reuse the history failure: the spinner clears and the reason
            // shows up in the chat either way.
            let _ = event_tx.send(ToApp::HistoryFailed { channel_id, before: None, reason });
            return;
        }
    };
    let mut warn = |m: String| {
        let _ = event_tx.send(ToApp::Debug(m));
    };
    let page = parse_history_page_lenient(&body, &channel_id, &mut warn);
    let got = page.len();
    let _ = event_tx.send(ToApp::Debug(format!(
        "Around page: {} messages (target {})",
        got, message_id
    )));
    let _ = event_tx.send(ToApp::HistoryAround {
        channel_id,
        messages: page,
        more: got >= AROUND_PAGE,
        target_id: message_id,
    });
}

pub(super) async fn send_files(
    httpc: reqwest::Client,
    tkn: String,
    event_tx: EventTx,
    channel_id: String,
    content: String,
    paths: Vec<String>,
    reply_to: Option<String>,
) {
    let url = format!("{}/channels/{}/messages", API_BASE, channel_id);
    let mut payload = json!({ "content": content });
    if let Some(reference) = reply_to.as_deref() {
        payload["message_reference"] = json!({
            "message_id": reference,
            "channel_id": channel_id.as_str(),
        });
    }

    // Read once: a retry must not re-read a file that may have changed (or
    // vanished) between attempts.
    let mut files: Vec<(String, Vec<u8>)> = Vec::with_capacity(paths.len());
    for path in &paths {
        match tokio::fs::read(path).await {
            Ok(bytes) => {
                let name = std::path::Path::new(path)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "file".to_string());
                files.push((name, bytes));
            }
            Err(e) => {
                let _ = event_tx.send(ToApp::Debug(format!("Read file error: {}", e)));
                return;
            }
        }
    }

    let route = format!("messages:{}", channel_id);
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let mut form = reqwest::multipart::Form::new()
            .text("payload_json", payload.to_string());
        for (i, (name, bytes)) in files.iter().enumerate() {
            let part = reqwest::multipart::Part::bytes(bytes.clone()).file_name(name.clone());
            form = form.part(format!("files[{}]", i), part);
        }

        ratelimit::wait(&route).await;
        let req = browser_headers(httpc.post(&url), &tkn).multipart(form);
        match req.send().await {
            Ok(resp) => {
                let limit = ratelimit::note(&route, resp.headers());
                let status = resp.status();
                if status.is_success() {
                    let _ = event_tx.send(ToApp::Debug("Files sent".into()));
                    return;
                }
                let body = resp.text().await.unwrap_or_default();
                let _ = event_tx.send(ToApp::Debug(format!("Send files failed {}: {}", status, body)));
                if send_retryable(status.as_u16()) && attempt < SEND_ATTEMPTS {
                    if limit.is_none() {
                        tokio::time::sleep(send_retry_delay(attempt)).await;
                    }
                    continue;
                }
                return;
            }
            Err(e) => {
                let _ = event_tx.send(ToApp::Debug(format!("Send files error: {}", e)));
                if attempt < SEND_ATTEMPTS {
                    tokio::time::sleep(send_retry_delay(attempt)).await;
                    continue;
                }
                return;
            }
        }
    }
}
