use eframe::egui::{self, Color32, RichText};

mod attachments;
mod chat;
mod input;
mod login;
mod markdown;
mod settings_window;
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
                            "Legacy vault format: it has no password. The password you entered \
                             is pinned to the vault only when you press the button — until then \
                             the file is not rewritten.",
                        )
                        .size(12.0)
                        .color(self.theme.text),
                    );
                    if ui
                        .button(RichText::new("Pin password").size(12.0).color(Color32::WHITE))
                        .clicked()
                    {
                        self.confirm_legacy_migration();
                    }
                });
            });
    }
}
