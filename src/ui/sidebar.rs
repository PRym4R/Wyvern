use eframe::egui::{self, Color32, RichText};

use crate::app::App;
use crate::messages::ToGateway;

impl App {
    pub(crate) fn draw_server_list(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("servers")
            .resizable(false)
            .default_width(72.0)
            .exact_width(72.0)
            .frame(egui::Frame::new().fill(self.theme.bg))
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    let home_btn = ui.add_sized(
                        [48.0, 48.0],
                        egui::Button::new(RichText::new("@").size(20.0).color(Color32::WHITE))
                            .fill(if self.selected_guild.is_none() { self.theme.accent } else { self.theme.input_bg })
                            .corner_radius(24.0),
                    );
                    if home_btn.clicked() {
                        self.selected_guild = None;
                        self.selected_channel = None;
                        self.show_friends = !self.show_friends;
                    }
                });
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);

                egui::ScrollArea::vertical().show(ui, |ui| {
                    let guilds = self.guilds.clone();
                    for (i, guild) in guilds.iter().enumerate() {
                        ui.vertical_centered(|ui| {
                            let is_sel = self.selected_guild == Some(i);
                            let icon_tex = guild.icon.as_ref()
                                .and_then(|h| self.download_guild_icon(ctx, &guild.id, h));
                            let btn = if let Some(tex) = icon_tex {
                                let img = tex.clone();
                                ui.add_sized(
                                    [48.0, 48.0],
                                    egui::ImageButton::new(
                                        egui::load::SizedTexture::new(img.id(), egui::vec2(48.0, 48.0))
                                    ).corner_radius(24.0),
                                )
                            } else {
                                let label: String = guild.name.chars().take(2).collect();
                                ui.add_sized(
                                    [48.0, 48.0],
                                    egui::Button::new(RichText::new(&label).size(16.0).color(Color32::WHITE))
                                        .fill(if is_sel { self.theme.accent } else { self.theme.input_bg })
                                        .corner_radius(24.0),
                                )
                            };
                            if btn.clicked() {
                                self.selected_guild = Some(i);
                                self.selected_channel = None;
                                self.show_friends = false;
                            }
                        });
                        ui.add_space(6.0);
                    }
                });

                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    let ubtn = ui.add_sized(
                        [48.0, 48.0],
                        egui::Button::new(RichText::new("◐").size(20.0).color(Color32::WHITE))
                            .fill(self.theme.input_bg)
                            .corner_radius(24.0),
                    );
                    if ubtn.clicked() {
                        self.theme_index = (self.theme_index + 1) % 3;
                    }
                    ubtn.on_hover_text("Toggle theme");

                    let abtn = ui.add_sized(
                        [48.0, 42.0],
                        egui::Button::new(RichText::new("👤").size(18.0).color(Color32::WHITE))
                            .fill(if self.connected { self.theme.accent } else { self.theme.input_bg })
                            .corner_radius(21.0),
                    );
                    let popup_id = egui::Id::new("account_switcher");
                    if abtn.clicked() {
                        ui.close_menu();
                        ui.memory_mut(|m| m.toggle_popup(popup_id));
                    }
                    abtn.clone().on_hover_text(&self.username);
                    egui::popup_below_widget(ui, popup_id, &abtn, egui::PopupCloseBehavior::CloseOnClickOutside, |ui| {
                        ui.set_min_width(190.0);
                        ui.label(RichText::new("Accounts").strong().size(12.0).color(self.theme.text_secondary));
                        ui.add_space(4.0);
                        ui.separator();
                        let mut switch_to: Option<String> = None;
                        let accounts = self.saved_accounts.clone();
                        for (i, acc) in accounts.iter().enumerate() {
                            let label = if acc.username.is_empty() {
                                self.mask_token(&acc.token)
                            } else {
                                acc.username.clone()
                            };
                            let is_active = self.active_index == Some(i);
                            let text = if is_active {
                                format!("{} ✓", label)
                            } else {
                                label
                            };
                            if ui.add(egui::Button::new(RichText::new(text).size(13.0)
                                .color(if is_active { Color32::BLACK } else { self.theme.text }))
                                .fill(if is_active { self.theme.accent } else { self.theme.input_bg })
                                .min_size(egui::vec2(180.0, 30.0))).clicked() && !is_active
                            {
                                switch_to = Some(acc.token.clone());
                            }
                        }
                        if let Some(t) = switch_to {
                            self.switch_account(t);
                            ui.close_menu();
                        }
                    });
                });
            });
    }
    pub(crate) fn draw_channel_list(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("channels")
            .resizable(true)
            .default_width(240.0)
            .min_width(180.0)
            .frame(egui::Frame::new().fill(self.theme.panel_bg))
            .show(ctx, |ui| {
                ui.set_min_width(ui.available_width());

                match self.selected_guild {
                    Some(guild_idx) => {
                        let Some(guild) = self.guilds.get(guild_idx).cloned() else {
                            self.selected_guild = None;
                            return;
                        };
                        ui.add_space(12.0);
                        ui.label(RichText::new(&guild.name).strong().size(15.0).color(self.theme.text));
                        ui.add_space(4.0);
                        ui.separator();
                        ui.add_space(4.0);

                        // Собираем только индексы каналов гильдии, а не копии
                        // самих каналов: список перерисовывается 20 раз в
                        // секунду, и полное клонирование на кадр съедало
                        // тысячи аллокаций.
                        let ch_idx: Vec<usize> = self.channels.iter()
                            .enumerate()
                            .filter(|(_, ch)| {
                                ch.guild_id.as_deref() == Some(guild.id.as_str()) && ch.channel_type == 0
                            })
                            .map(|(i, _)| i)
                            .collect();
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            for i in ch_idx {
                                let Some(ch) = self.channels.get(i) else { continue };
                                let is_sel = self.selected_channel == Some(i);
                                let label = format!("# {}", ch.name);
                                let response = ui.add_sized(
                                    [ui.available_width(), 32.0],
                                    egui::Button::new(
                                        RichText::new(&label)
                                            .color(if is_sel { Color32::WHITE } else { self.theme.text_secondary })
                                            .size(14.0)
                                    ).fill(if is_sel { self.theme.accent } else { Color32::TRANSPARENT })
                                );
                                if response.clicked() {
                                    let cid = ch.id.clone();
                                    self.selected_channel = Some(i);
                                    self.scroll_to_bottom = true;
                                    self.open_channel(&cid);
                                }
                            }
                        });
                    }
                    None => {
                        if self.show_friends {
                            ui.add_space(12.0);
                            ui.label(RichText::new("Friends").strong().size(15.0).color(self.theme.text));
                            ui.add_space(8.0);
                            ui.label(RichText::new(format!("{} friends", self.friends.len()))
                                .size(12.0).color(self.theme.text_secondary));
                            ui.add_space(4.0);
                            ui.separator();
                            // Список друзей и список DM перебираем по индексам и
                            // копируем только нужные строки: раньше тут на
                            // каждом кадре клонировался весь список целиком
                            // (сотни аллокаций 20 раз в секунду).
                            let friend_count = self.friends.len();
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                for i in 0..friend_count {
                                    let (label, username) = match self.friends.get(i) {
                                        Some(f) => (format!("@{}", f.username), f.username.clone()),
                                        None => continue,
                                    };
                                    let response = ui.add_sized(
                                        [ui.available_width(), 32.0],
                                        egui::Button::new(
                                            RichText::new(&label).size(14.0).color(self.theme.text)
                                        ).fill(Color32::TRANSPARENT),
                                    );
                                    if response.clicked() {
                                        if let Some(f) = self.friends.get(i) {
                                            if f.id.is_empty() {
                                                continue;
                                            }
                                            let fid = f.id.clone();
                                            self.push_debug(format!("Clicked friend '{}' (id {})", username, &fid[..fid.len().min(12)]));
                                            self.send_cmd(ToGateway::OpenDM { user_id: fid });
                                        }
                                    }
                                }
                            });
                        } else {
                            ui.add_space(12.0);
                            ui.label(RichText::new("DMs").strong().size(15.0).color(self.theme.text));
                            ui.add_space(4.0);
                            ui.separator();
                            let dm_count = self.channels.len();
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                for i in 0..dm_count {
                                    let (cid, cname) = match self.channels.get(i) {
                                        Some(ch)
                                            if ch.guild_id.is_none()
                                                && (ch.channel_type == 1 || ch.channel_type == 3) =>
                                        {
                                            (ch.id.clone(), ch.name.clone())
                                        }
                                        _ => continue,
                                    };
                                    let is_sel = self.selected_channel
                                        .and_then(|s| self.channels.get(s))
                                        .map(|c| c.id == cid)
                                        .unwrap_or(false);
                                    let response = ui.add_sized(
                                        [ui.available_width(), 32.0],
                                        egui::Button::new(
                                            RichText::new(&cname)
                                                .color(if is_sel { Color32::WHITE } else { self.theme.text_secondary })
                                                .size(14.0)
                                        ).fill(if is_sel { self.theme.accent } else { Color32::TRANSPARENT })
                                    );
                                    if response.clicked() {
                                        self.selected_channel = Some(i);
                                        self.scroll_to_bottom = true;
                                        self.open_channel(&cid);
                                    }
                                }
                            });
                        }
                    }
                }
            });
    }
}
