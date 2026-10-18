use super::DeletedStore;
use crate::app::MAX_MESSAGES_PER_CHANNEL;
use crate::models::ChatMessage;

/// A minimal message with the given id and content; timestamps aren't needed.
fn msg(id: &str) -> ChatMessage {
    ChatMessage {
        id: id.to_string(),
        channel_id: "c1".to_string(),
        author_id: "u1".to_string(),
        author_name: "Alice".to_string(),
        content: format!("text {id}"),
        ..Default::default()
    }
}

/// A unique temp path; the test owns it and removes it afterwards.
fn temp_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("wyvern-deleted-{}-{}", tag, std::process::id()))
}

/// What was recorded must come back from disk with its content intact.
#[test]
fn store_roundtrips_through_disk() {
    let path = temp_path("round");
    let mut store = DeletedStore::default();
    store.record("c1", [msg("100"), msg("200")]);
    store.save_to(&path).expect("store must save");

    let loaded = DeletedStore::load_from(&path);
    let kept = loaded.channel("c1");
    assert_eq!(kept.len(), 2, "both messages must survive the roundtrip");
    assert_eq!(kept[0].id, "100");
    assert_eq!(kept[0].content, "text 100", "content must be preserved");
    let _ = std::fs::remove_file(&path);
}

/// Recording the same id twice must not duplicate it, and order is restored.
#[test]
fn record_dedupes_and_sorts_by_id() {
    let mut store = DeletedStore::default();
    store.record("c1", [msg("300"), msg("100")]);
    store.record("c1", [msg("200"), msg("100")]);
    let kept = store.channel("c1");
    assert_eq!(kept.len(), 3, "duplicate id must be dropped");
    assert_eq!(
        kept.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["100", "200", "300"],
        "messages must be oldest first"
    );
}

/// A missing or corrupt file must degrade to an empty store, never an error.
#[test]
fn missing_or_corrupt_file_is_empty() {
    let missing = temp_path("missing");
    let _ = std::fs::remove_file(&missing);
    assert!(DeletedStore::load_from(&missing).channel("c1").is_empty());

    let corrupt = temp_path("corrupt");
    std::fs::write(&corrupt, b"{ not json").unwrap();
    assert!(
        DeletedStore::load_from(&corrupt).channel("c1").is_empty(),
        "a corrupt file must not block launch"
    );
    let _ = std::fs::remove_file(&corrupt);
}

/// The per-channel archive must not grow past the message cap.
#[test]
fn record_caps_per_channel() {
    let mut store = DeletedStore::default();
    let many = (0..MAX_MESSAGES_PER_CHANNEL + 20).map(|i| msg(&format!("{}", 1000 + i)));
    store.record("c1", many);
    let kept = store.channel("c1");
    assert_eq!(kept.len(), MAX_MESSAGES_PER_CHANNEL, "cap must hold");
    let newest = format!("{}", 1000 + MAX_MESSAGES_PER_CHANNEL + 19);
    assert_eq!(
        kept.last().map(|m| m.id.as_str()),
        Some(newest.as_str()),
        "newest message must be kept"
    );
}

/// `clear` empties the archive (the `keep_deleted` view was turned off).
#[test]
fn clear_drops_everything() {
    let mut store = DeletedStore::default();
    store.record("c1", [msg("100")]);
    store.clear();
    assert!(store.channel("c1").is_empty());
}
