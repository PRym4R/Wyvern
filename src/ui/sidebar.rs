use eframe::egui::{self, Color32, RichText};

use crate::app::App;
use crate::messages::ToGateway;
use crate::ui::ERROR_RED;

impl App {
    /// Clicking "@": clears the server and channel selection only.
    pub(crate) fn go_home(&mut self) {
        self.selected_guild = None;
        self.selected_channel = None;
    }
    /// Toggle the sidebar view: separate "Friends / DMs" button.
    pub(crate) fn set_friends_view(&mut self, show_friends: bool) {
        self.show_friends = show_friends;
    }
    pub(crate) fn draw_server_list(&mut self, ctx: &egui::Context) {
        let anim_time = if self.animations_enabled() { 0.22 } else { 0.0 };
        egui::SidePanel::left("servers")
            .resizable(false)
            .default_width(72.0)
            .exact_width(72.0)
            .frame(egui::Frame::new().fill(self.theme.bg))
            .show(ctx, |ui| {
                if let Some((win_end, _)) = crate::models::theme_gradient(self.theme_index) {
                    crate::ui::paint_vertical_gradient(
                        ui.painter(),
                        ui.max_rect(),
                        self.theme.bg,
                        win_end,
                    );
                }
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    let home_sel = ui.ctx().animate_bool_with_time_and_easing(
                        egui::Id::new("home_sel"),
                        self.selected_guild.is_none(),
                        anim_time,
                        crate::ui::ease_out_cubic,
                    );
                    let home_btn = ui.add_sized(
                        [48.0, 48.0],
                        egui::Button::new(RichText::new("@").size(20.0).color(Color32::WHITE))
                            .fill(self.theme.input_bg.lerp_to_gamma(self.theme.accent, home_sel))
                            .corner_radius(24.0),
                    );
                    if home_btn.clicked() {
                        self.go_home();
                    }
                    home_btn.on_hover_text("Direct messages");
                });
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);

                // Move the guild list out of self to avoid cloning it every frame.
                let guilds = std::mem::take(&mut self.guilds);
                // Test-only probe: how many guilds were left in self during render.
                #[cfg(test)]
                {
                    self.probe_guilds_in_render = self.guilds.len();
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (i, guild) in guilds.iter().enumerate() {
                        ui.vertical_centered(|ui| {
                            let guild_sel = ui.ctx().animate_bool_with_time_and_easing(
                                egui::Id::new(("guild_sel", &guild.id)),
                                self.selected_guild == Some(i),
                                anim_time,
                                crate::ui::ease_out_cubic,
                            );
                            let icon_tex = guild.icon.as_ref()
                                .and_then(|h| self.download_guild_icon(ctx, &guild.id, h));
                            let btn = if let Some(tex) = icon_tex {
                                let img = tex.clone();
                                ui.add_sized(
                                    [48.0, 48.0],
                                    egui::ImageButton::new(
                                        egui::load::SizedTexture::new(img.id(), egui::vec2(48.0, 48.0))
                                    ).corner_radius(24.0),
                                )
                            } else {
                                let label: String = guild.name.chars().take(2).collect();
                                ui.add_sized(
                                    [48.0, 48.0],
                                    egui::Button::new(RichText::new(&label).size(16.0).color(Color32::WHITE))
                                        .fill(self.theme.input_bg.lerp_to_gamma(self.theme.accent, guild_sel))
                                        .corner_radius(24.0),
                                )
                            };
                            if btn.clicked() {
                                self.selected_guild = Some(i);
                                self.selected_channel = None;
                                self.show_friends = false;
                            }
                            if guild_sel > 0.001 {
                                let bar = egui::Rect::from_center_size(
                                    egui::pos2(btn.rect.left() - 6.0, btn.rect.center().y),
                                    egui::vec2(3.0, 8.0 + 20.0 * guild_sel),
                                );
                                ui.painter().rect_filled(
                                    bar,
                                    1.5,
                                    self.theme.accent.gamma_multiply(guild_sel),
                                );
                            }
                            let (unread, mentions) = self.guild_badge(&guild.id);
                            if unread > 0 {
                                let color = if mentions > 0 { ERROR_RED } else { self.theme.text };
                                let text = if mentions > 0 { mentions } else { unread };
                                let r = 9.0;
                                let center = btn.rect.right_top() + egui::vec2(-r, r);
                                ui.painter().circle_filled(center, r, color);
                                ui.painter().text(
                                    center,
                                    egui::Align2::CENTER_CENTER,
                                    text.to_string(),
                                    egui::FontId::proportional(11.0),
                                    Color32::BLACK,
                                );
                            }
                        });
                        ui.add_space(6.0);
                    }
                });
                // Put the guild list back after the frame.
                self.guilds = guilds;

                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    let ubtn = ui.add_sized(
                        [48.0, 48.0],
                        egui::Button::new(RichText::new("◐").size(20.0).color(Color32::WHITE))
                            .fill(self.theme.input_bg)
                            .corner_radius(24.0),
                    );
                    if ubtn.clicked() {
                        let count = crate::models::THEME_NAMES.len() as u8;
                        self.theme_index = (self.theme_index + 1) % count;
                        // Apply now so the rest of this frame already uses it.
                        self.theme = crate::models::theme_by_index(self.theme_index);
                        self.write_settings();
                        ui.ctx().request_repaint();
                    }
                    let theme_name = crate::models::THEME_NAMES
                        .get(self.theme_index as usize)
                        .copied()
                        .unwrap_or("Dark");
                    ubtn.on_hover_text(format!("Theme: {theme_name} (click to cycle)"));

                    let sbtn = ui.add_sized(
                        [48.0, 42.0],
                        egui::Button::new(RichText::new("⚙").size(18.0).color(Color32::WHITE))
                            .fill(self.theme.input_bg)
                            .corner_radius(21.0),
                    );
                    if sbtn.clicked() {
                        self.settings_open = true;
                    }
                    sbtn.on_hover_text("Settings");

                    let abtn = ui.add_sized(
                        [48.0, 42.0],
                        egui::Button::new(RichText::new("👤").size(18.0).color(Color32::WHITE))
                            .fill(if self.connected { self.theme.accent } else { self.theme.input_bg })
                            .corner_radius(21.0),
                    );
                    let popup_id = egui::Id::new("account_switcher");
                    if abtn.clicked() {
                        ui.close_menu();
                        ui.memory_mut(|m| m.toggle_popup(popup_id));
                    }
                    abtn.clone().on_hover_text(&self.username);
                    egui::popup_below_widget(ui, popup_id, &abtn, egui::PopupCloseBehavior::CloseOnClickOutside, |ui| {
                        ui.set_min_width(190.0);
                        ui.label(RichText::new("Accounts").strong().size(12.0).color(self.theme.text_secondary));
                        ui.add_space(4.0);
                        ui.separator();
                        let mut switch_to: Option<String> = None;
                        // Iterate by index; only the clicked row's token gets cloned.
                        let account_count = self.saved_accounts.len();
                        #[cfg(test)]
                        {
                            // Test-only probe: how many times the list was cloned.
                            self.probe_accounts_cloned = 0;
                        }
                        for i in 0..account_count {
                            let Some(acc) = self.saved_accounts.get(i) else { continue };
                            let label = if acc.username.is_empty() {
                                self.mask_token(&acc.token)
                            } else {
                                acc.username.clone()
                            };
                            let is_active = self.active_index == Some(i);
                            let text = if is_active {
                                format!("{} ✓", label)
                            } else {
                                label
                            };
                            if ui.add(egui::Button::new(RichText::new(text).size(13.0)
                                .color(if is_active { Color32::BLACK } else { self.theme.text }))
                                .fill(if is_active { self.theme.accent } else { self.theme.input_bg })
                                .min_size(egui::vec2(180.0, 30.0))).clicked() && !is_active
                            {
                                if let Some(acc) = self.saved_accounts.get(i) {
                                    switch_to = Some(acc.token.clone());
                                }
                            }
                        }
                        if let Some(t) = switch_to {
                            self.switch_account(t);
                            ui.close_menu();
                        }
                    });
                });
            });
    }
    /// One channel row with its unread badge. Text and announcement channels
    /// open on click; a voice channel is shown greyed out and non-clickable
    /// (we don't connect to voice).
    fn draw_channel_row(&mut self, ui: &mut egui::Ui, i: usize) {
        let Some(ch) = self.channels.get(i) else { return };
        let voice = ch.channel_type == 2;
        let anim_time = if self.animations_enabled() { 0.22 } else { 0.0 };
        let prefix = match ch.channel_type {
            2 => "🔊",
            5 => "📢",
            _ => "#",
        };
        let is_sel = self.selected_channel == Some(i);
        let sel = ui.ctx().animate_bool_with_time_and_easing(
            egui::Id::new(("chan_sel", &ch.id)),
            is_sel,
            anim_time,
            crate::ui::ease_out_cubic,
        );
        let (unread, mentions) = self.channel_badge(&ch.id);
        let dot = unread == 0 && mentions == 0 && self.unread_dot.contains(&ch.id);
        let label = if mentions > 0 {
            format!("{} {}  @{}", prefix, ch.name, mentions)
        } else if unread > 0 {
            format!("{} {}  ({})", prefix, ch.name, unread)
        } else if dot {
            format!("{} {}  •", prefix, ch.name)
        } else {
            format!("{} {}", prefix, ch.name)
        };
        let base = if mentions > 0 {
            Color32::WHITE
        } else if unread > 0 || dot {
            self.theme.text
        } else {
            self.theme.text_secondary
        };
        let mut text = RichText::new(&label)
            .size(14.0)
            .color(base.lerp_to_gamma(Color32::WHITE, sel));
        if is_sel || mentions > 0 {
            text = text.strong();
        }
        let button = egui::Button::new(text)
            .fill(Color32::TRANSPARENT.lerp_to_gamma(self.theme.accent, sel));
        // Voice rows stay visible but inert, so they don't pretend to open.
        let response = ui
            .add_enabled_ui(!voice, |ui| {
                ui.add_sized([ui.available_width(), 32.0], button)
            })
            .inner;
        if response.clicked() {
            let cid = ch.id.clone();
            self.selected_channel = Some(i);
            self.scroll_to_bottom = true;
            self.open_channel(&cid);
        }
    }
    pub(crate) fn draw_channel_list(&mut self, ctx: &egui::Context) {
        let panel = egui::SidePanel::left("channels")
            .resizable(true)
            .default_width(240.0)
            .min_width(180.0)
            // Cap the panel width; otherwise it stretches to the window and crushes the input field.
            .max_width(360.0)
            .frame(egui::Frame::new().fill(self.theme.panel_bg));
        let resp = panel.show(ctx, |ui| {
                if let Some((win_end, _)) = crate::models::theme_gradient(self.theme_index) {
                    crate::ui::paint_vertical_gradient(
                        ui.painter(),
                        ui.max_rect(),
                        self.theme.panel_bg,
                        win_end,
                    );
                }
                ui.set_min_width(ui.available_width());

                match self.selected_guild {
                    Some(guild_idx) => {
                        // Borrow the guild by reference instead of cloning it each frame.
                        let Some(guild) = self.guilds.get(guild_idx) else {
                            self.selected_guild = None;
                            return;
                        };
                        ui.add_space(12.0);
                        ui.label(RichText::new(&guild.name).strong().size(15.0).color(self.theme.text));
                        ui.add_space(4.0);
                        ui.separator();
                        ui.add_space(4.0);

                        // Group text channels under their categories; the layout
                        // is computed before the scroll closure so the `guild`
                        // borrow ends before we touch `self` mutably.
                        let (loose, groups) = self.guild_channel_layout(&guild.id);
                        #[cfg(test)]
                        {
                            // Test-only probe: how many times the guild was cloned.
                            self.probe_channel_guild_cloned = 0;
                        }
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            for i in loose {
                                self.draw_channel_row(ui, i);
                            }
                            for (ci, kids) in groups {
                                let Some((cat_id, cat_name)) = self
                                    .channels
                                    .get(ci)
                                    .map(|c| (c.id.clone(), c.name.clone()))
                                else {
                                    continue;
                                };
                                // egui draws the fold triangle itself, so the
                                // header never depends on a font having a glyph.
                                let header = RichText::new(cat_name.to_uppercase())
                                    .size(12.0)
                                    .color(self.theme.text_secondary);
                                egui::CollapsingHeader::new(header)
                                    .id_salt(("category", cat_id))
                                    .default_open(true)
                                    .show(ui, |ui| {
                                        for i in kids {
                                            self.draw_channel_row(ui, i);
                                        }
                                    });
                            }
                        });
                    }
                    None => {
                        // "Friends / DMs" toggle, separate from the "@" home button.
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            for (label, want_friends) in [("Friends", true), ("DMs", false)] {
                                let active = self.show_friends == want_friends;
                                let color = if active { Color32::WHITE } else { self.theme.text_secondary };
                                let fill = if active { self.theme.accent } else { Color32::TRANSPARENT };
                                if ui
                                    .add_sized(
                                        [ui.available_width() / 2.0 - 4.0, 28.0],
                                        egui::Button::new(RichText::new(label).size(13.0).color(color)).fill(fill),
                                    )
                                    .clicked()
                                {
                                    self.set_friends_view(want_friends);
                                }
                            }
                        });
                        if self.show_friends {
                            ui.add_space(12.0);
                            ui.label(RichText::new("Friends").strong().size(15.0).color(self.theme.text));
                            ui.add_space(8.0);
                            ui.label(RichText::new(format!("{} friends", self.friends.len()))
                                .size(12.0).color(self.theme.text_secondary));
                            ui.add_space(4.0);
                            ui.separator();
                            // Iterate by index; clone only the rows actually needed.
                            let friend_count = self.friends.len();
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                for i in 0..friend_count {
                                    let (label, username) = match self.friends.get(i) {
                                        Some(f) => (format!("@{}", f.username), f.username.clone()),
                                        None => continue,
                                    };
                                    let response = ui.add_sized(
                                        [ui.available_width(), 32.0],
                                        egui::Button::new(
                                            RichText::new(&label).size(14.0).color(self.theme.text)
                                        ).fill(Color32::TRANSPARENT),
                                    );
                                    if response.clicked() {
                                        if let Some(f) = self.friends.get(i) {
                                            if f.id.is_empty() {
                                                continue;
                                            }
                                            let fid = f.id.clone();
                                            self.push_debug(format!("Clicked friend '{}' (id {})", username, &fid[..fid.len().min(12)]));
                                            self.send_cmd(ToGateway::OpenDM { user_id: fid });
                                        }
                                    }
                                }
                            });
                        } else {
                            ui.add_space(12.0);
                            ui.label(RichText::new("DMs").strong().size(15.0).color(self.theme.text));
                            ui.add_space(4.0);
                            ui.separator();
                            let dm_order = self.dm_indices();
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                for i in dm_order {
                                    let (cid, cname) = match self.channels.get(i) {
                                        Some(ch)
                                            if ch.guild_id.is_none()
                                                && (ch.channel_type == 1 || ch.channel_type == 3) =>
                                        {
                                            (ch.id.clone(), ch.name.clone())
                                        }
                                        _ => continue,
                                    };
                                    let is_sel = self.selected_channel
                                        .and_then(|s| self.channels.get(s))
                                        .map(|c| c.id == cid)
                                        .unwrap_or(false);
                                    let (unread, mentions) = self.channel_badge(&cid);
                                    let dot = unread == 0 && mentions == 0 && self.unread_dot.contains(&cid);
                                    let label = if mentions > 0 {
                                        format!("{}  @{}", cname, mentions)
                                    } else if unread > 0 {
                                        format!("{}  ({})", cname, unread)
                                    } else if dot {
                                        format!("{}  •", cname)
                                    } else {
                                        cname.clone()
                                    };
                                    let mut text = RichText::new(&label).size(14.0);
                                    text = if is_sel || mentions > 0 {
                                        text.color(Color32::WHITE).strong()
                                    } else if unread > 0 || dot {
                                        text.color(self.theme.text)
                                    } else {
                                        text.color(self.theme.text_secondary)
                                    };
                                    let response = ui.add_sized(
                                        [ui.available_width(), 32.0],
                                        egui::Button::new(text)
                                            .fill(if is_sel { self.theme.accent } else { Color32::TRANSPARENT }),
                                    );
                                    if response.clicked() {
                                        self.selected_channel = Some(i);
                                        self.scroll_to_bottom = true;
                                        self.open_channel(&cid);
                                    }
                                }
                            });
                        }
                    }
                }
            });
        // Remember and log the panel width only when it changes.
        let panel_w = resp.response.rect.width();
        if (panel_w - self.channel_panel_w).abs() > 0.5 {
            self.channel_panel_w = panel_w;
            self.push_debug(format!("CHANNELS_PANEL: w={panel_w:.0}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The "@" home button only clears the server selection.
    #[test]
    fn home_button_only_drops_the_server_selection() {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(rx);
        app.selected_guild = Some(2);
        app.selected_channel = Some(5);
        app.show_friends = false;

        app.go_home();

        assert!(app.selected_guild.is_none(), "выделение сервера должно сброситься");
        assert!(app.selected_channel.is_none(), "выделение канала должно сброситься");
        assert!(!app.show_friends, "«@» не должен переключать вид «Друзья / личные сообщения»");
    }

    /// The view is toggled by its own dedicated button.
    #[test]
    fn friends_view_has_its_own_switch() {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(rx);
        app.selected_guild = Some(0);
        app.selected_channel = Some(1);

        app.set_friends_view(true);
        assert!(app.show_friends);
        app.set_friends_view(false);
        assert!(!app.show_friends);
        // Toggling the view doesn't drop the open channel.
        assert_eq!(app.selected_channel, Some(1), "переключатель вида не должен сбрасывать выбор");
    }

    /// A freshly selected server must ease in, not snap.
    #[test]
    fn server_selection_eases_in_across_frames() {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(rx);
        app.guilds.push(crate::models::Guild {
            id: "g0".into(),
            name: "Guild".into(),
            icon: None,
        });

        let ctx = egui::Context::default();
        let input = |t: f64| egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(300.0, 600.0),
            )),
            time: Some(t),
            ..Default::default()
        };
        let bar_height = |out: &egui::FullOutput| {
            out.shapes
                .iter()
                .filter_map(|cs| match &cs.shape {
                    egui::Shape::Rect(r) if (r.rect.width() - 3.0).abs() < 0.01 => {
                        Some(r.rect.height())
                    }
                    _ => None,
                })
                .fold(0.0_f32, f32::max)
        };

        app.selected_guild = None;
        let settle = ctx.run(input(0.0), |ctx| app.draw_server_list(ctx));
        assert_eq!(bar_height(&settle), 0.0, "без выбора бара быть не должно");

        app.selected_guild = Some(0);
        let early = bar_height(&ctx.run(input(0.016), |ctx| app.draw_server_list(ctx)));
        let later = bar_height(&ctx.run(input(0.05), |ctx| app.draw_server_list(ctx)));
        assert!(early > 8.0, "бар должен появиться сразу: {early}");
        assert!(early < 28.0, "бар не должен быть максимальным на первом кадре: {early}");
        assert!(later > early, "бар должен расти: {early} -> {later}");

        app.settings.animations = Some(false);
        app.selected_guild = None;
        let _ = ctx.run(input(0.1), |ctx| app.draw_server_list(ctx));
        app.selected_guild = Some(0);
        let snapped = bar_height(&ctx.run(input(0.12), |ctx| app.draw_server_list(ctx)));
        assert!(
            (snapped - 28.0).abs() < 0.01,
            "с выключенными анимациями бар должен появляться сразу: {snapped}"
        );
    }

    /// The server panel must not clone the guild list every frame.
    #[test]
    fn drawing_the_server_list_does_not_clone_the_guilds() {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(rx);
        for i in 0..200 {
            app.guilds.push(crate::models::Guild {
                id: format!("g{i}"),
                name: format!("Сервер {i}"),
                icon: None,
            });
        }
        app.probe_guilds_in_render = usize::MAX;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1052.0, 1054.0),
            )),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let _ = ctx.run(input, |ctx| app.draw_server_list(ctx));

        assert_eq!(
            app.probe_guilds_in_render, 0,
            "панель серверов копирует список серверов: в self осталось {}",
            app.probe_guilds_in_render
        );
        assert_eq!(app.guilds.len(), 200, "список серверов должен вернуться на место");
    }

    /// The account switcher must not clone the whole account list every frame.
    #[test]
    fn drawing_the_account_switcher_does_not_clone_the_accounts() {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(rx);
        for i in 0..50 {
            app.saved_accounts.push(crate::models::StoredAccount {
                token: format!("tok{i}"),
                username: format!("user{i}"),
            });
        }
        app.probe_accounts_cloned = usize::MAX;

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1052.0, 1054.0),
            )),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        // Open the account popup, otherwise it isn't drawn.
        ctx.memory_mut(|m| m.toggle_popup(egui::Id::new("account_switcher")));
        let _ = ctx.run(input, |ctx| app.draw_server_list(ctx));

        assert_eq!(
            app.probe_accounts_cloned, 0,
            "панель аккаунтов копирует список аккаунтов ({} раз)",
            app.probe_accounts_cloned
        );
        assert_eq!(app.saved_accounts.len(), 50, "список аккаунтов должен остаться на месте");
    }

    /// The channel list must not clone the selected guild every frame.
    #[test]
    fn drawing_the_channel_list_does_not_clone_the_guild() {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(rx);
        app.guilds.push(crate::models::Guild {
            id: "g1".into(),
            name: "Сервер".into(),
            icon: None,
        });
        for i in 0..10 {
            app.channels.push(crate::models::ChatChannel {
                id: format!("c{i}"),
                name: format!("канал-{i}"),
                guild_id: Some("g1".into()),
                channel_type: 0,
                topic: None,
                position: i,
                parent_id: None,
                last_message_id: None,
            });
        }
        app.selected_guild = Some(0);
        app.probe_channel_guild_cloned = usize::MAX;

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1052.0, 1054.0),
            )),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let _ = ctx.run(input, |ctx| app.draw_channel_list(ctx));

        assert_eq!(
            app.probe_channel_guild_cloned, 0,
            "список каналов копирует выбранную гильдию"
        );
        assert_eq!(app.guilds.len(), 1, "гильдия должна остаться в списке");
    }
}
