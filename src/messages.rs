use crate::models::{ChatChannel, ChatMessage, Guild, UserProfile};

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
    Guild(Guild),
    Channel(ChatChannel),
    GuildChannels { guild_id: String, channels: Vec<ChatChannel> },
    DMChannel(ChatChannel),
    Friends(Vec<UserProfile>),
    Status(String),
    /// Discord rejected the token (close codes 4004/4007/4013/4014);
    /// reconnecting is pointless. Returns the client to the login screen,
    /// which the gateway cannot do itself.
    AuthFailed { reason: String },
    /// Send failed: Discord refused (403, 400, 429) or the request never
    /// arrived. Removes the optimistic echo by `local_id` and shows `reason`.
    SendFailed { channel_id: String, local_id: String, reason: String },
    Debug(String),
}

#[derive(Debug)]
pub(crate) enum ToGateway {
    /// Send a message. `local_id` identifies the optimistic echo to remove on
    /// failure.
    Send { channel_id: String, content: String, local_id: String },
    /// History page. `before` is the oldest shown id: `None` for the first
    /// page, otherwise paging upward.
    FetchHistory { channel_id: String, before: Option<String> },
    OpenDM { user_id: String },
    Shutdown,
}
