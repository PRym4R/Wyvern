use super::*;
use crate::app::App;
use crate::models::{ChatChannel, Guild};
use tokio::sync::mpsc;

fn temp_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("wyvern-settings-{}-{}.json", tag, std::process::id()))
}

fn offline_app() -> App {
    let (_, rx) = mpsc::unbounded_channel();
    App::new(rx)
}

#[test]
fn settings_round_trip_through_a_file() {
    let path = temp_path("roundtrip");
    let _ = std::fs::remove_file(&path);

    let mut s = Settings::default();
    s.theme_index = Some(2);
    s.window = Some([1000.0, 700.0]);
    s.last_channel = Some("c42".into());
    s.drafts.insert("c1".into(), "hello".into());
    s.ui_scale = Some(1.25);
    s.blur_media = true;
    s.notifications = true;
    s.keep_deleted = true;

    s.save_to(&path).expect("настройки должны записаться");
    assert!(
        !path.with_extension("json.tmp").exists(),
        "после записи не должно остаться временного файла"
    );
    assert_eq!(Settings::load_from(&path), s, "настройки должны пережить запись и чтение");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn missing_or_broken_file_is_empty_settings() {
    let missing = temp_path("missing");
    let _ = std::fs::remove_file(&missing);
    assert_eq!(Settings::load_from(&missing), Settings::default(), "нет файла — пустые настройки");

    let broken = temp_path("broken");
    std::fs::write(&broken, b"{ not json at all").unwrap();
    assert_eq!(
        Settings::load_from(&broken),
        Settings::default(),
        "битый файл не должен ронять запуск"
    );
    let _ = std::fs::remove_file(&broken);
}

#[test]
fn theme_is_persisted_through_the_app() {
    let path = temp_path("theme");
    let _ = std::fs::remove_file(&path);

    let mut app = offline_app();
    app.settings_path = Some(path.clone());
    app.theme_index = 2;
    app.write_settings();

    assert_eq!(Settings::load_from(&path).theme_index, Some(2), "тема не сохранилась");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn without_a_path_nothing_is_written() {
    // Default `App::new` has no settings path, so tests never touch `$HOME`.
    let mut app = offline_app();
    app.theme_index = 2;
    app.write_settings();
    assert!(app.settings_path.is_none());
}

#[test]
fn drafts_travel_with_the_channel() {
    let path = temp_path("drafts");
    let _ = std::fs::remove_file(&path);

    let mut app = offline_app();
    app.settings_path = Some(path.clone());

    app.open_channel("a");
    app.input = "draft a".into();
    app.open_channel("b");
    assert_eq!(app.input, "", "черновик не должен утечь в другой канал");
    app.input = "draft b".into();
    app.open_channel("a");
    assert_eq!(app.input, "draft a", "черновик канала не вернулся");

    let saved = Settings::load_from(&path);
    assert_eq!(saved.last_channel.as_deref(), Some("a"), "последний канал не записан");
    assert_eq!(
        saved.drafts.get("b").map(String::as_str),
        Some("draft b"),
        "черновик другого канала не записан"
    );
    assert_eq!(saved.drafts.get("a").map(String::as_str), Some("draft a"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn reopening_the_same_channel_keeps_the_text() {
    let mut app = offline_app();
    app.open_channel("a");
    app.input = "unsent".into();
    app.open_channel("a");
    assert_eq!(app.input, "unsent", "повторное открытие того же канала не должно терять текст");
}

#[test]
fn last_channel_is_restored_when_known() {
    let mut app = offline_app();
    app.guilds.push(Guild { id: "g1".into(), name: "G".into(), icon: None });
    app.channels.push(ChatChannel {
        id: "c7".into(),
        name: "general".into(),
        guild_id: Some("g1".into()),
        channel_type: 0,
        topic: None,
        position: 0,
    });
    app.settings.last_channel = Some("c7".into());

    assert!(app.restore_last_channel(), "канал из настроек должен восстанавливаться");
    assert_eq!(app.selected_channel, Some(0));
    assert_eq!(app.selected_guild, Some(0));
    assert_eq!(app.current_channel_id(), Some("c7"));
}

#[test]
fn keep_deleted_marks_instead_of_removing() {
    let mut app = offline_app();
    app.settings.keep_deleted = true;
    app.delete_message("c1", "m1");
    assert!(
        app.deleted_ids.contains("m1"),
        "с включённым хранением удалённое сообщение должно остаться (красным)"
    );

    let mut app = offline_app();
    app.delete_message("c1", "m1");
    assert!(
        !app.deleted_ids.contains("m1"),
        "без хранения удалённое сообщение просто исчезает, метки нет"
    );
}

#[test]
fn unknown_last_channel_falls_back_to_default() {
    let mut app = offline_app();
    app.channels.push(ChatChannel {
        id: "c1".into(),
        name: "x".into(),
        guild_id: None,
        channel_type: 0,
        topic: None,
        position: 0,
    });
    app.settings.last_channel = Some("gone".into());

    assert!(!app.restore_last_channel(), "исчезнувший канал не восстанавливаем");
    assert_eq!(app.selected_channel, None);
}
