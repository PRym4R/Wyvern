use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use eframe::egui;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::messages::{ToApp, ToGateway};
use crate::models::{image_size_of, size_from, Attachment, ChatChannel, ChatMessage, Embed, Guild, UserProfile};
use crate::util::super_props;

const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
const API_BASE: &str = "https://discord.com/api/v10";

/// Счётчик подключений: у каждого запуска гейтвея — своё поколение.
///
/// Нужен из-за переключения аккаунта. `Shutdown` кладётся в очередь, а старый
/// поток может сейчас спать между попытками переподключения или висеть на
/// сетевом запросе — тогда он успевает ещё раз подключиться и ОПОЗНАТЬСЯ со
/// старым токеном, и на Discord секунду живут две сессии. События от
/// устаревшего поколения приложение теперь просто не слушает.
#[derive(Default)]
pub(crate) struct Generation(AtomicU64);

impl Generation {
    /// Поколение нового подключения.
    pub(crate) fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }
    /// Живо ли ещё это поколение.
    pub(crate) fn is_current(&self, mine: u64) -> bool {
        self.0.load(Ordering::SeqCst) == mine
    }
}

/// Отправитель событий, который умеет замолчать, когда приложение ушло на
/// другой аккаунт.
#[derive(Clone)]
pub(crate) struct EventTx {
    tx: mpsc::UnboundedSender<ToApp>,
    /// Поколение, которому принадлежит этот гейтвей.
    mine: u64,
    current: Arc<Generation>,
    /// Окно приложения: событие приходит на UI-поток, а egui перерисовывает
    /// кадр только по требованию. Без этого пробуждения новое сообщение
    /// ждало бы случайного кадра — а кадров в покое теперь нет вовсе (Т-7).
    wake: Option<egui::Context>,
}

impl EventTx {
    pub(crate) fn new(tx: mpsc::UnboundedSender<ToApp>, mine: u64, current: Arc<Generation>) -> Self {
        Self { tx, mine, current, wake: None }
    }
    /// Привязать окно, которое нужно будить на каждое событие.
    pub(crate) fn with_wake(mut self, ctx: egui::Context) -> Self {
        self.wake = Some(ctx);
        self
    }
    /// Событие уходит в приложение, только если гейтвей ещё тот, за кем
    /// приложение следит. Иначе события устаревшего потока перетирали бы
    /// состояние нового: имя пользователя А выскакивало бы сразу после
    /// переключения на Б.
    pub(crate) fn send(&self, ev: ToApp) {
        if !self.current.is_current(self.mine) {
            return;
        }
        let _ = self.tx.send(ev);
        // Будим окно: событие из другого потока само кадра не вызовет.
        if let Some(ctx) = &self.wake {
            ctx.request_repaint();
        }
    }
    /// Этому гейтвею ещё можно работать.
    pub(crate) fn alive(&self) -> bool {
        self.current.is_current(self.mine)
    }
}

pub(crate) async fn run_gateway(
    mut cmd_rx: mpsc::UnboundedReceiver<ToGateway>,
    event_tx: EventTx,
    token: String,
) {
    let _ = event_tx.send(ToApp::Debug("Gateway thread started".into()));
    let mut session = SessionState::default();
    loop {
        // Приложение успело переключить аккаунт, пока мы спали между попытками.
        if !event_tx.alive() {
            let _ = event_tx.send(ToApp::Debug("Gateway superseded, stopping".into()));
            return;
        }
        let use_resume = session.session_id.is_some();
        match gw_inner(&mut cmd_rx, event_tx.clone(), &token, &mut session, use_resume).await {
            Ok(()) => {
                let _ = event_tx.send(ToApp::Debug("Gateway disconnected cleanly".into()));
                let _ = event_tx.send(ToApp::Status("Disconnected".into()));
                break;
            }
            Err(e) => {
                let fatal = e.downcast_ref::<GwClosed>().is_some_and(|c| c.fatal);
                let msg = e.to_string();
                let _ = event_tx.send(ToApp::Debug(format!("Gateway error: {}", msg)));
                if fatal {
                    // Discord отказал в самом токене. Следующая попытка даст
                    // тот же отказ: раньше клиент так и долбился в Discord
                    // каждые 3 секунды сутками, не говоря ни слова почему.
                    // Вместо этого возвращаемся на экран входа с текстом.
                    let _ = event_tx.send(ToApp::AuthFailed { reason: msg });
                    break;
                }
                let _ = event_tx.send(ToApp::Status(format!("Reconnecting: {}", msg)));
                // Паузу дробим и проверяем поколение: если за эти три секунды
                // пользователь переключил аккаунт, старый гейтвей обязан
                // остановиться сразу, а не доспать до конца и снова
                // подключиться со старым токеном.
                for _ in 0..30 {
                    if !event_tx.alive() {
                        let _ = event_tx.send(ToApp::Debug("Gateway superseded while waiting, stopping".into()));
                        return;
                    }
                    time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }
}

/// Обрыв гейтвея, который повторять бессмысленно: Discord отклонил сам
/// токен или набор подписок. Отдельный тип нужен, чтобы отличить его от
/// обычного обрыва, на который надо просто зайти снова.
#[derive(Debug)]
struct GwClosed {
    message: String,
    fatal: bool,
}

impl std::fmt::Display for GwClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for GwClosed {}

/// Что означает код закрытия вебсокета.
///
/// Discord присылает код, который прямо говорит, фатально это или нет.
/// Раньше код уходил в отладочный вывод и терялся: любой обрыв выглядел как
/// «websocket closed», и клиент по одному и тому же отказу по кругу
/// переподключался каждые 3 секунды — сутками, и по этому же поводу ещё и
/// ловил rate limit.
fn close_fatal_reason(code: u16) -> Option<&'static str> {
    match code {
        4004 => Some("токен отклонён Discord: он недействителен"),
        4007 => Some("токен отозван"),
        4013 => Some("набор подписок (intents) неверный — Discord его не принимает"),
        4014 => Some("эти подписки (intents) запрещены для этого аккаунта"),
        _ => None,
    }
}

/// Достать код закрытия из служебного сообщения задачи чтения.
fn close_code_of(raw: &str) -> Option<u16> {
    raw.strip_prefix(CLOSE_MARK)?.parse().ok()
}

/// Служебные метки от задачи чтения вебсокета. Читаем мы в отдельной задаче:
/// закрытие и конец потока видит она, а цикл гейтвея — нет, и без меток тот
/// узнавал бы об этом только по неудачным heartbeat'ам.
const CLOSE_MARK: &str = "__CLOSE__";
const WS_ERROR_MARK: &str = "__WS_ERROR__";
/// Сервер закрыл соединение молча, без close-фрейма.
const EOF_MARK: &str = "__EOF__";

/// Что прислала задача чтения: событие Discord или одна из служебных меток.
enum RawFrame {
    /// Событие Discord — разбираем как JSON.
    Event,
    /// Закрытие с кодом (0 — код не прислали).
    Closed(u16),
    /// Поток кончился без close-фрейма: обычный обрыв, заходим заново.
    Eof,
    /// Ошибка чтения.
    WsError(String),
}

fn classify_raw(raw: &str) -> RawFrame {
    if raw == EOF_MARK {
        return RawFrame::Eof;
    }
    if let Some(code) = close_code_of(raw) {
        return RawFrame::Closed(code);
    }
    if let Some(e) = raw.strip_prefix(WS_ERROR_MARK) {
        return RawFrame::WsError(e.to_string());
    }
    RawFrame::Event
}

#[derive(Default)]
struct SessionState {
    session_id: Option<String>,
    seq: Option<i64>,
}

/// Сколько сообщений тянуть за один раз. Discord отдаёт до 100, но начинать
/// надо с меньшего: пока грузится три страницы, пользователь смотрит в пустой
/// канал, а потом получает столько текста, что всё равно не прочитает. Как в
/// Discord — первые 50 сообщений сразу, дальше по мере прокрутки вверх.
pub(crate) const HISTORY_PAGE: usize = 50;

/// Отправить сообщение в канал.
///
/// Отдельная задача, а не тело цикла гейтвея: пока идёт POST, цикл должен
/// крутиться — принимать события, heartbeat'ы и следующие команды. Раньше
/// отправка стояла прямо в цикле, и на всё время запроса клиент не получал
/// ни новых сообщений, ни кликов по каналам. Таймаут у клиента обязателен
/// (он задаётся при сборке клиента в `gw_inner`): без него зависший POST
/// останавливал гейтвей навсегда.
async fn send_message(
    httpc: reqwest::Client,
    tkn: String,
    event_tx: EventTx,
    channel_id: String,
    content: String,
    local_id: String,
) {
    let url = format!("{}/channels/{}/messages", API_BASE, channel_id);
    send_message_to(httpc, tkn, event_tx, url, content, local_id, channel_id).await;
}

/// Почему Discord отказал в отправке — словами для пользователя.
///
/// Коды и тело ответа показывать нельзя: там HTML-страница на сотни строк, и
/// она либо не влезает в строку статуса, либо молча обрезается. Пользователю
/// нужно знать главное — писать сюда нельзя или можно повторить.
fn send_failure_reason(status: u16) -> &'static str {
    match status {
        403 => "в этот канал писать нельзя",
        404 => "канал не найден — возможно, прав на него нет",
        429 => "слишком много сообщений подряд, Discord просит подождать",
        // 413 — текст не влез, 400 — например, больше 2000 символов.
        400 | 413 => "Discord отклонил текст (скорее всего, длиннее 2000 символов)",
        _ => "Discord отклонил сообщение",
    }
}

/// Клиент для запросов к Discord API.
///
/// Таймаут здесь обязателен, а не украшение: `Client::new()` ждёт бесконечно,
/// и один зависший POST (сеть умерла на полпути, сервер не отвечает) держал
/// гейтвей в состоянии «подключён» и не давал переподключиться. Раньше это
/// случалось и без всяких зависаний — пока запрос шёл, цикл гейтвея не
/// крутился вовсе.
/// Сколько ждём ответа Discord API. Обрываться надо и с зависшей сетью:
/// пока запрос не вернулся, гейтвей не может ни переподключиться, ни принять
/// следующую команду. Проверяется тестом `stalled_post_gives_up_instead_of_
/// hanging` на настоящем сокете, который не отвечает.
const API_TIMEOUT: Duration = Duration::from_secs(20);

fn api_client() -> Result<reqwest::Client, String> {
    client_with_timeout(API_TIMEOUT)
}

fn client_with_timeout(timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("не собрать HTTP-клиент: {}", e))
}

/// Отправить сообщение по готовому адресу. URL вынесен отдельным аргументом
/// ради теста: зависший ответ должен обрываться по таймауту, и проверить это
/// можно только на настоящем сокете, который не отвечает — а подставить
/// localhost вместо discord.com иначе нечем.
async fn send_message_to(
    httpc: reqwest::Client,
    tkn: String,
    event_tx: EventTx,
    url: String,
    content: String,
    local_id: String,
    channel_id: String,
) {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_default();
    let req = httpc
        .post(&url)
        .header("Authorization", &*tkn)
        .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36")
        .header("X-Super-Properties", &super_props())
        .header("X-Discord-Locale", "en-US")
        .header("X-Discord-Timezone", "Europe/Moscow")
        .json(&json!({ "content": content, "nonce": nonce }));
    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                let _ = event_tx.send(ToApp::Debug(format!("Send failed {}: {}", status, body)));
                // Неудачу показываем пользователю и убираем эхо из чата. Раньше
                // об этом знал только отладочный лог: сообщение оставалось в
                // списке навсегда, выглядело как отправленное, а текст из поля
                // ввода уже очистился — вернуть его было нечем.
                let _ = event_tx.send(ToApp::SendFailed {
                    channel_id,
                    local_id,
                    reason: send_failure_reason(status.as_u16()).to_string(),
                });
            } else {
                let _ = event_tx.send(ToApp::Debug("Message sent".into()));
            }
        }
        Err(e) => {
            let _ = event_tx.send(ToApp::Debug(format!("Send error: {}", e)));
            let _ = event_tx.send(ToApp::SendFailed {
                channel_id,
                local_id,
                reason: "не удалось отправить: нет связи с Discord".to_string(),
            });
        }
    }
}

/// Открыть личный чат с пользователем. Отдельная задача по той же причине,
/// что и `send_message`: сетевой запрос не должен держать цикл гейтвея.
async fn open_dm(
    httpc: reqwest::Client,
    tkn: String,
    event_tx: EventTx,
    user_id: String,
) {
    let url = format!("{}/users/@me/channels", API_BASE);
    let req = httpc
        .post(&url)
        .header("Authorization", &*tkn)
        .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36")
        .header("X-Super-Properties", &super_props())
        .header("X-Discord-Locale", "en-US")
        .header("X-Discord-Timezone", "Europe/Moscow")
        .json(&json!({ "recipient_id": user_id }));
    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                let _ = event_tx.send(ToApp::Debug(format!("Open DM failed {}", status)));
                return;
            }
            let Ok(body) = resp.text().await else { return };
            let Ok(d) = serde_json::from_str::<Value>(&body) else { return };
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
        Err(e) => {
            let _ = event_tx.send(ToApp::Debug(format!("Open DM error: {}", e)));
        }
    }
}

/// Загрузить одну страницу истории и отдать её в UI.
///
/// `before` — самый старый id, который уже есть на экране: Discord отдаёт
/// сообщения от новых к старым, и если просить `before` от самого нового, он
/// вернёт ту же страницу ещё раз (раньше так и было — в канале на 91 сообщение
/// приезжало 300 строк с тройными дублями и лишними запросами). `None` — это
/// первая страница, её мы показываем сразу.
async fn fetch_history_page(
    httpc: reqwest::Client,
    tkn: String,
    event_tx: EventTx,
    channel_id: String,
    before: Option<String>,
) {
    let url = history_url(&channel_id, before.as_deref());

    let mut page: Vec<ChatMessage> = Vec::new();
    let mut got_page = false;
    let mut failed = false;
    let mut reason = String::new();
    let mut attempt = 0u32;
    while !got_page && attempt < 3 && !failed {
        attempt += 1;
        let req = httpc
            .get(&url)
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
                    let retry = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(2);
                    let _ = event_tx.send(ToApp::Debug(format!("History 429, retrying in {}s", retry)));
                    reason = "Discord просит подождать (лимит запросов)".to_string();
                    time::sleep(Duration::from_secs(retry)).await;
                    continue;
                }
                if !status.is_success() {
                    let _ = event_tx.send(ToApp::Debug(format!("History error {}", status)));
                    // 403 — нет прав на канал. Повтор не поможет, и молчать
                    // об этом нельзя: пользователь видит пустой канал.
                    reason = if status == 403 {
                        "нет прав на канал".to_string()
                    } else {
                        format!("сервер ответил {}", status)
                    };
                    failed = true;
                    break;
                }
                match resp.text().await {
                    Ok(body) => {
                        let mut warn = |m: String| {
                            let _ = event_tx.send(ToApp::Debug(m));
                        };
                        page = parse_history_page_lenient(&body, &channel_id, &mut warn);
                        got_page = true;
                    }
                    Err(e) => {
                        let _ = event_tx.send(ToApp::Debug(format!("History body error: {}", e)));
                        reason = "не удалось прочитать ответ".to_string();
                    }
                }
            }
            Err(e) => {
                let _ = event_tx.send(ToApp::Debug(format!("History request error: {}", e)));
                reason = "нет связи с Discord".to_string();
                time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    if failed || !got_page {
        // Ничего не пришло. Раньше здесь был просто `return`, и это ломало
        // приложение: тот, кто ждал ответа (спиннер первой страницы в
        // `history_loading` или догрузки вверх в `history_loading_more`),
        // ждал вечно. Одна неудача — и канал становился нечитаемым навсегда.
        if reason.is_empty() {
            reason = "история не пришла".to_string();
        }
        let _ = event_tx.send(ToApp::HistoryFailed { channel_id, before, reason });
        return;
    }

    let got = page.len();
    let more = more_history_available(got) && next_before_id(&page, before.as_deref()).is_some();
    let _ = event_tx.send(ToApp::Debug(format!(
        "History page: {} messages, more={} (before={})",
        got,
        more,
        before.as_deref().map(|b| &b[..b.len().min(14)]).unwrap_or("-")
    )));
    let event = if before.is_some() {
        ToApp::HistoryMore { channel_id, messages: page, more }
    } else {
        ToApp::History { channel_id, messages: page, more }
    };
    let _ = event_tx.send(event);
}

/// Адрес страницы истории. Первая страница — без `before`, дальше — от
/// самого старого id, который уже получили.
pub(crate) fn history_url(channel_id: &str, before: Option<&str>) -> String {
    match before {
        Some(b) => format!(
            "{}/channels/{}/messages?limit={}&before={}",
            API_BASE, channel_id, HISTORY_PAGE, b
        ),
        None => format!("{}/channels/{}/messages?limit={}", API_BASE, channel_id, HISTORY_PAGE),
    }
}

/// Id самого старого сообщения страницы — его просим как `before` у
/// Discord. Если страницы пошли по кругу (id повторился) или id пустой,
/// грузить дальше бессмысленно: возвращаем `None`.
pub(crate) fn next_before_id(page: &[ChatMessage], current: Option<&str>) -> Option<String> {
    let id = page.last()?.id.clone();
    if id.is_empty() || Some(id.as_str()) == current {
        return None;
    }
    Some(id)
}

/// Есть ли что грузить дальше вверх. Короткая страница = Discord дошёл до
/// начала канала: там меньше 50 сообщений и следующего запроса не будет.
/// Повторяющийся id (страницы пошли по кругу) — тоже конец, иначе запросы
/// не кончатся.
pub(crate) fn more_history_available(got: usize) -> bool {
    got >= HISTORY_PAGE
}

/// Discord не всегда присылает строку там, где мы ждём строку (например,
/// `content` у системных сообщений может быть числом). Такое поле берём как
/// есть: раньше `as_str().unwrap_or("")` тихо подставлял пустую строку, и
/// ронять из-за этого всю страницу истории нельзя.
fn de_text<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<Value>::deserialize(d)?
        .map(|v| match v {
            Value::String(s) => s,
            other => other.to_string(),
        })
        .unwrap_or_default())
}

/// То же, но с «пустым» значением: отсутствие поля и не-строка дают `None`.
fn de_opt_text<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(Option::<Value>::deserialize(d)?.and_then(|v| match v {
        Value::String(s) => Some(s),
        _ => None,
    }))
}

/// Одно число, пришедшее числом или строкой. Discord в размерах шлёт целое,
/// но в тексте JSON может прийти и строкой, и нечётное значение не должно
/// ронять разбор сообщения.
fn de_opt_u32<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    Ok(Option::<Value>::deserialize(d)?.and_then(|v| match v {
        Value::Number(n) => n.as_u64().map(|x| x.min(u64::from(u32::MAX)) as u32),
        Value::String(s) => s.trim().parse::<u32>().ok(),
        _ => None,
    }))
}

/// Автор сообщения в том виде, в каком его рисует клиент.
#[derive(serde::Deserialize)]
struct RawAuthor {
    #[serde(default, deserialize_with = "de_text")]
    id: String,
    #[serde(default, deserialize_with = "de_opt_text")]
    username: Option<String>,
    #[serde(default, deserialize_with = "de_opt_text")]
    avatar: Option<String>,
}

/// Вложение: только то, что нужно для показа. Имя файла клиенту не нужно,
/// `content_type` берём, потому что по нему решаем, грузить ли картинку
/// вообще, а размеры — по ним мы знаем высоту сообщения, не качая картинку.
#[derive(serde::Deserialize)]
struct RawAttachment {
    #[serde(default, deserialize_with = "de_opt_text")]
    url: Option<String>,
    #[serde(default, deserialize_with = "de_opt_text")]
    content_type: Option<String>,
    #[serde(default, deserialize_with = "de_opt_text")]
    description: Option<String>,
    /// Размер картинки в пикселях. Discord шлёт его полями `width`/`height`,
    /// а `size` у вложения — это размер ФАЙЛА в байтах, он нам не нужен.
    ///
    /// Раньше здесь стояло поле `size` с разбором через `image_size_of`, и
    /// размер не приходил никогда: `image_size_of` ищет ключи `width`/`height`
    /// внутри значения поля `size`, а там лежит число, ключей в котором нет.
    #[serde(default, deserialize_with = "de_opt_u32")]
    width: Option<u32>,
    #[serde(default, deserialize_with = "de_opt_u32")]
    height: Option<u32>,
}

#[derive(serde::Deserialize)]
struct RawImage {
    #[serde(default, deserialize_with = "de_opt_text")]
    url: Option<String>,
    /// Как и у вложения: пиксели приходят полями `width`/`height`.
    #[serde(default, deserialize_with = "de_opt_u32")]
    width: Option<u32>,
    #[serde(default, deserialize_with = "de_opt_u32")]
    height: Option<u32>,
}

/// Эмбед: автор, footer, provider, поля и прочее сюда не входят — serde
/// пропускает неизвестные поля, не выделяя под них памяти.
#[derive(serde::Deserialize)]
struct RawEmbed {
    #[serde(default, deserialize_with = "de_opt_text")]
    description: Option<String>,
    #[serde(default)]
    image: Option<RawImage>,
    #[serde(default)]
    thumbnail: Option<RawImage>,
    #[serde(default)]
    video: Option<RawImage>,
}

impl RawEmbed {
    fn into_embed(self) -> Option<Embed> {
        let image = [self.image, self.thumbnail, self.video]
            .into_iter()
            .flatten()
            .find(|i| i.url.as_deref().is_some_and(|u| !u.is_empty()));
        let image_url = image.as_ref().and_then(|i| i.url.clone());
        let image_size = image.and_then(|i| size_from(i.width, i.height));
        let description = match self.description {
            Some(s) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    None
                } else if trimmed.len() == s.len() {
                    // Обрезки не было — оставляем уже готовую строку.
                    Some(s)
                } else {
                    Some(trimmed.to_string())
                }
            }
            None => None,
        };
        if image_url.is_none() && description.is_none() {
            return None;
        }
        Some(Embed { image_url, image_size, description })
    }
}

/// Сообщение в том виде, в каком оно хранится у нас.
#[derive(serde::Deserialize)]
struct RawMessage {
    #[serde(default, deserialize_with = "de_text")]
    id: String,
    #[serde(default, deserialize_with = "de_text")]
    content: String,
    #[serde(default, deserialize_with = "de_text")]
    timestamp: String,
    #[serde(default)]
    author: Option<RawAuthor>,
    #[serde(default)]
    attachments: Vec<RawAttachment>,
    #[serde(default)]
    embeds: Vec<RawEmbed>,
}

impl RawMessage {
    /// `None` — сообщение без автора, в чат оно не попадает (как и раньше).
    fn into_message(self, channel_id: &str) -> Option<ChatMessage> {
        let author = self.author?;
        Some(ChatMessage {
            id: self.id,
            channel_id: channel_id.to_string(),
            author_id: author.id,
            author_name: author.username.unwrap_or_else(|| "?".into()),
            author_avatar: author.avatar,
            nickname: None,
            content: self.content,
            timestamp: self.timestamp,
            attachments: self
                .attachments
                .into_iter()
                .filter_map(|a| {
                    Some(Attachment {
                        url: a.url?,
                        content_type: a.content_type,
                        description: a.description,
                        size: size_from(a.width, a.height),
                    })
                })
                .collect(),
            // Эмбеды храним только в урезанном виде: полный JSON стоит
            // в разы дороже двух нужных полей.
            embeds: self.embeds.into_iter().filter_map(RawEmbed::into_embed).collect(),
            is_own: false,
        })
    }
}

/// Разобрать страницу истории из JSON Discord в сообщения.
///
/// Страница разбирается сразу в нужные структуры. Если сначала собрать
/// `serde_json::Value` на всю страницу, а потом вытащить из неё несколько
/// строк, то на сотне сообщений это десятки тысяч лишних аллокаций и
/// заметный пик памяти — всё дерево `Value` живёт до конца разбора.
///
/// Строгий разбор может упасть, если Discord пришлёт поле не того типа.
/// Тогда страница разбирается запасным, терпительным способом (см.
/// [`parse_message_value`]): потерять историю канала из-за одной странной
/// строки хуже, чем показать её чуть менее подробно.
pub(crate) fn parse_history_page_lenient(body: &str, channel_id: &str, warn: &mut dyn FnMut(String)) -> Vec<ChatMessage> {
    match parse_history_page(body, channel_id) {
        Ok(msgs) => msgs,
        Err(e) => match serde_json::from_str::<Vec<Value>>(body) {
            Ok(arr) => {
                warn(format!("History strict parse failed ({}), fallback used", e));
                arr.iter().filter_map(|m| parse_message_value(m, channel_id)).collect()
            }
            Err(e2) => {
                warn(format!("History parse error: {} / {}", e, e2));
                Vec::new()
            }
        },
    }
}

/// Строгий разбор страницы: ошибка формата возвращается вызывающему.
pub(crate) fn parse_history_page(body: &str, channel_id: &str) -> Result<Vec<ChatMessage>, serde_json::Error> {
    let raw: Vec<RawMessage> = serde_json::from_str(body)?;
    Ok(raw.into_iter().filter_map(|m| m.into_message(channel_id)).collect())
}

/// Разбор одного сообщения из уже готового дерева `Value` — для живых
/// событий гейтвея, где дерево уже собрано целиком. `fallback_channel`
/// нужен на случай, если в событии нет своего `channel_id`.
pub(crate) fn parse_message_value(m: &Value, fallback_channel: &str) -> Option<ChatMessage> {
    let author = m.get("author")?;
    Some(ChatMessage {
        id: m["id"].as_str().unwrap_or("").to_string(),
        channel_id: m["channel_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(fallback_channel)
            .to_string(),
        author_id: author["id"].as_str().unwrap_or("").to_string(),
        author_name: author["username"].as_str().unwrap_or("?").to_string(),
        author_avatar: author["avatar"].as_str().map(|s| s.to_string()),
        nickname: None,
        content: m["content"].as_str().unwrap_or("").to_string(),
        timestamp: m["timestamp"].as_str().unwrap_or("").to_string(),
        attachments: m["attachments"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| {
                        Some(Attachment {
                            url: a["url"].as_str()?.to_string(),
                            content_type: a["content_type"].as_str().map(|s| s.to_string()),
                            description: a["description"].as_str().map(|s| s.to_string()),
                            size: image_size_of(a),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        embeds: m["embeds"]
            .as_array()
            .map(|arr| arr.iter().filter_map(Embed::from_json).collect())
            .unwrap_or_default(),
        is_own: false,
    })
}

async fn gw_inner(
    cmd_rx: &mut mpsc::UnboundedReceiver<ToGateway>,
    event_tx: EventTx,
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
                    // Тащим сам код закрытия, а не отладочный вывод всей
                    // структуры: по коду решается, повторять подключение или
                    // нет (см. `close_fatal_reason`).
                    let code = c.map(|f| u16::from(f.code)).unwrap_or(0);
                    let _ = raw_tx.send(format!("__CLOSE__{}", code));
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    let _ = raw_tx.send(format!("{}{}", WS_ERROR_MARK, e));
                    break;
                }
            }
        }
        // Поток от сервера кончился. Если это был close-фрейм, цикл гейтвея
        // уже ушёл по нему; а вот молчаливый конец (сервер закрыл соединение
        // без кода) раньше просто обрывал чтение и оставлял гейтвей жить: тот
        // продолжал слать heartbeat'ы в мёртвый сокет, пока не набирал пять
        // неудач подряд — это минуты вместо трёх секунд, и всё это время
        // клиент молчал, ничем не показывая, что связи нет. Метка нужна, чтобы
        // цикл узнал об этом сразу.
        let _ = raw_tx.send(EOF_MARK.to_string());
    });

    // Клиент обязателен с таймаутом, иначе зависший POST останавливает гейтвей
    // навсегда (см. `api_client`).
    let http = api_client()?;
    let tkn = token.to_string();
    // Каналы, история которых уже грузится: защита от дублей при кликах.
    // Каналы и страницы, история которых уже грузится: защита от дублей при
    // кликах. Ключ — (канал, запрошенная страница): повтор той же страницы
    // действительно лишний, а вот первая страница и догрузка вверх — разные
    // запросы, и раньше вторая отбрасывалась как дубль первой.
    let history_inflight =
        std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::<(String, Option<String>)>::new()));
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
                match classify_raw(&raw) {
                    RawFrame::Closed(code) => match close_fatal_reason(code) {
                        // Отказ в самом токене: повтор не поможет. Помечаем
                        // ошибку как фатальную, и `run_gateway` вернёт клиент
                        // на экран входа вместо бесконечного переподключения.
                        Some(reason) => {
                            let _ = event_tx.send(ToApp::Debug(format!("WebSocket closed {}: {}", code, reason)));
                            return Err(Box::new(GwClosed {
                                message: format!("{} ({})", reason, code),
                                fatal: true,
                            }));
                        }
                        // Обычный обрыв: сеть, сон компьютера, реконнект со
                        // стороны Discord — заходим снова.
                        None => {
                            let _ = event_tx.send(ToApp::Debug(format!("WebSocket closed: {}", code)));
                            return Err("websocket closed".into());
                        }
                    },
                    // Поток кончился молча. Раньше чтение просто обрывалось,
                    // гейтвей оставался в живых и слал heartbeat'ы в мёртвый
                    // сокет, пока не набирал пять неудач подряд, — минуты
                    // молчания вместо трёх секунд переподключения.
                    RawFrame::Eof => {
                        let _ = event_tx.send(ToApp::Debug("WebSocket stream ended".into()));
                        return Err("websocket closed".into());
                    }
                    RawFrame::WsError(e) => {
                        let _ = event_tx.send(ToApp::Debug(format!("WebSocket error: {}", e)));
                        return Err("websocket error".into());
                    }
                    RawFrame::Event => {}
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
                                        let _ = event_tx.send(ToApp::Guild(Guild {
                                            id: gid.clone(),
                                            name: gname.clone(),
                                            icon: gicon,
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
                                            egoods: EventTx,
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
                                // Тот же разбор, что и у страницы истории, но
                                // из уже готового дерева события: пересобирать
                                // его через serde незачем.
                                if let Some(msg) = parse_message_value(&v["d"], "") {
                                    let _ = event_tx.send(ToApp::Message(msg));
                                }
                            }
                            "GUILD_CREATE" => {
                                let d = &v["d"];
                                let guild = Guild {
                                    id: d["id"].as_str().unwrap_or("").to_string(),
                                    name: d["name"].as_str().unwrap_or("Unknown").to_string(),
                                    icon: d["icon"].as_str().map(|s| s.to_string()),
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
                    ToGateway::Send { channel_id, content, local_id } => {
                        // Отправку тоже уводим из цикла в отдельную задачу.
                        // Пока идёт POST, гейтвей не читает события и не
                        // берёт команды: сообщение, пришедшее в это время,
                        // задерживалось на всё время запроса (а без
                        // таймаута — навсегда).
                        let httpc = http.clone();
                        let tkc = tkn.clone();
                        let ev = event_tx.clone();
                        tokio::spawn(async move {
                            send_message(httpc, tkc, ev, channel_id, content, local_id).await;
                        });
                    }
                    ToGateway::FetchHistory { channel_id, before } => {
                        // Историю тянем отдельной задачей. Раньше загрузка шла
                        // прямо в цикле команд и блокировала всё остальное:
                        // один запрос — это до трёх обращений к API с паузами,
                        // и клик по следующему каналу «зависал» до его конца.
                        // Повторный запрос по тому же каналу игнорируем, иначе
                        // два одинаковых запроса и лишний риск 429.
                        let inflight = history_inflight.clone();
                        let httpc = http.clone();
                        let tkc = tkn.clone();
                        let ev = event_tx.clone();
                        let cid = channel_id.clone();
                        // Ключ защиты — пара (канал, страница), а не один
                        // канал. Догрузка вверх и повторное открытие канала —
                        // это РАЗНЫЕ страницы, и раньше вторая отбрасывалась
                        // как дубль первой: запрос первой страницы уходил в
                        // никуда, спиннер не гас, а пришедшая потом догрузка
                        // ложилась в пустой список (Б-17).
                        let key = (cid.clone(), before.clone());
                        let cid_short: String = cid.chars().take(14).collect();
                        tokio::spawn(async move {
                            {
                                let mut busy = inflight.lock().await;
                                if !busy.insert(key.clone()) {
                                    let _ = ev.send(ToApp::Debug(format!(
                                        "History for {} already in flight, skipping",
                                        cid_short
                                    )));
                                    return;
                                }
                            }
                            fetch_history_page(httpc, tkc, ev.clone(), cid.clone(), before).await;
                            inflight.lock().await.remove(&key);
                        });
                    }
                    ToGateway::OpenDM { user_id } => {
                        // Тот же случай, что и с отправкой: запрос в цикле
                        // гейтвея замораживал его на всё время ожидания.
                        let httpc = http.clone();
                        let tkc = tkn.clone();
                        let ev = event_tx.clone();
                        tokio::spawn(async move {
                            open_dm(httpc, tkc, ev, user_id).await;
                        });
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

#[cfg(test)]
mod http_tests {
    use super::{
    api_client, client_with_timeout, send_failure_reason, send_message_to, EventTx, Generation,
    API_TIMEOUT,
};
    use crate::messages::ToApp;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// У клиента гейтвея обязан быть разумный таймаут. `Client::new()` ждёт
    /// бесконечно: один зависший запрос держал гейтвей в состоянии
    /// «подключён» и не давал переподключиться.
    #[test]
    fn api_client_has_a_sane_timeout() {
        assert!(API_TIMEOUT > Duration::from_secs(1), "слишком часто обрывать");
        assert!(
            API_TIMEOUT < Duration::from_secs(60),
            "настоящий ответ Discord столько не ждёт, а гейтвей столько молчит"
        );
        assert!(api_client().is_ok(), "клиент должен собираться");
    }

    /// Отправка не должна висеть на сервере, который принял соединение и
    /// замолчал. Проверяется на настоящем сокете: подменить его конусом
    /// моков и убедиться, что запрос ушёл, нельзя — а именно тут всё и ломалось.
    #[test]
    fn stalled_post_gives_up_instead_of_hanging() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Сервер принимает соединение и не отвечает никогда. Сокет держим открытым
        // всё это время: если его сразу бросить, клиент увидит обрыв и
        // закончит запрос с ошибкой без всякого таймаута — тест прошёл бы на
        // сломанном коде.
        std::thread::spawn(move || {
            if let Ok((_stream, _)) = listener.accept() {
                std::thread::sleep(Duration::from_secs(10));
            }
        });

        let (tx, mut rx) = mpsc::unbounded_channel();
        // Приёмник оборачиваем в EventTx с единственным поколением: события
        // оттуда идут прямо в приложение.
        let gen = Arc::new(Generation::default());
        let event_tx = EventTx::new(tx, gen.next(), gen.clone());
        // Таймаут уменьшен, чтобы тест не ждал двадцать секунд; смысл тот же.
        let client = client_with_timeout(Duration::from_millis(300)).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let started = std::time::Instant::now();
        rt.block_on(send_message_to(
            client,
            "токен".into(),
            event_tx,
            format!("http://{}/channels/1/messages", addr),
            "привет".into(),
            "local:7".into(),
            "c1".into(),
        ));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "зависший POST должен обрываться, а не ждать: {:?}",
            started.elapsed()
        );
        // Обрыв связи обязан быть виден пользователю: раньше уходила строка
        // только в отладочный лог, а эхо оставалось в чате навсегда.
        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            seen.push(ev);
        }
        assert!(
            seen.iter().any(|e| matches!(e, ToApp::SendFailed { reason, .. } if reason.contains("нет связи"))),
            "пользователь должен узнать об отказе, получено: {seen:?}"
        );
    }

    /// Отказ Discord должен объясняться словами, а не кодом. Пользователю
    /// важно одно: писать сюда нельзя или можно повторить. Код 403 в строке
    /// статуса не помогает ничего.
    #[test]
    fn send_failure_reasons_are_readable() {
        for (status, must_contain) in [
            (403u16, "писать нельзя"),
            (404, "прав"),
            (429, "подождать"),
            (400, "2000"),
            (413, "2000"),
            (500, "отклонил"),
        ] {
            let reason = send_failure_reason(status);
            assert!(
                reason.contains(must_contain),
                "код {status}: должно быть про {must_contain:?}, а написано {reason:?}"
            );
            assert!(!reason.contains(&status.to_string()), "код не должен попадать в текст: {reason:?}");
        }
    }
}

#[cfg(test)]
mod generation_tests {
    use super::{EventTx, Generation};
    use crate::messages::ToApp;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    /// Переключение аккаунта не должно оставлять прежний гейтвей живым.
    /// `Shutdown` кладётся в очередь, а поток может спать между попытками
    /// переподключения или висеть на запросе — тогда он успевает ещё раз
    /// подключиться со старым токеном, и Discord видит две сессии. События
    /// устаревшего поколения приложение слушать не должно.
    #[test]
    fn superseded_gateway_goes_silent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let gen = Arc::new(Generation::default());
        let old = EventTx::new(tx.clone(), gen.next(), gen.clone());
        // Переключили аккаунт.
        let new = EventTx::new(tx, gen.next(), gen.clone());

        assert!(!old.alive(), "прежний гейтвей должен понять, что он больше не нужен");
        assert!(new.alive());
        old.send(ToApp::Debug("старое имя пользователя".into()));
        assert!(
            rx.try_recv().is_err(),
            "событие устаревшего гейтвея перетирало бы состояние нового"
        );

        new.send(ToApp::Debug("новое".into()));
        assert!(matches!(rx.try_recv(), Ok(ToApp::Debug(_))), "новый гейтвей должен говорить");
    }

    /// Поколения должны идти по порядку, иначе «новый» гейтвей решит, что он
    /// устарел, и замолчит сам.
    #[test]
    fn generations_increase_monotonically() {
        let gen = Generation::default();
        let first = gen.next();
        let second = gen.next();
        assert!(second > first, "поколения должны расти: {first} → {second}");
        assert!(gen.is_current(second));
        assert!(!gen.is_current(first), "прежнее поколение больше не актуально");
    }
}

#[cfg(test)]
mod close_tests {
    use super::{close_code_of, close_fatal_reason, classify_raw, RawFrame, EOF_MARK, WS_ERROR_MARK};

    /// Коды, которыми Discord отказывает в самом токене или подписках,
    /// означают, что повторное подключение ничего не изменит. Их надо
    /// отличать от обычного обрыва: раньше все закрытия выглядели одинаково,
    /// и клиент по кругу, каждые 3 секунды, долбился в Discord сутками, не
    /// говоря пользователю ни слова.
    #[test]
    fn token_refusal_codes_are_fatal() {
        for code in [4004u16, 4007, 4013, 4014] {
            let reason = close_fatal_reason(code)
                .unwrap_or_else(|| panic!("код {code} должен быть фатальным"));
            assert!(!reason.is_empty(), "причина должна показываться пользователю");
        }
        // 4004 — самый частый случай: токен невалиден.
        assert!(close_fatal_reason(4004).unwrap().contains("токен"));
    }

    /// Обычные обрывы (сеть, сон, реконнект со стороны Discord)
    /// переподключаться должны, как раньше: иначе клиент не пережил бы
    /// обычную потерю связи.
    #[test]
    fn ordinary_close_codes_are_not_fatal() {
        for code in [0u16, 1000, 1001, 1006, 1011, 1012, 1013, 4000, 4008, 4011] {
            assert_eq!(close_fatal_reason(code), None, "код {code} — обычный обрыв");
        }
    }

    /// Задача чтения отдаёт код закрытия отдельной служебной строкой; если её
    /// разобрать не удалось, обрыв считаем обычным, но не фатальным.
    #[test]
    fn close_code_is_taken_from_the_read_task() {
        assert_eq!(close_code_of("__CLOSE__4004"), Some(4004));
        assert_eq!(close_code_of("__CLOSE__1000"), Some(1000));
        assert_eq!(close_code_of("__WS_ERROR__broken pipe"), None);
        assert_eq!(close_code_of("{\"op\":0}"), None);
        // Мусор вместо кода не должен превращаться в «фатально».
        assert_eq!(close_code_of("__CLOSE__мусор"), None);
    }

    /// Молчаливый конец потока (сервер закрыл соединение без close-фрейма)
    /// обязан отличаться от обычного события. Раньше чтение просто обрывалось,
    /// гейтвей оставался в живых и слал heartbeat'ы в мёртвый сокет, пока не
    /// набирал пять неудач подряд, — минуты молчания вместо трёх секунд
    /// переподключения.
    #[test]
    fn silent_end_of_stream_is_recognised() {
        assert!(matches!(classify_raw(EOF_MARK), RawFrame::Eof));
        // Метка обязана быть ровно такой, какую шлёт задача чтения: опечатка
        // здесь тихо отключила бы переподключение.
        assert!(matches!(classify_raw(super::EOF_MARK), RawFrame::Eof));
    }

    /// Остальные служебные метки не должны путаться с событиями Discord.
    #[test]
    fn other_frames_stay_distinct() {
        assert!(matches!(classify_raw("__CLOSE__4004"), RawFrame::Closed(4004)));
        assert!(matches!(classify_raw("__CLOSE__1000"), RawFrame::Closed(1000)));
        assert!(matches!(classify_raw(&format!("{}broken pipe", WS_ERROR_MARK)), RawFrame::WsError(_)));
        // Настоящее событие остаётся событием, даже если в нём есть "__CLOSE__"
        // в поле данных.
        assert!(matches!(classify_raw("{\"op\":0,\"t\":\"__CLOSE__4004\"}"), RawFrame::Event));
        assert!(matches!(classify_raw("{\"op\":11,\"d\":null}"), RawFrame::Event));
    }
}

#[cfg(test)]
mod parse_tests {
    use super::{parse_history_page, parse_history_page_lenient, parse_message_value};

    /// Сообщение в том виде, в каком его отдаёт Discord: много полей, которые
    /// клиенту не нужны (reaction_counts, mentions, flags и прочее).
    const REAL: &str = r#"[{
        "id": "1200000000000000001",
        "channel_id": "900000000000000000",
        "content": "привет",
        "timestamp": "2026-09-26T12:00:00.000000+00:00",
        "type": 0,
        "pinned": false,
        "mention_everyone": false,
        "edited_timestamp": null,
        "flags": 0,
        "author": {
            "id": "800000000000000000",
            "username": "vasya",
            "discriminator": "0",
            "avatar": "abc123",
            "global_name": "Вася",
            "bot": false
        },
        "attachments": [{
            "id": "1100000000000000000",
            "filename": "photo.png",
            "size": 123456,
            "width": 1600,
            "height": 1200,
            "content_type": "image/png",
            "description": "схема из чата",
            "url": "https://cdn.discordapp.com/attachments/1/photo.png",
            "proxy_url": "https://media.discordapp.net/attachments/1/photo.png"
        }],
        "embeds": [{
            "type": "rich",
            "title": "заголовок",
            "author": {"name": "Кто-то", "url": "https://example.com"},
            "footer": {"text": "подпись"},
            "provider": {"name": " twitch"},
            "image": {"url": "https://cdn.discordapp.com/embeds/1/picture.png", "width": 800, "height": 600},
            "fields": [{"name": "a", "value": "b", "inline": true}]
        }],
        "reaction_counts": [{"count": 1, "me": false}],
        "mentions": []
    }]"#;

    fn parse_one_in(body: &str, channel_id: &str) -> crate::models::ChatMessage {
        let mut msgs = parse_history_page(body, channel_id).expect("разбор не должен падать");
        assert_eq!(msgs.len(), 1, "ожидалось одно сообщение");
        msgs.pop().unwrap()
    }

    fn parse_one(body: &str) -> crate::models::ChatMessage {
        parse_one_in(body, "fallback")
    }

    #[test]
    fn history_page_maps_all_used_fields() {
        let m = parse_one(REAL);
        assert_eq!(m.id, "1200000000000000001");
        assert_eq!(m.channel_id, "fallback", "канал страницы важнее поля в сообщении");
        assert_eq!(m.author_id, "800000000000000000");
        assert_eq!(m.author_name, "vasya");
        assert_eq!(m.author_avatar.as_deref(), Some("abc123"));
        assert_eq!(m.content, "привет");
        assert_eq!(m.timestamp, "2026-09-26T12:00:00.000000+00:00");
        assert!(!m.is_own);
        assert!(m.nickname.is_none());

        assert_eq!(m.attachments.len(), 1);
        let a = &m.attachments[0];
        assert_eq!(a.url, "https://cdn.discordapp.com/attachments/1/photo.png");
        assert_eq!(a.content_type.as_deref(), Some("image/png"));
        assert_eq!(a.description.as_deref(), Some("схема из чата"));
        // Размер в пикселях приходит полями `width`/`height`, а `size` у
        // вложения — это размер файла в байтах. Раньше поле называлось `size`
        // и разбиралось как пара пикселей, из-за чего размер не приходил
        // никогда: по нему резервируется место под картинку, и без него
        // высота сообщения прыгала на 180 px при загрузке.
        assert_eq!(a.size, Some([1600, 1200]), "размер картинки должен доходить из истории");

        assert_eq!(m.embeds.len(), 1);
        assert_eq!(
            m.embeds[0].image_url.as_deref(),
            Some("https://cdn.discordapp.com/embeds/1/picture.png")
        );
        assert_eq!(m.embeds[0].image_size, Some([800, 600]), "размер картинки эмбеда — тоже");
        assert_eq!(m.embeds[0].description, None, "у эмбеда нет description — выкидываем пустое");
    }

    /// История и живое сообщение обязаны разбираться одинаково, иначе
    /// сообщение, приехавшее в реальном времени, будет выглядеть не так, как
    /// то же сообщение из истории.
    ///
    /// Разница одна и намеренная: страница истории берёт канал, для которого
    /// её запросили, а живое сообщение — свой `channel_id`. Поэтому здесь
    /// подставляем запасным именно тот канал, который указан в сообщении.
    #[test]
    fn history_and_live_parse_agree() {
        let from_history = parse_one_in(REAL, "900000000000000000");
        let from_live = parse_message_value(
            &serde_json::from_str::<serde_json::Value>(REAL).unwrap()[0],
            "900000000000000000",
        )
        .expect("живое сообщение должно разобраться");
        assert_eq!(from_history.id, from_live.id);
        assert_eq!(from_history.channel_id, from_live.channel_id);
        assert_eq!(from_history.author_id, from_live.author_id);
        assert_eq!(from_history.author_name, from_live.author_name);
        assert_eq!(from_history.author_avatar, from_live.author_avatar);
        assert_eq!(from_history.content, from_live.content);
        assert_eq!(from_history.timestamp, from_live.timestamp);
        assert_eq!(from_history.attachments.len(), from_live.attachments.len());
        assert_eq!(from_history.attachments[0].url, from_live.attachments[0].url);
        assert_eq!(
            from_history.attachments[0].description, from_live.attachments[0].description
        );
        // Размер картинки — тоже: раньше именно здесь пути разходились
        // (история отдавала `None`, живое сообщение — настоящие пиксели), и
        // тест этого не замечал, потому что размер не сравнивал.
        assert_eq!(from_history.attachments[0].size, from_live.attachments[0].size);
        assert_eq!(from_history.attachments[0].size, Some([1600, 1200]));
        assert_eq!(from_history.embeds.len(), from_live.embeds.len());
        assert_eq!(from_history.embeds[0].image_url, from_live.embeds[0].image_url);
        assert_eq!(from_history.embeds[0].image_size, from_live.embeds[0].image_size);
    }

    /// Живое сообщение знает свой канал сам; если поля нет — берём запасной.
    #[test]
    fn live_message_uses_own_channel_then_fallback() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"id":"1","channel_id":"chan-42","content":"x","author":{"id":"u","username":"n"}}"#).unwrap();
        let m = parse_message_value(&v, "fallback").unwrap();
        assert_eq!(m.channel_id, "chan-42");
        let v2: serde_json::Value =
            serde_json::from_str(r#"{"id":"1","content":"x","author":{"id":"u","username":"n"}}"#).unwrap();
        assert_eq!(parse_message_value(&v2, "fallback").unwrap().channel_id, "fallback");
    }

    /// Сообщение без автора в чат не попадает — как и раньше.
    #[test]
    fn message_without_author_is_skipped() {
        let body = r#"[{"id":"1","content":"системное","author":{"id":"u","username":"n"}},{"id":"2","content":"без автора"}]"#;
        let msgs = parse_history_page(body, "c").unwrap();
        assert_eq!(msgs.len(), 1, "сообщение без author пропускается");
        assert_eq!(msgs[0].id, "1");
    }

    /// Плохое значение в одном поле не должно ронять всю страницу: раньше
    /// `as_str().unwrap_or("")` тихо подставлял пустую строку.
    #[test]
    fn odd_field_types_do_not_break_the_page() {
        let body = r#"[{"id":7,"content":12345,"timestamp":null,
                        "author":{"id":"u","username":null,"avatar":null}},
                       {"id":"8","content":"ок","author":{"id":"u2","username":"n"}}]"#;
        let msgs = parse_history_page(body, "c").unwrap();
        assert_eq!(msgs.len(), 2, "оба сообщения должны остаться");
        assert_eq!(msgs[0].id, "7");
        assert_eq!(msgs[0].content, "12345");
        assert_eq!(msgs[0].timestamp, "");
        assert_eq!(msgs[0].author_name, "?", "нет username — как раньше показываем «?»");
        assert!(msgs[0].author_avatar.is_none());
    }

    /// Совсем пустой объект не должен ронять страницу.
    #[test]
    fn empty_and_missing_fields_survive() {
        let msgs = parse_history_page(r#"[{}]"#, "c").unwrap();
        assert!(msgs.is_empty(), "без автора сообщение не показываем");
        let msgs = parse_history_page(r#"[]"#, "c").unwrap();
        assert!(msgs.is_empty());
        assert_eq!(parse_history_page("не json", "c").is_err(), true, "битый JSON — ошибка разбора");
    }

    /// Обрезанное описание эмбеда должно уехать без пробелов, а нормальное —
    /// без лишней копии (содержимое не меняется).
    #[test]
    fn embed_description_is_trimmed() {
        let body = r#"[{"id":"1","content":"","author":{"id":"u","username":"n"},
                        "embeds":[{"description":"  текст  "},{"description":"   "}]}]"#;
        let msgs = parse_history_page(body, "c").unwrap();
        assert_eq!(msgs[0].embeds.len(), 1, "эмбед из одних пробелов выкидываем");
        assert_eq!(msgs[0].embeds[0].description.as_deref(), Some("текст"));
    }

    /// Вложение без url показать нечем — такое отбрасываем, а не ломаем страницу.
    #[test]
    fn attachment_without_url_is_dropped() {
        let body = r#"[{"id":"1","content":"","author":{"id":"u","username":"n"},
                        "attachments":[{"filename":"x.png"},{"url":"https://cdn.discordapp.com/a/1.png"}]}]"#;
        let msgs = parse_history_page(body, "c").unwrap();
        assert_eq!(msgs[0].attachments.len(), 1);
        assert_eq!(msgs[0].attachments[0].url, "https://cdn.discordapp.com/a/1.png");
    }

    /// Запасной разбор: если формат неожиданный (например, `author` пришёл
    /// не объектом), история всё равно показывается — пусть с пустыми полями.
    /// Строгий разбор на этом теле падает, а страница не должна пропадать
    /// целиком из-за одной строки.
    #[test]
    fn lenient_parse_survives_unexpected_shape() {
        let body = r#"[{"id":"1","content":"строка вместо объекта","author":"bob"},
                       {"id":"2","content":"нормальное","author":{"id":"u","username":"n"}}]"#;
        assert!(
            parse_history_page(body, "c").is_err(),
            "строгий разбор на этом должен ругаться — иначе тест бессмыслен"
        );
        let mut warns = Vec::new();
        let msgs = parse_history_page_lenient(body, "c", &mut |m| warns.push(m));
        assert_eq!(msgs.len(), 2, "оба сообщения должны показаться");
        assert_eq!(msgs[0].id, "1");
        assert_eq!(msgs[0].content, "строка вместо объекта");
        assert_eq!(msgs[1].author_name, "n");
        assert_eq!(warns.len(), 1, "о разборе запасным путём пишем в лог");
        assert!(warns[0].contains("fallback"), "лог должен говорить, что это запасной путь: {}", warns[0]);
    }

    /// Совсем нечитаемый ответ не должен ни паниковать, ни врать: пустой
    /// список и понятное сообщение в лог.
    #[test]
    fn lenient_parse_reports_garbage() {
        let mut warns = Vec::new();
        let msgs = parse_history_page_lenient("не json", "c", &mut |m| warns.push(m));
        assert!(msgs.is_empty());
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("History parse error"), "{}", warns[0]);
    }
}
