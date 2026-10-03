//! Lenient deserialization of Discord JSON payloads into UI models.

use serde::Deserialize;
use serde_json::Value;

use crate::models::{image_size_of, size_from, Attachment, ChatMessage, Embed};

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
