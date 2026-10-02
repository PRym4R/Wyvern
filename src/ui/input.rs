use eframe::egui::{self, Color32, RichText};

use crate::app::App;

use crate::messages::ToGateway;
use crate::models::{ChatChannel, ChatMessage, LOCAL_ID_PREFIX};
use crate::ui::ERROR_RED;

/// Префикс отладочных команд. Всё, что не начинается с него, — обычный текст
/// для канала, даже если похоже на команду.
///
/// Раньше отладочные команды висели прямо в поле сообщения: `/quit` закрывал
/// клиент (то есть пользователь, который хотел написать в канал «/quit»,
/// терял окно без предупреждения), а `/add <id>` молча создавал канал, которого
/// нет ни в одном списке и который тем не менее открывался.
const DEBUG_PREFIX: &str = "/debug ";
/// Сколько ждёт подтверждения, прежде чем команда забудется.
const DEBUG_CONFIRM_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

/// Уже́с самой узкой полосы под поле ввода. Меньше не полезно ни печатать, ни
/// читать, но вернуться к нулю поле тоже не должно.
const MIN_INPUT_WIDTH: f32 = 60.0;
/// Сколько места в строке ввода уходит на отступ, название канала, кнопку
/// «Send» и зазоры.
const INPUT_CHROME: f32 = 110.0;
/// Лимит Discord на длину сообщения в символах Unicode.
///
/// Именно символы, а не байты: 2000 кириллических букв весят 4000 байт, но
/// Discord их принимает. Считаем кодпоинты, как и он.
pub(crate) const MAX_MESSAGE_CHARS: usize = 2000;

/// Помещается ли текст в лимит Discord.
///
/// Сообщение длиннее лимита Discord отвергает кодом 400, а клиент потом
/// показывает его в чате как отправленное и не убирает — человек думает, что
/// всё в порядке. Поэтому проверяем ДО отправки.
pub(crate) fn within_message_limit(text: &str) -> bool {
    text.chars().count() <= MAX_MESSAGE_CHARS
}

/// Ширина поля ввода по свободному месту в строке.
///
/// Раньше здесь было голое `available_width() - 110`, и панель каналов могла
/// оставить строке 28 пикселей: ширина уходила в минус, egui в релизной
/// сборке тихо рисует такой прямоугольник пустым, и поле ввода просто
/// исчезало — печатать можно, не видно ничего. Панели каналов теперь ограничена
/// сверху, но и без неё отрицательной ширины быть не должно.
pub(crate) fn input_width(available: f32) -> f32 {
    (available - INPUT_CHROME).max(MIN_INPUT_WIDTH)
}

/// Обрезать пробелы и невидимые символы.
///
/// Обычный `trim()` не трогает неразрывный пробел, zero-width space и
/// byte-order mark: они не относятся к `White_Space` по Unicode. А человек,
/// у которого прилип невидимый символ с конца сообщения (буфер обмена,
/// расширение браузера, автозамена), отправляет «пустое» сообщение и потом
/// удивляется, откуда взялось сообщение из одного пробела.
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
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    ui.label(RichText::new(ch_name).strong().size(13.0).color(self.theme.text_secondary));
                    ui.add_space(8.0);

                    if !has_channel {
                        ui.label(RichText::new("← select a channel on the left").italics()
                            .size(12.0).color(self.theme.text_secondary));
                    } else {
                        // Счётчик «n/2000» стоит перед полем: свободная
                        // ширина для поля считается уже с учётом счётчика, и
                        // длинное число не вытолкнет кнопку Send за край.
                        let typed = trim_input(&self.input);
                        let count = typed.chars().count();
                        let fits = within_message_limit(typed);
                        ui.label(
                            RichText::new(format!("{count}/{MAX_MESSAGE_CHARS}"))
                                .size(12.0)
                                .color(if fits { self.theme.text_secondary } else { ERROR_RED }),
                        );
                        ui.add_space(6.0);
                        let field_w = input_width(ui.available_width());
                        self.push_debug(format!("INPUT_FIELD: w={:.0}", field_w));
                        let resp = ui.add_sized(
                            [field_w, 36.0],
                            egui::TextEdit::singleline(&mut self.input)
                                .hint_text("Type a message and press Enter, or click Send...")
                                .margin(egui::Margin::symmetric(12, 8)),
                        );
                        // Т-9: любая правка поля снимает защиту от повторной
                        // отправки того же текста зажатым Enter. Ставим это до
                        // проверки Enter, чтобы набор и Enter в одном кадре не
                        // блокировали друг друга.
                        if resp.changed() {
                            self.input_dirty = true;
                        }
                        // Отправка заблокирована, пока текст не влезает в
                        // лимит Discord: иначе он отвергнет сообщение кодом
                        // 400, а в чате останется «отправленное» навсегда.
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
        // Отладочные команды — только под своим префиксом и только с
        // подтверждением: добавлять канал, которого не видно в списках, одной
        // опечаткой нельзя.
        if let Some(rest) = text.strip_prefix(DEBUG_PREFIX) {
            self.handle_debug_command(rest.trim());
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

    /// Enter или кнопка «Send»: отправляем, если есть что, и очищаем поле.
    ///
    /// Очистка стоит вне проверки «есть ли что отправить» намеренно. Раньше
    /// она была внутри, и пробелы (или невидимые символы) оставались в поле
    /// навсегда: следующий настоящий Enter выглядел как «ничего не
    /// отправилось», хотя предыдущее сообщение ушло. Сбивает ровно в тот
    /// момент, когда человек проверяет, дошло ли сообщение.
    pub(crate) fn submit_input(&mut self) {
        let text = trim_input(&self.input).to_string();
        // Слишком длинное не отправляем и поле не чистим: Discord отверг бы
        // его кодом 400, а набранное потерялось бы. Пусть человек сократит.
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
            // Запоминаем, что ушло, и сбрасываем признак правки: повторная
            // отправка того же текста без правок теперь блокируется (Т-9).
            self.last_submitted = Some(text);
            self.input_dirty = false;
        }
        self.input.clear();
    }

    /// Enter: то же, что `submit_input`, но зажатая клавиша (автоповтор) не
    /// должна слать одно и то же повторно.
    ///
    /// Текст, вернувшийся в поле после неудачной отправки, при зажатом Enter
    /// уходил бы снова и снова: каждая попытка заканчивалась отказом, текст
    /// возвращался, и в лог Discord летела пачка одинаковых сообщений. Пока
    /// поле не изменили, повтор не отправляем — и НЕ чистим поле, иначе
    /// человек потерял бы восстановленный текст.
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

    /// Отладочная команда из поля сообщения. Первое нажатие только спрашивает
    /// подтверждение: команда добавляет канал, которого нет ни в одном списке,
    /// и ошибиться в id легко.
    fn handle_debug_command(&mut self, rest: &str) {
        if let Some(id) = rest.strip_prefix("add ").map(str::trim) {
            if id.is_empty() {
                self.status = "нужен id канала: /debug add <id>".to_string();
                return;
            }
            // Повтор той же команды в пределах окна — подтверждение.
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

    /// Добавить канал напрямую, мимо списков Discord. Такой канал виден только
    /// если знать его id, поэтому и нужен лишь для отладки.
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
    use crate::app::App;
    use crate::models::ChatChannel;

    /// Пробелы в поле не должны переживать отправку. Раньше очистка стояла
    /// внутри «есть ли что отправить», поэтому `trim()` давал пустую строку,
    /// очистки не происходило, и в поле навсегда оставалось «   ». Следующий
    /// настоящий Enter после этого выглядит как «ничего не отправилось» —
    /// а сообщение-то ушло.
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

    /// Обычный текст отправляется без краевых пробелов, а поле после этого
    /// пустое — иначе следующий Enter отправит пробелы вместо сообщения.
    #[test]
    fn submit_sends_trimmed_text_and_clears_the_field() {
        let (mut app, _ctx) = narrow_app();
        app.input = "  привет  ".to_string();

        app.submit_input();

        assert_eq!(app.input, "");
        assert_eq!(app.messages["c1"][0].content, "привет");
        // Сразу после отправки поле чистое: второй Enter ничего не отправит.
        let sent_before = app.messages["c1"].len();
        app.submit_input();
        assert_eq!(app.messages["c1"].len(), sent_before, "пустое поле не должно ничего слать");
    }

    /// Зажатый Enter не должен слать один и тот же текст пачками: после
    /// неудачной отправки текст возвращается в поле, и автоповтор отправлял бы
    /// его снова и снова. Пока поле не изменили, повтор игнорируется, а текст
    /// из поля при этом не пропадает.
    #[test]
    fn held_enter_does_not_resend_restored_text() {
        let (mut app, _ctx) = narrow_app();
        app.input = "привет".to_string();
        app.input_dirty = true; // пользователь набрал текст
        app.submit_from_enter();
        assert_eq!(app.messages["c1"].len(), 1, "первый Enter должен отправить");

        // Неудачная отправка вернула тот же текст, клавиша всё ещё зажата.
        app.input = "привет".to_string();
        app.input_dirty = false;
        app.submit_from_enter();
        assert_eq!(
            app.messages["c1"].len(),
            1,
            "повтор того же текста зажатым Enter не должен уйти второй раз"
        );
        assert_eq!(app.input, "привет", "заблокированный Enter не должен стирать поле");

        // Пользователь поправил текст — отправка снова работает.
        app.input = "привет!".to_string();
        app.input_dirty = true;
        app.submit_from_enter();
        assert_eq!(app.messages["c1"].len(), 2);
    }

    /// Приложение с открытым каналом в самом узком поддерживаемом окне.
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

    /// Ширина поля не должна уходить в минус ни при какой свободной ширине
    /// строки. Отрицательный прямоугольник egui рисует пустым, то есть поле
    /// исчезает, а панель каналов к тому же ещё и не вернуть обратно, пока не
    /// расширишь окно.
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

    /// Панель каналов растянута мышью на всё окно, как это делает пользователь:
    /// поле ввода обязано остаться видимым. Раньше верхней границы у панели не
    /// было, и на 700-пиксельном окне строка ввода получала 28 px, из которых
    /// кнопка с отступами забирала больше, чем оставалось.
    #[test]
    fn stretched_channel_panel_leaves_the_input_visible() {
        let (mut app, ctx) = narrow_app();

        // Один кадр, чтобы egui записал состояние панели, затем подкладываем
        // ему ту ширину, до которой пользователь может её растянуть.
        let _ = ctx.run(frame(), |ctx| {
            app.draw_server_list(ctx);
            app.draw_channel_list(ctx);
            app.draw_input_bar(ctx);
        });
        // Тянем правый край панели мышью вправо, как это делает пользователь.
        // Край панели — это рельс серверов (72) плюс её собственная ширина.
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

    /// Сообщение длиннее лимита Discord не должно уходить: Discord отвергнет
    /// его кодом 400, а клиент покажет текст как отправленный и не уберёт.
    /// Поле при этом не чистим, чтобы набранное можно было сократить.
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

        // Ровно лимит — это ещё можно.
        app.input = "я".repeat(MAX_MESSAGE_CHARS);
        app.submit_input();
        assert_eq!(app.messages["c1"].len(), 1, "2000 символов должны отправляться");
        assert_eq!(app.input, "", "успешная отправка очищает поле");

        // Лимит считается в символах Unicode, а не в байтах: 2000 кириллических
        // букв — это 4000 байт, но Discord их принимает.
        assert_eq!(
            "я".repeat(MAX_MESSAGE_CHARS).len(),
            MAX_MESSAGE_CHARS * 2,
            "тест должен проверять многобайтовый случай"
        );
    }
}
