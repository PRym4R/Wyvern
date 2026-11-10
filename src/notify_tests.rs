#[cfg(test)]
mod tests {
    use super::{applescript_escape, command_for, powershell_quote, sound_command_for};

    /// The sound uses a player each desktop actually has, and the arguments
    /// name a real sound rather than starting an empty player.
    #[test]
    fn each_platform_has_a_notification_sound() {
        let (program, args) = sound_command_for("linux");
        assert_eq!(program, "canberra-gtk-play", "на Linux нужен canberra");
        assert_eq!(args, vec!["-i", "message-new-instant"]);

        let (program, args) = sound_command_for("windows");
        assert_eq!(program, "powershell", "на Windows нужен PowerShell");
        let script = args.last().unwrap();
        assert!(
            script.contains("SystemSounds") && script.contains("Play()"),
            "скрипт должен проигрывать системный звук: {script}"
        );

        let (program, args) = sound_command_for("macos");
        assert_eq!(program, "afplay", "на macOS нужен afplay");
        assert!(
            args[0].ends_with(".aiff"),
            "нужен путь к системному звуку, а не пустой аргумент"
        );

        // An unknown OS falls back to the freedesktop tool.
        assert_eq!(sound_command_for("freebsd").0, "canberra-gtk-play");
    }

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
