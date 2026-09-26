use eframe::egui::{self, Color32, RichText};

use crate::app::App;

impl App {
    pub(crate) fn draw_login(&mut self, ctx: &egui::Context) {
        let style = (*ctx.style()).clone();
        let mut style = style;
        style.visuals.panel_fill = self.theme.channel_bg;
        style.visuals.widgets.inactive.bg_fill = self.theme.input_bg;
        style.visuals.widgets.hovered.bg_fill = self.theme.message_hover;
        ctx.set_style(style);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(100.0);
                ui.heading(RichText::new("Wyvern").size(32.0).color(self.theme.accent));
                ui.add_space(8.0);
                ui.label(RichText::new("Rust-powered, lightweight")
                    .size(14.0).color(self.theme.text_secondary));
                ui.add_space(32.0);
                ui.label(RichText::new("Session Token").color(self.theme.text));
                ui.add_space(6.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.token_input)
                        .password(true)
                        .desired_width(380.0)
                        .hint_text("Paste your token...")
                        .font(egui::FontId::proportional(16.0)),
                );
                ui.add_space(16.0);
                let btn = ui.add_sized(
                    [380.0, 42.0],
                    egui::Button::new(RichText::new("Login").size(16.0).color(Color32::BLACK))
                        .fill(self.theme.accent),
                );
                if btn.clicked() && !self.token_input.is_empty() {
                    let t = self.token_input.trim().to_string();
                    self.start_gateway(t.clone());
                    if self.accounts_unlocked {
                        self.add_saved_account(&t, "");
                    }
                }
                ui.add_space(10.0);
                ui.label(RichText::new("Master password (хранит аккаунты в шифрованном виде)").color(self.theme.text_secondary).size(12.0));
                ui.add_space(6.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.master_password)
                        .password(true)
                        .desired_width(380.0)
                        .hint_text("Password to unlock saved accounts...")
                        .font(egui::FontId::proportional(16.0)),
                );
                let unlock_btn = ui.add_sized(
                    [380.0, 36.0],
                    egui::Button::new(RichText::new(if self.accounts_unlocked { "Vault unlocked" } else { "Unlock / Create vault" }).size(14.0).color(Color32::BLACK))
                        .fill(if self.accounts_unlocked { self.theme.accent } else { self.theme.text_secondary }),
                );
                if unlock_btn.clicked() && !self.master_password.is_empty() {
                    let pw = self.master_password.clone();
                    self.saved_accounts = Self::load_accounts(&pw);
                    self.refresh_active_index();
                    self.accounts_unlocked = true;
                }
                ui.add_space(12.0);
                if !self.status.is_empty() {
                    ui.label(RichText::new(&self.status).color(Color32::from_rgb(250, 77, 77)).size(13.0));
                }
                if !self.debug_log.is_empty() {
                    ui.add_space(8.0);
                    ui.label(RichText::new(format!("Last: {}", self.debug_log.last().unwrap()))
                        .color(self.theme.text_secondary).size(11.0));
                }

                if !self.saved_accounts.is_empty() {
                    ui.add_space(30.0);
                    ui.label(RichText::new("Saved accounts:").strong().size(13.0).color(self.theme.text_secondary));
                    ui.add_space(6.0);
                    let mut switch_to: Option<String> = None;
                    let mut remove_idx: Option<usize> = None;
                    for (i, acc) in self.saved_accounts.iter().enumerate() {
                        ui.horizontal(|ui| {
                            let label = if acc.username.is_empty() {
                                self.mask_token(&acc.token)
                            } else {
                                acc.username.clone()
                            };
                            let is_active = self.active_index == Some(i);
                            let txt = if is_active { format!("{} (active)", label) } else { label };
                            let b = ui.add_sized(
                                [320.0, 34.0],
                                egui::Button::new(RichText::new(txt).size(13.0)
                                    .color(if is_active { Color32::BLACK } else { self.theme.text }))
                                    .fill(if is_active { self.theme.accent } else { self.theme.input_bg })
                                    .rounding(6.0),
                            );
                            if b.clicked() && !is_active {
                                switch_to = Some(acc.token.clone());
                            }
                            let x = ui.add_sized(
                                [28.0, 34.0],
                                egui::Button::new(RichText::new("✕").size(13.0).color(self.theme.text_secondary))
                                    .fill(self.theme.input_bg)
                                    .rounding(6.0),
                            );
                            if x.clicked() {
                                remove_idx = Some(i);
                            }
                        });
                        ui.add_space(4.0);
                    }
                    if let Some(t) = switch_to {
                        self.switch_account(t);
                    }
                    if let Some(i) = remove_idx {
                        self.saved_accounts.remove(i);
                        self.refresh_active_index();
                        self.save_accounts(&self.master_password);
                    }
                }

                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Add token:").size(12.0).color(self.theme.text_secondary));
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut self.login_new_token)
                            .password(true)
                            .desired_width(280.0)
                            .hint_text("Paste token to save...")
                            .font(egui::FontId::proportional(13.0)),
                    );
                    let save_btn = ui.add_sized(
                        [90.0, 28.0],
                        egui::Button::new(RichText::new("Save").size(13.0).color(Color32::BLACK))
                            .fill(self.theme.accent)
                            .rounding(6.0),
                    );
                    let entered = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (save_btn.clicked() || entered) && !self.login_new_token.trim().is_empty() {
                        let t = self.login_new_token.trim().to_string();
                        self.add_saved_account(&t, "");
                        self.login_new_token.clear();
                    }
                });
            });
        });
    }
}
