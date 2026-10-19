use std::collections::{HashMap, HashSet, VecDeque};

use eframe::egui::{self, Color32, TextureHandle};
use serde_json::Value;

/// Memory a cached value occupies, so the cache can be limited by bytes, not item count.
pub(crate) trait CacheCost {
    fn cache_bytes(&self) -> usize;
}

impl CacheCost for TextureHandle {
    fn cache_bytes(&self) -> usize {
        let s = self.size();
        s[0].saturating_mul(s[1]).saturating_mul(4)
    }
}

/// Size-bounded cache evicting the oldest entries, capped by count and bytes.
///
/// Eviction drops the last `TextureHandle` reference, freeing the texture in egui too.
pub(crate) struct BoundedCache<V> {
    map: HashMap<String, V>,
    order: VecDeque<String>,
    cap: usize,
    budget: usize,
    bytes: usize,
    /// Keys drawn this or the previous frame; never evicted, so on-screen images don't flicker.
    used_now: HashSet<String>,
    used_prev: HashSet<String>,
}

impl<V: CacheCost> BoundedCache<V> {
    pub(crate) fn with_budget(cap: usize, budget: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap,
            budget,
            bytes: 0,
            used_now: HashSet::new(),
            used_prev: HashSet::new(),
        }
    }
    /// Begin a frame, keeping the previous visible set for one more frame of protection.
    pub(crate) fn begin_frame(&mut self) {
        self.used_prev = std::mem::take(&mut self.used_now);
    }
    /// Whether the image was visible this or the previous frame.
    fn is_visible(&self, key: &str) -> bool {
        self.used_now.contains(key) || self.used_prev.contains(key)
    }
    /// Mark an image as drawn this frame; protected from eviction through next frame.
    pub(crate) fn mark_visible(&mut self, key: &str) {
        if !self.used_now.contains(key) {
            self.used_now.insert(key.to_string());
        }
    }
    /// Get from the cache; access moves the key to the back (LRU).
    pub(crate) fn get(&mut self, key: &str) -> Option<&V> {
        self.touch(key);
        self.map.get(key)
    }
    /// Move a key to the back of the order queue; O(n) is fine for its small size.
    fn touch(&mut self, key: &str) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            if let Some(k) = self.order.remove(pos) {
                self.order.push_back(k);
            }
        }
    }
    /// Memory the cache currently holds.
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }
    // Test/debug helpers, not used by the client itself.
    #[allow(dead_code)]
    pub(crate) fn contains_key(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }
    #[allow(dead_code)]
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }
    pub(crate) fn insert(&mut self, key: String, value: V) {
        if let Some(old) = self.map.insert(key.clone(), value) {
            // Same key overwritten: subtract the old weight to avoid double counting.
            self.bytes = self.bytes.saturating_sub(old.cache_bytes());
        } else {
            self.order.push_back(key.clone());
        }
        let added = self.map[&key].cache_bytes();
        self.bytes = self.bytes.saturating_add(added);
        // Keep the last entry (even if over budget) and visible ones.
        while self.order.len() > self.cap || (self.bytes > self.budget && self.order.len() > 1) {
            // Don't evict the just-inserted image; it's about to appear.
            let Some(pos) = self.order.iter().position(|k| k != &key && !self.is_visible(k)) else {
                // Everything is on screen; exceeding the budget is better than flicker.
                break;
            };
            if let Some(old) = self.order.remove(pos) {
                if let Some(v) = self.map.remove(&old) {
                    self.bytes = self.bytes.saturating_sub(v.cache_bytes());
                }
                self.used_now.remove(&old);
                self.used_prev.remove(&old);
            }
        }
    }
    /// Full reset (account switch): map, queue and byte count.
    pub(crate) fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.bytes = 0;
        self.used_now.clear();
        self.used_prev.clear();
    }
}

/// Embed as the client can render it: image and text; other JSON fields are dropped.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Embed {
    pub(crate) image_url: Option<String>,
    /// Image size sent by Discord, used to compute message height without downloading.
    pub(crate) image_size: Option<[u32; 2]>,
    pub(crate) description: Option<String>,
}

impl Embed {
    /// Extract only what the client draws; `None` if there is nothing to draw.
    pub(crate) fn from_json(e: &Value) -> Option<Self> {
        let image = ["image", "thumbnail", "video"]
            .iter()
            .find_map(|f| e.get(*f).filter(|v| v.is_object()));
        let image_url = image
            .and_then(|v| v["url"].as_str())
            .filter(|u| !u.is_empty())
            .map(|u| u.to_string());
        let description = e["description"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        if image_url.is_none() && description.is_none() {
            return None;
        }
        let image_size = image.and_then(image_size_of);
        Some(Embed { image_url, image_size, description })
    }
}

/// Pixel size from `width`/`height`, tolerant of string or float-encoded numbers.
pub(crate) fn size_from(width: Option<u32>, height: Option<u32>) -> Option<[u32; 2]> {
    match (width, height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Some([w, h]),
        _ => None,
    }
}

/// Like `size_from`, but reading from a built `Value` tree; both paths must agree.
pub(crate) fn image_size_of(v: &Value) -> Option<[u32; 2]> {
    let num = |k: &str| -> Option<u32> {
        match v.get(k) {
            Some(Value::Number(n)) => n.as_u64().map(|x| x.min(u64::from(u32::MAX)) as u32),
            Some(Value::String(s)) => s.trim().parse::<u32>().ok(),
            _ => None,
        }
    };
    size_from(num("width"), num("height"))
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
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
    pub(crate) embeds: Vec<Embed>,
    /// The message this one replies to, reduced to what the client shows.
    pub(crate) reply: Option<ReplyRef>,
    pub(crate) is_own: bool,
}

/// A referenced (replied-to) message: the bits needed for the quote line above
/// the reply's own content.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct ReplyRef {
    pub(crate) author_name: String,
    pub(crate) preview: String,
}

/// One-line preview of a message: folded, trimmed and capped, for reply quotes.
pub(crate) fn message_preview(text: &str) -> String {
    const PREVIEW_CHARS: usize = 120;
    let trimmed = text.trim();
    let mut preview: String = trimmed.chars().take(PREVIEW_CHARS).collect();
    if trimmed.chars().count() > PREVIEW_CHARS {
        preview.push('…');
    }
    preview.replace(['\n', '\r'], " ")
}

/// Prefix for locally echoed message ids; never valid for lookups or pagination.
pub(crate) const LOCAL_ID_PREFIX: &str = "local:";

impl ChatMessage {
    /// Whether this is our own optimistic echo, not yet confirmed by Discord.
    pub(crate) fn is_local_echo(&self) -> bool {
        self.id.starts_with(LOCAL_ID_PREFIX)
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Attachment {
    pub(crate) url: String,
    pub(crate) content_type: Option<String>,
    pub(crate) description: Option<String>,
    /// Image size sent by Discord, used to compute message height without downloading.
    pub(crate) size: Option<[u32; 2]>,
}

#[derive(Clone, Debug)]
pub(crate) struct ChatChannel {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) guild_id: Option<String>,
    pub(crate) channel_type: i64,
    pub(crate) topic: Option<String>,
    pub(crate) position: i32,
    /// Category (type 4) this channel sits under; `None` for top-level and DMs.
    pub(crate) parent_id: Option<String>,
    /// Newest message id known for the channel, from Discord; used to compute
    /// unread state against `read_state` on startup.
    pub(crate) last_message_id: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct Guild {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) icon: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct UserProfile {
    pub(crate) id: String,
    pub(crate) username: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredAccount {
    pub(crate) token: String,
    pub(crate) username: String,
}

/// Redacts the token in `{:?}` output so debug logs and panics can't leak it.
impl std::fmt::Debug for StoredAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredAccount")
            .field("token", &"…")
            .field("username", &self.username)
            .finish()
    }
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

impl CacheCost for LoadedImage {
    fn cache_bytes(&self) -> usize {
        match self {
            LoadedImage::Static(t) => t.cache_bytes(),
            // An animated image is all its frames, not just one.
            LoadedImage::Animated { frames, .. } => frames.iter().map(|f| f.cache_bytes()).sum(),
        }
    }
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

    /// Whether the image animates; still images need no per-frame redraw.
    pub(crate) fn is_animated(&self) -> bool {
        match self {
            LoadedImage::Static(_) => false,
            LoadedImage::Animated { frames, delays, .. } => {
                frames.len() > 1 && delays.iter().any(|d| *d > 0.0)
            }
        }
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

/// Measured row height plus the chat width and grouping it was measured at;
/// stale after a resize or when a predecessor re-groups the row.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MsgHeight {
    pub(crate) height: f32,
    pub(crate) width: u32,
    pub(crate) grouped: bool,
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
            text: Color32::from_rgb(230, 232, 235),
            text_secondary: Color32::from_rgb(148, 155, 164),
            accent: Color32::from_rgb(88, 101, 242),
            input_bg: Color32::from_rgb(64, 68, 75),
            message_hover: Color32::from_rgb(50, 52, 57),
            divider: Color32::from_rgb(72, 75, 82),
            self_bg: Color32::from_rgb(55, 58, 64),
        }
    }

    pub(crate) fn cyberpunk() -> Self {
        Self {
            bg: Color32::from_rgb(18, 6, 38),
            panel_bg: Color32::from_rgb(26, 10, 52),
            channel_bg: Color32::from_rgb(34, 12, 66),
            text: Color32::from_rgb(245, 236, 255),
            text_secondary: Color32::from_rgb(180, 160, 220),
            accent: Color32::from_rgb(255, 45, 149),
            input_bg: Color32::from_rgb(42, 18, 82),
            message_hover: Color32::from_rgb(38, 15, 72),
            divider: Color32::from_rgb(80, 40, 130),
            self_bg: Color32::from_rgb(46, 20, 88),
        }
    }

    pub(crate) fn light() -> Self {
        Self {
            bg: Color32::from_rgb(232, 234, 237),
            panel_bg: Color32::from_rgb(245, 246, 248),
            channel_bg: Color32::from_rgb(255, 255, 255),
            text: Color32::from_rgb(28, 30, 34),
            text_secondary: Color32::from_rgb(96, 102, 112),
            accent: Color32::from_rgb(70, 96, 220),
            input_bg: Color32::from_rgb(226, 229, 234),
            message_hover: Color32::from_rgb(238, 240, 244),
            divider: Color32::from_rgb(212, 216, 222),
            self_bg: Color32::from_rgb(232, 235, 240),
        }
    }

    /// Deep navy with a soft electric-blue accent.
    pub(crate) fn midnight() -> Self {
        Self {
            bg: Color32::from_rgb(15, 20, 28),
            panel_bg: Color32::from_rgb(19, 26, 36),
            channel_bg: Color32::from_rgb(11, 15, 22),
            text: Color32::from_rgb(219, 228, 240),
            text_secondary: Color32::from_rgb(125, 140, 163),
            accent: Color32::from_rgb(76, 141, 255),
            input_bg: Color32::from_rgb(28, 38, 52),
            message_hover: Color32::from_rgb(24, 34, 48),
            divider: Color32::from_rgb(36, 48, 64),
            self_bg: Color32::from_rgb(26, 37, 50),
        }
    }

    /// Arctic blue-grey, the Nord palette.
    pub(crate) fn nord() -> Self {
        Self {
            bg: Color32::from_rgb(46, 52, 64),
            panel_bg: Color32::from_rgb(41, 46, 57),
            channel_bg: Color32::from_rgb(36, 41, 51),
            text: Color32::from_rgb(236, 239, 244),
            text_secondary: Color32::from_rgb(143, 155, 176),
            accent: Color32::from_rgb(136, 192, 208),
            input_bg: Color32::from_rgb(59, 66, 82),
            message_hover: Color32::from_rgb(52, 59, 73),
            divider: Color32::from_rgb(67, 76, 94),
            self_bg: Color32::from_rgb(59, 66, 82),
        }
    }

    /// Dracula: purple night with a lavender accent.
    pub(crate) fn dracula() -> Self {
        Self {
            bg: Color32::from_rgb(40, 42, 54),
            panel_bg: Color32::from_rgb(33, 34, 44),
            channel_bg: Color32::from_rgb(30, 31, 41),
            text: Color32::from_rgb(248, 248, 242),
            text_secondary: Color32::from_rgb(154, 160, 181),
            accent: Color32::from_rgb(255, 121, 198),
            input_bg: Color32::from_rgb(52, 55, 70),
            message_hover: Color32::from_rgb(47, 50, 65),
            divider: Color32::from_rgb(68, 71, 90),
            self_bg: Color32::from_rgb(56, 59, 77),
        }
    }

    /// Tokyo Night: ink blue with a neon periwinkle accent.
    pub(crate) fn tokyo_night() -> Self {
        Self {
            bg: Color32::from_rgb(26, 27, 38),
            panel_bg: Color32::from_rgb(22, 22, 30),
            channel_bg: Color32::from_rgb(31, 35, 53),
            text: Color32::from_rgb(192, 202, 245),
            text_secondary: Color32::from_rgb(127, 138, 181),
            accent: Color32::from_rgb(122, 162, 247),
            input_bg: Color32::from_rgb(41, 46, 66),
            message_hover: Color32::from_rgb(36, 40, 59),
            divider: Color32::from_rgb(59, 66, 97),
            self_bg: Color32::from_rgb(42, 47, 69),
        }
    }

    /// True-black OLED theme with an electric-violet accent.
    pub(crate) fn amoled() -> Self {
        Self {
            bg: Color32::from_rgb(0, 0, 0),
            panel_bg: Color32::from_rgb(5, 5, 7),
            channel_bg: Color32::from_rgb(10, 10, 13),
            text: Color32::from_rgb(242, 242, 245),
            text_secondary: Color32::from_rgb(138, 138, 150),
            accent: Color32::from_rgb(179, 136, 255),
            input_bg: Color32::from_rgb(20, 20, 24),
            message_hover: Color32::from_rgb(18, 18, 22),
            divider: Color32::from_rgb(36, 36, 44),
            self_bg: Color32::from_rgb(23, 23, 29),
        }
    }

    /// Catppuccin Mocha: warm purple-grey with a mauve accent.
    pub(crate) fn catppuccin() -> Self {
        Self {
            bg: Color32::from_rgb(30, 30, 46),
            panel_bg: Color32::from_rgb(24, 24, 37),
            channel_bg: Color32::from_rgb(17, 17, 27),
            text: Color32::from_rgb(205, 214, 244),
            text_secondary: Color32::from_rgb(166, 173, 200),
            accent: Color32::from_rgb(166, 227, 161),
            input_bg: Color32::from_rgb(49, 50, 68),
            message_hover: Color32::from_rgb(41, 42, 58),
            divider: Color32::from_rgb(69, 71, 90),
            self_bg: Color32::from_rgb(49, 50, 68),
        }
    }

    /// Gruvbox: warm retro colours with an orange accent.
    pub(crate) fn gruvbox() -> Self {
        Self {
            bg: Color32::from_rgb(40, 40, 40),
            panel_bg: Color32::from_rgb(29, 32, 33),
            channel_bg: Color32::from_rgb(50, 48, 47),
            text: Color32::from_rgb(235, 219, 178),
            text_secondary: Color32::from_rgb(168, 153, 132),
            accent: Color32::from_rgb(254, 128, 25),
            input_bg: Color32::from_rgb(60, 56, 54),
            message_hover: Color32::from_rgb(50, 48, 47),
            divider: Color32::from_rgb(80, 73, 69),
            self_bg: Color32::from_rgb(60, 56, 54),
        }
    }

    /// Aurora: teal dawn fading into deep blue.
    pub(crate) fn aurora() -> Self {
        Self {
            bg: Color32::from_rgb(7, 26, 28),
            panel_bg: Color32::from_rgb(8, 30, 38),
            channel_bg: Color32::from_rgb(8, 22, 30),
            text: Color32::from_rgb(217, 247, 239),
            text_secondary: Color32::from_rgb(127, 179, 168),
            accent: Color32::from_rgb(53, 224, 181),
            input_bg: Color32::from_rgb(13, 38, 48),
            message_hover: Color32::from_rgb(11, 32, 41),
            divider: Color32::from_rgb(22, 64, 74),
            self_bg: Color32::from_rgb(14, 42, 52),
        }
    }

    /// Sunset: violet dusk fading into wine red.
    pub(crate) fn sunset() -> Self {
        Self {
            bg: Color32::from_rgb(27, 11, 42),
            panel_bg: Color32::from_rgb(32, 16, 48),
            channel_bg: Color32::from_rgb(30, 12, 46),
            text: Color32::from_rgb(255, 233, 226),
            text_secondary: Color32::from_rgb(201, 154, 168),
            accent: Color32::from_rgb(255, 138, 76),
            input_bg: Color32::from_rgb(46, 21, 63),
            message_hover: Color32::from_rgb(42, 18, 56),
            divider: Color32::from_rgb(74, 33, 64),
            self_bg: Color32::from_rgb(51, 22, 63),
        }
    }

    /// Ocean: navy surface fading into deep teal.
    pub(crate) fn ocean() -> Self {
        Self {
            bg: Color32::from_rgb(4, 18, 31),
            panel_bg: Color32::from_rgb(6, 26, 43),
            channel_bg: Color32::from_rgb(5, 24, 39),
            text: Color32::from_rgb(220, 243, 255),
            text_secondary: Color32::from_rgb(127, 168, 190),
            accent: Color32::from_rgb(53, 208, 186),
            input_bg: Color32::from_rgb(10, 34, 51),
            message_hover: Color32::from_rgb(8, 29, 45),
            divider: Color32::from_rgb(19, 64, 80),
            self_bg: Color32::from_rgb(11, 37, 54),
        }
    }
}

/// Themes in selection order. The index is persisted in `Settings.theme_index`,
/// so existing entries must never be reordered.
pub(crate) const THEME_NAMES: [&str; 13] = [
    "Dark",
    "Cyberpunk",
    "Light",
    "Midnight",
    "Nord",
    "Dracula",
    "Tokyo Night",
    "AMOLED",
    "Catppuccin",
    "Gruvbox",
    "Aurora",
    "Sunset",
    "Ocean",
];

/// Resolves a persisted theme index; unknown indices fall back to Dark.
pub(crate) fn theme_by_index(index: u8) -> Theme {
    match index {
        1 => Theme::cyberpunk(),
        2 => Theme::light(),
        3 => Theme::midnight(),
        4 => Theme::nord(),
        5 => Theme::dracula(),
        6 => Theme::tokyo_night(),
        7 => Theme::amoled(),
        8 => Theme::catppuccin(),
        9 => Theme::gruvbox(),
        10 => Theme::aurora(),
        11 => Theme::sunset(),
        12 => Theme::ocean(),
        _ => Theme::dark(),
    }
}

/// Bottom colours `(window, chat)` for gradient themes; `None` for solid ones.
pub(crate) fn theme_gradient(index: u8) -> Option<(Color32, Color32)> {
    match index {
        10 => Some((
            Color32::from_rgb(10, 35, 64),
            Color32::from_rgb(11, 36, 64),
        )),
        11 => Some((
            Color32::from_rgb(58, 16, 32),
            Color32::from_rgb(66, 18, 42),
        )),
        12 => Some((
            Color32::from_rgb(6, 42, 58),
            Color32::from_rgb(8, 58, 74),
        )),
        _ => None,
    }
}

/// Fixtures for `src/memcheck.rs`; keep the measurement code identical across versions.
#[cfg(test)]
pub(crate) fn test_guild(id: &str, name: &str) -> Guild {
    Guild { id: id.into(), name: name.into(), icon: None }
}

#[cfg(test)]
pub(crate) fn test_user(id: &str, username: &str) -> UserProfile {
    UserProfile { id: id.into(), username: username.into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The token must not leak through debug output.
    #[test]
    fn stored_account_debug_hides_the_token() {
        let acc = StoredAccount {
            token: "секретный-токен-1234567890".into(),
            username: "вася".into(),
        };
        let shown = format!("{:?}", acc);
        assert!(
            !shown.contains("секретный-токен-1234567890"),
            "отладочный вывод напечатал токен: {shown}"
        );
        assert!(
            shown.contains("вася"),
            "имя в выводе оставить полезно: {shown}"
        );
    }

    /// Only the image and text are kept from an embed.
    #[test]
    fn embed_keeps_only_what_is_drawn() {
        let raw = json!({
            "type": "rich",
            "title": "Заголовок",
            "description": "  текст  ",
            "url": "https://example.com",
            "color": 123,
            "author": { "name": "Автор" },
            "footer": { "text": "подвал" },
            "provider": { "name": "Провайдер" },
            "fields": [{ "name": "n", "value": "v" }],
            "thumbnail": { "url": "https://cdn.discordapp.com/thumb.png" },
        });
        let e = Embed::from_json(&raw).expect("эмбед с картинкой и текстом должен остаться");
        assert_eq!(e.image_url.as_deref(), Some("https://cdn.discordapp.com/thumb.png"));
        assert_eq!(e.description.as_deref(), Some("текст"));
    }

    /// `image` is preferred over `thumbnail`.
    #[test]
    fn embed_prefers_image_over_thumbnail() {
        let raw = json!({
            "thumbnail": { "url": "https://cdn.discordapp.com/thumb.png" },
            "image": { "url": "https://cdn.discordapp.com/full.png" },
        });
        let e = Embed::from_json(&raw).expect("эмбед с картинкой должен остаться");
        assert_eq!(e.image_url.as_deref(), Some("https://cdn.discordapp.com/full.png"));
        assert!(e.description.is_none());
    }

    /// An embed with nothing to draw isn't stored at all.
    #[test]
    fn embed_without_drawable_content_is_dropped() {
        let raw = json!({
            "title": "только заголовок",
            "author": { "name": "Автор" },
            "description": "   ",
        });
        assert!(Embed::from_json(&raw).is_none());
    }

    /// The compact embed must be much cheaper than the raw JSON tree.
    #[test]
    fn compact_embed_is_cheaper_than_raw_json() {
        let raw = json!({
            "type": "rich",
            "description": "описание ".repeat(10),
            "url": "https://example.com/watch",
            "color": 0x3498db,
            "author": { "name": "Автор эмбеда", "url": "https://example.com", "icon_url": "https://cdn.discordapp.com/embed/avatars/1.png" },
            "footer": { "text": "подвал", "icon_url": "https://cdn.discordapp.com/embed/avatars/2.png" },
            "image": { "url": "https://cdn.discordapp.com/embed/picture.png", "width": 1600, "height": 1200 },
            "thumbnail": { "url": "https://cdn.discordapp.com/embed/thumb.png", "width": 300, "height": 300 },
            "provider": { "name": "Провайдер", "url": "https://example.com" },
            "fields": [
                { "name": "поле один", "value": "значение один", "inline": true },
                { "name": "поле два", "value": "значение два", "inline": false }
            ],
            "timestamp": "2026-09-26T12:00:00.000Z"
        });
        let compact = Embed::from_json(&raw).expect("эмбед должен остаться");
        // What the client actually retains: two strings plus two Options.
        let retained = compact.image_url.as_ref().map_or(0, |s| s.len())
            + compact.description.as_ref().map_or(0, |s| s.len())
            + 2 * std::mem::size_of::<Option<String>>();
        let raw_text = serde_json::to_string(&raw).unwrap();
        assert!(
            retained * 3 < raw_text.len(),
            "урезанный эмбед ({} байт) должен быть заметно дешевле сырого JSON ({} байт)",
            retained,
            raw_text.len()
        );
        // A serde_json::Value tree costs even more: every string/key is a separate allocation.
    }

    /// Evicting a texture frees it in egui too, so the cache's bytes are real texture memory.
    #[test]
    fn evicting_a_texture_frees_it_in_egui_too() {
        let ctx = egui::Context::default();
        let mut cache: BoundedCache<TextureHandle> = BoundedCache::with_budget(1, usize::MAX);

        let first = ctx.load_texture(
            "первая",
            egui::ColorImage::new([2, 2], Color32::WHITE),
            egui::TextureOptions::LINEAR,
        );
        let first_id = first.id();
        cache.insert("first".into(), first);
        assert!(
            ctx.tex_manager().read().meta(first_id).is_some(),
            "только что загруженная текстура должна быть в egui"
        );

        let second = ctx.load_texture(
            "вторая",
            egui::ColorImage::new([2, 2], Color32::WHITE),
            egui::TextureOptions::LINEAR,
        );
        cache.insert("second".into(), second);
        assert!(!cache.contains_key("first"), "старая запись должна вытесниться");
        assert!(
            ctx.tex_manager().read().meta(first_id).is_none(),
            "вытесненная текстура должна освободиться и в egui"
        );
    }
}
