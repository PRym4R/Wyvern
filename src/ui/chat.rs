use eframe::egui::{self, Color32, RichText};

use crate::app::App;

impl App {
    pub(crate) fn draw_chat(&mut self, ctx: &egui::Context) {
        let mut style = (*ctx.style()).clone();
        style.visuals.panel_fill = self.theme.panel_bg;
        style.visuals.window_fill = self.theme.panel_bg;
        style.visuals.widgets.noninteractive.bg_fill = self.theme.channel_bg;
        style.visuals.widgets.inactive.bg_fill = self.theme.input_bg;
        style.visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, self.theme.text);
        style.visuals.widgets.hovered.bg_fill = self.theme.message_hover;
        style.visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, self.theme.accent);
        style.visuals.widgets.active.bg_fill = self.theme.accent;
        style.visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, Color32::BLACK);
        style.visuals.selection.bg_fill = self.theme.accent;
        style.visuals.selection.stroke = egui::Stroke::new(1.0, Color32::BLACK);
        style.visuals.hyperlink_color = self.theme.accent;
        style.spacing.item_spacing = egui::vec2(8.0, 2.0);
        style.spacing.button_padding = egui::vec2(10.0, 4.0);
        ctx.set_style(style);

        self.draw_server_list(ctx);
        let r1 = ctx.available_rect();
        self.draw_channel_list(ctx);
        let r2 = ctx.available_rect();
        self.draw_input_bar(ctx);
        let r3 = ctx.available_rect();
        self.draw_main_chat(ctx);
        let r4 = ctx.available_rect();
        if self.debug_frames % 15 == 0 {
            self.push_debug(format!("FRAME: screen={:.0}x{:.0} servers={:.0}x{:.0} channels={:.0}x{:.0} input={:.0}x{:.0} chat={:.0}x{:.0}",
                ctx.screen_rect().width(), ctx.screen_rect().height(),
                r1.width(), r1.height(),
                r2.width(), r2.height(),
                r3.width(), r3.height(),
                r4.width(), r4.height()));
        }
        self.debug_frames += 1;
    }
    pub(crate) fn draw_main_chat(&mut self, ctx: &egui::Context) {
        let msgs = self.current_channel_messages();

        for msg in &msgs {
            if let Some(avatar_hash) = &msg.author_avatar {
                let _ = self.download_avatar(ctx, &msg.author_id, avatar_hash);
            }
        }

        let channel_label = self.selected_channel
            .and_then(|i| self.channels.get(i))
            .map(|c| {
                if let Some(ref topic) = c.topic {
                    format!("{} — {}", c.name, topic)
                } else {
                    format!("# {}", c.name)
                }
            });

        let sel_cid = self.selected_channel
            .and_then(|i| self.channels.get(i))
            .map(|c| c.id.clone())
            .unwrap_or_else(|| "none".into());
        let key = format!("{}|{}", sel_cid, msgs.len());
        if key != self.last_render_key {
            self.last_render_key = key;
            let n_channels = self.channels.len();
            let srect = ctx.screen_rect();
            self.push_debug(format!("RENDER: sel='{}' sel_cid='{}' msgs={} total_channels={} screen={}x{}", channel_label.clone().unwrap_or_else(|| "none".into()), sel_cid, msgs.len(), n_channels, srect.width().round() as i32, srect.height().round() as i32));
        }

        match channel_label {
            Some(label) => {
                let msg_count = msgs.len();
                let stored_count = self.selected_channel
                    .and_then(|i| self.channels.get(i))
                    .map(|c| self.messages.get(&c.id).map_or(0, |v| v.len()))
                    .unwrap_or(0);
                let sel_id = self.selected_channel
                    .and_then(|i| self.channels.get(i))
                    .map(|c| c.id.clone())
                    .unwrap_or_else(|| "none".into());
                let stick = self.scroll_to_bottom;
                let panel_resp = egui::CentralPanel::default()
                    .frame(egui::Frame::none().fill(self.theme.channel_bg))
                    .show(ctx, |ui| {
                        ui.set_min_width(0.0);
                        let avail_w = ui.available_width();
                        let avail_h = ui.available_height();
                        ui.horizontal(|ui| {
                            ui.add_space(12.0);
                            ui.label(RichText::new("CHAT-AREA").strong().size(18.0).color(Color32::from_rgb(0, 255, 0)));
                            ui.label(RichText::new(format!("w={:.0} h={:.0}", avail_w, avail_h))
                                .size(12.0).color(Color32::from_rgb(255, 60, 60)));
                            ui.add_space(8.0);
                            ui.label(RichText::new(&label).strong().size(14.0).color(self.theme.text));
                            ui.add_space(8.0);
                            ui.label(RichText::new(format!("({} msgs / stored {})", msg_count, stored_count))
                                .size(12.0).color(self.theme.text_secondary));
                            ui.add_space(6.0);
                        });

                        ui.separator();

                        if self.history_loading.is_some() {
                            ui.horizontal_centered(|ui| {
                                ui.spinner();
                                ui.label(RichText::new("Loading messages...").color(self.theme.text_secondary).size(12.0));
                            });
                        }

                        let msgs_for_render = &msgs;
                        let scroll_out = egui::ScrollArea::vertical()
                            .id_salt(("chat", &sel_id))
                            .auto_shrink([false, false])
                            .stick_to_bottom(stick)
                            .show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                ui.add_space(8.0);
                                if msgs_for_render.is_empty() && self.history_loading.is_none() {
                                    ui.vertical_centered(|ui| {
                                        ui.add_space(60.0);
                                        ui.label(RichText::new("No messages here yet")
                                            .size(16.0).color(self.theme.text_secondary));
                                        ui.add_space(4.0);
                                        ui.label(RichText::new("Be the first to say something above")
                                            .size(12.0).color(self.theme.text_secondary));
                                    });
                                } else {
                                    for msg in msgs_for_render {
                                        let display = self.display_name(msg);
                                        let is_own = msg.is_own || (!self.user_id.is_empty() && msg.author_id == self.user_id);
                                        if is_own {
                                            let max_w = (ui.available_width() * 0.75).clamp(160.0, 480.0);
                                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                                                egui::Frame::none()
                                                    .fill(self.theme.self_bg)
                                                    .stroke(egui::Stroke::new(1.0, self.theme.divider))
                                                    .rounding(8.0)
                                                    .inner_margin(egui::Margin::symmetric(10, 8))
                                                    .show(ui, |ui| {
                                                    ui.set_max_width(max_w);
                                                    ui.vertical(|ui| {
                                                        ui.horizontal(|ui| {
                                                            ui.label(RichText::new(&display).strong().size(14.0).color(self.theme.accent));
                                                            if !msg.timestamp.is_empty() {
                                                                let t = self.short_time(&msg.timestamp);
                                                                ui.label(RichText::new(t).size(11.0).color(self.theme.text_secondary));
                                                            }
                                                        });
                                                        let content = self.display_content(msg);
                                                        let content = if content.is_empty() { "* (message)".to_string() } else { content };
                                                        ui.label(RichText::new(content).size(14.0).color(self.theme.text));
                                                        self.draw_attachments(ui, msg);
                                                    });
                                                });
                                            });
                                            ui.add_space(6.0);
                                        } else {
                                            egui::Frame::none()
                                                .fill(self.theme.message_hover)
                                                .stroke(egui::Stroke::new(1.0, self.theme.divider))
                                                .rounding(8.0)
                                                .inner_margin(egui::Margin::symmetric(10, 8))
                                                .show(ui, |ui| {
                                                ui.set_min_height(44.0);
                                                ui.horizontal(|ui| {
                                                    let size = 36.0;
                                                    let avatar_tex = msg.author_avatar.as_ref()
                                                        .and_then(|h| self.avatar_cache.get(&format!("{}_{}", msg.author_id, h)).cloned())
                                                        .or_else(|| {
                                                            if let Some(h) = &msg.author_avatar {
                                                                self.download_avatar(ui.ctx(), &msg.author_id, h)
                                                            } else { None }
                                                        });
                                                    if let Some(tex) = avatar_tex {
                                                        ui.add_sized(
                                                            egui::vec2(size, size),
                                                            egui::Image::new(
                                                                egui::load::SizedTexture::new(tex.id(), egui::vec2(size, size))
                                                            ).rounding(size / 2.0),
                                                        );
                                                    } else {
                                                        let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
                                                        ui.painter().circle_filled(rect.center(), size / 2.0, self.theme.accent);
                                                        let initial = msg.author_name.chars().next().unwrap_or('?').to_uppercase().to_string();
                                                        ui.painter().text(
                                                            rect.center(), egui::Align2::CENTER_CENTER, &initial,
                                                            egui::FontId::proportional(16.0), Color32::WHITE,
                                                        );
                                                    }
                                                    ui.vertical(|ui| {
                                                        ui.horizontal(|ui| {
                                                            ui.label(RichText::new(&display).strong().size(14.0).color(self.theme.accent));
                                                            if !msg.timestamp.is_empty() {
                                                                let t = self.short_time(&msg.timestamp);
                                                                ui.label(RichText::new(t).size(11.0).color(self.theme.text_secondary));
                                                            }
                                                        });
                                                        let content = self.display_content(msg);
                                                        let content = if content.is_empty() { "* (message)".to_string() } else { content };
                                                        ui.label(RichText::new(content).size(14.0).color(self.theme.text));
                                                        self.draw_attachments(ui, msg);
                                                    });
                                                });
                                                });
                                                ui.add_space(6.0);
                                        }
                                    }
                                }
                                ui.add_space(8.0);
                            });
                        let inner_h = scroll_out.inner_rect.height();
                        let content_h = scroll_out.content_size.y;
                        let offset_y = scroll_out.state.offset.y;
                        self.push_debug(format!("SCROLL: inner_h={:.0} content_h={:.0} offset_y={:.0} stick={}", inner_h, content_h, offset_y, stick));

                        if self.scroll_to_bottom {
                            if content_h > inner_h {
                                let mut st = scroll_out.state;
                                st.offset.y = content_h - inner_h;
                                self.last_scroll_offset_y = st.offset.y;
                                st.store(ui.ctx(), scroll_out.id);
                            }
                            self.scroll_to_bottom = false;
                        }
                        self.last_scroll_offset_y = self.last_scroll_offset_y.max(offset_y);
                    });
                let presp = panel_resp.response.rect;
                let p_rect = presp.min;
                let srect = ctx.screen_rect();
                self.push_debug(format!("PANEL: rect={:.0}x{:.0}+({:.0},{:.0}) screen={:.0}x{:.0}", presp.width(), presp.height(), p_rect.x, p_rect.y, srect.width(), srect.height()));
            }
            None => {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(120.0);
                        ui.label(RichText::new("Welcome to Wyvern").size(24.0).color(self.theme.text));
                        ui.add_space(8.0);
                        ui.label(RichText::new("1. Click a server icon on the far left")
                            .size(14.0).color(self.theme.text_secondary));
                        ui.label(RichText::new("2. Click a channel (# name) in the middle list")
                            .size(14.0).color(self.theme.text_secondary));
                        ui.label(RichText::new("3. Type a message at the bottom and press Enter")
                            .size(14.0).color(self.theme.text_secondary));
                    });
                });
            }
        }
    }
}
