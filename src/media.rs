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
include!("media_tests.rs");
