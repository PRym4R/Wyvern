use std::time::Instant;

use eframe::egui::{self, Color32, RichText};

use crate::app::{ToastKind, TOAST_TTL};
use crate::ui::ERROR_RED;

/// Fade in quickly, hold, then fade out over the last quarter of the TTL.
fn toast_alpha(age: f32, ttl: f32) -> f32 {
    let frac = (age / ttl).clamp(0.0, 1.0);
    let fade_in = (age / 0.12).clamp(0.0, 1.0);
    let fade_out = if frac > 0.75 {
        (1.0 - (frac - 0.75) / 0.25).clamp(0.0, 1.0)
    } else {
        1.0
    };
    fade_in * fade_out
}

/// Top margin that slides a fresh toast up into place; zero once settled.
fn toast_enter_offset(age: f32) -> i8 {
    const ENTER_SECS: f32 = 0.18;
    let t = crate::ui::ease_out_cubic(age / ENTER_SECS);
    ((1.0 - t) * 14.0).round() as i8
}

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
                let animations = self.animations_enabled();
                for toast in &self.toasts {
                    let age = now.saturating_duration_since(toast.born).as_secs_f32();
                    let alpha = if animations { toast_alpha(age, ttl) } else { 1.0 };
                    // The slide-in is short; the coarse busy-repaint is too slow
                    // for it, so keep frames coming while the toast is entering.
                    if animations && age < 0.25 {
                        ctx.request_repaint();
                    }
                    let accent = match toast.kind {
                        ToastKind::Info => info_accent,
                        ToastKind::Warn => Color32::from_rgb(240, 180, 60),
                        ToastKind::Error => ERROR_RED,
                    };
                    egui::Frame::new()
                        .outer_margin(egui::Margin {
                            left: 0,
                            right: 0,
                            top: if animations { toast_enter_offset(age) } else { 0 },
                            bottom: 0,
                        })
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

#[cfg(test)]
mod tests {
    use super::{toast_alpha, toast_enter_offset};

    #[test]
    fn toast_fades_in_then_out() {
        assert_eq!(toast_alpha(0.0, 4.0), 0.0, "тост должен появляться плавно");
        assert_eq!(toast_alpha(0.2, 4.0), 1.0);
        assert!(toast_alpha(4.0, 4.0) < 0.01, "к концу тост должен погаснуть");
    }

    #[test]
    fn toast_slides_up_then_settles() {
        assert!(toast_enter_offset(0.0) > 0, "свежий тост стартует ниже");
        assert_eq!(toast_enter_offset(0.2), 0, "после входа отступ должен исчезнуть");
    }
}
