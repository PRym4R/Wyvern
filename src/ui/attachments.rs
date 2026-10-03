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

/// Подсказка с URL строится только при наведении. `on_hover_text` принимает
/// уже готовый текст, поэтому `url.to_string()` вызывался каждый кадр на
/// каждую картинку; здесь строка создаётся внутри замыкания, а egui дёргает
/// его только для той картинки, над которой курсор.
fn hover_text_lazy(response: egui::Response, make: impl FnOnce() -> String) -> egui::Response {
    response.on_hover_ui(|ui| {
        ui.label(make());
    })
}

const VIDEO_EXTS: [&str; 5] = [".mp4", ".webm", ".ogg", ".m4v", ".mov"];

/// Во сколько картинку в чате можно показывать. От этого зависит и размер
/// текстуры, и высота сообщения в списке.
pub(crate) const MAX_IMAGE_DISPLAY: f32 = 360.0;

/// Место под картинку, для которой Discord не прислал размер. Берётся
/// только как последний обходной путь: обычно размер известен заранее, и
/// резервируется ровно столько места, сколько картинка займёт. Число одно
/// и то же для оценки высоты сообщения и для отрисовки, иначе список дёргается.
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

/// Перебрать картинки сообщения — вложения и эмбеды — и вызвать `f` на
/// каждой. Вместе со ссылкой отдаём размер, который Discord прислал рядом с
/// ней: по нему место под картинку резервируется точно, не дожидаясь
/// загрузки, и сообщение не меняет высоту, когда картинка наконец приходит.
///
/// Список не собирается: он нужен на каждом кадре и для каждого сообщения
/// (в том числе для оценки высоты ещё не нарисованных), а копия URL'ов в куче
/// — ровно та аллокация, ради которой эту строчку когда-то и переписывали.
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

/// Размер из пары `width`/`height` в виде, который ждёт `display_size`.
fn known_size(size: Option<[u32; 2]>) -> Option<egui::Vec2> {
    size.map(|[w, h]| egui::vec2(w as f32, h as f32))
}

/// Высота картинки в чате: настоящая, если она уже в кэше.
pub(crate) fn display_size(size: egui::Vec2) -> egui::Vec2 {
    if size.x <= 0.0 || size.y <= 0.0 {
        return egui::Vec2::ZERO;
    }
    let scale = (MAX_IMAGE_DISPLAY / size.x).min(MAX_IMAGE_DISPLAY / size.y).min(1.0);
    egui::vec2(size.x * scale, size.y * scale)
}

/// Сколько места займёт картинка в сообщении, когда её ещё нет в кэше.
/// Размер Discord присылает вместе со ссылкой, поэтому место резервируется
/// точно и сообщение не скачет на сотни пикселей в момент загрузки. Если
/// размера нет (старые сообщения, битая ссылка) — берём заглушку.
pub(crate) fn reserved_size(known: Option<egui::Vec2>) -> egui::Vec2 {
    match known {
        Some(s) => display_size(s),
        None => egui::vec2(MAX_IMAGE_DISPLAY, IMAGE_PLACEHOLDER),
    }
}

impl App {
    pub(crate) fn draw_attachments(&mut self, ui: &mut egui::Ui, msg: &ChatMessage) {
        // Ссылка на картинку в уже скачанном виде или место под неё: URL'ы
        // берём из сообщения, а не копируем — список показывается на каждом
        // кадре, и копии URL'ов в куче не нужны.
        for_each_image(msg, |url, known| {
            if let Some(tex) = self.download_image(ui.ctx(), url) {
                // Картинка (или гифка) на экране: не дадим вытеснить её из
                // кэша, пока её видно. Иначе в следующем кадре её снова
                // качают — она мигает, и высота сообщения скачет.
                self.image_cache.mark_visible(url);
                // Анимированную нужно перерисовывать непрерывно, пока она на
                // экране: в покое кадров больше нет (Т-7).
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
                // Картинка не загрузится уже никогда (битая или слишком
                // большая) — не крутим вечный спиннер, а говорим об этом.
                ui.label(
                    RichText::new("не удалось загрузить")
                        .size(11.0)
                        .color(self.theme.text_secondary),
                );
            } else {
                // Ждём: место под картинку резервируем сразу и ровно столько,
                // сколько она потом займёт, иначе в момент её появления
                // высота сообщения скачет и всё, что ниже, уезжает вниз.
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

    /// Текст подсказки строится только при наведении: иначе `url.to_string()`
    /// аллоцировался на каждую картинку каждый кадр.
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

