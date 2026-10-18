use eframe::egui::{self, RichText};

use crate::settings::Settings;

impl crate::app::App {
    /// Settings window: theme, UI scale, media/notifications, history.
    pub(crate) fn draw_settings(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        egui::Window::new("Настройки")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(380.0);

                ui.heading("Тема");
                ui.horizontal(|ui| {
                    let mut changed = false;
                    changed |= ui.radio_value(&mut self.theme_index, 0u8, "Тёмная").changed();
                    changed |= ui.radio_value(&mut self.theme_index, 1u8, "Киберпанк").changed();
                    changed |= ui.radio_value(&mut self.theme_index, 2u8, "Светлая").changed();
                    if changed {
                        self.write_settings();
                    }
                });

                ui.separator();
                ui.heading("Интерфейс");
                let mut scale = self.settings.ui_scale.unwrap_or(1.0);
                let resp = ui.add(
                    egui::Slider::new(&mut scale, 0.8..=1.6)
                        .text("Масштаб интерфейса и шрифта")
                        .fixed_decimals(2),
                );
                if resp.changed() {
                    self.settings.ui_scale = Some(scale);
                }
                // Persist once the drag ends, not on every pixel moved.
                if resp.drag_stopped() {
                    self.write_settings();
                }
                if ui.button("Сбросить масштаб").clicked() {
                    self.settings.ui_scale = Some(1.0);
                    self.write_settings();
                }

                ui.separator();
                ui.heading("Медиа и уведомления");
                let mut blur = self.settings.blur_media;
                if ui
                    .checkbox(&mut blur, "Прятать картинки и GIF до клика")
                    .changed()
                {
                    self.settings.blur_media = blur;
                    // Revealing state belongs to the old mode; start fresh.
                    self.revealed_images.clear();
                    self.write_settings();
                }
                let mut notes = self.settings.notifications;
                if ui
                    .checkbox(&mut notes, "Push-уведомления о новых сообщениях")
                    .changed()
                {
                    self.settings.notifications = notes;
                    self.write_settings();
                }

                ui.separator();
                ui.heading("История");
                let mut keep = self.settings.keep_deleted;
                if ui
                    .checkbox(&mut keep, "Хранить удалённые сообщения (красным)")
                    .changed()
                {
                    self.settings.keep_deleted = keep;
                    self.write_settings();
                }
                ui.label(
                    RichText::new(
                        "People, reading is fine — just don't reply to deleted messages, \
                         that goes against the sender's intent. ...kidding, I don't care.",
                    )
                    .size(11.0)
                    .color(self.theme.text_secondary),
                );

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Очистить черновики").clicked() {
                        self.settings.drafts.clear();
                        self.write_settings();
                    }
                    if ui.button("Сбросить размер окна").clicked() {
                        self.settings.window = None;
                        self.write_settings();
                    }
                });
                ui.label(
                    RichText::new(format!("Файл настроек: {}", Settings::path().display()))
                        .size(11.0)
                        .color(self.theme.text_secondary),
                );
            });
        if !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.settings_open = false;
        }
    }
}
