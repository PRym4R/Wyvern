use std::time::Duration;

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

pub(crate) async fn run_gateway(
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

/// Сколько сообщений тянуть за один раз. Discord отдаёт до 100, но начинать
/// надо с меньшего: пока грузится три страницы, пользователь смотрит в пустой
/// канал, а потом получает столько текста, что всё равно не прочитает. Как в
/// Discord — первые 50 сообщений сразу, дальше по мере прокрутки вверх.
pub(crate) const HISTORY_PAGE: usize = 50;

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
    event_tx: mpsc::UnboundedSender<ToApp>,
    channel_id: String,
    before: Option<String>,
) {
    let url = history_url(&channel_id, before.as_deref());

    let mut page: Vec<ChatMessage> = Vec::new();
    let mut got_page = false;
    let mut failed = false;
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
                    time::sleep(Duration::from_secs(retry)).await;
                    continue;
                }
                if !status.is_success() {
                    let _ = event_tx.send(ToApp::Debug(format!("History error {}", status)));
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
                    }
                }
            }
            Err(e) => {
                let _ = event_tx.send(ToApp::Debug(format!("History request error: {}", e)));
                time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    if failed || !got_page {
        // Ничего не пришло — снять «Loading…» должен вызывающий: он ждёт
        // ответ по этому каналу, а пустую страницу отправлять незачем.
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
    // Каналы, история которых уже грузится: защита от дублей при кликах.
    let history_inflight = std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new()));
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
                    ToGateway::Send { channel_id, content } => {
                        let url = format!("{}/channels/{}/messages", API_BASE, channel_id);
                        let req = http.post(&url)
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
                        tokio::spawn(async move {
                            {
                                let mut busy = inflight.lock().await;
                                if !busy.insert(cid.clone()) {
                                    let _ = ev.send(ToApp::Debug(format!(
                                        "History for {} already in flight, skipping",
                                        &cid[..cid.len().min(14)]
                                    )));
                                    return;
                                }
                            }
                            fetch_history_page(httpc, tkc, ev.clone(), cid.clone(), before).await;
                            inflight.lock().await.remove(&cid);
                        });
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
