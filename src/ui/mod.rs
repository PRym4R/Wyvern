use eframe::egui::Color32;

mod attachments;
mod chat;
mod input;
mod login;
mod sidebar;

/// Цвет, которым рисуют то, что пошло не так: неудачная отправка, отказ
/// Discord. Жил в login.rs приватной константой, поэтому в чате — единственном
/// месте, где неудача реально случается, — показать её было нечем.
pub(crate) const ERROR_RED: Color32 = Color32::from_rgb(250, 77, 77);
