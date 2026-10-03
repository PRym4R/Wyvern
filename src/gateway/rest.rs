//! Discord REST/HTTP calls: sending messages, DMs, friends, and history pages.

use std::time::Duration;

use serde_json::{json, Value};
use tokio::time;

use crate::messages::ToApp;
use crate::models::{ChatChannel, ChatMessage, UserProfile};
use crate::util::super_props;

use super::parse::parse_history_page_lenient;
use super::{EventTx, API_BASE};

/// Messages per history page. Discord allows up to 100, but loading three
/// pages at once shows an empty channel; 50, then more on scroll.
pub(crate) const HISTORY_PAGE: usize = 50;

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
pub(crate) const API_TIMEOUT: Duration = Duration::from_secs(20);

pub(super) fn api_client() -> Result<reqwest::Client, String> {
    client_with_timeout(API_TIMEOUT)
}

pub(crate) fn client_with_timeout(timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("не собрать HTTP-клиент: {}", e))
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
    let req = httpc
        .post(&url)
        .header("Authorization", &*tkn)
        .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36")
        .header("X-Super-Properties", &super_props())
        .header("X-Discord-Locale", "en-US")
        .header("X-Discord-Timezone", "Europe/Moscow")
        .json(&body);
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
pub(super) async fn open_dm(
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
pub(super) async fn fetch_relationships(
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
pub(super) async fn fetch_history_page(
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
