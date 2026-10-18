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
    /// Keep deleted messages in the list, drawn in red, instead of removing them.
    pub(crate) keep_deleted: bool,
}

impl Settings {
    /// Path to `settings.json`; `WYVERN_SETTINGS_PATH` overrides it (tests).
    pub(crate) fn path() -> PathBuf {
        if let Ok(custom) = std::env::var("WYVERN_SETTINGS_PATH") {
            if !custom.is_empty() {
                return PathBuf::from(custom);
            }
        }
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            PathBuf::from(".wyvern_settings.json")
        } else {
            Path::new(&home).join(".wyvern_settings.json")
        }
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
    /// leave a truncated file.
    pub(crate) fn save_to(&self, path: &Path) -> std::io::Result<()> {
        use std::io::Write;
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let tmp = path.with_extension("json.tmp");
        let result = (|| {
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
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod settings_tests;
