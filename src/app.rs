use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, TextureHandle};
use tokio::sync::mpsc;

use crate::gateway::run_gateway;
use crate::messages::{ToApp, ToGateway};
use crate::models::{
    BoundedCache, ChatChannel, ChatMessage, Guild, ImagePayload, LoadedImage, StoredAccount,
    Theme, UserProfile,
};

/// Больше этого сообщений на канал не держим в памяти.
const MAX_MESSAGES_PER_CHANNEL: usize = 300;
/// Сколько текстур аватаров/иконок guild'ов держим.
const MAX_AVATAR_CACHE: usize = 192;
/// Сколько картинок-вложений держим (каждая — это мегабайты VRAM/RAM).
const MAX_IMAGE_CACHE: usize = 32;
/// Сколько неудачных URL'ов запоминаем, чтобы не качать их снова.
pub(crate) const MAX_FAILED_IMAGES: usize = 512;

/// Оставляем только последние N сообщений: иначе активный канал в шумном
/// чате раздувает память бесконечно.
fn trim_messages(entry: &mut Vec<Arc<ChatMessage>>) {
    if entry.len() > MAX_MESSAGES_PER_CHANNEL {
        let extra = entry.len() - MAX_MESSAGES_PER_CHANNEL;
        entry.drain(..extra);
    }
}

pub(crate) struct App {
    pub(crate) connected: bool,
    pub(crate) username: String,
    pub(crate) user_id: String,
    pub(crate) user_avatar: Option<String>,
    pub(crate) guilds: Vec<Guild>,
    pub(crate) channels: Vec<ChatChannel>,
    pub(crate) selected_guild: Option<usize>,
    pub(crate) selected_channel: Option<usize>,
    pub(crate) messages: HashMap<String, Vec<Arc<ChatMessage>>>,
    pub(crate) input: String,
    pub(crate) token_input: String,
    pub(crate) master_password: String,
    pub(crate) login_password: String,
    /// Не-ошибка на экране входа («пароль принят после обрезки пробелов»).
    pub(crate) login_notice: String,
    pub(crate) login_selected: Option<String>,
    pub(crate) remember_account: bool,
    pub(crate) saved_accounts: Vec<StoredAccount>,
    pub(crate) active_index: Option<usize>,
    pub(crate) status: String,
    pub(crate) debug_log: Vec<String>,
    pub(crate) to_gw: Option<mpsc::UnboundedSender<ToGateway>>,
    pub(crate) from_gw: mpsc::UnboundedReceiver<ToApp>,
    pub(crate) gw_started: bool,
    pub(crate) avatar_cache: BoundedCache<TextureHandle>,
    pub(crate) pending_avatars: HashMap<String, std::sync::mpsc::Receiver<Option<egui::ColorImage>>>,
    pub(crate) image_cache: BoundedCache<LoadedImage>,
    pub(crate) pending_images: HashMap<String, std::sync::mpsc::Receiver<Option<ImagePayload>>>,
    pub(crate) failed_images: HashSet<String>,
    pub(crate) theme: Theme,
    pub(crate) show_friends: bool,
    pub(crate) friends: Vec<UserProfile>,
    pub(crate) history_loading: Option<String>,
    pub(crate) autoselected: bool,
    pub(crate) accounts_unlocked: bool,
    pub(crate) theme_index: u8,
    pub(crate) last_render_key: String,
    pub(crate) scroll_to_bottom: bool,
    pub(crate) debug_frames: u64,
    pub(crate) last_scroll_offset_y: f32,
}

impl App {
    pub(crate) fn new(from_gw: mpsc::UnboundedReceiver<ToApp>) -> Self {
        Self {
            connected: false,
            username: String::new(),
            user_id: String::new(),
            user_avatar: None,
            guilds: Vec::new(),
            channels: Vec::new(),
            selected_guild: None,
            selected_channel: None,
            messages: HashMap::new(),
            input: String::new(),
            token_input: String::new(),
            master_password: String::new(),
            login_password: String::new(),
            login_notice: String::new(),
            login_selected: None,
            remember_account: true,
            saved_accounts: Vec::new(),
            active_index: None,
            status: String::new(),
            debug_log: Vec::new(),
            to_gw: None,
            from_gw,
            gw_started: false,
            accounts_unlocked: false,
            avatar_cache: BoundedCache::new(MAX_AVATAR_CACHE),
            pending_avatars: HashMap::new(),
            image_cache: BoundedCache::new(MAX_IMAGE_CACHE),
            pending_images: HashMap::new(),
            failed_images: HashSet::new(),
            theme: Theme::dark(),
            show_friends: false,
            friends: Vec::new(),
            history_loading: None,
            autoselected: false,
            theme_index: 1,
            last_render_key: String::new(),
            scroll_to_bottom: true,
            debug_frames: 0,
            last_scroll_offset_y: 0.0,
        }
    }
    pub(crate) fn push_debug(&mut self, msg: String) {
        let line = format!("[GW] {}", msg);
        eprintln!("{}", line);
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/wyvern_layout.log") {
            let _ = writeln!(f, "{}", line);
        }
        self.debug_log.push(msg);
        if self.debug_log.len() > 100 {
            self.debug_log.remove(0);
        }
    }
    pub(crate) fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.from_gw.try_recv() {
            match ev {
                ToApp::Ready { username, user_id, avatar } => {
                    self.username = username.clone();
                    self.user_id = user_id;
                    self.user_avatar = avatar;
                    self.connected = true;
                    self.status = format!("Online: {}", self.username);
                    let tkn = self.token_input.clone();
                    if self.remember_account {
                        self.add_saved_account(&tkn, &username);
                    } else {
                        // Аккаунт уже в списке — просто дописываем имя, если его не было.
                        let blank = self
                            .saved_accounts
                            .iter()
                            .position(|a| a.token == tkn && a.username.is_empty());
                        if let Some(i) = blank {
                            self.saved_accounts[i].username = username.clone();
                            self.save_accounts(&self.master_password);
                        }
                    }
                    self.push_debug("READY received!".into());
                }
                ToApp::Message(msg) => {
                    let cid = msg.channel_id.clone();
                    let entry = self.messages.entry(cid).or_default();
                    entry.push(Arc::new(msg));
                    trim_messages(entry);
                }
                ToApp::History { channel_id, messages } => {
                    let entry = self.messages.entry(channel_id.clone()).or_default();
                    entry.clear();
                    entry.extend(messages.into_iter().map(Arc::new));
                    trim_messages(entry);
                    let stored = entry.len();
                    let dump = entry.iter()
                        .map(|m| format!("[{}] {} (id {}): {}{}", m.timestamp, m.author_name, m.author_id, m.content,
                            if m.attachments.is_empty() { String::new() } else { format!(" <{} attachments>", m.attachments.len()) }))
                        .collect::<Vec<_>>()
                        .join("\n");
                    let _ = std::fs::write("/tmp/wyvern_messages_dump.txt", dump);
                    let cid_short = if channel_id.len() > 14 { channel_id[..14].to_string() } else { channel_id.clone() };
                    self.history_loading = None;
                    self.scroll_to_bottom = true;
                    self.push_debug(format!("Stored {} msgs for channel {}", stored, cid_short));
                }
                ToApp::Guild(g) => {
                    if !self.guilds.iter().any(|x| x.id == g.id) {
                        self.guilds.push(g);
                    }
                }
                ToApp::Channel(ch) => {
                    if !self.channels.iter().any(|x| x.id == ch.id) {
                        self.channels.push(ch);
                    }
                }
                ToApp::GuildChannels { guild_id, channels } => {
                    let before = self.channels.len();
                    for ch in channels {
                        if !self.channels.iter().any(|x| x.id == ch.id) {
                            self.channels.push(ch);
                        }
                    }
                    if self.channels.len() > before {
                        self.push_debug(format!("Guild {}: +{} channels", guild_id, self.channels.len() - before));
                    }

                    if !self.autoselected && self.selected_guild.is_none() && self.selected_channel.is_none() && !self.channels.is_empty() {
                        let guild_idx = self.guilds.iter().position(|g| g.id == guild_id)
                            .or_else(|| self.guilds.iter().position(|g| {
                                self.channels.iter().any(|c| c.guild_id.as_deref() == Some(g.id.as_str()))
                            }))
                            .unwrap_or(0);
                        self.selected_guild = Some(guild_idx);
                        self.autoselected = true;

                        let gid = self.guilds.get(guild_idx).map(|g| g.id.clone());
                        if let Some(gid) = gid {
                            let chan_idx = self.channels.iter().position(|c| c.guild_id.as_deref() == Some(gid.as_str()));
                            if let Some(chan_idx) = chan_idx {
                                self.selected_channel = Some(chan_idx);
                                let cid = self.channels[chan_idx].id.clone();
                                self.open_channel(&cid);
                                self.push_debug(format!("Auto-selected channel {}", self.channels[chan_idx].name));
                            }
                        }
                    }
                }
                ToApp::DMChannel(ch) => {
                    self.push_debug(format!("Opening DM '{}' id={}", ch.name, &ch.id[..ch.id.len().min(14)]));
                    let idx = if let Some(idx) = self.channels.iter().position(|c| c.id == ch.id) {
                        idx
                    } else {
                        self.channels.push(ch.clone());
                        self.channels.len() - 1
                    };
                    self.selected_guild = None;
                    self.selected_channel = Some(idx);
                    self.show_friends = false;
                    self.scroll_to_bottom = true;
                    let cid = self.channels[idx].id.clone();
                    self.open_channel(&cid);
                }
                ToApp::UserUpdate { id, username, avatar, nickname } => {
                    for msg in self.messages.values_mut().flat_map(|v| v.iter_mut()) {
                        if msg.author_id == id {
                            // Сообщение могло уже раздаваться в рендер, поэтому
                            // правим копию, а не на месте.
                            let m = Arc::make_mut(msg);
                            m.author_name = username.clone();
                            m.author_avatar = avatar.clone();
                            if nickname.is_some() {
                                m.nickname = nickname.clone();
                            }
                        }
                    }
                    for ch in self.channels.iter_mut() {
                        if ch.id == id {
                            ch.name = username.clone();
                        }
                    }
                }
                ToApp::Friends(list) => {
                    self.friends = list;
                    self.push_debug(format!("Loaded {} friends", self.friends.len()));
                }
                ToApp::Status(s) => self.status = s,
                ToApp::Debug(d) => self.push_debug(d),
            }
        }
        ctx.request_repaint_after(Duration::from_millis(50));
    }
    pub(crate) fn start_gateway(&mut self, token: String) {
        let (to_gw_tx, to_gw_rx) = mpsc::unbounded_channel();
        let (from_gw_tx, from_gw_rx) = mpsc::unbounded_channel();
        self.to_gw = Some(to_gw_tx);
        self.from_gw = from_gw_rx;
        self.gw_started = true;

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(run_gateway(to_gw_rx, from_gw_tx, token));
        });
    }
    pub(crate) fn send_cmd(&self, cmd: ToGateway) {
        if let Some(tx) = &self.to_gw {
            let _ = tx.send(cmd);
        }
    }
}

impl App {
    pub(crate) fn switch_account(&mut self, token: String) {
        let token = token.trim().to_string();
        if token.is_empty() {
            return;
        }
        self.send_cmd(ToGateway::Shutdown);
        self.to_gw = None;
        self.gw_started = false;
        self.connected = false;
        self.guilds.clear();
        self.channels.clear();
        self.messages.clear();
        self.friends.clear();
        self.selected_guild = None;
        self.selected_channel = None;
        self.autoselected = false;
        self.history_loading = None;
        self.show_friends = false;
        self.username.clear();
        self.user_id.clear();
        self.user_avatar = None;
        self.token_input = token.clone();
        self.start_gateway(token.clone());
        self.add_saved_account(&token, "");
        self.push_debug(format!("Switched account to {}", self.mask_token(&token)));
    }
    /// Вход по свежему токену из формы логина.
    /// Если включён "запомнить" — сначала открываем/создаём хранилище паролем.
    pub(crate) fn login_with_token(&mut self) {
        let token = self.token_input.trim().to_string();
        if token.is_empty() {
            return;
        }
        self.status.clear();
        self.login_notice.clear();
        if self.remember_account {
            let pw = if self.login_password.is_empty() {
                self.master_password.clone()
            } else {
                self.login_password.clone()
            };
            if pw.is_empty() {
                // Запомнить без пароля нельзя: файл надо чем-то шифровать.
                self.status = "Чтобы запомнить аккаунт, введи пароль хранилища".to_string();
            } else {
                match self.unlock_vault(&pw) {
                    Ok(Some(hint)) => self.login_notice = hint,
                    Ok(None) => {}
                    // Вход всё равно продолжаем — просто аккаунт не сохранится.
                    Err(e) => self.status = format!("{} — аккаунт не сохранён", e),
                }
            }
        }
        self.login_selected = None;
        self.token_input = token.clone();
        self.start_gateway(token.clone());
        if self.accounts_unlocked {
            self.add_saved_account(&token, "");
        }
    }
    /// Открыть канал. История всё равно грузится заново, поэтому сообщения
    /// других каналов можно выбросить — память не растёт при переключении.
    pub(crate) fn open_channel(&mut self, channel_id: &str) {
        self.history_loading = Some(channel_id.to_string());
        self.messages.retain(|k, _| k == channel_id);
        self.send_cmd(ToGateway::FetchHistory { channel_id: channel_id.to_string() });
    }
    pub(crate) fn display_name(&self, msg: &ChatMessage) -> String {
        msg.nickname.clone().unwrap_or_else(|| msg.author_name.clone())
    }
    pub(crate) fn display_content(&self, msg: &ChatMessage) -> String {
        if !msg.content.trim().is_empty() {
            return msg.content.clone();
        }
        for att in &msg.attachments {
            if let Some(d) = &att.description {
                if !d.trim().is_empty() {
                    return d.clone();
                }
            }
        }
        for e in &msg.embeds {
            if let Some(d) = e["description"].as_str() {
                if !d.trim().is_empty() {
                    return d.to_string();
                }
            }
        }
        String::new()
    }
    pub(crate) fn short_time(&self, iso: &str) -> String {
        let t = iso.trim_start_matches('T');
        if t.len() >= 16 {
            t[11..16].to_string()
        } else {
            iso.to_string()
        }
    }
    pub(crate) fn guild_channels(&self, guild_id: &str) -> Vec<&ChatChannel> {
        self.channels.iter()
            .filter(|ch| ch.guild_id.as_deref() == Some(guild_id) && ch.channel_type == 0)
            .collect()
    }
    /// Сообщения текущего канала. Отдаём `Arc`, поэтому вызывающий код
    /// копирует только указатели, а не все сообщения целиком.
    pub(crate) fn current_channel_messages(&self) -> Vec<Arc<ChatMessage>> {
        self.selected_channel
            .and_then(|i| self.channels.get(i))
            .map(|ch| ch.id.clone())
            .and_then(|id| self.messages.get(&id))
            .cloned()
            .unwrap_or_default()
            .into()
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);

        self.theme = match self.theme_index {
            0 => Theme::dark(),
            1 => Theme::cyberpunk(),
            _ => Theme::light(),
        };

        if self.token_input.is_empty() || (!self.connected && !self.gw_started) {
            self.draw_login(ctx);
        } else {
            self.draw_chat(ctx);
        }
    }
}


#[cfg(test)]
mod layout_tests {
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
            owner_id: "".into(),
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

    /// Изолированный файл хранилища на время теста: никто не трогает
    /// настоящий ~/.wyvern_accounts.json.
    struct TempVault {
        path: std::path::PathBuf,
    }
    impl TempVault {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            p.push(format!("wyvern-test-{}-{}.json", tag, std::process::id()));
            let _ = std::fs::remove_file(&p);
            std::env::set_var("WYVERN_ACCOUNTS_PATH", &p);
            Self { path: p }
        }
    }
    impl Drop for TempVault {
        fn drop(&mut self) {
            std::env::remove_var("WYVERN_ACCOUNTS_PATH");
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// Пробелы в пароле — самая частая причина «неверного пароля».
    #[test]
    fn vault_tolerates_password_spaces() {
        for saved in ["hunter2", "hunter2 ", " hunter2", "hunter2\n"] {
            let _vault = TempVault::new("space");
            let accs = vec![StoredAccount { token: "tok-space".into(), username: "bob".into() }];
            let mut app = App::new(mpsc::unbounded_channel().1);
            app.saved_accounts = accs;
            app.save_accounts(saved);
            assert!(App::accounts_path().exists(), "файл не записался");

            // Вводим без пробелов — должно открыться.
            let mut app2 = App::new(mpsc::unbounded_channel().1);
            let hint = app2.unlock_vault("hunter2").expect("пароль без пробелов должен подойти");
            assert_eq!(app2.saved_accounts.len(), 1, "аккаунт не загрузился (saved={:?})", saved);
            assert_eq!(app2.saved_accounts[0].username, "bob");
            if saved == "hunter2" {
                assert!(hint.is_none(), "для точного пароля подсказки быть не должно");
            } else {
                assert!(hint.is_some(), "для пароля с пробелами ждём подсказку, saved={:?}", saved);
            }
        }
    }

    /// Хранилище без аккаунтов — это не «неверный пароль».
    #[test]
    fn empty_vault_is_not_a_wrong_password() {
        let _vault = TempVault::new("empty");
        let encrypted = App::encrypt_accounts(&[], "pw123").expect("encrypt");
        std::fs::write(App::accounts_path(), encrypted).unwrap();

        let mut app = App::new(mpsc::unbounded_channel().1);
        let res = app.unlock_vault("pw123");
        assert!(res.is_ok(), "пустое хранилище с верным паролем должно открываться: {:?}", res);
        assert!(app.saved_accounts.is_empty());
        assert!(app.accounts_unlocked);
    }

    /// Неверный пароль — честная ошибка, а не тихий пустой список.
    #[test]
    fn wrong_password_still_errors() {
        let _vault = TempVault::new("wrong");
        let encrypted = App::encrypt_accounts(
            &[StoredAccount { token: "t".into(), username: "u".into() }],
            "right",
        )
        .expect("encrypt");
        std::fs::write(App::accounts_path(), encrypted).unwrap();

        let mut app = App::new(mpsc::unbounded_channel().1);
        let res = app.unlock_vault("wrong");
        let err = res.err().expect("неверный пароль должен давать ошибку");
        assert!(err.contains("Неверный пароль"), "непонятное сообщение: {}", err);
        assert!(!app.accounts_unlocked);
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
        trim_messages(&mut entry);
        assert_eq!(entry.len(), MAX_MESSAGES_PER_CHANNEL);
        // Остались самые новые, порядок сохранён.
        assert_eq!(entry[0].id, "m50");
        assert_eq!(entry[MAX_MESSAGES_PER_CHANNEL - 1].id, format!("m{}", MAX_MESSAGES_PER_CHANNEL + 49));

        // При открытии канала история других каналов выбрасывается.
        let mut app = make_app();
        app.messages.insert("c1".into(), vec![entry[0].clone()]);
        app.open_channel("c0");
        assert!(app.messages.contains_key("c0"), "активный канал должен остаться");
        assert!(!app.messages.contains_key("c1"), "история других каналов должна быть выброшена");
    }

    #[test]
    fn bounded_cache_evicts_oldest() {
        let mut cache = BoundedCache::new(3);
        for i in 0..10 {
            cache.insert(format!("k{}", i), i);
        }
        assert_eq!(cache.len(), 3, "кеш не должен расти дальше лимита");
        assert!(!cache.contains_key("k0"), "самый старый должен вытесниться");
        assert!(cache.contains_key("k9"), "свежее должно остаться");
        assert_eq!(cache.get("k9"), Some(&9));
    }
}
