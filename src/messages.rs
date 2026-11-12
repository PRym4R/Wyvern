use crate::models::{ChatChannel, ChatMessage, Guild, Reaction, UserProfile, VoiceUser};

#[derive(Clone, Debug)]
pub(crate) struct ReadState {
    pub(crate) channel_id: String,
    /// Newest message id we had acknowledged in the channel, if any.
    pub(crate) last_message_id: Option<String>,
    pub(crate) mention_count: u32,
}

#[derive(Clone, Debug)]
pub(crate) enum ToApp {
    Ready { username: String, user_id: String, avatar: Option<String> },
    Message(ChatMessage),
    /// Message edited: replace the row with the same id with the full object.
    MessageUpdated(ChatMessage),
    /// Message deleted: drop the row by id and its cached height.
    MessageDeleted { channel_id: String, message_id: String },
    /// Bulk delete: one event for many ids.
    MessageDeletedBulk { channel_id: String, message_ids: Vec<String> },
    /// One user added a reaction. `reaction` holds the emoji only (count 1);
    /// `user_id` lets the app keep our own optimistic pill from counting twice.
    ReactionAdded {
        channel_id: String,
        message_id: String,
        reaction: Reaction,
        user_id: String,
    },
    /// One user took a reaction back; the counter drops and an empty pill goes.
    ReactionRemoved {
        channel_id: String,
        message_id: String,
        reaction: Reaction,
        user_id: String,
    },
    /// Everyone lost one emoji (moderator action): the whole pill goes.
    ReactionEmojiCleared {
        channel_id: String,
        message_id: String,
        reaction: Reaction,
    },
    /// Every reaction on the message was removed.
    ReactionsCleared { channel_id: String, message_id: String },
    /// Someone started typing in a channel. `user_name` may be empty when the
    /// event carries only a user id (DMs); the app then resolves the name from
    /// messages it has already seen.
    Typing { channel_id: String, user_id: String, user_name: String },
    /// Channel name or topic changed; shown in the chat header.
    ChannelUpdated { channel_id: String, name: Option<String>, topic: Option<String> },
    /// First page of channel history, replacing what is there. `more` = older
    /// messages are loadable on scroll. Discord sends newest-first;
    /// `App::apply_history` reverses it to oldest-first.
    History { channel_id: String, messages: Vec<ChatMessage>, more: bool },
    /// Older messages (scrolled up): inserted at the top, not replacing.
    HistoryMore { channel_id: String, messages: Vec<ChatMessage>, more: bool },
    /// History page failed after three attempts (network, 403, 500). Without
    /// this the loading spinner never clears. `before` selects which flag to
    /// clear: `None` for the first page, `Some` for loading older messages.
    HistoryFailed { channel_id: String, before: Option<String>, reason: String },
    /// One page fetched around a message, so the view can land on a search hit
    /// that is far back in history. `target_id` is the row to anchor on.
    HistoryAround {
        channel_id: String,
        messages: Vec<ChatMessage>,
        more: bool,
        target_id: String,
    },
    /// Guild-wide search results, newest first. `query` is echoed so a late
    /// answer for an older query can be dropped.
    SearchResults { query: String, messages: Vec<ChatMessage> },
    /// Guild search failed (no permission, network, rate limit). The overlay
    /// shows the reason instead of a spinner that never ends.
    SearchFailed { query: String, reason: String },
    /// One participant's voice state changed. `channel` is `None` when the user
    /// is no longer in any voice channel; that also covers a disconnect.
    VoiceState {
        channel: Option<String>,
        user: VoiceUser,
    },
    /// Voice audio: `ready` once playback started, otherwise `reason` says why
    /// (no output device, failed handshake, UDP blocked).
    VoiceAudio { ready: bool, reason: Option<String> },
    /// Someone started or stopped talking in the current voice channel.
    VoiceSpeaking { user_id: String, speaking: bool },
    Guild(Guild),
    Channel(ChatChannel),
    GuildChannels { guild_id: String, channels: Vec<ChatChannel> },
    DMChannel(ChatChannel),
    /// Per-channel read state from READY (`last_message_id` / `mention_count`).
    /// Restores unread and mention badges on startup instead of starting blank.
    ReadStates(Vec<ReadState>),
    Friends(Vec<UserProfile>),
    Status(String),
    /// Discord rejected the token (close codes 4004/4007/4013/4014);
    /// reconnecting is pointless. Returns the client to the login screen,
    /// which the gateway cannot do itself.
    AuthFailed { reason: String },
    /// Send failed: Discord refused (403, 400, 429) or the request never
    /// arrived. Removes the optimistic echo by `local_id` and shows `reason`.
    SendFailed { channel_id: String, local_id: String, reason: String },
    /// Edit refused (404, no permission, network). The row keeps its old text;
    /// `content` lets the composer restore the draft for another try.
    EditFailed { channel_id: String, message_id: String, content: String, reason: String },
    /// Delete refused; the row stays and `reason` is shown in chat.
    DeleteFailed { channel_id: String, reason: String },
    /// Reaction refused (no permission, deleted message, network). `add` says
    /// which way the optimistic toggle went so the app can undo exactly that.
    ReactionFailed {
        channel_id: String,
        message_id: String,
        emoji: String,
        add: bool,
    },
    Debug(String),
}

#[derive(Debug)]
pub(crate) enum ToGateway {
    /// Send a message. `local_id` identifies the optimistic echo to remove on
    /// failure. `reply_to` is the id of the message being answered, if any;
    /// it becomes `message_reference` in the REST body.
    Send { channel_id: String, content: String, local_id: String, reply_to: Option<String> },
    /// Send a message with file attachments read from disk.
    SendFiles { channel_id: String, content: String, paths: Vec<String>, reply_to: Option<String> },
    /// Edit one of our messages. Discord answers with MESSAGE_UPDATE, which is
    /// what actually replaces the row; a rejection comes back as `EditFailed`.
    EditMessage { channel_id: String, message_id: String, content: String },
    /// Delete one of our messages. Discord answers with MESSAGE_DELETE, which
    /// removes the row; a rejection comes back as `DeleteFailed`.
    DeleteMessage { channel_id: String, message_id: String },
    /// Add (`add`) or remove our own reaction. `emoji` is the API key:
    /// `name:id` for a custom emoji, the bare character otherwise.
    React {
        channel_id: String,
        message_id: String,
        emoji: String,
        add: bool,
    },
    /// History page. `before` is the oldest shown id: `None` for the first
    /// page, otherwise paging upward.
    FetchHistory { channel_id: String, before: Option<String> },
    /// Fetch 25 messages around one id: a search hit may be far from the
    /// newest page. The answer replaces that channel's list.
    JumpToMessage { channel_id: String, message_id: String },
    /// Search the whole guild's messages. `guild_id` scopes it; the answer is
    /// `SearchResults` / `SearchFailed` with the query echoed.
    SearchGuild { guild_id: String, query: String },
    /// Join a voice channel (op 4). Discord answers with `VOICE_STATE_UPDATE`
    /// and `VOICE_SERVER_UPDATE`; this step only does the control plane, so the
    /// answer pins the participant list and our own state.
    JoinVoice {
        guild_id: String,
        channel_id: String,
        self_mute: bool,
        self_deaf: bool,
    },
    /// Leave whatever voice channel we are in (op 4 with a null channel).
    LeaveVoice { guild_id: String, self_mute: bool, self_deaf: bool },
    /// Change mic/deafen flags without leaving the channel.
    VoiceFlags {
        guild_id: String,
        channel_id: String,
        self_mute: bool,
        self_deaf: bool,
    },
    /// Tell Discord the channel was read up to `message_id`. Best-effort: the
    /// local unread badge is cleared regardless of the reply.
    AckMessage { channel_id: String, message_id: String },
    /// Subscribe to a guild (op 14) so Discord sends non-stateful events
    /// (MESSAGE_CREATE, TYPING_START, ...) for large guilds. Without it the
    /// gateway silently withholds new messages while REST history still works.
    SubscribeGuild { guild_id: String, channel_id: String },
    OpenDM { user_id: String },
    Shutdown,
}
