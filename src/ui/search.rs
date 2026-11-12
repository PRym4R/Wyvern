//! Ctrl+F overlay: search the messages already loaded in the channel, and on
//! demand the whole guild through Discord's REST search.
//!
//! The local pass is instant and works offline, which covers most "where was
//! that message" cases; the guild pass costs a request and can find messages
//! that were never loaded, so it only runs when the button is pressed.

use eframe::egui::{self, Color32, FontId, TextFormat};

use crate::search;

/// Window width: wide enough for a snippet line, narrow enough to leave the
/// chat visible around it.
const WINDOW_W: f32 = 440.0;
/// Result list height before it scrolls.
const LIST_H: f32 = 320.0;
/// Failure text: no theme slot for errors, and a bare grey label reads as info.
const ERROR_COLOR: Color32 = Color32::from_rgb(240, 120, 120);

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
        self.search_remote.clear();
        self.search_remote_loading = false;
        self.search_remote_error = None;
        self.search_remote_query.clear();
    }

    /// Closes the window but keeps the tint: after a guild-wide jump the target
    /// is in another channel and should still stand out there.
    pub(crate) fn close_search_keep_highlight(&mut self) {
        let keep = self.search_highlight.clone();
        self.close_search();
        self.search_highlight = keep;
    }

    /// Recomputes the local hits when the channel, the query or the message
    /// count changed. The key keeps an idle frame from re-scanning the list, and
    /// new messages show up in the results while the window is open.
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
        // New text makes the old server answer meaningless; a request still in
        // flight will find `search_remote_query` cleared and drop itself.
        if self.search_key.1 != self.search_query {
            self.search_remote.clear();
            self.search_remote_error = None;
            self.search_remote_loading = false;
            self.search_remote_query.clear();
            self.search_index = 0;
        }
        self.search_hits = self
            .messages
            .get(&channel)
            .map(|msgs| search::hits(msgs, &self.search_query))
            .unwrap_or_default();
        self.search_key = (channel, self.search_query.clone(), count);
        self.search_index = 0;
    }

    /// Asks Discord for guild-wide matches. The answer arrives asynchronously
    /// as `ToApp::SearchResults` / `SearchFailed`.
    pub(crate) fn search_guild(&mut self) {
        let query = self.search_query.trim().to_string();
        if query.is_empty() {
            return;
        }
        let Some(channel_id) = self.current_channel_id() else { return };
        let Some(guild_id) = self
            .channels
            .iter()
            .find(|c| c.id == channel_id)
            .and_then(|c| c.guild_id.clone())
        else {
            return;
        };
        self.search_remote_query = query.clone();
        self.search_remote.clear();
        self.search_remote_error = None;
        self.search_remote_loading = true;
        self.send_cmd(crate::messages::ToGateway::SearchGuild { guild_id, query });
    }

    /// Moves the view to a guild-wide hit: an already loaded row is anchored in
    /// place, anything else opens its channel with `?around=`.
    pub(crate) fn open_search_result(&mut self, index: usize) {
        let Some(hit) = self.search_remote.get(index).cloned() else { return };
        let loaded_here = self.current_channel_id() == Some(hit.channel_id.as_str())
            && self
                .messages
                .get(&hit.channel_id)
                .map(|msgs| msgs.iter().any(|m| m.id == hit.message_id))
                .unwrap_or(false);
        if loaded_here {
            self.search_highlight = Some(hit.message_id.clone());
            self.scroll_to_bottom = false;
            self.chat_at_bottom = false;
            self.chat_anchor = Some((hit.message_id.clone(), 0.0));
            self.close_search_keep_highlight();
        } else {
            // The chat only shows the selected channel, so jump must move the
            // selection too (sidebar clicks do the same before opening).
            if let Some(i) = self.channels.iter().position(|c| c.id == hit.channel_id) {
                self.selected_channel = Some(i);
            }
            self.open_channel_with(&hit.channel_id, Some(&hit.message_id));
            self.close_search_keep_highlight();
        }
    }

    /// Moves the view to a local hit: its row is anchored to the top of the
    /// viewport and tinted until the search closes.
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

    /// One hit as a three-part job: author (grey), snippet, and the matched
    /// text highlighted. `origin` adds the channel line for server results.
    fn hit_job(
        hit: &search::Hit,
        width: f32,
        text: Color32,
        secondary: Color32,
        accent: Color32,
    ) -> egui::text::LayoutJob {
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
        if let Some(origin) = &hit.origin {
            job.append(origin, 0.0, small.clone());
            job.append("\n", 0.0, TextFormat::default());
        }
        job.append(&hit.author, 0.0, small);
        job.append("\n", 0.0, TextFormat::default());
        job.append(&hit.snippet.before, 0.0, body.clone());
        job.append(&hit.snippet.hit, 0.0, marked);
        job.append(&hit.snippet.after, 0.0, body);
        job
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
        #[cfg(test)]
        {
            // Re-set every frame: the button disappears in DMs and while a
            // search is running.
            self.probe_search_button = None;
        }
        let mut open = true;
        let mut jump: Option<usize> = None;
        let mut remote_jump: Option<usize> = None;
        let mut ask_guild = false;
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
                        .hint_text("Search this channel or the server")
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

                // The channel's guild: DMs have none, and then the button is
                // not offered at all.
                let in_guild = self
                    .current_channel_id()
                    .and_then(|cid| self.channels.iter().find(|c| c.id == cid))
                    .and_then(|c| c.guild_id.clone())
                    .is_some();
                let typed = !self.search_query.trim().is_empty();

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

                if in_guild {
                    ui.horizontal(|ui| {
                        let btn = ui.add_enabled(
                            typed && !self.search_remote_loading,
                            egui::Button::new("Search this server"),
                        );
                        #[cfg(test)]
                        {
                            self.probe_search_button = Some(btn.rect);
                        }
                        if btn.clicked() {
                            ask_guild = true;
                        }
                        if self.search_remote_loading {
                            ui.spinner();
                            ui.label(
                                egui::RichText::new("searching…")
                                    .size(12.0)
                                    .color(self.theme.text_secondary),
                            );
                        }
                    });
                }

                let count = self.search_hits.len();
                let status = if !typed {
                    "Type to search the messages loaded here".to_string()
                } else {
                    match count {
                        0 => "Nothing in this channel".to_string(),
                        1 => "1 match here".to_string(),
                        n => format!("{n} matches here"),
                    }
                };
                ui.label(
                    egui::RichText::new(status)
                        .size(12.0)
                        .color(self.theme.text_secondary),
                );

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
                            let job = Self::hit_job(hit, width, text, secondary, accent);
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

                        if let Some(err) = &self.search_remote_error {
                            ui.add_space(6.0);
                            ui.label(
                                egui::RichText::new(format!("Server search failed: {err}"))
                                    .size(12.0)
                                    .color(ERROR_COLOR),
                            );
                        } else if !self.search_remote.is_empty() {
                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new("Elsewhere in the server")
                                    .size(11.0)
                                    .strong()
                                    .color(secondary),
                            );
                            for (i, hit) in self.search_remote.iter().enumerate() {
                                let job = Self::hit_job(hit, width, text, secondary, accent);
                                let row = ui.add(
                                    egui::Button::new(job)
                                        .min_size(egui::vec2(width, 0.0)),
                                );
                                if row.clicked() {
                                    remote_jump = Some(i);
                                }
                            }
                        } else if !self.search_remote_loading
                            && typed
                            && self.search_remote_query == self.search_query.trim()
                        {
                            // An answer arrived and had nothing outside this
                            // channel; silence would look like a failed search.
                            ui.add_space(6.0);
                            ui.label(
                                egui::RichText::new("Nothing else on the server")
                                    .size(12.0)
                                    .color(secondary),
                            );
                        }
                    });
            });
        // Escape closes the overlay even while the field has focus; the
        // composer skips its own Escape handling while this window is open.
        if !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.close_search();
            return;
        }
        if ask_guild {
            self.search_guild();
        }
        if let Some(index) = jump {
            self.jump_to_search_hit(index);
        }
        if let Some(index) = remote_jump {
            self.open_search_result(index);
        }
    }
}
