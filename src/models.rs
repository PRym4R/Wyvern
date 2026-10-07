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
#[derive(Clone, Debug, Default)]
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
    pub(crate) embeds: Vec<Embed>,
    pub(crate) is_own: bool,
}

/// Prefix for locally echoed message ids; never valid for lookups or pagination.
pub(crate) const LOCAL_ID_PREFIX: &str = "local:";

impl ChatMessage {
    /// Whether this is our own optimistic echo, not yet confirmed by Discord.
    pub(crate) fn is_local_echo(&self) -> bool {
        self.id.starts_with(LOCAL_ID_PREFIX)
    }
}

#[derive(Clone, Debug)]
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

/// Measured row height plus the chat width it was measured at; stale after resize.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MsgHeight {
    pub(crate) height: f32,
    pub(crate) width: u32,
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
