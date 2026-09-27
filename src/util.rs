use serde_json::json;

pub(crate) fn base64_encode(s: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
}

pub(crate) fn base64_string(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub(crate) fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

pub(crate) fn super_props() -> String {
    base64_encode(&serde_json::to_string(&json!({
        "os": "Linux",
        "browser": "Discord Client",
        "device": "",
        "release_channel": "stable",
        "client_build_number": 361909,
        "client_event_source": null
    })).unwrap())
}
