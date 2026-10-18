use eframe::egui::{self, RichText};

use crate::app::{ConfirmAction, ConfirmDialog};
use crate::settings::Settings;

impl crate::app::App {
    /// Settings window: theme, UI scale, media/notifications, history.
    pub(crate) fn draw_settings(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(380.0);

                ui.heading("Theme");
                ui.horizontal(|ui| {
                    let mut changed = false;
                    changed |= ui.radio_value(&mut self.theme_index, 0u8, "Dark").changed();
                    changed |= ui.radio_value(&mut self.theme_index, 1u8, "Cyberpunk").changed();
                    changed |= ui.radio_value(&mut self.theme_index, 2u8, "Light").changed();
                    if changed {
                        self.write_settings();
                    }
                });

                ui.separator();
                ui.heading("Interface");
                let mut scale = self.settings.ui_scale.unwrap_or(1.0);
                let resp = ui.add(
                    egui::Slider::new(&mut scale, 0.8..=1.6)
                        .text("UI and font scale")
                        .fixed_decimals(2),
                );
                if resp.changed() {
                    self.settings.ui_scale = Some(scale);
                }
                // Persist once the drag ends, not on every pixel moved.
                if resp.drag_stopped() {
                    self.write_settings();
                }
                if ui.button("Reset scale").clicked() {
                    self.settings.ui_scale = Some(1.0);
                    self.write_settings();
                }

                ui.separator();
                ui.heading("Media and notifications");
                let mut blur = self.settings.blur_media;
                if ui
                    .checkbox(&mut blur, "Hide images and GIFs until clicked")
                    .changed()
                {
                    self.settings.blur_media = blur;
                    // Revealing state belongs to the old mode; start fresh.
                    self.revealed_images.clear();
                    self.write_settings();
                }
                let mut notes = self.settings.notifications;
                if ui
                    .checkbox(&mut notes, "Notify about new messages")
                    .changed()
                {
                    self.settings.notifications = notes;
                    self.write_settings();
                }

                ui.separator();
                ui.heading("History");
                let mut keep = self.settings.keep_deleted;
                if ui
                    .checkbox(&mut keep, "Keep deleted messages (shown red)")
                    .changed()
                {
                    self.settings.keep_deleted = keep;
                    if !keep {
                        // Turning the view off also discards the stored archive.
                        self.deleted_ids.clear();
                        self.deleted_store.clear();
                        self.write_deleted();
                    }
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
                    if ui.button("Clear drafts").clicked() {
                        self.confirm = Some(ConfirmDialog {
                            title: "Clear drafts".into(),
                            body: "Discard every unsent draft in every channel?".into(),
                            confirm_label: "Clear".into(),
                            action: ConfirmAction::ClearDrafts,
                        });
                    }
                    if ui.button("Reset window size").clicked() {
                        self.settings.window = None;
                        self.write_settings();
                    }
                });
                ui.label(
                    RichText::new(format!("Settings file: {}", Settings::path().display()))
                        .size(11.0)
                        .color(self.theme.text_secondary),
                );
            });
        // A confirmation on top of the settings owns Esc while it is open.
        if !open || (self.confirm.is_none() && ctx.input(|i| i.key_pressed(egui::Key::Escape))) {
            self.settings_open = false;
        }
    }
}
