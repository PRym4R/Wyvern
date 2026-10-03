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

/// What the composer is replying to. `message_id` goes into
/// `message_reference`; the rest only feeds the reply bar.
#[derive(Clone, Debug)]
pub(crate) struct ReplyTarget {
    pub(crate) message_id: String,
    pub(crate) author_name: String,
    pub(crate) preview: String,
}

/// Message being edited in the composer. The text itself lives in `input`;
/// only the id and channel go to the gateway.
#[derive(Clone, Debug)]
pub(crate) struct EditTarget {
    pub(crate) channel_id: String,
    pub(crate) message_id: String,
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
    /// Message the composer is replying to; cleared on send, cancel, or channel switch.
    pub(crate) reply_to: Option<ReplyTarget>,
    /// Message the composer is editing; cleared on save, cancel, or channel switch.
    pub(crate) edit_target: Option<EditTarget>,
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
            reply_to: None,
            edit_target: None,
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
                ToApp::EditFailed {
                    channel_id,
                    message_id,
                    content,
                    reason,
                } => {
                    // A failed edit keeps the row as it was. Put the draft back
                    // into the composer, but only if the user hasn't already
                    // started writing something else.
                    if self.current_channel_id() == Some(channel_id.as_str()) {
                        let same = self
                            .edit_target
                            .as_ref()
                            .is_some_and(|e| e.message_id == message_id);
                        if self.input.trim().is_empty() && (self.edit_target.is_none() || same) {
                            self.input = content;
                            self.input_dirty = true;
                            self.edit_target = Some(EditTarget {
                                channel_id,
                                message_id,
                            });
                        }
                        self.send_error = Some(reason);
                    }
                }
                ToApp::DeleteFailed { channel_id, reason } => {
                    // The row stays; show the reason only where it happened.
                    if self.current_channel_id() == Some(channel_id.as_str()) {
                        self.send_error = Some(reason);
                    }
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
        self.reply_to = None;
        self.edit_target = None;
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
        // Switching channels drops the reply target along with the old list.
        self.reply_to = None;
        self.edit_target = None;
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
#[path = "app_tests.rs"]
mod layout_tests;
