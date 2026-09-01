use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use eframe::egui::{self, Color32, RichText, TextureHandle};
use futures_util::{SinkExt, StreamExt};
use rand::RngCore;
use serde_json::{json, Value};
use sha2::Sha256;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::connect_async;

const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
const API_BASE: &str = "https://discord.com/api/v10";
const CDN_BASE: &str = "https://cdn.discordapp.com";
const MAX_CONCURRENT_AVATAR_DOWNLOADS: usize = 4;
static AVATAR_DOWNLOADS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

fn base64_encode(s: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
}

fn base64_string(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

fn super_props() -> String {
    base64_encode(&serde_json::to_string(&json!({
        "os": "Linux",
        "browser": "Discord Client",
        "device": "",
        "release_channel": "stable",
        "client_build_number": 361909,
        "client_event_source": null
    })).unwrap())
}

fn auth_headers() -> Vec<(&'static str, String)> {
    vec![
        ("User-Agent".into(), "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36".into()),
        ("X-Super-Properties".into(), super_props()),
        ("X-Discord-Locale".into(), "en-US".into()),
        ("X-Discord-Timezone".into(), "Europe/Moscow".into()),
    ]
}

#[derive(Clone, Debug)]
struct ChatMessage {
    id: String,
    channel_id: String,
    author_id: String,
    author_name: String,
    author_avatar: Option<String>,
    nickname: Option<String>,
    content: String,
    timestamp: String,
    attachments: Vec<Attachment>,
    embeds: Vec<Value>,
    is_own: bool,
}

#[derive(Clone, Debug)]
struct Attachment {
    filename: String,
    url: String,
    content_type: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    size: u64,
    description: Option<String>,
}

#[derive(Clone, Debug)]
struct ChatChannel {
    id: String,
    name: String,
    guild_id: Option<String>,
    channel_type: i64,
    topic: Option<String>,
    position: i32,
}

#[derive(Clone, Debug)]
struct Guild {
    id: String,
    name: String,
    icon: Option<String>,
    owner_id: String,
}

#[derive(Clone, Debug)]
struct UserProfile {
    id: String,
    username: String,
    avatar: Option<String>,
    discriminator: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct StoredAccount {
    token: String,
    username: String,
}

#[derive(Clone)]
enum LoadedImage {
    Static(TextureHandle),
    Animated {
        frames: Vec<TextureHandle>,
        delays: Vec<f32>,
        started: std::time::Instant,
    },
}

enum ImagePayload {
    Static(egui::ColorImage),
    Animated { frames: Vec<(egui::ColorImage, f32)> },
}

impl LoadedImage {
    fn size_vec2(&self) -> egui::Vec2 {
        match self {
            LoadedImage::Static(t) => t.size_vec2(),
            LoadedImage::Animated { frames, .. } => frames[0].size_vec2(),
        }
    }

    fn id(&self) -> egui::TextureId {
        self.display_texture().id()
    }

    fn display_texture(&self) -> TextureHandle {
        match self {
            LoadedImage::Static(t) => t.clone(),
            LoadedImage::Animated { frames, delays, started } => {
                if frames.len() == 1 || delays.iter().all(|d| *d <= 0.0) {
                    return frames[0].clone();
                }
                let total: f32 = delays.iter().sum();
                let mut elapsed = started.elapsed().as_secs_f32();
                if total > 0.0 {
                    elapsed = elapsed % total;
                }
                let mut acc = 0.0f32;
                for (i, d) in delays.iter().enumerate() {
                    acc += d;
                    if elapsed < acc {
                        return frames[i].clone();
                    }
                }
                frames.last().cloned().unwrap_or_else(|| frames[0].clone())
            }
        }
    }
}

#[derive(Clone, Debug)]
enum ToApp {
    Ready { username: String, user_id: String, avatar: Option<String> },
    Message(ChatMessage),
    History { channel_id: String, messages: Vec<ChatMessage> },
    Guild(Guild),
    Channel(ChatChannel),
    GuildChannels { guild_id: String, channels: Vec<ChatChannel> },
    DMChannel(ChatChannel),
    UserUpdate { id: String, username: String, avatar: Option<String>, nickname: Option<String> },
    Friends(Vec<UserProfile>),
    Status(String),
    Debug(String),
}

#[derive(Debug)]
enum ToGateway {
    Send { channel_id: String, content: String },
    FetchHistory { channel_id: String },
    OpenDM { user_id: String },
    Shutdown,
}

struct App {
    connected: bool,
    username: String,
    user_id: String,
    user_avatar: Option<String>,
    guilds: Vec<Guild>,
    channels: Vec<ChatChannel>,
    selected_guild: Option<usize>,
    selected_channel: Option<usize>,
    messages: HashMap<String, Vec<ChatMessage>>,
    input: String,
    token_input: String,
    master_password: String,
    saved_accounts: Vec<StoredAccount>,
    login_new_token: String,
    active_index: Option<usize>,
    status: String,
    debug_log: Vec<String>,
    to_gw: Option<mpsc::UnboundedSender<ToGateway>>,
    from_gw: mpsc::UnboundedReceiver<ToApp>,
    gw_started: bool,
    avatar_cache: HashMap<String, TextureHandle>,
    pending_avatars: HashMap<String, std::sync::mpsc::Receiver<Option<egui::ColorImage>>>,
    image_cache: HashMap<String, LoadedImage>,
    pending_images: HashMap<String, std::sync::mpsc::Receiver<Option<ImagePayload>>>,
    failed_images: HashSet<String>,
    theme: Theme,
    show_friends: bool,
    friends: Vec<UserProfile>,
    history_loading: Option<String>,
    autoselected: bool,
    accounts_unlocked: bool,
    theme_index: u8,
    last_render_key: String,
    scroll_to_bottom: bool,
    debug_frames: u64,
    last_scroll_offset_y: f32,
}

#[derive(Clone, Debug)]
struct Theme {
    bg: Color32,
    panel_bg: Color32,
    channel_bg: Color32,
    text: Color32,
    text_secondary: Color32,
    accent: Color32,
    input_bg: Color32,
    message_hover: Color32,
    divider: Color32,
    self_bg: Color32,
}

impl Theme {
    fn dark() -> Self {
        Self {
            bg: Color32::from_rgb(49, 51, 56),
            panel_bg: Color32::from_rgb(42, 44, 48),
            channel_bg: Color32::from_rgb(30, 31, 34),
            text: Color32::from_rgb(220, 221, 222),
            text_secondary: Color32::from_rgb(114, 118, 125),
            accent: Color32::from_rgb(88, 101, 242),
            input_bg: Color32::from_rgb(64, 68, 75),
            message_hover: Color32::from_rgb(50, 52, 57),
            divider: Color32::from_rgb(66, 68, 72),
            self_bg: Color32::from_rgb(55, 58, 64),
        }
    }

    fn cyberpunk() -> Self {
        Self {
            bg: Color32::from_rgb(16, 12, 32),
            panel_bg: Color32::from_rgb(22, 17, 42),
            channel_bg: Color32::from_rgb(28, 22, 52),
            text: Color32::from_rgb(223, 226, 255),
            text_secondary: Color32::from_rgb(140, 142, 185),
            accent: Color32::from_rgb(0, 229, 255),
            input_bg: Color32::from_rgb(34, 28, 62),
            message_hover: Color32::from_rgb(32, 25, 58),
            divider: Color32::from_rgb(60, 52, 110),
            self_bg: Color32::from_rgb(36, 29, 66),
        }
    }

    fn light() -> Self {
        Self {
            bg: Color32::from_rgb(232, 234, 237),
            panel_bg: Color32::from_rgb(242, 243, 245),
            channel_bg: Color32::from_rgb(255, 255, 255),
            text: Color32::from_rgb(30, 31, 34),
            text_secondary: Color32::from_rgb(120, 122, 128),
            accent: Color32::from_rgb(70, 96, 220),
            input_bg: Color32::from_rgb(228, 230, 234),
            message_hover: Color32::from_rgb(240, 241, 244),
            divider: Color32::from_rgb(220, 222, 226),
            self_bg: Color32::from_rgb(235, 237, 241),
        }
    }
}

impl App {
    fn new(from_gw: mpsc::UnboundedReceiver<ToApp>) -> Self {
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
            saved_accounts: Vec::new(),
            login_new_token: String::new(),
            active_index: None,
            status: String::new(),
            debug_log: Vec::new(),
            to_gw: None,
            from_gw,
            gw_started: false,
            accounts_unlocked: false,
            avatar_cache: HashMap::new(),
            pending_avatars: HashMap::new(),
            image_cache: HashMap::new(),
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

    fn push_debug(&mut self, msg: String) {
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

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.from_gw.try_recv() {
            match ev {
                ToApp::Ready { username, user_id, avatar } => {
                    self.username = username.clone();
                    self.user_id = user_id;
                    self.user_avatar = avatar;
                    self.connected = true;
                    self.status = format!("Online: {}", self.username);
                    let tkn = self.token_input.clone();
                    self.add_saved_account(&tkn, &username);
                    self.push_debug("READY received!".into());
                }
                ToApp::Message(msg) => {
                    self.messages.entry(msg.channel_id.clone()).or_default().push(msg);
                }
                ToApp::History { channel_id, messages } => {
                    let entry = self.messages.entry(channel_id.clone()).or_default();
                    entry.clear();
                    entry.extend(messages);
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
                                self.history_loading = Some(cid.clone());
                                self.send_cmd(ToGateway::FetchHistory { channel_id: cid });
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
                    self.history_loading = Some(cid.clone());
                    self.send_cmd(ToGateway::FetchHistory { channel_id: cid });
                }
                ToApp::UserUpdate { id, username, avatar, nickname } => {
                    for msg in self.messages.values_mut().flat_map(|v| v.iter_mut()) {
                        if msg.author_id == id {
                            msg.author_name = username.clone();
                            msg.author_avatar = avatar.clone();
                            if nickname.is_some() {
                                msg.nickname = nickname.clone();
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

    fn start_gateway(&mut self, token: String) {
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

    fn send_cmd(&self, cmd: ToGateway) {
        if let Some(tx) = &self.to_gw {
            let _ = tx.send(cmd);
        }
    }

    fn accounts_path() -> std::path::PathBuf {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            std::path::PathBuf::from(".wyvern_accounts.json")
        } else {
            std::path::Path::new(&home).join(".wyvern_accounts.json")
        }
    }

    fn derive_key(password: &str, salt: &[u8]) -> [u8; 32] {
        let mut key = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, 100_000, &mut key);
        key
    }

    fn encrypt_accounts(accounts: &[StoredAccount], password: &str) -> Option<String> {
        if password.is_empty() {
            return None;
        }
        let mut salt = [0u8; 16];
        let mut nonce_bytes = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut salt);
        rand::thread_rng().fill_bytes(&mut nonce_bytes);

        let key = Self::derive_key(password, &salt);
        let cipher = Aes256Gcm::new_from_slice(&key).ok()?;
        let nonce = Nonce::from_slice(&nonce_bytes);
        let plaintext = serde_json::to_string(accounts).ok()?;
        let ct = cipher.encrypt(nonce, plaintext.as_bytes()).ok()?;

        Some(json!({
            "version": 1,
            "salt": base64_string(&salt),
            "nonce": base64_string(&nonce_bytes),
            "data": base64_string(&ct),
        }).to_string())
    }

    fn decrypt_accounts(content: &str, password: &str) -> Option<Vec<StoredAccount>> {
        if password.is_empty() {
            return None;
        }
        let v: Value = serde_json::from_str(content).ok()?;
        let salt = base64_decode(&v["salt"].as_str()?.to_string())?;
        let nonce_bytes = base64_decode(&v["nonce"].as_str()?.to_string())?;
        let ct = base64_decode(&v["data"].as_str()?.to_string())?;

        let key = Self::derive_key(password, &salt);
        let cipher = Aes256Gcm::new_from_slice(&key).ok()?;
        let nonce = Nonce::from_slice(&nonce_bytes);
        let pt = cipher.decrypt(nonce, ct.as_slice()).ok()?;
        serde_json::from_slice::<Vec<StoredAccount>>(&pt).ok()
    }

    fn load_accounts(password: &str) -> Vec<StoredAccount> {
        let path = Self::accounts_path();
        if let Ok(s) = std::fs::read_to_string(&path) {
            if let Ok(v) = serde_json::from_str::<Vec<StoredAccount>>(&s) {
                if password.is_empty() {
                    return Vec::new();
                }
                return v;
            }
            if let Some(accs) = Self::decrypt_accounts(&s, password) {
                return accs;
            }
        }
        Vec::new()
    }

    fn save_accounts(&self, password: &str) {
        if let Some(s) = Self::encrypt_accounts(&self.saved_accounts, password) {
            let _ = std::fs::write(Self::accounts_path(), s);
        }
    }

    fn mask_token(&self, token: &str) -> String {
        if token.len() <= 8 {
            return "••••••••".to_string();
        }
        let first = &token[..4];
        let last = &token[token.len() - 4..];
        format!("{}…{}", first, last)
    }

    fn add_saved_account(&mut self, token: &str, username: &str) {
        let token = token.trim().to_string();
        if token.is_empty() {
            return;
        }
        if let Some(a) = self.saved_accounts.iter_mut().find(|a| a.token == token) {
            if !username.is_empty() {
                a.username = username.to_string();
            }
        } else {
            self.saved_accounts.push(StoredAccount {
                token,
                username: username.to_string(),
            });
        }
        self.refresh_active_index();
        self.save_accounts(&self.master_password);
    }

    fn refresh_active_index(&mut self) {
        self.active_index = self.saved_accounts.iter().position(|a| a.token == self.token_input);
    }

    fn switch_account(&mut self, token: String) {
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
        self.login_new_token.clear();
        self.start_gateway(token.clone());
        self.add_saved_account(&token, "");
        self.push_debug(format!("Switched account to {}", self.mask_token(&token)));
    }

    fn display_name(&self, msg: &ChatMessage) -> String {
        msg.nickname.clone().unwrap_or_else(|| msg.author_name.clone())
    }

    fn display_content(&self, msg: &ChatMessage) -> String {
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

    fn short_time(&self, iso: &str) -> String {
        let t = iso.trim_start_matches('T');
        if t.len() >= 16 {
            t[11..16].to_string()
        } else {
            iso.to_string()
        }
    }

    fn guild_channels(&self, guild_id: &str) -> Vec<&ChatChannel> {
        self.channels.iter()
            .filter(|ch| ch.guild_id.as_deref() == Some(guild_id) && ch.channel_type == 0)
            .collect()
    }

    fn current_channel_messages(&self) -> Vec<ChatMessage> {
        self.selected_channel
            .and_then(|i| self.channels.get(i))
            .map(|ch| ch.id.clone())
            .and_then(|id| self.messages.get(&id))
            .cloned()
            .unwrap_or_default()
            .into()
    }

    fn download_avatar(&mut self, ctx: &egui::Context, user_id: &str, avatar_hash: &str) -> Option<TextureHandle> {
        let cache_key = format!("{}_{}", user_id, avatar_hash);
        if let Some(tex) = self.avatar_cache.get(&cache_key) {
            return Some(tex.clone());
        }

        let url = format!("{}/avatars/{}/{}.png?size=64", CDN_BASE, user_id, avatar_hash);
        let ctx2 = ctx.clone();
        let key = cache_key.clone();

        let pending = self.pending_avatars.entry(cache_key.clone()).or_insert_with(|| {
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let url_moved = url.clone();
            std::thread::spawn(move || {
                if AVATAR_DOWNLOADS_IN_FLIGHT.fetch_add(1, Ordering::SeqCst) >= MAX_CONCURRENT_AVATAR_DOWNLOADS {
                    AVATAR_DOWNLOADS_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
                    let _ = result_tx.send(None);
                    return;
                }
                let client = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(10))
                    .build()
                    .unwrap();
                if let Ok(resp) = client.get(&url_moved).send() {
                    if let Ok(bytes) = resp.bytes() {
                        if let Ok(img) = image::load_from_memory(&bytes) {
                            let rgba = img.to_rgba8();
                            let (w, h) = rgba.dimensions();
                            let pixels = rgba.into_raw();
                            let color_image = egui::ColorImage::from_rgba_unmultiplied(
                                [w as usize, h as usize],
                                &pixels,
                            );
                            let _ = result_tx.send(Some(color_image));
                            AVATAR_DOWNLOADS_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
                            return;
                        }
                    }
                }
                AVATAR_DOWNLOADS_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
                let _ = result_tx.send(None);
            });
            result_rx
        });

        if let Ok(result) = pending.try_recv() {
            self.pending_avatars.remove(&key);
            if let Some(color_image) = result {
                let handle = ctx2.load_texture(&key, color_image, egui::TextureOptions::default());
                self.avatar_cache.insert(key.clone(), handle.clone());
                ctx2.request_repaint();
                return Some(handle);
            }
        }

        None
    }

    fn download_guild_icon(&mut self, ctx: &egui::Context, guild_id: &str, icon_hash: &str) -> Option<TextureHandle> {
        let cache_key = format!("guild_icon_{}_{}", guild_id, icon_hash);
        if let Some(tex) = self.avatar_cache.get(&cache_key) {
            return Some(tex.clone());
        }

        let url = format!("{}/icons/{}/{}.png?size=64", CDN_BASE, guild_id, icon_hash);
        let ctx2 = ctx.clone();
        let key = cache_key.clone();

        let pending = self.pending_avatars.entry(cache_key.clone()).or_insert_with(|| {
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let url_moved = url.clone();
            std::thread::spawn(move || {
                if AVATAR_DOWNLOADS_IN_FLIGHT.fetch_add(1, Ordering::SeqCst) >= MAX_CONCURRENT_AVATAR_DOWNLOADS {
                    AVATAR_DOWNLOADS_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
                    let _ = result_tx.send(None);
                    return;
                }
                let client = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(10))
                    .build()
                    .unwrap();
                if let Ok(resp) = client.get(&url_moved).send() {
                    if let Ok(bytes) = resp.bytes() {
                        if let Ok(img) = image::load_from_memory(&bytes) {
                            let rgba = img.to_rgba8();
                            let (w, h) = rgba.dimensions();
                            let pixels = rgba.into_raw();
                            let color_image = egui::ColorImage::from_rgba_unmultiplied(
                                [w as usize, h as usize],
                                &pixels,
                            );
                            let _ = result_tx.send(Some(color_image));
                            AVATAR_DOWNLOADS_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
                            return;
                        }
                    }
                }
                AVATAR_DOWNLOADS_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
                let _ = result_tx.send(None);
            });
            result_rx
        });

        if let Ok(result) = pending.try_recv() {
            self.pending_avatars.remove(&key);
            if let Some(color_image) = result {
                let handle = ctx2.load_texture(&key, color_image, egui::TextureOptions::default());
                self.avatar_cache.insert(key.clone(), handle.clone());
                ctx2.request_repaint();
                return Some(handle);
            }
        }

        None
    }

    fn decode_image_payload(bytes: &[u8]) -> Option<ImagePayload> {
    if bytes.len() < 6 {
        return None;
    }
    let is_gif = &bytes[..6] == b"GIF89a" || &bytes[..6] == b"GIF87a";
    if is_gif {
            if let Ok(decoder) = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)) {
                use image::AnimationDecoder;
                if let Ok(frames) = decoder.into_frames().collect_frames() {
                if frames.len() > 1 {
                    let mut out = Vec::new();
                    for fr in frames {
                        let (num, den) = fr.delay().numer_denom_ms();
                        let secs = if den == 0 {
                            0.1
                        } else {
                            (num as f64 / den as f64) / 1000.0
                        };
                        let buf = fr.into_buffer();
                        let (w, h) = buf.dimensions();
                        if w == 0 || h == 0 {
                            return None;
                        }
                        let pixels = buf.into_raw();
                        let ci = egui::ColorImage::from_rgba_unmultiplied(
                            [w as usize, h as usize],
                            &pixels,
                        );
                        out.push((ci, secs.max(0.02) as f32));
                    }
                    return Some(ImagePayload::Animated { frames: out });
                }
            }
        }
    }
    if let Ok(img) = image::load_from_memory(bytes) {
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        if w == 0 || h == 0 {
            return None;
        }
        let pixels = rgba.into_raw();
        let color_image = egui::ColorImage::from_rgba_unmultiplied(
            [w as usize, h as usize],
            &pixels,
        );
        return Some(ImagePayload::Static(color_image));
    }
    None
}

fn download_image(&mut self, ctx: &egui::Context, url: &str) -> Option<LoadedImage> {
    let cache_key = url.to_string();
    if let Some(img) = self.image_cache.get(&cache_key) {
        return Some(img.clone());
    }
    if self.failed_images.contains(&cache_key) {
        return None;
    }

    let ctx2 = ctx.clone();
    let key = cache_key.clone();

    let lower = url.to_lowercase();
    if lower.contains("/avatars/") || lower.contains("/users/") {
        self.failed_images.insert(key);
        return None;
    }

    let pending = self.pending_images.entry(cache_key.clone()).or_insert_with(|| {
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let url_moved = url.to_string();
        std::thread::spawn(move || {
            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .unwrap();
            if let Ok(resp) = client.get(&url_moved).send() {
                if let Ok(bytes) = resp.bytes() {
                    if let Some(payload) = Self::decode_image_payload(&bytes) {
                        let _ = result_tx.send(Some(payload));
                        return;
                    }
                }
            }
            let _ = result_tx.send(None);
        });
        result_rx
    });

    if let Ok(result) = pending.try_recv() {
        self.pending_images.remove(&key);
        match result {
            Some(ImagePayload::Static(color_image)) => {
                let handle = ctx2.load_texture(&key, color_image, egui::TextureOptions::LINEAR);
                let loaded = LoadedImage::Static(handle);
                self.image_cache.insert(key.clone(), loaded.clone());
                ctx2.request_repaint();
                return Some(loaded);
            }
            Some(ImagePayload::Animated { frames }) => {
                let mut handles = Vec::with_capacity(frames.len());
                let mut delays = Vec::with_capacity(frames.len());
                for (i, (ci, delay)) in frames.into_iter().enumerate() {
                    let tkey = format!("{}#f{}", key, i);
                    handles.push(ctx2.load_texture(&tkey, ci, egui::TextureOptions::LINEAR));
                    delays.push(delay);
                }
                if handles.len() > 1 {
                    let loaded = LoadedImage::Animated {
                        frames: handles,
                        delays,
                        started: std::time::Instant::now(),
                    };
                    self.image_cache.insert(key.clone(), loaded.clone());
                    ctx2.request_repaint();
                    return Some(loaded);
                }
            }
            None => {
                self.failed_images.insert(key);
            }
        }
    }

    None
}

    fn extract_embed_image_url(&self, url: &str) -> Option<String> {
        let lower = url.to_lowercase();
        if lower.is_empty() {
            return None;
        }
        if lower.contains("/avatars/") || lower.contains("/users/") {
            return None;
        }
        if lower.ends_with(".mp4") || lower.ends_with(".webm") || lower.ends_with(".ogg") || lower.ends_with(".m4v") || lower.ends_with(".mov") {
            return None;
        }
        Some(url.to_string())
    }

    fn draw_attachments(&mut self, ui: &mut egui::Ui, msg: &ChatMessage) {
        let mut urls: Vec<String> = Vec::new();
        for att in &msg.attachments {
            if att.content_type.as_deref().map(|ct| ct.starts_with("image/")).unwrap_or(false) {
                urls.push(att.url.clone());
            }
        }
        for e in &msg.embeds {
            for field in ["image", "thumbnail", "video"] {
                if let Some(u) = e[field]["url"].as_str() {
                    if let Some(u2) = self.extract_embed_image_url(u) {
                        urls.push(u2);
                    }
                }
            }
        }
        for url in urls {
            if let Some(tex) = self.download_image(ui.ctx(), &url) {
                let size = tex.size_vec2();
                if size.x <= 0.0 || size.y <= 0.0 {
                    continue;
                }
                let max_w = 360.0_f32;
                let max_h = 360.0_f32;
                let scale = (max_w / size.x).min(max_h / size.y).min(1.0);
                let disp = egui::vec2(size.x * scale, size.y * scale);
                ui.add(egui::Image::new(egui::load::SizedTexture::new(tex.id(), disp)))
                    .on_hover_text(url.clone());
            } else {
                ui.spinner();
            }
        }
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

impl App {
    fn draw_login(&mut self, ctx: &egui::Context) {
        let style = (*ctx.style()).clone();
        let mut style = style;
        style.visuals.panel_fill = self.theme.channel_bg;
        style.visuals.widgets.inactive.bg_fill = self.theme.input_bg;
        style.visuals.widgets.hovered.bg_fill = self.theme.message_hover;
        ctx.set_style(style);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(100.0);
                ui.heading(RichText::new("Wyvern").size(32.0).color(self.theme.accent));
                ui.add_space(8.0);
                ui.label(RichText::new("Rust-powered, lightweight")
                    .size(14.0).color(self.theme.text_secondary));
                ui.add_space(32.0);
                ui.label(RichText::new("Session Token").color(self.theme.text));
                ui.add_space(6.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.token_input)
                        .password(true)
                        .desired_width(380.0)
                        .hint_text("Paste your token...")
                        .font(egui::FontId::proportional(16.0)),
                );
                ui.add_space(16.0);
                let btn = ui.add_sized(
                    [380.0, 42.0],
                    egui::Button::new(RichText::new("Login").size(16.0).color(Color32::BLACK))
                        .fill(self.theme.accent),
                );
                if btn.clicked() && !self.token_input.is_empty() {
                    let t = self.token_input.trim().to_string();
                    self.start_gateway(t.clone());
                    if self.accounts_unlocked {
                        self.add_saved_account(&t, "");
                    }
                }
                ui.add_space(10.0);
                ui.label(RichText::new("Master password (хранит аккаунты в шифрованном виде)").color(self.theme.text_secondary).size(12.0));
                ui.add_space(6.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.master_password)
                        .password(true)
                        .desired_width(380.0)
                        .hint_text("Password to unlock saved accounts...")
                        .font(egui::FontId::proportional(16.0)),
                );
                let unlock_btn = ui.add_sized(
                    [380.0, 36.0],
                    egui::Button::new(RichText::new(if self.accounts_unlocked { "Vault unlocked" } else { "Unlock / Create vault" }).size(14.0).color(Color32::BLACK))
                        .fill(if self.accounts_unlocked { self.theme.accent } else { self.theme.text_secondary }),
                );
                if unlock_btn.clicked() && !self.master_password.is_empty() {
                    let pw = self.master_password.clone();
                    self.saved_accounts = Self::load_accounts(&pw);
                    self.refresh_active_index();
                    self.accounts_unlocked = true;
                }
                ui.add_space(12.0);
                if !self.status.is_empty() {
                    ui.label(RichText::new(&self.status).color(Color32::from_rgb(250, 77, 77)).size(13.0));
                }
                if !self.debug_log.is_empty() {
                    ui.add_space(8.0);
                    ui.label(RichText::new(format!("Last: {}", self.debug_log.last().unwrap()))
                        .color(self.theme.text_secondary).size(11.0));
                }

                if !self.saved_accounts.is_empty() {
                    ui.add_space(30.0);
                    ui.label(RichText::new("Saved accounts:").strong().size(13.0).color(self.theme.text_secondary));
                    ui.add_space(6.0);
                    let mut switch_to: Option<String> = None;
                    let mut remove_idx: Option<usize> = None;
                    for (i, acc) in self.saved_accounts.iter().enumerate() {
                        ui.horizontal(|ui| {
                            let label = if acc.username.is_empty() {
                                self.mask_token(&acc.token)
                            } else {
                                acc.username.clone()
                            };
                            let is_active = self.active_index == Some(i);
                            let txt = if is_active { format!("{} (active)", label) } else { label };
                            let b = ui.add_sized(
                                [320.0, 34.0],
                                egui::Button::new(RichText::new(txt).size(13.0)
                                    .color(if is_active { Color32::BLACK } else { self.theme.text }))
                                    .fill(if is_active { self.theme.accent } else { self.theme.input_bg })
                                    .rounding(6.0),
                            );
                            if b.clicked() && !is_active {
                                switch_to = Some(acc.token.clone());
                            }
                            let x = ui.add_sized(
                                [28.0, 34.0],
                                egui::Button::new(RichText::new("✕").size(13.0).color(self.theme.text_secondary))
                                    .fill(self.theme.input_bg)
                                    .rounding(6.0),
                            );
                            if x.clicked() {
                                remove_idx = Some(i);
                            }
                        });
                        ui.add_space(4.0);
                    }
                    if let Some(t) = switch_to {
                        self.switch_account(t);
                    }
                    if let Some(i) = remove_idx {
                        self.saved_accounts.remove(i);
                        self.refresh_active_index();
                        self.save_accounts(&self.master_password);
                    }
                }

                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Add token:").size(12.0).color(self.theme.text_secondary));
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut self.login_new_token)
                            .password(true)
                            .desired_width(280.0)
                            .hint_text("Paste token to save...")
                            .font(egui::FontId::proportional(13.0)),
                    );
                    let save_btn = ui.add_sized(
                        [90.0, 28.0],
                        egui::Button::new(RichText::new("Save").size(13.0).color(Color32::BLACK))
                            .fill(self.theme.accent)
                            .rounding(6.0),
                    );
                    let entered = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (save_btn.clicked() || entered) && !self.login_new_token.trim().is_empty() {
                        let t = self.login_new_token.trim().to_string();
                        self.add_saved_account(&t, "");
                        self.login_new_token.clear();
                    }
                });
            });
        });
    }

    fn draw_chat(&mut self, ctx: &egui::Context) {
        let mut style = (*ctx.style()).clone();
        style.visuals.panel_fill = self.theme.panel_bg;
        style.visuals.window_fill = self.theme.panel_bg;
        style.visuals.widgets.noninteractive.bg_fill = self.theme.channel_bg;
        style.visuals.widgets.inactive.bg_fill = self.theme.input_bg;
        style.visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, self.theme.text);
        style.visuals.widgets.hovered.bg_fill = self.theme.message_hover;
        style.visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, self.theme.accent);
        style.visuals.widgets.active.bg_fill = self.theme.accent;
        style.visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, Color32::BLACK);
        style.visuals.selection.bg_fill = self.theme.accent;
        style.visuals.selection.stroke = egui::Stroke::new(1.0, Color32::BLACK);
        style.visuals.hyperlink_color = self.theme.accent;
        style.spacing.item_spacing = egui::vec2(8.0, 2.0);
        style.spacing.button_padding = egui::vec2(10.0, 4.0);
        ctx.set_style(style);

        self.draw_server_list(ctx);
        let r1 = ctx.available_rect();
        self.draw_channel_list(ctx);
        let r2 = ctx.available_rect();
        self.draw_input_bar(ctx);
        let r3 = ctx.available_rect();
        self.draw_main_chat(ctx);
        let r4 = ctx.available_rect();
        if self.debug_frames % 15 == 0 {
            self.push_debug(format!("FRAME: screen={:.0}x{:.0} servers={:.0}x{:.0} channels={:.0}x{:.0} input={:.0}x{:.0} chat={:.0}x{:.0}",
                ctx.screen_rect().width(), ctx.screen_rect().height(),
                r1.width(), r1.height(),
                r2.width(), r2.height(),
                r3.width(), r3.height(),
                r4.width(), r4.height()));
        }
        self.debug_frames += 1;
    }

    fn draw_server_list(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("servers")
            .resizable(false)
            .default_width(72.0)
            .exact_width(72.0)
            .frame(egui::Frame::none().fill(self.theme.bg))
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    let home_btn = ui.add_sized(
                        [48.0, 48.0],
                        egui::Button::new(RichText::new("@").size(20.0).color(Color32::WHITE))
                            .fill(if self.selected_guild.is_none() { self.theme.accent } else { self.theme.input_bg })
                            .rounding(24.0),
                    );
                    if home_btn.clicked() {
                        self.selected_guild = None;
                        self.selected_channel = None;
                        self.show_friends = !self.show_friends;
                    }
                });
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);

                egui::ScrollArea::vertical().show(ui, |ui| {
                    let guilds = self.guilds.clone();
                    for (i, guild) in guilds.iter().enumerate() {
                        ui.vertical_centered(|ui| {
                            let is_sel = self.selected_guild == Some(i);
                            let icon_tex = guild.icon.as_ref()
                                .and_then(|h| self.download_guild_icon(ctx, &guild.id, h));
                            let btn = if let Some(tex) = icon_tex {
                                let img = tex.clone();
                                ui.add_sized(
                                    [48.0, 48.0],
                                    egui::ImageButton::new(
                                        egui::load::SizedTexture::new(img.id(), egui::vec2(48.0, 48.0))
                                    ).rounding(24.0),
                                )
                            } else {
                                let label: String = guild.name.chars().take(2).collect();
                                ui.add_sized(
                                    [48.0, 48.0],
                                    egui::Button::new(RichText::new(&label).size(16.0).color(Color32::WHITE))
                                        .fill(if is_sel { self.theme.accent } else { self.theme.input_bg })
                                        .rounding(24.0),
                                )
                            };
                            if btn.clicked() {
                                self.selected_guild = Some(i);
                                self.selected_channel = None;
                                self.show_friends = false;
                            }
                        });
                        ui.add_space(6.0);
                    }
                });

                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    let ubtn = ui.add_sized(
                        [48.0, 48.0],
                        egui::Button::new(RichText::new("◐").size(20.0).color(Color32::WHITE))
                            .fill(self.theme.input_bg)
                            .rounding(24.0),
                    );
                    if ubtn.clicked() {
                        self.theme_index = (self.theme_index + 1) % 3;
                    }
                    ubtn.on_hover_text("Toggle theme");

                    let abtn = ui.add_sized(
                        [48.0, 42.0],
                        egui::Button::new(RichText::new("👤").size(18.0).color(Color32::WHITE))
                            .fill(if self.connected { self.theme.accent } else { self.theme.input_bg })
                            .rounding(21.0),
                    );
                    let popup_id = egui::Id::new("account_switcher");
                    if abtn.clicked() {
                        ui.close_menu();
                        ui.memory_mut(|m| m.toggle_popup(popup_id));
                    }
                    abtn.clone().on_hover_text(&self.username);
                    egui::popup_below_widget(ui, popup_id, &abtn, egui::PopupCloseBehavior::CloseOnClickOutside, |ui| {
                        ui.set_min_width(190.0);
                        ui.label(RichText::new("Accounts").strong().size(12.0).color(self.theme.text_secondary));
                        ui.add_space(4.0);
                        ui.separator();
                        let mut switch_to: Option<String> = None;
                        let accounts = self.saved_accounts.clone();
                        for (i, acc) in accounts.iter().enumerate() {
                            let label = if acc.username.is_empty() {
                                self.mask_token(&acc.token)
                            } else {
                                acc.username.clone()
                            };
                            let is_active = self.active_index == Some(i);
                            let text = if is_active {
                                format!("{} ✓", label)
                            } else {
                                label
                            };
                            if ui.add(egui::Button::new(RichText::new(text).size(13.0)
                                .color(if is_active { Color32::BLACK } else { self.theme.text }))
                                .fill(if is_active { self.theme.accent } else { self.theme.input_bg })
                                .min_size(egui::vec2(180.0, 30.0))).clicked() && !is_active
                            {
                                switch_to = Some(acc.token.clone());
                            }
                        }
                        if let Some(t) = switch_to {
                            self.switch_account(t);
                            ui.close_menu();
                        }
                    });
                });
            });
    }

    fn draw_channel_list(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("channels")
            .resizable(true)
            .default_width(240.0)
            .min_width(180.0)
            .frame(egui::Frame::none().fill(self.theme.panel_bg))
            .show(ctx, |ui| {
                ui.set_min_width(ui.available_width());

                match self.selected_guild {
                    Some(guild_idx) => {
                        let Some(guild) = self.guilds.get(guild_idx).cloned() else {
                            self.selected_guild = None;
                            return;
                        };
                        ui.add_space(12.0);
                        ui.label(RichText::new(&guild.name).strong().size(15.0).color(self.theme.text));
                        ui.add_space(4.0);
                        ui.separator();
                        ui.add_space(4.0);

                        let chs: Vec<ChatChannel> = self.guild_channels(&guild.id).into_iter().cloned().collect();
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            for ch in &chs {
                                let is_sel = self.selected_channel
                                    .and_then(|i| self.channels.get(i))
                                    .map(|c| c.id == ch.id)
                                    .unwrap_or(false);
                                let label = format!("# {}", ch.name);
                                let response = ui.add_sized(
                                    [ui.available_width(), 32.0],
                                    egui::Button::new(
                                        RichText::new(&label)
                                            .color(if is_sel { Color32::WHITE } else { self.theme.text_secondary })
                                            .size(14.0)
                                    ).fill(if is_sel { self.theme.accent } else { Color32::TRANSPARENT })
                                );
                                if response.clicked() {
                                    let idx = self.channels.iter().position(|c| c.id == ch.id);
                                    self.selected_channel = idx;
                                    self.scroll_to_bottom = true;
                                    self.send_cmd(ToGateway::FetchHistory { channel_id: ch.id.clone() });
                                    self.history_loading = Some(ch.id.clone());
                                }
                            }
                        });
                    }
                    None => {
                        if self.show_friends {
                            ui.add_space(12.0);
                            ui.label(RichText::new("Friends").strong().size(15.0).color(self.theme.text));
                            ui.add_space(8.0);
                            ui.label(RichText::new(format!("{} friends", self.friends.len()))
                                .size(12.0).color(self.theme.text_secondary));
                            ui.add_space(4.0);
                            ui.separator();
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                let users: Vec<UserProfile> = self.friends.clone();
                                for f in &users {
                                    let label = format!("@{}", f.username);
                                    let fid = f.id.clone();
                                    let response = ui.add_sized(
                                        [ui.available_width(), 32.0],
                                        egui::Button::new(
                                            RichText::new(&label).size(14.0).color(self.theme.text)
                                        ).fill(Color32::TRANSPARENT),
                                    );
                                    if response.clicked() && !fid.is_empty() {
                                        self.push_debug(format!("Clicked friend '{}' (id {})", f.username, &fid[..fid.len().min(12)]));
                                        self.send_cmd(ToGateway::OpenDM { user_id: fid });
                                    }
                                }
                            });
                        } else {
                            ui.add_space(12.0);
                            ui.label(RichText::new("DMs").strong().size(15.0).color(self.theme.text));
                            ui.add_space(4.0);
                            ui.separator();
                            let dms: Vec<ChatChannel> = self.channels.iter()
                                .filter(|ch| ch.guild_id.is_none() && (ch.channel_type == 1 || ch.channel_type == 3))
                                .cloned()
                                .collect();
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                for ch in &dms {
                                    let is_sel = self.selected_channel
                                        .and_then(|i| self.channels.get(i))
                                        .map(|c| c.id == ch.id)
                                        .unwrap_or(false);
                                    let response = ui.add_sized(
                                        [ui.available_width(), 32.0],
                                        egui::Button::new(
                                            RichText::new(&ch.name)
                                                .color(if is_sel { Color32::WHITE } else { self.theme.text_secondary })
                                                .size(14.0)
                                        ).fill(if is_sel { self.theme.accent } else { Color32::TRANSPARENT })
                                    );
                                    if response.clicked() {
                                        let idx = self.channels.iter().position(|c| c.id == ch.id);
                                        self.selected_channel = idx;
                                        self.scroll_to_bottom = true;
                                        self.send_cmd(ToGateway::FetchHistory { channel_id: ch.id.clone() });
                                    }
                                }
                            });
                        }
                    }
                }
            });
    }

    fn draw_input_bar(&mut self, ctx: &egui::Context) {
        let has_channel = self.selected_channel.is_some();
        let ch_name = self.selected_channel
            .and_then(|i| self.channels.get(i))
            .map(|c| format!("# {}", c.name))
            .unwrap_or_else(|| "No channel selected".into());

        let input_resp = egui::TopBottomPanel::bottom("input_panel")
            .min_height(56.0)
            .show(ctx, |ui| {
                let ir = ui.min_rect();
                self.push_debug(format!("INPUT_BAR: h={:.0} y={:.0}", ir.height(), ir.min.y));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    ui.label(RichText::new(ch_name).strong().size(13.0).color(self.theme.text_secondary));
                    ui.add_space(8.0);

                    if !has_channel {
                        ui.label(RichText::new("← select a channel on the left").italics()
                            .size(12.0).color(self.theme.text_secondary));
                    } else {
                        let resp = ui.add_sized(
                            [ui.available_width() - 110.0, 36.0],
                            egui::TextEdit::singleline(&mut self.input)
                                .hint_text("Type a message and press Enter, or click Send...")
                                .margin(egui::Margin::symmetric(12, 8)),
                        );
                        let send_btn = ui.add_sized(
                            [84.0, 36.0],
                            egui::Button::new(RichText::new("Send").size(14.0).color(Color32::WHITE))
                                .fill(self.theme.accent),
                        );

                        let enter_pressed = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        let send_clicked = send_btn.clicked();

                        if (enter_pressed || send_clicked) {
                            let text = self.input.trim().to_string();
                            if !text.is_empty() {
                                self.handle_input(&text);
                                self.input.clear();
                            }
                            resp.request_focus();
                        }
                    }
                });
                ui.add_space(6.0);
            });
    }

    fn draw_main_chat(&mut self, ctx: &egui::Context) {
        let msgs = self.current_channel_messages();

        for msg in &msgs {
            if let Some(avatar_hash) = &msg.author_avatar {
                let _ = self.download_avatar(ctx, &msg.author_id, avatar_hash);
            }
        }

        let channel_label = self.selected_channel
            .and_then(|i| self.channels.get(i))
            .map(|c| {
                if let Some(ref topic) = c.topic {
                    format!("{} — {}", c.name, topic)
                } else {
                    format!("# {}", c.name)
                }
            });

        let sel_cid = self.selected_channel
            .and_then(|i| self.channels.get(i))
            .map(|c| c.id.clone())
            .unwrap_or_else(|| "none".into());
        let key = format!("{}|{}", sel_cid, msgs.len());
        if key != self.last_render_key {
            self.last_render_key = key;
            let n_channels = self.channels.len();
            let srect = ctx.screen_rect();
            self.push_debug(format!("RENDER: sel='{}' sel_cid='{}' msgs={} total_channels={} screen={}x{}", channel_label.clone().unwrap_or_else(|| "none".into()), sel_cid, msgs.len(), n_channels, srect.width().round() as i32, srect.height().round() as i32));
        }

        match channel_label {
            Some(label) => {
                let msg_count = msgs.len();
                let stored_count = self.selected_channel
                    .and_then(|i| self.channels.get(i))
                    .map(|c| self.messages.get(&c.id).map_or(0, |v| v.len()))
                    .unwrap_or(0);
                let sel_id = self.selected_channel
                    .and_then(|i| self.channels.get(i))
                    .map(|c| c.id.clone())
                    .unwrap_or_else(|| "none".into());
                let stick = self.scroll_to_bottom;
                let panel_resp = egui::CentralPanel::default()
                    .frame(egui::Frame::none().fill(self.theme.channel_bg))
                    .show(ctx, |ui| {
                        ui.set_min_width(0.0);
                        let avail_w = ui.available_width();
                        let avail_h = ui.available_height();
                        ui.horizontal(|ui| {
                            ui.add_space(12.0);
                            ui.label(RichText::new("CHAT-AREA").strong().size(18.0).color(Color32::from_rgb(0, 255, 0)));
                            ui.label(RichText::new(format!("w={:.0} h={:.0}", avail_w, avail_h))
                                .size(12.0).color(Color32::from_rgb(255, 60, 60)));
                            ui.add_space(8.0);
                            ui.label(RichText::new(&label).strong().size(14.0).color(self.theme.text));
                            ui.add_space(8.0);
                            ui.label(RichText::new(format!("({} msgs / stored {})", msg_count, stored_count))
                                .size(12.0).color(self.theme.text_secondary));
                            ui.add_space(6.0);
                        });

                        ui.separator();

                        if self.history_loading.is_some() {
                            ui.horizontal_centered(|ui| {
                                ui.spinner();
                                ui.label(RichText::new("Loading messages...").color(self.theme.text_secondary).size(12.0));
                            });
                        }

                        let msgs_for_render = msgs.clone();
                        let scroll_out = egui::ScrollArea::vertical()
                            .id_salt(("chat", &sel_id))
                            .auto_shrink([false, false])
                            .stick_to_bottom(stick)
                            .show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                ui.add_space(8.0);
                                if msgs_for_render.is_empty() && self.history_loading.is_none() {
                                    ui.vertical_centered(|ui| {
                                        ui.add_space(60.0);
                                        ui.label(RichText::new("No messages here yet")
                                            .size(16.0).color(self.theme.text_secondary));
                                        ui.add_space(4.0);
                                        ui.label(RichText::new("Be the first to say something above")
                                            .size(12.0).color(self.theme.text_secondary));
                                    });
                                } else {
                                    for msg in &msgs_for_render {
                                        let display = self.display_name(msg);
                                        let is_own = msg.is_own || (!self.user_id.is_empty() && msg.author_id == self.user_id);
                                        if is_own {
                                            let max_w = (ui.available_width() * 0.75).clamp(160.0, 480.0);
                                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                                                egui::Frame::none()
                                                    .fill(self.theme.self_bg)
                                                    .stroke(egui::Stroke::new(1.0, self.theme.divider))
                                                    .rounding(8.0)
                                                    .inner_margin(egui::Margin::symmetric(10, 8))
                                                    .show(ui, |ui| {
                                                    ui.set_max_width(max_w);
                                                    ui.vertical(|ui| {
                                                        ui.horizontal(|ui| {
                                                            ui.label(RichText::new(&display).strong().size(14.0).color(self.theme.accent));
                                                            if !msg.timestamp.is_empty() {
                                                                let t = self.short_time(&msg.timestamp);
                                                                ui.label(RichText::new(t).size(11.0).color(self.theme.text_secondary));
                                                            }
                                                        });
                                                        let content = self.display_content(msg);
                                                        let content = if content.is_empty() { "* (message)".to_string() } else { content };
                                                        ui.label(RichText::new(content).size(14.0).color(self.theme.text));
                                                        self.draw_attachments(ui, msg);
                                                    });
                                                });
                                            });
                                            ui.add_space(6.0);
                                        } else {
                                            egui::Frame::none()
                                                .fill(self.theme.message_hover)
                                                .stroke(egui::Stroke::new(1.0, self.theme.divider))
                                                .rounding(8.0)
                                                .inner_margin(egui::Margin::symmetric(10, 8))
                                                .show(ui, |ui| {
                                                ui.set_min_height(44.0);
                                                ui.horizontal(|ui| {
                                                    let size = 36.0;
                                                    let avatar_tex = msg.author_avatar.as_ref()
                                                        .and_then(|h| self.avatar_cache.get(&format!("{}_{}", msg.author_id, h)).cloned())
                                                        .or_else(|| {
                                                            if let Some(h) = &msg.author_avatar {
                                                                self.download_avatar(ui.ctx(), &msg.author_id, h)
                                                            } else { None }
                                                        });
                                                    if let Some(tex) = avatar_tex {
                                                        ui.add_sized(
                                                            egui::vec2(size, size),
                                                            egui::Image::new(
                                                                egui::load::SizedTexture::new(tex.id(), egui::vec2(size, size))
                                                            ).rounding(size / 2.0),
                                                        );
                                                    } else {
                                                        let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
                                                        ui.painter().circle_filled(rect.center(), size / 2.0, self.theme.accent);
                                                        let initial = msg.author_name.chars().next().unwrap_or('?').to_uppercase().to_string();
                                                        ui.painter().text(
                                                            rect.center(), egui::Align2::CENTER_CENTER, &initial,
                                                            egui::FontId::proportional(16.0), Color32::WHITE,
                                                        );
                                                    }
                                                    ui.vertical(|ui| {
                                                        ui.horizontal(|ui| {
                                                            ui.label(RichText::new(&display).strong().size(14.0).color(self.theme.accent));
                                                            if !msg.timestamp.is_empty() {
                                                                let t = self.short_time(&msg.timestamp);
                                                                ui.label(RichText::new(t).size(11.0).color(self.theme.text_secondary));
                                                            }
                                                        });
                                                        let content = self.display_content(msg);
                                                        let content = if content.is_empty() { "* (message)".to_string() } else { content };
                                                        ui.label(RichText::new(content).size(14.0).color(self.theme.text));
                                                        self.draw_attachments(ui, msg);
                                                    });
                                                });
                                                });
                                                ui.add_space(6.0);
                                        }
                                    }
                                }
                                ui.add_space(8.0);
                            });
                        let inner_h = scroll_out.inner_rect.height();
                        let content_h = scroll_out.content_size.y;
                        let offset_y = scroll_out.state.offset.y;
                        self.push_debug(format!("SCROLL: inner_h={:.0} content_h={:.0} offset_y={:.0} stick={}", inner_h, content_h, offset_y, stick));

                        if self.scroll_to_bottom {
                            if content_h > inner_h {
                                let mut st = scroll_out.state;
                                st.offset.y = content_h - inner_h;
                                self.last_scroll_offset_y = st.offset.y;
                                st.store(ui.ctx(), scroll_out.id);
                            }
                            self.scroll_to_bottom = false;
                        }
                        self.last_scroll_offset_y = self.last_scroll_offset_y.max(offset_y);
                    });
                let presp = panel_resp.response.rect;
                let p_rect = presp.min;
                let srect = ctx.screen_rect();
                self.push_debug(format!("PANEL: rect={:.0}x{:.0}+({:.0},{:.0}) screen={:.0}x{:.0}", presp.width(), presp.height(), p_rect.x, p_rect.y, srect.width(), srect.height()));
            }
            None => {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(120.0);
                        ui.label(RichText::new("Welcome to Wyvern").size(24.0).color(self.theme.text));
                        ui.add_space(8.0);
                        ui.label(RichText::new("1. Click a server icon on the far left")
                            .size(14.0).color(self.theme.text_secondary));
                        ui.label(RichText::new("2. Click a channel (# name) in the middle list")
                            .size(14.0).color(self.theme.text_secondary));
                        ui.label(RichText::new("3. Type a message at the bottom and press Enter")
                            .size(14.0).color(self.theme.text_secondary));
                    });
                });
            }
        }
    }

    fn handle_input(&mut self, text: &str) {
        if text == "/quit" {
            std::process::exit(0);
        }
        if let Some(id) = text.strip_prefix("/add ") {
            let id = id.trim().to_string();
            if !id.is_empty() {
                self.channels.push(ChatChannel {
                    id: id.clone(),
                    name: format!("#{}", &id),
                    guild_id: None,
                    channel_type: 0,
                    topic: None,
                    position: 999,
                });
                let idx = self.channels.len() - 1;
                self.selected_channel = Some(idx);
                self.send_cmd(ToGateway::FetchHistory { channel_id: id });
            }
            return;
        }

        if let Some(idx) = self.selected_channel {
            let cid = self.channels[idx].id.clone();
            self.messages.entry(cid.clone()).or_default().push(ChatMessage {
                id: String::new(),
                channel_id: cid.clone(),
                author_id: self.user_id.clone(),
                author_name: self.username.clone(),
                author_avatar: self.user_avatar.clone(),
                nickname: None,
                content: text.to_string(),
                timestamp: String::new(),
                attachments: Vec::new(),
                embeds: Vec::new(),
                is_own: true,
            });
            self.send_cmd(ToGateway::Send { channel_id: cid, content: text.to_string() });
        }
    }
}

async fn run_gateway(
    mut cmd_rx: mpsc::UnboundedReceiver<ToGateway>,
    event_tx: mpsc::UnboundedSender<ToApp>,
    token: String,
) {
    let _ = event_tx.send(ToApp::Debug("Gateway thread started".into()));
    let mut session = SessionState::default();
    loop {
        let use_resume = session.session_id.is_some();
        match gw_inner(&mut cmd_rx, event_tx.clone(), &token, &mut session, use_resume).await {
            Ok(()) => {
                let _ = event_tx.send(ToApp::Debug("Gateway disconnected cleanly".into()));
                let _ = event_tx.send(ToApp::Status("Disconnected".into()));
                break;
            }
            Err(e) => {
                let _ = event_tx.send(ToApp::Debug(format!("Gateway error: {}", e)));
                let _ = event_tx.send(ToApp::Status(format!("Reconnecting: {}", e)));
                time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

#[derive(Default)]
struct SessionState {
    session_id: Option<String>,
    seq: Option<i64>,
}

async fn gw_inner(
    cmd_rx: &mut mpsc::UnboundedReceiver<ToGateway>,
    event_tx: mpsc::UnboundedSender<ToApp>,
    token: &str,
    session: &mut SessionState,
    use_resume: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let _ = event_tx.send(ToApp::Status("Connecting...".into()));
    if use_resume {
        let _ = event_tx.send(ToApp::Debug("Connecting to gateway (resume)...".into()));
    } else {
        let _ = event_tx.send(ToApp::Debug("Connecting to gateway...".into()));
    }

    let (ws_stream, _) = connect_async(GATEWAY_URL).await?;
    let (mut write, mut read) = ws_stream.split();

    let hello = read.next().await.ok_or("no hello")??;
    let hello: Value = match hello {
        WsMessage::Text(t) => serde_json::from_str(&t)?,
        _ => return Err("unexpected first message".into()),
    };
    let op = hello["op"].as_i64().unwrap_or(-1);
    if op != 10 {
        return Err(format!("expected op 10 hello, got op {}", op).into());
    }
    let interval = hello["d"]["heartbeat_interval"].as_u64().ok_or("no heartbeat_interval")?;
    let _ = event_tx.send(ToApp::Debug(format!("Hello received, interval={}ms", interval)));

    let identify = json!({
        "op": 2,
        "d": {
            "token": token,
            "properties": {
                "os": "Linux",
                "browser": "Discord Client",
                "device": "",
                "release_channel": "stable",
                "client_build_number": 361909,
                "client_event_source": null
            },
            "intents": 327679,
            "presence": {
                "status": "online",
                "since": null,
                "activities": [],
                "afk": false
            }
        }
    });
    let first_payload = if use_resume {
        Some(json!({
            "op": 6,
            "d": {
                "token": token,
                "session_id": session.session_id.clone().unwrap_or_default(),
                "seq": session.seq,
            }
        }))
    } else {
        None
    };
    if let Some(payload) = &first_payload {
        write.send(WsMessage::Text(serde_json::to_string(payload)?.into())).await?;
        let _ = event_tx.send(ToApp::Debug("Resume sent".into()));
    } else {
        write.send(WsMessage::Text(serde_json::to_string(&identify)?.into())).await?;
        let _ = event_tx.send(ToApp::Debug("Identify sent".into()));
    }

    let (ws_tx, mut ws_rx) = mpsc::unbounded_channel::<WsMessage>();
    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<String>();

    tokio::spawn(async move {
        while let Some(msg) = read.next().await {
            match msg {
                Ok(WsMessage::Text(t)) => { let _ = raw_tx.send(t.to_string()); }
                Ok(WsMessage::Ping(d)) => { let _ = ws_tx.send(WsMessage::Pong(d)); }
                Ok(WsMessage::Close(c)) => {
                    let _ = raw_tx.send(format!("__CLOSE__{:?}", c));
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    let _ = raw_tx.send(format!("__WS_ERROR__{}", e));
                    break;
                }
            }
        }
    });

    let http = reqwest::Client::new();
    let tkn = token.to_string();
    let mut heartbeat = time::interval(Duration::from_millis(interval));
    heartbeat.tick().await;
    let mut seq: Option<i64> = session.seq;
    let mut heartbeat_failures = 0u32;

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                let p = json!({ "op": 1, "d": seq });
                if write.send(WsMessage::Text(serde_json::to_string(&p).unwrap().into())).await.is_err() {
                    let _ = event_tx.send(ToApp::Debug("WebSocket write failed during heartbeat".into()));
                    return Err("heartbeat write failed".into());
                }
                heartbeat_failures += 1;
                if heartbeat_failures > 5 {
                    let _ = event_tx.send(ToApp::Debug("Too many heartbeats without ACK".into()));
                    return Err("too many failed heartbeats".into());
                }
            }
            Some(ws_msg) = ws_rx.recv() => {
                let _ = write.send(ws_msg).await;
            }
            Some(raw) = raw_rx.recv() => {
                if raw.starts_with("__CLOSE__") {
                    let _ = event_tx.send(ToApp::Debug(format!("WebSocket closed: {}", &raw[8..])));
                    return Err("websocket closed".into());
                }
                if raw.starts_with("__WS_ERROR__") {
                    let _ = event_tx.send(ToApp::Debug(format!("WebSocket error: {}", &raw[11..])));
                    return Err("websocket error".into());
                }

                let v: Value = match serde_json::from_str(&raw) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                if let Some(s) = v["s"].as_i64() {
                    seq = Some(s);
                    session.seq = Some(s);
                }

                let op = v["op"].as_i64().unwrap_or(-1);
                match op {
                    0 => {
                        let t = v["t"].as_str().unwrap_or("");
                        match t {
                            "READY" => {
                                let d = &v["d"];
                                let u = d["user"]["username"].as_str().unwrap_or("?").to_string();
                                let uid = d["user"]["id"].as_str().unwrap_or("").to_string();
                                let avatar = d["user"]["avatar"].as_str().map(|s| s.to_string());
                                session.session_id = d["session_id"].as_str().map(|s| s.to_string());
                                session.seq = seq;
                                let _ = event_tx.send(ToApp::Ready { username: u, user_id: uid, avatar });

                                if let Some(guilds) = d["guilds"].as_array() {
                                    let _ = event_tx.send(ToApp::Debug(format!("READY: {} guilds", guilds.len())));
                                    for g in guilds {
                                        let gid = g["id"].as_str().unwrap_or("").to_string();
                                        if gid.is_empty() { continue; }
                                        let gname = g["name"].as_str().unwrap_or("Unknown").to_string();
                                        let gicon = g["icon"].as_str().map(|s| s.to_string());
                                        let gowner = g["owner_id"].as_str().unwrap_or("").to_string();
                                        let _ = event_tx.send(ToApp::Guild(Guild {
                                            id: gid.clone(),
                                            name: gname.clone(),
                                            icon: gicon,
                                            owner_id: gowner,
                                        }));
                                        let _ = event_tx.send(ToApp::Debug(format!("Will load channels for '{}'", &gname)));
                                    }

                                    let guilds = d["guilds"].as_array().cloned().unwrap_or_default();
                                    let egoods = event_tx.clone();
                                    let httpc = http.clone();
                                    let tkc = tkn.clone();
                                    tokio::spawn(async move {
                                        const CONCURRENCY: usize = 8;

                                        async fn fetch_one(
                                            httpc: reqwest::Client,
                                            tkc: String,
                                            egoods: mpsc::UnboundedSender<ToApp>,
                                            gid: String,
                                            gname: String,
                                        ) {
                                            for _attempt in 0..3 {
                                                let url = format!("{}/guilds/{}/channels", API_BASE, gid);
                                                let req = httpc.get(&url)
                                                    .header("Authorization", &*tkc)
                                                    .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36")
                                                    .header("X-Super-Properties", &super_props())
                                                    .header("X-Discord-Locale", "en-US")
                                                    .header("X-Discord-Timezone", "Europe/Moscow");
                                                match req.send().await {
                                                    Ok(resp) => {
                                                        let status = resp.status();
                                                        if status == 429 {
                                                            let retry = resp.headers()
                                                                .get("retry-after")
                                                                .and_then(|v| v.to_str().ok())
                                                                .and_then(|s| s.parse::<u64>().ok())
                                                                .unwrap_or(2);
                                                            let _ = egoods.send(ToApp::Debug(format!("429 for '{}', retrying in {}s", gname, retry)));
                                                            tokio::time::sleep(Duration::from_secs(retry)).await;
                                                            continue;
                                                        }
                                                        if status.is_success() {
                                                            match resp.text().await {
                                                                Ok(body) => {
                                                                    if let Ok(arr) = serde_json::from_str::<Vec<Value>>(&body) {
                                                                        let mut channels: Vec<ChatChannel> = Vec::new();
                                                                        for c in arr {
                                                                            let ctype = c["type"].as_i64().unwrap_or(0);
                                                                            if ctype == 0 || ctype == 11 || ctype == 12 {
                                                                                channels.push(ChatChannel {
                                                                                    id: c["id"].as_str().unwrap_or("").to_string(),
                                                                                    name: c["name"].as_str().unwrap_or("unknown").to_string(),
                                                                                    guild_id: Some(gid.clone()),
                                                                                    channel_type: ctype,
                                                                                    topic: c["topic"].as_str().map(|s| s.to_string()),
                                                                                    position: c["position"].as_i64().unwrap_or(0) as i32,
                                                                                });
                                                                            }
                                                                        }
                                                                        channels.sort_by_key(|c| c.position);
                                                                        let _ = egoods.send(ToApp::GuildChannels {
                                                                            guild_id: gid.clone(),
                                                                            channels,
                                                                        });
                                                                    }
                                                                }
                                                                Err(_) => {}
                                                            }
                                                            break;
                                                        } else if status == 403 {
                                                            let _ = egoods.send(ToApp::Debug(format!("No access to '{}', skipping", gname)));
                                                            break;
                                                        } else {
                                                            let _ = egoods.send(ToApp::Debug(format!("Guild '{}' channels error {}", gname, status)));
                                                            break;
                                                        }
                                                    }
                                                    Err(e) => {
                                                        let _ = egoods.send(ToApp::Debug(format!("Guild '{}' channels request error: {}", gname, e)));
                                                        break;
                                                    }
                                                }
                                            }
                                        }

                                        let mut guilds: Vec<(String, String)> = guilds.into_iter()
                                            .filter_map(|g| {
                                                let gid = g["id"].as_str().unwrap_or("").to_string();
                                                if gid.is_empty() { return None; }
                                                let gname = g["name"].as_str().unwrap_or("Unknown").to_string();
                                                Some((gid, gname))
                                            })
                                            .collect();

                                        while !guilds.is_empty() {
                                            let batch = guilds.drain(..guilds.len().min(CONCURRENCY)).collect::<Vec<_>>();
                                            let mut handles = Vec::new();
                                            for (gid, gname) in batch {
                                                handles.push(tokio::spawn(fetch_one(
                                                    httpc.clone(),
                                                    tkc.clone(),
                                                    egoods.clone(),
                                                    gid,
                                                    gname,
                                                )));
                                            }
                                            for h in handles {
                                                let _ = h.await;
                                            }
                                            tokio::time::sleep(Duration::from_millis(250)).await;
                                        }
                                    });
                                }

                                {
                                    let egoods = event_tx.clone();
                                    let httpc = http.clone();
                                    let tkc = tkn.clone();
                                    tokio::spawn(async move {
                                        let url = format!("{}/users/@me/relationships", API_BASE);
                                        let req = httpc.get(&url)
                                            .header("Authorization", &*tkc)
                                            .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36")
                                            .header("X-Super-Properties", &super_props())
                                            .header("X-Discord-Locale", "en-US")
                                            .header("X-Discord-Timezone", "Europe/Moscow");
                                        match req.send().await {
                                            Ok(resp) => {
                                                let status = resp.status();
                                                if status.is_success() {
                                                    if let Ok(body) = resp.text().await {
                                                        if let Ok(arr) = serde_json::from_str::<Vec<Value>>(&body) {
                                                            let mut friends: Vec<UserProfile> = Vec::new();
                                                            for r in arr {
                                                                if r["type"].as_i64().unwrap_or(0) != 1 { continue; }
                                                                let u = &r["user"];
                                                                friends.push(UserProfile {
                                                                    id: u["id"].as_str().unwrap_or("").to_string(),
                                                                    username: u["username"].as_str().unwrap_or("?").to_string(),
                                                                    avatar: u["avatar"].as_str().map(|s| s.to_string()),
                                                                    discriminator: u["discriminator"].as_str().unwrap_or("").to_string(),
                                                                });
                                                            }
                                                            let _ = egoods.send(ToApp::Friends(friends));
                                                        }
                                                    }
                                                } else {
                                                    let _ = egoods.send(ToApp::Debug(format!("Friends fetch status {}", status)));
                                                }
                                            }
                                            Err(e) => {
                                                let _ = egoods.send(ToApp::Debug(format!("Friends fetch error: {}", e)));
                                            }
                                        }
                                    });
                                }

                                let priv_channels = d["private_channels"].as_array()
                                    .cloned()
                                    .or_else(|| d["channels"].as_array().cloned())
                                    .unwrap_or_default();
                                let _ = event_tx.send(ToApp::Debug(format!("READY: {} private channels", priv_channels.len())));
                                for ch in &priv_channels {
                                    let ctype = ch["type"].as_i64().unwrap_or(0);
                                    if ctype == 1 || ctype == 3 {
                                        let name = ch["name"].as_str()
                                            .filter(|s| !s.is_empty())
                                            .map(|s| s.to_string())
                                            .or_else(|| {
                                                ch["recipients"].as_array().map(|r| {
                                                    r.iter()
                                                        .filter_map(|u| u["username"].as_str().map(|s| s.to_string()))
                                                        .collect::<Vec<_>>()
                                                        .join(", ")
                                                }).filter(|s| !s.is_empty())
                                            })
                                            .unwrap_or_else(|| "DM".to_string());
                                        let _ = event_tx.send(ToApp::Channel(ChatChannel {
                                            id: ch["id"].as_str().unwrap_or("").to_string(),
                                            name,
                                            guild_id: None,
                                            channel_type: ctype,
                                            topic: None,
                                            position: 0,
                                        }));
                                    }
                                }
                            }
                            "RESUMED" => {
                                let _ = event_tx.send(ToApp::Debug("Session resumed".into()));
                                let _ = event_tx.send(ToApp::Status("Resumed".into()));
                            }
                            "MESSAGE_CREATE" => {
                                let d = &v["d"];
                                let author = &d["author"];
                                let msg = ChatMessage {
                                    id: d["id"].as_str().unwrap_or("").to_string(),
                                    channel_id: d["channel_id"].as_str().unwrap_or("").to_string(),
                                    author_id: author["id"].as_str().unwrap_or("").to_string(),
                                    author_name: author["username"].as_str().unwrap_or("?").to_string(),
                                    author_avatar: author["avatar"].as_str().map(|s| s.to_string()),
                                    nickname: None,
                                    content: d["content"].as_str().unwrap_or("").to_string(),
                                    timestamp: d["timestamp"].as_str().unwrap_or("").to_string(),
                                    attachments: d["attachments"].as_array().map(|arr| {
                                        arr.iter().filter_map(|a| {
                                            Some(Attachment {
                                                filename: a["filename"].as_str()?.to_string(),
                                                url: a["url"].as_str()?.to_string(),
                                                content_type: a["content_type"].as_str().map(|s| s.to_string()),
                                                width: a["width"].as_u64().map(|v| v as u32),
                                                height: a["height"].as_u64().map(|v| v as u32),
                                                size: a["size"].as_u64().unwrap_or(0),
                                                description: a["description"].as_str().map(|s| s.to_string()),
                                            })
                                        }).collect()
                                    }).unwrap_or_default(),
                                    embeds: d["embeds"].as_array().cloned().unwrap_or_default(),
                                    is_own: false,
                                };
                                let _ = event_tx.send(ToApp::Message(msg));
                            }
                            "GUILD_CREATE" => {
                                let d = &v["d"];
                                let guild = Guild {
                                    id: d["id"].as_str().unwrap_or("").to_string(),
                                    name: d["name"].as_str().unwrap_or("Unknown").to_string(),
                                    icon: d["icon"].as_str().map(|s| s.to_string()),
                                    owner_id: d["owner_id"].as_str().unwrap_or("").to_string(),
                                };
                                let _ = event_tx.send(ToApp::Guild(guild));

                                if let Some(chs) = d["channels"].as_array() {
                                    for ch in chs {
                                        let cid = ch["id"].as_str().unwrap_or("").to_string();
                                        let cname = ch["name"].as_str().unwrap_or("unknown").to_string();
                                        let ctype = ch["type"].as_i64().unwrap_or(0);
                                        let topic = ch["topic"].as_str().map(|s| s.to_string());
                                        let pos = ch["position"].as_i64().unwrap_or(0) as i32;
                                        let _ = event_tx.send(ToApp::Channel(ChatChannel {
                                            id: cid,
                                            name: cname,
                                            guild_id: Some(d["id"].as_str().unwrap_or("").to_string()),
                                            channel_type: ctype,
                                            topic,
                                            position: pos,
                                        }));
                                    }
                                }
                            }
                            "CHANNEL_CREATE" | "DM_CHANNEL_CREATE" => {
                                let d = &v["d"];
                                let ctype = d["type"].as_i64().unwrap_or(0);
                                if ctype == 1 || ctype == 3 {
                                    let recipient = d["recipients"].as_array()
                                        .and_then(|r| r.first())
                                        .and_then(|r| r["username"].as_str())
                                        .unwrap_or("Unknown")
                                        .to_string();
                                    let _ = event_tx.send(ToApp::Channel(ChatChannel {
                                        id: d["id"].as_str().unwrap_or("").to_string(),
                                        name: recipient,
                                        guild_id: None,
                                        channel_type: ctype,
                                        topic: None,
                                        position: 0,
                                    }));
                                } else {
                                    let _ = event_tx.send(ToApp::Channel(ChatChannel {
                                        id: d["id"].as_str().unwrap_or("").to_string(),
                                        name: d["name"].as_str().unwrap_or("unknown").to_string(),
                                        guild_id: d["guild_id"].as_str().map(|s| s.to_string()),
                                        channel_type: ctype,
                                        topic: d["topic"].as_str().map(|s| s.to_string()),
                                        position: d["position"].as_i64().unwrap_or(0) as i32,
                                    }));
                                }
                            }
                            _ => {}
                        }
                    }
                    1 => {
                        let _ = event_tx.send(ToApp::Debug("Gateway requested heartbeat".into()));
                        heartbeat_failures = 0;
                        let p = json!({ "op": 1, "d": seq });
                        if write.send(WsMessage::Text(serde_json::to_string(&p).unwrap().into())).await.is_err() {
                            break;
                        }
                    }
                    9 => {
                        let d = v["d"].as_bool().unwrap_or(false);
                        let _ = event_tx.send(ToApp::Debug(format!("Invalid session (resumable={})", d)));
                        if !d {
                            session.session_id = None;
                            session.seq = None;
                            return Err("Invalid session, re-identifying".into());
                        }
                        session.seq = seq;
                        return Err("Invalid session, keeping session for resume".into());
                    }
                    7 => {
                        let _ = event_tx.send(ToApp::Debug("Reconnect requested by gateway".into()));
                        return Err("Reconnect requested".into());
                    }
                    11 => {
                        heartbeat_failures = 0;
                    }
                    _ => {}
                }
            }
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    ToGateway::Send { channel_id, content } => {
                        let url = format!("{}/channels/{}/messages", API_BASE, channel_id);
                        let mut req = http.post(&url)
                            .header("Authorization", &*tkn)
                            .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36")
                            .header("X-Super-Properties", &super_props())
                            .header("X-Discord-Locale", "en-US")
                            .header("X-Discord-Timezone", "Europe/Moscow")
                            .json(&json!({ "content": content, "nonce": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis().to_string() }));
                        match req.send().await {
                            Ok(resp) => {
                                let status = resp.status();
                                if !status.is_success() {
                                    let body = resp.text().await.unwrap_or_default();
                                    let _ = event_tx.send(ToApp::Debug(format!("Send failed {}: {}", status, body)));
                                } else {
                                    let _ = event_tx.send(ToApp::Debug("Message sent".into()));
                                }
                            }
                            Err(e) => {
                                let _ = event_tx.send(ToApp::Debug(format!("Send error: {}", e)));
                            }
                        }
                    }
                    ToGateway::FetchHistory { channel_id } => {
                        let cid_for_msg = channel_id.clone();
                        let mut all: Vec<ChatMessage> = Vec::new();
                        let mut before: Option<String> = None;
                        let mut done = false;
                        let mut failed = false;

                        while !done {
                            let url = match &before {
                                Some(b) => format!("{}/channels/{}/messages?limit=100&before={}", API_BASE, channel_id, b),
                                None => format!("{}/channels/{}/messages?limit=100", API_BASE, channel_id),
                            };
                            let mut attempt = 0u32;
                            let mut page_ok = false;
                            while !page_ok && attempt < 3 {
                                attempt += 1;
                                let req = http.get(&url)
                                    .header("Authorization", &*tkn)
                                    .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36")
                                    .header("X-Super-Properties", &super_props())
                                    .header("X-Discord-Locale", "en-US")
                                    .header("X-Discord-Timezone", "Europe/Moscow");
                                match req.send().await {
                                    Ok(resp) => {
                                        let status = resp.status();
                                        let _ = event_tx.send(ToApp::Debug(format!("History response: {}", status)));
                                        if status == 429 {
                                            let retry = resp.headers()
                                                .get("retry-after")
                                                .and_then(|v| v.to_str().ok())
                                                .and_then(|s| s.parse::<u64>().ok())
                                                .unwrap_or(2);
                                            let _ = event_tx.send(ToApp::Debug(format!("History 429, retrying in {}s", retry)));
                                            tokio::time::sleep(Duration::from_secs(retry)).await;
                                            continue;
                                        }
                                        if status.is_success() {
                                            match resp.text().await {
                                                Ok(body) => {
                                                    match serde_json::from_str::<Vec<Value>>(&body) {
                                                        Ok(arr) => {
                                                            if arr.is_empty() {
                                                                page_ok = true;
                                                                done = true;
                                                                break;
                                                            }
                                                            let mut page: Vec<ChatMessage> = arr.iter().filter_map(|m| {
                                                                let author = m.get("author")?;
                                                                Some(ChatMessage {
                                                                    id: m["id"].as_str().unwrap_or("").to_string(),
                                                                    channel_id: cid_for_msg.clone(),
                                                                    author_id: author["id"].as_str().unwrap_or("").to_string(),
                                                                    author_name: author["username"].as_str().unwrap_or("?").to_string(),
                                                                    author_avatar: author["avatar"].as_str().map(|s| s.to_string()),
                                                                    nickname: None,
                                                                    content: m["content"].as_str().unwrap_or("").to_string(),
                                                                    timestamp: m["timestamp"].as_str().unwrap_or("").to_string(),
                                                                    attachments: m["attachments"].as_array().map(|arr| {
                                                                        arr.iter().filter_map(|a| {
                                                                            Some(Attachment {
                                                                                filename: a["filename"].as_str()?.to_string(),
                                                                                url: a["url"].as_str()?.to_string(),
                                                                                content_type: a["content_type"].as_str().map(|s| s.to_string()),
                                                                                width: a["width"].as_u64().map(|v| v as u32),
                                                                                height: a["height"].as_u64().map(|v| v as u32),
                                                                                size: a["size"].as_u64().unwrap_or(0),
                                                                                description: a["description"].as_str().map(|s| s.to_string()),
                                                                            })
                                                                        }).collect()
                                                                    }).unwrap_or_default(),
                                                                    embeds: m["embeds"].as_array().cloned().unwrap_or_default(),
                                                                    is_own: false,
                                                                })
                                                            }).collect();
                                                            if page.is_empty() {
                                                                page_ok = true;
                                                                done = true;
                                                                break;
                                                            }
                                                            before = page.first().map(|m| m.id.clone());
                                                            all.extend(page);
                                                            page_ok = true;
                                                            if all.len() >= 300 {
                                                                done = true;
                                                                break;
                                                            }
                                                            tokio::time::sleep(Duration::from_millis(300)).await;
                                                        }
                                                        Err(e) => {
                                                            let _ = event_tx.send(ToApp::Debug(format!("History parse error: {}", e)));
                                                        }
                                                    }
                                                }
                                                Err(e) => {
                                                    let _ = event_tx.send(ToApp::Debug(format!("History body error: {}", e)));
                                                }
                                            }
                                        } else {
                                            let _ = event_tx.send(ToApp::Debug(format!("History error {}", status)));
                                            failed = true;
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        let _ = event_tx.send(ToApp::Debug(format!("History request error: {}", e)));
                                        tokio::time::sleep(Duration::from_secs(2)).await;
                                    }
                                }
                                if failed {
                                    break;
                                }
                            }
                            if failed {
                                done = true;
                            }
                        }

                        let _ = event_tx.send(ToApp::Debug(format!("History: {} messages total", all.len())));
                        all.reverse();
                        let _ = event_tx.send(ToApp::History { channel_id: cid_for_msg.clone(), messages: all });
                    }
                    ToGateway::OpenDM { user_id } => {
                        let url = format!("{}/users/@me/channels", API_BASE);
                        let req = http.post(&url)
                            .header("Authorization", &*tkn)
                            .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36")
                            .header("X-Super-Properties", &super_props())
                            .header("X-Discord-Locale", "en-US")
                            .header("X-Discord-Timezone", "Europe/Moscow")
                            .json(&json!({ "recipient_id": user_id }));
                        match req.send().await {
                            Ok(resp) => {
                                let status = resp.status();
                                if status.is_success() {
                                    match resp.text().await {
                                        Ok(body) => {
                                            if let Ok(d) = serde_json::from_str::<Value>(&body) {
                                                let recipient = d["recipients"].as_array()
                                                    .and_then(|r| r.first())
                                                    .and_then(|r| r["username"].as_str())
                                                    .unwrap_or("DM")
                                                    .to_string();
                                                let _ = event_tx.send(ToApp::DMChannel(ChatChannel {
                                                    id: d["id"].as_str().unwrap_or("").to_string(),
                                                    name: recipient,
                                                    guild_id: None,
                                                    channel_type: 1,
                                                    topic: None,
                                                    position: 0,
                                                }));
                                            }
                                        }
                                        Err(_) => {}
                                    }
                                } else {
                                    let _ = event_tx.send(ToApp::Debug(format!("Open DM failed {}", status)));
                                }
                            }
Err(e) => {
                                    let _ = event_tx.send(ToApp::Debug(format!("Open DM error: {}", e)));
}
                        }
                    }
                    ToGateway::Shutdown => {
                        let _ = event_tx.send(ToApp::Debug("Shutdown requested".into()));
                        return Ok(());
                    }
                }
            }
            else => break,
        }
    }

    Ok(())

}

fn main() -> eframe::Result<()> {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("[PANIC] {}", info);
        let _ = std::fs::write("/tmp/discord_panic.log", format!("{:?}", info));
    }));

    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_min_inner_size([700.0, 500.0])
            .with_title("Wyvern"),
        ..Default::default()
    };

    let (_, rx) = mpsc::unbounded_channel();
    let app = App::new(rx);

    eframe::run_native(
        "wyvern",
        opts,
        Box::new(|_cc| Ok(Box::new(app))),
    )
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
            vec![ChatMessage {
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
            }],
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
        }).collect::<Vec<_>>();

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
        assert!(App::load_accounts("").is_empty(), "no password, no accounts");
    }
}
