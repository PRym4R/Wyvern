use crate::models::{ChatChannel, ChatMessage, Guild, UserProfile};

#[derive(Clone, Debug)]
pub(crate) enum ToApp {
    Ready { username: String, user_id: String, avatar: Option<String> },
    Message(ChatMessage),
    /// Первая страница истории канала: она заменяет то, что уже есть.
    /// `more` — есть ли что подгружать вверх при прокрутке.
    ///
    /// Страница приходит в том порядке, в каком её отдал Discord: **от новых
    /// к старым**. `App::apply_history` разворачивает её, потому что список
    /// должен идти от старых к новым: от первой строки берётся `before` для
    /// догрузки вверх, а потолок выбрасывает самые старые.
    History { channel_id: String, messages: Vec<ChatMessage>, more: bool },
    /// Догрузка более старых сообщений (прокрутка вверх): вставляются в
    /// начало списка, а не заменяют его. Порядок тот же: от новых к старым.
    HistoryMore { channel_id: String, messages: Vec<ChatMessage>, more: bool },
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
    /// Страница истории. `before` — самый старый id, который уже показан:
    /// `None` для первой страницы, дальше по мере прокрутки вверх.
    FetchHistory { channel_id: String, before: Option<String> },
    OpenDM { user_id: String },
    Shutdown,
}
