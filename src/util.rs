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
        // Раньше здесь было зашито «Linux»; Discord сверяет отпечаток клиента,
        // и подмена системы на чужой машине — лишний повод не поверить.
        "os": std::env::consts::OS,
        "browser": "Discord Client",
        "device": "",
        "release_channel": "stable",
        "client_build_number": 361909,
        "client_event_source": null
    })).unwrap())
}

#[cfg(test)]
mod tests {
    use super::{base64_decode, super_props};

    /// super_props должен честно называть систему клиента: `std::env::consts::OS`
    /// даёт «linux»/«windows»/«macos», ровно те строки, что ждёт Discord.
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
}
