use crate::models::{ChatChannel, ChatMessage, Guild, UserProfile};

#[derive(Clone, Debug)]
pub(crate) enum ToApp {
    Ready { username: String, user_id: String, avatar: Option<String> },
    Message(ChatMessage),
    History { channel_id: String, messages: Vec<ChatMessage> },
    Guild(Guild),
    Channel(ChatChannel),
    GuildChannels { guild_id: String, channels: Vec<ChatChannel> },
    DMChannel(ChatChannel),
    Friends(Vec<UserProfile>),
    Status(String),
    Debug(String),
}

#[derive(Debug)]
pub(crate) enum ToGateway {
    Send { channel_id: String, content: String },
    FetchHistory { channel_id: String },
    OpenDM { user_id: String },
    Shutdown,
}
