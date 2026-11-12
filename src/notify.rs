//! Desktop notifications through the platform's own tool: `notify-send` on
//! Linux, a PowerShell balloon on Windows, `osascript` on macOS.
//!
//! Spawned and forgotten: a missing tool must never block a frame or panic.
//! The command lines are built by a pure helper so the escaping is testable
//! (`notify_tests.rs`).

use std::process::{Command, Stdio};

/// How long the Windows helper stays alive: the balloon needs the process,
/// and Windows drops it once the process exits.
const WINDOWS_LIFETIME: u32 = 6;

/// Show a desktop notification. Failures are silent by design: not being able
/// to notify is never worth a toast of its own.
pub(crate) fn desktop(title: &str, body: &str) {
    if title.trim().is_empty() && body.trim().is_empty() {
        return;
    }
    spawn(&command_for(std::env::consts::OS, title, body));
}

/// Fire and forget: a missing tool must never block a frame or panic.
fn spawn((program, args): &(String, Vec<String>)) {
    let _ = Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// The program and arguments that notify on `os` (`std::env::consts::OS`).
///
/// The OS is a parameter, not a `cfg`, so every branch is compiled and tested
/// everywhere and none of them counts as dead code.
pub(crate) fn command_for(os: &str, title: &str, body: &str) -> (String, Vec<String>) {
    match os {
        "windows" => (
            "powershell".to_string(),
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-WindowStyle".to_string(),
                "Hidden".to_string(),
                "-Command".to_string(),
                windows_script(title, body),
            ],
        ),
        "macos" => (
            "osascript".to_string(),
            vec![
                "-e".to_string(),
                format!(
                    "display notification \"{}\" with title \"{}\"",
                    applescript_escape(body),
                    applescript_escape(title)
                ),
            ],
        ),
        // Everything else is X11/Wayland enough for libnotify's tool.
        _ => (
            "notify-send".to_string(),
            vec![title.to_string(), body.to_string()],
        ),
    }
}

/// PowerShell script for a Windows balloon tip: no AppID registration needed,
/// unlike the WinRT toast API, which silently does nothing for unpackaged apps.
fn windows_script(title: &str, body: &str) -> String {
    format!(
        "Add-Type -AssemblyName System.Windows.Forms; \
         $n = New-Object System.Windows.Forms.NotifyIcon; \
         $n.Icon = [System.Drawing.SystemIcons]::Information; \
         $n.Visible = $true; \
         $n.ShowBalloonTip(5000, {}, {}, [System.Windows.Forms.ToolTipIcon]::None); \
         Start-Sleep -Seconds {}; \
         $n.Dispose()",
        powershell_quote(title),
        powershell_quote(body),
        WINDOWS_LIFETIME
    )
}

/// PowerShell single-quoted string: the only escape is a doubled quote.
fn powershell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// AppleScript string body: backslash and double quote need escaping.
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
include!("notify_tests.rs");
