use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::app::MAX_MESSAGES_PER_CHANNEL;
use crate::models::ChatMessage;

/// Deleted messages kept on disk so the `keep_deleted` view survives a restart.
///
/// The channel list lives only in memory and is refetched from Discord on
/// launch, where deleted messages are already gone; without this store the red
/// rows vanish. The file is plain JSON: none of it is secret, and a corrupt
/// file must never block launch, so every read falls back to an empty store.
#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct DeletedStore {
    /// Channel id -> that channel's deleted messages, oldest first.
    pub(crate) by_channel: HashMap<String, Vec<ChatMessage>>,
}

impl DeletedStore {
    /// Path to `deleted.json`; `WYVERN_DELETED_PATH` overrides it (tests).
    pub(crate) fn path() -> PathBuf {
        crate::paths::state_file("WYVERN_DELETED_PATH", "deleted.json")
    }

    /// Read the store. A missing, unreadable, or malformed file is empty, never
    /// an error: bad history must not block launch.
    pub(crate) fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Write through a temp file + rename, so a crash mid-write can't leave a
    /// truncated file.
    pub(crate) fn save_to(&self, path: &Path) -> std::io::Result<()> {
        use std::io::Write;
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let tmp = path.with_extension("json.tmp");
        let result = (|| {
            if let Some(dir) = path.parent() {
                if !dir.as_os_str().is_empty() {
                    std::fs::create_dir_all(dir)?;
                }
            }
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, path)
        })();
        if result.is_err() {
            // Don't leave a stray temp file behind on failure.
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// Remember a channel's deleted messages: deduped by id, oldest first,
    /// capped at the per-channel limit so the file can't grow without bound.
    pub(crate) fn record(&mut self, channel_id: &str, msgs: impl IntoIterator<Item = ChatMessage>) {
        let entry = self.by_channel.entry(channel_id.to_string()).or_default();
        for m in msgs {
            if !entry.iter().any(|e| e.id == m.id) {
                entry.push(m);
            }
        }
        entry.sort_by(|a, b| sort_key(&a.id).cmp(&sort_key(&b.id)));
        if entry.len() > MAX_MESSAGES_PER_CHANNEL {
            let extra = entry.len() - MAX_MESSAGES_PER_CHANNEL;
            entry.drain(..extra);
        }
    }

    /// Deleted messages of a channel, oldest first.
    pub(crate) fn channel(&self, channel_id: &str) -> &[ChatMessage] {
        self.by_channel.get(channel_id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Drop everything; the `keep_deleted` view was turned off.
    pub(crate) fn clear(&mut self) {
        self.by_channel.clear();
    }
}

/// Numeric snowflake ids sort chronologically; anything else goes last.
fn sort_key(id: &str) -> (u8, u128) {
    match id.parse::<u128>() {
        Ok(n) => (0, n),
        Err(_) => (1, 0),
    }
}

#[cfg(test)]
#[path = "deleted_tests.rs"]
mod deleted_tests;
