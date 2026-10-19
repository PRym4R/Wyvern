use eframe::egui::{self, RichText};

use crate::app::{ConfirmAction, ConfirmDialog};
use crate::settings::Settings;

/// Tabs shown down the left of the settings window.
const SECTIONS: [&str; 6] = ["Theme", "Interface", "Media", "History", "Network", "Advanced"];

impl crate::app::App {
    /// Settings window: a full-client overlay with tabs down the left.
    pub(crate) fn draw_settings(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let screen = ctx.screen_rect();
        let mut close = false;
        egui::Window::new("Settings")
            .title_bar(false)
            .resizable(false)
            .collapsible(false)
            .movable(false)
            .fixed_rect(screen)
            .frame(
                egui::Frame::default()
                    .fill(self.theme.panel_bg)
                    .inner_margin(egui::Margin::symmetric(14, 12)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(RichText::new("Settings").color(self.theme.text));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                    });
                });
                ui.separator();
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(170.0);
                        for (i, name) in SECTIONS.iter().enumerate() {
                            if ui.selectable_label(self.settings_section == i, *name).clicked() {
                                self.settings_section = i;
                            }
                        }
                    });
                    ui.separator();
                    ui.vertical(|ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("settings_content")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                self.draw_settings_section(ui);
                            });
                    });
                });
            });
        // A confirmation on top of the settings owns Esc while it is open.
        if close || (self.confirm.is_none() && ctx.input(|i| i.key_pressed(egui::Key::Escape))) {
            self.settings_open = false;
        }
    }

    /// Body of the currently selected settings tab.
    fn draw_settings_section(&mut self, ui: &mut egui::Ui) {
        match self.settings_section {
            0 => {
                ui.heading("Theme");
                let mut changed = false;
                changed |= ui.radio_value(&mut self.theme_index, 0u8, "Dark").changed();
                changed |= ui.radio_value(&mut self.theme_index, 1u8, "Cyberpunk").changed();
                changed |= ui.radio_value(&mut self.theme_index, 2u8, "Light").changed();
                if changed {
                    self.write_settings();
                }
            }
            1 => {
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
                ui.add_space(6.0);
                if ui.button("Reset scale").clicked() {
                    self.settings.ui_scale = Some(1.0);
                    self.write_settings();
                }
                if ui.button("Reset window size").clicked() {
                    self.settings.window = None;
                    self.write_settings();
                }
            }
            2 => {
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
            }
            3 => {
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
                ui.add_space(6.0);
                ui.label(
                    RichText::new(
                        "People, reading is fine — just don't reply to deleted messages, \
                         that goes against the sender's intent. ...kidding, I don't care.",
                    )
                    .size(11.0)
                    .color(self.theme.text_secondary),
                );
            }
            4 => {
                ui.heading("Network");
                ui.label("Outbound proxy URL (leave empty for a direct connection).");
                ui.add_space(4.0);
                let mut proxy = self.settings.proxy.clone().unwrap_or_default();
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut proxy)
                        .hint_text("http://127.0.0.1:8080 or socks5://127.0.0.1:1080")
                        .desired_width(420.0),
                );
                if resp.changed() {
                    let trimmed = proxy.trim().to_string();
                    self.settings.proxy = if trimmed.is_empty() { None } else { Some(trimmed.clone()) };
                    crate::net::set_proxy(&trimmed);
                }
                if resp.lost_focus() {
                    self.write_settings();
                }
                ui.add_space(6.0);
                ui.label(
                    RichText::new(
                        "Applies to API and media requests right away; the gateway uses it on \
                         its next connection. Styles: http, https, socks5 (no auth).",
                    )
                    .size(11.0)
                    .color(self.theme.text_secondary),
                );
            }
            _ => {
                ui.heading("Advanced");
                if ui.button("Clear drafts").clicked() {
                    self.confirm = Some(ConfirmDialog {
                        title: "Clear drafts".into(),
                        body: "Discard every unsent draft in every channel?".into(),
                        confirm_label: "Clear".into(),
                        action: ConfirmAction::ClearDrafts,
                    });
                }
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!("Settings file: {}", Settings::path().display()))
                        .size(11.0)
                        .color(self.theme.text_secondary),
                );
            }
        }
    }
}
