use eframe::egui::{self, RichText};

use crate::app::{ConfirmAction, ConfirmDialog};
use crate::settings::Settings;

/// Tabs shown down the left of the settings window.
const SECTIONS: [&str; 6] = ["Theme", "Interface", "Media", "History", "Network", "Advanced"];

/// Proxy kinds for the Network tab dropdown.
const PROXY_KINDS: [&str; 4] = ["HTTP", "SOCKS5", "SOCKS5 (remote DNS)", "Shadowsocks"];

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
                ui.label("Route Discord traffic through a proxy. Leave the host empty for a direct connection.");
                ui.add_space(6.0);
                let mut changed = false;
                let mut commit = false;
                egui::Grid::new("proxy_grid")
                    .num_columns(2)
                    .spacing([10.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Type");
                        egui::ComboBox::from_id_salt("proxy_kind")
                            .selected_text(PROXY_KINDS[self.proxy_kind.min(3)])
                            .show_ui(ui, |ui| {
                                for (i, label) in PROXY_KINDS.iter().enumerate() {
                                    changed |= ui
                                        .selectable_value(&mut self.proxy_kind, i, *label)
                                        .changed();
                                }
                            });
                        ui.end_row();

                        ui.label("Host");
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut self.proxy_host)
                                .hint_text("127.0.0.1 or proxy.example.com")
                                .desired_width(320.0),
                        );
                        changed |= r.changed();
                        commit |= r.lost_focus();
                        ui.end_row();

                        ui.label("Port");
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut self.proxy_port)
                                .hint_text("8080")
                                .desired_width(120.0),
                        );
                        changed |= r.changed();
                        commit |= r.lost_focus();
                        ui.end_row();

                        let ss = self.proxy_kind == 3;
                        ui.label(if ss { "Method" } else { "Username" });
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut self.proxy_user)
                                .hint_text(if ss {
                                    "e.g. chacha20-ietf-poly1305"
                                } else {
                                    "optional"
                                })
                                .desired_width(320.0),
                        );
                        changed |= r.changed();
                        commit |= r.lost_focus();
                        ui.end_row();

                        ui.label("Password");
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut self.proxy_pass)
                                .hint_text(if ss { "required" } else { "optional" })
                                .password(true)
                                .desired_width(320.0),
                        );
                        changed |= r.changed();
                        commit |= r.lost_focus();
                        ui.end_row();

                        ui.label("ss:// link");
                        ui.horizontal(|ui| {
                            let r = ui.add(
                                egui::TextEdit::singleline(&mut self.proxy_link)
                                    .hint_text("paste an ss:// link to fill the fields")
                                    .desired_width(250.0),
                            );
                            if ui.button("Import").clicked()
                                || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                            {
                                self.import_proxy_link();
                                commit = true;
                            }
                        });
                        ui.end_row();
                    });
                if changed {
                    self.apply_proxy_form_live();
                }
                if commit {
                    self.write_settings();
                }
                if ui.button("Clear proxy").clicked() {
                    self.proxy_kind = 0;
                    self.proxy_host.clear();
                    self.proxy_port.clear();
                    self.proxy_user.clear();
                    self.proxy_pass.clear();
                    self.proxy_link.clear();
                    self.apply_proxy_form();
                }
                ui.add_space(6.0);
                let active = self
                    .settings
                    .proxy
                    .clone()
                    .unwrap_or_else(|| "none (direct)".into());
                ui.label(
                    RichText::new(format!("Active: {active}"))
                        .size(12.0)
                        .color(self.theme.text),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "HTTP, SOCKS5 (with optional username/password) and Shadowsocks \
                         (aes-128/256-gcm, chacha20-ietf-poly1305) are supported. API and media \
                         requests use it right away; the gateway reconnects to switch. Shadowsocks \
                         runs natively — paste an ss:// link above to fill the fields.",
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
