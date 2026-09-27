use eframe::egui;
use eframe::egui::RichText;

use crate::app::App;
use crate::models::ChatMessage;

/// Есть ли в строке подстрока без учёта регистра. Свой вариант вместо
/// `to_lowercase()`, потому что вызывается на каждом кадре для каждой
/// картинки, а копия строки URL — это лишняя аллокация мусора.
fn contains_ci(hay: &str, needle: &str) -> bool {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    !n.is_empty() && h.len() >= n.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// Заканчивается ли строка на подстроку без учёта регистра.
fn ends_with_ci(hay: &str, needle: &str) -> bool {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    h.len() >= n.len() && h[h.len() - n.len()..].eq_ignore_ascii_case(n)
}

const VIDEO_EXTS: [&str; 5] = [".mp4", ".webm", ".ogg", ".m4v", ".mov"];

/// Ссылка на картинку из эмбеда, если её вообще нужно показывать.
/// Аватары и видео пропускаем: аватары мы рисуем отдельно, видео всё равно
/// нечем показать. Возвращаем кусок исходной строки, а не её копию.
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

impl App {
    pub(crate) fn draw_attachments(&mut self, ui: &mut egui::Ui, msg: &ChatMessage) {
        // Ссылки берём из сообщения, а не копируем: список показывается на
        // каждом кадре, и копии URL'ов в куче не нужны.
        let mut urls: Vec<&str> = Vec::new();
        for att in &msg.attachments {
            if att.content_type.as_deref().map(|ct| ct.starts_with("image/")).unwrap_or(false) {
                urls.push(att.url.as_str());
            }
        }
        for e in &msg.embeds {
            if let Some(u) = e.image_url.as_deref() {
                if let Some(u) = embed_image_url(u) {
                    urls.push(u);
                }
            }
        }
        for url in urls {
            if let Some(tex) = self.download_image(ui.ctx(), url) {
                let size = tex.size_vec2();
                if size.x <= 0.0 || size.y <= 0.0 {
                    continue;
                }
                let max_w = 360.0_f32;
                let max_h = 360.0_f32;
                let scale = (max_w / size.x).min(max_h / size.y).min(1.0);
                let disp = egui::vec2(size.x * scale, size.y * scale);
                ui.add(egui::Image::new(egui::load::SizedTexture::new(tex.id(), disp)))
                    .on_hover_text(url.to_string());
            } else if self.failed_images.contains(url) {
                // Картинка не загрузится уже никогда (битая или слишком
                // большая) — не крутим вечный спиннер, а говорим об этом.
                ui.label(
                    RichText::new("не удалось загрузить")
                        .size(11.0)
                        .color(self.theme.text_secondary),
                );
            } else {
                ui.spinner();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{contains_ci, embed_image_url, ends_with_ci};

    /// Поведение `embed_image_url` должно совпадать с прежней версией на
    /// `to_lowercase()`: регистр в URL не должен ничего менять.
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
        // Расширение в середине URL видео не делает.
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
}

