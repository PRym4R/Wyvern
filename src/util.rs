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

/// Отпечаток клиента для Discord.
///
/// Один и тот же объект уходит и в IDENTIFY по WebSocket, и (в base64) в
/// заголовок `X-Super-Properties` у REST. Раньше оба места держали свою
/// копию зашитых значений, и правка одного не чинила другое: в IDENTIFY
/// так и осталось «Linux». Держим один источник, чтобы отпечаток не разъезжался.
pub(crate) fn client_properties() -> serde_json::Value {
    json!({
        // Раньше здесь было зашито «Linux»; Discord сверяет отпечаток клиента,
        // и подмена системы на чужой машине — лишний повод не поверить.
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

    /// Заголовок REST и IDENTIFY по WebSocket должны нести один отпечаток:
    /// раньше это были две копии, и в IDENTIFY так и осталось зашитое «Linux».
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
