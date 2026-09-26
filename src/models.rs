use std::collections::{HashMap, VecDeque};

use eframe::egui::{self, Color32, TextureHandle};
use serde_json::Value;

/// Кеш с ограничением размера: при переполнении вытесняется самый старый элемент.
/// Нужен, чтобы текстуры (они живут в памяти egui до конца сессии) не копились
/// бесконечно.
pub(crate) struct BoundedCache<V> {
    map: HashMap<String, V>,
    order: VecDeque<String>,
    cap: usize,
}

impl<V> BoundedCache<V> {
    pub(crate) fn new(cap: usize) -> Self {
        Self { map: HashMap::new(), order: VecDeque::new(), cap }
    }
    pub(crate) fn get(&self, key: &str) -> Option<&V> {
        self.map.get(key)
    }
    // Хелперы для тестов и отладки — в самом клиенте не вызываются.
    #[allow(dead_code)]
    pub(crate) fn contains_key(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }
    #[allow(dead_code)]
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }
    pub(crate) fn insert(&mut self, key: String, value: V) {
        if self.map.insert(key.clone(), value).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ChatMessage {
    pub(crate) id: String,
    pub(crate) channel_id: String,
    pub(crate) author_id: String,
    pub(crate) author_name: String,
    pub(crate) author_avatar: Option<String>,
    pub(crate) nickname: Option<String>,
    pub(crate) content: String,
    pub(crate) timestamp: String,
    pub(crate) attachments: Vec<Attachment>,
    pub(crate) embeds: Vec<Value>,
    pub(crate) is_own: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Attachment {
    pub(crate) filename: String,
    pub(crate) url: String,
    pub(crate) content_type: Option<String>,
    pub(crate) width: Option<u32>,
    pub(crate) height: Option<u32>,
    pub(crate) size: u64,
    pub(crate) description: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct ChatChannel {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) guild_id: Option<String>,
    pub(crate) channel_type: i64,
    pub(crate) topic: Option<String>,
    pub(crate) position: i32,
}

#[derive(Clone, Debug)]
pub(crate) struct Guild {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) icon: Option<String>,
    pub(crate) owner_id: String,
}

#[derive(Clone, Debug)]
pub(crate) struct UserProfile {
    pub(crate) id: String,
    pub(crate) username: String,
    pub(crate) avatar: Option<String>,
    pub(crate) discriminator: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredAccount {
    pub(crate) token: String,
    pub(crate) username: String,
}

#[derive(Clone)]
pub(crate) enum LoadedImage {
    Static(TextureHandle),
    Animated {
        frames: Vec<TextureHandle>,
        delays: Vec<f32>,
        started: std::time::Instant,
    },
}

pub(crate) enum ImagePayload {
    Static(egui::ColorImage),
    Animated { frames: Vec<(egui::ColorImage, f32)> },
}


impl LoadedImage {
    pub(crate) fn size_vec2(&self) -> egui::Vec2 {
        match self {
            LoadedImage::Static(t) => t.size_vec2(),
            LoadedImage::Animated { frames, .. } => frames[0].size_vec2(),
        }
    }

    pub(crate) fn id(&self) -> egui::TextureId {
        self.display_texture().id()
    }

    pub(crate) fn display_texture(&self) -> TextureHandle {
        match self {
            LoadedImage::Static(t) => t.clone(),
            LoadedImage::Animated { frames, delays, started } => {
                if frames.len() == 1 || delays.iter().all(|d| *d <= 0.0) {
                    return frames[0].clone();
                }
                let total: f32 = delays.iter().sum();
                let mut elapsed = started.elapsed().as_secs_f32();
                if total > 0.0 {
                    elapsed = elapsed % total;
                }
                let mut acc = 0.0f32;
                for (i, d) in delays.iter().enumerate() {
                    acc += d;
                    if elapsed < acc {
                        return frames[i].clone();
                    }
                }
                frames.last().cloned().unwrap_or_else(|| frames[0].clone())
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Theme {
    pub(crate) bg: Color32,
    pub(crate) panel_bg: Color32,
    pub(crate) channel_bg: Color32,
    pub(crate) text: Color32,
    pub(crate) text_secondary: Color32,
    pub(crate) accent: Color32,
    pub(crate) input_bg: Color32,
    pub(crate) message_hover: Color32,
    pub(crate) divider: Color32,
    pub(crate) self_bg: Color32,
}

impl Theme {
    pub(crate) fn dark() -> Self {
        Self {
            bg: Color32::from_rgb(49, 51, 56),
            panel_bg: Color32::from_rgb(42, 44, 48),
            channel_bg: Color32::from_rgb(30, 31, 34),
            text: Color32::from_rgb(220, 221, 222),
            text_secondary: Color32::from_rgb(114, 118, 125),
            accent: Color32::from_rgb(88, 101, 242),
            input_bg: Color32::from_rgb(64, 68, 75),
            message_hover: Color32::from_rgb(50, 52, 57),
            divider: Color32::from_rgb(66, 68, 72),
            self_bg: Color32::from_rgb(55, 58, 64),
        }
    }

    pub(crate) fn cyberpunk() -> Self {
        Self {
            bg: Color32::from_rgb(16, 12, 32),
            panel_bg: Color32::from_rgb(22, 17, 42),
            channel_bg: Color32::from_rgb(28, 22, 52),
            text: Color32::from_rgb(223, 226, 255),
            text_secondary: Color32::from_rgb(140, 142, 185),
            accent: Color32::from_rgb(0, 229, 255),
            input_bg: Color32::from_rgb(34, 28, 62),
            message_hover: Color32::from_rgb(32, 25, 58),
            divider: Color32::from_rgb(60, 52, 110),
            self_bg: Color32::from_rgb(36, 29, 66),
        }
    }

    pub(crate) fn light() -> Self {
        Self {
            bg: Color32::from_rgb(232, 234, 237),
            panel_bg: Color32::from_rgb(242, 243, 245),
            channel_bg: Color32::from_rgb(255, 255, 255),
            text: Color32::from_rgb(30, 31, 34),
            text_secondary: Color32::from_rgb(120, 122, 128),
            accent: Color32::from_rgb(70, 96, 220),
            input_bg: Color32::from_rgb(228, 230, 234),
            message_hover: Color32::from_rgb(240, 241, 244),
            divider: Color32::from_rgb(220, 222, 226),
            self_bg: Color32::from_rgb(235, 237, 241),
        }
    }
}
