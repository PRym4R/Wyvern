use super::*;

fn make_app() -> App {
    let (_, rx) = mpsc::unbounded_channel();
    let mut a = App::new(rx);
    a.connected = true;
    a.gw_started = true;
    a.token_input = "x".into();
    a.guilds.push(Guild {
        id: "g1".into(),
        name: "Test Guild".into(),
        icon: None,
    });
    for i in 0..189 {
        a.channels.push(ChatChannel {
            id: format!("c{}", i),
            name: format!("channel-{}", i),
            guild_id: None,
            channel_type: 1,
            topic: None,
            position: i,
        });
    }
    a.selected_guild = None;
    a.selected_channel = Some(0);
    a.messages.insert(
        "c0".into(),
        vec![Arc::new(ChatMessage {
            id: "m1".into(),
            channel_id: "c0".into(),
            author_id: "u1".into(),
            author_name: "Alice".into(),
            author_avatar: None,
            nickname: None,
            content: "hello world".into(),
            timestamp: "2026-01-01T00:00:00.000Z".into(),
            attachments: vec![],
            embeds: vec![],
            is_own: false,
        })],
    );
    a
}

fn input_panel_height(ctx: &egui::Context) -> Option<f32> {
    use egui::containers::panel::PanelState;
    let id = egui::Id::new("input_panel");
    ctx.data_mut(|d| d.get_persisted::<PanelState>(id)).map(|s| s.rect.height())
}

fn chat_scroll_offset(app: &App) -> f32 {
    app.last_scroll_offset_y
}

#[test]
fn panel_layout_diagnostic() {
    std::env::set_var("NO_COLOR", "1");
    let mut app = make_app();
    let ctx = egui::Context::default();
    let size = egui::vec2(1052.0, 1054.0);
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
        ..Default::default()
    };

    let output = ctx.run(raw, |ctx| {
        app.draw_chat(ctx);
    });
    let _ = output;
    if let Some(h) = input_panel_height(&ctx) {
        eprintln!("[TEST] input panel height after frame 1: {:.1}", h);
    }
}

/// Idle must not request repaints; poll used to do so unconditionally 20x/s.
#[test]
fn idle_frame_does_not_ask_for_repaint() {
    std::env::set_var("NO_COLOR", "1");
    let mut app = make_app();
    let ctx = egui::Context::default();
    let size = egui::vec2(1052.0, 1054.0);
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
        ..Default::default()
    };

    // Run several frames so egui's scroll animation settles; the last one matters.
    let mut output = None;
    for _ in 0..10 {
        output = Some(ctx.run(raw.clone(), |ctx| app.run_frame(ctx)));
    }
    let delay = output.unwrap().viewport_output[&egui::ViewportId::ROOT].repaint_delay;
    assert_eq!(
        delay,
        Duration::MAX,
        "в покое клиент всё ещё просит кадр через {delay:?}"
    );
}

/// While a load is pending, keep frames coming; its result is reaped during draw.
#[test]
fn pending_work_still_asks_for_repaint() {
    std::env::set_var("NO_COLOR", "1");
    let mut app = make_app();
    let (_tx, rx) = std::sync::mpsc::channel();
    app.pending_avatars.insert("u1_deadbeef".into(), rx);
    let ctx = egui::Context::default();
    let size = egui::vec2(1052.0, 1054.0);
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
        ..Default::default()
    };
    let output = ctx.run(raw, |ctx| app.run_frame(ctx));
    let delay = output.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
    assert!(
        delay <= Duration::from_millis(50),
        "висящая загрузка должна держать кадры, а delay = {delay:?}"
    );
}

#[test]
fn input_panel_multiframe_growth() {
    std::env::set_var("NO_COLOR", "1");
    let mut app = make_app();
    let ctx = egui::Context::default();
    let size = egui::vec2(1052.0, 1054.0);
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
        ..Default::default()
    };
    let mut last = 0.0_f32;
    for frame in 1..=90 {
        let _ = ctx.run(raw.clone(), |ctx| {
            app.draw_chat(ctx);
        });
        if let Some(h) = input_panel_height(&ctx) {
            if frame == 1 || frame == 10 || frame == 30 || frame == 60 || frame == 90 {
                eprintln!("[TEST] frame {frame}: input panel height = {h:.1}");
            }
            last = h;
        }
    }
    assert!(last < 200.0, "input panel kept growing, final height {:.1}", last);
}

#[test]
fn chat_scroll_to_bottom_on_history() {
    std::env::set_var("NO_COLOR", "1");
    let mut app = make_app();
    let many = (0..300).map(|i| ChatMessage {
        id: format!("m{}", i),
        channel_id: "c0".into(),
        author_id: "u1".into(),
        author_name: "Alice".into(),
        author_avatar: None,
        nickname: None,
        content: format!("message number {}", i),
        timestamp: "2026-01-01T00:00:00.000Z".into(),
        attachments: vec![],
        embeds: vec![],
        is_own: false,
    }).map(Arc::new).collect::<Vec<_>>();

    let ctx = egui::Context::default();
    let size = egui::vec2(1052.0, 1054.0);
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
        ..Default::default()
    };

    // Several frames of empty/short history with stick=false (like while loading).
    let _ = ctx.run(raw.clone(), |ctx| { app.draw_chat(ctx); });

    // History arrives -> many messages, stick=true.
    app.messages.insert("c0".into(), many);
    app.scroll_to_bottom = true;

    let _ = ctx.run(raw.clone(), |ctx| { app.draw_chat(ctx); });

    let offset = chat_scroll_offset(&app);
    eprintln!("[TEST] scroll offset after history: {:?}", offset);
    assert!(offset > 15000.0, "did not scroll to bottom, offset={:?}", offset);
}

#[test]
fn account_encryption_roundtrip() {
    let accs = vec![
        StoredAccount { token: "tok123".into(), username: "alice".into() },
        StoredAccount { token: "tok456".into(), username: "bob".into() },
    ];

    let encrypted = App::encrypt_accounts(&accs, "hunter2").expect("encrypt failed");
    assert!(encrypted.contains("\"version\""));
    assert!(!encrypted.contains("tok123"), "token leaked in plaintext");

    let decrypted = App::decrypt_accounts(&encrypted, "hunter2").expect("decrypt failed");
    assert_eq!(decrypted.len(), 2);
    assert_eq!(decrypted[0].token, "tok123");
    assert_eq!(decrypted[1].username, "bob");

    assert!(App::decrypt_accounts(&encrypted, "wrongpass").is_none(), "wrong password must fail");
    assert!(App::decrypt_accounts(&encrypted, "").is_none());
    let empty = vec![];
    assert!(App::encrypt_accounts(&empty, "").is_none(), "empty password must refuse encryption");
    assert!(App::load_accounts_with(&encrypted, "").is_none(), "empty password opens nothing");
}

/// Isolated vault file per test, held on the App instance since tests run in parallel.
fn vaulted_app(tag: &str) -> (App, std::path::PathBuf) {
    let mut p = std::env::temp_dir();
    p.push(format!("wyvern-test-{}-{}.json", tag, std::process::id()));
    let _ = std::fs::remove_file(&p);
    let mut app = App::new(mpsc::unbounded_channel().1);
    app.vault_path_override = Some(p.clone());
    (app, p)
}

/// Vault tests must never touch the user's real file.
#[test]
fn vault_tests_never_touch_real_file() {
    let real = App::accounts_path();
    let before = std::fs::read(&real).ok();

    let (mut app, tmp) = vaulted_app("isolated");
    app.saved_accounts = vec![StoredAccount { token: "t-iso".into(), username: "u".into() }];
    app.save_accounts("pw");
    assert!(tmp.exists(), "тест должен писать во временный файл");

    if let Some(before) = before {
        let after = std::fs::read(&real).expect("настоящий файл пропал");
        assert_eq!(before, after, "тест изменил настоящий файл хранилища!");
    } else {
        assert!(!real.exists(), "тест создал настоящий файл хранилища: {}", real.display());
    }
    let _ = std::fs::remove_file(&tmp);
}

/// Password whitespace is the most common cause of "wrong password".
#[test]
fn vault_tolerates_password_spaces() {
    // Password brute-force costs many PBKDF2 iterations; the lock keeps timing tests from interfering.
    let _guard = crate::crypto::VAULT_COST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for saved in ["hunter2", "hunter2 ", " hunter2", "hunter2\n"] {
        let (mut app, tmp) = vaulted_app("space");
        app.saved_accounts =
            vec![StoredAccount { token: "tok-space".into(), username: "bob".into() }];
        app.save_accounts(saved);
        assert!(tmp.exists(), "файл не записался");

        // Enter without spaces: a second App on the same file, like a client restart.
        let mut app2 = App::new(mpsc::unbounded_channel().1);
        app2.vault_path_override = Some(tmp.clone());
        let hint = app2.unlock_vault("hunter2").expect("пароль без пробелов должен подойти");
        assert_eq!(app2.saved_accounts.len(), 1, "аккаунт не загрузился (saved={:?})", saved);
        assert_eq!(app2.saved_accounts[0].username, "bob");
        if saved == "hunter2" {
            assert!(hint.is_none(), "для точного пароля подсказки быть не должно");
        } else {
            assert!(hint.is_some(), "для пароля с пробелами ждём подсказку, saved={:?}", saved);
        }
        let _ = std::fs::remove_file(&tmp);
    }
}

/// An empty vault is not a wrong password.
#[test]
fn empty_vault_is_not_a_wrong_password() {
    let (mut app, tmp) = vaulted_app("empty");
    let encrypted = App::encrypt_accounts(&[], "pw123").expect("encrypt");
    std::fs::write(&tmp, encrypted).unwrap();

    let res = app.unlock_vault("pw123");
    assert!(res.is_ok(), "пустое хранилище с верным паролем должно открываться: {:?}", res);
    assert!(app.saved_accounts.is_empty());
    assert!(app.accounts_unlocked);
    let _ = std::fs::remove_file(&tmp);
}

/// Full user path: login with token+password, restart, unlock, pick account, login.
#[test]
fn full_cycle_save_restart_unlock_and_login() {
    let (mut app, tmp) = vaulted_app("cycle");
    let token = "MTIz.тест.токен".to_string();
    let pw = "мой-пароль";

    // 1. Unlock with password and save the account.
    app.unlock_vault(pw).expect("первый вход создаёт хранилище");
    app.add_saved_account(&token, "мой_юзер");
    assert!(tmp.exists(), "файл хранилища не создан");
    assert_eq!(app.saved_accounts.len(), 1);
    assert_eq!(app.account_label(&app.saved_accounts[0]), "мой_юзер");

    // 2. "Restart": new instance; drop the old one first to release the vault lock.
    drop(app);
    let mut app2 = App::new(mpsc::unbounded_channel().1);
    app2.vault_path_override = Some(tmp.clone());
    assert!(app2.saved_accounts.is_empty(), "после перезапуска список пуст");
    assert!(!app2.accounts_unlocked, "хранилище закрыто");
    assert!(app2.login_password.is_empty());

    // 3. Enter the vault password.
    let hint = app2.unlock_vault(pw).expect("пароль должен подойти после перезапуска");
    assert!(hint.is_none(), "точный пароль не должен давать подсказку");
    assert_eq!(app2.saved_accounts.len(), 1, "аккаунт не подгрузился");
    assert_eq!(app2.saved_accounts[0].token, token);
    assert!(app2.accounts_unlocked);

    // 4. Click an account in the bottom strip.
    app2.select_account(token.clone());
    assert_eq!(app2.login_selected.as_deref(), Some(token.as_str()));
    assert_eq!(app2.login_password, pw, "раз уже открыто — пароль подставляется");

    // 5. Press login: token goes to the gateway, selection clears.
    app2.login_with_password(&token);
    assert_eq!(app2.token_input, token, "вход должен выбрать аккаунт");
    assert!(app2.gw_started, "гейтвей должен стартовать");
    assert!(app2.login_selected.is_none(), "после входа выбор сброшен");
    assert!(app2.login_password.is_empty(), "пароль из поля должен очищаться");

    // 6. The vault on disk survived the login.
    drop(app2);
    let mut app3 = App::new(mpsc::unbounded_channel().1);
    app3.vault_path_override = Some(tmp.clone());
    assert!(app3.unlock_vault(pw).is_ok(), "файл должен остаться читаемым");
    assert_eq!(app3.saved_accounts.len(), 1);
    let _ = std::fs::remove_file(&tmp);
}

/// Wrong password is an honest error, not a silent empty list.
#[test]
fn wrong_password_still_errors() {
    let (mut app, tmp) = vaulted_app("wrong");
    let encrypted = App::encrypt_accounts(
        &[StoredAccount { token: "t".into(), username: "u".into() }],
        "right",
    )
    .expect("encrypt");
    std::fs::write(&tmp, encrypted).unwrap();

    let res = app.unlock_vault("wrong");
    let err = res.err().expect("неверный пароль должен давать ошибку");
    assert!(err.contains("Неверный пароль"), "непонятное сообщение: {}", err);
    assert!(!app.accounts_unlocked);
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn history_is_capped_and_old_channels_dropped() {
    let mut entry: Vec<Arc<ChatMessage>> = Vec::new();
    let mut msg = ChatMessage {
        id: "m0".into(),
        channel_id: "c0".into(),
        author_id: "u1".into(),
        author_name: "Alice".into(),
        author_avatar: None,
        nickname: None,
        content: "x".into(),
        timestamp: "2026-01-01T00:00:00.000Z".into(),
        attachments: vec![],
        embeds: vec![],
        is_own: false,
    };
    for i in 0..MAX_MESSAGES_PER_CHANNEL + 50 {
        msg.id = format!("m{}", i);
        entry.push(Arc::new(msg.clone()));
    }
    trim_messages(&mut entry, true);
    assert_eq!(entry.len(), MAX_MESSAGES_PER_CHANNEL);
    // Newest remain, order preserved.
    assert_eq!(entry[0].id, "m50");
    assert_eq!(entry[MAX_MESSAGES_PER_CHANNEL - 1].id, format!("m{}", MAX_MESSAGES_PER_CHANNEL + 49));

    // Opening a channel drops other channels' history.
    let mut app = make_app();
    app.messages.insert("c1".into(), vec![entry[0].clone()]);
    app.open_channel("c0");
    assert!(app.messages.contains_key("c0"), "активный канал должен остаться");
    assert!(!app.messages.contains_key("c1"), "история других каналов должна быть выброшена");
    assert_eq!(app.history_loading.as_deref(), Some("c0"), "пока грузим — должен быть спиннер");
}

/// Unclaimed downloads from the old channel must not be held all session; nobody will reap them.
#[test]
fn switching_channel_drops_pending_downloads() {
    use crate::models::ImagePayload;
    let mut app = make_app();
    let (img_tx, img_rx) = std::sync::mpsc::channel();
    img_tx
        .send(Some(ImagePayload::Static(egui::ColorImage::new([512, 512], egui::Color32::BLACK))))
        .unwrap();
    app.pending_images.insert("https://cdn.discordapp.com/attachments/1/old.png".into(), img_rx);
    let (_av_tx, av_rx) = std::sync::mpsc::channel::<AvatarFetch>();
    app.pending_avatars.insert("u1_deadbeef".into(), av_rx);
    assert_eq!(app.pending_images.len(), 1);
    assert_eq!(app.pending_avatars.len(), 1);

    app.open_channel("c0");

    assert!(app.pending_images.is_empty(), "пиксели прежнего канала не должны висеть в памяти");
    assert!(app.pending_avatars.is_empty(), "незабранные аватары тоже");
}

/// Message builder for history tests.
fn test_msg(id: &str, channel_id: &str, content: &str) -> ChatMessage {        ChatMessage {
        id: id.into(),
        channel_id: channel_id.into(),
        author_id: "u1".into(),
        author_name: "Alice".into(),
        author_avatar: None,
        nickname: None,
        content: content.into(),
        timestamp: "2026-01-01T00:00:00.000Z".into(),
        attachments: vec![],
        embeds: vec![],
        is_own: false,
    }
}

/// A late response for a closed channel must not clear the current channel's loading spinner.
#[test]
fn late_history_keeps_current_channel_loading() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.channels.push(ChatChannel {
        id: "c1".into(),
        name: "first".into(),
        guild_id: None,
        channel_type: 1,
        topic: None,
        position: 0,
    });
    app.channels.push(ChatChannel {
        id: "c2".into(),
        name: "second".into(),
        guild_id: None,
        channel_type: 1,
        topic: None,
        position: 1,
    });
    app.selected_channel = Some(1);
    app.open_channel("c2");
    assert_eq!(app.history_loading.as_deref(), Some("c2"));

    // A late response arrives for the first channel.
    tx.send(ToApp::History {
        channel_id: "c1".into(),
        messages: vec![test_msg("m1", "c1", "старое")],
        more: false,
    })
    .unwrap();
    app.poll(&ctx);
    assert_eq!(
        app.history_loading.as_deref(),
        Some("c2"),
        "спиннер текущего канала снимать нельзя"
    );
    assert_eq!(app.messages.get("c1").map(|v| v.len()), Some(1), "ответ должен сохраниться");

    // A response for the current channel is accepted and clears the spinner.
    tx.send(ToApp::History {
        channel_id: "c2".into(),
        messages: vec![test_msg("m2", "c2", "свежее")],
        more: false,
    })
    .unwrap();
    app.poll(&ctx);
    let msgs = app.messages.get("c2").expect("история текущего канала должна сохраниться");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].content, "свежее");
    assert!(app.history_loading.is_none(), "спиннер должен сняться");
}

/// History pagination: `before` comes from the oldest message, else Discord returns the same page.
#[test]
fn history_pagination_uses_oldest_id() {
    // Discord returns newest-first: [100, 99, ..., 1].
    let page: Vec<ChatMessage> = (1..=3)
        .rev()
        .map(|i| test_msg(&format!("{}", 100 + i), "c1", "x"))
        .collect();
    assert_eq!(page[0].id, "103", "первым идёт самое новое");
    assert_eq!(crate::gateway::next_before_id(&page, None).as_deref(), Some("101"));

    // A repeated id means pages loop; stop loading.
    assert_eq!(
        crate::gateway::next_before_id(&page, Some("101")),
        None,
        "одинаковый id должен останавливать пагинацию"
    );
    // Empty page: history ended.
    assert_eq!(crate::gateway::next_before_id(&[], None), None);
    // Short page: reached the channel start.
    assert!(!crate::gateway::more_history_available(3));
    assert!(!crate::gateway::more_history_available(49));
    assert!(crate::gateway::more_history_available(50));
    assert!(crate::gateway::more_history_available(100));

    // Page URL: first has no `before`, second uses the old id.
    let first = crate::gateway::history_url("42", None);
    assert!(!first.contains("before="), "первая страница без before: {}", first);
    assert!(
        first.ends_with(&format!("/channels/42/messages?limit={}", crate::gateway::HISTORY_PAGE)),
        "{}",
        first
    );
    let second = crate::gateway::history_url("42", Some("101"));
    assert!(second.ends_with(&format!("/channels/42/messages?limit={}&before=101", crate::gateway::HISTORY_PAGE)), "{}", second);
}

/// Scroll-up history: the page prepends instead of replacing, and the boundary message isn't duplicated.
#[test]
fn older_page_is_prepended_without_duplicates() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.channels.push(ChatChannel {
        id: "c1".into(),
        name: "chan".into(),
        guild_id: None,
        channel_type: 1,
        topic: None,
        position: 0,
    });
    app.selected_channel = Some(0);
    app.open_channel("c1");

    // First page as Discord sends it (newest-first, oldest m51); it should sit at the end.
    let first: Vec<ChatMessage> = (51..=100).rev().map(|i| test_msg(&format!("m{i}"), "c1", "новое")).collect();
    assert_eq!(first[0].id, "m100", "Discord отдаёт страницу от новых к старым");
    tx.send(ToApp::History { channel_id: "c1".into(), messages: first, more: true }).unwrap();
    app.poll(&ctx);
    let stored = app.messages.get("c1").unwrap().len();
    assert_eq!(stored, crate::gateway::HISTORY_PAGE);
    assert!(!app.history_exhausted, "Discord сказал, что история есть дальше");
    assert!(app.history_loading.is_none(), "спиннер первой страницы снялся");
    // The stored list is reversed: oldest to newest.
    let first_stored = app.messages.get("c1").unwrap();
    assert_eq!(first_stored[0].id, "m51", "в начале списка самое старое");
    assert_eq!(first_stored[stored - 1].id, "m100", "в конце самое новое");

    // Scrolled up: request and receive a strictly older page.
    app.request_older_history();
    assert!(app.history_loading_more, "должен гореть индикатор догрузки");
    let older: Vec<ChatMessage> = (1..=50).rev().map(|i| test_msg(&format!("m{i}"), "c1", "старое")).collect();
    tx.send(ToApp::HistoryMore { channel_id: "c1".into(), messages: older, more: false }).unwrap();
    app.poll(&ctx);

    let msgs = app.messages.get("c1").unwrap();
    assert_eq!(msgs.len(), crate::gateway::HISTORY_PAGE * 2, "страница должна добавиться, а не заменить");
    // Key here is order and no duplicates; violations looked like the chat copying itself.
    let ids: Vec<&str> = msgs.iter().map(|m| m.id.as_str()).collect();
    let want: Vec<String> = (1..=100).map(|i| format!("m{i}")).collect();
    assert_eq!(
        ids,
        want.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        "список должен идти строго от старых к новым, без повторов"
    );
    assert_eq!(msgs[0].content, "старое", "в начале самое старое");
    assert_eq!(
        msgs[msgs.len() - 1].content,
        "новое",
        "прежние сообщения не должны пропасть"
    );
    assert!(app.history_exhausted, "короткой страницей история признана конченной");
    assert!(!app.history_loading_more, "индикатор догрузки должен погаснуть");
}

/// A message can appear in both adjacent pages; drop the duplicate.
#[test]
fn boundary_message_is_not_duplicated() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.channels.push(ChatChannel {
        id: "c1".into(),
        name: "chan".into(),
        guild_id: None,
        channel_type: 1,
        topic: None,
        position: 0,
    });
    app.selected_channel = Some(0);
    app.open_channel("c1");
    let first: Vec<ChatMessage> = (51..=100).rev().map(|i| test_msg(&format!("m{i}"), "c1", "x")).collect();
    tx.send(ToApp::History { channel_id: "c1".into(), messages: first, more: true }).unwrap();
    app.poll(&ctx);

    // Older page, but Discord sent the oldest message (m51) again.
    let older: Vec<ChatMessage> = (1..=51).rev().map(|i| test_msg(&format!("m{i}"), "c1", "x")).collect();
    tx.send(ToApp::HistoryMore { channel_id: "c1".into(), messages: older, more: false }).unwrap();
    app.poll(&ctx);

    let msgs = app.messages.get("c1").unwrap();
    assert_eq!(
        msgs.iter().filter(|m| m.id == "m51").count(),
        1,
        "сообщение на стыке страниц должно быть одно"
    );
    let ids: Vec<&str> = msgs.iter().map(|m| m.id.as_str()).collect();
    let want: Vec<String> = (1..=100).map(|i| format!("m{i}")).collect();
    assert_eq!(
        ids,
        want.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        "список должен идти от старых к новым без повторов"
    );
}

/// Don't re-request while a page is in flight, or one scroll fires duplicate requests.
#[test]
fn older_history_requested_once_at_a_time() {
    let app_ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.channels.push(ChatChannel {
        id: "c1".into(),
        name: "chan".into(),
        guild_id: None,
        channel_type: 1,
        topic: None,
        position: 0,
    });
    app.selected_channel = Some(0);
    app.open_channel("c1");
    let page: Vec<ChatMessage> =
        (0..crate::gateway::HISTORY_PAGE).map(|i| test_msg(&format!("m{}", i + 1), "c1", "x")).collect();
    tx.send(ToApp::History { channel_id: "c1".into(), messages: page, more: true }).unwrap();
    app.poll(&app_ctx);

    app.request_older_history();
    assert!(app.history_loading_more);
    app.request_older_history();
    assert!(app.history_loading_more, "второй запрос не должен уйти");
    // Don't request upward while the first page is loading either.
    app.history_loading_more = false;
    app.history_loading = Some("c1".into());
    app.request_older_history();
    assert!(!app.history_loading_more, "во время первой загрузки вверх не лезем");
}

/// Discord pages newest-first but we store oldest-first; otherwise `before` comes
/// from the newest and the same page repeats, filling the chat with copies.
#[test]
fn discord_page_is_stored_oldest_first_and_paged_from_the_oldest() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.to_gw = Some(cmd_tx);
    app.channels.push(ChatChannel {
        id: "c1".into(),
        name: "chan".into(),
        guild_id: None,
        channel_type: 1,
        topic: None,
        position: 0,
    });
    app.selected_channel = Some(0);
    app.open_channel("c1");

    // Page exactly as Discord sends it: newest-first.
    let page: Vec<ChatMessage> = (1..=crate::gateway::HISTORY_PAGE)
        .rev()
        .map(|i| test_msg(&format!("m{i}"), "c1", "x"))
        .collect();
    assert_eq!(page[0].id, format!("m{}", crate::gateway::HISTORY_PAGE));
    tx.send(ToApp::History { channel_id: "c1".into(), messages: page, more: true }).unwrap();
    app.poll(&ctx);

    let msgs = app.messages.get("c1").expect("страница сохранена");
    assert_eq!(
        msgs[0].id, "m1",
        "список должен идти от старых к новым, иначе первым окажется самое новое"
    );
    assert_eq!(
        msgs[msgs.len() - 1].id,
        format!("m{}", crate::gateway::HISTORY_PAGE),
        "самое новое сообщение страницы должно быть в конце списка"
    );

    // Paging upward uses the oldest line, not the newest, or Discord returns the same page.
    app.request_older_history();
    let sent: Vec<ToGateway> = std::iter::from_fn(|| cmd_rx.try_recv().ok()).collect();
    let before = sent
        .iter()
        .find_map(|c| match c {
            ToGateway::FetchHistory { before, .. } => before.clone(),
            _ => None,
        })
        .expect("запрос истории вверх должен уйти");
    assert_eq!(
        before, "m1",
        "догружать надо от самой старой страницы, а не от самой новой"
    );
}

/// At the channel start, stop hitting the API; paging from a short page would loop.
#[test]
fn exhausted_channel_stops_asking() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.channels.push(ChatChannel {
        id: "c1".into(),
        name: "chan".into(),
        guild_id: None,
        channel_type: 1,
        topic: None,
        position: 0,
    });
    app.selected_channel = Some(0);
    app.open_channel("c1");
    // Short page: start of the channel.
    tx.send(ToApp::History {
        channel_id: "c1".into(),
        messages: vec![test_msg("m1", "c1", "единственное")],
        more: false,
    })
    .unwrap();
    app.poll(&ctx);
    assert!(app.history_exhausted);
    app.request_older_history();
    assert!(!app.history_loading_more, "после конца истории запрашивать нельзя");
}

/// New messages scroll to the bottom only if the user is already there.
#[test]
fn new_message_scrolls_only_when_at_bottom() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.channels.push(ChatChannel {
        id: "c1".into(),
        name: "chan".into(),
        guild_id: None,
        channel_type: 1,
        topic: None,
        position: 0,
    });
    app.selected_channel = Some(0);
    app.open_channel("c1");

    app.chat_at_bottom = false;
    app.scroll_to_bottom = false;
    tx.send(ToApp::Message(test_msg("m1", "c1", "пока нас смотрят историю"))).unwrap();
    app.poll(&ctx);
    assert!(!app.scroll_to_bottom, "читающего историю нельзя перематывать вниз");

    app.chat_at_bottom = true;
    tx.send(ToApp::Message(test_msg("m2", "c1", "новое"))).unwrap();
    app.poll(&ctx);
    assert!(app.scroll_to_bottom, "внизу новое сообщение должно тянуть вниз");
}

/// Our own message shows immediately; the confirmation must replace the echo, not duplicate it.
#[test]
fn own_message_is_replaced_not_duplicated() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.to_gw = Some(cmd_tx);
    app.user_id = "me".into();
    app.username = "Я".into();
    app.channels.push(ChatChannel {
        id: "c1".into(),
        name: "chan".into(),
        guild_id: None,
        channel_type: 1,
        topic: None,
        position: 0,
    });
    app.selected_channel = Some(0);
    app.open_channel("c1");
    // An empty Discord response would leave the loading screen; not needed here.
    tx.send(ToApp::History { channel_id: "c1".into(), messages: vec![], more: false }).unwrap();
    app.poll(&ctx);

    // Sent: one message appears immediately with a fake id.
    app.handle_input("привет");
    let msgs = app.messages.get("c1").unwrap();
    assert_eq!(msgs.len(), 1, "локальное эхо должно появиться сразу");
    assert!(msgs[0].is_local_echo(), "у неподтверждённого сообщения id служебный");

    // Discord confirms the send with the same text and a real id.
    let mut from_discord = test_msg("9001", "c1", "привет");
    from_discord.author_id = "me".into();
    from_discord.author_name = "Я".into();
    from_discord.timestamp = "2026-01-01T00:05:00.000Z".into();
    tx.send(ToApp::Message(from_discord)).unwrap();
    app.poll(&ctx);

    let msgs = app.messages.get("c1").unwrap();
    assert_eq!(msgs.len(), 1, "подтверждение должно занять место эха, а не добавиться рядом");
    assert_eq!(msgs[0].id, "9001", "в списке должен остаться настоящий id");
    assert_eq!(msgs[0].content, "привет");
    assert!(!msgs[0].is_local_echo());

    // A second message with the same text: with two echoes, each confirmation finds its own.
    app.handle_input("привет");
    assert_eq!(app.messages.get("c1").unwrap().len(), 2);
    let mut again = test_msg("9002", "c1", "привет");
    again.author_id = "me".into();
    tx.send(ToApp::Message(again)).unwrap();
    app.poll(&ctx);
    let msgs = app.messages.get("c1").unwrap();
    assert_eq!(msgs.len(), 2, "и здесь не должно быть дубля");
    assert_eq!(msgs[0].id, "9001", "порядок сообщений не должен меняться");
    assert_eq!(msgs[1].id, "9002");

    // A foreign message with the same text doesn't touch our echo.
    let mut foreign = test_msg("9003", "c1", "привет");
    foreign.author_id = "u-other".into();
    tx.send(ToApp::Message(foreign)).unwrap();
    app.poll(&ctx);
    assert_eq!(app.messages.get("c1").unwrap().len(), 3);
}

/// A local echo id must never be sent as `before`; the API would error.
#[test]
fn local_echo_id_never_goes_to_pagination() {
    let (tx, rx) = mpsc::unbounded_channel();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.to_gw = Some(cmd_tx);
    app.user_id = "me".into();
    app.channels.push(ChatChannel {
        id: "c1".into(),
        name: "chan".into(),
        guild_id: None,
        channel_type: 1,
        topic: None,
        position: 0,
    });
    app.selected_channel = Some(0);
    app.open_channel("c1");
    // The first page never arrives here; clear the wait manually to reach the check.
    app.history_loading = None;
    let _ = tx;
    // The first-page request from opening is irrelevant.
    let _ = std::iter::from_fn(|| cmd_rx.try_recv().ok()).count();

    // Only our unsent message is stored, so there's nothing to page from and no API call.
    app.handle_input("ещё не отправлено");
    app.request_older_history();
    let sent: Vec<ToGateway> = std::iter::from_fn(|| cmd_rx.try_recv().ok()).collect();
    let asks_history = sent.iter().any(|c| matches!(c, ToGateway::FetchHistory { .. }));
    assert!(!asks_history, "нельзя пагинировать по ненастоящему id: {:?}", sent);
    assert!(app.history_exhausted);
    // The older-history spinner must not be running.
    assert!(!app.history_loading_more);
}

/// One failed history load must not make the channel unreadable forever; clear the spinner.
#[test]
fn failed_history_clears_the_spinner_and_explains() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.channels.push(ChatChannel {
        id: "c1".into(),
        name: "chan".into(),
        guild_id: None,
        channel_type: 1,
        topic: None,
        position: 0,
    });
    app.selected_channel = Some(0);
    app.open_channel("c1");
    assert_eq!(app.history_loading.as_deref(), Some("c1"));

    // Network returned 403, retried three times, then gave up.
    tx.send(ToApp::HistoryFailed {
        channel_id: "c1".into(),
        before: None,
        reason: "нет прав на канал".into(),
    })
    .unwrap();
    app.poll(&ctx);

    assert_eq!(app.history_loading, None, "спиннер первой страницы должен погаснуть");
    assert_eq!(
        app.history_error.as_ref().map(|(c, r)| (c.as_str(), r.as_str())),
        Some(("c1", "нет прав на канал")),
        "пользователь должен видеть, что произошло"
    );
    assert!(app.history_exhausted, "после 403 больше не долбим в API");
    // The channel works again: upward paging isn't stuck.
    assert!(!app.history_loading_more);
}

/// Same for older history: one failure must not leave its spinner on forever.
#[test]
fn failed_older_page_clears_its_own_spinner() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.channels.push(ChatChannel {
        id: "c1".into(),
        name: "chan".into(),
        guild_id: None,
        channel_type: 1,
        topic: None,
        position: 0,
    });
    app.selected_channel = Some(0);
    app.open_channel("c1");
    let page: Vec<ChatMessage> = (1..=crate::gateway::HISTORY_PAGE)
        .rev()
        .map(|i| test_msg(&format!("m{i}"), "c1", "x"))
        .collect();
    tx.send(ToApp::History { channel_id: "c1".into(), messages: page, more: true }).unwrap();
    app.poll(&ctx);
    app.request_older_history();
    assert!(app.history_loading_more);

    tx.send(ToApp::HistoryFailed {
        channel_id: "c1".into(),
        before: Some("m1".into()),
        reason: "нет связи с Discord".into(),
    })
    .unwrap();
    app.poll(&ctx);

    assert!(!app.history_loading_more, "спиннер догрузки должен погаснуть");
    assert_eq!(app.history_loading, None, "нечего было и гасить — первая страница уже пришла");
    assert_eq!(app.history_error.as_ref().map(|(_, r)| r.as_str()), Some("нет связи с Discord"));
}

/// A failure for another channel must not touch the current one's load.
#[test]
fn failed_history_of_other_channel_leaves_current_alone() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    for id in ["c1", "c2"] {
        app.channels.push(ChatChannel {
            id: id.into(),
            name: "chan".into(),
            guild_id: None,
            channel_type: 1,
            topic: None,
            position: 0,
        });
    }
    app.selected_channel = Some(1);
    app.open_channel("c2");
    assert_eq!(app.history_loading.as_deref(), Some("c2"));

    tx.send(ToApp::HistoryFailed {
        channel_id: "c1".into(),
        before: None,
        reason: "нет прав на канал".into(),
    })
    .unwrap();
    app.poll(&ctx);

    assert_eq!(app.history_loading.as_deref(), Some("c2"), "чужой канал не должен снимать наш спиннер");
    assert!(app.history_error.is_none(), "и показывать чужую ошибку в нашем канале нельзя");
}

/// Rejected token must return to the login screen; otherwise there's no way out of chat.
#[test]
fn rejected_token_returns_to_login_screen() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.token_input = "неверныйтокен".into();
    // Mark the gateway as started; the real thread would replace the event channel.
    app.gw_started = true;
    app.connected = true;
    assert!(!app.shows_login(), "после входа должен быть чат");

    tx.send(ToApp::AuthFailed {
        reason: "токен отклонён Discord: он недействителен (4004)".into(),
    })
    .unwrap();
    app.poll(&ctx);

    assert!(!app.connected, "показ «я онлайн» должен погаснуть");
    assert!(!app.gw_started);
    assert!(app.shows_login(), "вернуться на экран входа обязательно, иначе выйти нечем");
    assert!(
        app.status.contains("4004"),
        "пользователь должен видеть, что именно отказало: {:?}",
        app.status
    );
}

/// A disconnect is not a login but also not online; clear `connected` so the button dims.
#[test]
fn connection_lost_clears_online_flag() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.token_input = "токен".into();
    app.connected = true;
    app.gw_started = true;

    tx.send(ToApp::Status("Reconnecting: websocket closed".into())).unwrap();
    app.poll(&ctx);
    assert!(!app.connected, "мы сейчас не онлайн, даже если экран чата открыт");

    // Conversely, an ordinary status must not kick the user back to login.
    app.connected = true;
    tx.send(ToApp::Status("Connecting...".into())).unwrap();
    app.poll(&ctx);
    assert!(app.connected);
    assert!(app.gw_started);
    assert!(!app.shows_login(), "переподключение не должно выкидывать на экран входа");
}

/// "Remember" without a vault password must stop the login, not silently continue.
#[test]
fn remember_without_vault_password_stops_the_login() {
    let (mut app, tmp) = vaulted_app("b21-empty");
    app.token_input = "токен".into();
    app.remember_account = true;
    app.login_password.clear();
    app.master_password.clear();

    app.login_with_token();

    assert!(
        !app.gw_started,
        "вход не должен продолжаться: подключаться с обещанием сохранить аккаунт нельзя"
    );
    assert!(
        app.status.contains("пароль хранилища"),
        "пользователь должен видеть, что делать: {:?}",
        app.status
    );
    // The notice survives the first frame since no gateway status will overwrite it.
    assert!(app.to_gw.is_none(), "гейтвей запускаться не должен");
    let _ = std::fs::remove_file(&tmp);
}

/// Failed vault unlock must also stop the login; the account wouldn't be saved.
#[test]
fn failed_vault_unlock_stops_the_login() {
    let (mut app, tmp) = vaulted_app("b21-wrong");
    app.saved_accounts = vec![StoredAccount { token: "старый".into(), username: "u".into() }];
    app.save_accounts("правильный");
    app.token_input = "токен".into();
    app.remember_account = true;
    app.login_password = "неправильный".into();
    app.master_password.clear();

    app.login_with_token();

    assert!(!app.gw_started, "вход с неверным паролем хранилища продолжаться не должен");
    assert!(
        app.status.contains("пароль хранилища"),
        "пользователь должен видеть, что делать: {:?}",
        app.status
    );
    let _ = std::fs::remove_file(&tmp);
}

/// Unchecked "remember" leaves login unaffected.
#[test]
fn login_without_remember_is_unaffected() {
    let (mut app, tmp) = vaulted_app("b21-plain");
    app.token_input = "токен".into();
    app.remember_account = false;
    app.login_password.clear();

    app.login_with_token();

    assert!(app.gw_started, "без «Запомнить» вход должен продолжаться");
    assert!(app.status.is_empty(), "вход без ошибок не должен ничего ругать: {:?}", app.status);
    let _ = std::fs::remove_file(&tmp);
}

/// Media caches and failure lists belong to the account and must not survive a switch.
#[test]
fn switching_account_drops_the_previous_accounts_media() {
    let mut app = make_app();
    app.no_gateway = true;
    // Token already listed, so add_saved_account won't write the vault.
    app.saved_accounts = vec![StoredAccount {
        token: "switch-me".into(),
        username: String::new(),
    }];

    let ctx = egui::Context::default();
    let handle = ctx.load_texture(
        "old-img",
        egui::ColorImage::new([4, 4], egui::Color32::BLUE),
        egui::TextureOptions::default(),
    );
    app.image_cache.insert(
        "https://cdn.discordapp.com/attachments/1/old.png".into(),
        LoadedImage::Static(handle),
    );
    let avatar = ctx.load_texture(
        "old-avatar",
        egui::ColorImage::new([4, 4], egui::Color32::RED),
        egui::TextureOptions::default(),
    );
    app.avatar_cache.insert("a1_hash".into(), avatar);
    app.failed_images.insert("https://cdn.example/broken.png".into());
    app.failed_avatars.insert("u2_bad".into());
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(Some(ImagePayload::Static(egui::ColorImage::new([512, 512], egui::Color32::BLACK))))
        .unwrap();
    app.pending_images.insert(
        "https://cdn.discordapp.com/attachments/1/pending.png".into(),
        rx,
    );

    app.switch_account("switch-me".into());

    assert!(
        app.image_cache.get("https://cdn.discordapp.com/attachments/1/old.png").is_none(),
        "кэш картинок прежнего аккаунта должен быть очищен"
    );
    assert!(app.avatar_cache.get("a1_hash").is_none(), "кэш аватаров прежнего аккаунта тоже");
    assert!(
        app.failed_images.is_empty(),
        "отказы картинок прежнего аккаунта не должны блокировать новый"
    );
    assert!(app.failed_avatars.is_empty(), "отказы аватаров тоже");
    assert!(app.pending_images.is_empty(), "незабранные загрузки не должны висеть в памяти");
    assert!(app.pending_avatars.is_empty());
}

/// Text in the composer is channel content, not a client command.
#[test]
fn slash_looking_text_is_sent_not_executed() {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    let mut app = app_with_channel(cmd_tx);

    app.handle_input("/quit");

    assert_eq!(
        app.messages.get("c1").map(|v| v.len()),
        Some(1),
        "/quit должен уйти в канал как сообщение, а не закрыть клиент"
    );
    assert_eq!(app.messages["c1"][0].content, "/quit");
    let sent: Vec<ToGateway> = std::iter::from_fn(|| cmd_rx.try_recv().ok()).collect();
    assert!(
        sent.iter().any(|c| matches!(c, ToGateway::Send { content, .. } if content == "/quit")),
        "текст должен уйти на отправку: {sent:?}"
    );
}

/// `/add` used to create a phantom channel and open it; debug commands now need confirmation.
#[test]
fn debug_channel_needs_confirmation() {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    let mut app = app_with_channel(cmd_tx);
    let channels_before = app.channels.len();

    app.handle_input("/add 123456789");
    assert_eq!(
        app.channels.len(),
        channels_before,
        "/add больше не команда — такой текст уходит в канал"
    );
    assert!(app.pending_debug_add.is_none());

    // Under its own prefix it's a command, but only after a repeat.
    let _ = std::iter::from_fn(|| cmd_rx.try_recv().ok()).count();
    app.handle_input("/debug add 123456789");
    assert_eq!(app.channels.len(), channels_before, "первый ввод только спрашивает");
    assert!(app.pending_debug_add.is_some(), "команда ждёт подтверждения");
    assert!(
        app.status.contains("123456789"),
        "пользователь должен понимать, что нажать: {:?}",
        app.status
    );

    app.handle_input("/debug add 123456789");
    assert_eq!(app.channels.len(), channels_before + 1, "повтор добавляет канал");
    assert!(app.pending_debug_add.is_none(), "после подтверждения ждать нечего");
    assert_eq!(app.channels.last().unwrap().id, "123456789");

    // A different id doesn't confirm; a new command is pending.
    let _ = std::iter::from_fn(|| cmd_rx.try_recv().ok()).count();
    app.handle_input("/debug add 111");
    let before = app.channels.len();
    app.handle_input("/debug add 222");
    assert_eq!(app.channels.len(), before, "подтверждением может быть только та же команда");
}

/// App with one open channel and a ready command receiver.
fn app_with_channel(cmd_tx: mpsc::UnboundedSender<ToGateway>) -> App {
    let (_, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.to_gw = Some(cmd_tx);
    app.user_id = "me".into();
    app.channels.push(ChatChannel {
        id: "c1".into(),
        name: "chan".into(),
        guild_id: None,
        channel_type: 1,
        topic: None,
        position: 0,
    });
    app.selected_channel = Some(0);
    app
}

/// A page loaded upward must not be immediately trimmed away when the list is at the cap.
#[test]
fn older_page_survives_the_memory_ceiling() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.to_gw = Some(cmd_tx);
    app.channels.push(ChatChannel {
        id: "c1".into(),
        name: "chan".into(),
        guild_id: None,
        channel_type: 1,
        topic: None,
        position: 0,
    });
    app.selected_channel = Some(0);
    // List full: 500 messages, m0 through m499.
    let full: Vec<Arc<ChatMessage>> = (0..MAX_MESSAGES_PER_CHANNEL)
        .map(|i| Arc::new(test_msg(&format!("m{i}"), "c1", "x")))
        .collect();
    app.messages.insert("c1".into(), full);

    let older: Vec<ChatMessage> = (1..=50).rev().map(|i| test_msg(&format!("old{i}"), "c1", "старое")).collect();
    tx.send(ToApp::HistoryMore { channel_id: "c1".into(), messages: older, more: false }).unwrap();
    app.poll(&ctx);

    let entry = app.messages.get("c1").unwrap();
    assert_eq!(entry.len(), MAX_MESSAGES_PER_CHANNEL, "потолок по памяти должен держаться");
    assert_eq!(
        entry[0].id, "old1",
        "только что загруженная страница обязана остаться в чате, а не исчезнуть"
    );
    // The newest ones were dropped instead, and the user is told.
    assert_eq!(
        app.trimmed_newest, 50,
        "сколько сообщений скрыто, должно быть известно: показать это молча нельзя"
    );
    assert_eq!(
        entry[MAX_MESSAGES_PER_CHANNEL - 1].id, "m449",
        "уйти должны самые новые, а не самые старые"
    );
}

/// Conversely, a normal update still drops the oldest so a busy channel stays bounded.
#[test]
fn new_message_still_drops_the_oldest_ones() {
    let (_, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    let mut entry: Vec<Arc<ChatMessage>> = (0..MAX_MESSAGES_PER_CHANNEL)
        .map(|i| Arc::new(test_msg(&format!("m{i}"), "c1", "x")))
        .collect();
    entry.push(Arc::new(test_msg("mine", "c1", "моё")));
    app.messages.insert("c1".into(), entry);

    let dropped = trim_messages(app.messages.get_mut("c1").unwrap(), true);
    let entry = app.messages.get("c1").unwrap();
    assert_eq!(entry.len(), MAX_MESSAGES_PER_CHANNEL);
    assert_eq!(entry[0].id, "m1", "своё сообщение вытесняет самое старое");
    assert_eq!(entry[MAX_MESSAGES_PER_CHANNEL - 1].id, "mine");
    assert_eq!(dropped, 1, "сколько выброшено — известно, но это не предел окна");
}

/// Account switch must bump the gateway generation so the old thread's events stop arriving.
#[test]
fn switching_account_supersedes_the_old_gateway() {
    let mut app = App::new(mpsc::unbounded_channel().1);
    app.user_id = "старая".into();
    app.username = "Старый".into();

    let first = app.gateway_generation.next();
    app.connected = true;

    // Switch: the previous generation must stop being current.
    app.switch_account("новыйтокен".into());
    assert!(
        !app.gateway_generation.is_current(first),
        "переключение аккаунта обязано поднять поколение"
    );
    assert!(!app.connected, "новый аккаунт ещё не подключился");
    assert!(app.gw_started);
}

/// A late older-page response after leaving the channel must not present mid-history as content.
#[test]
fn stale_older_page_into_cleared_list_asks_first_page_again() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.to_gw = Some(cmd_tx);
    for id in ["c1", "c2"] {
        app.channels.push(ChatChannel {
            id: id.into(),
            name: "chan".into(),
            guild_id: None,
            channel_type: 1,
            topic: None,
            position: 0,
        });
    }
    app.selected_channel = Some(0);
    app.open_channel("c1");
    let _ = std::iter::from_fn(|| cmd_rx.try_recv().ok()).count();
    // First page arrived, then an older-page request went out.
    let first: Vec<ChatMessage> = (1..=crate::gateway::HISTORY_PAGE)
        .rev()
        .map(|i| test_msg(&format!("m{i}"), "c1", "x"))
        .collect();
    tx.send(ToApp::History { channel_id: "c1".into(), messages: first, more: true }).unwrap();
    app.poll(&ctx);
    app.request_older_history();
    assert!(app.history_loading_more);

    // User switched channels (dropping the old list) and returned; now an empty list waits for the first page.
    app.selected_channel = Some(1);
    app.open_channel("c2");
    let _ = std::iter::from_fn(|| cmd_rx.try_recv().ok()).count();
    app.selected_channel = Some(0);
    app.open_channel("c1");
    let _ = std::iter::from_fn(|| cmd_rx.try_recv().ok()).count();
    assert!(app.messages.get("c1").is_none(), "open_channel чистит список канала");

    // The late older page arrives, from the request made before leaving.
    let older: Vec<ChatMessage> = (200..200 + crate::gateway::HISTORY_PAGE)
        .rev()
        .map(|i| test_msg(&format!("m{i}"), "c1", "старое"))
        .collect();
    tx.send(ToApp::HistoryMore { channel_id: "c1".into(), messages: older, more: true }).unwrap();
    app.poll(&ctx);

    assert!(
        app.messages.get("c1").is_none_or(|v| v.is_empty()),
        "середина истории не должна показываться как содержимое канала: {:?}",
        app.messages.get("c1").map(|v| v.len())
    );
    // Instead the first page was requested, which is where the list starts.
    let asked: Vec<ToGateway> = std::iter::from_fn(|| cmd_rx.try_recv().ok()).collect();
    assert!(
        asked.iter().any(|c| matches!(c, ToGateway::FetchHistory { before: None, .. })),
        "нужно попросить первую страницу заново, а не показывать середину: {asked:?}"
    );
    // The first-page spinner must stay on; it's waiting for that page.
    assert_eq!(app.history_loading.as_deref(), Some("c1"), "спиннер гасить рано");
}

/// Same for another channel: its page must not end up in our list.
#[test]
fn stale_older_page_of_other_channel_is_dropped() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.selected_channel = None;
    let page: Vec<ChatMessage> = (1..=5).rev().map(|i| test_msg(&format!("m{i}"), "c9", "x")).collect();
    tx.send(ToApp::HistoryMore { channel_id: "c9".into(), messages: page, more: true }).unwrap();
    app.poll(&ctx);
    assert!(
        app.messages.get("c9").is_none_or(|v| v.is_empty()),
        "поздняя догрузка в пустой список чужих сообщений класть нельзя"
    );
}

/// On a failed send, remove the echo and return the text instead of faking delivery.
#[test]
fn failed_send_removes_echo_and_returns_text() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.to_gw = Some(cmd_tx);
    app.user_id = "me".into();
    app.channels.push(ChatChannel {
        id: "c1".into(),
        name: "chan".into(),
        guild_id: None,
        channel_type: 1,
        topic: None,
        position: 0,
    });
    app.selected_channel = Some(0);
    app.open_channel("c1");
    let _ = std::iter::from_fn(|| cmd_rx.try_recv().ok()).count();

    app.handle_input("не отправится");
    // Enter clears the field immediately, before Discord responds.
    app.input.clear();
    let local_id = app.messages.get("c1").unwrap()[0].id.clone();
    assert!(local_id.starts_with(crate::models::LOCAL_ID_PREFIX));

    tx.send(ToApp::SendFailed {
        channel_id: "c1".into(),
        local_id: local_id.clone(),
        reason: "в этот канал писать нельзя".into(),
    })
    .unwrap();
    app.poll(&ctx);

    assert!(
        app.messages.get("c1").unwrap().is_empty(),
        "неотправленное сообщение не должно висеть в чате: {:?}",
        app.messages.get("c1")
    );
    assert_eq!(app.input, "не отправится", "текст надо вернуть, чтобы можно было повторить");
    assert!(
        app.send_error.as_deref().is_some_and(|r| r.contains("писать нельзя")),
        "пользователь должен видеть причину: {:?}",
        app.send_error
    );
}

/// If the user already started a new draft, don't restore the old text over it.
#[test]
fn failed_send_does_not_overwrite_new_draft() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.channels.push(ChatChannel {
        id: "c1".into(),
        name: "chan".into(),
        guild_id: None,
        channel_type: 1,
        topic: None,
        position: 0,
    });
    app.selected_channel = Some(0);
    app.open_channel("c1");
    app.user_id = "me".into();

    app.handle_input("старое");
    let local_id = app.messages.get("c1").unwrap()[0].id.clone();
    app.input = "новый черновик".into();
    tx.send(ToApp::SendFailed {
        channel_id: "c1".into(),
        local_id,
        reason: "нет связи с Discord".into(),
    })
    .unwrap();
    app.poll(&ctx);
    assert_eq!(app.input, "новый черновик", "черновик пользователя не должен затираться");
}

/// Sending clears the previous failure notice; it's no longer relevant.
#[test]
fn sending_clears_the_previous_failure_notice() {
    let (tx, rx) = mpsc::unbounded_channel();
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.to_gw = Some(cmd_tx);
    app.user_id = "me".into();
    app.channels.push(ChatChannel {
        id: "c1".into(),
        name: "chan".into(),
        guild_id: None,
        channel_type: 1,
        topic: None,
        position: 0,
    });
    app.selected_channel = Some(0);
    app.open_channel("c1");
    tx.send(ToApp::SendFailed {
        channel_id: "c1".into(),
        local_id: "local:0".into(),
        reason: "нет связи с Discord".into(),
    })
    .unwrap();
    app.poll(&egui::Context::default());
    assert!(app.send_error.is_some());

    app.handle_input("ещё раз");
    assert!(app.send_error.is_none(), "старая неудача не должна висеть над новым сообщением");
}

/// A live message already present in history is skipped to avoid duplicate rendering.
#[test]
fn live_message_already_in_history_is_not_doubled() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.channels.push(ChatChannel {
        id: "c1".into(),
        name: "chan".into(),
        guild_id: None,
        channel_type: 1,
        topic: None,
        position: 0,
    });
    app.selected_channel = Some(0);
    app.open_channel("c1");

    // History page as Discord sends it: newest-first.
    let page: Vec<ChatMessage> = (1..=3).rev().map(|i| test_msg(&format!("m{i}"), "c1", "из истории")).collect();
    tx.send(ToApp::History { channel_id: "c1".into(), messages: page, more: false }).unwrap();
    app.poll(&ctx);
    assert_eq!(app.messages.get("c1").unwrap().len(), 3);

    // The same m3 arrives live; it's already in the list.
    tx.send(ToApp::Message(test_msg("m3", "c1", "из истории"))).unwrap();
    app.poll(&ctx);
    let msgs = app.messages.get("c1").unwrap();
    assert_eq!(msgs.len(), 3, "живой дубль не должен добавляться");
    assert_eq!(msgs.iter().filter(|m| m.id == "m3").count(), 1, "сообщение должно быть одно");

    // A genuinely new one goes to the tail as usual.
    tx.send(ToApp::Message(test_msg("m4", "c1", "новое"))).unwrap();
    app.poll(&ctx);
    let msgs = app.messages.get("c1").unwrap();
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[msgs.len() - 1].id, "m4", "новое сообщение в конце списка");
}

/// Cache item with a declared byte weight.
struct Weighted(u32);

impl crate::models::CacheCost for Weighted {
    fn cache_bytes(&self) -> usize {
        self.0 as usize
    }
}

#[test]
fn bounded_cache_evicts_oldest() {
    let mut cache = BoundedCache::with_budget(3, usize::MAX);
    for i in 0..10 {
        cache.insert(format!("k{}", i), Weighted(i));
    }
    assert_eq!(cache.len(), 3, "кеш не должен расти дальше лимита");
    assert!(!cache.contains_key("k0"), "самый старый должен вытесниться");
    assert!(cache.contains_key("k9"), "свежее должно остаться");
    assert_eq!(cache.get("k9").map(|v| v.0), Some(9));
}

/// Evicts by least recent use, not insertion order; `get` must refresh the queue.
#[test]
fn bounded_cache_evicts_least_recently_used() {
    let mut cache = BoundedCache::with_budget(2, usize::MAX);
    cache.insert("a".into(), Weighted(1));
    cache.insert("b".into(), Weighted(2));
    // Touching "a" does nothing in FIFO but makes "b" older in LRU.
    assert_eq!(cache.get("a").map(|v| v.0), Some(1));
    cache.insert("c".into(), Weighted(3));

    assert!(cache.contains_key("a"), "к чему обращались, должно остаться");
    assert!(!cache.contains_key("b"), "вытесниться должно давнее по использованию");
    assert!(cache.contains_key("c"));
}

/// Byte budget outranks count: small avatars and large photos share one budget.
#[test]
fn bounded_cache_respects_byte_budget() {
    let mut cache = BoundedCache::with_budget(100, 250);
    for i in 0..4u32 {
        cache.insert(format!("k{}", i), Weighted(100));
    }
    assert_eq!(cache.len(), 2, "в бюджет 250 байт влезает только два по 100");
    assert!(!cache.contains_key("k0"), "самый старый вытесняется первым");
    assert!(cache.contains_key("k3"), "свежее остаётся");
    assert_eq!(cache.bytes(), 200, "счётчик памяти должен совпадать с содержимым");
}

/// Replacing a key must not double-count bytes.
#[test]
fn bounded_cache_replacing_key_keeps_bytes_right() {
    let mut cache = BoundedCache::with_budget(10, 1000);
    cache.insert("k".into(), Weighted(100));
    cache.insert("k".into(), Weighted(250));
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.bytes(), 250);
}

/// An item larger than the whole budget is still kept, or the chat would stay empty.
#[test]
fn bounded_cache_keeps_single_item_over_budget() {
    let mut cache = BoundedCache::with_budget(10, 100);
    cache.insert("k".into(), Weighted(500));
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.bytes(), 500);
}

/// A visible image must not be evicted by a freshly loaded one, or it would
/// flicker as it's refetched (especially large GIFs).
#[test]
fn bounded_cache_does_not_evict_visible_items() {
    let mut cache = BoundedCache::with_budget(10, 120);
    // Three images on screen, exactly filling the budget.
    cache.insert("a".into(), Weighted(40));
    cache.insert("b".into(), Weighted(40));
    cache.insert("c".into(), Weighted(40));
    cache.begin_frame();
    // This frame's draw marks all three.
    cache.mark_visible("a");
    cache.mark_visible("b");
    cache.mark_visible("c");
    // A fourth arrives and overflows: an invisible item should go, not a visible one.
    cache.insert("d".into(), Weighted(40));
    assert!(
        cache.contains_key("a") && cache.contains_key("b") && cache.contains_key("c"),
        "картинка, которая на экране, вытеснена только что загруженной"
    );

    // Over two frames a and b stop being drawn and are evicted, while visible c is kept.
    cache.begin_frame();
    cache.begin_frame();
    cache.mark_visible("c");
    cache.insert("e".into(), Weighted(40));
    assert!(cache.contains_key("c"), "видимая картинка не должна вытесняться");
    assert!(
        !cache.contains_key("a") && !cache.contains_key("b"),
        "давно не видимые должны освободить место"
    );
}

/// Live messages from closed channels must not accumulate; Discord sends them
/// for every channel but only one list is shown.
#[test]
fn live_messages_for_closed_channels_are_not_stored() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
    let mut app = app_with_channel(cmd_tx);
    app.from_gw = rx;

    // c1 is open; 200 messages go to c2 and one to c1.
    for i in 0..200 {
        tx.send(ToApp::Message(test_msg(&format!("b{i}"), "c2", "чужое"))).unwrap();
    }
    tx.send(ToApp::Message(test_msg("mine", "c1", "своё"))).unwrap();
    // Events are drained in batches, so loop until the queue empties before asserting.
    while !app.from_gw.is_empty() {
        app.poll(&ctx);
    }

    assert!(
        app.messages.get("c2").is_none_or(|e| e.is_empty()),
        "сообщения закрытого канала некому показывать, копить их незачем"
    );
    assert_eq!(app.messages.get("c1").map(|e| e.len()), Some(1), "открытый канал работает как раньше");
}

/// A message from a closed channel must not scroll the open chat down.
#[test]
fn live_message_from_another_channel_does_not_scroll_the_chat() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
    let mut app = app_with_channel(cmd_tx);
    app.from_gw = rx;
    app.chat_at_bottom = true;
    app.scroll_to_bottom = false;

    tx.send(ToApp::Message(test_msg("b1", "c2", "чужое"))).unwrap();
    app.poll(&ctx);
    assert!(!app.scroll_to_bottom, "сообщение чужого канала не должно прокручивать открытый чат");

    // An own message still scrolls.
    tx.send(ToApp::Message(test_msg("mine", "c1", "своё"))).unwrap();
    app.poll(&ctx);
    assert!(app.scroll_to_bottom, "сообщение открытого канала должно прокрутить вниз");
}

/// Debug log goes to file/stderr only with WYVERN_DEBUG.
#[test]
fn debug_log_writes_only_when_enabled() {
    let path = std::path::Path::new("/tmp/wyvern_layout.log");
    let _ = std::fs::remove_file(path);
    let (_tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);

    app.debug_to_disk = false;
    app.push_debug("тихая строка".into());
    assert!(!path.exists(), "без WYVERN_DEBUG журнал не должен писать в файл");

    app.debug_to_disk = true;
    app.push_debug("громкая строка".into());
    assert!(path.exists(), "с включённым флагом строка должна попасть в файл");

    let _ = std::fs::remove_file(path);
}

/// Old log lines are evicted from the front, not by shifting the whole vector.
///
/// Checks both contract (≤100, oldest out, newest kept) and structure (`remove(0)` must not return).
#[test]
fn debug_log_evicts_oldest_from_the_front() {
    let (_tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.debug_to_disk = false;
    for i in 0..150 {
        app.push_debug(format!("строка {i}"));
    }

    let log: &std::collections::VecDeque<String> = &app.debug_log;
    assert_eq!(log.len(), 100, "журнал не должен превышать сто строк");
    assert_eq!(log.front().map(String::as_str), Some("строка 50"));
    assert_eq!(log.back().map(String::as_str), Some("строка 149"));
}

/// Message edits apply immediately; MESSAGE_UPDATE used to be ignored.
#[test]
fn edit_replaces_message_text() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.messages.insert("c1".into(), vec![Arc::new(test_msg("m1", "c1", "до правки"))]);
    app.msg_heights.insert("m1".into(), MsgHeight { height: 40.0, width: 0 });

    let mut edited = test_msg("m1", "c1", "после правки");
    edited.author_name = String::new();
    tx.send(ToApp::MessageUpdated(edited)).unwrap();
    app.poll(&ctx);

    let msgs = app.messages.get("c1").unwrap();
    assert_eq!(msgs.len(), 1, "правка не должна добавлять вторую строку");
    assert_eq!(msgs[0].content, "после правки");
    assert!(
        !app.msg_heights.contains_key("m1"),
        "высота правленой строки должна быть сброшена"
    );
}

/// MESSAGE_UPDATE carries only changed fields, so an embed update must not wipe the text.
#[test]
fn partial_edit_keeps_old_text() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.messages.insert("c1".into(), vec![Arc::new(test_msg("m1", "c1", "текст остаётся"))]);

    let mut update = test_msg("m1", "c1", "");
    update.author_name = String::new();
    update.embeds.push(crate::models::Embed {
        description: Some("превью".into()),
        ..Default::default()
    });
    tx.send(ToApp::MessageUpdated(update)).unwrap();
    app.poll(&ctx);

    let msgs = app.messages.get("c1").unwrap();
    assert_eq!(msgs[0].content, "текст остаётся", "частичная правка не должна стирать текст");
    assert_eq!(msgs[0].embeds.len(), 1, "эмбед из правки должен примениться");
}

/// A deleted message disappears immediately, not after rejoin.
#[test]
fn delete_removes_message_and_height() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.messages.insert(
        "c1".into(),
        vec![
            Arc::new(test_msg("m1", "c1", "первое")),
            Arc::new(test_msg("m2", "c1", "второе")),
        ],
    );
    app.msg_heights.insert("m1".into(), MsgHeight { height: 30.0, width: 0 });

    tx.send(ToApp::MessageDeleted {
        channel_id: "c1".into(),
        message_id: "m1".into(),
    })
    .unwrap();
    app.poll(&ctx);

    let msgs = app.messages.get("c1").unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].id, "m2");
    assert!(!app.msg_heights.contains_key("m1"), "высота удалённой строки не нужна");
}

/// Bulk delete arrives as one event, not N.
#[test]
fn bulk_delete_removes_all_listed() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.messages.insert(
        "c1".into(),
        vec![
            Arc::new(test_msg("m1", "c1", "a")),
            Arc::new(test_msg("m2", "c1", "b")),
            Arc::new(test_msg("m3", "c1", "c")),
        ],
    );

    tx.send(ToApp::MessageDeletedBulk {
        channel_id: "c1".into(),
        message_ids: vec!["m1".into(), "m3".into()],
    })
    .unwrap();
    app.poll(&ctx);

    let msgs = app.messages.get("c1").unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].id, "m2");
}

/// Renames show immediately; a missing topic must not overwrite the old one.
#[test]
fn channel_update_renames_channel() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.channels.push(ChatChannel {
        id: "c1".into(),
        name: "старое".into(),
        guild_id: None,
        channel_type: 1,
        topic: Some("тема".into()),
        position: 0,
    });

    tx.send(ToApp::ChannelUpdated {
        channel_id: "c1".into(),
        name: Some("новое".into()),
        topic: None,
    })
    .unwrap();
    app.poll(&ctx);

    assert_eq!(app.channels[0].name, "новое");
    assert_eq!(
        app.channels[0].topic.as_deref(),
        Some("тема"),
        "отсутствующая тема не должна затираться"
    );
}

/// The gateway queue must not be fully drained in one frame; bursts of
/// `MESSAGE_CREATE` would stall it with per-message duplicate scans.
#[test]
fn gateway_backlog_is_drained_in_bounded_batches() {
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(rx);
    app.connected = true;
    app.gw_started = true;
    app.channels.push(ChatChannel {
        id: "c1".into(),
        name: "канал".into(),
        guild_id: None,
        channel_type: 1,
        topic: None,
        position: 0,
    });
    app.selected_channel = Some(0);

    let total = MAX_EVENTS_PER_FRAME + 25;
    for i in 0..total {
        tx.send(ToApp::Message(test_msg(
            &format!("m{i}"),
            "c1",
            &format!("сообщение {i}"),
        )))
        .unwrap();
    }

    app.poll(&ctx);
    assert_eq!(
        app.messages["c1"].len(),
        MAX_EVENTS_PER_FRAME,
        "за один кадр должно разбираться не больше {MAX_EVENTS_PER_FRAME} событий"
    );

    // The remainder isn't lost; the next poll takes it.
    app.poll(&ctx);
    assert_eq!(
        app.messages["c1"].len(),
        total,
        "остаток очереди должен разобраться следующим кадром"
    );
}
