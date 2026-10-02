use eframe::egui::{self, Color32, RichText};

mod attachments;
mod chat;
mod input;
mod login;
mod sidebar;

/// Цвет, которым рисуют то, что пошло не так: неудачная отправка, отказ
/// Discord. Жил в login.rs приватной константой, поэтому в чате — единственном
/// месте, где неудача реально случается, — показать её было нечем.
pub(crate) const ERROR_RED: Color32 = Color32::from_rgb(250, 77, 77);

impl crate::app::App {
    /// Полоса «хранилище старого формата» вверху окна.
    ///
    /// В старом открытом файле пароля нет вовсе, поэтому подходит любой ввод, и
    /// раньше первая же запись перешифровывала файл этим (случайным) паролем.
    /// Теперь запись ждёт подтверждения, а подтверждение надо где-то нажать —
    /// экран входа после входа исчезает, значит полоса нужна в чате.
    pub(crate) fn draw_legacy_vault_banner(&mut self, ctx: &egui::Context) {
        if !self.vault_legacy {
            return;
        }
        egui::TopBottomPanel::top("vault_legacy")
            .frame(egui::Frame::new().fill(self.theme.panel_bg).inner_margin(8))
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(
                            "Хранилище старого формата: пароля в нём нет. Пароль, который ты ввёл, \
                             закрепится за хранилищем только по кнопке — до этого файл не \
                             перезаписывается.",
                        )
                        .size(12.0)
                        .color(self.theme.text),
                    );
                    if ui
                        .button(RichText::new("Закрепить пароль").size(12.0).color(Color32::WHITE))
                        .clicked()
                    {
                        self.confirm_legacy_migration();
                    }
                });
            });
    }
}
