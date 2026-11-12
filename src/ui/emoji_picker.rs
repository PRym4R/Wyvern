use eframe::egui::{self, RichText};

use crate::app::EmojiTarget;
use crate::emoji;

/// Cells per row; eight fits the window width without horizontal scrolling.
const COLUMNS: usize = 8;
/// One cell; the glyph is drawn at 18 px inside it.
const CELL: f32 = 30.0;
/// Height of the result grid before it scrolls.
const GRID_H: f32 = 210.0;

impl crate::app::App {
    /// Opens the picker for `target` with an empty query.
    pub(crate) fn open_emoji_picker(&mut self, target: EmojiTarget) {
        self.emoji_picker = Some((target, String::new()));
        self.emoji_picker_focus = true;
    }

    /// Searchable emoji window, shared by the composer button, the hover strip
    /// and the message menu: one picker, two outcomes.
    pub(crate) fn draw_emoji_picker(&mut self, ctx: &egui::Context) {
        let Some((target, mut query)) = self.emoji_picker.clone() else {
            return;
        };
        let mut open = true;
        let mut chosen: Option<&'static emoji::Emoji> = None;
        let search_id = egui::Id::new("emoji_search");
        egui::Window::new("Emoji")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(COLUMNS as f32 * (CELL + 6.0))
            .show(ctx, |ui| {
                let field = ui.add(
                    egui::TextEdit::singleline(&mut query)
                        .id(search_id)
                        .hint_text("Search by name or keyword")
                        .desired_width(f32::INFINITY),
                );
                // Focus once on open; re-requesting it every frame would eat
                // clicks on the grid below.
                if std::mem::take(&mut self.emoji_picker_focus) {
                    field.request_focus();
                }
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) && !query.is_empty() {
                    chosen = emoji::search(&query, 1).first().copied();
                }
                ui.add_space(4.0);
                let results = emoji::search(&query, emoji::MAX_RESULTS);
                if results.is_empty() {
                    ui.label(
                        RichText::new("Nothing matches")
                            .size(12.0)
                            .color(self.theme.text_secondary),
                    );
                }
                egui::ScrollArea::vertical()
                    .max_height(GRID_H)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        #[cfg(test)]
                        self.probe_emoji_cells.clear();
                        for row in results.chunks(COLUMNS) {
                            ui.horizontal(|ui| {
                                for emoji in row {
                                    let button = ui
                                        .add_sized(
                                            egui::vec2(CELL, CELL),
                                            egui::Button::new(RichText::new(emoji.ch).size(18.0)),
                                        )
                                        .on_hover_text(emoji.name);
                                    #[cfg(test)]
                                    self.probe_emoji_cells
                                        .push((button.rect, emoji.ch.to_string()));
                                    if button.clicked() {
                                        chosen = Some(emoji);
                                    }
                                }
                            });
                        }
                    });
            });
        let Some(emoji) = chosen else {
            if !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.emoji_picker = None;
            } else {
                self.emoji_picker = Some((target, query));
            }
            return;
        };
        match &target {
            EmojiTarget::Insert => {
                self.input.push_str(emoji.ch);
                self.input_dirty = true;
                ctx.memory_mut(|m| m.request_focus(egui::Id::new("message_input")));
            }
            // Deferred like every other reaction click: the message list lives
            // outside `self.messages` while it is drawn.
            EmojiTarget::React {
                channel_id,
                message_id,
            } => {
                self.reaction_click = Some((
                    channel_id.clone(),
                    message_id.clone(),
                    emoji.ch.to_string(),
                ));
            }
        }
        self.emoji_picker = None;
    }
}
