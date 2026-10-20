//! Native "choose files" dialog, used by the paperclip in the composer.

/// Opens the platform's file chooser and returns the selected paths.
///
/// The dialog blocks until the user closes it, so callers run this on a worker
/// thread (see `App::pick_attachments`).
pub(crate) fn pick_files() -> Vec<String> {
    pick_files_impl()
}

/// Splits a dialog's stdout into paths.
///
/// Zenity and yad use a newline separator, kdialog uses `--separate-output`;
/// accepting `|` too keeps the parser tolerant of older zenity defaults.
fn split_paths(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .split(['\n', '|'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(target_os = "linux")]
fn pick_files_impl() -> Vec<String> {
    // Prefer zenity (GTK, installed with most desktops), then kdialog/yad.
    const ATTEMPTS: [(&str, &[&str]); 3] = [
        (
            "zenity",
            &[
                "--file-selection",
                "--multiple",
                "--separator=\n",
                "--title=Attach files",
            ],
        ),
        (
            "kdialog",
            &["--getopenfilename", ".", "--multiple", "--separate-output"],
        ),
        (
            "yad",
            &[
                "--file",
                "--multiple",
                "--separator=\n",
                "--title=Attach files",
            ],
        ),
    ];
    for (cmd, args) in ATTEMPTS {
        match std::process::Command::new(cmd).args(args).output() {
            Ok(out) if out.status.success() => return split_paths(&out.stdout),
            // The dialog ran and the user cancelled; don't fall through.
            Ok(_) => return Vec::new(),
            // Not installed: try the next one.
            Err(_) => continue,
        }
    }
    Vec::new()
}

#[cfg(windows)]
fn pick_files_impl() -> Vec<String> {
    use std::os::windows::process::CommandExt;
    // A GUI app must not flash a console window for the helper.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let script = r#"
Add-Type -AssemblyName System.Windows.Forms | Out-Null
$dlg = New-Object System.Windows.Forms.OpenFileDialog
$dlg.Multiselect = $true
$dlg.Title = 'Attach files'
if ($dlg.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
    $dlg.FileNames | ForEach-Object { Write-Output $_ }
}
"#;
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-STA", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match out {
        Ok(o) if o.status.success() => split_paths(&o.stdout),
        _ => Vec::new(),
    }
}

#[cfg(target_os = "macos")]
fn pick_files_impl() -> Vec<String> {
    let script = r#"set theFiles to choose file with prompt "Attach files" with multiple selections allowed
set out to ""
repeat with f in theFiles
    set out to out & (POSIX path of f) & linefeed
end repeat
return out"#;
    match std::process::Command::new("osascript")
        .args(["-e", script])
        .output()
    {
        Ok(o) if o.status.success() => split_paths(&o.stdout),
        _ => Vec::new(),
    }
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
fn pick_files_impl() -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::split_paths;

    #[test]
    fn splits_newline_and_pipe_output() {
        assert_eq!(
            split_paths(b"/tmp/a.png\n/tmp/b with space.jpg\n"),
            vec!["/tmp/a.png", "/tmp/b with space.jpg"]
        );
        assert_eq!(split_paths(b"/tmp/a.png|/tmp/b.png"), vec!["/tmp/a.png", "/tmp/b.png"]);
        assert!(split_paths(b"\n  \n").is_empty(), "пустые строки не должны становиться путями");
    }
}
