use std::sync::Arc;

use eframe::egui::{self, Color32, RichText};

use crate::app::{App, ReplyTarget};
use crate::models::{ChatMessage, MsgHeight};
use crate::ui::attachments::{display_size, for_each_image, reserved_size};
use crate::ui::ERROR_RED;

/// How far beyond the viewport messages are drawn, so fast scrolling
/// doesn't reveal blank strips.
const OVERSCAN: f32 = 600.0;
/// How near the top triggers loading older messages.
const LOAD_MORE_AT_TOP: f32 = 48.0;
/// Height reserved for the "Loading older messages" row, so the list
/// doesn't shift when it appears or disappears.
const MORE_ROW_H: f32 = 20.0;

// Height-estimation constants; must match what drawing produces, or the
// scrollbar lies and the list jumps. Values measured on live egui.
const NAME_ROW_H: f32 = 18.0;
const LINE_H: f32 = 16.5;
const BUBBLE_BASE_H: f32 = 30.0;
/// Another author's message is never shorter than an avatar-lined row.
const OTHER_MIN_H: f32 = 72.0;
/// Gap after an image inside a message.
const IMAGE_GAP: f32 = 6.0;
/// Average 14pt character width; only for estimation — on-screen heights
/// are measured for real.
const CHAR_W: f32 = 7.0;
/// Height-cache cap. Message ids would linger after messages are trimmed,
/// so we clear the whole cache.
const MAX_HEIGHT_CACHE: usize = 4096;

/// Which actions a message's context menu offers. Not every action applies to
/// every message (Edit/Delete only to our own confirmed messages; Reply needs
/// an id to reference).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct MessageMenu {
    pub(crate) reply: bool,
    pub(crate) edit: bool,
    pub(crate) delete: bool,
    pub(crate) copy: bool,
}

/// A chosen context-menu entry. Copy is wired up now; the others carry the
/// user's intent to be connected to the gateway protocol later.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MessageAction {
    Reply,
    Edit,
    Delete,
    Copy,
}

/// Finds the visible message range. `offsets` is prefix sums, so this is a
/// binary search rather than a full scan.
pub(crate) fn visible_window(offsets: &[f32], offset: f32, viewport: f32) -> (usize, usize) {
    let n = offsets.len().saturating_sub(1);
    if n == 0 {
        return (0, 0);
    }
    let top = offset - OVERSCAN;
    let bottom = offset + viewport + OVERSCAN;
    // First message whose top is above the window (still drawn via OVERSCAN).
    let first = offsets[..n].partition_point(|v| *v <= top).min(n - 1);
    // First message whose top is past the bottom edge.
    let end = offsets.partition_point(|v| *v < bottom).max(first + 1).min(n);
    (first, end)
}

/// Lines a text of `chars` characters takes at width `text_w`.
fn estimate_lines(chars: usize, text_w: f32) -> f32 {
    if text_w <= 1.0 {
        return 1.0;
    }
    ((chars as f32 * CHAR_W) / text_w).ceil().max(1.0)
}

/// One-line preview of a replied message, capped so the composer bar stays
/// short. Newlines are folded so the bar never grows taller.
pub(crate) fn reply_preview(text: &str) -> String {
    const PREVIEW_CHARS: usize = 120;
    let mut preview: String = text.chars().take(PREVIEW_CHARS).collect();
    if text.chars().count() > PREVIEW_CHARS {
        preview.push('…');
    }
    preview.replace(['\n', '\r'], " ")
}

/// Chat width rounded to a pixel, used as the height-cache key. Ignoring
/// sub-pixels prevents width jitter from invalidating the cache every frame.
fn width_key(width: f32) -> u32 {
    width.round().max(0.0) as u32
}

impl App {
    pub(crate) fn draw_chat(&mut self, ctx: &egui::Context) {
        let mut style = (*ctx.style()).clone();
        style.visuals.panel_fill = self.theme.panel_bg;
        style.visuals.window_fill = self.theme.panel_bg;
        style.visuals.widgets.noninteractive.bg_fill = self.theme.channel_bg;
        style.visuals.widgets.inactive.bg_fill = self.theme.input_bg;
        style.visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, self.theme.text);
        style.visuals.widgets.hovered.bg_fill = self.theme.message_hover;
        style.visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0_f32, self.theme.accent);
        style.visuals.widgets.active.bg_fill = self.theme.accent;
        style.visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0_f32, Color32::BLACK);
        style.visuals.selection.bg_fill = self.theme.accent;
        style.visuals.selection.stroke = egui::Stroke::new(1.0_f32, Color32::BLACK);
        style.visuals.hyperlink_color = self.theme.accent;
        style.spacing.item_spacing = egui::vec2(8.0, 2.0);
        style.spacing.button_padding = egui::vec2(10.0, 4.0);
        ctx.set_style(style);

        self.draw_legacy_vault_banner(ctx);
        self.draw_server_list(ctx);
        let r1 = ctx.available_rect();
        self.draw_channel_list(ctx);
        let r2 = ctx.available_rect();
        self.draw_input_bar(ctx);
        let r3 = ctx.available_rect();
        self.draw_main_chat(ctx);
        let r4 = ctx.available_rect();
        if self.debug_frames % 15 == 0 {
            self.push_debug(format!("FRAME: screen={:.0}x{:.0} servers={:.0}x{:.0} channels={:.0}x{:.0} input={:.0}x{:.0} chat={:.0}x{:.0}",
                ctx.screen_rect().width(), ctx.screen_rect().height(),
                r1.width(), r1.height(),
                r2.width(), r2.height(),
                r3.width(), r3.height(),
                r4.width(), r4.height()));
        }
        self.debug_frames += 1;
    }
    /// Whether this is our own message; own and other bubbles differ.
    pub(crate) fn is_own_msg(&self, msg: &ChatMessage) -> bool {
        msg.is_own || (!self.user_id.is_empty() && msg.author_id == self.user_id)
    }
    /// Which context-menu actions apply to `msg`. Reply/Edit/Delete need a
    /// confirmed id; only our own messages can be edited or deleted.
    pub(crate) fn message_menu(&self, msg: &ChatMessage) -> MessageMenu {
        let confirmed = !msg.id.is_empty();
        let own = self.is_own_msg(msg);
        MessageMenu {
            reply: confirmed,
            edit: own && confirmed,
            delete: own && confirmed,
            copy: !self.display_content(msg).trim().is_empty(),
        }
    }
    /// Runs a chosen menu action. Copy is implemented; the rest are the
    /// connection points for the upcoming reply/edit/delete work.
    fn run_message_action(
        &mut self,
        ctx: &egui::Context,
        action: MessageAction,
        msg: &ChatMessage,
    ) {
        match action {
            MessageAction::Copy => {
                let text = self.display_content(msg);
                if !text.trim().is_empty() {
                    ctx.copy_text(text.to_string());
                }
            }
            // Reply hands the target to the composer; sending it is A1's REST path.
            MessageAction::Reply => {
                let author_name = self.display_name(msg).to_string();
                let preview = reply_preview(self.display_content(msg));
                let preview = if preview.is_empty() {
                    "сообщение".to_string()
                } else {
                    preview
                };
                self.reply_to = Some(ReplyTarget {
                    message_id: msg.id.clone(),
                    author_name,
                    preview,
                });
            }
            // Edit/Delete: the menu wiring exists; the gateway commands are
            // added together with A2/A3.
            MessageAction::Edit | MessageAction::Delete => {}
        }
    }
    /// Context menu for one message, attached to its frame response.
    fn message_context_menu(&mut self, response: &egui::Response, msg: &ChatMessage) {
        let menu = self.message_menu(msg);
        response.context_menu(|ui| {
            if ui
                .add_enabled(menu.reply, egui::Button::new("Ответить"))
                .clicked()
            {
                self.run_message_action(ui.ctx(), MessageAction::Reply, msg);
                ui.close_menu();
            }
            if ui
                .add_enabled(menu.edit, egui::Button::new("Изменить"))
                .clicked()
            {
                self.run_message_action(ui.ctx(), MessageAction::Edit, msg);
                ui.close_menu();
            }
            if ui
                .add_enabled(menu.delete, egui::Button::new("Удалить"))
                .clicked()
            {
                self.run_message_action(ui.ctx(), MessageAction::Delete, msg);
                ui.close_menu();
            }
            ui.separator();
            if ui
                .add_enabled(menu.copy, egui::Button::new("Копировать текст"))
                .clicked()
            {
                self.run_message_action(ui.ctx(), MessageAction::Copy, msg);
                ui.close_menu();
            }
        });
    }
    /// Measured height after drawing, otherwise an estimate from text length;
    /// virtualization needs it to decide what to draw.
    fn msg_height(&mut self, msg: &ChatMessage, width: f32) -> f32 {
        // Messages without an id (unconfirmed sends) aren't cached: the key
        // would collide.
        if !msg.id.is_empty() {
            if let Some(h) = self.msg_heights.get(&msg.id) {
                // Only valid at the same width; after a resize old values
                // are wrong and the list drifts.
                if h.width == width_key(width) {
                    return h.height;
                }
            }
        }
        self.estimate_msg_height(msg, width)
    }
    /// Estimated height. Error washes out once the message is drawn, but a
    /// systematic bias makes the list and scrollbar drift, hence measured
    /// constants.
    fn estimate_msg_height(&mut self, msg: &ChatMessage, width: f32) -> f32 {
        let own = self.is_own_msg(msg);
        // Text width inside the bubble: own messages are capped to a share
        // of chat width, others also lose room to the avatar.
        let text_w = if own {
            (width * 0.75).clamp(160.0, 480.0) - 21.0
        } else {
            width - 65.0
        };
        let lines = estimate_lines(self.display_content(msg).chars().count(), text_w);
        let mut images = 0.0f32;
        for_each_image(msg, |url, known| {
            // Prefer the cached height, else the size Discord sent with the URL.
            let h = match self.image_cache.get(url) {
                Some(img) => display_size(img.size_vec2()).y,
                None => reserved_size(known).y,
            };
            images += h + IMAGE_GAP;
        });
        let total = BUBBLE_BASE_H + NAME_ROW_H + lines * LINE_H + images;
        if own { total } else { total.max(OTHER_MIN_H) }
    }
    /// Scroll offset to open the frame at. Recomputes the anchored message's
    /// position so the view stays put as the list changes.
    fn anchored_offset(&self, msgs: &[Arc<ChatMessage>], offsets: &[f32]) -> Option<f32> {
        let (id, dy) = self.chat_anchor.as_ref()?;
        let i = msgs.iter().position(|m| &m.id == id)?;
        Some((offsets[i] - dy).max(0.0))
    }
    /// Distance from the bottom still counted as "at bottom". Tiny, or the
    /// chat would keep snapping the user back while scrolling.
    const BOTTOM_SLACK: f32 = 1.0;
    /// Whether to hold the bottom. The list length changes as history,
    /// heights, and images arrive, so the bottom must be recomputed each frame.
    fn keep_at_bottom(&self) -> bool {
        self.scroll_to_bottom || self.chat_at_bottom
    }
    pub(crate) fn draw_main_chat(&mut self, ctx: &egui::Context) {
        // Take the channel list for the frame to draw from an owned `Vec`,
        // avoiding a per-frame clone of every `Arc`. Restored at the end.
        let (taken_channel, msgs) = self.take_channel_messages();

        // No separate avatar prefetch: each message row already fetches its
        // avatar, and a separate pass allocated a key String per message.

        let channel_label = self.selected_channel
            .and_then(|i| self.channels.get(i))
            .map(|c| {
                if let Some(ref topic) = c.topic {
                    format!("{} — {}", c.name, topic)
                } else {
                    format!("# {}", c.name)
                }
            });

        let sel_cid = self.selected_channel
            .and_then(|i| self.channels.get(i))
            .map(|c| c.id.clone())
            .unwrap_or_else(|| "none".into());
        let key = format!("{}|{}", sel_cid, msgs.len());
        if key != self.last_render_key {
            self.last_render_key = key;
            let n_channels = self.channels.len();
            let srect = ctx.screen_rect();
            // The list is already taken, so use that length as the message
            // count for the open channel.
            let stored = msgs.len();
            self.push_debug(format!("RENDER: sel='{}' sel_cid='{}' msgs={} stored={} total_channels={} screen={}x{}", channel_label.clone().unwrap_or_else(|| "none".into()), sel_cid, msgs.len(), stored, n_channels, srect.width().round() as i32, srect.height().round() as i32));
        }

        match channel_label {
            Some(_) => {
                let sel_id = sel_cid.clone();
                let stick = self.keep_at_bottom();
                // The prefix-sum buffer lives outside self for the frame, so
                // messages can be read while writing to it. Heights are cached
                // by id, so prepending history is safe.
                let mut offsets: Vec<f32> = std::mem::take(&mut self.msg_offsets);
                // Content width is only known from last frame; the list is
                // computed before egui shows it.
                let width = if self.msg_width > 1.0 { self.msg_width } else { ctx.available_rect().width() };
                offsets.clear();
                offsets.reserve(msgs.len() + 1);
                offsets.push(0.0);
                let mut sum = 0.0_f32;
                for m in &msgs {
                    sum += self.msg_height(m, width);
                    offsets.push(sum);
                }
                let total = sum;
                // When the chat moves to the bottom itself, there's nothing to
                // anchor; anchoring would pin the old position.
                let hint = if stick { None } else { self.anchored_offset(&msgs, &offsets) };
                // Open at the bottom directly: the remaining space is known
                // upfront, so a rewinding frame doesn't draw the channel top.
                let inner = if self.chat_inner_h > 1.0 {
                    self.chat_inner_h
                } else {
                    ctx.available_rect().height()
                };
                let offset = if stick {
                    // Request the bottom with last frame's estimate error,
                    // otherwise the last message lands above the edge and
                    // `chat_at_bottom` goes false.
                    let err = (self.chat_content_h - self.chat_est_h).clamp(
                        -(total / 2.0),
                        total / 2.0,
                    );
                    (total - inner + err).max(0.0)
                } else {
                    hint.unwrap_or(self.chat_offset_y)
                };

                let panel_resp = egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(self.theme.channel_bg))
                    .show(ctx, |ui| {
                        ui.set_min_width(0.0);

                        // Send failures are shown here: the status field only
                        // exists on the login screen, so the message vanished
                        // silently.
                        if let Some(reason) = self.send_error.as_ref() {
                            ui.horizontal_centered(|ui| {
                                ui.label(RichText::new(format!("Не отправлено: {reason}"))
                                    .size(12.0)
                                    .color(ERROR_RED));
                            });
                        }

                        if self.history_loading.is_some() {
                            ui.horizontal_centered(|ui| {
                                ui.spinner();
                                ui.label(RichText::new("Loading messages...").size(12.0).color(self.theme.text_secondary));
                            });
                        } else if let Some((_, reason)) = self.history_error.as_ref() {
                            // Don't show an empty channel silently: it looks
                            // like the channel has no messages.
                            ui.horizontal_centered(|ui| {
                                ui.label(RichText::new(format!("Не удалось загрузить историю: {reason}"))
                                    .size(12.0)
                                    .color(self.theme.text_secondary));
                            });
                        }

                        let msgs_for_render = &msgs;
                        let mut want_older = false;
                        let scroll_out = egui::ScrollArea::vertical()
                            .id_salt(("chat", &sel_id))
                            .auto_shrink([false, false])
                            // Only stick when we move there ourselves; asking
                            // every frame would fight the wheel. Bottom-holding
                            // is done via the offset passed below.
                            .stick_to_bottom(self.scroll_to_bottom)
                            // Use the offset the message window was computed
                            // with, not last frame's, or a rewinding frame
                            // draws the channel top.
                            .vertical_scroll_offset(offset)
                            .show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                self.msg_width = ui.available_width();
                                ui.add_space(8.0);
                                // Always reserve space for the load-older row.
                                ui.vertical_centered(|ui| {
                                    ui.set_min_height(MORE_ROW_H);
                                    if self.history_loading_more {
                                        ui.horizontal(|ui| {
                                            ui.spinner();
                                            ui.label(RichText::new("Loading older messages...").size(12.0).color(self.theme.text_secondary));
                                        });
                                    } else if let Some((_, reason)) = self.history_error.as_ref() {
                                        // Loading older messages failed; a
                                        // spinner here would spin forever.
                                        ui.horizontal(|ui| {
                                            ui.label(RichText::new(format!("Не удалось догрузить: {reason}"))
                                                .size(12.0)
                                                .color(self.theme.text_secondary));
                                        });
                                    } else if self.trimmed_newest > 0 {
                                        // Memory cap hit: newest messages were
                                        // dropped to fit the loaded page. Say so,
                                        // or it looks like the end of history.
                                        let dropped = self.trimmed_newest;
                                        ui.vertical_centered(|ui| {
                                            ui.label(
                                                RichText::new(format!(
                                                    "Достигнут предел: в этом канале хранится не больше {} сообщений",
                                                    crate::app::MAX_MESSAGES_PER_CHANNEL
                                                ))
                                                .size(12.0)
                                                .color(self.theme.text_secondary),
                                            );
                                            ui.label(
                                                RichText::new(format!(
                                                    "{dropped} новых скрыто — открой канал заново, чтобы увидеть их"
                                                ))
                                                .size(11.0)
                                                .color(self.theme.text_secondary),
                                            );
                                        });
                                    }
                                });
                                if msgs_for_render.is_empty() && self.history_loading.is_none() {
                                    ui.vertical_centered(|ui| {
                                        ui.add_space(60.0);
                                        ui.label(RichText::new("No messages here yet")
                                            .size(16.0).color(self.theme.text_secondary));
                                        ui.add_space(4.0);
                                        ui.label(RichText::new("Be the first to say something above")
                                            .size(12.0).color(self.theme.text_secondary));
                                    });
                                } else {
                                    // Draw only the window plus overscan, but
                                    // reserve space for skipped messages so the
                                    // scrollbar reports the full history.
                                    let (first, end) = visible_window(&offsets, offset, ui.available_height());
                                    // A slice, not a copy: `msgs` is ours for
                                    // the frame, so a window Vec would be waste.
                                    let window = msgs_for_render.get(first..end).unwrap_or(&[]);
                                    // Bottom of the last drawn message, used to
                                    // compute gaps.
                                    let mut drawn = 0.0_f32;
                                    // Reused buffer for the avatar cache key,
                                    // instead of a String per message per frame.
                                    let mut avatar_key = String::new();
                                    for (k, msg) in window.iter().enumerate() {
                                        // Test-only: strong refs to the message
                                        // at draw time; a per-frame clone would
                                        // double the count.
                                        #[cfg(test)]
                                        {
                                            self.probe_msg_refs = self
                                                .probe_msg_refs
                                                .max(Arc::strong_count(msg));
                                        }
                                        let i = first + k;
                                        let top = offsets[i];
                                        let gap = top - drawn;
                                        if gap > 0.0 {
                                            ui.add_space(gap);
                                        }
                                        let before = ui.min_rect().bottom();
                                        self.draw_message(ui, msg, &mut avatar_key);
                                        let h = (ui.min_rect().bottom() - before).max(1.0);
                                        drawn = top + h;
                                        // Usually unchanged, so look it up first
                                        // and only clone the id on change. A
                                        // missing entry counts too: the anchor
                                        // needs the measured height.
                                        let key = width_key(width);
                                        let stored = self.msg_heights.get(&msg.id).copied();
                                        if !msg.id.is_empty()
                                            && stored.map(|m| (m.height, m.width)) != Some((h, key))
                                        {
                                            if self.msg_heights.len() > MAX_HEIGHT_CACHE {
                                                self.msg_heights.clear();
                                            }
                                            self.msg_heights.insert(
                                                msg.id.clone(),
                                                MsgHeight { height: h, width: key },
                                            );
                                        }
                                    }
                                    // Pad to the estimated total so the
                                    // scrollbar reflects the whole list.
                                    let tail = (total - drawn).max(0.0);
                                    if tail > 0.0 {
                                        ui.add_space(tail);
                                    }
                                    // At the top, request older messages — but
                                    // not when already at bottom or when the
                                    // whole list fits (nothing to scroll).
                                    let scrollable = total > ui.available_height();
                                    if scrollable && offset <= LOAD_MORE_AT_TOP {
                                        want_older = true;
                                    }
                                }
                                ui.add_space(8.0);
                            });
                        self.want_older = want_older;

                        let inner_h = scroll_out.inner_rect.height();
                        let content_h = scroll_out.content_size.y;
                        let offset_y = scroll_out.state.offset.y;
                        self.chat_inner_h = inner_h;
                        self.push_debug(format!("SCROLL: inner_h={:.0} content_h={:.0} offset_y={:.0} ask={:.0} stick={}", inner_h, content_h, offset_y, offset, stick));

                        // Force the bottom only for an explicit jump: egui
                        // computes content after drawing, but the jump must
                        // happen before it. Plain bottom-holding uses the
                        // requested offset instead of egui state.
                        let forced_bottom = self.scroll_to_bottom;
                        if forced_bottom {
                            let mut st = scroll_out.state;
                            st.offset.y = (content_h - inner_h).max(0.0);
                            st.store(ui.ctx(), scroll_out.id);
                        }
                        self.scroll_to_bottom = false;
                        if forced_bottom {
                            self.chat_anchor = None;
                        }
                        self.chat_content_h = content_h;
                        self.chat_est_h = total;
                        let max_off = (content_h - inner_h).max(0.0);
                        // The list landed neither where we asked nor at the
                        // real bottom, so the user scrolled; don't override
                        // that. A small discrepancy is just our estimate error.
                        let moved_by_user = (offset_y - offset).abs() > Self::BOTTOM_SLACK
                            && (offset_y - max_off).abs() > Self::BOTTOM_SLACK;
                        // Where the chat actually settled: at the bottom when
                        // held and untouched, otherwise where scrolling left it.
                        let settled = if (forced_bottom || stick) && !moved_by_user {
                            max_off
                        } else {
                            offset_y
                        };
                        self.chat_offset_y = settled;
                        self.last_scroll_offset_y = self.last_scroll_offset_y.max(settled);
                        // At the bottom, a new message pulls the chat down;
                        // higher up, the reader is left alone. Tiny threshold,
                        // or the chat snaps back from small scrolls.
                        self.chat_at_bottom = settled >= max_off - Self::BOTTOM_SLACK;
                        // Anchor from where the chat actually settled, not
                        // where we asked: egui applies the wheel after drawing,
                        // so anchoring on "asked" freezes the view each frame.
                        // No anchor is needed while holding the bottom.
                        self.chat_anchor = if stick && !moved_by_user {
                            None
                        } else {
                            let top = visible_window(&offsets, settled, inner_h).0;
                            msgs.get(top).map(|m| (m.id.clone(), offsets[top] - settled))
                        };
                        scroll_out
                    });
                // Put the list back: `request_older_history` reads the oldest
                // id from it, while the frame drew from its own Vec.
                self.restore_channel_messages(taken_channel, msgs);
                self.msg_offsets = offsets;
                if self.want_older {
                    self.want_older = false;
                    self.request_older_history();
                }
                let presp = panel_resp.response.rect;
                let p_rect = presp.min;
                let srect = ctx.screen_rect();
                self.push_debug(format!("PANEL: rect={:.0}x{:.0}+({:.0},{:.0}) screen={:.0}x{:.0}", presp.width(), presp.height(), p_rect.x, p_rect.y, srect.width(), srect.height()));
            }
            None => {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(120.0);
                        ui.label(RichText::new("Welcome to Wyvern").size(24.0).color(self.theme.text));
                        ui.add_space(8.0);
                        ui.label(RichText::new("1. Click a server icon on the far left")
                            .size(14.0).color(self.theme.text_secondary));
                        ui.label(RichText::new("2. Click a channel (# name) in the middle list")
                            .size(14.0).color(self.theme.text_secondary));
                        ui.label(RichText::new("3. Type a message at the bottom and press Enter")
                            .size(14.0).color(self.theme.text_secondary));
                    });
                });
            }
        }
    }
    /// Draws one message. The caller measures height from the scroll content.
    pub(crate) fn draw_message(&mut self, ui: &mut egui::Ui, msg: &ChatMessage, avatar_key: &mut String) {
        let display = self.display_name(msg);
        if self.is_own_msg(msg) {
            let max_w = (ui.available_width() * 0.75).clamp(160.0, 480.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                let frame = egui::Frame::new()
                    .fill(self.theme.self_bg)
                    .stroke(egui::Stroke::new(1.0_f32, self.theme.divider))
                    .corner_radius(8.0)
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                    ui.set_max_width(max_w);
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(display).strong().size(14.0).color(self.theme.accent));
                            if !msg.timestamp.is_empty() {
                                let t = self.short_time(&msg.timestamp);
                                ui.label(RichText::new(t).size(11.0).color(self.theme.text_secondary));
                            }
                        });
                        let content = self.display_content(msg);
                        let content = if content.is_empty() { "* (message)" } else { content };
                        ui.label(RichText::new(content).size(14.0).color(self.theme.text));
                        self.draw_attachments(ui, msg);
                    });
                });
                self.message_context_menu(&frame.response, msg);
            });
            ui.add_space(6.0);
        } else {
            let frame = egui::Frame::new()
                .fill(self.theme.message_hover)
                .stroke(egui::Stroke::new(1.0_f32, self.theme.divider))
                .corner_radius(8.0)
                .inner_margin(egui::Margin::symmetric(10, 8))
                .show(ui, |ui| {
                ui.set_min_height(44.0);
                ui.horizontal(|ui| {
                    let size = 36.0;
                    let avatar_tex = msg.author_avatar.as_ref()
                        .and_then(|h| {
                            avatar_key.clear();
                            avatar_key.push_str(&msg.author_id);
                            avatar_key.push('_');
                            avatar_key.push_str(h);
                            self.avatar_cache.get(avatar_key.as_str()).cloned()
                        })
                        .or_else(|| {
                            if let Some(h) = &msg.author_avatar {
                                self.download_avatar(ui.ctx(), &msg.author_id, h)
                            } else { None }
                        });
                    if let Some(tex) = avatar_tex {
                        ui.add_sized(
                            egui::vec2(size, size),
                            egui::Image::new(
                                egui::load::SizedTexture::new(tex.id(), egui::vec2(size, size))
                            ).corner_radius(size / 2.0),
                        );
                    } else {
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
                        ui.painter().circle_filled(rect.center(), size / 2.0, self.theme.accent);
                        let initial = msg.author_name.chars().next().unwrap_or('?').to_uppercase().to_string();
                        ui.painter().text(
                            rect.center(), egui::Align2::CENTER_CENTER, &initial,
                            egui::FontId::proportional(16.0), Color32::WHITE,
                        );
                    }
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(display).strong().size(14.0).color(self.theme.accent));
                            if !msg.timestamp.is_empty() {
                                let t = self.short_time(&msg.timestamp);
                                ui.label(RichText::new(t).size(11.0).color(self.theme.text_secondary));
                            }
                        });
                        let content = self.display_content(msg);
                        let content = if content.is_empty() { "* (message)" } else { content };
                        ui.label(RichText::new(content).size(14.0).color(self.theme.text));
                        self.draw_attachments(ui, msg);
                    });
                });
                });
                self.message_context_menu(&frame.response, msg);
                ui.add_space(6.0);
        }
    }
}

#[cfg(test)]
include!("chat_tests.rs");
