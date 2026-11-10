//! Atomic, private writes for the state stores.
//!
//! `settings.json` holds drafts and the proxy password, `deleted.json` holds
//! other people's messages, `accounts.json` holds the tokens. A second user on
//! the same machine must not read any of that, so every write creates the temp
//! file `0600` (and a directory we create ourselves `0700`) instead of relying
//! on the process umask.

use std::path::{Path, PathBuf};

/// Write `contents` to `path` through a temp file, fsync and rename.
///
/// Same directory and same name suffix as before, so a leftover `*.tmp` from an
/// older build is simply overwritten.
pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            // Only touch the mode of a directory this call just created: a
            // user-supplied parent (`WYVERN_SETTINGS_PATH`) may be anything.
            #[cfg(unix)]
            let existed = dir.exists();
            std::fs::create_dir_all(dir)?;
            #[cfg(unix)]
            if !existed {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
        }
    }

    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);

    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // Applies at creation, so the file is never readable by others,
            // not even for the instant before a chmod.
            options.mode(0o600);
        }
        let mut f = options.open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // A stale temp file keeps its old mode; `mode()` only applies when
            // the file is created.
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(contents)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();

    if result.is_err() {
        // Don't leave a stray temp file behind on failure.
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
include!("fsutil_tests.rs");
