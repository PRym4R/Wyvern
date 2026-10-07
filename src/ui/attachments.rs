use eframe::egui;
use eframe::egui::RichText;

use crate::app::App;
use crate::models::ChatMessage;

/// Case-insensitive substring check; avoids a `to_lowercase()` allocation per frame.
fn contains_ci(hay: &str, needle: &str) -> bool {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    !n.is_empty() && h.len() >= n.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// Case-insensitive suffix check.
fn ends_with_ci(hay: &str, needle: &str) -> bool {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    h.len() >= n.len() && h[h.len() - n.len()..].eq_ignore_ascii_case(n)
}

/// Builds the URL tooltip only on hover, so `to_string` isn't run every frame.
fn hover_text_lazy(response: egui::Response, make: impl FnOnce() -> String) -> egui::Response {
    response.on_hover_ui(|ui| {
        ui.label(make());
    })
}

const VIDEO_EXTS: [&str; 5] = [".mp4", ".webm", ".ogg", ".m4v", ".mov"];

/// Max size an image may be displayed at in chat.
pub(crate) const MAX_IMAGE_DISPLAY: f32 = 360.0;

/// Fallback placeholder height for images with no known size. Shared by
/// height estimation and drawing so the list doesn't jump.
pub(crate) const IMAGE_PLACEHOLDER: f32 = 180.0;

/// Returns an embed image URL worth showing: skips avatars and videos.
/// Borrows from the input rather than copying.
pub(crate) fn embed_image_url(url: &str) -> Option<&str> {
    if url.is_empty() {
        return None;
    }
    if contains_ci(url, "/avatars/") || contains_ci(url, "/users/") {
        return None;
    }
    if VIDEO_EXTS.iter().any(|ext| ends_with_ci(url, ext)) {
        return None;
    }
    Some(url)
}

/// Calls `f` for each image (attachment or embed) with its known size, so
/// space is reserved without waiting for the download.
///
/// Iterates instead of collecting: this runs every frame for every message.
pub(crate) fn for_each_image(msg: &ChatMessage, mut f: impl FnMut(&str, Option<egui::Vec2>)) {
    for att in &msg.attachments {
        if att.content_type.as_deref().map(|ct| ct.starts_with("image/")).unwrap_or(false) {
            f(att.url.as_str(), known_size(att.size));
        }
    }
    for e in &msg.embeds {
        if let Some(u) = e.image_url.as_deref().and_then(embed_image_url) {
            f(u, known_size(e.image_size));
        }
    }
}

/// Converts a `width`/`height` pair to the `Vec2` `display_size` expects.
fn known_size(size: Option<[u32; 2]>) -> Option<egui::Vec2> {
    size.map(|[w, h]| egui::vec2(w as f32, h as f32))
}

/// Image size as displayed in chat, scaled down to fit `MAX_IMAGE_DISPLAY`.
pub(crate) fn display_size(size: egui::Vec2) -> egui::Vec2 {
    if size.x <= 0.0 || size.y <= 0.0 {
        return egui::Vec2::ZERO;
    }
    let scale = (MAX_IMAGE_DISPLAY / size.x).min(MAX_IMAGE_DISPLAY / size.y).min(1.0);
    egui::vec2(size.x * scale, size.y * scale)
}

/// Space an image will occupy before it's cached, so the message doesn't jump.
/// Falls back to `IMAGE_PLACEHOLDER` when no size is known.
pub(crate) fn reserved_size(known: Option<egui::Vec2>) -> egui::Vec2 {
    match known {
        Some(s) => display_size(s),
        None => egui::vec2(MAX_IMAGE_DISPLAY, IMAGE_PLACEHOLDER),
    }
}

impl App {
    pub(crate) fn draw_attachments(&mut self, ui: &mut egui::Ui, msg: &ChatMessage) {
        // Draw the downloaded image or reserve its space; URLs are borrowed,
        // not copied.
        for_each_image(msg, |url, known| {
            if let Some(tex) = self.download_image(ui.ctx(), url) {
                // Keep a visible image pinned in the cache, or it gets
                // re-downloaded next frame and flickers.
                self.image_cache.mark_visible(url);
                // Animated images need continuous repaints while visible.
                if tex.is_animated() {
                    self.animating = true;
                }
                let disp = display_size(tex.size_vec2());
                if disp.x <= 0.0 || disp.y <= 0.0 {
                    return;
                }
                let resp = ui.add(egui::Image::new(egui::load::SizedTexture::new(tex.id(), disp)));
                hover_text_lazy(resp, || url.to_string());
            } else if self.failed_images.contains(url) {
                // Image will never load (broken or too large): show a note
                // instead of an endless spinner.
                ui.label(
                    RichText::new("не удалось загрузить")
                        .size(11.0)
                        .color(self.theme.text_secondary),
                );
            } else {
                // Reserve the image's space now so the message doesn't jump
                // when it appears.
                let disp = reserved_size(known);
                let color = self.theme.input_bg;
                let (rect, _) = ui.allocate_exact_size(disp, egui::Sense::hover());
                ui.painter().rect_filled(rect, 6.0, color);
                ui.allocate_new_ui(
                    egui::UiBuilder::new()
                        .max_rect(rect)
                        .layout(egui::Layout::top_down(egui::Align::Center)),
                    |ui| {
                        ui.spinner();
                    },
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{contains_ci, embed_image_url, ends_with_ci, hover_text_lazy};
    use eframe::egui;

    /// `embed_image_url` must be case-insensitive in URLs.
    #[test]
    fn embed_url_filters_match_old_rules() {
        assert_eq!(embed_image_url("https://cdn.discordapp.com/embeds/1/pic.png"), Some("https://cdn.discordapp.com/embeds/1/pic.png"));
        assert_eq!(embed_image_url("https://cdn.discordapp.com/EMBEDS/1/PIC.PNG"), Some("https://cdn.discordapp.com/EMBEDS/1/PIC.PNG"));
        assert_eq!(embed_image_url(""), None);
        assert_eq!(embed_image_url("https://cdn.discordapp.com/avatars/1/a.png"), None);
        assert_eq!(embed_image_url("https://cdn.discordapp.com/AVATARS/1/a.png"), None);
        assert_eq!(embed_image_url("https://cdn.discordapp.com/users/1/a.png"), None);
        assert_eq!(embed_image_url("https://cdn.discordapp.com/embeds/1/clip.MP4"), None);
        assert_eq!(embed_image_url("https://cdn.discordapp.com/embeds/1/clip.webm"), None);
        assert_eq!(embed_image_url("https://cdn.discordapp.com/embeds/1/clip.ogg"), None);
        assert_eq!(embed_image_url("https://cdn.discordapp.com/embeds/1/clip.M4V"), None);
        assert_eq!(embed_image_url("https://cdn.discordapp.com/embeds/1/clip.mov"), None);
        // An extension mid-URL doesn't make it a video.
        assert_eq!(
            embed_image_url("https://cdn.discordapp.com/embeds/1/mp4.png"),
            Some("https://cdn.discordapp.com/embeds/1/mp4.png")
        );
    }

    #[test]
    fn case_insensitive_helpers() {
        assert!(contains_ci("Hello/World", "/world"));
        assert!(!contains_ci("Hello", "/world"));
        assert!(!contains_ci("hi", ""));
        assert!(ends_with_ci("clip.MOV", ".mov"));
        assert!(!ends_with_ci("mov", ".mov"));
        assert!(ends_with_ci("anything", ""));
    }

    /// Tooltip text is built only on hover.
    #[test]
    fn tooltip_text_is_built_only_on_hover() {
        use std::cell::Cell;
        let ctx = egui::Context::default();
        let calls = Cell::new(0u32);
        let rect = egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(80.0, 20.0));

        let frame = |pointer: egui::Pos2| {
            let input = egui::RawInput {
                events: vec![egui::Event::PointerMoved(pointer)],
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let resp = ui.put(rect, egui::Label::new("картинка"));
                    hover_text_lazy(resp, || {
                        calls.set(calls.get() + 1);
                        "https://cdn.discordapp.com/x.png".to_string()
                    });
                });
            });
        };

        frame(egui::pos2(500.0, 500.0));
        assert_eq!(calls.get(), 0, "без наведения текст подсказки не строится");
        frame(rect.center());
        assert_eq!(calls.get(), 1, "под курсором подсказка строится");
    }
}

