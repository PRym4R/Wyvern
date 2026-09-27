use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use eframe::egui::{self, TextureHandle};

use crate::app::{App, MAX_FAILED_IMAGES};
use crate::models::{ImagePayload, LoadedImage};

const CDN_BASE: &str = "https://cdn.discordapp.com";
const MAX_CONCURRENT_AVATAR_DOWNLOADS: usize = 4;
/// Сколько картинок качается одновременно. Без потолка канал с полсотней
/// картинок порождает полсотню потоков, и все они одновременно держат в
/// памяти декодированные пиксели — это сотни мегабайт на ровном месте.
const MAX_CONCURRENT_IMAGE_DOWNLOADS: usize = 3;
static AVATAR_DOWNLOADS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
static IMAGE_DOWNLOADS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
/// Большую картинку нет смысла хранить в полном разрешении: в чате она
/// рисуется максимум 360x360, то есть даже на HiDPI-экране (2x) 768 пикселей
/// хватает ровно. 1536 превращали одну фотку в 9 МБ текстуры.
const MAX_IMAGE_DIM: u32 = 768;
/// Гифка — это сразу много кадров, поэтому кадры мельче. 16 кадров по 448
/// пикселей — это 9 МБ на анимацию вместо 28.
const MAX_GIF_DIM: u32 = 448;
const MAX_GIF_FRAMES: usize = 16;

/// Один общий клиент на всё приложение: свой `Client` на каждую картинку —
/// это новый пул соединений и TLS-сессия на каждый запрос.
fn http() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(20))
            .pool_max_idle_per_host(8)
            .build()
            .expect("не собрать HTTP-клиент")
    })
}

/// Уменьшить картинку, если она заметно больше нужного.
fn shrink(img: image::DynamicImage, max_dim: u32) -> image::DynamicImage {
    if img.width() > max_dim || img.height() > max_dim {
        img.thumbnail(max_dim, max_dim)
    } else {
        img
    }
}

fn remember_failed(failed: &mut std::collections::HashSet<String>, key: String) {
    if failed.len() >= MAX_FAILED_IMAGES {
        failed.clear();
    }
    failed.insert(key);
}

/// Занять место в лимите одновременных загрузок картинок. `false` — лимит
/// исчерпан, тогда загрузку лучше отложить до следующего кадра (картинка
/// попадёт в кэш и больше не будет качаться заново).
fn take_image_slot() -> bool {
    IMAGE_DOWNLOADS_IN_FLIGHT.fetch_add(1, Ordering::SeqCst) < MAX_CONCURRENT_IMAGE_DOWNLOADS
}

fn release_image_slot() {
    // saturating: если счётчик когда-то уйдёт в ноль, лучше он останется
    // нулём, чем завертится и больше не ограничит ничего.
    let _ = IMAGE_DOWNLOADS_IN_FLIGHT.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
        Some(v.saturating_sub(1))
    });
}

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
                if let Ok(resp) = http().get(&url_moved).send() {
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
                if let Ok(resp) = http().get(&url_moved).send() {
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
                    for fr in frames.into_iter().take(MAX_GIF_FRAMES) {
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
                        let frame = image::DynamicImage::ImageRgba8(buf);
                        let rgba = shrink(frame, MAX_GIF_DIM).into_rgba8();
                        let (fw, fh) = rgba.dimensions();
                        if fw == 0 || fh == 0 {
                            return None;
                        }
                        let pixels = rgba.into_raw();
                        let ci = egui::ColorImage::from_rgba_unmultiplied(
                            [fw as usize, fh as usize],
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
        let img = shrink(img, MAX_IMAGE_DIM);
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
        remember_failed(&mut self.failed_images, key);
        return None;
    }

    // Не начинаем новую загрузку, если лимит уже выбран: лишний поток только
    // зря съест память на декодирование. На следующем кадре попробуем снова.
    if !self.pending_images.contains_key(&cache_key) && !take_image_slot() {
        return None;
    }

    let pending = self.pending_images.entry(cache_key.clone()).or_insert_with(|| {
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let url_moved = url.to_string();
        std::thread::spawn(move || {
            if let Ok(resp) = http().get(&url_moved).send() {
                if let Ok(bytes) = resp.bytes() {
                    if let Some(payload) = Self::decode_image_payload(&bytes) {
                        release_image_slot();
                        let _ = result_tx.send(Some(payload));
                        return;
                    }
                }
            }
            release_image_slot();
            let _ = result_tx.send(None);
        });
        result_rx
    });

    match pending.try_recv() {
        Ok(result) => {
            self.pending_images.remove(&key);
            match result {
                Some(ImagePayload::Static(color_image)) => {
                    let handle = ctx2.load_texture(&key, color_image, egui::TextureOptions::LINEAR);
                    let loaded = LoadedImage::Static(handle);
                    self.image_cache.insert(key.clone(), loaded.clone());
                    self.push_debug(format!(
                        "IMG: {} в кэше — {} шт, {:.1} МБ текстур",
                        key,
                        self.image_cache.len(),
                        self.image_cache.bytes() as f64 / (1024.0 * 1024.0)
                    ));
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
                        let frames_count = handles.len();
                        let loaded = LoadedImage::Animated {
                            frames: handles,
                            delays,
                            started: std::time::Instant::now(),
                        };
                        self.image_cache.insert(key.clone(), loaded.clone());
                        self.push_debug(format!(
                            "GIF: {} кадров в кэше — {} шт, {:.1} МБ текстур",
                            frames_count,
                            self.image_cache.len(),
                            self.image_cache.bytes() as f64 / (1024.0 * 1024.0)
                        ));
                        ctx2.request_repaint();
                        return Some(loaded);
                    }
                }
                None => {
                    remember_failed(&mut self.failed_images, key);
                }
            }
        }
        // Поток ещё работает — подождём следующего кадра.
        Err(std::sync::mpsc::TryRecvError::Empty) => {}
        // Поток умер, не ответив. Запись из карты убираем, иначе картинка
        // больше никогда не попробует скачаться, а память под этот ключ
        // утекает.
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            release_image_slot();
            self.pending_images.remove(&key);
        }
    }

    None
}
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageEncoder, Rgb, RgbImage, Rgba, RgbaImage};

    /// Большая картинка должна уменьшаться до лимита, иначе одна фотка
    /// в 12 МБ съедает столько же VRAM/RAM.
    #[test]
    fn big_image_is_shrunk() {
        let img = RgbImage::from_pixel(3000, 2000, Rgb([10, 20, 30]));
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(img.as_raw(), 3000, 2000, image::ExtendedColorType::Rgb8)
            .unwrap();
        eprintln!("[TEST] исходный PNG: {} КБ", png.len() / 1024);

        match App::decode_image_payload(&png) {
            Some(ImagePayload::Static(ci)) => {
                assert!(
                    ci.size[0] <= MAX_IMAGE_DIM as usize && ci.size[1] <= MAX_IMAGE_DIM as usize,
                    "картинка не ужата: {:?}",
                    ci.size
                );
                // Пропорции сохранены.
                let ratio = ci.size[0] as f32 / ci.size[1] as f32;
                assert!((ratio - 1.5).abs() < 0.05, "пропорции сломаны: {}", ratio);
                eprintln!("[TEST] после ужатия: {:?}", ci.size);
            }
            other => panic!("ожидалась статичная картинка, получено {:?}", other.is_some()),
        }
    }

    /// Маленькую картинку трогать не надо.
    #[test]
    fn small_image_keeps_size() {
        let img = RgbImage::new(64, 48);
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(img.as_raw(), 64, 48, image::ExtendedColorType::Rgb8)
            .unwrap();
        match App::decode_image_payload(&png) {
            Some(ImagePayload::Static(ci)) => assert_eq!(ci.size, [64, 48]),
            other => panic!("ожидалась статичная картинка, получено {:?}", other.is_some()),
        }
    }

    /// Гифка: кадров не больше лимита, каждый кадр ужат.
    #[test]
    fn gif_frames_are_capped() {
        let (w, h, n) = (240u32, 160u32, 30usize);
        let mut out = Vec::new();
        {
            let mut enc = image::codecs::gif::GifEncoder::new(&mut out);
            for f in 0..n {
                let frame = RgbaImage::from_pixel(w, h, Rgba([(f * 7) as u8, 40, 90, 255]));
                enc.encode_frame(image::Frame::from_parts(
                    frame,
                    0,
                    0,
                    image::Delay::from_numer_denom_ms(50, 1),
                )).unwrap();
            }
        }
        eprintln!("[TEST] GIF {}x{} {} кадров = {} КБ", w, h, n, out.len() / 1024);

        match App::decode_image_payload(&out) {
            Some(ImagePayload::Animated { frames }) => {
                assert!(frames.len() <= MAX_GIF_FRAMES, "кадров слишком много: {}", frames.len());
                for (ci, _) in &frames {
                    assert!(ci.size[0] <= MAX_GIF_DIM as usize && ci.size[1] <= MAX_GIF_DIM as usize,
                        "кадр не ужат: {:?}", ci.size);
                }
                eprintln!("[TEST] осталось кадров: {}, размер {:?}", frames.len(), frames[0].0.size);
            }
            other => panic!("ожидалась анимация, получено {:?}", other.is_some()),
        }
    }
}

#[cfg(test)]
impl App {
    /// Тот же путь, что и в `download_image` после получения ответа, только
    /// без сети: декодируем байты и кладём результат в кэш по его политике.
    pub(crate) fn cache_image_bytes(&mut self, ctx: &egui::Context, key: &str, bytes: &[u8]) -> bool {
        match Self::decode_image_payload(bytes) {
            Some(ImagePayload::Static(color_image)) => {
                let handle = ctx.load_texture(key, color_image, egui::TextureOptions::LINEAR);
                self.image_cache
                    .insert(key.to_string(), LoadedImage::Static(handle));
                true
            }
            Some(ImagePayload::Animated { frames }) => {
                let mut handles = Vec::with_capacity(frames.len());
                let mut delays = Vec::with_capacity(frames.len());
                for (i, (ci, delay)) in frames.into_iter().enumerate() {
                    handles.push(ctx.load_texture(format!("{}#f{}", key, i), ci, egui::TextureOptions::LINEAR));
                    delays.push(delay);
                }
                if handles.len() > 1 {
                    self.image_cache.insert(
                        key.to_string(),
                        LoadedImage::Animated { frames: handles, delays, started: std::time::Instant::now() },
                    );
                    true
                } else {
                    false
                }
            }
            None => false,
        }
    }
}
