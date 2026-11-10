use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Persisted UI state, kept in `settings.json` next to the accounts vault.
///
/// Deliberately separate from the encrypted vault: none of it is secret, and a
/// corrupt file must never cost the user an account. Every field is optional so
/// a partial, older, or hand-edited file still loads.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    /// Index into the theme list (0 dark, 1 cyberpunk, 2 light).
    pub(crate) theme_index: Option<u8>,
    /// Last window inner size in logical points, restored on the next launch.
    pub(crate) window: Option<[f32; 2]>,
    /// Channel id opened last, restored if it still exists.
    pub(crate) last_channel: Option<String>,
    /// Unsent composer text per channel id.
    pub(crate) drafts: HashMap<String, String>,
    /// UI scale (1.0 = default); `None` means 1.0.
    pub(crate) ui_scale: Option<f32>,
    /// Hide images/GIFs behind a click-to-reveal placeholder.
    pub(crate) blur_media: bool,
    /// Show a desktop notification for incoming messages.
    pub(crate) notifications: bool,
    /// Play the system notification sound together with the notification.
    pub(crate) notify_sound: bool,
    /// Do not disturb: no notifications and no sound, not even mention-free ones.
    pub(crate) dnd: bool,
    /// Keep deleted messages in the list, drawn in red, instead of removing them.
    pub(crate) keep_deleted: bool,
    /// Outbound proxy URL (http/https/socks5); empty or absent means direct.
    pub(crate) proxy: Option<String>,
    /// Animate selections, toasts, settings and image fade-ins; `None` means on.
    pub(crate) animations: Option<bool>,
}

impl Settings {
    /// Path to `settings.json`; `WYVERN_SETTINGS_PATH` overrides it (tests).
    pub(crate) fn path() -> PathBuf {
        crate::paths::state_file("WYVERN_SETTINGS_PATH", "settings.json")
    }

    /// Read settings from `path`. A missing, unreadable, or malformed file is an
    /// empty `Settings`, never an error: bad settings must not block launch.
    pub(crate) fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Write settings through a temp file + rename, so a crash mid-write can't
    /// leave a truncated file. Drafts and the proxy password live here, so the
    /// file is written `0600` (see [`crate::fsutil`]).
    pub(crate) fn save_to(&self, path: &Path) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        crate::fsutil::write_atomic(path, text.as_bytes())
    }
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod settings_tests;
