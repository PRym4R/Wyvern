use std::sync::Arc;

use eframe::egui::{self, Color32, RichText};

use crate::app::App;
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
                egui::Frame::new()
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
            });
            ui.add_space(6.0);
        } else {
            egui::Frame::new()
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
                ui.add_space(6.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::visible_window;
    use crate::ui::chat::OVERSCAN;

    /// Prefix sums of the given heights.
    fn offsets(h: &[f32]) -> Vec<f32> {
        let mut o = vec![0.0];
        let mut s = 0.0;
        for v in h {
            s += v;
            o.push(s);
        }
        o
    }

    /// The window must cover the viewport plus overscan without exceeding
    /// the list.
    #[test]
    fn visible_window_covers_viewport() {
        let o = offsets(&[100.0; 100]);
        for offset in [0.0, 1.0, 500.0, 1000.0, 5000.0, 9_000.0, 9_900.0] {
            let (first, end) = visible_window(&o, offset, 800.0);
            assert!(first < end, "окно пустое при offset={offset}");
            assert!(end <= 100, "окно вышло за список: {end} при offset={offset}");
            // Everything visible must be drawn: window top not below the
            // viewport start, window end past its end.
            assert!(
                o[first] <= offset,
                "начало видимого пропущено: first={first} offset={offset}"
            );
            assert!(
                end == 100 || o[end] >= offset + 800.0,
                "конец видимого не нарисован: end={end} offset={offset}"
            );
            // And there's overscan so fast scrolling shows no blanks.
            assert!(
                o[first + 1] > offset - OVERSCAN,
                "пропущено начало: first={first} offset={offset}"
            );
        }
    }

    /// The first message isn't skipped even at zero scroll.
    #[test]
    fn visible_window_keeps_first_message() {
        let o = offsets(&[40.0; 3]);
        let (first, end) = visible_window(&o, 0.0, 500.0);
        assert_eq!(first, 0);
        assert!(end >= 1);
    }

    /// Empty and single-message lists must not break the calculation.
    #[test]
    fn visible_window_handles_empty_list() {
        assert_eq!(visible_window(&[0.0], 0.0, 500.0), (0, 0));
        assert_eq!(visible_window(&[], 0.0, 500.0), (0, 0));
        let one = offsets(&[100.0]);
        assert_eq!(visible_window(&one, 0.0, 500.0), (0, 1));
    }
}

/// Geometry tests on real egui: catch gaps, a lying scrollbar, and view
/// jumps during history loading that unit tests can't.
#[cfg(test)]
mod geometry_tests {
    use std::sync::Arc;

    use tokio::sync::mpsc;

    use super::*;
    use crate::messages::{ToApp, ToGateway};
    use crate::models::ChatChannel;

    /// The measurement screen the test numbers were captured on.
    fn screen() -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1052.0, 1054.0),
            )),
            ..Default::default()
        }
    }

    fn message(i: usize) -> ChatMessage {
        ChatMessage {
            id: format!("m{i:04}"),
            channel_id: "c0".into(),
            author_id: format!("u{}", i % 7),
            author_name: format!("user{}", i % 7),
            author_avatar: None,
            nickname: None,
            // Varying lengths, so virtualization has something to catch.
            content: "сообщение номер ".to_string() + &i.to_string()
                + &" и ещё немного текста сверху, чтобы высота отличалась".repeat(i % 4),
            timestamp: "2026-01-01T00:00:00.000Z".into(),
            attachments: vec![],
            embeds: vec![],
            is_own: i % 11 == 0,
        }
    }

    /// A channel with `n` messages, plus the sender so new messages arrive
    /// through the same path as the real gateway.
    fn app_with_messages(n: usize) -> Harness {
        let (tx, rx) = mpsc::unbounded_channel::<ToApp>();
        let (to_gw, cmds) = mpsc::unbounded_channel();
        let mut app = App::new(rx);
        app.connected = true;
        app.gw_started = true;
        app.to_gw = Some(to_gw);
        app.channels.push(ChatChannel {
            id: "c0".into(),
            name: "основной".into(),
            guild_id: None,
            channel_type: 0,
            topic: None,
            position: 0,
        });
        app.selected_channel = Some(0);
        let msgs: Vec<Arc<ChatMessage>> = (0..n).map(|i| Arc::new(message(i))).collect();
        app.messages.insert("c0".into(), msgs);
        Harness { app, tx, cmds }
    }

    struct Harness {
        app: App,
        /// Messages from the gateway to the app.
        tx: mpsc::UnboundedSender<ToApp>,
        /// Commands from the app to the gateway.
        cmds: mpsc::UnboundedReceiver<ToGateway>,
    }

    /// A frame must not clone the channel's message list.
    ///
    /// The probe counts strong refs at draw time: a clone would show more
    /// than one, while a borrowed list shows exactly one.
    #[test]
    fn drawing_the_chat_does_not_clone_the_message_list() {
        let mut h = app_with_messages(50);
        let ctx = egui::Context::default();
        h.app.probe_msg_refs = 0;
        frame(&mut h.app, &ctx);
        assert_eq!(
            h.app.probe_msg_refs, 1,
            "кадр копирует список сообщений канала: сильных ссылок было {}",
            h.app.probe_msg_refs
        );
    }

    /// A height cached at one width must not be reused after a resize; the
    /// cache key includes the width.
    #[test]
    fn cached_height_is_dropped_when_the_width_changes() {
        let mut h = app_with_messages(1);
        let m = message(0);
        h.app
            .msg_heights
            .insert(m.id.clone(), MsgHeight { height: 999.0, width: 400 });
        // Same width: from the cache.
        assert_eq!(h.app.msg_height(&m, 400.0), 999.0);
        // Different width: the old value is invalid, so it's recomputed.
        let fresh = h.app.msg_height(&m, 800.0);
        let estimate = h.app.estimate_msg_height(&m, 800.0);
        assert_ne!(fresh, 999.0, "после ресайза старые высоты не годятся");
        assert_eq!(fresh, estimate, "на новой ширине высота берётся из оценки");
    }

    /// The wheel must scroll both ways. Check what's drawn, not the internal
    /// counter: egui applies the wheel after drawing, so they can diverge.
    #[test]
    fn wheel_scrolls_the_chat_in_both_directions() {
        let mut h = app_with_messages(300);
        let ctx = egui::Context::default();
        frames(&mut h.app, &ctx, 3);
        let (inner, content, _ask) = scroll_sizes(&h.app);
        let bottom = content - inner;
        assert!(bottom > 1000.0, "список должен быть заметно выше окна");
        assert!(
            (h.app.chat_offset_y - bottom).abs() < 1.0,
            "открытый чат должен быть внизу: смещение {:.0}, низ {bottom:.0}",
            h.app.chat_offset_y
        );

        // Up to the start of history.
        let up = wheel_until_stop(&mut h.app, &ctx, 120.0, true);
        let (_i, _c, ask) = scroll_sizes(&h.app);
        eprintln!("[TEST] наверх за {up} событий: на экране смещение {ask:.0}");
        assert!(
            ask < 1.0,
            "колесо вверх не довело список до начала: на экране смещение {ask:.0}"
        );

        // And back down to the last message.
        let down = wheel_until_stop(&mut h.app, &ctx, -120.0, false);
        let (inner, content, ask) = scroll_sizes(&h.app);
        let bottom = content - inner;
        eprintln!("[TEST] вниз за {down} событий: на экране {ask:.0}, низ {bottom:.0}");
        assert!(
            (ask - bottom).abs() < 1.0,
            "колесо вниз не довело список до низа: на экране {ask:.0}, низ {bottom:.0}"
        );
    }

    /// A slow wheel must still move the drawn list; with fast scrolls the
    /// anchor was reset in time and hid the bug.
    #[test]
    fn slow_wheel_moves_the_drawn_list() {
        let mut h = app_with_messages(300);
        let ctx = egui::Context::default();
        frames(&mut h.app, &ctx, 3);
        let (_i, _c, start) = scroll_sizes(&h.app);
        assert!(start > 1000.0, "список должен быть заметно выше окна");

        // Small step, the case where the anchor survived to the next frame.
        let mut stuck = 0;
        let mut last = start;
        for _ in 0..60 {
            wheel_frame(&mut h.app, &ctx, 3.0);
            let (_i, _c, ask) = scroll_sizes(&h.app);
            if (ask - last).abs() < 0.5 {
                stuck += 1;
            } else {
                stuck = 0;
            }
            last = ask;
        }
        let (_i, _c, end) = scroll_sizes(&h.app);
        eprintln!("[TEST] медленное колесо: {start:.0} -> {end:.0}, замерло {stuck} раз");
        assert!(
            stuck < 10,
            "нарисованный список не идёт за медленным колесом: смещение {start:.0} -> {end:.0}, \
             {stuck} кадров из 60 стояли на месте"
        );
        assert!(
            start - end > 20.0,
            "медленное колесо вверх почти не сдвинуло список: {start:.0} -> {end:.0}"
        );
    }

    /// Wheel until the drawn list stops moving.
    ///
    /// Watches `ask` and requires it to move in the scroll direction without
    /// snapping back. egui smooths wheel input, so "stopped" means several
    /// identical frames in a row.
    fn wheel_until_stop(
        app: &mut App,
        ctx: &egui::Context,
        dy: f32,
        up: bool,
    ) -> usize {
        let mut still = 0;
        let mut events = 0;
        while still < 12 {
            let (_i, _c, before) = scroll_sizes(app);
            wheel_frame(app, ctx, dy);
            let (_i, _c, after) = scroll_sizes(app);
            events += 1;
            let delta = after - before;
            if delta.abs() < 0.5 {
                still += 1;
            } else {
                still = 0;
                // Scrolling up must move toward the start and vice versa; a
                // backward jump means the frame draws at the old offset.
                let wrong = if up { delta > 0.5 } else { delta < -0.5 };
                assert!(
                    !wrong,
                    "колесо {} увело список не туда: {before:.0} -> {after:.0} на событии {events}",
                    if up { "вверх" } else { "вниз" }
                );
            }
            assert!(events < 4000, "колесо не доводит список до края");
        }
        events
    }

    /// A frame holding the bottom must draw the last message whole; pure
    /// estimation opens the list slightly high and clips it.
    #[test]
    fn the_frame_at_the_bottom_draws_the_last_message_whole() {
        let mut h = app_with_messages(300);
        let ctx = egui::Context::default();
        for i in 0..6 {
            h.app.poll(&ctx);
            frame(&mut h.app, &ctx);
            let (inner, _content, ask) = scroll_sizes(&h.app);
            let n = h.app.messages["c0"].len();
            let last = h.app.messages["c0"][n - 1].clone();
            let h_last = h
                .app
                .msg_heights
                .get(&last.id)
                .map(|mh| mh.height)
                .expect("последнее сообщение должно быть нарисовано");
            let bottom = h.app.msg_offsets[n - 1] + h_last;
            assert!(
                bottom <= ask + inner + 1.0,
                "кадр {i}: последнее сообщение обрезано нижней кромкой окна — \
                 оно кончается на {bottom:.1}, а окно на {:.1} (просили смещение {ask:.0})",
                ask + inner
            );
        }
    }

    /// Estimation must stay close to the real height: a bias made the list
    /// drift and the bottom unreachable. Measured on live egui.
    #[test]
    fn estimate_height_is_close_to_the_real_one() {
        use crate::models::Attachment;
        let mut h = app_with_messages(1);
        let ctx = egui::Context::default();
        frames(&mut h.app, &ctx, 2);
        let width = h.app.msg_width;

        // Cases where estimation tends to miss: own/other messages of 1-20
        // lines, one long unbreakable word, and a message with an image.
        let mut cases: Vec<(String, ChatMessage)> = Vec::new();
        for (who, own) in [("чужое", false), ("своё", true)] {
            for lines in [1usize, 2, 5, 10, 20] {
                let mut m = message(lines);
                m.is_own = own;
                m.author_id = "u0".into();
                m.content = "слово ".repeat(lines * 12);
                cases.push((format!("{who} {lines} строк"), m));
            }
            let mut m = message(99);
            m.is_own = own;
            m.author_id = "u0".into();
            m.content = "ы".repeat(400);
            cases.push((format!("{who} длинное слово"), m));
            let mut m = message(98);
            m.is_own = own;
            m.author_id = "u0".into();
            m.content = "с картинкой".into();
            m.attachments.push(Attachment {
                url: "https://example.invalid/i.png".into(),
                content_type: Some("image/png".into()),
                description: None,
                size: Some([100, 200]),
            });
            cases.push((format!("{who} с картинкой"), m));
        }

        let mut sum_est = 0.0;
        let mut sum_real = 0.0;
        for (name, m) in &cases {
            let est = h.app.estimate_msg_height(m, width);
            // Draw one at a time; the list fits the window.
            h.app.messages.insert("c0".into(), vec![Arc::new(m.clone())]);
            scroll_to(&mut h.app, 0.0);
            frame(&mut h.app, &ctx);
            frame(&mut h.app, &ctx);
            let real = h.app.msg_heights.get(&m.id).map(|mh| mh.height).unwrap_or(0.0);
            assert!(
                (est - real).abs() <= real * 0.15 + 12.0,
                "{name}: оценка {est:.0} против настоящих {real:.0}"
            );
            sum_est += est;
            sum_real += real;
        }
        // Total list length must also converge; the scrollbar uses it.
        assert!(
            (sum_est - sum_real).abs() <= sum_real * 0.05,
            "длина списка по оценке {sum_est:.0} против настоящей {sum_real:.0}"
        );
    }

    /// The bottom must be reachable with the last message flush against the
    /// window edge.
    #[test]
    fn bottom_is_reachable_and_last_message_sits_at_the_edge() {
        let mut h = app_with_messages(300);
        let ctx = egui::Context::default();
        frames(&mut h.app, &ctx, 3);

        // Scroll by injecting offsets, as egui reports after the wheel, and
        // check the bottom is reachable and held.
        for step in 1..=40 {
            let (inner, content, _ask) = scroll_sizes(&h.app);
            let max = (content - inner).max(0.0);
            scroll_to(&mut h.app, max * step as f32 / 40.0);
            frame(&mut h.app, &ctx);
            assert!(
                h.app.chat_offset_y <= max + 1.0,
                "смещение {step} больше низа: {:.0} при {max:.0}",
                h.app.chat_offset_y
            );
        }

        let (inner, content, _ask) = scroll_sizes(&h.app);
        let max = content - inner;
        // The last message must end exactly at the window bottom: not above
        // it (under-scrolled) and not past it (over-scrolled).
        let n = h.app.messages["c0"].len();
        let last_bottom = h.app.msg_offsets[n] - h.app.chat_offset_y;
        assert!(
            (last_bottom - inner).abs() <= inner * 0.05,
            "последнее сообщение не у низа окна: {last_bottom:.1} при окне {inner:.0} (смещение {:.0}, низ {max:.0})",
            h.app.chat_offset_y
        );
    }

    /// While the user is at the bottom, the list must stay there as it grows
    /// (history, corrected heights, images). Otherwise the chat drifts away
    /// from the last message and never returns.
    #[test]
    fn bottom_is_held_while_the_list_changes_under_it() {
        let mut h = app_with_messages(300);
        let ctx = egui::Context::default();
        frames(&mut h.app, &ctx, 3);
        assert!(h.app.chat_at_bottom, "после открытия чат должен быть внизу");

        // A new message lengthens the list, so the offset must follow.
        let before = h.app.chat_offset_y;
        h.tx.send(ToApp::Message(message(9999))).unwrap();
        h.app.poll(&ctx);
        frame(&mut h.app, &ctx);
        assert!(
            h.app.chat_offset_y > before,
            "новое сообщение не утянуло чат вниз: было {before:.0}, стало {:.0}",
            h.app.chat_offset_y
        );
        assert!(h.app.chat_at_bottom, "после нового сообщения чат должен быть внизу");

        // A message's height was corrected upward; the bottom must follow.
        let grew = h
            .app
            .msg_heights
            .values_mut()
            .next()
            .map(|mh| {
                mh.height += 40.0;
                mh.height
            })
            .expect("кэш высот не пуст после кадров");
        frame(&mut h.app, &ctx);
        // Read sizes after the frame, where the height grew.
        let (inner, content, _ask) = scroll_sizes(&h.app);
        let max = content - inner;
        assert!(
            (h.app.chat_offset_y - max).abs() < 1.0,
            "чат отполз от низа после роста сообщения: смещение {:.0}, низ {max:.0} (высота выросла на {grew:.0})",
            h.app.chat_offset_y
        );
        assert!(h.app.chat_at_bottom, "после уточнения высоты чат должен быть внизу");

        // A reader scrolled up must not be thrown back down.
        let up = total_height(&h.app) * 0.4;
        scroll_to(&mut h.app, up);
        frame(&mut h.app, &ctx);
        assert!(!h.app.chat_at_bottom, "пользователь ушёл вверх — чат не внизу");
        let held = h.app.chat_offset_y;
        frame(&mut h.app, &ctx);
        frame(&mut h.app, &ctx);
        assert!(
            (h.app.chat_offset_y - held).abs() < 1.0,
            "чат утёк вниз у читающего историю: {held:.0} -> {:.0}",
            h.app.chat_offset_y
        );
    }

    /// The frame a new message arrives in must reach the true bottom, not
    /// lag by that message's height. The `content_h - est_h` correction
    /// covers it, since `est_h` is last frame's `total`.
    #[test]
    fn new_message_frame_reaches_the_real_bottom() {
        let mut h = app_with_messages(300);
        let ctx = egui::Context::default();
        frames(&mut h.app, &ctx, 3);
        assert!(h.app.chat_at_bottom, "открытый чат должен быть внизу");

        // The message arrives between frames.
        h.tx.send(ToApp::Message(message(9999))).unwrap();
        h.app.poll(&ctx);
        frame(&mut h.app, &ctx);

        let (inner, content, ask) = scroll_sizes(&h.app);
        let real_bottom = (content - inner).max(0.0);
        let new_h = h
            .app
            .msg_heights
            .get("m9999")
            .map(|mh| mh.height)
            .expect("новое сообщение должно быть измерено");
        assert!(
            new_h > 40.0,
            "сообщение для проверки должно быть заметно выше допуска: {new_h:.0}"
        );
        assert!(
            (ask - real_bottom).abs() <= 1.0,
            "кадр с новым сообщением не дотянул до низа на высоту сообщения: \
             просили {ask:.1}, настоящий низ {real_bottom:.1} (сообщение {new_h:.0})"
        );
    }

    /// One application frame.
    fn frame(app: &mut App, ctx: &egui::Context) -> egui::FullOutput {
        ctx.run(screen(), |ctx| app.draw_chat(ctx))
    }

    /// Several frames in a row: the first only warms the height cache and
    /// debug line, the last matters.
    fn frames(app: &mut App, ctx: &egui::Context, n: usize) {
        for _ in 0..n {
            frame(app, ctx);
        }
    }

    /// A frame with a real wheel event over the chat. `dy` follows egui's
    /// sign: positive scrolls toward the start, negative toward new messages.
    ///
    /// The cursor is nudged half a pixel each frame; egui only treats the
    /// pointer as hovering if it moved, or the wheel never reaches the scroll.
    fn wheel_frame(app: &mut App, ctx: &egui::Context, dy: f32) {
        NUDGE.with(|n| {
            let shift = n.get();
            n.set(if shift >= 1.0 { 0.0 } else { shift + 0.5 });
            let mut input = screen();
            input
                .events
                .push(egui::Event::PointerMoved(egui::Pos2::new(600.0 + shift, 500.0)));
            input.events.push(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, dy),
                modifiers: egui::Modifiers::default(),
            });
            let _ = ctx.run(input, |ctx| app.draw_chat(ctx));
        });
    }

    thread_local! {
        /// Counter for the pointer nudge, see `wheel_frame`.
        static NUDGE: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) };
    }

    /// Drain everything the app sent to the gateway.
    fn drain(cmds: &mut mpsc::UnboundedReceiver<ToGateway>) -> Vec<ToGateway> {
        let mut out = Vec::new();
        while let Ok(c) = cmds.try_recv() {
            out.push(c);
        }
        out
    }

    /// Set the scroll offset to `off` and drop the anchor, as if the user
    /// scrolled manually; also clears "at bottom" so it isn't pulled back.
    fn scroll_to(app: &mut App, off: f32) {
        app.scroll_to_bottom = false;
        app.chat_at_bottom = false;
        app.chat_anchor = None;
        app.chat_offset_y = off;
    }

    /// Scroll sizes parsed from the `SCROLL:` debug line: viewport height,
    /// content height, and the offset we asked for.
    fn scroll_sizes(app: &App) -> (f32, f32, f32) {
        let line = app
            .debug_log
            .iter()
            .rev()
            .find(|l| l.starts_with("SCROLL:"))
            .expect("отладочная строка скролла");
        let mut inner = 0.0;
        let mut content = 0.0;
        let mut ask = 0.0;
        for part in line.split_whitespace() {
            if let Some(v) = part.strip_prefix("inner_h=") {
                inner = v.parse().expect("inner_h");
            }
            if let Some(v) = part.strip_prefix("content_h=") {
                content = v.parse().expect("content_h");
            }
            // The offset we asked for; it drives the rendered frame. Where
            // the chat settled afterward is checked separately.
            if let Some(v) = part.strip_prefix("ask=") {
                ask = v.parse().expect("ask");
            }
        }
        (inner, content, ask)
    }

    /// Total list height from the frame's prefix sums.
    fn total_height(app: &App) -> f32 {
        *app.msg_offsets.last().expect("буфер префиксных сумм пуст")
    }

    /// Virtualization must actually save work: only a handful of hundreds of
    /// messages are drawn, so the height cache stays small.
    #[test]
    fn virtualization_draws_only_what_is_on_screen() {
        let mut h = app_with_messages(300);
        let ctx = egui::Context::default();

        // As when opening a channel: first history page, chat at bottom.
        frames(&mut h.app, &ctx, 3);

        let (inner, content, _ask) = scroll_sizes(&h.app);
        let measured = h.app.msg_heights.len();
        eprintln!("[TEST] inner={inner:.0} content={content:.0} измерено={measured}");
        assert!(measured > 0, "ни одно сообщение не нарисовано");
        assert!(
            measured < 100,
            "виртуализация не работает: измерено {measured} высот из 300"
        );
        // The scrollbar must know the full history; invisible space is
        // reserved as padding at the end.
        assert!(
            (content - total_height(&h.app)).abs() < 120.0,
            "полоса прокрутки врёт: content={content:.0}, список={:.0}",
            total_height(&h.app)
        );
        assert!(content > inner * 3.0, "история должна быть заметно выше экрана");
    }

    /// The main virtualization risk is gaps. At several scroll positions,
    /// every visible message must be drawn and thus have a cached height.
    #[test]
    fn scrolled_list_has_no_gaps() {
        let mut h = app_with_messages(300);
        let ctx = egui::Context::default();
        frames(&mut h.app, &ctx, 3);
        let (inner, _content, _ask) = scroll_sizes(&h.app);

        // Middle, start, and very bottom of the list.
        let total = total_height(&h.app);
        for off in [0.0, inner, total * 0.5, total - inner, total - 1.0] {
            scroll_to(&mut h.app, off);
            // Two frames: the first fills the height cache, the second uses
            // it, so the check doesn't measure itself.
            frames(&mut h.app, &ctx, 2);
            let real_off = h.app.chat_offset_y;
            let ids: Vec<String> = h.app.messages["c0"].iter().map(|m| m.id.clone()).collect();
            let mut drawn = 0;
            for (i, id) in ids.iter().enumerate() {
                let top = h.app.msg_offsets[i];
                let bottom = h.app.msg_offsets[i + 1];
                if bottom <= real_off || top >= real_off + inner {
                    continue;
                }
                drawn += 1;
                assert!(
                    h.app.msg_heights.contains_key(id),
                    "сообщение {id} не нарисовано при прокрутке {real_off:.0} (верх {top:.0})"
                );
            }
            eprintln!("[TEST] offset={real_off:.0} видимых={drawn} всего замерено={}", h.app.msg_heights.len());
            assert!(drawn > 3, "при прокрутке {real_off:.0} видно всего {drawn} сообщений");
        }
    }

    /// Loading older history must not shift the view: the anchored message
    /// stays at the same screen position.
    #[test]
    fn prepending_history_keeps_the_view_in_place() {
        let mut h = app_with_messages(300);
        let ctx = egui::Context::default();
        frames(&mut h.app, &ctx, 3);

        // The user is reading mid-history.
        let half = total_height(&h.app) * 0.5;
        scroll_to(&mut h.app, half);
        frames(&mut h.app, &ctx, 3);
        let (anchor_id, dy) = h
            .app
            .chat_anchor
            .clone()
            .expect("вид должен держаться на сообщении, а не на номере строки");
        eprintln!("[TEST] якорь {anchor_id} на {dy:.1} пикселей выше верха окна");

        // A page of 50 older messages loaded.
        let old: Vec<Arc<ChatMessage>> =
            (1000..1050).map(|i| Arc::new(message(i))).collect();
        h.app.messages.get_mut("c0").unwrap().splice(..0, old);
        frames(&mut h.app, &ctx, 3);

        let msgs = &h.app.messages["c0"];
        let i = msgs
            .iter()
            .position(|m| m.id == anchor_id)
            .expect("сообщение, на котором держался вид, пропало из списка");
        let on_screen = h.app.msg_offsets[i] - h.app.chat_offset_y;
        eprintln!(
            "[TEST] после подгрузки: якорь на {on_screen:.1} пикселей выше верха окна (было {dy:.1})"
        );
        assert!(
            (on_screen - dy).abs() < 1.0,
            "вид уехал при подгрузке истории: было {dy:.1}, стало {on_screen:.1}"
        );
        assert_eq!(
            h.app.chat_anchor.as_ref().map(|a| a.0.as_str()),
            Some(anchor_id.as_str()),
            "якорь потерялся"
        );
    }

    /// A new message pulls the chat down only if the user was at the bottom.
    #[test]
    fn new_message_pulls_down_only_from_the_bottom() {
        let mut h = app_with_messages(300);
        let ctx = egui::Context::default();
        frames(&mut h.app, &ctx, 3);
        assert!(h.app.chat_at_bottom, "после открытия канала чат должен быть внизу");

        // 1. User at the bottom: a new message must keep them there.
        let mut m = message(300);
        m.id = "m0300".into();
        h.tx.send(ToApp::Message(m.clone())).unwrap();
        h.app.poll(&ctx);
        frames(&mut h.app, &ctx, 2);
        assert!(h.app.chat_at_bottom, "внизу новое сообщение должно тянуть вниз");
        // Use this frame's content height: `msg_offsets` predates the new
        // message's measurement and lags by the estimate error.
        let (inner, content, _ask) = scroll_sizes(&h.app);
        let bottom = content - inner;
        eprintln!("[TEST] внизу: offset={:.0}, низ={bottom:.0}", h.app.chat_offset_y);
        assert!(
            (h.app.chat_offset_y - bottom).abs() < 2.0,
            "внизу чат должен остаться внизу: offset={:.0}, низ={bottom:.0}",
            h.app.chat_offset_y
        );

        // 2. User scrolled up: a new message must not throw them down.
        let up = total_height(&h.app) * 0.4;
        scroll_to(&mut h.app, up);
        frames(&mut h.app, &ctx, 2);
        assert!(!h.app.chat_at_bottom, "прокрутка выше низа — это не низ");
        let before = h.app.chat_offset_y;
        let mut m2 = message(301);
        m2.id = "m0301".into();
        h.tx.send(ToApp::Message(m2)).unwrap();
        h.app.poll(&ctx);
        frames(&mut h.app, &ctx, 2);
        eprintln!(
            "[TEST] вверху: было {before:.0}, стало {:.0}",
            h.app.chat_offset_y
        );
        assert!(
            !h.app.scroll_to_bottom,
            "читающий историю не должен прыгать вниз"
        );
        assert!(
            (h.app.chat_offset_y - before).abs() < 2.0,
            "вид уехал на {:.0} пикселей при новом сообщении",
            (h.app.chat_offset_y - before).abs()
        );
    }

    /// Loading older messages must fire exactly one request, with `before`
    /// set to the oldest shown id.
    #[test]
    fn older_history_is_requested_once_with_oldest_id() {
        let mut h = app_with_messages(300);
        let ctx = egui::Context::default();
        frames(&mut h.app, &ctx, 3);
        // The first frames open the channel at the bottom and stay quiet.
        assert!(drain(&mut h.cmds).is_empty(), "внизу историю не просят");

        // Scrolled to the very start of history.
        scroll_to(&mut h.app, 0.0);
        frames(&mut h.app, &ctx, 3);

        let sent = drain(&mut h.cmds);
        assert_eq!(
            sent.len(),
            1,
            "к началу истории должно уйти ровно одно сообщение, ушло {sent:?}"
        );
        match &sent[0] {
            ToGateway::FetchHistory { channel_id, before } => {
                assert_eq!(channel_id, "c0");
                assert_eq!(
                    before.as_deref(),
                    Some("m0000"),
                    "`before` должен быть id самого старого"
                );
            }
            other => panic!("ожидался FetchHistory, пришло {other:?}"),
        }
        assert!(h.app.history_loading_more, "пока страница в пути, повторов быть не должно");
        // More frames at the same spot: no second request.
        frames(&mut h.app, &ctx, 3);
        assert!(drain(&mut h.cmds).is_empty(), "повторный запрос истории");

        // The page arrived marked "nothing further", so stop asking.
        h.tx
            .send(ToApp::HistoryMore {
                channel_id: "c0".into(),
                messages: (1000..1050).map(message).collect(),
                more: false,
            })
            .unwrap();
        h.app.poll(&ctx);
        frames(&mut h.app, &ctx, 3);
        assert!(h.app.history_exhausted, "после «дальше пусто» просить нечего");
        scroll_to(&mut h.app, 0.0);
        frames(&mut h.app, &ctx, 3);
        assert!(drain(&mut h.cmds).is_empty(), "истории больше нет, а запрос ушёл");
    }

    /// The same via the real channel-open path: `open_channel` requests the
    /// first page, the gateway answers separately. Exactly one page appears
    /// and no more is requested until the user scrolls up.
    #[test]
    fn opening_a_channel_fetches_exactly_one_page() {
        let (gw_tx, gw_rx) = mpsc::unbounded_channel::<ToApp>();
        let (app_tx, mut cmds) = mpsc::unbounded_channel();
        let mut app = App::new(gw_rx);
        app.connected = true;
        app.gw_started = true;
        app.to_gw = Some(app_tx);
        app.channels.push(ChatChannel {
            id: "c0".into(),
            name: "основной".into(),
            guild_id: None,
            channel_type: 0,
            topic: None,
            position: 0,
        });
        let ctx = egui::Context::default();

        // Click a channel.
        app.open_channel("c0");
        app.selected_channel = Some(0);
        frames(&mut app, &ctx, 3);

        let sent = drain(&mut cmds);
        eprintln!("[TEST] при открытии канала ушло запросов: {sent:?}");
        assert_eq!(sent.len(), 1, "открытие канала — это один запрос");
        match &sent[0] {
            ToGateway::FetchHistory { before, .. } => {
                assert!(before.is_none(), "первая страница идёт без `before`")
            }
            other => panic!("ожидался FetchHistory, пришло {other:?}"),
        }

        // The gateway replied with a page of 50 messages.
        let page: Vec<ChatMessage> = (0..50).map(message).collect();
        gw_tx
            .send(ToApp::History { channel_id: "c0".into(), messages: page, more: true })
            .unwrap();
        app.poll(&ctx);
        frames(&mut app, &ctx, 3);

        eprintln!(
            "[TEST] в канале {} сообщений, смещение {:.0}, внизу ли: {}",
            app.messages["c0"].len(),
            app.chat_offset_y,
            app.chat_at_bottom
        );
        assert_eq!(app.messages["c0"].len(), 50, "в канале должна быть одна страница");
        assert!(app.chat_at_bottom, "открытый канал показывает новое сообщение, а не начало");
        assert!(
            drain(&mut cmds).is_empty(),
            "пока не доскроллили вверх, следующая страница не нужна"
        );
    }

    /// Loads one page, not the whole channel: after reaching the start and
    /// receiving a page, further loading is the reader's decision. Otherwise
    /// the client hammers the API up to the cap.
    #[test]
    fn scrolling_up_loads_one_page_not_the_whole_channel() {
        let mut h = app_with_messages(50);
        let ctx = egui::Context::default();
        frames(&mut h.app, &ctx, 3);
        assert!(
            drain(&mut h.cmds).is_empty(),
            "при открытии канала история не должна грузиться сама"
        );

        // The reader scrolled to the very start.
        scroll_to(&mut h.app, 0.0);
        frames(&mut h.app, &ctx, 3);
        let first_page = drain(&mut h.cmds).len();
        eprintln!("[TEST] доскроллил вверх: запросов {first_page}");

        // Discord replies with a page and "more". The reader stays on the
        // same messages, so no new requests.
        let mut total = first_page;
        for round in 0..4 {
            h.tx
                .send(ToApp::HistoryMore {
                    channel_id: "c0".into(),
                    messages: (1000 + round * 50..1050 + round * 50).map(message).collect(),
                    more: true,
                })
                .unwrap();
            h.app.poll(&ctx);
            frames(&mut h.app, &ctx, 3);
            let got = drain(&mut h.cmds).len();
            total += got;
            eprintln!(
                "[TEST] раунд {round}: сообщений {}, запросов {got}, всего {total}, смещение {:.0}",
                h.app.messages["c0"].len(),
                h.app.chat_offset_y
            );
        }
        assert_eq!(
            total, 1,
            "одна прокрутка вверх должна стоить одну страницу, а запросов ушло {total}"
        );
    }

    /// Empty and tiny channels are normal; neither must crash or request
    /// history that doesn't exist.
    #[test]
    fn empty_and_tiny_channels_are_fine() {
        for n in [0, 1, 2] {
            let mut h = app_with_messages(n);
            let ctx = egui::Context::default();
            scroll_to(&mut h.app, 0.0);
            frames(&mut h.app, &ctx, 3);
            let (_inner, content, _ask) = scroll_sizes(&h.app);
            eprintln!("[TEST] {n} сообщений: скролл {content:.0}");
            assert!(content < 500.0, "{n} сообщений: скролл {content:.0} — список раздут");
            if n == 0 {
                assert!(h.app.chat_anchor.is_none(), "пустой канал не должен держать якорь");
            }
            let sent = drain(&mut h.cmds);
            assert!(sent.is_empty(), "в канале из {n} сообщений просить нечего, а ушло {sent:?}");
        }
    }
}

#[cfg(test)]
mod wheel_probe {
    use eframe::egui;
    use std::sync::Mutex;

    /// Offset of the last drawn scroll: egui state is private, so we capture
    /// it ourselves.
    static OFFSET: Mutex<Vec<f32>> = Mutex::new(Vec::new());

    /// A minimal ScrollArea without our code: does a wheel event fed through
    /// RawInput reach it? If not, the problem is how we feed egui events.
    #[test]
    fn plain_scroll_area_reacts_to_wheel() {
        let ctx = egui::Context::default();
        let screen = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0))),
            ..Default::default()
        };
        let mut shift = 0.0f32;
        let mut run = |n: usize, dy: f32| {
            for _ in 0..n {
                let mut input = screen.clone();
                input.events.push(egui::Event::PointerMoved(egui::Pos2::new(400.0 + shift, 300.0)));
                input.events.push(egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, dy),
                    modifiers: egui::Modifiers::default(),
                });
                shift = if shift >= 1.0 { 0.0 } else { shift + 0.5 };
                let _ = ctx.run(input, |ctx| {
                    let out = egui::CentralPanel::default()
                        .show(ctx, |ui| {
                            egui::ScrollArea::vertical()
                                .id_salt("probe")
                                .show(ui, |ui| {
                                    ui.allocate_space(egui::vec2(ui.available_width(), 5000.0));
                                })
                                .state
                                .offset
                                .y
                        })
                        .inner;
                    OFFSET.lock().unwrap().push(out);
                });
            }
        };
        run(1, 0.0);
        let before = OFFSET.lock().unwrap().last().copied().unwrap_or(0.0);
        run(20, -120.0);
        let after = OFFSET.lock().unwrap().last().copied().unwrap_or(0.0);
        eprintln!("[PROBE] смещение {before:.1} -> {after:.1}");
        assert!(
            (after - before).abs() > 10.0,
            "egui сам по себе не отреагировал на колесо: {before:.1} -> {after:.1}"
        );
    }
}
