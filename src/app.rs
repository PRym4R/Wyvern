use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, TextureHandle};
use tokio::sync::mpsc;

use crate::gateway::{run_gateway, EventTx, Generation};
use crate::media::AvatarFetch;
use crate::messages::{ToApp, ToGateway};
use crate::models::{
    BoundedCache, ChatChannel, ChatMessage, Guild, ImagePayload, LoadedImage, StoredAccount,
    Theme, UserProfile,
};

/// Больше этого сообщений на канал не держим в памяти. История теперь
/// подгружается вверх по мере прокрутки, поэтому потолок нужен обязательно:
/// без него длинный канал растёт бесконечно, и держать в памяти сотни
/// сообщений, которых не видно, смысла нет.
pub(crate) const MAX_MESSAGES_PER_CHANNEL: usize = 500;
/// Сколько текстур аватаров/иконок guild'ов держим.
const MAX_AVATAR_CACHE: usize = 192;
/// Сколько картинок-вложений держим (каждая — это мегабайты VRAM/RAM).
const MAX_IMAGE_CACHE: usize = 32;
/// Сколько байт может держать кэш картинок. Ограничение по числу картинок
/// ничего не значит: одна фотка на 1536 пикселей — это 9 МБ, а тридцать
/// таких — это 280 МБ, и столько памяти клиенту не нужно: в чате картинка
/// рисуется максимум 360x360.
const IMAGE_CACHE_BUDGET: usize = 48 * 1024 * 1024;
/// Потолок памяти под аватары: при 64x64 это 192 * 16 КБ = 3 МБ.
const AVATAR_CACHE_BUDGET: usize = 8 * 1024 * 1024;
/// Сколько неудачных URL'ов запоминаем, чтобы не качать их снова.
pub(crate) const MAX_FAILED_IMAGES: usize = 512;

/// Нужно ли писать отладочный лог в файл и stderr.
///
/// По умолчанию — нет. Отладочные строки идут по нескольку раз на кадр, и
/// без флага журнал превращался в поток дискового I/O на 20 Гц, который вдобавок
/// сам себя переписывал каждые ~100 секунд, — то есть не мог дожить до конца
/// разбора бага, ради которого его писали. Включается `WYVERN_DEBUG=1`.
fn debug_to_disk_from_env() -> bool {
    matches!(
        std::env::var("WYVERN_DEBUG").ok().as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Оставляем не больше N сообщений: иначе активный канал в шумном чате
/// раздувает память бесконечно. Список идёт от старых к новым, и какой конец
/// резать — зависит от того, куда смотрит пользователь.
///
/// `keep_newest` — смотрим на новые сообщения (пришло своё или первая
/// страница канала): тогда выбрасываются самые старые, до которых человек
/// всё равно не доберётся.
///
/// `keep_newest = false` — смотрим вверх, на догруженную историю. Раньше
/// обрезка шла от начала списка в обоих случаях, и страница, только что
/// загруженная по прокрутке вверх, тут же выбрасывалась: список и так был
/// полон, и верх истории молча упирался в стену (Б-7). Теперь на место
/// пришедшей страницы уходят самые новые — те, к чему человек поднялся
/// не ради.
///
/// Сколько ушло новых, возвращаем: об этом надо сказать пользователю, иначе
/// сообщения просто пропадают из вида.
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
    /// Хранилище оказалось старого, открытого формата: пароля в нём нет, и
    /// «подошёл любой» — не проверка. Пока флаг стоит, файл не перезаписывается:
    /// иначе опечатка в пароле молча закрыла бы хранилище не тем паролем.
    pub(crate) vault_legacy: bool,
    pub(crate) login_password: String,
    /// Не-ошибка на экране входа («пароль принят после обрезки пробелов»).
    pub(crate) login_notice: String,
    /// Путь к файлу хранилища, если он отличается от `~/.wyvern_accounts.json`.
    /// Нужен тестам: они идут параллельно, и общий env-переопределитель
    /// приводил к записи в настоящий файл.
    pub(crate) vault_path_override: Option<std::path::PathBuf>,
    pub(crate) login_selected: Option<String>,
    pub(crate) remember_account: bool,
    pub(crate) saved_accounts: Vec<StoredAccount>,
    pub(crate) active_index: Option<usize>,
    pub(crate) status: String,
    /// Не отправилось последнее сообщение: почему. Раньше отказ жил только в
    /// `status`, а тот рисуется на экране входа — то есть в чате о неудаче
    /// не говорилось вообще ничего, и сообщение просто исчезало.
    pub(crate) send_error: Option<String>,
    /// Последние строки отладочного журнала. `VecDeque`, а не `Vec`: при
    /// вытеснении самой старой строки `remove(0)` сдвигал весь вектор (до
    /// сотни элементов) на каждой новой строке, а строк бывает и по десятку
    /// на кадр.
    pub(crate) debug_log: VecDeque<String>,
    /// Писать ли отладочный лог на диск и в stderr. См. `debug_to_disk_from_env`.
    pub(crate) debug_to_disk: bool,
    /// Сколько сильных ссылок на сообщение видел последний нарисованный кадр.
    /// Пробник для теста: копия списка на кадр удваивала бы счётчик, а «взять
    /// список из карты на время кадра» — нет. Только для тестов.
    #[cfg(test)]
    pub(crate) probe_msg_refs: usize,
    pub(crate) to_gw: Option<mpsc::UnboundedSender<ToGateway>>,
    pub(crate) from_gw: mpsc::UnboundedReceiver<ToApp>,
    pub(crate) gw_started: bool,
    /// Поколение гейтвея: см. `Generation` в gateway.rs. Переключение
    /// аккаунта поднимает его на единицу, и поток прежнего аккаунта
    /// умолкает, а не перетирает состояние нового.
    pub(crate) gateway_generation: Arc<Generation>,
    pub(crate) avatar_cache: BoundedCache<TextureHandle>,
    pub(crate) pending_avatars: HashMap<String, std::sync::mpsc::Receiver<AvatarFetch>>,
    /// Аватары и иконки, которые не загрузились. Раньше такого списка не
    /// было вовсе, и отказ означал новый запрос на каждом кадре: 20 запросов
    /// в секунду на каждый невидимый аватар. Эти самые запросы съедали лимит
    /// CDN, на который клиент и упирался, — и порождали новую волну отказов.
    pub(crate) failed_avatars: HashSet<String>,
    pub(crate) image_cache: BoundedCache<LoadedImage>,
    pub(crate) pending_images: HashMap<String, std::sync::mpsc::Receiver<Option<ImagePayload>>>,
    pub(crate) failed_images: HashSet<String>,
    pub(crate) theme: Theme,
    pub(crate) show_friends: bool,
    pub(crate) friends: Vec<UserProfile>,
    pub(crate) history_loading: Option<String>,
    /// Идёт ли догрузка более старых сообщений (прокрутка вверх). Отдельно
    /// от `history_loading`, который означает «канал открыт, первой страницы
    /// ещё нет»: подгрузка вверх идёт уже по открытому каналу.
    pub(crate) history_loading_more: bool,
    /// Старше показанного в канале ничего нет — дошли до начала. Дальше по
    /// прокрутке вверх не просим, иначе клиент будет снова и снова бить в
    /// API за страницей, которой не существует.
    pub(crate) history_exhausted: bool,
    /// Сколько самых новых сообщений пришлось выбросить, чтобы втиснуть
    /// догруженную вверх страницу в потолок по памяти. Показывается в чате:
    /// иначе сообщения пропадают из вида молча, а верх истории выглядит так,
    /// будто он кончился.
    pub(crate) trimmed_newest: usize,
    /// Отладочная команда, ждущая подтверждения: канал, который добавит
    /// повторный ввод. `None` — ничего не ждём.
    pub(crate) pending_debug_add: Option<(String, std::time::Instant)>,
    /// Ширина панели каналов в прошлом кадре. Нужна, чтобы писать о её
    /// изменении в лог по одному разу, а не двадцать раз в секунду.
    pub(crate) channel_panel_w: f32,
    /// Последняя неудача при загрузке истории: для какого канала и почему.
    /// Пока строка стоит, канал не должен ни крутить бесконечный спиннер, ни
    /// молча выглядеть пустым — пользователь обязан видеть, что история не
    /// пришла, иначе канал выглядит как сломанный.
    pub(crate) history_error: Option<(String, String)>,
    /// Прокрутка чата в самом низу. По этому признаку новое сообщение
    /// прокручивает чат вниз, а читающего историю выше не выбрасывает.
    pub(crate) chat_at_bottom: bool,
    /// Измеренная высота сообщений по их id. Нужна виртуализации: без неё
    /// неизвестно, какие сообщения попадают в окно, а рисовать все — это
    /// десятки тысяч аллокаций на кадр. Ключ — id, а не индекс, поэтому
    /// подгрузка истории в начало списка кэш не портит.
    pub(crate) msg_heights: HashMap<String, f32>,
    /// Префиксные суммы высот: буфер кадра, переиспользуется между кадрами.
    pub(crate) msg_offsets: Vec<f32>,
    /// Счётчик для id неподтверждённых собственных сообщений. Он общий на всё
    /// приложение, а не на канал: id должен быть уникален в пределах сеанса,
    /// чтобы эхо находилось однозначно.
    pub(crate) next_local_id: u64,
    /// Ширина содержимого чата с прошлого кадра — от неё зависят переносы
    /// строк, а значит и высоты сообщений.
    pub(crate) msg_width: f32,
    /// Высота окна чата с прошлого кадра. Нужна, чтобы открыть список сразу
    /// на самом низу: egui узнаёт высоту содержимого только после отрисовки,
    /// а перемотать вниз нужно до неё.
    pub(crate) chat_inner_h: f32,
    /// Настоящая высота содержимого списка с прошлого кадра и та, которую мы
    /// для него запросили. Разница — ошибка оценки высот сообщений, а на
    /// длинном списке она доходит до десятков пикселей. Без неё низ
    /// просился бы выше последнего сообщения, и чат переставал считать себя
    /// внизу: новое сообщение его уже не тянуло.
    pub(crate) chat_content_h: f32,
    pub(crate) chat_est_h: f32,
    /// Прокрутка чата: зеркало того, чем сейчас открыт скролл.
    pub(crate) chat_offset_y: f32,
    /// На чём держится вид: id сообщения у верхней границы окна и на сколько
    /// оно выше неё. Когда список меняется (подгрузилась история вверх, у
    /// сообщения уточнилась высота), это сообщение остаётся на месте.
    pub(crate) chat_anchor: Option<(String, f32)>,
    /// Просить ещё более старые сообщения — выставляется при отрисовке.
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
            login_selected: None,
            remember_account: true,
            saved_accounts: Vec::new(),
            active_index: None,
            status: String::new(),
            send_error: None,
            debug_log: VecDeque::new(),
            debug_to_disk: debug_to_disk_from_env(),
            #[cfg(test)]
            probe_msg_refs: 0,
            to_gw: None,
            from_gw,
            gw_started: false,
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
        // На диск и в stderr — только по WYVERN_DEBUG. Без флага отладочный
        // лог не должен стоить ничего: строки идут по нескольку раз на кадр,
        // и это был поток I/O на 20 Гц, из-за которого файл не доживал до
        // конца разбора (см. `debug_to_disk_from_env`).
        //
        // В памяти держим всегда: на него смотрят тесты и экран отладки.
        if self.debug_to_disk {
            let line = format!("[GW] {}", msg);
            eprintln!("{}", line);
            use std::io::Write;
            // Лог не должен расти вечно: каждые 15 кадров в него падает строка,
            // за сутки это десятки мегабайт. Переезжаем на .1 и начинаем заново.
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
                // Живое сообщение из канала, который сейчас не открыт, брать
                // некуда. Discord шлёт MESSAGE_CREATE по всем каналам сразу, а
                // список на экране только один, и раньше строка ложилась в
                // список любого канала: их никто не читал, они копились до
                // потолка на каждый активный канал (на шумном сервере — десятки
                // мегабайт), а сообщение из соседнего канала ещё и выставляло
                // `scroll_to_bottom` — открытый чат дёргался вниз, хотя в нём
                // ничего не появилось. Терять нечего: `open_channel` при
                // переключении всё равно перечитывает историю заново.
                ToApp::Message(_msg) if self.current_channel_id() != Some(_msg.channel_id.as_str()) => {
                }
                ToApp::Message(msg) => {
                    let cid = msg.channel_id.clone();
                    // Сообщение может прийти дважды: гейтвей шлёт живую копию
                    // всего, что появилось после подписки, а та же строка уже
                    // успела попасть в страницу истории. Дубль в списке рисуется
                    // дважды, и в чате это видно как «сообщение скопировалось».
                    let duplicate = !msg.id.is_empty()
                        && self.messages.get(&cid).is_some_and(|e| {
                            // Ищем с хвоста: живое сообщение почти всегда там,
                            // а список не длиннее MAX_MESSAGES_PER_CHANNEL,
                            // так что просмотр целиком ничего не стоит.
                            e.iter().rev().any(|m| m.id == msg.id)
                        });
                    if duplicate {
                        self.push_debug(format!("Duplicate live message {} ignored", msg.id));
                    } else {
                        // Наше собственное сообщение мы показали сами, ещё до
                        // ответа Discord, под заведомо ненастоящим id. Пришедшее
                        // подтверждение занимает его место — иначе в чате
                        // оказывались две копии одного и того же текста.
                        let replaced = self.replace_local_echo(&cid, &msg);
                        if !replaced {
                            let entry = self.messages.entry(cid).or_default();
                            entry.push(Arc::new(msg));
                            // Своё сообщение пришло, когда пользователь смотрит
                            // на новые: место уходит самым старым.
                            trim_messages(entry, true);
                        }
                        // Внизу ли пользователь — решаем по прошлому кадру: если
                        // он читает историю выше, новое сообщение не должно
                        // выбрасывать его в самый конец.
                        if self.chat_at_bottom {
                            self.scroll_to_bottom = true;
                        }
                    }
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
                ToApp::Friends(list) => {
                    self.friends = list;
                    self.push_debug(format!("Loaded {} friends", self.friends.len()));
                }
                ToApp::Status(s) => {
                    // Статус соединения — это состояние, а не ошибка, но он
                    // же должен означать, что мы сейчас не онлайн. Раньше
                    // `connected` гасился только при выходе из аккаунта, и
                    // после обрыва кнопка «я онлайн» продолжала гореть, а
                    // экран входа не возвращался.
                    if s.starts_with("Reconnecting") || s == "Disconnected" {
                        self.connected = false;
                    }
                    self.status = s;
                }
                ToApp::SendFailed { channel_id, local_id, reason } => {
                    // Отправка не вышла: эхо убираем, иначе оно навсегда
                    // осталось бы в списке и выглядело как отправленное
                    // сообщение (Б-9), а текст вернуть в поле человек уже не
                    // мог: Enter очистил его до того, как пришёл ответ.
                    let mut taken_back: Option<String> = None;
                    if let Some(entry) = self.messages.get_mut(&channel_id) {
                        if let Some(pos) = entry.iter().rposition(|m| m.id == local_id) {
                            taken_back = Some(entry.remove(pos).content.clone());
                            // Список уменьшился — измеренные высоты и якорь к
                            // нему больше не относятся. Если не сбросить, высота
                            // продолжит считаться от удалённого сообщения и
                            // чат дёрнется.
                            self.msg_heights.clear();
                            self.msg_offsets.clear();
                            self.chat_anchor = None;
                            self.scroll_to_bottom = true;
                        }
                    }
                    // Возвращаем текст в поле, если пользователь в этом канале и
                    // ещё не начал писать новое: иначе наш старый текст затёр бы
                    // то, что он набирает прямо сейчас.
                    if self.current_channel_id() == Some(channel_id.as_str()) && self.input.trim().is_empty() {
                        self.input = taken_back.unwrap_or_default();
                    }
                    self.send_error = Some(reason);
                    // Обрезка по символам: id служебный, но вставляют его мы
                    // сами, а не Discord (см. маску токена).
                    let short: String = local_id.chars().take(14).collect();
                    self.push_debug(format!("Send failed for {}", short));
                }
                ToApp::AuthFailed { reason } => {
                    // Discord отклонил токен. Возвращаемся на экран входа:
                    // `connected` и `gw_started` больше не сбросят нигде, а
                    // без этого неверный токен оставлял клиент в экране чата,
                    // из которого нечем выйти (кнопки выхода нет ни в одном
                    // меню). Токен в поле оставляем — его надо исправить.
                    self.connected = false;
                    self.gw_started = false;
                    self.to_gw = None;
                    self.push_debug(format!("Auth failed: {}", reason));
                    self.status = reason;
                }
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
        // Каждый запуск гейтвея получает своё поколение. Прежний поток ещё
        // какое-то время жив (Shutdown кладётся в очередь, а он может спать
        // между попытками), и без поколения его события перетирали бы
        // состояние нового: сразу после переключения аккаунта в шапке на
        // секунду всплывало бы имя прежнего пользователя, а на Discord висели
        // бы две сессии.
        let generation = self.gateway_generation.next();
        let event_tx = EventTx::new(from_gw_tx, generation, self.gateway_generation.clone());

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
                // Раньше вход всё равно продолжался, а это сообщение жило
                // один кадр: гейтвей тут же слал «Connecting…» и затирал его.
                // Пользователь видел мелькнувшую надпись и больше ничего — ни
                // входа, ни объяснения, почему аккаунт не сохранился. Лучше
                // остановиться и сказать, что делать.
                self.status =
                    "Чтобы запомнить аккаунт, введи пароль хранилища (или сними галочку «Запомнить»)"
                        .to_string();
                return;
            }
            match self.unlock_vault(&pw) {
                Ok(Some(hint)) => self.login_notice = hint,
                Ok(None) => {}
                // Хранилище не открылось. Продолжить вход молча значило бы
                // потерять аккаунт, не сказав об этом ни слова: сообщение
                // пережил бы тот же один кадр.
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
    /// Открыть канал. Первая страница истории всё равно грузится заново,
    /// поэтому сообщения других каналов можно выбросить — память не растёт
    /// при переключении.
    pub(crate) fn open_channel(&mut self, channel_id: &str) {
        self.history_loading = Some(channel_id.to_string());
        self.history_loading_more = false;
        self.history_exhausted = false;
        // Прежняя неудача показывалась для этого же канала — при новой
        // попытке она больше не актуальна.
        self.history_error = None;
        self.trimmed_newest = 0;
        self.messages.retain(|k, _| k == channel_id);
        // Незабранные загрузки прежнего канала больше никто не заберёт: к
        // моменту переключения они уже лежат в канале с готовыми
        // декодированными пикселями (до 4 МБ на картинку) — и так и висели
        // бы до конца сессии. Сбрасываем получателей, и отправитель сразу
        // отпускает память. Аватары и иконки тоже: в новом канале они всё
        // равно перезапросятся.
        self.pending_images.clear();
        self.pending_avatars.clear();
        self.msg_heights.clear();
        self.send_cmd(ToGateway::FetchHistory { channel_id: channel_id.to_string(), before: None });
    }
    /// Положить страницу истории в список канала.
    ///
    /// Первая страница заменяет содержимое, догрузка вверх (`prepend`)
    /// вставляется в начало. Спиннеры и прокрутка трогаются только у того
    /// канала, который открыт сейчас: поздний ответ по уже закрытому каналу
    /// не должен снимать «Loading messages…» у того, который грузится сейчас.
    fn apply_history(&mut self, channel_id: &str, incoming: Vec<ChatMessage>, more: bool, prepend: bool) {
        // Discord отдаёт страницу от новых к старым, а список должен идти от
        // старых к новым. Только тогда первая строка списка — самая старая, и
        // от неё берётся `before` для догрузки вверх, а потолок выбрасывает
        // самые старые, а не самые новые.
        //
        // Без разворота `before` уходил от самого нового сообщения, Discord
        // присылал ту же страницу ещё раз, и каждая догрузка вверх подставляла
        // её в начало: чат рос копиями одних и тех же сообщений, и прокрутка
        // вверх циклично показывала одно и то же.
        let mut incoming = incoming;
        incoming.reverse();
        // Догрузка вверх, пришедшая в пустой список, — это страница из
        // середины истории, а не содержимое канала. Сценарий: канал открыт,
        // ушла догрузка вверх, пользователь ушёл и вернулся (`open_channel`
        // чистит список), а поздний ответ ложится в пустое. Канал показывал бы
        // 50 сообщений из середины без новых, и восстановиться можно было бы
        // только перезаходом — молча.
        if prepend && !self.messages.contains_key(channel_id) {
            let cid_short: String = channel_id.chars().take(14).collect();
            self.push_debug(format!(
                "Dropping stale older page for {}: list was cleared, asking first page again",
                cid_short
            ));
            // Просим первую страницу заново: с неё список и должен начинаться.
            // Флаги не трогаем — спиннер первой страницы ещё должен гореть.
            self.send_cmd(ToGateway::FetchHistory { channel_id: channel_id.to_string(), before: None });
            return;
        }
        let added = {
            let entry = self.messages.entry(channel_id.to_string()).or_default();
            if prepend {
                // `before` у Discord строгий, но на стыке страниц крайнее
                // сообщение приходит двумя копиями: убираем лишнюю, иначе
                // верх списка зарос бы повторами.
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
        // Потолок. С какой стороны резать — см. `trim_messages`: при своём
        // сообщении или первой странице выбрасываются самые старые, при
        // догрузке вверх — самые новые, потому что пользователь поднялся
        // именно к старым.
        let (stored, dropped_newest) = {
            let entry = self.messages.get_mut(channel_id).expect("страницу только что положили");
            let dropped = trim_messages(entry, !prepend);
            (entry.len(), dropped)
        };
        // Первую страницу канала грузим целиком заново, поэтому счётчик сбросим:
        // он относится к прежнему окну истории.
        if !prepend {
            self.trimmed_newest = 0;
        }
        if dropped_newest > 0 {
            self.trimmed_newest = dropped_newest;
        }
        // Дамп сообщений — отладочная вещь, пишется только когда явно
        // попросили переменной окружения: на каждый выбор канала он собирал
        // строку на полэкрана текста. По догрузке вверх не пишем: там он
        // перезаписывал бы файл на каждой странице.
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
            // Догружать больше некуда либо потому, что Discord сказал «дальше
            // пусто», либо потому, что сообщений уже потолок.
            self.history_exhausted = !more || stored >= MAX_MESSAGES_PER_CHANNEL;
            if !prepend {
                self.scroll_to_bottom = true;
                // Список переставлен целиком — измеренные высоты к нему больше
                // не относятся.
                self.msg_heights.clear();
            }
        }
        // Обрезка по символам: по байтам не-ASCII id упал бы (см. Б-6).
        let cid_short: String = channel_id.chars().take(14).collect();
        self.push_debug(format!("Stored {} msgs ({} new) for channel {}{}", stored, added, cid_short,
            if for_current { "" } else { " (не текущий канал)" }));
    }
    /// Страница истории не пришла: снять ожидание, показать почему.
    ///
    /// Раньше на этот случай не было события вовсе, и тот, кто ждал ответа,
    /// ждал вечно: одна сетевая ошибка или один 403 оставляли канал с
    /// бесконечным спиннером, а догрузка вверх не работала больше никогда —
    /// она проверяет те же флаги.
    ///
    /// `before` говорит, чьего ответа мы ждали: `None` — первой страницы,
    /// `Some` — догрузки вверх. Снять надо именно тот флаг: неудача по чужому
    /// каналу не должна трогать текущий.
    fn history_failed(&mut self, channel_id: &str, before: Option<&str>, reason: &str) {
        if before.is_none() {
            // Ждал ли кто-то первую страницу именно этого канала? Поздний
            // ответ по уже закрытому каналу трогать нечего.
            if self.history_loading.as_deref() == Some(channel_id) {
                self.history_loading = None;
            }
        } else {
            self.history_loading_more = false;
        }
        // Больше не долбим в API: 403 не лечится сам, а при обрыве сети
        // пользователь откроет канал заново и получит свежую попытку.
        if self.current_channel_id() == Some(channel_id) {
            self.history_exhausted = true;
            self.history_error = Some((channel_id.to_string(), reason.to_string()));
            self.scroll_to_bottom = true;
        }
        // Имя канала в отладочной строке обрезаем по символам, а не по
        // байтам: срез по байтам у не-ASCII id упал бы с той же ошибкой, что
        // и маска токена (Б-6).
        let cid_short: String = channel_id.chars().take(14).collect();
        self.push_debug(format!("History failed for {} ({}): {}", cid_short, before.is_some(), reason));
    }
    /// Догрузить более старые сообщения: просим страницу от самой старой
    /// строки, что уже есть в канале. Это именно первая строка списка — список
    /// идёт от старых к новым (см. `apply_history`); если бы порядок был
    /// другим, `before` ушёл бы от самого нового сообщения, Discord вернул бы
    /// ту же страницу, и чат пошёл бы по кругу. Пока предыдущая страница в
    /// пути, повторно не спрашиваем — иначе один скролл вверх даст пачку
    /// одинаковых запросов.
    ///
    /// Наше собственное неподтверждённое сообщение пропускаем: его id ненастоящий
    /// и в переписке Discord такого нет.
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
            // В списке нет ни одного настоящего сообщения — продолжать историю
            // не от чего. Свое неподтверждённое в счёт не идёт: его id в
            // переписке Discord не существует.
            self.history_exhausted = true;
            return;
        };
        self.history_loading_more = true;
        self.send_cmd(ToGateway::FetchHistory { channel_id: cid, before: Some(oldest) });
    }
    /// Пришло подтверждение нашей отправки: занять им место локального эха.
    ///
    /// Мы показываем своё сообщение сразу, не дожидаясь Discord, и подставляем
    /// ему заведомо ненастоящий id. Если бы мы просто добавили присланное
    /// сообщение, в чате оказались бы две копии одного текста: у эха id пустой
    /// либо служебный, а у ответа настоящий, и проверка дубля их не видела.
    ///
    /// Возвращает `true`, если эхо нашлось и было заменено.
    fn replace_local_echo(&mut self, channel_id: &str, msg: &ChatMessage) -> bool {
        // Подтверждать отправку может только наше собственное сообщение.
        if self.user_id.is_empty() || msg.author_id != self.user_id {
            return false;
        }
        let Some(entry) = self.messages.get_mut(channel_id) else { return false };
        // Ищем с хвоста: эхо добавляется в конец, и подтверждения приходят в том
        // же порядке. Текст сравниваем — Discord его на отправке не меняет.
        // Если эха нет (например, список перезагрузили историей), сообщение
        // добавится как обычное, и это правильно.
        match entry.iter().rposition(|m| m.is_local_echo() && m.content == msg.content) {
            Some(idx) => {
                entry[idx] = Arc::new(msg.clone());
                true
            }
            None => false,
        }
    }
    /// Id канала, который открыт сейчас. Состояние истории и чата относятся
    /// именно к нему.
    pub(crate) fn current_channel_id(&self) -> Option<&str> {
        let ch = self.channels.get(self.selected_channel?)?;
        Some(ch.id.as_str())
    }
    /// Имя для показа. Возвращаем ссылку на строку самого сообщения: раньше
    /// здесь на каждом кадре клонировалось имя каждого видимого сообщения.
    pub(crate) fn display_name<'a>(&self, msg: &'a ChatMessage) -> &'a str {
        msg.nickname.as_deref().unwrap_or(msg.author_name.as_str())
    }
    /// Текст для показа: контент, иначе описание вложения или эмбеда.
    /// Тоже ссылка, без копии на кадр.
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
    /// «12:34» из метки времени. Срез исходной строки, копий не делает.
    pub(crate) fn short_time<'a>(&self, iso: &'a str) -> &'a str {
        let t = iso.trim_start_matches('T');
        if t.len() >= 16 {
            &t[11..16]
        } else {
            iso
        }
    }
    /// Забирает сообщения открытого канала на время кадра.
    ///
    /// Рисовать кадр нужно по владеющему `Vec`: иначе `self` занят на всё
    /// время отрисовки, и `&mut`-методы (`draw_message`, кэш высот) из цикла не
    /// позвать. Раньше ради этого делали `.cloned()` всего списка — на каждом
    /// кадре, то есть копия всех `Arc` канала двадцать раз в секунду. Вместо
    /// копии список *уезжает* из карты: на месте ключа остаётся пустой `Vec`,
    /// поэтому `contains_key` не меняется, а сам список возвращает
    /// `restore_channel_messages` в конце кадра.
    ///
    /// Возвращаем и id канала: по нему список кладётся обратно.
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

    /// Кладёт на место список, забранный `take_channel_messages`.
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
/// Экран входа вместо чата?
    ///
    /// Отдельный метод, а не условие прямо в `update`: от того, сюда ли мы
    /// попадём, зависит, сможет ли пользователь выйти из сломанного входа
    /// обратно, и это стоит проверять тестом, а не глазами.
    pub(crate) fn shows_login(&self) -> bool {
        self.token_input.is_empty() || (!self.connected && !self.gw_started)
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

        if self.shows_login() {
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

    /// Изолированный файл хранилища на время теста. Важно: переопределение
    /// живёт в конкретном экземпляре App, а не в переменной окружения —
    /// тесты идут параллельно, и общий env приводил к тому, что тест
    /// перезаписывал настоящий ~/.wyvern_accounts.json.
    fn vaulted_app(tag: &str) -> (App, std::path::PathBuf) {
        let mut p = std::env::temp_dir();
        p.push(format!("wyvern-test-{}-{}.json", tag, std::process::id()));
        let _ = std::fs::remove_file(&p);
        let mut app = App::new(mpsc::unbounded_channel().1);
        app.vault_path_override = Some(p.clone());
        (app, p)
    }

    /// Тесты хранилища не должны трогать настоящий файл пользователя.
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

    /// Пробелы в пароле — самая частая причина «неверного пароля».
    #[test]
    fn vault_tolerates_password_spaces() {
        // Перебор вариантов пароля считает сотни тысяч итераций PBKDF2, а
        // измеряющие время тесты ходят параллельно: без замка они мешали бы
        // друг другу.
        let _guard = crate::crypto::VAULT_COST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for saved in ["hunter2", "hunter2 ", " hunter2", "hunter2\n"] {
            let (mut app, tmp) = vaulted_app("space");
            app.saved_accounts =
                vec![StoredAccount { token: "tok-space".into(), username: "bob".into() }];
            app.save_accounts(saved);
            assert!(tmp.exists(), "файл не записался");

            // Вводим без пробелов — должно открыться (другой экземпляр App
            // смотрит в тот же файл, как это делает перезапуск клиента).
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

    /// Хранилище без аккаунтов — это не «неверный пароль».
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

    /// Весь путь пользователя: вошёл по токену с паролем → закрыл клиент →
    /// снова открыл → ввёл пароль → кликнул аккаунт → вошёл.
    /// Ровно тот сценарий, на котором клиент раньше говорил «неверный пароль».
    #[test]
    fn full_cycle_save_restart_unlock_and_login() {
        let (mut app, tmp) = vaulted_app("cycle");
        let token = "MTIz.тест.токен".to_string();
        let pw = "мой-пароль";

        // 1. Открыли хранилище паролем и сохранили аккаунт.
        app.unlock_vault(pw).expect("первый вход создаёт хранилище");
        app.add_saved_account(&token, "мой_юзер");
        assert!(tmp.exists(), "файл хранилища не создан");
        assert_eq!(app.saved_accounts.len(), 1);
        assert_eq!(app.account_label(&app.saved_accounts[0]), "мой_юзер");

        // 2. «Перезапуск клиента»: новый экземпляр, ничего не помнит.
        let mut app2 = App::new(mpsc::unbounded_channel().1);
        app2.vault_path_override = Some(tmp.clone());
        assert!(app2.saved_accounts.is_empty(), "после перезапуска список пуст");
        assert!(!app2.accounts_unlocked, "хранилище закрыто");
        assert!(app2.login_password.is_empty());

        // 3. Ввели пароль хранилища.
        let hint = app2.unlock_vault(pw).expect("пароль должен подойти после перезапуска");
        assert!(hint.is_none(), "точный пароль не должен давать подсказку");
        assert_eq!(app2.saved_accounts.len(), 1, "аккаунт не подгрузился");
        assert_eq!(app2.saved_accounts[0].token, token);
        assert!(app2.accounts_unlocked);

        // 4. Кликнули по аккаунту в нижней ленте.
        app2.select_account(token.clone());
        assert_eq!(app2.login_selected.as_deref(), Some(token.as_str()));
        assert_eq!(app2.login_password, pw, "раз уже открыто — пароль подставляется");

        // 5. Нажали «Войти»: токен пошёл в гейтвей, выбор сброшен.
        app2.login_with_password(&token);
        assert_eq!(app2.token_input, token, "вход должен выбрать аккаунт");
        assert!(app2.gw_started, "гейтвей должен стартовать");
        assert!(app2.login_selected.is_none(), "после входа выбор сброшен");
        assert!(app2.login_password.is_empty(), "пароль из поля должен очищаться");

        // 6. Хранилище на диске не пострадало от входа.
        let mut app3 = App::new(mpsc::unbounded_channel().1);
        app3.vault_path_override = Some(tmp.clone());
        assert!(app3.unlock_vault(pw).is_ok(), "файл должен остаться читаемым");
        assert_eq!(app3.saved_accounts.len(), 1);
        let _ = std::fs::remove_file(&tmp);
    }

    /// Неверный пароль — честная ошибка, а не тихий пустой список.
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
        // Остались самые новые, порядок сохранён.
        assert_eq!(entry[0].id, "m50");
        assert_eq!(entry[MAX_MESSAGES_PER_CHANNEL - 1].id, format!("m{}", MAX_MESSAGES_PER_CHANNEL + 49));

        // При открытии канала история других каналов выбрасывается.
        let mut app = make_app();
        app.messages.insert("c1".into(), vec![entry[0].clone()]);
        app.open_channel("c0");
        assert!(app.messages.contains_key("c0"), "активный канал должен остаться");
        assert!(!app.messages.contains_key("c1"), "история других каналов должна быть выброшена");
        assert_eq!(app.history_loading.as_deref(), Some("c0"), "пока грузим — должен быть спиннер");
    }

    /// Незабранная загрузка прежнего канала не должна висеть в памяти до
    /// конца сессии: в канале уже лежат декодированные пиксели (мегабайты
    /// на картинку), и забрать их уже никто не придёт.
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

    /// Сообщение для тестов истории.
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

    /// Поздний ответ по каналу, который пользователь уже закрыл, не должен
    /// снимать «Loading messages…» у канала, который грузится сейчас: из-за
    /// этого открытый канал выглядел как пустой, пока грузился. Раньше
    /// ответы приходили вперемешку (команда гейтвея брала историю по очереди).
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

        // Приходит поздний ответ по первому каналу.
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

        // Ответ по текущему каналу принимается и снимает спиннер.
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

    /// Пагинация истории: `before` берём от самого старого сообщения
    /// страницы, иначе Discord отдаёт ту же страницу заново (в канале на 91
    /// сообщение приезжало 300 строк с тройными дублями).
    #[test]
    fn history_pagination_uses_oldest_id() {
        // Discord отдаёт от новых к старым: [100, 99, ..., 1].
        let page: Vec<ChatMessage> = (1..=3)
            .rev()
            .map(|i| test_msg(&format!("{}", 100 + i), "c1", "x"))
            .collect();
        assert_eq!(page[0].id, "103", "первым идёт самое новое");
        assert_eq!(crate::gateway::next_before_id(&page, None).as_deref(), Some("101"));

        // Повторяющийся id — значит страницы идут по кругу, грузить дальше
        // бессмысленно (иначе запросы не кончатся).
        assert_eq!(
            crate::gateway::next_before_id(&page, Some("101")),
            None,
            "одинаковый id должен останавливать пагинацию"
        );
        // Пустая страница — история кончилась.
        assert_eq!(crate::gateway::next_before_id(&[], None), None);
        // Короткая страница = дошли до начала канала, дальше не просим.
        assert!(!crate::gateway::more_history_available(3));
        assert!(!crate::gateway::more_history_available(49));
        assert!(crate::gateway::more_history_available(50));
        assert!(crate::gateway::more_history_available(100));

        // Адрес страницы: первая без `before`, вторая — от старого id.
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

    /// Подгрузка истории вверх по прокрутке: страница добавляется в начало
    /// списка, а не заменяет его, и крайнее сообщение не дублируется.
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

        // Первая страница ровно такая, какую присылает Discord: от новых к
        // старым, самое старое — m51. В списке она должна лежать в конце.
        let first: Vec<ChatMessage> = (51..=100).rev().map(|i| test_msg(&format!("m{i}"), "c1", "новое")).collect();
        assert_eq!(first[0].id, "m100", "Discord отдаёт страницу от новых к старым");
        tx.send(ToApp::History { channel_id: "c1".into(), messages: first, more: true }).unwrap();
        app.poll(&ctx);
        let stored = app.messages.get("c1").unwrap().len();
        assert_eq!(stored, crate::gateway::HISTORY_PAGE);
        assert!(!app.history_exhausted, "Discord сказал, что история есть дальше");
        assert!(app.history_loading.is_none(), "спиннер первой страницы снялся");
        // В списке порядок обратный: от старых к новым.
        let first_stored = app.messages.get("c1").unwrap();
        assert_eq!(first_stored[0].id, "m51", "в начале списка самое старое");
        assert_eq!(first_stored[stored - 1].id, "m100", "в конце самое новое");

        // Прокрутили вверх: просим и получаем страницу строго старше.
        app.request_older_history();
        assert!(app.history_loading_more, "должен гореть индикатор догрузки");
        let older: Vec<ChatMessage> = (1..=50).rev().map(|i| test_msg(&format!("m{i}"), "c1", "старое")).collect();
        tx.send(ToApp::HistoryMore { channel_id: "c1".into(), messages: older, more: false }).unwrap();
        app.poll(&ctx);

        let msgs = app.messages.get("c1").unwrap();
        assert_eq!(msgs.len(), crate::gateway::HISTORY_PAGE * 2, "страница должна добавиться, а не заменить");
        // Главное здесь — порядок и отсутствие повторов. Именно их нарушение
        // выглядело как «чат копируется»: страницы приходили по кругу, и под
        // каждым новым запросом появлялись те же самые сообщения.
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

    /// На стыке страниц одно сообщение может прийти в обеих — дубль убираем.
    /// Иначе оно мигало бы дважды подряд при прокрутке вверх.
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

        // Старше — но самое старое сообщение (m51) Discord прислал снова.
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

    /// Пока страница в пути, повторно по этому каналу не просим: один скролл
    /// вверх иначе даст пачку одинаковых запросов и лишний риск 429.
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
        // Пока идёт первая страница канала — тоже не просим вверх.
        app.history_loading_more = false;
        app.history_loading = Some("c1".into());
        app.request_older_history();
        assert!(!app.history_loading_more, "во время первой загрузки вверх не лезем");
    }

    /// Страница от Discord приходит от новых к старым, а хранить её надо от
    /// старых к новым. Если хранить как прислали, первой в списке оказывается
    /// самая новая строка, и `before` для догрузки вверх берётся от неё:
    /// Discord присылает ту же страницу ещё раз, клиент подставляет её в
    /// начало — и список растёт копиями одних и тех же сообщений. Именно это
    /// и было видно в чате: прокручиваешь вверх и циклично видишь одни и те же
    /// сообщения.
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

        // Страница ровно такая, какую присылает Discord: от новых к старым.
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

        // Догрузка вверх идёт от самой старой строки, а не от самой новой:
        // иначе Discord вернёт ту же страницу.
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

    /// Дойдя до начала канала, клиент больше не бьёт в API: `before` от
    /// кратчайшей страницы вернул бы её же.
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
        // Короткая страница = начало канала.
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

    /// Новое сообщение тянет чат вниз только если пользователь и так внизу:
    /// читающего историю выше выбрасывать в конец нельзя.
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

    /// Своё сообщение показывается сразу, не дожидаясь Discord, и пришедшее
    /// подтверждение должно занять его место, а не встать рядом вторым.
    /// Раньше у эха был пустой id, у ответа — настоящий, проверка дубля их не
    /// видела, и каждое отправленное сообщение появлялось в чате дважды.
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
        // Пустой ответ Discord затянет экран — для этого теста не нужно.
        tx.send(ToApp::History { channel_id: "c1".into(), messages: vec![], more: false }).unwrap();
        app.poll(&ctx);

        // Отправили: в чате сразу появилось одно сообщение, но id ненастоящий.
        app.handle_input("привет");
        let msgs = app.messages.get("c1").unwrap();
        assert_eq!(msgs.len(), 1, "локальное эхо должно появиться сразу");
        assert!(msgs[0].is_local_echo(), "у неподтверждённого сообщения id служебный");

        // Discord подтвердил отправку тем же текстом и настоящим id.
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

        // Второе сообщение с тем же текстом — тоже: эха теперь два, значит
        // каждое подтверждение находит своё, а не первое попавшееся.
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

        // Чужое сообщение с тем же текстом эхо не трогает: оно не наше.
        let mut foreign = test_msg("9003", "c1", "привет");
        foreign.author_id = "u-other".into();
        tx.send(ToApp::Message(foreign)).unwrap();
        app.poll(&ctx);
        assert_eq!(app.messages.get("c1").unwrap().len(), 3);
    }

    /// Служебный id неподтверждённого сообщения нельзя отправлять в Discord
    /// как `before`: такого id в переписке нет, и API вернёт ошибку.
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
        // Первая страница в этом тесте не приходит: снимаем ожидание вручную,
        // чтобы дойти до самой проверки.
        app.history_loading = None;
        let _ = tx;
        // Запрос первой страницы от открытия канала — он к делу не относится.
        let _ = std::iter::from_fn(|| cmd_rx.try_recv().ok()).count();

        // В списке только наше неотправленное сообщение — продолжать историю
        // не от чего, и клиент не должен стучаться в API.
        app.handle_input("ещё не отправлено");
        app.request_older_history();
        let sent: Vec<ToGateway> = std::iter::from_fn(|| cmd_rx.try_recv().ok()).collect();
        let asks_history = sent.iter().any(|c| matches!(c, ToGateway::FetchHistory { .. }));
        assert!(!asks_history, "нельзя пагинировать по ненастоящему id: {:?}", sent);
        assert!(app.history_exhausted);
        // Спиннер догрузки не должен гореть при этом.
        assert!(!app.history_loading_more);
    }

    /// Одна неудачная загрузка истории не должна делать канал нечитаемым
    /// навсегда. Раньше на неудачу не было события вовсе, и ждавший ответа
    /// ждал вечно: спиннер «Loading messages…» горел сутками, а колесо вверх
    /// переставало работать, потому что догрузка проверяет те же флаги.
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

        // Сеть отдала 403, три попытки — и сдалась.
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
        // Канал при этом снова рабочий: догрузка вверх не залипает.
        assert!(!app.history_loading_more);
    }

    /// То же для догрузки вверх: одна неудача — и «Loading older messages…»
    /// горел бы вечно, а колесо перестало бы листать историю.
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

    /// Неудача по чужому каналу не должна трогать текущий: у него своя загрузка.
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

    /// Discord отклонил токен — клиент должен вернуться на экран входа.
    /// Раньше это был тупик: `connected` и `gw_started` гасились только при
    /// выходе из аккаунта, гейтвей молча переподключался каждые 3 секунды, а
    /// кнопки выхода нет ни в одном меню. Ввести неверный токен — значит
    /// навсегда остаться в экране чата с пустым списком каналов.
    #[test]
    fn rejected_token_returns_to_login_screen() {
        let ctx = egui::Context::default();
        let (tx, rx) = mpsc::unbounded_channel();
        let mut app = App::new(rx);
        app.token_input = "неверныйтокен".into();
        // Гейтвей уже запущен (сам тред в тесте не нужен — он бы подменил
        // канал событий на свой).
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

    /// Обрыв соединения — это не вход, но и не «мы онлайн». Флаг `connected`
    /// раньше гасился только выходом из аккаунта, и после обрыва кнопка «👤»
    /// продолжала гореть акцентным цветом.
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

        // И наоборот: обычный статус не должен выкидывать в экран входа —
        // при переподключении пользователь должен остаться в чате.
        app.connected = true;
        tx.send(ToApp::Status("Connecting...".into())).unwrap();
        app.poll(&ctx);
        assert!(app.connected);
        assert!(app.gw_started);
        assert!(!app.shows_login(), "переподключение не должно выкидывать на экран входа");
    }

    /// «Запомнить» без пароля хранилища не может молча обойтись: раньше вход
/// продолжался, а сообщение «введи пароль хранилища» жило один кадр — его
/// тут же затирал статус гейтвея «Connecting…». Пользователь видел
/// мелькнувшую надпись и ничего: ни входа, ни объяснения, почему аккаунт не
/// сохранился.
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
        // И пароль в подсказке не пропадает после первого кадра: вход не
        // начался, значит и статус гейтвея не придёт и не затрёт его.
        assert!(app.to_gw.is_none(), "гейтвей запускаться не должен");
        let _ = std::fs::remove_file(&tmp);
    }

    /// Не открылось хранилище — вход молчать не должен по той же причине:
    /// аккаунт не сохранится, а объяснение исчезнет через кадр.
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

    /// Галочка снята — вход идёт как обычно, иначе перебор побочных эффектов
    /// превратит «запомнить» в «обязательно».
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

    /// Строка в поле сообщения — это текст для канала, а не команда клиенту.
///
/// `/quit` в поле ввода закрывал окно без сохранения и без предупреждения:
/// человек, который хотел написать в канал именно «/quit», просто терял
/// клиент. Проверяем, что такой текст уходит в канал как обычное сообщение.
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

    /// Прежний `/add <id>` создавал канал, которого нет ни в одном списке
    /// (channel_type 0, а боковая панель показывает только 1 и 3), и открывал
    /// его сразу. Теперь отладовые команды живут под своим префиксом и требуют
    /// подтверждения: одной опечатки мало.
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

        // Под своим префиксом — команда, но только после повтора.
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

        // Чужой id подтверждением не считается: ждём уже другой команды.
        let _ = std::iter::from_fn(|| cmd_rx.try_recv().ok()).count();
        app.handle_input("/debug add 111");
        let before = app.channels.len();
        app.handle_input("/debug add 222");
        assert_eq!(app.channels.len(), before, "подтверждением может быть только та же команда");
    }

    /// Приложение с одним открытым каналом и готовым приёмником команд.
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

    /// Догруженная вверх страница не должна тут же выбрасываться, когда список
/// уже упёрся в потолок по памяти.
///
/// Сценарий: в канале набралось 500 сообщений, пользователь прокручивает
/// вверх, страница приходит — и `trim_messages` режет список от начала, то
/// есть выбрасывает ровно то, что только что загрузилось. Верх истории молча
/// упирается в стену: колесо крутится, запрос уходит, а в чате ничего не
/// меняется.
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
        // Список полон: 500 последних сообщений, от m0 до m499.
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
        // На её месте ушли самые новые — и об этом сказано пользователю.
        assert_eq!(
            app.trimmed_newest, 50,
            "сколько сообщений скрыто, должно быть известно: показать это молча нельзя"
        );
        assert_eq!(
            entry[MAX_MESSAGES_PER_CHANNEL - 1].id, "m449",
            "уйти должны самые новые, а не самые старые"
        );
    }

    /// Обратная сторона: обычное обновление (своё сообщение, первая страница)
    /// должно по-прежнему выбрасывать старые. Иначе шумный канал раздует
    /// память, ради чего потолок и стоит.
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

    /// Переключение аккаунта должно поднимать поколение гейтвея: события
/// прежнего потока, который ещё какое-то время жив, обязаны перестать приходить
/// в приложение. Раньше прежний гейтвей успевал прислать `Ready` со старым
/// именем пользователя, и сразу после переключения в шапке мелькало имя
/// прежнего аккаунта.
#[test]
    fn switching_account_supersedes_the_old_gateway() {
        let mut app = App::new(mpsc::unbounded_channel().1);
        app.user_id = "старая".into();
        app.username = "Старый".into();

        let first = app.gateway_generation.next();
        app.connected = true;

        // Переключение: прежнее поколение обязано перестать быть текущим.
        app.switch_account("новыйтокен".into());
        assert!(
            !app.gateway_generation.is_current(first),
            "переключение аккаунта обязано поднять поколение"
        );
        assert!(!app.connected, "новый аккаунт ещё не подключился");
        assert!(app.gw_started);
    }

    /// Догрузка вверх, пришедшая после ухода из канала, не должна выдавать
/// середину истории за содержимое канала.
///
/// Сценарий: открыт A, ушла догрузка вверх, пользователь ушёл на B и вернулся
/// на A (`open_channel` чистит список и просит первую страницу), после чего
/// приходит поздняя `HistoryMore(A)`. Раньше она вставлялась в ПУСТОЙ список
/// как есть: канал показывал 50 сообщений из середины, новых не было вовсе,
/// и восстановиться можно было только перезаходом — молча.
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
        // Первая страница пришла, потом ушла догрузка вверх.
        let first: Vec<ChatMessage> = (1..=crate::gateway::HISTORY_PAGE)
            .rev()
            .map(|i| test_msg(&format!("m{i}"), "c1", "x"))
            .collect();
        tx.send(ToApp::History { channel_id: "c1".into(), messages: first, more: true }).unwrap();
        app.poll(&ctx);
        app.request_older_history();
        assert!(app.history_loading_more);

        // Пользователь ушёл на другой канал (список прежнего выбрасывается) и
        // вернулся: теперь ждём первую страницу, а список пуст.
        app.selected_channel = Some(1);
        app.open_channel("c2");
        let _ = std::iter::from_fn(|| cmd_rx.try_recv().ok()).count();
        app.selected_channel = Some(0);
        app.open_channel("c1");
        let _ = std::iter::from_fn(|| cmd_rx.try_recv().ok()).count();
        assert!(app.messages.get("c1").is_none(), "open_channel чистит список канала");

        // Пришла поздняя догрузка вверх — со страницы, запрошенной до ухода.
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
        // Вместо этого запрошена первая страница — с неё список и начинается.
        let asked: Vec<ToGateway> = std::iter::from_fn(|| cmd_rx.try_recv().ok()).collect();
        assert!(
            asked.iter().any(|c| matches!(c, ToGateway::FetchHistory { before: None, .. })),
            "нужно попросить первую страницу заново, а не показывать середину: {asked:?}"
        );
        // Спиннер первой страницы не должен погаснуть: он ждёт этой самой страницы.
        assert_eq!(app.history_loading.as_deref(), Some("c1"), "спиннер гасить рано");
    }

    /// Тот же случай для чужого канала: его страница тоже не должна попасть в
    /// наш список, иначе в чате появились бы сообщения не из того канала.
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

    /// Неудачная отправка: сообщение не должно остаться в чате навсегда.
/// Раньше эхо добавлялось и больше никуда не девалось — при отказе Discord
/// (403, лимит, длинный текст, обрыв связи) человек видел своё сообщение в
/// чате, как будто оно дошло, а текст из поля уже очистился.
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
        // Enter очищает поле сразу, не дожидаясь Discord.
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

    /// Если человек уже начал писать новое, возвращать старый текст нельзя —
    /// он затёр бы то, что набирается прямо сейчас.
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

    /// Начал отправлять — прежняя неудача погасла: она уже не в тему.
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

    /// Живое сообщение уже в истории — дубль пропускается.
    /// появилось после подписки, а та же строка уже успела попасть в страницу
    /// истории. Дубль рисуется дважды — в чате это видно как «сообщение
    /// скопировалось», поэтому повтор по id пропускаем.
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

        // Страница истории, как её отдаёт Discord: от новых к старым.
        let page: Vec<ChatMessage> = (1..=3).rev().map(|i| test_msg(&format!("m{i}"), "c1", "из истории")).collect();
        tx.send(ToApp::History { channel_id: "c1".into(), messages: page, more: false }).unwrap();
        app.poll(&ctx);
        assert_eq!(app.messages.get("c1").unwrap().len(), 3);

        // Тот же m3 приходит живым событием — в списке он уже есть.
        tx.send(ToApp::Message(test_msg("m3", "c1", "из истории"))).unwrap();
        app.poll(&ctx);
        let msgs = app.messages.get("c1").unwrap();
        assert_eq!(msgs.len(), 3, "живой дубль не должен добавляться");
        assert_eq!(msgs.iter().filter(|m| m.id == "m3").count(), 1, "сообщение должно быть одно");

        // А вот настоящее новое — в хвост, как обычно.
        tx.send(ToApp::Message(test_msg("m4", "c1", "новое"))).unwrap();
        app.poll(&ctx);
        let msgs = app.messages.get("c1").unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[msgs.len() - 1].id, "m4", "новое сообщение в конце списка");
    }

    /// Элемент кэша с заявленным «весом» в байтах.
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

    /// Ограничение по памяти важнее ограничения по числу: восемь мелких
    /// аватарок и три большие фотки должны уживаться в одном бюджете.
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

    /// Перезапись того же ключа не должна удваивать память в счётчике.
    #[test]
    fn bounded_cache_replacing_key_keeps_bytes_right() {
        let mut cache = BoundedCache::with_budget(10, 1000);
        cache.insert("k".into(), Weighted(100));
        cache.insert("k".into(), Weighted(250));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.bytes(), 250);
    }

    /// Картинка крупнее всего бюджета всё равно должна показаться, иначе
    /// чат просто останется пустым.
    #[test]
    fn bounded_cache_keeps_single_item_over_budget() {
        let mut cache = BoundedCache::with_budget(10, 100);
        cache.insert("k".into(), Weighted(500));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.bytes(), 500);
    }

    /// Живые сообщения из закрытых каналов не должны копиться в памяти.
    ///
    /// Discord шлёт MESSAGE_CREATE по всем каналам сразу, а список на экране
    /// один. Раньше строка ложилась в список любого канала, и пока человек
    /// сидел в одном канале, чужие копились до потолка на каждый активный
    /// канал — на шумном сервере это десятки мегабайт впустую.
    #[test]
    fn live_messages_for_closed_channels_are_not_stored() {
        let ctx = egui::Context::default();
        let (tx, rx) = mpsc::unbounded_channel();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let mut app = app_with_channel(cmd_tx);
        app.from_gw = rx;

        // Открыт c1, а пишут двести раз в c2 и один раз в c1.
        for i in 0..200 {
            tx.send(ToApp::Message(test_msg(&format!("b{i}"), "c2", "чужое"))).unwrap();
        }
        tx.send(ToApp::Message(test_msg("mine", "c1", "своё"))).unwrap();
        app.poll(&ctx);

        assert!(
            app.messages.get("c2").is_none_or(|e| e.is_empty()),
            "сообщения закрытого канала некому показывать, копить их незачем"
        );
        assert_eq!(app.messages.get("c1").map(|e| e.len()), Some(1), "открытый канал работает как раньше");
    }

    /// Сообщение из закрытого канала не должно прокручивать открытый чат вниз.
    ///
    /// `scroll_to_bottom` выставлялся без проверки канала, и чат дёргался вниз,
    /// хотя в нём ничего не появилось: человек читал историю, а его сбрасывало
    /// в конец из-за соседнего канала.
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

        // Своё сообщение по-прежнему прокручивает.
        tx.send(ToApp::Message(test_msg("mine", "c1", "своё"))).unwrap();
        app.poll(&ctx);
        assert!(app.scroll_to_bottom, "сообщение открытого канала должно прокрутить вниз");
    }

    /// Отладочный лог идёт в файл и stderr только по WYVERN_DEBUG.
    ///
    /// Раньше каждая строка без разбора писалась на диск, а строки падают по
    /// нескольку раз на кадр: это поток I/O на 20 Гц, из-за которого файл
    /// переписывался каждые ~100 секунд и до конца разбора бага не доживал.
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

    /// Вытеснение старой строки журнала — из начала очереди, а не сдвигом
    /// всего вектора.
    ///
    /// Тест держит два условия разом: контракт (не длиннее 100, уходят
    /// самые старые, остаются самые новые) и структуру — иначе `remove(0)`
    /// втихую вернётся, и на каждой строке будет сдвигаться сотня элементов.
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
}
