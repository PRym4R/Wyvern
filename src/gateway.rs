use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::messages::{ToApp, ToGateway};
use crate::models::{Attachment, ChatChannel, ChatMessage, Guild, UserProfile};
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
                                        let gowner = g["owner_id"].as_str().unwrap_or("").to_string();
                                        let _ = event_tx.send(ToApp::Guild(Guild {
                                            id: gid.clone(),
                                            name: gname.clone(),
                                            icon: gicon,
                                            owner_id: gowner,
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
                                                                    avatar: u["avatar"].as_str().map(|s| s.to_string()),
                                                                    discriminator: u["discriminator"].as_str().unwrap_or("").to_string(),
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
                                let d = &v["d"];
                                let author = &d["author"];
                                let msg = ChatMessage {
                                    id: d["id"].as_str().unwrap_or("").to_string(),
                                    channel_id: d["channel_id"].as_str().unwrap_or("").to_string(),
                                    author_id: author["id"].as_str().unwrap_or("").to_string(),
                                    author_name: author["username"].as_str().unwrap_or("?").to_string(),
                                    author_avatar: author["avatar"].as_str().map(|s| s.to_string()),
                                    nickname: None,
                                    content: d["content"].as_str().unwrap_or("").to_string(),
                                    timestamp: d["timestamp"].as_str().unwrap_or("").to_string(),
                                    attachments: d["attachments"].as_array().map(|arr| {
                                        arr.iter().filter_map(|a| {
                                            Some(Attachment {
                                                filename: a["filename"].as_str()?.to_string(),
                                                url: a["url"].as_str()?.to_string(),
                                                content_type: a["content_type"].as_str().map(|s| s.to_string()),
                                                width: a["width"].as_u64().map(|v| v as u32),
                                                height: a["height"].as_u64().map(|v| v as u32),
                                                size: a["size"].as_u64().unwrap_or(0),
                                                description: a["description"].as_str().map(|s| s.to_string()),
                                            })
                                        }).collect()
                                    }).unwrap_or_default(),
                                    embeds: d["embeds"].as_array().cloned().unwrap_or_default(),
                                    is_own: false,
                                };
                                let _ = event_tx.send(ToApp::Message(msg));
                            }
                            "GUILD_CREATE" => {
                                let d = &v["d"];
                                let guild = Guild {
                                    id: d["id"].as_str().unwrap_or("").to_string(),
                                    name: d["name"].as_str().unwrap_or("Unknown").to_string(),
                                    icon: d["icon"].as_str().map(|s| s.to_string()),
                                    owner_id: d["owner_id"].as_str().unwrap_or("").to_string(),
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
                        let mut req = http.post(&url)
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
                    ToGateway::FetchHistory { channel_id } => {
                        let cid_for_msg = channel_id.clone();
                        let mut all: Vec<ChatMessage> = Vec::new();
                        let mut before: Option<String> = None;
                        let mut done = false;
                        let mut failed = false;

                        while !done {
                            let url = match &before {
                                Some(b) => format!("{}/channels/{}/messages?limit=100&before={}", API_BASE, channel_id, b),
                                None => format!("{}/channels/{}/messages?limit=100", API_BASE, channel_id),
                            };
                            let mut attempt = 0u32;
                            let mut page_ok = false;
                            while !page_ok && attempt < 3 {
                                attempt += 1;
                                let req = http.get(&url)
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
                                            let retry = resp.headers()
                                                .get("retry-after")
                                                .and_then(|v| v.to_str().ok())
                                                .and_then(|s| s.parse::<u64>().ok())
                                                .unwrap_or(2);
                                            let _ = event_tx.send(ToApp::Debug(format!("History 429, retrying in {}s", retry)));
                                            tokio::time::sleep(Duration::from_secs(retry)).await;
                                            continue;
                                        }
                                        if status.is_success() {
                                            match resp.text().await {
                                                Ok(body) => {
                                                    match serde_json::from_str::<Vec<Value>>(&body) {
                                                        Ok(arr) => {
                                                            if arr.is_empty() {
                                                                page_ok = true;
                                                                done = true;
                                                                break;
                                                            }
                                                            let mut page: Vec<ChatMessage> = arr.iter().filter_map(|m| {
                                                                let author = m.get("author")?;
                                                                Some(ChatMessage {
                                                                    id: m["id"].as_str().unwrap_or("").to_string(),
                                                                    channel_id: cid_for_msg.clone(),
                                                                    author_id: author["id"].as_str().unwrap_or("").to_string(),
                                                                    author_name: author["username"].as_str().unwrap_or("?").to_string(),
                                                                    author_avatar: author["avatar"].as_str().map(|s| s.to_string()),
                                                                    nickname: None,
                                                                    content: m["content"].as_str().unwrap_or("").to_string(),
                                                                    timestamp: m["timestamp"].as_str().unwrap_or("").to_string(),
                                                                    attachments: m["attachments"].as_array().map(|arr| {
                                                                        arr.iter().filter_map(|a| {
                                                                            Some(Attachment {
                                                                                filename: a["filename"].as_str()?.to_string(),
                                                                                url: a["url"].as_str()?.to_string(),
                                                                                content_type: a["content_type"].as_str().map(|s| s.to_string()),
                                                                                width: a["width"].as_u64().map(|v| v as u32),
                                                                                height: a["height"].as_u64().map(|v| v as u32),
                                                                                size: a["size"].as_u64().unwrap_or(0),
                                                                                description: a["description"].as_str().map(|s| s.to_string()),
                                                                            })
                                                                        }).collect()
                                                                    }).unwrap_or_default(),
                                                                    embeds: m["embeds"].as_array().cloned().unwrap_or_default(),
                                                                    is_own: false,
                                                                })
                                                            }).collect();
                                                            if page.is_empty() {
                                                                page_ok = true;
                                                                done = true;
                                                                break;
                                                            }
                                                            before = page.first().map(|m| m.id.clone());
                                                            all.extend(page);
                                                            page_ok = true;
                                                            if all.len() >= 300 {
                                                                done = true;
                                                                break;
                                                            }
                                                            tokio::time::sleep(Duration::from_millis(300)).await;
                                                        }
                                                        Err(e) => {
                                                            let _ = event_tx.send(ToApp::Debug(format!("History parse error: {}", e)));
                                                        }
                                                    }
                                                }
                                                Err(e) => {
                                                    let _ = event_tx.send(ToApp::Debug(format!("History body error: {}", e)));
                                                }
                                            }
                                        } else {
                                            let _ = event_tx.send(ToApp::Debug(format!("History error {}", status)));
                                            failed = true;
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        let _ = event_tx.send(ToApp::Debug(format!("History request error: {}", e)));
                                        tokio::time::sleep(Duration::from_secs(2)).await;
                                    }
                                }
                                if failed {
                                    break;
                                }
                            }
                            if failed {
                                done = true;
                            }
                        }

                        let _ = event_tx.send(ToApp::Debug(format!("History: {} messages total", all.len())));
                        all.reverse();
                        let _ = event_tx.send(ToApp::History { channel_id: cid_for_msg.clone(), messages: all });
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
