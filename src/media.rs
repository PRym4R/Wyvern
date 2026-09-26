use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use eframe::egui::{self, TextureHandle};

use crate::app::App;
use crate::models::{ImagePayload, LoadedImage};

const CDN_BASE: &str = "https://cdn.discordapp.com";
const MAX_CONCURRENT_AVATAR_DOWNLOADS: usize = 4;
static AVATAR_DOWNLOADS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

impl App {
    pub(crate) fn download_avatar(&mut self, ctx: &egui::Context, user_id: &str, avatar_hash: &str) -> Option<TextureHandle> {
        let cache_key = format!("{}_{}", user_id, avatar_hash);
        if let Some(tex) = self.avatar_cache.get(&cache_key) {
            return Some(tex.clone());
        }

        let url = format!("{}/avatars/{}/{}.png?size=64", CDN_BASE, user_id, avatar_hash);
        let ctx2 = ctx.clone();
        let key = cache_key.clone();

        let pending = self.pending_avatars.entry(cache_key.clone()).or_insert_with(|| {
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let url_moved = url.clone();
            std::thread::spawn(move || {
                if AVATAR_DOWNLOADS_IN_FLIGHT.fetch_add(1, Ordering::SeqCst) >= MAX_CONCURRENT_AVATAR_DOWNLOADS {
                    AVATAR_DOWNLOADS_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
                    let _ = result_tx.send(None);
                    return;
                }
                let client = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(10))
                    .build()
                    .unwrap();
                if let Ok(resp) = client.get(&url_moved).send() {
                    if let Ok(bytes) = resp.bytes() {
                        if let Ok(img) = image::load_from_memory(&bytes) {
                            let rgba = img.to_rgba8();
                            let (w, h) = rgba.dimensions();
                            let pixels = rgba.into_raw();
                            let color_image = egui::ColorImage::from_rgba_unmultiplied(
                                [w as usize, h as usize],
                                &pixels,
                            );
                            let _ = result_tx.send(Some(color_image));
                            AVATAR_DOWNLOADS_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
                            return;
                        }
                    }
                }
                AVATAR_DOWNLOADS_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
                let _ = result_tx.send(None);
            });
            result_rx
        });

        if let Ok(result) = pending.try_recv() {
            self.pending_avatars.remove(&key);
            if let Some(color_image) = result {
                let handle = ctx2.load_texture(&key, color_image, egui::TextureOptions::default());
                self.avatar_cache.insert(key.clone(), handle.clone());
                ctx2.request_repaint();
                return Some(handle);
            }
        }

        None
    }
    pub(crate) fn download_guild_icon(&mut self, ctx: &egui::Context, guild_id: &str, icon_hash: &str) -> Option<TextureHandle> {
        let cache_key = format!("guild_icon_{}_{}", guild_id, icon_hash);
        if let Some(tex) = self.avatar_cache.get(&cache_key) {
            return Some(tex.clone());
        }

        let url = format!("{}/icons/{}/{}.png?size=64", CDN_BASE, guild_id, icon_hash);
        let ctx2 = ctx.clone();
        let key = cache_key.clone();

        let pending = self.pending_avatars.entry(cache_key.clone()).or_insert_with(|| {
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let url_moved = url.clone();
            std::thread::spawn(move || {
                if AVATAR_DOWNLOADS_IN_FLIGHT.fetch_add(1, Ordering::SeqCst) >= MAX_CONCURRENT_AVATAR_DOWNLOADS {
                    AVATAR_DOWNLOADS_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
                    let _ = result_tx.send(None);
                    return;
                }
                let client = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(10))
                    .build()
                    .unwrap();
                if let Ok(resp) = client.get(&url_moved).send() {
                    if let Ok(bytes) = resp.bytes() {
                        if let Ok(img) = image::load_from_memory(&bytes) {
                            let rgba = img.to_rgba8();
                            let (w, h) = rgba.dimensions();
                            let pixels = rgba.into_raw();
                            let color_image = egui::ColorImage::from_rgba_unmultiplied(
                                [w as usize, h as usize],
                                &pixels,
                            );
                            let _ = result_tx.send(Some(color_image));
                            AVATAR_DOWNLOADS_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
                            return;
                        }
                    }
                }
                AVATAR_DOWNLOADS_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
                let _ = result_tx.send(None);
            });
            result_rx
        });

        if let Ok(result) = pending.try_recv() {
            self.pending_avatars.remove(&key);
            if let Some(color_image) = result {
                let handle = ctx2.load_texture(&key, color_image, egui::TextureOptions::default());
                self.avatar_cache.insert(key.clone(), handle.clone());
                ctx2.request_repaint();
                return Some(handle);
            }
        }

        None
    }
    pub(crate) fn decode_image_payload(bytes: &[u8]) -> Option<ImagePayload> {
    if bytes.len() < 6 {
        return None;
    }
    let is_gif = &bytes[..6] == b"GIF89a" || &bytes[..6] == b"GIF87a";
    if is_gif {
            if let Ok(decoder) = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)) {
                use image::AnimationDecoder;
                if let Ok(frames) = decoder.into_frames().collect_frames() {
                if frames.len() > 1 {
                    let mut out = Vec::new();
                    for fr in frames {
                        let (num, den) = fr.delay().numer_denom_ms();
                        let secs = if den == 0 {
                            0.1
                        } else {
                            (num as f64 / den as f64) / 1000.0
                        };
                        let buf = fr.into_buffer();
                        let (w, h) = buf.dimensions();
                        if w == 0 || h == 0 {
                            return None;
                        }
                        let pixels = buf.into_raw();
                        let ci = egui::ColorImage::from_rgba_unmultiplied(
                            [w as usize, h as usize],
                            &pixels,
                        );
                        out.push((ci, secs.max(0.02) as f32));
                    }
                    return Some(ImagePayload::Animated { frames: out });
                }
            }
        }
    }
    if let Ok(img) = image::load_from_memory(bytes) {
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        if w == 0 || h == 0 {
            return None;
        }
        let pixels = rgba.into_raw();
        let color_image = egui::ColorImage::from_rgba_unmultiplied(
            [w as usize, h as usize],
            &pixels,
        );
        return Some(ImagePayload::Static(color_image));
    }
    None
}
pub(crate) fn download_image(&mut self, ctx: &egui::Context, url: &str) -> Option<LoadedImage> {
    let cache_key = url.to_string();
    if let Some(img) = self.image_cache.get(&cache_key) {
        return Some(img.clone());
    }
    if self.failed_images.contains(&cache_key) {
        return None;
    }

    let ctx2 = ctx.clone();
    let key = cache_key.clone();

    let lower = url.to_lowercase();
    if lower.contains("/avatars/") || lower.contains("/users/") {
        self.failed_images.insert(key);
        return None;
    }

    let pending = self.pending_images.entry(cache_key.clone()).or_insert_with(|| {
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let url_moved = url.to_string();
        std::thread::spawn(move || {
            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .unwrap();
            if let Ok(resp) = client.get(&url_moved).send() {
                if let Ok(bytes) = resp.bytes() {
                    if let Some(payload) = Self::decode_image_payload(&bytes) {
                        let _ = result_tx.send(Some(payload));
                        return;
                    }
                }
            }
            let _ = result_tx.send(None);
        });
        result_rx
    });

    if let Ok(result) = pending.try_recv() {
        self.pending_images.remove(&key);
        match result {
            Some(ImagePayload::Static(color_image)) => {
                let handle = ctx2.load_texture(&key, color_image, egui::TextureOptions::LINEAR);
                let loaded = LoadedImage::Static(handle);
                self.image_cache.insert(key.clone(), loaded.clone());
                ctx2.request_repaint();
                return Some(loaded);
            }
            Some(ImagePayload::Animated { frames }) => {
                let mut handles = Vec::with_capacity(frames.len());
                let mut delays = Vec::with_capacity(frames.len());
                for (i, (ci, delay)) in frames.into_iter().enumerate() {
                    let tkey = format!("{}#f{}", key, i);
                    handles.push(ctx2.load_texture(&tkey, ci, egui::TextureOptions::LINEAR));
                    delays.push(delay);
                }
                if handles.len() > 1 {
                    let loaded = LoadedImage::Animated {
                        frames: handles,
                        delays,
                        started: std::time::Instant::now(),
                    };
                    self.image_cache.insert(key.clone(), loaded.clone());
                    ctx2.request_repaint();
                    return Some(loaded);
                }
            }
            None => {
                self.failed_images.insert(key);
            }
        }
    }

    None
}
}
