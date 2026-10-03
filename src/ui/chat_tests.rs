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

    /// The height cache must not accumulate messages that scrolled out of the
    /// window: it once grew to a cap and then wiped everything, re-measuring
    /// the whole channel. Only the current window should stay cached.
    #[test]
    fn height_cache_drops_messages_that_left_the_window() {
        let mut h = app_with_messages(300);
        let ctx = egui::Context::default();
        // Open at the top and remember what was measured there.
        scroll_to(&mut h.app, 0.0);
        frames(&mut h.app, &ctx, 2);
        let top_ids: Vec<String> = h.app.msg_heights.keys().cloned().collect();
        assert!(!top_ids.is_empty(), "наверху нечего измерять");

        // Jump to the bottom: the top window is far away and must be gone.
        let (inner, _content, _ask) = scroll_sizes(&h.app);
        let far = (total_height(&h.app) - inner).max(0.0);
        scroll_to(&mut h.app, far);
        frames(&mut h.app, &ctx, 2);

        let kept = top_ids
            .iter()
            .filter(|id| h.app.msg_heights.contains_key(*id))
            .count();
        assert_eq!(
            kept, 0,
            "высоты ушедших из окна сообщений остались в кэше: {kept} из {}",
            top_ids.len()
        );
        assert!(
            h.app.msg_heights.len() < 100,
            "кэш высот разросся: {} записей",
            h.app.msg_heights.len()
        );
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

#[cfg(test)]
mod menu_tests {
    use eframe::egui;
    use tokio::sync::mpsc;

    use super::*;
    use crate::messages::ToApp;
    use crate::models::ChatMessage;

    /// A minimal message; only the fields the menu looks at matter.
    fn message(id: &str, content: &str, own: bool) -> ChatMessage {
        ChatMessage {
            id: id.into(),
            channel_id: "c0".into(),
            author_id: "u1".into(),
            author_name: "user".into(),
            author_avatar: None,
            nickname: None,
            content: content.into(),
            timestamp: String::new(),
            attachments: vec![],
            embeds: vec![],
            is_own: own,
        }
    }

    fn app() -> App {
        let (_tx, rx) = mpsc::unbounded_channel::<ToApp>();
        App::new(rx)
    }

    /// Copying a message puts its text into egui's clipboard output.
    #[test]
    fn copy_action_puts_text_on_the_clipboard() {
        let mut app = app();
        let msg = message("m1", "привет мир", false);
        let ctx = egui::Context::default();
        let out = ctx.run(egui::RawInput::default(), |ctx| {
            app.run_message_action(ctx, MessageAction::Copy, &msg);
        });
        let copied: Vec<String> = out
            .platform_output
            .commands
            .iter()
            .filter_map(|c| match c {
                egui::OutputCommand::CopyText(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(copied, vec!["привет мир".to_string()]);
    }

    /// A message without text (e.g. image only) has nothing to copy.
    #[test]
    fn copy_action_ignores_empty_content() {
        let mut app = app();
        let msg = message("m1", "", false);
        let ctx = egui::Context::default();
        let out = ctx.run(egui::RawInput::default(), |ctx| {
            app.run_message_action(ctx, MessageAction::Copy, &msg);
        });
        let copied = out
            .platform_output
            .commands
            .iter()
            .filter(|c| matches!(c, egui::OutputCommand::CopyText(_)))
            .count();
        assert_eq!(copied, 0, "пустое сообщение нечего копировать");
    }

    /// The menu offers everything for a confirmed own message, but hides
    /// edit/delete for others' messages and everything id-based for an
    /// unconfirmed send.
    #[test]
    fn menu_reflects_which_actions_apply() {
        let app = app();
        let own = app.message_menu(&message("m1", "текст", true));
        assert!(own.reply && own.edit && own.delete && own.copy, "own: {own:?}");

        let other = app.message_menu(&message("m1", "текст", false));
        assert!(other.reply && other.copy, "other reply/copy: {other:?}");
        assert!(!other.edit && !other.delete, "other edit/delete: {other:?}");

        let unconfirmed = app.message_menu(&message("", "текст", true));
        assert!(!unconfirmed.reply && !unconfirmed.edit && !unconfirmed.delete);
        assert!(unconfirmed.copy, "unconfirmed copy: {unconfirmed:?}");

        let empty = app.message_menu(&message("m1", "", false));
        assert!(!empty.copy, "empty copy: {empty:?}");
    }

    /// Choosing "Reply" points the composer at the chosen message.
    #[test]
    fn choosing_reply_sets_the_composer_target() {
        let mut app = app();
        let msg = message("m1", "исходное сообщение", false);
        let ctx = egui::Context::default();
        app.run_message_action(&ctx, MessageAction::Reply, &msg);

        let target = app.reply_to.clone().expect("reply должен быть выбран");
        assert_eq!(target.message_id, "m1");
        assert_eq!(target.author_name, "user");
        assert_eq!(target.preview, "исходное сообщение");
    }
}
