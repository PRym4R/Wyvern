use eframe::egui::{self, Color32, RichText};

mod attachments;
mod chat;
mod input;
mod login;
mod sidebar;

/// Color for failures: failed sends and Discord errors.
pub(crate) const ERROR_RED: Color32 = Color32::from_rgb(250, 77, 77);

impl crate::app::App {
    /// "Legacy vault" banner at the top of the window.
    ///
    /// The login screen is gone after sign-in, so confirmation lives here.
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
