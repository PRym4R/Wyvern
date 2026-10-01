use eframe::egui::{self, Color32, RichText};

use crate::app::App;

use crate::messages::ToGateway;
use crate::models::{ChatChannel, ChatMessage, LOCAL_ID_PREFIX};

impl App {
    pub(crate) fn draw_input_bar(&mut self, ctx: &egui::Context) {
        let has_channel = self.selected_channel.is_some();
        let ch_name = self.selected_channel
            .and_then(|i| self.channels.get(i))
            .map(|c| format!("# {}", c.name))
            .unwrap_or_else(|| "No channel selected".into());

        let _input_resp = egui::TopBottomPanel::bottom("input_panel")
            .min_height(56.0)
            .show(ctx, |ui| {
                let ir = ui.min_rect();
                self.push_debug(format!("INPUT_BAR: h={:.0} y={:.0}", ir.height(), ir.min.y));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    ui.label(RichText::new(ch_name).strong().size(13.0).color(self.theme.text_secondary));
                    ui.add_space(8.0);

                    if !has_channel {
                        ui.label(RichText::new("← select a channel on the left").italics()
                            .size(12.0).color(self.theme.text_secondary));
                    } else {
                        let resp = ui.add_sized(
                            [ui.available_width() - 110.0, 36.0],
                            egui::TextEdit::singleline(&mut self.input)
                                .hint_text("Type a message and press Enter, or click Send...")
                                .margin(egui::Margin::symmetric(12, 8)),
                        );
                        let send_btn = ui.add_sized(
                            [84.0, 36.0],
                            egui::Button::new(RichText::new("Send").size(14.0).color(Color32::WHITE))
                                .fill(self.theme.accent),
                        );

                        let enter_pressed = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        let send_clicked = send_btn.clicked();

                        if enter_pressed || send_clicked {
                            let text = self.input.trim().to_string();
                            if !text.is_empty() {
                                self.handle_input(&text);
                                self.input.clear();
                            }
                            resp.request_focus();
                        }
                    }
                });
                ui.add_space(6.0);
            });
    }
    pub(crate) fn handle_input(&mut self, text: &str) {
        if text == "/quit" {
            std::process::exit(0);
        }
        if let Some(id) = text.strip_prefix("/add ") {
            let id = id.trim().to_string();
            if !id.is_empty() {
                self.channels.push(ChatChannel {
                    id: id.clone(),
                    name: format!("#{}", &id),
                    guild_id: None,
                    channel_type: 0,
                    topic: None,
                    position: 999,
                });
                let idx = self.channels.len() - 1;
                self.selected_channel = Some(idx);
                self.scroll_to_bottom = true;
                self.open_channel(&id);
            }
            return;
        }

        if let Some(idx) = self.selected_channel {
            let cid = self.channels[idx].id.clone();
            // Показываем своё сообщение сразу, не дожидаясь Discord. Настоящий
            // id у него ещё нет, поэтому даём заведомо ненастоящий, помеченный
            // префиксом: когда придёт MESSAGE_CREATE, клиент заменит эту
            // строку на присланную, а не добавит вторую копию. Раньше id был
            // пустой, проверка дубля по id его не видела, и каждое отправленное
            // сообщение показывалось дважды.
            let local_id = format!("{}{}", LOCAL_ID_PREFIX, self.next_local_id);
            self.next_local_id += 1;
            // Прежняя неудача погасла: пользователь пишет заново, значит
            // сообщение о старом отказе уже не в тему.
            self.send_error = None;
            self.messages.entry(cid.clone()).or_default().push(std::sync::Arc::new(ChatMessage {
                id: local_id.clone(),
                channel_id: cid.clone(),
                author_id: self.user_id.clone(),
                author_name: self.username.clone(),
                author_avatar: self.user_avatar.clone(),
                nickname: None,
                content: text.to_string(),
                timestamp: String::new(),
                attachments: Vec::new(),
                embeds: Vec::new(),
                is_own: true,
            }));
            self.send_cmd(ToGateway::Send { channel_id: cid, content: text.to_string(), local_id });
        }
    }
}
