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

/// Client fingerprint sent both in WebSocket IDENTIFY and (base64) in the
/// REST `X-Super-Properties` header. One source so the two cannot diverge.
pub(crate) fn client_properties() -> serde_json::Value {
    json!({
        // Real OS, not a hardcoded "Linux": Discord checks the fingerprint.
        "os": std::env::consts::OS,
        "browser": "Discord Client",
        "device": "",
        "release_channel": "stable",
        "client_build_number": 361909,
        "client_event_source": null
    })
}

pub(crate) fn super_props() -> String {
    base64_encode(&serde_json::to_string(&client_properties()).unwrap())
}

#[cfg(test)]
mod tests {
    use super::{base64_decode, client_properties, super_props};

    /// `super_props` must report the real OS (`std::env::consts::OS`).
    #[test]
    fn super_props_reports_the_real_os() {
        let decoded = base64_decode(&super_props()).expect("super_props — валидный base64");
        let v: serde_json::Value =
            serde_json::from_slice(&decoded).expect("super_props — валидный JSON");
        assert_eq!(
            v["os"].as_str(),
            Some(std::env::consts::OS),
            "система в отпечатке клиента не совпадает с настоящей"
        );
    }

    /// REST header and WebSocket IDENTIFY must carry one fingerprint.
    #[test]
    fn super_props_and_client_properties_are_the_same_fingerprint() {
        let decoded = base64_decode(&super_props()).expect("super_props — валидный base64");
        let header: serde_json::Value =
            serde_json::from_slice(&decoded).expect("super_props — валидный JSON");
        assert_eq!(
            header,
            client_properties(),
            "X-Super-Properties и IDENTIFY разошлись"
        );
    }
}
