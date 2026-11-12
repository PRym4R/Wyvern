//! Ctrl+F overlay: search over the messages already loaded in the channel.
//!
//! The REST search across the whole guild is a later step; this one is instant
//! and works offline, which covers the common "where was that message" case.

use eframe::egui::{self, Color32, FontId, TextFormat};

use crate::search;

/// Window width: wide enough for a snippet line, narrow enough to leave the
/// chat visible around it.
const WINDOW_W: f32 = 440.0;
/// Result list height before it scrolls.
const LIST_H: f32 = 300.0;

impl crate::app::App {
    /// Opens the search window and asks for keyboard focus next frame.
    pub(crate) fn open_search(&mut self) {
        self.search_open = true;
        self.search_focus = true;
    }

    /// Closes the window and forgets the tint on the message it jumped to.
    pub(crate) fn close_search(&mut self) {
        self.search_open = false;
        self.search_query.clear();
        self.search_hits.clear();
        self.search_index = 0;
        self.search_key = (String::new(), String::new(), 0);
        self.search_highlight = None;
    }

    /// Recomputes the hits when the channel, the query or the message count
    /// changed. The key keeps an idle frame from re-scanning the list, and new
    /// messages show up in the results while the window is open.
    pub(crate) fn refresh_search(&mut self) {
        let channel = self.current_channel_id().unwrap_or_default().to_string();
        let count = self
            .messages
            .get(&channel)
            .map(|msgs| msgs.len())
            .unwrap_or(0);
        if self.search_key.0 == channel
            && self.search_key.1 == self.search_query
            && self.search_key.2 == count
        {
            return;
        }
        self.search_hits = self
            .messages
            .get(&channel)
            .map(|msgs| search::hits(msgs, &self.search_query))
            .unwrap_or_default();
        self.search_key = (channel, self.search_query.clone(), count);
        self.search_index = 0;
    }

    /// Moves the view to a hit: its row is anchored to the top of the viewport
    /// and tinted until the search closes.
    pub(crate) fn jump_to_search_hit(&mut self, index: usize) {
        let Some(hit) = self.search_hits.get(index) else {
            return;
        };
        let id = hit.message_id.clone();
        self.search_index = index;
        self.search_highlight = Some(id.clone());
        // An explicit move: holding the bottom would override the anchor.
        self.scroll_to_bottom = false;
        self.chat_at_bottom = false;
        self.chat_anchor = Some((id, 0.0));
    }

    /// The window itself: a query field, a hit list and the keyboard that ties
    /// them together (Ctrl+F toggles, Up/Down select, Enter jumps, Esc closes).
    pub(crate) fn draw_search(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F)) {
            if self.search_open {
                self.close_search();
            } else {
                self.open_search();
            }
        }
        if !self.search_open {
            return;
        }
        let mut open = true;
        let mut jump: Option<usize> = None;
        let mut moved = false;
        egui::Window::new("Search")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(WINDOW_W)
            // Open over the chat, clear of the channel list on the left.
            .default_pos(egui::pos2(
                (ctx.screen_rect().right() - WINDOW_W - 24.0).max(24.0),
                72.0,
            ))
            .show(ctx, |ui| {
                let field = ui.add(
                    egui::TextEdit::singleline(&mut self.search_query)
                        .id(egui::Id::new("search_query"))
                        .hint_text("Search this channel")
                        .desired_width(f32::INFINITY),
                );
                // Focus once on open; re-requesting it every frame would eat
                // clicks on the result list.
                if std::mem::take(&mut self.search_focus) {
                    field.request_focus();
                }
                // After the edit, so the list matches what was just typed
                // instead of lagging a frame behind.
                self.refresh_search();

                let (up, down, enter) = ui.input(|i| {
                    (
                        i.key_pressed(egui::Key::ArrowUp),
                        i.key_pressed(egui::Key::ArrowDown),
                        i.key_pressed(egui::Key::Enter),
                    )
                });
                if !self.search_hits.is_empty() {
                    let last = self.search_hits.len() - 1;
                    if down {
                        self.search_index = (self.search_index + 1).min(last);
                        moved = true;
                    }
                    if up {
                        self.search_index = self.search_index.saturating_sub(1);
                        moved = true;
                    }
                    if enter {
                        jump = Some(self.search_index);
                    }
                }

                let count = self.search_hits.len();
                let status = if self.search_query.trim().is_empty() {
                    "Type to search the messages loaded here".to_string()
                } else {
                    match count {
                        0 => "Nothing found".to_string(),
                        1 => "1 match".to_string(),
                        n => format!("{n} matches"),
                    }
                };
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(status)
                            .size(12.0)
                            .color(self.theme.text_secondary),
                    );
                });

                let text = self.theme.text;
                let secondary = self.theme.text_secondary;
                let accent = self.theme.accent;
                egui::ScrollArea::vertical()
                    .max_height(LIST_H)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Inside the list, so the scrollbar width is already
                        // taken off the line length.
                        let width = (ui.available_width() - 8.0).max(120.0);
                        #[cfg(test)]
                        self.probe_search_rows.clear();
                        for (i, hit) in self.search_hits.iter().enumerate() {
                            // One job, three colours: the author line, the
                            // context and the hit itself in the accent colour.
                            let small = TextFormat {
                                font_id: FontId::proportional(11.0),
                                color: secondary,
                                ..Default::default()
                            };
                            let body = TextFormat {
                                font_id: FontId::proportional(13.0),
                                color: text,
                                ..Default::default()
                            };
                            let marked = TextFormat {
                                font_id: FontId::proportional(13.0),
                                color: Color32::WHITE,
                                background: accent,
                                ..Default::default()
                            };
                            let mut job = egui::text::LayoutJob::default();
                            job.wrap.max_width = width;
                            job.wrap.max_rows = 3;
                            job.append(&hit.author, 0.0, small);
                            job.append("\n", 0.0, TextFormat::default());
                            job.append(&hit.snippet.before, 0.0, body.clone());
                            job.append(&hit.snippet.hit, 0.0, marked);
                            job.append(&hit.snippet.after, 0.0, body);
                            let selected = i == self.search_index;
                            let row = ui.add(
                                egui::Button::new(job)
                                    .selected(selected)
                                    .min_size(egui::vec2(width, 0.0)),
                            );
                            #[cfg(test)]
                            self.probe_search_rows.push((row.rect, hit.message_id.clone()));
                            if row.clicked() {
                                jump = Some(i);
                            }
                            // Keyboard selection can walk past the visible
                            // rows; follow it.
                            if selected && moved {
                                row.scroll_to_me(Some(egui::Align::Center));
                            }
                        }
                    });
            });
        // Escape closes the overlay even while the field has focus; the
        // composer skips its own Escape handling while this window is open.
        if !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.close_search();
            return;
        }
        if let Some(index) = jump {
            self.jump_to_search_hit(index);
        }
    }
}
