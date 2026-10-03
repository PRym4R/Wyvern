use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use eframe::egui::{self, TextureHandle};

use crate::app::{App, MAX_FAILED_IMAGES};
use crate::models::{ImagePayload, LoadedImage};

const CDN_BASE: &str = "https://cdn.discordapp.com";
const MAX_CONCURRENT_AVATAR_DOWNLOADS: usize = 4;
/// Caps concurrent image downloads so decoded pixels don't pile up in memory.
const MAX_CONCURRENT_IMAGE_DOWNLOADS: usize = 3;
static AVATAR_DOWNLOADS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
static IMAGE_DOWNLOADS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
/// Chat renders images at most 360x360, so 768px covers even HiDPI (2x) screens.
const MAX_IMAGE_DIM: u32 = 768;
/// GIFs hold many frames, so frames are smaller to stay within budget.
const MAX_GIF_DIM: u32 = 448;
/// Floor for long GIFs: downscaled frames beat a truncated animation.
const MIN_GIF_DIM: u32 = 64;
/// Memory budget for one GIF's frames; frame counts vary, so budget in bytes.
const MAX_GIF_BYTES: usize = 32 * 1024 * 1024;
/// Hard frame-count cap; thousands of textures are unacceptable.
const MAX_GIF_FRAMES: usize = 2000;
/// Max source pixels to decode; decoding uses the original, not the display size.
const MAX_SOURCE_PIXELS: u64 = 40_000_000;
/// Cap on response bytes; pixel size is only checked after the body is read.
const MAX_DOWNLOAD_BYTES: u64 = MAX_SOURCE_PIXELS * 8;

/// Read the response body, at most `max` bytes; `None` if it exceeds that.
fn read_limited(resp: reqwest::blocking::Response, max: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut buf = Vec::new();
    resp.take(max + 1).read_to_end(&mut buf).ok()?;
    if buf.len() as u64 > max {
        return None;
    }
    Some(buf)
}

/// Case-insensitive substring search without allocating a lowercased copy.
fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    !n.is_empty() && h.len() >= n.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// One shared client; per-image clients would create a new pool and TLS session.
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

/// Shrink the image if it exceeds the target dimension.
fn shrink(img: image::DynamicImage, max_dim: u32) -> image::DynamicImage {
    if img.width() > max_dim || img.height() > max_dim {
        img.thumbnail(max_dim, max_dim)
    } else {
        img
    }
}

/// Whether the source dimensions fit the limit; decoding uses source pixels.
pub(crate) fn source_size_allowed(w: u32, h: u32) -> bool {
    u64::from(w) * u64::from(h) <= MAX_SOURCE_PIXELS
}

/// Skip a GIF sub-block chain and return the position past its terminator.
fn skip_gif_sub_blocks(bytes: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let len = *bytes.get(pos)? as usize;
        pos = pos.checked_add(1)?;
        if len == 0 {
            return Some(pos);
        }
        pos = pos.checked_add(len)?;
        if pos > bytes.len() {
            return None;
        }
    }
}

/// Count GIF frames from the file structure without decoding pixels.
/// Needed up front to pick a frame size that keeps the whole GIF in budget.
pub(crate) fn gif_frame_count(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 13 || (&bytes[..6] != b"GIF89a" && &bytes[..6] != b"GIF87a") {
        return None;
    }
    // Bits 0–2 of the packed field encode the global color table size (2^(n+1)).
    let packed = bytes[10];
    let mut pos = 13usize;
    if packed & 0x80 != 0 {
        pos = pos.checked_add(3usize * (1usize << ((packed & 0x07) + 1)))?;
    }
    let mut count = 0usize;
    while pos < bytes.len() {
        match bytes[pos] {
            // End of file.
            0x3B => break,
            // Extension: 0x21 + label + sub-blocks.
            0x21 => {
                pos = skip_gif_sub_blocks(bytes, pos.checked_add(2)?)?;
            }
            // Frame: 0x2C + 9-byte descriptor, then optional local color table and LZW data.
            0x2C => {
                count += 1;
                if pos + 10 > bytes.len() {
                    return None;
                }
                let ipacked = bytes[pos + 9];
                pos += 10;
                if ipacked & 0x80 != 0 {
                    pos = pos.checked_add(3usize * (1usize << ((ipacked & 0x07) + 1)))?;
                }
                pos = pos.checked_add(1)?; // LZW minimum code size
                pos = skip_gif_sub_blocks(bytes, pos)?;
            }
            // Zero-byte padding between blocks.
            0x00 => pos += 1,
            // Unknown byte: treat the structure as not understood.
            _ => return None,
        }
    }
    Some(count)
}

/// Frame dimension that keeps all a GIF's frames within `MAX_GIF_BYTES`.
fn gif_frame_dim(frames: usize) -> u32 {
    let frames = frames.clamp(1, MAX_GIF_FRAMES);
    let per_frame = (MAX_GIF_BYTES / 4) / frames;
    let mut dim = (per_frame as f64).sqrt().floor() as u32;
    // Integer sqrt may overshoot by a pixel; step down until it fits.
    while dim > MIN_GIF_DIM
        && (dim as usize) * (dim as usize) * 4 * frames > MAX_GIF_BYTES
    {
        dim -= 1;
    }
    dim.clamp(MIN_GIF_DIM, MAX_GIF_DIM)
}

/// Record a failure, evicting one entry on overflow rather than clearing all.
fn remember_failed(failed: &mut std::collections::HashSet<String>, key: String) {
    if failed.len() >= MAX_FAILED_IMAGES {
        if let Some(victim) = failed.iter().next().cloned() {
            failed.remove(&victim);
        }
    }
    failed.insert(key);
}

/// Reserve an image download slot; `false` means the limit is reached.
/// Only increments the counter when a slot is actually granted.
fn take_image_slot() -> bool {
    IMAGE_DOWNLOADS_IN_FLIGHT
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
            if v < MAX_CONCURRENT_IMAGE_DOWNLOADS {
                Some(v + 1)
            } else {
                None
            }
        })
        .is_ok()
}

fn release_image_slot() {
    // saturating: keep the counter at zero instead of wrapping around.
    let _ = IMAGE_DOWNLOADS_IN_FLIGHT.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
        Some(v.saturating_sub(1))
    });
}

/// A slot that releases itself, even if the decoding thread panics.
struct ImageSlot;

impl Drop for ImageSlot {
    fn drop(&mut self) {
        release_image_slot();
    }
}

/// Avatar fetch result; a CDN failure is remembered, a busy slot is not.
#[derive(Debug, PartialEq)]
pub(crate) enum AvatarFetch {
    Ready(egui::ColorImage),
    /// Failed: network error, bad hash, or 404. Do not retry.
    Failed,
    /// No free download slot; retry immediately.
    Busy,
}

impl App {
    pub(crate) fn download_avatar(&mut self, ctx: &egui::Context, user_id: &str, avatar_hash: &str) -> Option<TextureHandle> {
        let url = format!("{}/avatars/{}/{}.png?size=64", CDN_BASE, user_id, avatar_hash);
        self.fetch_avatar(ctx, format!("{}_{}", user_id, avatar_hash), url)
    }
    /// Fetch an avatar or guild icon.
    /// Failures are remembered so a missing image isn't re-requested every frame.
    fn fetch_avatar(&mut self, ctx: &egui::Context, cache_key: String, url: String) -> Option<TextureHandle> {
        if let Some(tex) = self.avatar_cache.get(&cache_key) {
            return Some(tex.clone());
        }
        if self.failed_avatars.contains(&cache_key) {
            return None;
        }

        let ctx2 = ctx.clone();
        let key = cache_key.clone();

        let pending = self.pending_avatars.entry(cache_key.clone()).or_insert_with(|| {
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let url_moved = url.clone();
            std::thread::spawn(move || {
                if AVATAR_DOWNLOADS_IN_FLIGHT.fetch_add(1, Ordering::SeqCst) >= MAX_CONCURRENT_AVATAR_DOWNLOADS {
                    AVATAR_DOWNLOADS_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
                    // All slots busy isn't a real failure; don't remember it.
                    let _ = result_tx.send(AvatarFetch::Busy);
                    return;
                }
                let outcome = match http().get(&url_moved).send().ok()
                    .and_then(|resp| read_limited(resp, MAX_DOWNLOAD_BYTES))
                    .and_then(|bytes| image::load_from_memory(&bytes).ok())
                {
                    Some(img) => {
                        let rgba = img.to_rgba8();
                        let (w, h) = rgba.dimensions();
                        AvatarFetch::Ready(egui::ColorImage::from_rgba_unmultiplied(
                            [w as usize, h as usize],
                            &rgba.into_raw(),
                        ))
                    }
                    None => AvatarFetch::Failed,
                };
                AVATAR_DOWNLOADS_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
                let _ = result_tx.send(outcome);
            });
            result_rx
        });

        if let Ok(result) = pending.try_recv() {
            // Always drop the entry: the thread finished; failure memory decides.
            self.pending_avatars.remove(&key);
            return self.store_avatar(ctx2, key, result);
        }
        None
    }

    /// Handle an avatar fetch result; split out so failure rules are testable
    /// without the network.
    fn store_avatar(
        &mut self,
        ctx: egui::Context,
        key: String,
        answer: AvatarFetch,
    ) -> Option<TextureHandle> {
        match answer {
            AvatarFetch::Ready(color_image) => {
                let handle = ctx.load_texture(&key, color_image, egui::TextureOptions::default());
                self.avatar_cache.insert(key.clone(), handle.clone());
                ctx.request_repaint();
                Some(handle)
            }
            AvatarFetch::Failed => {
                // Real failure: remember the key, else the next frame refetches.
                remember_failed(&mut self.failed_avatars, key);
                None
            }
            // A busy slot isn't a failure; remembering it would hide the avatar.
            AvatarFetch::Busy => None,
        }
    }
    pub(crate) fn download_guild_icon(&mut self, ctx: &egui::Context, guild_id: &str, icon_hash: &str) -> Option<TextureHandle> {
        let url = format!("{}/icons/{}/{}.png?size=64", CDN_BASE, guild_id, icon_hash);
        self.fetch_avatar(ctx, format!("guild_icon_{}_{}", guild_id, icon_hash), url)
    }
    /// Decode a static image with size limits checked before pixel allocation.
    fn decode_static(bytes: &[u8]) -> Option<image::DynamicImage> {
        use image::ImageReader;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(MAX_SOURCE_PIXELS as u32);
        limits.max_image_height = Some(MAX_SOURCE_PIXELS as u32);
        limits.max_alloc = Some(MAX_SOURCE_PIXELS * 4);
        let mut reader = ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
        reader.limits(limits);
        reader.decode().ok()
    }

    pub(crate) fn decode_image_payload(bytes: &[u8]) -> Option<ImagePayload> {
    if bytes.len() < 6 {
        return None;
    }
    let is_gif = &bytes[..6] == b"GIF89a" || &bytes[..6] == b"GIF87a";
    if is_gif {
        if let Ok(decoder) = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)) {
            use image::{AnimationDecoder, ImageDecoder};
            // Check the canvas before decoding so frame pixels aren't allocated.
            let (cw, ch) = decoder.dimensions();
            if !source_size_allowed(cw, ch) {
                return None;
            }
            let mut frames = decoder.into_frames();
            // Pick frame size by frame count so long GIFs shrink but play fully.
            let frame_dim = gif_frame_count(bytes)
                .map(gif_frame_dim)
                .unwrap_or(MAX_GIF_DIM);
            let mut out = Vec::new();
            let mut bytes_used = 0usize;
            while out.len() < MAX_GIF_FRAMES {
                let Some(Ok(fr)) = frames.next() else { break };
                let (num, den) = fr.delay().numer_denom_ms();
                let secs = if den == 0 {
                    0.1
                } else {
                    (num as f64 / den as f64) / 1000.0
                };
                let buf = fr.into_buffer();
                let (w, h) = buf.dimensions();
                if w == 0 || h == 0 {
                    break;
                }
                let frame = image::DynamicImage::ImageRgba8(buf);
                let rgba = shrink(frame, frame_dim).into_rgba8();
                let (fw, fh) = rgba.dimensions();
                if fw == 0 || fh == 0 {
                    break;
                }
                // Safety net if the frame count was wrong: stop at the budget.
                let frame_bytes = fw as usize * fh as usize * 4;
                if out.len() > 1 && bytes_used + frame_bytes > MAX_GIF_BYTES {
                    break;
                }
                bytes_used += frame_bytes;
                let pixels = rgba.into_raw();
                let ci = egui::ColorImage::from_rgba_unmultiplied(
                    [fw as usize, fh as usize],
                    &pixels,
                );
                out.push((ci, secs.max(0.02) as f32));
            }
            // A single frame is a static image; the common path below decodes it.
            if out.len() > 1 {
                return Some(ImagePayload::Animated { frames: out });
            }
        }
    }
    if let Some(img) = Self::decode_static(bytes) {
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

    if contains_ignore_case(url, "/avatars/") || contains_ignore_case(url, "/users/") {
        remember_failed(&mut self.failed_images, key);
        return None;
    }

    // Skip new downloads when the limit is full; retry on a later frame.
    if !self.pending_images.contains_key(&cache_key) && !take_image_slot() {
        return None;
    }

    let pending = self.pending_images.entry(cache_key.clone()).or_insert_with(|| {
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let url_moved = url.to_string();
        std::thread::spawn(move || {
            // The slot is released on any exit, including a panic.
            let _slot = ImageSlot;
            if let Ok(resp) = http().get(&url_moved).send() {
                if let Some(bytes) = read_limited(resp, MAX_DOWNLOAD_BYTES) {
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

    match pending.try_recv() {
        Ok(Some(payload)) => {
            self.pending_images.remove(&key);
            return self.store_image_payload(&ctx2, key, payload);
        }
        Ok(None) => {
            self.pending_images.remove(&key);
            remember_failed(&mut self.failed_images, key);
        }
        // Thread still running; wait for the next frame.
        Err(std::sync::mpsc::TryRecvError::Empty) => {}
        // Thread died without replying; drop the entry to avoid a permanent leak.
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            self.pending_images.remove(&key);
        }
    }

    None
}

/// Turn decoded pixels into textures and store them in the cache.
/// Separate from `download_image` so results can be reaped without rendering.
fn store_image_payload(
    &mut self,
    ctx: &egui::Context,
    key: String,
    payload: ImagePayload,
) -> Option<LoadedImage> {
    match payload {
        ImagePayload::Static(color_image) => {
            let handle = ctx.load_texture(&key, color_image, egui::TextureOptions::LINEAR);
            let loaded = LoadedImage::Static(handle);
            self.image_cache.insert(key.clone(), loaded.clone());
            self.push_debug(format!(
                "IMG: {} в кэше — {} шт, {:.1} МБ текстур",
                key,
                self.image_cache.len(),
                self.image_cache.bytes() as f64 / (1024.0 * 1024.0)
            ));
            ctx.request_repaint();
            Some(loaded)
        }
        ImagePayload::Animated { frames } => {
            let mut handles = Vec::with_capacity(frames.len());
            let mut delays = Vec::with_capacity(frames.len());
            for (i, (ci, delay)) in frames.into_iter().enumerate() {
                let tkey = format!("{}#f{}", key, i);
                handles.push(ctx.load_texture(&tkey, ci, egui::TextureOptions::LINEAR));
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
                ctx.request_repaint();
                Some(loaded)
            } else {
                None
            }
        }
    }
}

/// Reap finished downloads even for images no longer on screen.
/// Otherwise decoded pixels linger in `pending_images`; the cache bounds memory.
pub(crate) fn reap_pending_images(&mut self, ctx: &egui::Context) {
    if self.pending_images.is_empty() {
        return;
    }
    // Take the whole map to avoid copying keys each frame; put unfinished back.
    for (key, rx) in std::mem::take(&mut self.pending_images) {
        match rx.try_recv() {
            Ok(Some(payload)) => {
                self.store_image_payload(ctx, key, payload);
            }
            Ok(None) => {
                remember_failed(&mut self.failed_images, key);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.pending_images.insert(key, rx);
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
        }
    }
}
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageEncoder, Rgb, RgbImage, Rgba, RgbaImage};

    /// Serializes tests that touch the process-wide download counter.
    static SLOTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn plain_app() -> App {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(rx)
    }

    /// Build a real GIF of the given size and frame count for decoding tests.
    fn encode_test_gif(w: u32, h: u32, n: usize) -> Vec<u8> {
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
                ))
                .unwrap();
            }
        }
        out
    }

    /// A failed avatar must not spawn a new request on every frame.
    #[test]
    fn failed_avatar_is_not_requested_again_on_every_frame() {
        let ctx = egui::Context::default();
        let mut app = plain_app();
        let (tx, rx) = std::sync::mpsc::channel::<AvatarFetch>();
        tx.send(AvatarFetch::Failed).unwrap();
        app.pending_avatars.insert("u1_deadbeef".into(), rx);

        // Frame where the thread failed: the pending entry is gone...
        let answer = app.pending_avatars["u1_deadbeef"].try_recv();
        assert_eq!(answer, Ok(AvatarFetch::Failed));
        app.pending_avatars.remove("u1_deadbeef");
        assert!(app.store_avatar(ctx.clone(), "u1_deadbeef".into(), AvatarFetch::Failed).is_none());

        // ...and the key is nowhere; only the failure memory stops a new thread.
        assert!(app.avatar_cache.get("u1_deadbeef").is_none());
        assert!(!app.pending_avatars.contains_key("u1_deadbeef"));
        assert!(
            app.failed_avatars.contains("u1_deadbeef"),
            "отказ не запомнен — следующий кадр снова спросит аватар: {:?}",
            app.failed_avatars
        );
    }

    /// A busy download slot must not be remembered as a failure.
    #[test]
    fn busy_avatar_slot_is_not_remembered_as_failure() {
        let ctx = egui::Context::default();
        let mut app = plain_app();
        assert!(app.store_avatar(ctx, "u1_cafe".into(), AvatarFetch::Busy).is_none());
        assert!(
            !app.failed_avatars.contains("u1_cafe"),
            "занятость слота — не отказ, ключ нельзя запоминать: {:?}",
            app.failed_avatars
        );
        assert!(!app.pending_avatars.contains_key("u1_cafe"));
    }

    /// Success must cache the texture so the next frame doesn't refetch it.
    #[test]
    fn ready_avatar_goes_to_cache_and_is_forgotten_as_pending() {
        let ctx = egui::Context::default();
        let mut app = plain_app();
        let img = egui::ColorImage::new([8, 8], egui::Color32::RED);
        assert!(app.store_avatar(ctx.clone(), "u1_ok".into(), AvatarFetch::Ready(img)).is_some());
        assert!(app.avatar_cache.get("u1_ok").is_some());
        assert!(!app.failed_avatars.contains("u1_ok"), "удавшийся аватар не в списке отказов");
    }

    /// Guild icons use the same path, so their failures must be remembered too.
    #[test]
    fn guild_icon_failure_is_remembered_too() {
        let ctx = egui::Context::default();
        let mut app = plain_app();
        assert!(app
            .store_avatar(ctx, "guild_icon_g1_abc".into(), AvatarFetch::Failed)
            .is_none());
        assert!(
            app.failed_avatars.contains("guild_icon_g1_abc"),
            "иконка сервера должна запоминаться так же, как аватар"
        );
    }

    /// Failure memory must stay bounded; avatar hashes never repeat.
    #[test]
    fn failed_avatars_are_bounded() {
        let ctx = egui::Context::default();
        let mut app = plain_app();
        for i in 0..(crate::app::MAX_FAILED_IMAGES + 10) {
            assert!(app
                .store_avatar(ctx.clone(), format!("u{i}_hash"), AvatarFetch::Failed)
                .is_none());
        }
        assert!(
            app.failed_avatars.len() <= crate::app::MAX_FAILED_IMAGES,
            "память об отказах выросла без предела: {}",
            app.failed_avatars.len()
        );
        // Old keys are evicted gradually, but recent ones are remembered.
        assert!(app.failed_avatars.len() > 0, "список отказов не должен опустеть");
    }

    /// Overflow must evict one entry, not clear everything during a failure storm.
    #[test]
    fn failed_avatars_are_evicted_one_by_one() {
        let ctx = egui::Context::default();
        let mut app = plain_app();
        let max = crate::app::MAX_FAILED_IMAGES;
        for i in 0..max {
            app.store_avatar(ctx.clone(), format!("u{i}_h"), AvatarFetch::Failed);
        }
        // Overflow by exactly one entry.
        app.store_avatar(ctx.clone(), "overflow_hash".into(), AvatarFetch::Failed);

        let survivors = (0..max)
            .filter(|i| app.failed_avatars.contains(&format!("u{i}_h")))
            .count();
        assert!(
            survivors >= max - 1,
            "переполнение стёрло больше одной записи: выжило {survivors} из {max}"
        );
    }

    /// Avatar URL detection must be case-insensitive without allocating per frame.
    #[test]
    fn avatar_url_detection_is_case_insensitive() {
        let is_avatar =
            |u: &str| contains_ignore_case(u, "/avatars/") || contains_ignore_case(u, "/users/");
        for url in [
            "https://cdn.discordapp.com/avatars/1/hash.png",
            "https://cdn.discordapp.com/AVATARS/1/hash.png",
            "https://cdn.discordapp.com/Users/1/hash.png",
            "https://example.com/USERS/1/hash.png",
        ] {
            assert!(is_avatar(url), "ссылка должна опознаваться как аватарка: {url}");
            assert_eq!(
                is_avatar(url),
                url.to_lowercase().contains("/avatars/") || url.to_lowercase().contains("/users/"),
                "регистронезависимый поиск разошёлся со старым: {url}"
            );
        }
        assert!(!is_avatar("https://example.com/pic.png"), "обычная картинка — не аватарка");
    }

    /// A refused slot must not increment the counter, or it drifts up forever.
    #[test]
    fn refused_download_does_not_eat_slot() {
        let _guard = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        while take_image_slot() {}
        let busy = IMAGE_DOWNLOADS_IN_FLIGHT.load(Ordering::SeqCst);
        for _ in 0..100 {
            assert!(!take_image_slot(), "слоты выдали сверх лимита");
        }
        assert_eq!(
            IMAGE_DOWNLOADS_IN_FLIGHT.load(Ordering::SeqCst),
            busy,
            "отказ занял слот — счётчик уехал вверх"
        );
        release_image_slot();
        assert!(take_image_slot(), "освобождённый слот не вернулся");
        // Don't affect other tests: reset the counter to zero.
        IMAGE_DOWNLOADS_IN_FLIGHT.store(0, Ordering::SeqCst);
    }

    /// Local server serving an image with a delay to test slot limiting.
    fn slow_png_server(bytes: Vec<u8>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("не занять порт");
        let addr = listener.local_addr().unwrap().to_string();
        let bytes = std::sync::Arc::new(bytes);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let bytes = bytes.clone();
                std::thread::spawn(move || {
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf);
                    std::thread::sleep(Duration::from_millis(40));
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&bytes);
                    let _ = stream.flush();
                });
            }
        });
        addr
    }

    /// Full download path HTTP -> decode -> cache with more images than slots.
    #[test]
    fn more_images_than_slots_all_load() {
        let _guard = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        let img = RgbImage::from_pixel(8, 8, Rgb([200, 30, 30]));
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(img.as_raw(), 8, 8, image::ExtendedColorType::Rgb8)
            .unwrap();
        let addr = slow_png_server(png);
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(rx);
        let ctx = egui::Context::default();
        let urls: Vec<String> = (0..MAX_CONCURRENT_IMAGE_DOWNLOADS + 3)
            .map(|i| format!("http://{}/{}.png", addr, i))
            .collect();

        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut loaded = 0;
        while std::time::Instant::now() < deadline {
            for u in &urls {
                let _ = app.download_image(&ctx, u);
            }
            loaded = urls
                .iter()
                .filter(|u| app.image_cache.get(u).is_some())
                .count();
            if loaded == urls.len() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(loaded, urls.len(), "докачались не все картинки");
        assert!(
            app.pending_images.is_empty(),
            "в очереди остались висящие загрузки: {:?}",
            app.pending_images.len()
        );
        assert_eq!(
            IMAGE_DOWNLOADS_IN_FLIGHT.load(Ordering::SeqCst),
            0,
            "слоты загрузки не вернулись"
        );
    }

    /// A finished download must be reaped even if its image was scrolled away.
    #[test]
    fn completed_download_for_scrolled_away_image_is_reaped() {
        let ctx = egui::Context::default();
        let mut app = plain_app();
        let key = "https://cdn.discordapp.com/attachments/1/off.png".to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Some(ImagePayload::Static(egui::ColorImage::new(
            [8, 8],
            egui::Color32::RED,
        ))))
        .unwrap();
        app.pending_images.insert(key.clone(), rx);

        // Frame where nothing draws this image (channel not open).
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run(raw, |ctx| app.run_frame(ctx));

        assert!(
            !app.pending_images.contains_key(&key),
            "готовый результат остался висеть в очереди"
        );
        assert!(
            app.image_cache.contains_key(&key),
            "готовую картинку нужно забрать в кэш, где память ограничена"
        );
    }

    /// Large images must be shrunk to the dimension limit.
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
                // Aspect ratio preserved.
                let ratio = ci.size[0] as f32 / ci.size[1] as f32;
                assert!((ratio - 1.5).abs() < 0.05, "пропорции сломаны: {}", ratio);
                eprintln!("[TEST] после ужатия: {:?}", ci.size);
            }
            other => panic!("ожидалась статичная картинка, получено {:?}", other.is_some()),
        }
    }

    /// Small images keep their size.
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

    /// A GIF plays all its frames instead of truncating midway.
    #[test]
    fn gif_plays_all_its_frames() {
        let (w, h, n) = (240u32, 160u32, 30usize);
        let out = encode_test_gif(w, h, n);
        eprintln!("[TEST] GIF {}x{} {} кадров = {} КБ", w, h, n, out.len() / 1024);

        assert_eq!(gif_frame_count(&out), Some(n), "разбор структуры GIF врёт");

        match App::decode_image_payload(&out) {
            Some(ImagePayload::Animated { frames }) => {
                assert_eq!(frames.len(), n, "гифка обрезана: {} из {}", frames.len(), n);
                for (ci, _) in &frames {
                    assert!(
                        ci.size[0] <= MAX_GIF_DIM as usize && ci.size[1] <= MAX_GIF_DIM as usize,
                        "кадр не ужат: {:?}",
                        ci.size
                    );
                }
                eprintln!("[TEST] кадров: {}, размер {:?}", frames.len(), frames[0].0.size);
            }
            other => panic!("ожидалась анимация, получено {:?}", other.is_some()),
        }
    }

    /// A long GIF is downscaled but still plays fully within the memory budget.
    #[test]
    fn long_gif_is_downscaled_but_complete() {
        let (w, h, n) = (400u32, 400u32, 120usize);
        let out = encode_test_gif(w, h, n);
        let total: usize = match App::decode_image_payload(&out) {
            Some(ImagePayload::Animated { frames }) => {
                assert_eq!(frames.len(), n, "гифка обрезана: {} из {}", frames.len(), n);
                frames.iter().map(|(ci, _)| ci.size[0] * ci.size[1] * 4).sum()
            }
            other => panic!("ожидалась анимация, получено {:?}", other.is_some()),
        };
        assert!(
            total <= MAX_GIF_BYTES,
            "кадры гифки не влезли в бюджет: {} байт",
            total
        );
        eprintln!(
            "[TEST] 120 кадров 400x400: {} МБ, размер кадра {:?}",
            total as f64 / (1024.0 * 1024.0),
            gif_frame_dim(n)
        );
    }

    /// The frame-size formula keeps all frames in budget and within bounds.
    #[test]
    fn gif_frame_dim_stays_within_budget() {
        for n in [1usize, 10, 30, 60, 120, 300, 512, 1000, 2000, 100_000] {
            let dim = gif_frame_dim(n);
            assert!(
                (MIN_GIF_DIM..=MAX_GIF_DIM).contains(&dim),
                "размер {} вне границ при {} кадрах",
                dim,
                n
            );
            // Until clamped to the minimum, frames must fit the budget.
            if dim > MIN_GIF_DIM {
                let effective = n.clamp(1, MAX_GIF_FRAMES);
                assert!(
                    effective * (dim as usize) * (dim as usize) * 4 <= MAX_GIF_BYTES,
                    "{} кадров по {} пикселей не влезают",
                    effective,
                    dim
                );
            }
        }
        // Ordinary GIFs keep full resolution.
        assert_eq!(gif_frame_dim(30), MAX_GIF_DIM);
    }

    /// GIF structure parsing must not break on non-GIF data.
    #[test]
    fn gif_frame_count_rejects_garbage() {
        assert_eq!(gif_frame_count(b""), None);
        assert_eq!(gif_frame_count(b"not a gif at all"), None);
    }

    /// A single-frame GIF is static, not an animation.
    #[test]
    fn single_frame_gif_is_static() {
        let mut out = Vec::new();
        {
            let mut enc = image::codecs::gif::GifEncoder::new(&mut out);
            enc.encode_frame(image::Frame::from_parts(
                RgbaImage::from_pixel(64, 64, Rgba([1, 2, 3, 255])),
                0,
                0,
                image::Delay::from_numer_denom_ms(50, 1),
            )).unwrap();
        }
        match App::decode_image_payload(&out) {
            Some(ImagePayload::Static(ci)) => assert_eq!(ci.size, [64, 64]),
            other => panic!("ожидалась статичная картинка, получено {:?}", other.is_some()),
        }
    }

    /// A GIF whose canvas exceeds the limit is refused before decoding.
    #[test]
    fn oversized_gif_is_refused() {
        // Header claims 10000x10000, above MAX_SOURCE_PIXELS.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"GIF89a");
        bytes.extend_from_slice(&10000u16.to_le_bytes());
        bytes.extend_from_slice(&10000u16.to_le_bytes());
        bytes.push(0); // no global color table
        bytes.push(0x21); // extension
        bytes.push(0xF9); // Graphic Control
        bytes.extend_from_slice(&[4, 0, 0, 0, 0, 0, 0, 0]);
        bytes.push(0x3B); // end
        assert!(
            App::decode_image_payload(&bytes).is_none(),
            "гифка неподходящего размера не должна распаковываться"
        );
    }

    /// An oversized static image is rejected before pixel allocation.
    #[test]
    fn static_size_limit_is_respected() {
        // Limit boundary: 39.9 Mpx allowed, 42 Mpx not.
        assert!(source_size_allowed(7000, 5700), "39.9 Мпикс должны помещаться");
        assert!(!source_size_allowed(7000, 6000), "42 Мпикс уже не помещаются");
        assert!(source_size_allowed(64, 64));

        // PNG claiming 10000x10000 in its header; rejected before allocation.
        let mut png = Vec::new();
        png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&10000u32.to_be_bytes());
        ihdr.extend_from_slice(&10000u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // RGBA8
        push_chunk(&mut png, b"IHDR", &ihdr);
        push_chunk(&mut png, b"IDAT", &[0x78, 0x01, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01]);
        push_chunk(&mut png, b"IEND", &[]);
        assert!(
            App::decode_image_payload(&png).is_none(),
            "картинка недопустимого размера не должна распаковываться"
        );
    }

    fn push_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        // No CRC needed: the limit rejects the image from its header.
        out.extend_from_slice(&[0, 0, 0, 0]);
    }

    /// Server returning a body of a given size to test the read cap cheaply.
    fn body_server(total: usize) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                total
            );
            if stream.write_all(head.as_bytes()).is_err() {
                return;
            }
            let chunk = vec![0u8; 64 * 1024];
            let mut sent = 0;
            while sent < total {
                let n = (total - sent).min(chunk.len());
                if stream.write_all(&chunk[..n]).is_err() {
                    return;
                }
                sent += n;
            }
            let _ = stream.flush();
        });
        addr
    }

    /// The response body is read with a cap; embed URLs come from foreign sites.
    #[test]
    fn oversized_response_body_is_refused() {
        let addr = body_server(200);
        let resp = http().get(format!("http://{}/small.png", addr)).send().unwrap();
        let small = read_limited(resp, 1024).expect("маленькое тело должно прочитаться");
        assert_eq!(small.len(), 200);

        let addr = body_server(8 * 1024 * 1024);
        let resp = http().get(format!("http://{}/huge.png", addr)).send().unwrap();
        assert!(
            read_limited(resp, 4096).is_none(),
            "тело сверх потолка должно отсекаться, а не читаться целиком"
        );
    }
}

#[cfg(test)]
impl App {
    /// Same path as `download_image` after the response, but without the network.
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
