//! Cross-platform locations for persistent state and diagnostics.
//!
//! Before 0.9.2 everything lived in hidden files directly under `$HOME`
//! (`~/.wyvern_*.json`). That has no meaning on Windows and clutters the home
//! directory, so state now goes under the OS data directory and diagnostics
//! under the cache directory, both resolved through the `dirs` crate. Legacy
//! files are moved once on startup by [`migrate_from_home`].

use std::path::{Path, PathBuf};

/// App folder name under the platform data/cache directory.
const APP_DIR: &str = "wyvern";

/// State file names inside [`data_dir`], paired with their pre-0.9.2 names in
/// `$HOME` so a single table drives the one-time migration.
pub(crate) const LEGACY_FILES: [(&str, &str); 4] = [
    (".wyvern_settings.json", "settings.json"),
    (".wyvern_deleted.json", "deleted.json"),
    (".wyvern_accounts.json", "accounts.json"),
    (".wyvern_accounts.lock", "accounts.lock"),
];

/// Directory holding persistent state (settings, deleted history, vault).
pub(crate) fn data_dir() -> PathBuf {
    dirs::data_dir()
        .map(|d| d.join(APP_DIR))
        .unwrap_or_else(|| PathBuf::from(APP_DIR))
}

/// Directory for disposable diagnostics (debug log, panic logs).
/// `WYVERN_LOG_DIR` overrides it for tests and sandboxes.
pub(crate) fn log_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("WYVERN_LOG_DIR") {
        if !custom.is_empty() {
            return PathBuf::from(custom);
        }
    }
    dirs::cache_dir()
        .map(|d| d.join(APP_DIR))
        .unwrap_or_else(std::env::temp_dir)
}

/// Path to one state file. `override_var` (e.g. `WYVERN_SETTINGS_PATH`) lets
/// tests and sandboxes redirect the file away from the real one.
pub(crate) fn state_file(override_var: &str, name: &str) -> PathBuf {
    if let Ok(custom) = std::env::var(override_var) {
        if !custom.is_empty() {
            return PathBuf::from(custom);
        }
    }
    data_dir().join(name)
}

/// Gateway debug log (only written with `WYVERN_DEBUG=1`).
pub(crate) fn layout_log_path() -> PathBuf {
    log_dir().join("layout.log")
}

/// Numbered panic log name (`panic_N.log`) so a series survives instead of only
/// the last one.
fn panic_file_name(n: usize) -> String {
    format!("panic_{}.log", n)
}

/// Numbered panic log in the log directory; only tests build the path directly
/// (the panic hook asks for the next free number via [`next_panic_log_path`]).
#[cfg(test)]
pub(crate) fn panic_log_path(n: usize) -> PathBuf {
    log_dir().join(panic_file_name(n))
}

/// First free panic log number (up to 99) in `dir`.
fn next_panic_log_in(dir: &Path) -> PathBuf {
    (1..=99)
        .map(|n| dir.join(panic_file_name(n)))
        .find(|p| !p.exists())
        .unwrap_or_else(|| dir.join(panic_file_name(99)))
}

/// First free panic log number (up to 99) in the log directory.
pub(crate) fn next_panic_log_path() -> PathBuf {
    next_panic_log_in(&log_dir())
}

/// Move pre-0.9.2 files from `home` into `data`, returning the names moved.
/// Never overwrites an existing target and never errors: a failed move just
/// leaves the legacy file for the next launch. `home`/`data` are parameters so
/// the logic can be tested without touching the real `$HOME`.
pub(crate) fn migrate(home: &Path, data: &Path) -> Vec<&'static str> {
    let mut moved = Vec::new();
    for (old_name, new_name) in LEGACY_FILES {
        let old = home.join(old_name);
        let new = data.join(new_name);
        if old.exists() && !new.exists() {
            if std::fs::create_dir_all(data).is_ok() && std::fs::rename(&old, &new).is_ok() {
                moved.push(new_name);
            }
        }
    }
    moved
}

/// One-time migration using the real home and data directories. Also creates
/// the data directory so a fresh install can open its state files.
pub(crate) fn migrate_from_home() {
    let data = data_dir();
    let _ = std::fs::create_dir_all(&data);
    if let Some(home) = dirs::home_dir() {
        migrate(&home, &data);
    }
}

#[cfg(test)]
#[path = "paths_tests.rs"]
mod paths_tests;
