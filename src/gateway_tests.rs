#[cfg(test)]
mod http_tests {
    use super::{
        api_client, client_with_timeout, fetch_relationships, send_failure_reason, send_message_to,
        EventTx, Generation, API_TIMEOUT,
    };
    use crate::messages::ToApp;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// The gateway client must have a sane timeout; a stalled request would
    /// otherwise block reconnection.
    #[test]
    fn api_client_has_a_sane_timeout() {
        assert!(API_TIMEOUT > Duration::from_secs(1), "слишком часто обрывать");
        assert!(
            API_TIMEOUT < Duration::from_secs(60),
            "настоящий ответ Discord столько не ждёт, а гейтвей столько молчит"
        );
        assert!(api_client().is_ok(), "клиент должен собираться");
    }

    /// Friends are fetched alongside guild channels and hit 429; verify on a
    /// real socket that the 429 is retried and only friends are kept.
    #[test]
    fn relationships_fetch_retries_after_429() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for attempt in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                if attempt == 0 {
                    let _ = stream.write_all(
                        b"HTTP/1.1 429 Too Many Requests\r\nretry-after: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                } else {
                    let body = r#"[{"id":"1","type":1,"user":{"id":"42","username":"friend"}},{"id":"2","type":2,"user":{"id":"43","username":"blocked"}}]"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes());
                }
                let _ = stream.flush();
            }
        });

        let client = api_client().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let friends = rt
            .block_on(fetch_relationships(
                client,
                "токен".into(),
                format!("http://{}/users/@me/relationships", addr),
            ))
            .expect("после 429 друзья должны догрузиться");
        assert_eq!(friends.len(), 1, "на экран идут только друзья: {friends:?}");
        assert_eq!(friends[0].username, "friend");
    }

    /// A send must not hang on a server that accepts the connection then goes
    /// silent; verified against a real unanswered socket.
    #[test]
    fn stalled_post_gives_up_instead_of_hanging() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Server accepts and never responds; keep the socket open, or the
        // client would see a disconnect and the test would pass on broken code.
        std::thread::spawn(move || {
            if let Ok((_stream, _)) = listener.accept() {
                std::thread::sleep(Duration::from_secs(10));
            }
        });

        let (tx, mut rx) = mpsc::unbounded_channel();
        // Wrap the receiver in an EventTx with a single generation so events
        // go straight through.
        let gen = Arc::new(Generation::default());
        let event_tx = EventTx::new(tx, gen.next(), gen.clone());
        // Shortened timeout so the test doesn't wait twenty seconds.
        let client = client_with_timeout(Duration::from_millis(300)).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let started = std::time::Instant::now();
        rt.block_on(send_message_to(
            client,
            "токен".into(),
            event_tx,
            format!("http://{}/channels/1/messages", addr),
            "привет".into(),
            "local:7".into(),
            "c1".into(),
        ));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "зависший POST должен обрываться, а не ждать: {:?}",
            started.elapsed()
        );
        // Connection loss must surface to the user, not just the debug log.
        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            seen.push(ev);
        }
        assert!(
            seen.iter().any(|e| matches!(e, ToApp::SendFailed { reason, .. } if reason.contains("нет связи"))),
            "пользователь должен узнать об отказе, получено: {seen:?}"
        );
    }

    /// Send failures are explained in words, not status codes.
    #[test]
    fn send_failure_reasons_are_readable() {
        for (status, must_contain) in [
            (403u16, "писать нельзя"),
            (404, "прав"),
            (429, "подождать"),
            (400, "2000"),
            (413, "2000"),
            (500, "отклонил"),
        ] {
            let reason = send_failure_reason(status);
            assert!(
                reason.contains(must_contain),
                "код {status}: должно быть про {must_contain:?}, а написано {reason:?}"
            );
            assert!(!reason.contains(&status.to_string()), "код не должен попадать в текст: {reason:?}");
        }
    }
}

#[cfg(test)]
mod generation_tests {
    use super::{EventTx, Generation};
    use crate::messages::ToApp;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    /// Switching accounts must silence the old gateway: it can otherwise
    /// reconnect with the old token and create two sessions.
    #[test]
    fn superseded_gateway_goes_silent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let gen = Arc::new(Generation::default());
        let old = EventTx::new(tx.clone(), gen.next(), gen.clone());
        // Account switched.
        let new = EventTx::new(tx, gen.next(), gen.clone());

        assert!(!old.alive(), "прежний гейтвей должен понять, что он больше не нужен");
        assert!(new.alive());
        old.send(ToApp::Debug("старое имя пользователя".into()));
        assert!(
            rx.try_recv().is_err(),
            "событие устаревшего гейтвея перетирало бы состояние нового"
        );

        new.send(ToApp::Debug("новое".into()));
        assert!(matches!(rx.try_recv(), Ok(ToApp::Debug(_))), "новый гейтвей должен говорить");
    }

    /// Generations must increase monotonically, or a new gateway would think
    /// it is stale and go silent.
    #[test]
    fn generations_increase_monotonically() {
        let gen = Generation::default();
        let first = gen.next();
        let second = gen.next();
        assert!(second > first, "поколения должны расти: {first} → {second}");
        assert!(gen.is_current(second));
        assert!(!gen.is_current(first), "прежнее поколение больше не актуально");
    }
}

#[cfg(test)]
mod session_tests {
    use super::{claim_history, release_history, SessionState};

    /// Duplicate-history protection must survive a reconnect: `run_gateway`
    /// passes one `SessionState` (and its `Arc`) into every `gw_inner`.
    #[tokio::test]
    async fn history_inflight_survives_a_reconnect() {
        let session = SessionState::default();
        let key = ("c1".to_string(), None);

        assert!(
            claim_history(&session.history_inflight, key.clone()).await,
            "первый запрос истории должен пройти"
        );
        // "Reconnect" is the same SessionState in the next gw_inner.
        assert!(
            !claim_history(&session.history_inflight, key.clone()).await,
            "после реконнекта защита от дублей пропала: запрос уйдёт второй раз"
        );

        // Request finished: release the pair so the next click passes.
        release_history(&session.history_inflight, &key).await;
        assert!(
            claim_history(&session.history_inflight, key).await,
            "после завершения запроса канал снова должен открываться"
        );
    }
}

#[cfg(test)]
mod identify_tests {
    use super::identify_payload;

    /// IDENTIFY must carry the real client fingerprint, the same object as
    /// `X-Super-Properties`.
    #[test]
    fn identify_carries_the_real_client_fingerprint() {
        let p = identify_payload("tok");
        assert_eq!(p["op"].as_i64(), Some(2), "это должен быть IDENTIFY");
        assert_eq!(
            p["d"]["properties"]["os"],
            std::env::consts::OS,
            "в IDENTIFY ушла чужая система"
        );
        assert_eq!(
            p["d"]["properties"],
            crate::util::client_properties(),
            "IDENTIFY и X-Super-Properties должны нести один отпечаток"
        );
    }
}

#[cfg(test)]
mod close_tests {
    use super::{close_code_of, close_fatal_reason, classify_raw, RawFrame, EOF_MARK, WS_ERROR_MARK};

    /// Token/intent refusal codes mean reconnecting changes nothing; they must
    /// be distinguished from ordinary drops.
    #[test]
    fn token_refusal_codes_are_fatal() {
        for code in [4004u16, 4007, 4013, 4014] {
            let reason = close_fatal_reason(code)
                .unwrap_or_else(|| panic!("код {code} должен быть фатальным"));
            assert!(!reason.is_empty(), "причина должна показываться пользователю");
        }
        // 4004 is the most common: invalid token.
        assert!(close_fatal_reason(4004).unwrap().contains("токен"));
    }

    /// Ordinary drops (network, sleep, Discord-side reconnect) must still
    /// reconnect.
    #[test]
    fn ordinary_close_codes_are_not_fatal() {
        for code in [0u16, 1000, 1001, 1006, 1011, 1012, 1013, 4000, 4008, 4011] {
            assert_eq!(close_fatal_reason(code), None, "код {code} — обычный обрыв");
        }
    }

    /// The read task reports the close code as a marker; an unparseable one is
    /// treated as an ordinary drop.
    #[test]
    fn close_code_is_taken_from_the_read_task() {
        assert_eq!(close_code_of("__CLOSE__4004"), Some(4004));
        assert_eq!(close_code_of("__CLOSE__1000"), Some(1000));
        assert_eq!(close_code_of("__WS_ERROR__broken pipe"), None);
        assert_eq!(close_code_of("{\"op\":0}"), None);
        // Garbage instead of a code must not become "fatal".
        assert_eq!(close_code_of("__CLOSE__мусор"), None);
    }

    /// A silent end of stream (no close frame) must be distinguished from an
    /// event so the gateway reconnects immediately.
    #[test]
    fn silent_end_of_stream_is_recognised() {
        assert!(matches!(classify_raw(EOF_MARK), RawFrame::Eof));
        // The marker must match what the read task sends; a typo would silently
        // disable reconnection.
        assert!(matches!(classify_raw(super::EOF_MARK), RawFrame::Eof));
    }

    /// The other markers must not be confused with Discord events.
    #[test]
    fn other_frames_stay_distinct() {
        assert!(matches!(classify_raw("__CLOSE__4004"), RawFrame::Closed(4004)));
        assert!(matches!(classify_raw("__CLOSE__1000"), RawFrame::Closed(1000)));
        assert!(matches!(classify_raw(&format!("{}broken pipe", WS_ERROR_MARK)), RawFrame::WsError(_)));
        // A real event stays an event even if its data contains "__CLOSE__".
        assert!(matches!(classify_raw("{\"op\":0,\"t\":\"__CLOSE__4004\"}"), RawFrame::Event));
        assert!(matches!(classify_raw("{\"op\":11,\"d\":null}"), RawFrame::Event));
    }
}

#[cfg(test)]
mod parse_tests {
    use super::{parse_history_page, parse_history_page_lenient, parse_message_value};

    /// A message as Discord sends it, with many fields the client ignores.
    const REAL: &str = r#"[{
        "id": "1200000000000000001",
        "channel_id": "900000000000000000",
        "content": "привет",
        "timestamp": "2026-09-26T12:00:00.000000+00:00",
        "type": 0,
        "pinned": false,
        "mention_everyone": false,
        "edited_timestamp": null,
        "flags": 0,
        "author": {
            "id": "800000000000000000",
            "username": "vasya",
            "discriminator": "0",
            "avatar": "abc123",
            "global_name": "Вася",
            "bot": false
        },
        "attachments": [{
            "id": "1100000000000000000",
            "filename": "photo.png",
            "size": 123456,
            "width": 1600,
            "height": 1200,
            "content_type": "image/png",
            "description": "схема из чата",
            "url": "https://cdn.discordapp.com/attachments/1/photo.png",
            "proxy_url": "https://media.discordapp.net/attachments/1/photo.png"
        }],
        "embeds": [{
            "type": "rich",
            "title": "заголовок",
            "author": {"name": "Кто-то", "url": "https://example.com"},
            "footer": {"text": "подпись"},
            "provider": {"name": " twitch"},
            "image": {"url": "https://cdn.discordapp.com/embeds/1/picture.png", "width": 800, "height": 600},
            "fields": [{"name": "a", "value": "b", "inline": true}]
        }],
        "reaction_counts": [{"count": 1, "me": false}],
        "mentions": []
    }]"#;

    fn parse_one_in(body: &str, channel_id: &str) -> crate::models::ChatMessage {
        let mut msgs = parse_history_page(body, channel_id).expect("разбор не должен падать");
        assert_eq!(msgs.len(), 1, "ожидалось одно сообщение");
        msgs.pop().unwrap()
    }

    fn parse_one(body: &str) -> crate::models::ChatMessage {
        parse_one_in(body, "fallback")
    }

    #[test]
    fn history_page_maps_all_used_fields() {
        let m = parse_one(REAL);
        assert_eq!(m.id, "1200000000000000001");
        assert_eq!(m.channel_id, "fallback", "канал страницы важнее поля в сообщении");
        assert_eq!(m.author_id, "800000000000000000");
        assert_eq!(m.author_name, "vasya");
        assert_eq!(m.author_avatar.as_deref(), Some("abc123"));
        assert_eq!(m.content, "привет");
        assert_eq!(m.timestamp, "2026-09-26T12:00:00.000000+00:00");
        assert!(!m.is_own);
        assert!(m.nickname.is_none());

        assert_eq!(m.attachments.len(), 1);
        let a = &m.attachments[0];
        assert_eq!(a.url, "https://cdn.discordapp.com/attachments/1/photo.png");
        assert_eq!(a.content_type.as_deref(), Some("image/png"));
        assert_eq!(a.description.as_deref(), Some("схема из чата"));
        // Pixels come from `width`/`height`; `size` is file size in bytes.
        assert_eq!(a.size, Some([1600, 1200]), "размер картинки должен доходить из истории");

        assert_eq!(m.embeds.len(), 1);
        assert_eq!(
            m.embeds[0].image_url.as_deref(),
            Some("https://cdn.discordapp.com/embeds/1/picture.png")
        );
        assert_eq!(m.embeds[0].image_size, Some([800, 600]), "размер картинки эмбеда — тоже");
        assert_eq!(m.embeds[0].description, None, "у эмбеда нет description — выкидываем пустое");
    }

    /// History and live messages must parse identically, except the channel:
    /// a page takes the requested channel, a live message its own `channel_id`.
    #[test]
    fn history_and_live_parse_agree() {
        let from_history = parse_one_in(REAL, "900000000000000000");
        let from_live = parse_message_value(
            &serde_json::from_str::<serde_json::Value>(REAL).unwrap()[0],
            "900000000000000000",
        )
        .expect("живое сообщение должно разобраться");
        assert_eq!(from_history.id, from_live.id);
        assert_eq!(from_history.channel_id, from_live.channel_id);
        assert_eq!(from_history.author_id, from_live.author_id);
        assert_eq!(from_history.author_name, from_live.author_name);
        assert_eq!(from_history.author_avatar, from_live.author_avatar);
        assert_eq!(from_history.content, from_live.content);
        assert_eq!(from_history.timestamp, from_live.timestamp);
        assert_eq!(from_history.attachments.len(), from_live.attachments.len());
        assert_eq!(from_history.attachments[0].url, from_live.attachments[0].url);
        assert_eq!(
            from_history.attachments[0].description, from_live.attachments[0].description
        );
        // Image size must match too; the two paths used to diverge here.
        assert_eq!(from_history.attachments[0].size, from_live.attachments[0].size);
        assert_eq!(from_history.attachments[0].size, Some([1600, 1200]));
        assert_eq!(from_history.embeds.len(), from_live.embeds.len());
        assert_eq!(from_history.embeds[0].image_url, from_live.embeds[0].image_url);
        assert_eq!(from_history.embeds[0].image_size, from_live.embeds[0].image_size);
    }

    /// A live message uses its own channel; fall back if the field is absent.
    #[test]
    fn live_message_uses_own_channel_then_fallback() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"id":"1","channel_id":"chan-42","content":"x","author":{"id":"u","username":"n"}}"#).unwrap();
        let m = parse_message_value(&v, "fallback").unwrap();
        assert_eq!(m.channel_id, "chan-42");
        let v2: serde_json::Value =
            serde_json::from_str(r#"{"id":"1","content":"x","author":{"id":"u","username":"n"}}"#).unwrap();
        assert_eq!(parse_message_value(&v2, "fallback").unwrap().channel_id, "fallback");
    }

    /// A message without an author is not shown.
    #[test]
    fn message_without_author_is_skipped() {
        let body = r#"[{"id":"1","content":"системное","author":{"id":"u","username":"n"}},{"id":"2","content":"без автора"}]"#;
        let msgs = parse_history_page(body, "c").unwrap();
        assert_eq!(msgs.len(), 1, "сообщение без author пропускается");
        assert_eq!(msgs[0].id, "1");
    }

    /// A bad value in one field must not drop the whole page.
    #[test]
    fn odd_field_types_do_not_break_the_page() {
        let body = r#"[{"id":7,"content":12345,"timestamp":null,
                        "author":{"id":"u","username":null,"avatar":null}},
                       {"id":"8","content":"ок","author":{"id":"u2","username":"n"}}]"#;
        let msgs = parse_history_page(body, "c").unwrap();
        assert_eq!(msgs.len(), 2, "оба сообщения должны остаться");
        assert_eq!(msgs[0].id, "7");
        assert_eq!(msgs[0].content, "12345");
        assert_eq!(msgs[0].timestamp, "");
        assert_eq!(msgs[0].author_name, "?", "нет username — как раньше показываем «?»");
        assert!(msgs[0].author_avatar.is_none());
    }

    /// A completely empty object must not break the page.
    #[test]
    fn empty_and_missing_fields_survive() {
        let msgs = parse_history_page(r#"[{}]"#, "c").unwrap();
        assert!(msgs.is_empty(), "без автора сообщение не показываем");
        let msgs = parse_history_page(r#"[]"#, "c").unwrap();
        assert!(msgs.is_empty());
        assert_eq!(parse_history_page("не json", "c").is_err(), true, "битый JSON — ошибка разбора");
    }

    /// Embed descriptions are trimmed; untrimmed ones are kept as-is.
    #[test]
    fn embed_description_is_trimmed() {
        let body = r#"[{"id":"1","content":"","author":{"id":"u","username":"n"},
                        "embeds":[{"description":"  текст  "},{"description":"   "}]}]"#;
        let msgs = parse_history_page(body, "c").unwrap();
        assert_eq!(msgs[0].embeds.len(), 1, "эмбед из одних пробелов выкидываем");
        assert_eq!(msgs[0].embeds[0].description.as_deref(), Some("текст"));
    }

    /// An attachment without a url is dropped, not a page failure.
    #[test]
    fn attachment_without_url_is_dropped() {
        let body = r#"[{"id":"1","content":"","author":{"id":"u","username":"n"},
                        "attachments":[{"filename":"x.png"},{"url":"https://cdn.discordapp.com/a/1.png"}]}]"#;
        let msgs = parse_history_page(body, "c").unwrap();
        assert_eq!(msgs[0].attachments.len(), 1);
        assert_eq!(msgs[0].attachments[0].url, "https://cdn.discordapp.com/a/1.png");
    }

    /// Fallback parse: an unexpected shape (e.g. non-object `author`) still
    /// shows the page; the strict pass fails but the page must not vanish.
    #[test]
    fn lenient_parse_survives_unexpected_shape() {
        let body = r#"[{"id":"1","content":"строка вместо объекта","author":"bob"},
                       {"id":"2","content":"нормальное","author":{"id":"u","username":"n"}}]"#;
        assert!(
            parse_history_page(body, "c").is_err(),
            "строгий разбор на этом должен ругаться — иначе тест бессмыслен"
        );
        let mut warns = Vec::new();
        let msgs = parse_history_page_lenient(body, "c", &mut |m| warns.push(m));
        assert_eq!(msgs.len(), 2, "оба сообщения должны показаться");
        assert_eq!(msgs[0].id, "1");
        assert_eq!(msgs[0].content, "строка вместо объекта");
        assert_eq!(msgs[1].author_name, "n");
        assert_eq!(warns.len(), 1, "о разборе запасным путём пишем в лог");
        assert!(warns[0].contains("fallback"), "лог должен говорить, что это запасной путь: {}", warns[0]);
    }

    /// Unreadable input must not panic: an empty list and a clear log line.
    #[test]
    fn lenient_parse_reports_garbage() {
        let mut warns = Vec::new();
        let msgs = parse_history_page_lenient("не json", "c", &mut |m| warns.push(m));
        assert!(msgs.is_empty());
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("History parse error"), "{}", warns[0]);
    }
}

#[cfg(test)]
mod heartbeat_tests {
    use super::{reconnect_delay, reconnect_delay_with_jitter, HeartbeatBook};
    use std::time::Duration;

    /// One unanswered heartbeat is enough to drop the connection.
    #[test]
    fn one_missed_heartbeat_is_enough_to_reconnect() {
        let mut hb = HeartbeatBook::default();
        assert!(hb.tick().is_ok(), "первый heartbeat уходит");
        assert!(hb.tick().is_err(), "без ACK второй уже рвёт соединение");
        // ACK clears the wait.
        hb.ack();
        assert!(hb.tick().is_ok());
        assert!(hb.tick().is_err());
    }

    /// Backoff grows exponentially and is capped.
    #[test]
    fn reconnect_backoff_grows_and_is_capped() {
        assert_eq!(reconnect_delay(0), Duration::from_secs(1));
        assert_eq!(reconnect_delay(1), Duration::from_secs(2));
        assert_eq!(reconnect_delay(2), Duration::from_secs(4));
        assert_eq!(reconnect_delay(5), Duration::from_secs(32));
        assert_eq!(reconnect_delay(6), Duration::from_secs(60));
        // No growth or overflow beyond this.
        assert_eq!(reconnect_delay(50), Duration::from_secs(60));
    }

    /// Jitter is at most a quarter of the base and clamps at the edges.
    #[test]
    fn reconnect_jitter_stays_within_a_quarter() {
        let base = reconnect_delay(3); // 8 s
        assert_eq!(reconnect_delay_with_jitter(3, 0.0), base);
        assert_eq!(reconnect_delay_with_jitter(3, 1.0), base + Duration::from_secs(2));
        assert_eq!(reconnect_delay_with_jitter(3, -5.0), base);
        assert_eq!(reconnect_delay_with_jitter(3, 5.0), base + Duration::from_secs(2));
    }
}
