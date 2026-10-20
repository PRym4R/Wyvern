use super::*;

fn temp_dir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("wyvern-paths-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn data_dir_lives_under_app_folder() {
    assert!(data_dir().ends_with(APP_DIR), "данные должны лежать в папке приложения");
}

#[test]
fn panic_logs_are_numbered() {
    let dir = temp_dir("panic");
    assert_eq!(panic_log_path(3).file_name().unwrap().to_str().unwrap(), "panic_3.log");
    assert_eq!(next_panic_log_in(&dir).file_name().unwrap().to_str().unwrap(), "panic_1.log");
    std::fs::write(dir.join("panic_1.log"), "x").unwrap();
    assert_eq!(next_panic_log_in(&dir).file_name().unwrap().to_str().unwrap(), "panic_2.log");
    std::fs::write(dir.join("panic_2.log"), "x").unwrap();
    assert_eq!(next_panic_log_in(&dir).file_name().unwrap().to_str().unwrap(), "panic_3.log");
}

#[test]
fn state_file_honours_override() {
    let var = "WYVERN_TEST_PATHS_PATH";
    let custom = std::env::temp_dir().join("wyvern-override.json");
    std::env::set_var(var, &custom);
    let got = state_file(var, "settings.json");
    std::env::remove_var(var);
    assert_eq!(got, custom, "переопределение пути должно победить data_dir");
}

#[test]
fn state_file_falls_back_to_data_dir() {
    let var = "WYVERN_TEST_PATHS_EMPTY";
    std::env::set_var(var, "");
    let got = state_file(var, "settings.json");
    std::env::remove_var(var);
    assert_eq!(got, data_dir().join("settings.json"), "пустая переменная — не переопределение");
}

#[test]
fn migrate_moves_legacy_files() {
    let home = temp_dir("home");
    let data = temp_dir("data");
    std::fs::write(home.join(".wyvern_settings.json"), "{\"a\":1}").unwrap();
    std::fs::write(home.join(".wyvern_deleted.json"), "[]").unwrap();
    let moved = migrate(&home, &data);
    assert!(moved.contains(&"settings.json"), "настройки должны переехать");
    assert!(moved.contains(&"deleted.json"), "история удалённых должна переехать");
    assert!(!home.join(".wyvern_settings.json").exists(), "старый файл не должен остаться");
    assert_eq!(std::fs::read_to_string(data.join("settings.json")).unwrap(), "{\"a\":1}");
}

#[test]
fn migrate_keeps_existing_target() {
    let home = temp_dir("home2");
    let data = temp_dir("data2");
    std::fs::write(home.join(".wyvern_settings.json"), "old").unwrap();
    std::fs::write(data.join("settings.json"), "new").unwrap();
    let moved = migrate(&home, &data);
    assert!(!moved.contains(&"settings.json"), "существующий файл перезаписывать нельзя");
    assert_eq!(std::fs::read_to_string(data.join("settings.json")).unwrap(), "new");
    assert!(home.join(".wyvern_settings.json").exists(), "старый файл остаётся, раз не переносился");
}
