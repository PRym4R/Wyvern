#[cfg(test)]
mod tests {
    use super::write_atomic;
    use std::path::PathBuf;

    /// Each test writes under its own name: the suite runs threads in parallel.
    fn temp_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("wyvern-fsutil-{}-{}", std::process::id(), name));
        path
    }

    /// The write replaces the target atomically and leaves no `.tmp` behind.
    #[test]
    fn write_atomic_replaces_the_file_and_cleans_up() {
        let path = temp_path("replace.json");
        write_atomic(&path, b"first").expect("первая запись");
        write_atomic(&path, b"second").expect("вторая запись");
        assert_eq!(
            std::fs::read_to_string(&path).expect("прочитать файл"),
            "second",
            "файл должен содержать последнюю запись"
        );
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        assert!(!PathBuf::from(tmp).exists(), "временный файл остался");
        let _ = std::fs::remove_file(&path);
    }

    /// A created parent directory is private, not world-traversable (Unix).
    #[test]
    fn created_directory_is_private_on_unix() {
        let dir = temp_path("dir").join("nested");
        let path = dir.join("state.json");
        write_atomic(&path, b"x").expect("запись во вложенный каталог");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).expect("метаданные каталога").permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "каталог состояния должен быть 0700");
            let mode = std::fs::metadata(&path).expect("метаданные файла").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "файл состояния должен быть 0600");
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(temp_path("dir"));
    }

    /// Writing into an existing shared directory must not chmod it.
    #[test]
    fn existing_directory_keeps_its_mode() {
        let dir = std::env::temp_dir();
        let before = std::fs::metadata(&dir).expect("метаданные /tmp").permissions();

        let path = temp_path("existing.json");
        write_atomic(&path, b"x").expect("запись в готовый каталог");
        let after = std::fs::metadata(&dir).expect("метаданные /tmp").permissions();
        assert_eq!(
            before, after,
            "режим существующего каталога менять нельзя"
        );
        let _ = std::fs::remove_file(&path);
    }
}
