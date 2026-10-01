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
    /// Страница истории не пришла: сеть, 403, 500 — после трёх попыток.
    ///
    /// Без этого события приложение ждёт ответа, которого не будет, и
    /// спиннер «Loading messages…» (или «Loading older messages…») горит
    /// бесконечно, а канал становится нечитаемым: догрузка вверх больше не
    /// работает никогда. `before` — `None` для первой страницы, `Some` для
    /// догрузки вверх: снять надо именно тот флаг, который ждёт ответа.
    HistoryFailed { channel_id: String, before: Option<String>, reason: String },
    Guild(Guild),
    Channel(ChatChannel),
    GuildChannels { guild_id: String, channels: Vec<ChatChannel> },
    DMChannel(ChatChannel),
    Friends(Vec<UserProfile>),
    Status(String),
    /// Discord отказал в самом токене (код закрытия 4004/4007/4013/4014).
    /// Повторное подключение тут бессмысленно: тот же токен снова получит
    /// отказ, а каждые 3 секунды долбить в Discord — это ещё и rate limit.
    ///
    /// Событие нужно, чтобы вернуть клиент на экран входа: `connected`
    /// сбрасывается только в `switch_account`, а гейтвей сам вернуться не
    /// может. Без этого ввести неверный токен — значит навсегда остаться в
    /// экране чата, из которого нечем выйти.
    AuthFailed { reason: String },
    /// Отправка не удалась: Discord отказал (403, 400, 429) или запрос не
    /// дошёл. Раньше это уходило только в отладочный лог, и сообщение навсегда
    /// оставалось в чате с пустым служебным id — выглядело как отправленное,
    /// хотя его никто не видел, а текст из поля ввода уже очистился.
    ///
    /// `local_id` — какое именно эхо убрать, `reason` — что показать
    /// пользователю (без ответа Discord и его кодов).
    SendFailed { channel_id: String, local_id: String, reason: String },
    Debug(String),
}

#[derive(Debug)]
pub(crate) enum ToGateway {
    /// Отправить сообщение. `local_id` — id нашего локального эха: по нему
    /// при неудаче сообщение убирается из чата, иначе оно осталось бы там
    /// навсегда, выглядя как отправленное.
    Send { channel_id: String, content: String, local_id: String },
    /// Страница истории. `before` — самый старый id, который уже показан:
    /// `None` для первой страницы, дальше по мере прокрутки вверх.
    FetchHistory { channel_id: String, before: Option<String> },
    OpenDM { user_id: String },
    Shutdown,
}
