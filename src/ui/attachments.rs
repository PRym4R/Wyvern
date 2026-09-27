use eframe::egui;

use crate::app::App;
use crate::models::ChatMessage;

impl App {
    pub(crate) fn extract_embed_image_url(&self, url: &str) -> Option<String> {
        let lower = url.to_lowercase();
        if lower.is_empty() {
            return None;
        }
        if lower.contains("/avatars/") || lower.contains("/users/") {
            return None;
        }
        if lower.ends_with(".mp4") || lower.ends_with(".webm") || lower.ends_with(".ogg") || lower.ends_with(".m4v") || lower.ends_with(".mov") {
            return None;
        }
        Some(url.to_string())
    }
    pub(crate) fn draw_attachments(&mut self, ui: &mut egui::Ui, msg: &ChatMessage) {
        let mut urls: Vec<String> = Vec::new();
        for att in &msg.attachments {
            if att.content_type.as_deref().map(|ct| ct.starts_with("image/")).unwrap_or(false) {
                urls.push(att.url.clone());
            }
        }
        for e in &msg.embeds {
            if let Some(u) = &e.image_url {
                if let Some(u2) = self.extract_embed_image_url(u) {
                    urls.push(u2);
                }
            }
        }
        for url in urls {
            if let Some(tex) = self.download_image(ui.ctx(), &url) {
                let size = tex.size_vec2();
                if size.x <= 0.0 || size.y <= 0.0 {
                    continue;
                }
                let max_w = 360.0_f32;
                let max_h = 360.0_f32;
                let scale = (max_w / size.x).min(max_h / size.y).min(1.0);
                let disp = egui::vec2(size.x * scale, size.y * scale);
                ui.add(egui::Image::new(egui::load::SizedTexture::new(tex.id(), disp)))
                    .on_hover_text(url.clone());
            } else {
                ui.spinner();
            }
        }
    }
}
