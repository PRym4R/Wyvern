#[cfg(test)]
mod tests {
    use super::{applescript_escape, command_for, powershell_quote};

    /// Each platform gets its own tool; the OS comes from std, so every branch
    /// is reachable in the tests.
    #[test]
    fn each_platform_uses_its_own_tool() {
        assert_eq!(command_for("linux", "t", "b").0, "notify-send", "на Linux нужен notify-send");
        assert_eq!(command_for("windows", "t", "b").0, "powershell", "на Windows нужен PowerShell");
        assert_eq!(command_for("macos", "t", "b").0, "osascript", "на macOS нужен osascript");
        assert_eq!(
            command_for("freebsd", "t", "b").0,
            "notify-send",
            "неизвестная ОС должна падать на общий вариант"
        );
    }

    /// Title and body reach the platform tool as separate arguments on Linux.
    #[test]
    fn linux_keeps_title_and_body_separate() {
        let (_, args) = command_for("linux", "Alice", "привет");
        assert_eq!(args, vec!["Alice".to_string(), "привет".to_string()], "заголовок и текст перепутаны");
    }

    /// A quote in the text must not break out of the PowerShell string.
    #[test]
    fn windows_escapes_single_quotes() {
        assert_eq!(powershell_quote("it's"), "'it''s'", "одинарная кавычка не удвоена");
        let (_, args) = command_for("windows", "Bo'b", "don't panic");
        let script = args.last().unwrap();
        assert!(script.contains("'Bo''b'"), "заголовок не экранирован: {}", script);
        assert!(script.contains("'don''t panic'"), "текст не экранирован: {}", script);
        assert!(
            script.contains("ShowBalloonTip"),
            "скрипт должен показывать уведомление, а не что-то ещё"
        );
    }

    /// AppleScript needs backslashes and quotes escaped, in that order.
    #[test]
    fn macos_escapes_quotes_and_backslashes() {
        assert_eq!(applescript_escape("a\"b"), "a\\\"b", "кавычка не экранирована");
        assert_eq!(applescript_escape("a\\b"), "a\\\\b", "обратный слэш не экранирован");
        assert_eq!(applescript_escape("\\\""), "\\\\\\\"", "порядок экранирования неверен");
        let (_, args) = command_for("macos", "t", "say \"hi\"");
        assert!(
            args.last().unwrap().contains("say \\\"hi\\\""),
            "текст не попал в AppleScript экранированным"
        );
    }
}
