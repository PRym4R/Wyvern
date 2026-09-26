use eframe::egui::{self, Color32, RichText};

use crate::app::App;

const CARD_WIDTH: f32 = 400.0;
const ERROR_RED: Color32 = Color32::from_rgb(250, 77, 77);

fn initial_of(name: &str) -> String {
    name.chars().next().unwrap_or('?').to_ascii_uppercase().to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

impl App {
    pub(crate) fn draw_login(&mut self, ctx: &egui::Context) {
        let mut style = (*ctx.style()).clone();
        style.visuals.panel_fill = self.theme.channel_bg;
        style.visuals.widgets.inactive.bg_fill = self.theme.input_bg;
        style.visuals.widgets.hovered.bg_fill = self.theme.message_hover;
        style.visuals.widgets.active.bg_fill = self.theme.message_hover;
        style.visuals.widgets.open.bg_fill = self.theme.input_bg;
        style.visuals.selection.bg_fill = self.theme.accent.gamma_multiply(0.35);
        ctx.set_style(style);

        self.draw_account_strip(ctx);

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(self.theme.channel_bg))
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(48.0);
                    ui.heading(RichText::new("Wyvern").size(34.0).color(self.theme.accent));
                    ui.label(RichText::new("Rust-powered, lightweight")
                        .size(13.0).color(self.theme.text_secondary));
                    ui.add_space(20.0);

                    egui::Frame::new()
                        .fill(self.theme.panel_bg)
                        .corner_radius(14.0)
                        .inner_margin(egui::Margin::same(18))
                        .stroke(egui::Stroke::new(1.0_f32, self.theme.divider))
                        .show(ui, |ui| {
                            ui.set_min_width(CARD_WIDTH);
                            if self.login_selected.is_some() {
                                self.login_account_form(ui);
                            } else {
                                self.login_token_form(ui);
                            }
                        });

                    ui.add_space(12.0);
                    self.login_footer(ui);
                });
            });
    }

    /// Лента сохранённых аккаунтов внизу слева.
    /// ЛКМ — выбрать аккаунт (форма спросит пароль), ✕ — удалить из хранилища.
    fn draw_account_strip(&mut self, ctx: &egui::Context) {
        if self.saved_accounts.is_empty() {
            return;
        }
        let theme = self.theme.clone();
        let mut pick: Option<String> = None;
        let mut remove: Option<usize> = None;

        egui::TopBottomPanel::bottom("login_accounts")
            .resizable(false)
            .show_separator_line(false)
            .exact_height(56.0)
            .frame(egui::Frame::new()
                .fill(theme.panel_bg)
                .inner_margin(egui::Margin::symmetric(14, 10))
                .corner_radius(12.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("АККАУНТЫ").size(10.0).color(theme.text_secondary));
                    egui::ScrollArea::horizontal()
                        .id_salt("login_accounts_scroll")
                        .auto_shrink([false, false])
                        .max_height(34.0)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                for (i, acc) in self.saved_accounts.iter().enumerate() {
                                    let name = truncate(&self.account_label(acc), 18);
                                    let selected = self.login_selected.as_deref() == Some(acc.token.as_str());
                                    let is_active = self.active_index == Some(i);
                                    let highlighted = selected || is_active;
                                    let fill = if selected {
                                        theme.accent.gamma_multiply(0.22)
                                    } else {
                                        theme.input_bg
                                    };
                                    let border = if selected {
                                        theme.accent
                                    } else if is_active {
                                        theme.accent.gamma_multiply(0.45)
                                    } else {
                                        theme.divider
                                    };

                                    let chip = egui::Frame::new()
                                        .fill(fill)
                                        .corner_radius(10.0)
                                        .inner_margin(egui::Margin::symmetric(8, 4))
                                        .stroke(egui::Stroke::new(1.0_f32, border))
                                        .show(ui, |ui| {
                                            ui.spacing_mut().item_spacing = egui::vec2(8.0, 0.0);
                                            let (ar, ar_resp) = ui.allocate_exact_size(
                                                egui::vec2(22.0, 22.0),
                                                egui::Sense::click(),
                                            );
                                            ui.painter().circle_filled(
                                                ar.center(),
                                                11.0,
                                                if highlighted { theme.accent } else { theme.divider },
                                            );
                                            ui.painter().text(
                                                ar.center(),
                                                egui::Align2::CENTER_CENTER,
                                                initial_of(&name),
                                                egui::FontId::proportional(12.0),
                                                if highlighted { Color32::BLACK } else { theme.text },
                                            );
                                            let name_resp = ui.add(
                                                egui::Button::new(
                                                    RichText::new(&name).size(13.0).color(theme.text),
                                                )
                                                .fill(Color32::TRANSPARENT)
                                                .stroke(egui::Stroke::NONE)
                                                .min_size(egui::vec2(0.0, 22.0)),
                                            );
                                            (ar_resp, name_resp)
                                        });

                                    let (ar_resp, name_resp) = chip.inner;
                                    if ar_resp.hovered() || name_resp.hovered() {
                                        ui.painter().rect_stroke(
                                            chip.response.rect,
                                            10.0,
                                            egui::Stroke::new(1.0_f32, theme.accent),
                                            egui::StrokeKind::Inside,
                                        );                                    }
                                    if ar_resp.clicked() || name_resp.clicked() {
                                        pick = Some(acc.token.clone());
                                    }
                                    name_resp.on_hover_text(self.mask_token(&acc.token));

                                    let x = ui.add_sized(
                                        [20.0, 22.0],
                                        egui::Button::new(
                                            RichText::new("✕").size(11.0).color(theme.text_secondary),
                                        )
                                        .fill(Color32::TRANSPARENT)
                                        .stroke(egui::Stroke::NONE)
                                        .corner_radius(6.0),
                                    );
                                    if x.clicked() {
                                        remove = Some(i);
                                    }
                                    ui.add_space(4.0);
                                }
                            });
                        });
                });
            });

        if let Some(token) = pick {
            self.select_account(token);
        }
        if let Some(i) = remove {
            let gone = self.saved_accounts.get(i).map(|a| a.token.clone());
            self.saved_accounts.remove(i);
            if let Some(t) = gone {
                if self.login_selected.as_deref() == Some(t.as_str()) {
                    self.clear_login_selection();
                }
            }
            self.refresh_active_index();
            self.save_accounts(&self.master_password);
        }
    }

    /// Форма входа по свежему токену: токен и пароль хранилища рядом,
    /// галка "запомнить" — под ними.
    fn login_token_form(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("НОВЫЙ ВХОД").size(10.0).color(self.theme.text_secondary));
        ui.add_space(10.0);

        let field = (ui.available_width() - 10.0) / 2.0;
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.set_width(field);
                ui.label(RichText::new("Токен").size(12.0).color(self.theme.text_secondary));
                ui.add(
                    egui::TextEdit::singleline(&mut self.token_input)
                        .password(true)
                        .desired_width(f32::INFINITY)
                        .hint_text("Вставь токен...")
                        .font(egui::FontId::proportional(15.0))
                        .margin(egui::Margin::symmetric(10, 8)),
                );
            });
            ui.add_space(10.0);
            ui.vertical(|ui| {
                ui.set_width(field);
                ui.label(RichText::new("Пароль хранилища").size(12.0).color(self.theme.text_secondary));
                ui.add(
                    egui::TextEdit::singleline(&mut self.login_password)
                        .password(true)
                        .desired_width(f32::INFINITY)
                        .hint_text("Локальный пароль...")
                        .font(egui::FontId::proportional(15.0))
                        .margin(egui::Margin::symmetric(10, 8)),
                );
                ui.label(
                    RichText::new("только для файла аккаунтов, Discord его не видит")
                        .size(10.0)
                        .color(self.theme.text_secondary),
                );
            });
        });
        ui.add_space(10.0);

        ui.checkbox(
            &mut self.remember_account,
            RichText::new("Запомнить этот аккаунт").size(13.0).color(self.theme.text),
        );
        ui.add_space(16.0);

        let has_token = !self.token_input.trim().is_empty();
        let (label, enabled) = if has_token {
            ("Войти", true)
        } else {
            ("Показать сохранённые аккаунты", !self.login_password.is_empty())
        };
        let enter = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
        let btn = ui.add_sized(
            [ui.available_width(), 40.0],
            egui::Button::new(
                RichText::new(label)
                    .size(15.0)
                    .color(if enabled { Color32::BLACK } else { self.theme.text_secondary }),
            )
            .fill(if enabled { self.theme.accent } else { self.theme.input_bg })
            .corner_radius(8.0),
        );
        if (btn.clicked() || enter) && enabled {
            if has_token {
                self.login_with_token();
            } else {
                self.unlock_from_form();
            }
        }
    }

    /// Форма входа в уже сохранённый аккаунт: нужен только пароль хранилища.
    fn login_account_form(&mut self, ui: &mut egui::Ui) {
        let Some(token) = self.login_selected.clone() else { return };
        let name = self
            .saved_accounts
            .iter()
            .find(|a| a.token == token)
            .map(|a| self.account_label(a))
            .unwrap_or_else(|| self.mask_token(&token));

        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(36.0, 36.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 18.0, self.theme.accent);
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                initial_of(&name),
                egui::FontId::proportional(16.0),
                Color32::BLACK,
            );
            ui.vertical(|ui| {
                ui.label(RichText::new(&name).size(17.0).color(self.theme.text));
                ui.label(
                    RichText::new(format!("Токен {}", self.mask_token(&token)))
                        .size(11.0)
                        .color(self.theme.text_secondary),
                );
            });
        });
        ui.add_space(16.0);

        ui.label(RichText::new("Пароль хранилища").size(12.0).color(self.theme.text_secondary));
        ui.add(
            egui::TextEdit::singleline(&mut self.login_password)
                .password(true)
                .desired_width(f32::INFINITY)
                .hint_text("Пароль от ~/.wyvern_accounts.json...")
                .font(egui::FontId::proportional(15.0))
                .margin(egui::Margin::symmetric(10, 8)),
        );
        ui.add_space(16.0);

        let enter = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
        let (back_clicked, go_clicked) = ui.horizontal(|ui| {
            let back = ui.add_sized(
                [96.0, 40.0],
                egui::Button::new(RichText::new("Назад").size(14.0).color(self.theme.text))
                    .fill(self.theme.input_bg)
                    .corner_radius(8.0),
            );
            let go = ui.add_sized(
                [ui.available_width(), 40.0],
                egui::Button::new(RichText::new("Войти").size(15.0).color(Color32::BLACK))
                    .fill(self.theme.accent)
                    .corner_radius(8.0),
            );
            (back.clicked(), go.clicked())
        })
        .inner;

        if back_clicked {
            self.clear_login_selection();
        } else if go_clicked || enter {
            self.login_with_password(&token);
        }
    }

    fn login_footer(&self, ui: &mut egui::Ui) {
        if !self.status.is_empty() {
            ui.label(RichText::new(&self.status).size(13.0).color(ERROR_RED));
        } else if let Some(last) = self.debug_log.last() {
            ui.label(RichText::new(format!("Last: {}", last))
                .size(11.0)
                .color(self.theme.text_secondary));
        }
        if self.saved_accounts.is_empty() {
            ui.label(
                RichText::new("Пароль хранилища открывает список аккаунтов внизу слева")
                    .size(11.0)
                    .color(self.theme.text_secondary),
            );
        }
    }

    fn unlock_from_form(&mut self) {
        let pw = self.login_password.clone();
        match self.unlock_vault(&pw) {
            Ok(()) => {
                self.status = if self.saved_accounts.is_empty() {
                    "Хранилище открыто, но аккаунтов в нём пока нет".to_string()
                } else {
                    String::new()
                }
            }
            Err(e) => self.status = e.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::StoredAccount;

    /// Позиции всех нарисованных строк текста: (текст, x, y).
    fn texts(app: &mut App) -> Vec<(String, f32, f32)> {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 900.0),
            )),
            ..Default::default()
        };
        let out = ctx.run(raw, |ctx| app.draw_login(ctx));
        let mut found = Vec::new();
        for cs in out.shapes {
            if let egui::Shape::Text(t) = cs.shape {
                found.push((t.galley.text().to_string(), t.pos.x, t.pos.y));
            }
        }
        found
    }

    fn at(list: &[(String, f32, f32)], needle: &str) -> Vec<(f32, f32)> {
        list.iter()
            .filter(|(t, _, _)| t.contains(needle))
            .map(|(_, x, y)| (*x, *y))
            .collect()
    }

    fn make_app() -> App {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(rx);
        app.status = "статус".into();
        app
    }

    /// Токен и пароль хранилища — в одну строку, галка «запомнить» под ними.
    #[test]
    fn token_and_password_are_in_one_row() {
        let mut app = make_app();
        let list = texts(&mut app);

        let token = at(&list, "Вставь токен...");
        let pass = at(&list, "Локальный пароль...");
        assert_eq!(token.len(), 1, "поле токена не найдено: {:?}", list);
        assert_eq!(pass.len(), 1, "поле пароля не найдено: {:?}", list);
        let (tx, ty) = token[0];
        let (px, py) = pass[0];
        eprintln!("[TEST] токен x={tx:.0} y={ty:.0} | пароль x={px:.0} y={py:.0} | галка y={:.0}", at(&list, "Запомнить этот аккаунт")[0].1);
        assert!((ty - py).abs() < 0.5, "поля должны быть в одной строке: y {} vs {}", ty, py);
        assert!(px > tx + 100.0, "пароль должен быть справа от токена: x {} vs {}", px, tx);

        // Подписи полей — тоже в одной строке.
        let lbl_t = at(&list, "Токен");
        let lbl_p = at(&list, "Пароль хранилища");
        assert!((lbl_t[0].1 - lbl_p[0].1).abs() < 0.5, "подписи полей не в одной строке");

        // Галка «запомнить» — ниже обоих полей.
        let remember = at(&list, "Запомнить этот аккаунт");
        assert_eq!(remember.len(), 1, "галка не найдена");
        assert!(
            remember[0].1 > ty + 20.0,
            "галка должна быть под полями: y {} vs поля y {}",
            remember[0].1,
            ty
        );
    }

    /// Надпись «Показать сохранённые аккаунты» должна быть ровно одна.
    #[test]
    fn saved_accounts_caption_is_not_duplicated() {
        let mut app = make_app();
        let list = texts(&mut app);
        assert_eq!(
            at(&list, "Показать сохранённые аккаунты").len(),
            1,
            "надпись должна быть ровно один раз"
        );

        // Если токен введён — кнопка становится «Войти», лишней надписи нет.
        app.token_input = "MTIz.token.value".into();
        let list = texts(&mut app);
        assert!(at(&list, "Показать сохранённые аккаунты").is_empty());
        assert_eq!(at(&list, "Войти").len(), 1);
    }

    /// Сохранённые аккаунты рисуются полосой снизу, и по клику выбирается аккаунт.
    #[test]
    fn account_strip_lists_accounts() {
        let mut app = make_app();
        app.saved_accounts = vec![
            StoredAccount { token: "tok-one".into(), username: "alice".into() },
            StoredAccount { token: "tok-two".into(), username: String::new() },
        ];
        let list = texts(&mut app);
        assert_eq!(at(&list, "АККАУНТЫ").len(), 1, "полоса аккаунтов не нарисована");
        assert_eq!(at(&list, "alice").len(), 1);
        // Без имени показывается маска токена.
        assert_eq!(at(&list, "••••").len(), 1, "аккаунт без имени не показан: {:?}", list);
    }
}
