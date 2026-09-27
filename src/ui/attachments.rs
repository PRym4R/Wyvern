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

/// Во сколько картинку в чате можно показывать. От этого зависит и размер
/// текстуры, и высота сообщения в списке.
pub(crate) const MAX_IMAGE_DISPLAY: f32 = 360.0;

/// Место под картинку, которая ещё скачивается. Средняя фотка в чате как раз
/// столько и занимает; важно, что это число одно и то же для оценки высоты
/// сообщения и для отрисовки, иначе список дёргается.
pub(crate) const IMAGE_PLACEHOLDER: f32 = 180.0;

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

/// Перебрать картинки сообщения — вложения и эмбеды — и вызвать `f` на каждой.
///
/// Список не собирается: он нужен на каждом кадре и для каждого сообщения
/// (в том числе для оценки высоты ещё не нарисованных), а копия URL'ов в куче
/// — ровно та аллокация, ради которой эту строчку когда-то и переписывали.
pub(crate) fn for_each_image(msg: &ChatMessage, mut f: impl FnMut(&str)) {
    for att in &msg.attachments {
        if att.content_type.as_deref().map(|ct| ct.starts_with("image/")).unwrap_or(false) {
            f(att.url.as_str());
        }
    }
    for e in &msg.embeds {
        if let Some(u) = e.image_url.as_deref().and_then(embed_image_url) {
            f(u);
        }
    }
}

/// Высота картинки в чате: настоящая, если она уже в кэше.
pub(crate) fn display_size(size: egui::Vec2) -> egui::Vec2 {
    if size.x <= 0.0 || size.y <= 0.0 {
        return egui::Vec2::ZERO;
    }
    let scale = (MAX_IMAGE_DISPLAY / size.x).min(MAX_IMAGE_DISPLAY / size.y).min(1.0);
    egui::vec2(size.x * scale, size.y * scale)
}

impl App {
    pub(crate) fn draw_attachments(&mut self, ui: &mut egui::Ui, msg: &ChatMessage) {
        // Ссылка на картинку в уже скачанном виде или место под неё: URL'ы
        // берём из сообщения, а не копируем — список показывается на каждом
        // кадре, и копии URL'ов в куче не нужны.
        let width = ui.available_width();
        for_each_image(msg, |url| {
            if let Some(tex) = self.download_image(ui.ctx(), url) {
                let disp = display_size(tex.size_vec2());
                if disp.x <= 0.0 || disp.y <= 0.0 {
                    return;
                }
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
                // Ждём: место под картинку резервируем сразу, иначе в момент
                // её появления высота сообщения скачет на сотни пикселей и всё,
                // что ниже, уезжает вниз.
                let color = self.theme.input_bg;
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(width, IMAGE_PLACEHOLDER), egui::Sense::hover());
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

