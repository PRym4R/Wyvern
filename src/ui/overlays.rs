use std::time::Instant;

use eframe::egui::{self, Color32, RichText};

use crate::app::{ToastKind, TOAST_TTL};
use crate::ui::ERROR_RED;

impl crate::app::App {
    /// Corner notices stacked in the top-right, above both the login screen and
    /// the chat. Non-interactive: they never steal a click from the UI below.
    pub(crate) fn draw_toasts(&mut self, ctx: &egui::Context) {
        if self.toasts.is_empty() {
            return;
        }
        let now = Instant::now();
        let ttl = TOAST_TTL.as_secs_f32();
        let text_color = self.theme.text;
        let panel = self.theme.panel_bg;
        let info_accent = self.theme.accent;
        egui::Area::new(egui::Id::new("toasts"))
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, 12.0))
            .interactable(false)
            .show(ctx, |ui| {
                for toast in &self.toasts {
                    let age = now.saturating_duration_since(toast.born).as_secs_f32();
                    let frac = (age / ttl).clamp(0.0, 1.0);
                    // Hold full opacity, then fade over the final quarter.
                    let alpha = if frac > 0.75 {
                        (1.0 - (frac - 0.75) / 0.25).clamp(0.0, 1.0)
                    } else {
                        1.0
                    };
                    let accent = match toast.kind {
                        ToastKind::Info => info_accent,
                        ToastKind::Warn => Color32::from_rgb(240, 180, 60),
                        ToastKind::Error => ERROR_RED,
                    };
                    egui::Frame::new()
                        .fill(panel.gamma_multiply(alpha))
                        .stroke(egui::Stroke::new(1.0_f32, accent.gamma_multiply(alpha)))
                        .corner_radius(6.0)
                        .inner_margin(egui::Margin::symmetric(12, 8))
                        .show(ui, |ui| {
                            ui.set_max_width(320.0);
                            ui.label(
                                RichText::new(&toast.text)
                                    .size(13.0)
                                    .color(text_color.gamma_multiply(alpha)),
                            );
                        });
                    ui.add_space(6.0);
                }
            });
    }

    /// Modal gate before an irreversible action. Esc or a backdrop click cancel;
    /// the buttons either confirm or cancel.
    pub(crate) fn draw_confirm(&mut self, ctx: &egui::Context) {
        let (title, body, label) = match self.confirm.as_ref() {
            Some(d) => (d.title.clone(), d.body.clone(), d.confirm_label.clone()),
            None => return,
        };
        let text_color = self.theme.text;
        let secondary = self.theme.text_secondary;
        let mut confirmed = false;
        let mut cancelled = false;
        let resp = egui::Modal::new(egui::Id::new("confirm_modal")).show(ctx, |ui| {
            ui.set_max_width(340.0);
            ui.label(RichText::new(title).strong().size(15.0).color(text_color));
            ui.add_space(6.0);
            ui.label(RichText::new(body).size(13.0).color(secondary));
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    cancelled = true;
                }
                if ui
                    .add(egui::Button::new(RichText::new(label).color(Color32::WHITE)))
                    .clicked()
                {
                    confirmed = true;
                }
            });
        });
        if resp.should_close() {
            cancelled = true;
        }
        if confirmed {
            if let Some(dialog) = self.confirm.take() {
                self.run_confirm(dialog.action);
            }
        } else if cancelled {
            self.confirm = None;
        }
    }
}
