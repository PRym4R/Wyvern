use eframe::egui::{self, Color32, RichText};

use crate::app::App;

use crate::messages::ToGateway;
use crate::models::{ChatChannel, ChatMessage, LOCAL_ID_PREFIX};
use crate::ui::ERROR_RED;

/// Debug-command prefix; anything else is plain channel text, even if command-like.
const DEBUG_PREFIX: &str = "/debug ";
/// How long a command waits for confirmation before being forgotten.
const DEBUG_CONFIRM_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

/// Narrowest usable input-field width; it must not collapse to zero.
const MIN_INPUT_WIDTH: f32 = 60.0;
/// Row space taken by padding, channel name, "Send" button and gaps.
const INPUT_CHROME: f32 = 110.0;
/// Width reserved for the "n/2000" counter; always kept so the field doesn't jump.
const COUNTER_WIDTH: f32 = 60.0;
/// Discord message-length limit in Unicode characters (code points, not bytes).
pub(crate) const MAX_MESSAGE_CHARS: usize = 2000;

/// Whether text fits Discord's limit; checked before sending.
pub(crate) fn within_message_limit(text: &str) -> bool {
    text.chars().count() <= MAX_MESSAGE_CHARS
}

/// Input-field width from the row's free space, clamped to a minimum.
pub(crate) fn input_width(available: f32) -> f32 {
    (available - INPUT_CHROME).max(MIN_INPUT_WIDTH)
}

/// Trim whitespace plus invisible characters that plain `trim()` misses.
pub(crate) fn trim_input(text: &str) -> &str {
    text.trim_matches(|c: char| {
        c.is_whitespace() || matches!(c, '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}' | '\u{2060}')
    })
}

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
                // Reply bar: shows what the next message answers and can be cancelled.
                if let Some(reply) = self.reply_to.clone() {
                    ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        let (bar, _) =
                            ui.allocate_exact_size(egui::vec2(2.0, 16.0), egui::Sense::hover());
                        ui.painter().rect_filled(bar, 1.0, self.theme.accent);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Отмена").clicked() {
                                self.cancel_reply();
                            }
                            ui.add(
                                egui::Label::new(
                                    RichText::new(format!(
                                        "Ответ {}: {}",
                                        reply.author_name, reply.preview
                                    ))
                                    .size(12.0)
                                    .color(self.theme.text_secondary),
                                )
                                .truncate(),
                            );
                        });
                    });
                }
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    ui.label(RichText::new(ch_name).strong().size(13.0).color(self.theme.text_secondary));
                    ui.add_space(8.0);

                    if !has_channel {
                        ui.label(RichText::new("← select a channel on the left").italics()
                            .size(12.0).color(self.theme.text_secondary));
                    } else {
                        // Show the counter only when there is text; its space stays reserved.
                        let typed = trim_input(&self.input);
                        let count = typed.chars().count();
                        let fits = within_message_limit(typed);
                        let (counter_rect, _) = ui.allocate_exact_size(
                            egui::vec2(COUNTER_WIDTH, 16.0),
                            egui::Sense::hover(),
                        );
                        if count > 0 {
                            ui.painter().text(
                                counter_rect.right_center(),
                                egui::Align2::RIGHT_CENTER,
                                format!("{count}/{MAX_MESSAGE_CHARS}"),
                                egui::FontId::proportional(12.0),
                                if fits { self.theme.text_secondary } else { ERROR_RED },
                            );
                        }
                        ui.add_space(6.0);
                        let field_w = input_width(ui.available_width());
                        self.push_debug(format!("INPUT_FIELD: w={:.0}", field_w));
                        let resp = ui.add_sized(
                            [field_w, 36.0],
                            egui::TextEdit::singleline(&mut self.input)
                                .hint_text("Type a message and press Enter, or click Send...")
                                .margin(egui::Margin::symmetric(12, 8)),
                        );
                        // Any edit clears the held-Enter resend guard; set before the Enter check.
                        if resp.changed() {
                            self.input_dirty = true;
                        }
                        // Block sending while text exceeds the limit, so Discord won't reject it.
                        let send_btn = ui.add_enabled(
                            fits,
                            egui::Button::new(RichText::new("Send").size(14.0).color(Color32::WHITE))
                                .fill(self.theme.accent)
                                .min_size(egui::vec2(84.0, 36.0)),
                        ).on_hover_text(if fits { "" } else { "сообщение длиннее 2000 символов" });

                        let enter_pressed = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        let send_clicked = send_btn.clicked();

                        if enter_pressed {
                            self.submit_from_enter();
                            resp.request_focus();
                        } else if send_clicked {
                            self.submit_input();
                            resp.request_focus();
                        }
                    }
                });
                ui.add_space(6.0);
            });
    }
    pub(crate) fn handle_input(&mut self, text: &str) {
        // Debug commands are prefix-gated and confirmed, so a typo can't add a hidden channel.
        if let Some(rest) = text.strip_prefix(DEBUG_PREFIX) {
            self.handle_debug_command(rest.trim());
            return;
        }

        if let Some(idx) = self.selected_channel {
            let cid = self.channels[idx].id.clone();
            // Reply target, if any, travels with this send only.
            let reply_to = self.reply_to.as_ref().map(|r| r.message_id.clone());
            // Optimistic echo with a fake local id; MESSAGE_CREATE replaces it by id.
            let local_id = format!("{}{}", LOCAL_ID_PREFIX, self.next_local_id);
            self.next_local_id += 1;
            // The user is typing again, so the old send error is stale.
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
            self.send_cmd(ToGateway::Send { channel_id: cid, content: text.to_string(), local_id, reply_to });
        }
    }

    /// Enter or "Send": send if there's text, then always clear the field so whitespace doesn't linger.
    pub(crate) fn submit_input(&mut self) {
        let text = trim_input(&self.input).to_string();
        // Too long: don't send and don't clear, so the text can be shortened.
        if !within_message_limit(&text) {
            self.push_debug(format!(
                "Message over limit: {} chars > {}",
                text.chars().count(),
                MAX_MESSAGE_CHARS
            ));
            return;
        }
        if !text.is_empty() {
            self.handle_input(&text);
            // The reply target is consumed by this send; the next message starts fresh.
            self.reply_to = None;
            // Remember what was sent and clear the edit flag to block duplicate sends.
            self.last_submitted = Some(text);
            self.input_dirty = false;
        }
        self.input.clear();
    }

    /// Like `submit_input`, but held Enter (auto-repeat) must not resend the same text.
    pub(crate) fn submit_from_enter(&mut self) {
        let text = trim_input(&self.input).to_string();
        if !text.is_empty()
            && self.last_submitted.as_deref() == Some(text.as_str())
            && !self.input_dirty
        {
            return;
        }
        self.submit_input();
    }

    /// Drops the pending reply, returning the composer to its normal state.
    pub(crate) fn cancel_reply(&mut self) {
        self.reply_to = None;
    }

    /// Debug command from the message field; the first press only asks for confirmation.
    fn handle_debug_command(&mut self, rest: &str) {
        if let Some(id) = rest.strip_prefix("add ").map(str::trim) {
            if id.is_empty() {
                self.status = "нужен id канала: /debug add <id>".to_string();
                return;
            }
            // Repeating the same command within the window confirms it.
            let confirmed = match &self.pending_debug_add {
                Some((prev, when)) => prev == id && when.elapsed() < DEBUG_CONFIRM_WINDOW,
                None => false,
            };
            if confirmed {
                self.pending_debug_add = None;
                self.add_debug_channel(id);
            } else {
                self.pending_debug_add = Some((id.to_string(), std::time::Instant::now()));
                self.status = format!("канал {id} будет добавлен после повторного /debug add {id}");
                self.push_debug(format!("Debug add awaiting confirmation: {id}"));
            }
            return;
        }
        self.status = format!("неизвестная отладочная команда: {rest}");
        self.push_debug(format!("Unknown debug command: {rest}"));
    }

    /// Add a channel directly, bypassing Discord's lists; only reachable by id.
    fn add_debug_channel(&mut self, id: &str) {
        self.channels.push(ChatChannel {
            id: id.to_string(),
            name: format!("#{}", id),
            guild_id: None,
            channel_type: 0,
            topic: None,
            position: 999,
        });
        let idx = self.channels.len() - 1;
        self.selected_channel = Some(idx);
        self.scroll_to_bottom = true;
        self.open_channel(id);
        self.status = format!("канал {id} добавлен");
        self.push_debug(format!("Debug channel added: {id}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, ReplyTarget};
    use crate::models::ChatChannel;

    /// Whitespace-only input must not survive submit: the field clears even when nothing is sent.
    #[test]
    fn whitespace_only_input_is_cleared_on_submit() {
        let (mut app, _ctx) = narrow_app();
        app.input = "   \u{200b}\n".to_string();

        app.submit_input();

        assert!(
            app.input.is_empty(),
            "в поле осталось {:?}, хотя отправлять было нечего",
            app.input
        );
        assert!(
            app.messages.get("c1").is_none_or(|v| v.is_empty()),
            "пробелы не должны становиться сообщением"
        );
    }

    /// Plain text is sent trimmed, and the field is empty afterwards.
    #[test]
    fn submit_sends_trimmed_text_and_clears_the_field() {
        let (mut app, _ctx) = narrow_app();
        app.input = "  привет  ".to_string();

        app.submit_input();

        assert_eq!(app.input, "");
        assert_eq!(app.messages["c1"][0].content, "привет");
        // The field is clear right after sending, so a second Enter sends nothing.
        let sent_before = app.messages["c1"].len();
        app.submit_input();
        assert_eq!(app.messages["c1"].len(), sent_before, "пустое поле не должно ничего слать");
    }

    /// Held Enter must not resend the same text, while keeping the restored text in the field.
    #[test]
    fn held_enter_does_not_resend_restored_text() {
        let (mut app, _ctx) = narrow_app();
        app.input = "привет".to_string();
        app.input_dirty = true; // user typed text
        app.submit_from_enter();
        assert_eq!(app.messages["c1"].len(), 1, "первый Enter должен отправить");

        // Failed send restored the same text; the key is still held.
        app.input = "привет".to_string();
        app.input_dirty = false;
        app.submit_from_enter();
        assert_eq!(
            app.messages["c1"].len(),
            1,
            "повтор того же текста зажатым Enter не должен уйти второй раз"
        );
        assert_eq!(app.input, "привет", "заблокированный Enter не должен стирать поле");

        // The user edited the text, so sending works again.
        app.input = "привет!".to_string();
        app.input_dirty = true;
        app.submit_from_enter();
        assert_eq!(app.messages["c1"].len(), 2);
    }

    /// App with an open channel in the narrowest supported window.
    fn narrow_app() -> (App, egui::Context) {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(rx);
        app.connected = true;
        app.gw_started = true;
        app.channels.push(ChatChannel {
            id: "c1".into(),
            name: "chan".into(),
            guild_id: None,
            channel_type: 1,
            topic: None,
            position: 0,
        });
        app.selected_channel = Some(0);
        let ctx = egui::Context::default();
        (app, ctx)
    }

    fn frame() -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(700.0, 600.0))),
            ..Default::default()
        }
    }

    /// Field width must never go negative: egui draws negative rects empty, hiding the field.
    #[test]
    fn input_width_never_goes_negative() {
        assert_eq!(input_width(0.0), MIN_INPUT_WIDTH);
        assert_eq!(input_width(28.0), MIN_INPUT_WIDTH, "28 px в строке — это худший случай");
        assert_eq!(input_width(110.0), MIN_INPUT_WIDTH);
        assert_eq!(input_width(710.0), 600.0, "на просторной строке поле должно расти");
        for available in (0..=1000).step_by(7) {
            let w = input_width(available as f32);
            assert!(w >= MIN_INPUT_WIDTH, "ширина {available} дала {w}");
        }
    }

    /// A mouse-stretched channel panel must still leave the input field visible.
    #[test]
    fn stretched_channel_panel_leaves_the_input_visible() {
        let (mut app, ctx) = narrow_app();

        // One frame records panel state, then drag to the widest the user can reach.
        let _ = ctx.run(frame(), |ctx| {
            app.draw_server_list(ctx);
            app.draw_channel_list(ctx);
            app.draw_input_bar(ctx);
        });
        // Drag the panel's right edge right; edge = server rail (72) + panel width.
        let edge = 72.0 + 240.0;
        let y = 300.0;
        for step in 0..24 {
            let x = edge + (620.0 - edge) * (step as f32) / 24.0;
            let mut f = frame();
            f.events.push(egui::Event::PointerMoved(egui::Pos2::new(x, y)));
            if step == 0 {
                f.events.push(egui::Event::PointerButton {
                    pos: egui::Pos2::new(x, y),
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                });
            }
            if step == 23 {
                f.events.push(egui::Event::PointerButton {
                    pos: egui::Pos2::new(x, y),
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::default(),
                });
            }
            let _ = ctx.run(f, |ctx| {
                app.draw_server_list(ctx);
                app.draw_channel_list(ctx);
                app.draw_input_bar(ctx);
            });
        }

        let panel_w = app.channel_panel_w;
        assert!(
            panel_w > 300.0,
            "тест должен действительно растянуть панель мышью, а получилось {panel_w}"
        );
        assert!(
            panel_w <= 360.5,
            "панель каналов должна быть ограничена сверху, а не съедать окно: {panel_w}"
        );

        let line = app
            .debug_log
            .iter()
            .rev()
            .find(|l| l.starts_with("INPUT_FIELD:"))
            .expect("отладочная строка ширины поля");
        let w: f32 = line
            .split_whitespace()
            .find_map(|p| p.strip_prefix("w="))
            .expect("ширина в строке")
            .parse()
            .expect("число");
        assert!(
            w >= MIN_INPUT_WIDTH,
            "поле ввода схлопнулось при растянутой панели каналов: {w} px"
        );
    }

    /// Over-limit messages aren't sent and don't clear the field.
    #[test]
    fn over_limit_message_is_not_sent() {
        let (mut app, _ctx) = narrow_app();

        app.input = "я".repeat(MAX_MESSAGE_CHARS + 1);
        app.submit_input();
        assert!(
            app.messages.get("c1").is_none_or(|v| v.is_empty()),
            "сообщение длиннее лимита не должно уходить"
        );
        assert_eq!(
            app.input.chars().count(),
            MAX_MESSAGE_CHARS + 1,
            "заблокированная отправка не должна стирать набранное"
        );

        // Exactly at the limit is still allowed.
        app.input = "я".repeat(MAX_MESSAGE_CHARS);
        app.submit_input();
        assert_eq!(app.messages["c1"].len(), 1, "2000 символов должны отправляться");
        assert_eq!(app.input, "", "успешная отправка очищает поле");

        // The limit counts Unicode chars, not bytes.
        assert_eq!(
            "я".repeat(MAX_MESSAGE_CHARS).len(),
            MAX_MESSAGE_CHARS * 2,
            "тест должен проверять многобайтовый случай"
        );
    }

    /// The "0/2000" counter isn't drawn on an empty field.
    #[test]
    fn empty_input_has_no_character_counter() {
        let (mut app, ctx) = narrow_app();
        let out = ctx.run(frame(), |ctx| app.draw_input_bar(ctx));
        assert!(
            !has_counter_text(&out),
            "на пустом поле не должно быть счётчика символов"
        );
    }

    /// Once text is typed, the counter shows how much of the limit is used.
    #[test]
    fn typed_input_shows_character_counter() {
        let (mut app, ctx) = narrow_app();
        app.input = "привет".to_string();
        let out = ctx.run(frame(), |ctx| app.draw_input_bar(ctx));
        assert!(
            has_counter_text(&out),
            "при набранном тексте счётчик должен быть виден"
        );
    }

    /// Find "n/2000"-style text in the drawn frame.
    fn has_counter_text(out: &egui::FullOutput) -> bool {
        out.shapes.iter().any(|cs| match &cs.shape {
            egui::Shape::Text(t) => t.galley.text().contains("/2000"),
            _ => false,
        })
    }

    /// Setting a reply and sending must put the target into the outgoing command.
    #[test]
    fn reply_message_carries_message_reference_and_clears_state() {
        let (mut app, _ctx) = narrow_app();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        app.to_gw = Some(tx);
        app.reply_to = Some(ReplyTarget {
            message_id: "m42".into(),
            author_name: "Алиса".into(),
            preview: "исходное".into(),
        });
        app.input = "ответ".to_string();

        app.submit_input();

        match rx.try_recv() {
            Ok(ToGateway::Send {
                content, reply_to, ..
            }) => {
                assert_eq!(content, "ответ");
                assert_eq!(
                    reply_to.as_deref(),
                    Some("m42"),
                    "reply-сообщение должно ссылаться на исходное"
                );
            }
            other => panic!("ожидалась отправка, получено {other:?}"),
        }
        assert!(
            app.reply_to.is_none(),
            "после отправки reply должен сброситься"
        );
    }

    /// A plain message must not grow a `message_reference`.
    #[test]
    fn plain_message_is_sent_without_reply() {
        let (mut app, _ctx) = narrow_app();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        app.to_gw = Some(tx);
        app.input = "привет".to_string();

        app.submit_input();

        match rx.try_recv() {
            Ok(ToGateway::Send {
                content, reply_to, ..
            }) => {
                assert_eq!(content, "привет");
                assert!(
                    reply_to.is_none(),
                    "обычная отправка без reply: {reply_to:?}"
                );
            }
            other => panic!("ожидалась отправка, получено {other:?}"),
        }
    }

    /// Cancelling a reply returns the composer to its normal state.
    #[test]
    fn cancelling_reply_clears_the_state() {
        let (mut app, _ctx) = narrow_app();
        app.reply_to = Some(ReplyTarget {
            message_id: "m42".into(),
            author_name: "Алиса".into(),
            preview: "исходное".into(),
        });

        app.cancel_reply();

        assert!(app.reply_to.is_none(), "отмена должна убрать reply");
    }

    /// The reply bar names the author, shows the source text and offers cancel.
    #[test]
    fn reply_bar_shows_the_replied_message() {
        let (mut app, ctx) = narrow_app();
        app.reply_to = Some(ReplyTarget {
            message_id: "m42".into(),
            author_name: "Алиса".into(),
            preview: "исходный текст".into(),
        });

        let out = ctx.run(frame(), |ctx| app.draw_input_bar(ctx));

        assert!(frame_has_text(&out, "Алиса"), "в строке ответа нет автора");
        assert!(
            frame_has_text(&out, "исходный текст"),
            "в строке ответа нет текста"
        );
        assert!(frame_has_text(&out, "Отмена"), "нет кнопки отмены reply");
    }

    /// Find a text fragment in the drawn frame.
    fn frame_has_text(out: &egui::FullOutput, needle: &str) -> bool {
        out.shapes.iter().any(|cs| match &cs.shape {
            egui::Shape::Text(t) => t.galley.text().contains(needle),
            _ => false,
        })
    }
}
