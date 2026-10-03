use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, TextureHandle};
use tokio::sync::mpsc;

use crate::gateway::{run_gateway, EventTx, Generation};
use crate::media::AvatarFetch;
use crate::messages::{ToApp, ToGateway};
use crate::models::{
    BoundedCache, ChatChannel, ChatMessage, Guild, ImagePayload, LoadedImage, MsgHeight,
    StoredAccount, Theme, UserProfile,
};

/// Caps messages per channel; history loads upward, so an unbounded list would grow forever.
pub(crate) const MAX_MESSAGES_PER_CHANNEL: usize = 500;
/// Caps per-frame gateway event processing so bursts (with per-message duplicate scans) don't stall the UI.
pub(crate) const MAX_EVENTS_PER_FRAME: usize = 200;
/// Max avatar/guild-icon textures cached.
const MAX_AVATAR_CACHE: usize = 192;
/// Max image attachments cached (each costs megabytes of VRAM/RAM).
const MAX_IMAGE_CACHE: usize = 32;
/// Byte budget for the image cache; counting images alone is meaningless since each can be megabytes.
const IMAGE_CACHE_BUDGET: usize = 48 * 1024 * 1024;
/// Byte budget for avatars (small textures, but they still add up).
const AVATAR_CACHE_BUDGET: usize = 8 * 1024 * 1024;
/// Max failed image URLs remembered to avoid refetching them.
pub(crate) const MAX_FAILED_IMAGES: usize = 512;

/// Whether to log debug lines to disk/stderr; off by default. Enable with `WYVERN_DEBUG=1`.
fn debug_to_disk_from_env() -> bool {
    matches!(
        std::env::var("WYVERN_DEBUG").ok().as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Trims a channel to the cap, dropping the end the user isn't viewing: oldest
/// when `keep_newest`, newest when loading older history. Returns count dropped.
fn trim_messages(entry: &mut Vec<Arc<ChatMessage>>, keep_newest: bool) -> usize {
    let extra = entry.len().saturating_sub(MAX_MESSAGES_PER_CHANNEL);
    if extra == 0 {
        return 0;
    }
    if keep_newest {
        entry.drain(..extra);
    } else {
        entry.truncate(MAX_MESSAGES_PER_CHANNEL);
    }
    extra
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
    /// Vault is in the old unencrypted format; while set, don't rewrite the file or a typo could silently re-key it.
    pub(crate) vault_legacy: bool,
    pub(crate) login_password: String,
    /// Non-error notice shown on the login screen (e.g. password accepted after trimming).
    pub(crate) login_notice: String,
    /// Vault path override; tests run in parallel, so a shared env var would hit the real file.
    pub(crate) vault_path_override: Option<std::path::PathBuf>,
    /// Session-long lock on the vault file so another client instance can't share and clobber it.
    pub(crate) vault_lock: Option<std::fs::File>,
    pub(crate) login_selected: Option<String>,
    pub(crate) remember_account: bool,
    pub(crate) saved_accounts: Vec<StoredAccount>,
    pub(crate) active_index: Option<usize>,
    pub(crate) status: String,
    /// Why the last message failed to send; shown in chat, unlike `status` which is login-only.
    pub(crate) send_error: Option<String>,
    /// Last submitted text, so held Enter/auto-repeat can't resend the same failed message.
    pub(crate) last_submitted: Option<String>,
    /// Whether the input changed since the last send; blocks resending identical text.
    pub(crate) input_dirty: bool,
    /// Recent debug log lines; `VecDeque` so evicting the oldest doesn't shift the whole buffer.
    pub(crate) debug_log: VecDeque<String>,
    /// Whether to log to disk/stderr; see `debug_to_disk_from_env`.
    pub(crate) debug_to_disk: bool,
    /// Test probe: strong refs seen last frame; a per-frame list copy would inflate this.
    #[cfg(test)]
    pub(crate) probe_msg_refs: usize,
    /// Test probe: guilds left in `self.guilds` during panel render (full on copy, zero on take).
    #[cfg(test)]
    pub(crate) probe_guilds_in_render: usize,
    /// Test-only: skip starting a real gateway, since account switch resets state offline.
    #[cfg(test)]
    pub(crate) no_gateway: bool,
    /// Test probe: per-frame clones of the whole account list (a copy sets this to 1).
    #[cfg(test)]
    pub(crate) probe_accounts_cloned: usize,
    /// Test probe: per-frame clones of the selected guild in the channel list.
    #[cfg(test)]
    pub(crate) probe_channel_guild_cloned: usize,
    pub(crate) to_gw: Option<mpsc::UnboundedSender<ToGateway>>,
    pub(crate) from_gw: mpsc::UnboundedReceiver<ToApp>,
    pub(crate) gw_started: bool,
    /// Current egui context, so the gateway can wake the window per event when idle.
    pub(crate) egui_ctx: Option<egui::Context>,
    /// Whether an animated image was drawn this frame; it must keep repainting while visible.
    pub(crate) animating: bool,
    /// Gateway generation (see `Generation`); switching accounts bumps it so the old thread goes quiet.
    pub(crate) gateway_generation: Arc<Generation>,
    pub(crate) avatar_cache: BoundedCache<TextureHandle>,
    pub(crate) pending_avatars: HashMap<String, std::sync::mpsc::Receiver<AvatarFetch>>,
    /// Failed avatar/icon URLs; avoids retrying them every frame and burning the CDN limit.
    pub(crate) failed_avatars: HashSet<String>,
    pub(crate) image_cache: BoundedCache<LoadedImage>,
    pub(crate) pending_images: HashMap<String, std::sync::mpsc::Receiver<Option<ImagePayload>>>,
    pub(crate) failed_images: HashSet<String>,
    pub(crate) theme: Theme,
    pub(crate) show_friends: bool,
    pub(crate) friends: Vec<UserProfile>,
    pub(crate) history_loading: Option<String>,
    /// Loading older messages by scrolling up, distinct from `history_loading` (first page).
    pub(crate) history_loading_more: bool,
    /// Reached the start of the channel; stop requesting older pages that don't exist.
    pub(crate) history_exhausted: bool,
    /// Newest messages dropped to fit a loaded-upward page under the cap; shown in chat.
    pub(crate) trimmed_newest: usize,
    /// Debug command awaiting confirmation (channel id); `None` means nothing pending.
    pub(crate) pending_debug_add: Option<(String, std::time::Instant)>,
    /// Channel panel width last frame, so its change is logged once instead of every frame.
    pub(crate) channel_panel_w: f32,
    /// Last history load failure (channel, reason); keeps the channel from looking broken.
    pub(crate) history_error: Option<(String, String)>,
    /// Whether the chat is scrolled to the bottom; new messages only auto-scroll then.
    pub(crate) chat_at_bottom: bool,
    /// Measured message heights by id for virtualization; keyed by id so prepends don't invalidate it.
    pub(crate) msg_heights: HashMap<String, MsgHeight>,
    /// Prefix sums of heights; per-frame buffer reused across frames.
    pub(crate) msg_offsets: Vec<f32>,
    /// Counter for local echo ids; app-wide so ids stay unique per session.
    pub(crate) next_local_id: u64,
    /// Chat content width last frame; line wrapping and thus message heights depend on it.
    pub(crate) msg_width: f32,
    /// Chat viewport height last frame, needed to jump to the bottom before egui knows content height.
    pub(crate) chat_inner_h: f32,
    /// Actual vs requested content height last frame; the gap corrects height estimate error.
    pub(crate) chat_content_h: f32,
    pub(crate) chat_est_h: f32,
    /// Chat scroll mirror: what the scroll area is currently showing.
    pub(crate) chat_offset_y: f32,
    /// View anchor: id of the message at the top edge and its offset, kept stable across list changes.
    pub(crate) chat_anchor: Option<(String, f32)>,
    /// Request older messages; set during rendering.
    pub(crate) want_older: bool,
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
            vault_legacy: false,
            login_password: String::new(),
            login_notice: String::new(),
            vault_path_override: None,
            vault_lock: None,
            login_selected: None,
            remember_account: true,
            saved_accounts: Vec::new(),
            active_index: None,
            status: String::new(),
            send_error: None,
            last_submitted: None,
            input_dirty: false,
            debug_log: VecDeque::new(),
            debug_to_disk: debug_to_disk_from_env(),
            #[cfg(test)]
            probe_msg_refs: 0,
            #[cfg(test)]
            probe_guilds_in_render: 0,
            #[cfg(test)]
            no_gateway: false,
            #[cfg(test)]
            probe_accounts_cloned: 0,
            #[cfg(test)]
            probe_channel_guild_cloned: 0,
            to_gw: None,
            from_gw,
            gw_started: false,
            egui_ctx: None,
            animating: false,
            gateway_generation: Arc::new(Generation::default()),
            accounts_unlocked: false,
            avatar_cache: BoundedCache::with_budget(MAX_AVATAR_CACHE, AVATAR_CACHE_BUDGET),
            pending_avatars: HashMap::new(),
            failed_avatars: HashSet::new(),
            image_cache: BoundedCache::with_budget(MAX_IMAGE_CACHE, IMAGE_CACHE_BUDGET),
            pending_images: HashMap::new(),
            failed_images: HashSet::new(),
            theme: Theme::dark(),
            show_friends: false,
            friends: Vec::new(),
            history_loading: None,
            history_loading_more: false,
            history_exhausted: false,
            trimmed_newest: 0,
            pending_debug_add: None,
            channel_panel_w: 0.0,
            history_error: None,
            chat_at_bottom: true,
            msg_heights: HashMap::new(),
            msg_offsets: Vec::new(),
            next_local_id: 0,
            msg_width: 0.0,
            chat_inner_h: 0.0,
            chat_content_h: 0.0,
            chat_est_h: 0.0,
            chat_offset_y: 0.0,
            chat_anchor: None,
            want_older: false,
            autoselected: false,
            theme_index: 1,
            last_render_key: String::new(),
            scroll_to_bottom: true,
            debug_frames: 0,
            last_scroll_offset_y: 0.0,
        }
    }
    pub(crate) fn push_debug(&mut self, msg: String) {
        // Only write to disk/stderr with WYVERN_DEBUG; the in-memory log is always kept.
        if self.debug_to_disk {
            let line = format!("[GW] {}", msg);
            eprintln!("{}", line);
            use std::io::Write;
            // Rotate the log file at 2 MB so it can't grow forever.
            let path = "/tmp/wyvern_layout.log";
            let too_big = std::fs::metadata(path).map(|m| m.len() > 2 * 1024 * 1024).unwrap_or(false);
            if too_big {
                let _ = std::fs::rename(path, format!("{}.1", path));
            }
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(f, "{}", line);
            }
        }
        self.debug_log.push_back(msg);
        if self.debug_log.len() > 100 {
            self.debug_log.pop_front();
        }
    }
    /// Drains pending gateway events; returns true if any arrived so one more layout frame is drawn.
    pub(crate) fn poll(&mut self, ctx: &egui::Context) -> bool {
        // Remember the context so the gateway can wake it per event instead of waiting for a redraw.
        self.egui_ctx = Some(ctx.clone());
        let mut any = false;
        // Process only a batch per frame so bursts don't cause multi-millisecond stalls; the rest waits.
        let mut processed = 0usize;
        while processed < MAX_EVENTS_PER_FRAME {
            let Ok(ev) = self.from_gw.try_recv() else { break };
            processed += 1;
            any = true;
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
                        // Account already listed; just fill in the missing username.
                        let blank = self
                            .saved_accounts
                            .iter()
                            .position(|a| a.token == tkn && a.username.is_empty());
                        if let Some(i) = blank {
                            self.saved_accounts[i].username = username.clone();
                            let pw = self.master_password.clone();
                            self.save_accounts(&pw);
                        }
                    }
                    self.push_debug("READY received!".into());
                }
                // Live messages for a channel that isn't open have nowhere to go; open_channel reloads its history anyway.
                ToApp::Message(_msg) if self.current_channel_id() != Some(_msg.channel_id.as_str()) => {
                }
                ToApp::Message(msg) => {
                    let cid = msg.channel_id.clone();
                    // A message can arrive both live and in a history page; a duplicate would render twice.
                    let duplicate = !msg.id.is_empty()
                        && self.messages.get(&cid).is_some_and(|e| {
                            // Search from the tail; live messages are almost always there and the list is bounded.
                            e.iter().rev().any(|m| m.id == msg.id)
                        });
                    if duplicate {
                        self.push_debug(format!("Duplicate live message {} ignored", msg.id));
                    } else {
                        // The confirmation replaces our local echo; otherwise the same text would appear twice.
                        let replaced = self.replace_local_echo(&cid, &msg);
                        if !replaced {
                            let entry = self.messages.entry(cid).or_default();
                            entry.push(Arc::new(msg));
                            // A new own message while viewing the newest: drop the oldest.
                            trim_messages(entry, true);
                        }
                        // Decide from last frame's position; don't yank a user reading history to the bottom.
                        if self.chat_at_bottom {
                            self.scroll_to_bottom = true;
                        }
                    }
                }
                ToApp::MessageUpdated(msg) => self.update_message(msg),
                ToApp::MessageDeleted { channel_id, message_id } => {
                    self.delete_message(&channel_id, &message_id)
                }
                ToApp::MessageDeletedBulk { channel_id, message_ids } => {
                    self.delete_messages(&channel_id, &message_ids)
                }
                ToApp::ChannelUpdated { channel_id, name, topic } => {
                    self.update_channel(&channel_id, name, topic)
                }
                ToApp::History { channel_id, messages, more } => {
                    self.apply_history(&channel_id, messages, more, false);
                }
                ToApp::HistoryMore { channel_id, messages, more } => {
                    self.apply_history(&channel_id, messages, more, true);
                }
                ToApp::HistoryFailed { channel_id, before, reason } => {
                    self.history_failed(&channel_id, before.as_deref(), &reason);
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
                        self.channels.push(ch);
                        self.channels.len() - 1
                    };
                    self.selected_guild = None;
                    self.selected_channel = Some(idx);
                    self.show_friends = false;
                    self.scroll_to_bottom = true;
                    let cid = self.channels[idx].id.clone();
                    self.open_channel(&cid);
                }
                ToApp::Friends(list) => {
                    self.friends = list;
                    self.push_debug(format!("Loaded {} friends", self.friends.len()));
                }
                ToApp::Status(s) => {
                    // Connection status also means we're not online; clear `connected` on reconnect/disconnect.
                    if s.starts_with("Reconnecting") || s == "Disconnected" {
                        self.connected = false;
                    }
                    self.status = s;
                }
                ToApp::SendFailed { channel_id, local_id, reason } => {
                    // Send failed: remove the local echo and restore the text, which Enter had already cleared.
                    let mut taken_back: Option<String> = None;
                    if let Some(entry) = self.messages.get_mut(&channel_id) {
                        if let Some(pos) = entry.iter().rposition(|m| m.id == local_id) {
                            taken_back = Some(entry.remove(pos).content.clone());
                            // The list shrank, so cached heights and the anchor no longer apply.
                            self.msg_heights.clear();
                            self.msg_offsets.clear();
                            self.chat_anchor = None;
                            self.scroll_to_bottom = true;
                        }
                    }
                    // Restore the text only if the user is in this channel and hasn't started a new draft.
                    if self.current_channel_id() == Some(channel_id.as_str()) && self.input.trim().is_empty() {
                        self.input = taken_back.unwrap_or_default();
                    }
                    self.send_error = Some(reason);
                    // Truncate by chars; the id is ours, not Discord's (see token masking).
                    let short: String = local_id.chars().take(14).collect();
                    self.push_debug(format!("Send failed for {}", short));
                }
                ToApp::AuthFailed { reason } => {
                    // Token rejected: return to the login screen, keeping the token in the field so it can be fixed.
                    self.connected = false;
                    self.gw_started = false;
                    self.to_gw = None;
                    self.push_debug(format!("Auth failed: {}", reason));
                    self.status = reason;
                }
                ToApp::Debug(d) => self.push_debug(d),
            }
        }
        // Queue not empty; the next `poll` handles the rest since events were seen.
        if !self.from_gw.is_empty() {
            self.push_debug(format!(
                "Gateway backlog: processed {} events, more pending",
                processed
            ));
        }
        any
    }
    pub(crate) fn start_gateway(&mut self, token: String) {
        #[cfg(test)]
        if self.no_gateway {
            // Account-switch test stays offline; it only checks state reset.
            self.gw_started = true;
            return;
        }
        let (to_gw_tx, to_gw_rx) = mpsc::unbounded_channel();
        let (from_gw_tx, from_gw_rx) = mpsc::unbounded_channel();
        self.to_gw = Some(to_gw_tx);
        self.from_gw = from_gw_rx;
        self.gw_started = true;
        // Each gateway run gets its own generation so the lingering old thread can't clobber the new state.
        let generation = self.gateway_generation.next();
        let event_tx = EventTx::new(from_gw_tx, generation, self.gateway_generation.clone());
        // Bind the context so the gateway wakes it per event; idle has no frames otherwise.
        let event_tx = match &self.egui_ctx {
            Some(ctx) => event_tx.with_wake(ctx.clone()),
            None => event_tx,
        };

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(run_gateway(to_gw_rx, event_tx, token));
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
        self.history_loading_more = false;
        self.history_exhausted = false;
        self.history_error = None;
        self.msg_heights.clear();
        self.msg_offsets.clear();
        self.chat_anchor = None;
        self.chat_offset_y = 0.0;
        self.chat_content_h = 0.0;
        self.chat_est_h = 0.0;
        self.show_friends = false;
        self.username.clear();
        self.user_id.clear();
        self.user_avatar = None;
        // Media caches and failure lists belong to the old account; the new one must not inherit them.
        self.image_cache.clear();
        self.avatar_cache.clear();
        self.pending_images.clear();
        self.pending_avatars.clear();
        self.failed_images.clear();
        self.failed_avatars.clear();
        self.token_input = token.clone();
        self.start_gateway(token.clone());
        self.add_saved_account(&token, "");
        self.push_debug(format!("Switched account to {}", self.mask_token(&token)));
    }
    /// Login with a fresh token; if "remember" is on, unlock/create the vault first.
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
                // Can't remember without a vault password; stop and tell the user instead of continuing silently.
                self.status =
                    "Чтобы запомнить аккаунт, введи пароль хранилища (или сними галочку «Запомнить»)"
                        .to_string();
                return;
            }
            match self.unlock_vault(&pw) {
                Ok(Some(hint)) => self.login_notice = hint,
                Ok(None) => {}
                // Vault didn't open; continuing would lose the account without a word.
                Err(e) => {
                    self.status = format!("{} — введи пароль хранилища или сними галочку «Запомнить»", e);
                    return;
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
    /// Open a channel; other channels' messages can be dropped since history reloads anyway.
    pub(crate) fn open_channel(&mut self, channel_id: &str) {
        self.history_loading = Some(channel_id.to_string());
        self.history_loading_more = false;
        self.history_exhausted = false;
        // A previous failure for this channel is stale on a new attempt.
        self.history_error = None;
        self.trimmed_newest = 0;
        self.messages.retain(|k, _| k == channel_id);
        // Drop unclaimed media downloads from the old channel so decoded pixels aren't held all session.
        self.pending_images.clear();
        self.pending_avatars.clear();
        self.msg_heights.clear();
        self.send_cmd(ToGateway::FetchHistory { channel_id: channel_id.to_string(), before: None });
    }
    /// Stores a history page: the first page replaces, `prepend` inserts at the front.
    /// Spinners/scroll are only touched for the currently open channel.
    fn apply_history(&mut self, channel_id: &str, incoming: Vec<ChatMessage>, more: bool, prepend: bool) {
        // Discord returns newest-first but we store oldest-first, so `before` pages from
        // the oldest; otherwise the same page repeats and the chat fills with copies.
        let mut incoming = incoming;
        incoming.reverse();
        // An older-page response into an empty list is stale mid-history; request the first page instead.
        if prepend && !self.messages.contains_key(channel_id) {
            let cid_short: String = channel_id.chars().take(14).collect();
            self.push_debug(format!(
                "Dropping stale older page for {}: list was cleared, asking first page again",
                cid_short
            ));
            // Ask for the first page again and leave the first-page spinner running.
            self.send_cmd(ToGateway::FetchHistory { channel_id: channel_id.to_string(), before: None });
            return;
        }
        let added = {
            let entry = self.messages.entry(channel_id.to_string()).or_default();
            if prepend {
                // A boundary message can arrive in both pages; drop the duplicate.
                let dup = entry.first().map(|m| m.id.clone());
                let mut page: Vec<Arc<ChatMessage>> = incoming.into_iter().map(Arc::new).collect();
                if let Some(dup) = dup {
                    page.retain(|m| m.id != dup);
                }
                let added = page.len();
                entry.splice(..0, page);
                added
            } else {
                entry.clear();
                entry.extend(incoming.into_iter().map(Arc::new));
                0
            }
        };
        // Enforce the cap; see `trim_messages` for which end is dropped.
        let (stored, dropped_newest) = {
            let entry = self.messages.get_mut(channel_id).expect("страницу только что положили");
            let dropped = trim_messages(entry, !prepend);
            (entry.len(), dropped)
        };
        // First page reloads the whole channel, so reset the counter from the previous window.
        if !prepend {
            self.trimmed_newest = 0;
        }
        if dropped_newest > 0 {
            self.trimmed_newest = dropped_newest;
        }
        // Message dump only with WYVERN_DUMP, and never on prepend (would rewrite the file per page).
        if !prepend && std::env::var_os("WYVERN_DUMP").is_some() {
            if let Some(entry) = self.messages.get(channel_id) {
                let dump = entry.iter()
                    .map(|m| format!("[{}] {} (id {}): {}{}", m.timestamp, m.author_name, m.author_id, m.content,
                        if m.attachments.is_empty() { String::new() } else { format!(" <{} attachments>", m.attachments.len()) }))
                    .collect::<Vec<_>>()
                    .join("\n");
                let _ = std::fs::write("/tmp/wyvern_messages_dump.txt", dump);
            }
        }
        let for_current = self.current_channel_id() == Some(channel_id);
        if for_current {
            self.history_loading = None;
            self.history_loading_more = false;
            // No more to load: Discord said so, or the per-channel cap is reached.
            self.history_exhausted = !more || stored >= MAX_MESSAGES_PER_CHANNEL;
            if !prepend {
                self.scroll_to_bottom = true;
                // The list was fully replaced, so cached heights no longer apply.
                self.msg_heights.clear();
            }
        }
        // Truncate by chars; slicing bytes would panic on non-ASCII ids.
        let cid_short: String = channel_id.chars().take(14).collect();
        self.push_debug(format!("Stored {} msgs ({} new) for channel {}{}", stored, added, cid_short,
            if for_current { "" } else { " (не текущий канал)" }));
    }
    /// History page failed: clear the matching wait and show the reason.
    /// `before` picks which wait: `None` for the first page, `Some` for older history.
    fn history_failed(&mut self, channel_id: &str, before: Option<&str>, reason: &str) {
        if before.is_none() {
            // Only clear the first-page wait if it was for this channel.
            if self.history_loading.as_deref() == Some(channel_id) {
                self.history_loading = None;
            }
        } else {
            self.history_loading_more = false;
        }
        // Stop hitting the API; a 403 won't fix itself, and reopening gives a fresh try.
        if self.current_channel_id() == Some(channel_id) {
            self.history_exhausted = true;
            self.history_error = Some((channel_id.to_string(), reason.to_string()));
            self.scroll_to_bottom = true;
        }
        // Truncate by chars; a byte slice would panic on non-ASCII ids.
        let cid_short: String = channel_id.chars().take(14).collect();
        self.push_debug(format!("History failed for {} ({}): {}", cid_short, before.is_some(), reason));
    }
    /// Requests older history from the oldest stored message (list is oldest-first).
    /// Skips local echoes, whose ids aren't real, and won't refetch while a page is in flight.
    pub(crate) fn request_older_history(&mut self) {
        if self.history_loading.is_some() || self.history_loading_more || self.history_exhausted {
            return;
        }
        let Some(cid) = self.current_channel_id().map(str::to_string) else { return };
        let Some(oldest) = self
            .messages
            .get(&cid)
            .and_then(|v| v.iter().find(|m| !m.is_local_echo() && !m.id.is_empty()))
            .map(|m| m.id.clone())
        else {
            // No real message to page from; local echoes don't count.
            self.history_exhausted = true;
            return;
        };
        self.history_loading_more = true;
        self.send_cmd(ToGateway::FetchHistory { channel_id: cid, before: Some(oldest) });
    }
    /// Replaces our local echo with the confirmed message so the text isn't duplicated.
    /// Returns true if an echo was found and replaced.
    fn replace_local_echo(&mut self, channel_id: &str, msg: &ChatMessage) -> bool {
        // Only our own message can confirm a send.
        if self.user_id.is_empty() || msg.author_id != self.user_id {
            return false;
        }
        let Some(entry) = self.messages.get_mut(channel_id) else { return false };
        // Search from the tail and match by text; if there's no echo, it's added normally.
        match entry.iter().rposition(|m| m.is_local_echo() && m.content == msg.content) {
            Some(idx) => {
                entry[idx] = Arc::new(msg.clone());
                true
            }
            None => false,
        }
    }
    /// Id of the currently open channel, which history and chat state refer to.
    pub(crate) fn current_channel_id(&self) -> Option<&str> {
        let ch = self.channels.get(self.selected_channel?)?;
        Some(ch.id.as_str())
    }
    /// Display name; returns a borrowed str instead of cloning it per frame.
    pub(crate) fn display_name<'a>(&self, msg: &'a ChatMessage) -> &'a str {
        msg.nickname.as_deref().unwrap_or(msg.author_name.as_str())
    }
    /// Display content: message content, else an attachment or embed description; borrowed.
    pub(crate) fn display_content<'a>(&self, msg: &'a ChatMessage) -> &'a str {
        if !msg.content.trim().is_empty() {
            return msg.content.as_str();
        }
        for att in &msg.attachments {
            if let Some(d) = &att.description {
                if !d.trim().is_empty() {
                    return d.as_str();
                }
            }
        }
        for e in &msg.embeds {
            if let Some(d) = &e.description {
                if !d.is_empty() {
                    return d.as_str();
                }
            }
        }
        ""
    }
    /// "12:34" from an ISO timestamp; a borrowed slice, no copy.
    pub(crate) fn short_time<'a>(&self, iso: &'a str) -> &'a str {
        let t = iso.trim_start_matches('T');
        if t.len() >= 16 {
            &t[11..16]
        } else {
            iso
        }
    }
    /// Edits the message with the same id. Discord sends only changed fields, so
    /// empty ones don't overwrite; cached height is dropped since text may grow.
    pub(crate) fn update_message(&mut self, msg: ChatMessage) {
        let id = msg.id.clone();
        let mut changed = false;
        if let Some(entry) = self.messages.get_mut(&msg.channel_id) {
            if let Some(slot) = entry.iter_mut().find(|m| m.id == id) {
                let old = Arc::make_mut(slot);
                if !msg.content.is_empty() {
                    old.content = msg.content;
                }
                if !msg.author_name.is_empty() {
                    old.author_name = msg.author_name;
                }
                if !msg.attachments.is_empty() {
                    old.attachments = msg.attachments;
                }
                if !msg.embeds.is_empty() {
                    old.embeds = msg.embeds;
                }
                changed = true;
            }
        }
        if changed {
            self.msg_heights.remove(&id);
        }
    }
    /// Remove one message and its cached height so it doesn't linger until rejoin.
    pub(crate) fn delete_message(&mut self, channel_id: &str, message_id: &str) {
        if let Some(entry) = self.messages.get_mut(channel_id) {
            entry.retain(|m| m.id != message_id);
        }
        self.msg_heights.remove(message_id);
    }
    /// Bulk delete: one event for many ids.
    pub(crate) fn delete_messages(&mut self, channel_id: &str, ids: &[String]) {
        if let Some(entry) = self.messages.get_mut(channel_id) {
            entry.retain(|m| !ids.iter().any(|id| id == &m.id));
        }
        for id in ids {
            self.msg_heights.remove(id);
        }
    }
    /// Rename/retopic a channel; the header reads these from `self.channels`.
    pub(crate) fn update_channel(
        &mut self,
        channel_id: &str,
        name: Option<String>,
        topic: Option<String>,
    ) {
        if let Some(ch) = self.channels.iter_mut().find(|c| c.id == channel_id) {
            if let Some(n) = name {
                ch.name = n;
            }
            if let Some(t) = topic {
                ch.topic = Some(t);
            }
        }
    }
    /// Takes the open channel's messages for the frame so rendering owns the Vec and
    /// can call `&mut` methods without cloning the Arc list each frame.
    /// `restore_channel_messages` puts them back; also returns the channel id.
    pub(crate) fn take_channel_messages(
        &mut self,
    ) -> (Option<String>, Vec<Arc<ChatMessage>>) {
        let Some(id) = self
            .selected_channel
            .and_then(|i| self.channels.get(i))
            .map(|ch| ch.id.clone())
        else {
            return (None, Vec::new());
        };
        match self.messages.get_mut(&id) {
            Some(entry) => (Some(id), std::mem::take(entry)),
            None => (None, Vec::new()),
        }
    }

    /// Puts back the list taken by `take_channel_messages`.
    pub(crate) fn restore_channel_messages(
        &mut self,
        id: Option<String>,
        msgs: Vec<Arc<ChatMessage>>,
    ) {
        if let Some(id) = id {
            if let Some(entry) = self.messages.get_mut(&id) {
                *entry = msgs;
            }
        }
    }
    /// Whether the login screen shows instead of the chat; a separate method so it can be tested.
    pub(crate) fn shows_login(&self) -> bool {
        self.token_input.is_empty() || (!self.connected && !self.gw_started)
    }

    /// One app frame: poll, draw, schedule; separate from `eframe::App::update` for tests.
    pub(crate) fn run_frame(&mut self, ctx: &egui::Context) {
        let had_events = self.poll(ctx);

        self.theme = match self.theme_index {
            0 => Theme::dark(),
            1 => Theme::cyberpunk(),
            _ => Theme::light(),
        };

        // Animation flag is rebuilt each frame by rendering if an animated image is visible.
        self.animating = false;
        // New image-cache frame: last frame's visible items age by one frame.
        self.image_cache.begin_frame();
        if self.shows_login() {
            self.draw_login(ctx);
        } else {
            self.draw_chat(ctx);
        }
        // Reap finished downloads after drawing so offscreen pixels don't pile up; LRU then spares visible items.
        self.reap_pending_images(ctx);

        self.schedule_repaint(ctx, had_events);
    }

    /// Whether to request the next frame; idle requests nothing so the app doesn't repaint 20x/s.
    fn schedule_repaint(&self, ctx: &egui::Context, had_events: bool) {
        // Background loads are reaped in draw and animations advance by time, so keep frames coming.
        let busy = !self.pending_avatars.is_empty()
            || !self.pending_images.is_empty()
            || self.history_loading.is_some()
            || self.history_loading_more
            || self.animating;
        if busy {
            ctx.request_repaint_after(Duration::from_millis(50));
        } else if had_events {
            // An event just changed state; draw one more frame to settle layout, then sleep.
            ctx.request_repaint();
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.run_frame(ctx);
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
}
