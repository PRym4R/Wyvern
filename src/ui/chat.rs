use std::sync::Arc;

use eframe::egui::{self, Color32, RichText};

use crate::app::App;
use crate::models::ChatMessage;
use crate::ui::attachments::{display_size, for_each_image, reserved_size};
use crate::ui::ERROR_RED;

/// Насколько за границу окна рисуем сообщения. Запас нужен, чтобы при быстром
/// скролле не появлялись пустые полосы: между колёсами мыши egui успевает
/// показать кадр, и всё, что в него попало, должно быть нарисовано.
const OVERSCAN: f32 = 600.0;
/// На сколько близко к верху списка просим более старые сообщения.
const LOAD_MORE_AT_TOP: f32 = 48.0;
/// Строка «Loading older messages…». Место под неё резервируем всегда: если
/// строка появится или исчезнет, весь список сдвинется.
const MORE_ROW_H: f32 = 20.0;

// Константы оценки высоты сообщения. Держат в одном месте с теми же числами,
// что задаёт отрисовка, — иначе оценка будет врать всегда в одну сторону.
// Числа сняты замером на живом egui: реальная высота строки шрифта 14 pt —
// 16.1, а всё, что вокруг неё (поля пузыря, имя с временем, зазор после
// сообщения), — 48 пикселей. Раньше тут стояло 18 и 63, и список был
// завышен на 14%: полоса прокрутки врала, а по мере прокрутки длина списка
// менялась на сотни пикселей — то самое дёрганье, на которое жалуются.
const NAME_ROW_H: f32 = 18.0;
const LINE_H: f32 = 16.5;
const BUBBLE_BASE_H: f32 = 30.0;
/// Сообщение другого автора никогда не короче аватара с одной строкой: рядом
/// с именем встаёт 36-пиксельный аватар, и однострочное сообщение от этого
/// выше. Замерено: 72 пикселя.
const OTHER_MIN_H: f32 = 72.0;
/// Зазор после картинки внутри сообщения.
const IMAGE_GAP: f32 = 6.0;
/// Средняя ширина символа текста 14 pt. Только для оценки: как только
/// сообщение попадает на экран, его высоту мы меряем по-настоящему.
const CHAR_W: f32 = 7.0;
/// Потолок кэша высот. Сообщения уходят из списка (потолок на канал), а ключи
/// по id остались бы — на всякий случай чистим целиком.
const MAX_HEIGHT_CACHE: usize = 4096;

/// Сколько сообщений нужно обойти, чтобы найти видимые. Список уже отсортирован
/// по высоте (точнее, по префиксным суммам), поэтому это деление пополам, а не
/// проход по всем сообщениям канала.
pub(crate) fn visible_window(offsets: &[f32], offset: f32, viewport: f32) -> (usize, usize) {
    let n = offsets.len().saturating_sub(1);
    if n == 0 {
        return (0, 0);
    }
    let top = offset - OVERSCAN;
    let bottom = offset + viewport + OVERSCAN;
    // Первое сообщение, верх которого ещё выше окна: оно тоже попадает в
    // кадр, потому что OVERSCAN.
    let first = offsets[..n].partition_point(|v| *v <= top).min(n - 1);
    // Первое сообщение, верх которого ушёл за нижнюю границу окна.
    let end = offsets.partition_point(|v| *v < bottom).max(first + 1).min(n);
    (first, end)
}

/// Сколько строк займёт текст такой длины в поле такой ширины.
fn estimate_lines(chars: usize, text_w: f32) -> f32 {
    if text_w <= 1.0 {
        return 1.0;
    }
    ((chars as f32 * CHAR_W) / text_w).ceil().max(1.0)
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
    /// Своё ли это сообщение. От нашего и от чужого пузырь разный, а значит
    /// разная и оценка высоты.
    pub(crate) fn is_own_msg(&self, msg: &ChatMessage) -> bool {
        msg.is_own || (!self.user_id.is_empty() && msg.author_id == self.user_id)
    }
    /// Высота сообщения: измеренная после отрисовки, а для ещё не нарисованных
    /// — оценка по длине текста. Оценка нужна, чтобы вообще знать, где
    /// кончается видимая часть списка: без неё виртуализация не сможет решить,
    /// какие сообщения рисувать.
    fn msg_height(&mut self, msg: &ChatMessage, width: f32) -> f32 {
        // Сообщение без id (наше собственное, пока Discord не подтвердил
        // отправку) не кэшируем: id у таких пустой, ключ бы совпал.
        if !msg.id.is_empty() {
            if let Some(h) = self.msg_heights.get(&msg.id) {
                return *h;
            }
        }
        self.estimate_msg_height(msg, width)
    }
    /// Высота сообщения «на глаз». Ошибка здесь почти не страшна: она уходит,
    /// как только сообщение попадает в кадр и его реальная высота попадает в
    /// кэш. Но когда ошибка систематическая и в одну сторону (как было: плюс
    /// 14% на каждом сообщении), длина списка плывёт по мере прокрутки, и
    /// полоса прокрутки под ней едет. Поэтому константы выше сняты замером.
    fn estimate_msg_height(&mut self, msg: &ChatMessage, width: f32) -> f32 {
        let own = self.is_own_msg(msg);
        // Ширина текста внутри пузыря: у своего сообщения она ограничена
        // долей ширины чата, у чужого ещё и занята аватаром.
        let text_w = if own {
            (width * 0.75).clamp(160.0, 480.0) - 21.0
        } else {
            width - 65.0
        };
        let lines = estimate_lines(self.display_content(msg).chars().count(), text_w);
        let mut images = 0.0f32;
        for_each_image(msg, |url, known| {
            // Из кэша высота известна точно; иначе берём размер, который
            // Discord прислал вместе со ссылкой, — он тоже точный.
            let h = match self.image_cache.get(url) {
                Some(img) => display_size(img.size_vec2()).y,
                None => reserved_size(known).y,
            };
            images += h + IMAGE_GAP;
        });
        let total = BUBBLE_BASE_H + NAME_ROW_H + lines * LINE_H + images;
        if own { total } else { total.max(OTHER_MIN_H) }
    }
    /// Прокрутка, которой надо открыть кадр. Если вид держится на сообщении с
    /// прошлого кадра, считаем его положение заново: так список может
    /// меняться (подгрузилась история вверх, у сообщения уточнилась высота),
    /// а глаза остаются на том же месте.
    fn anchored_offset(&self, msgs: &[Arc<ChatMessage>], offsets: &[f32]) -> Option<f32> {
        let (id, dy) = self.chat_anchor.as_ref()?;
        let i = msgs.iter().position(|m| &m.id == id)?;
        Some((offsets[i] - dy).max(0.0))
    }
    /// Насколько близко смещение должно быть к низу, чтобы считать, что чат
    /// внизу. Порог крошечный: пока пользователь крутит колесо, смещение
    /// меняется на единицы пикселей за кадр, и с большим порогом чат
    /// не отпускал бы его от низа — на любой сдвиг его тут же тянуло обратно.
    const BOTTOM_SLACK: f32 = 1.0;
    /// Держим ли низ. Пока пользователь внизу, список должен оставаться внизу
    /// сам: подгрузилась история, уточнилась высота сообщения, пришла
    /// картинка — всё это меняет длину списка, и если не пересчитывать низ
    /// каждый кадр, чат сам отползает от последнего сообщения и больше не
    /// возвращается: новое сообщение его уже не утянет.
    fn keep_at_bottom(&self) -> bool {
        self.scroll_to_bottom || self.chat_at_bottom
    }
    pub(crate) fn draw_main_chat(&mut self, ctx: &egui::Context) {
        // Список канала уезжает из карты на время кадра, чтобы рисовать по
        // владеющему `Vec`: копия всех `Arc` канала делалась на каждом кадре
        // (Т-3). Возвращается на место в конце кадра.
        let (taken_channel, msgs) = self.take_channel_messages();

        // Аватары отдельно prefetch'ить не нужно: каждая строка сообщения и
        // так достаёт свой аватар (и качает, если его нет). Отдельный проход
        // только мешал: на нём на каждый кадр выделялось по строке-ключу на
        // каждое сообщение канала.

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
            // Список уже забран, поэтому его длину берём у себя: для открытого
            // канала это и есть число сообщений в карте.
            let stored = msgs.len();
            self.push_debug(format!("RENDER: sel='{}' sel_cid='{}' msgs={} stored={} total_channels={} screen={}x{}", channel_label.clone().unwrap_or_else(|| "none".into()), sel_cid, msgs.len(), stored, n_channels, srect.width().round() as i32, srect.height().round() as i32));
        }

        match channel_label {
            Some(_) => {
                let sel_id = sel_cid.clone();
                let stick = self.keep_at_bottom();
                // Буфер префиксных сумм на время кадра живёт отдельно от self:
                // иначе нельзя одновременно читать сообщения (это ссылка на
                // self) и писать в тот же буфер. Высоты же кэшируются по id
                // сообщения, поэтому вставка в начало списка при подгрузке
                // истории вверх ничего не ломает.
                let mut offsets: Vec<f32> = std::mem::take(&mut self.msg_offsets);
                // Ширина содержимого известна только с прошлого кадра: считаем
                // список до того, как egui его покажет.
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
                // Когда чат сам уезжает вниз (первая страница истории, новое
                // сообщение или просто уточнившаяся высота), вид ещё не выбран
                // — держаться не за что. Якорь здесь только вредит: он закрепил
                // бы прокрутку там, где она была до перемотки, и новое
                // сообщение так и не показалось бы.
                let hint = if stick { None } else { self.anchored_offset(&msgs, &offsets) };
                // Открываем сразу на низу: сколько места останется под список,
                // известно заранее (полоса прокрутки высоту не ест). Иначе кадр,
                // который перематывает вниз, рисовал бы верх канала.
                let inner = if self.chat_inner_h > 1.0 {
                    self.chat_inner_h
                } else {
                    ctx.available_rect().height()
                };
                let offset = if stick {
                    // Просим низ не по оценке высот, а с поправкой на то,
                    // насколько оценка промахнулась в прошлом кадре. Оценка
                    // на длинном списке врёт на десятки пикселей, и без
                    // поправки «низ» уезжал вверх: последнее сообщение
                    // оказывалось на 35 px выше края, а `chat_at_bottom` гас —
                    // после этого новое сообщение уже не тянуло чат вниз.
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

                        // Отказ отправки виден здесь, а не в поле статуса: тот
                        // рисуется только на экране входа, и в чате о неудаче
                        // не говорилось ничего — сообщение просто пропадало.
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
                            // История не пришла. Молча показывать пустой канал
                            // нельзя: это выглядит как «в канале ничего нет», и
                            // пользователь заново кликает по каналу в надежде.
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
                            // egui просим прилипнуть ко дну только когда мы
                            // сами туда едем. Если просить это каждый кадр, пока
                            // пользователь внизу, egui начнёт возвращать его на
                            // низ сам и колесо перестанет работать. Удержание низа
                            // делаем сами — смещением, которое передаём строкой
                            // ниже.
                            .stick_to_bottom(self.scroll_to_bottom)
                            // Именно то смещение, по которому мы посчитали
                            // окно сообщений, а не запомненное с прошлого
                            // кадра: иначе кадр, который перематывает вниз,
                            // нарисовал бы верх канала (на один кадр, но
                            // список дёргается).
                            .vertical_scroll_offset(offset)
                            .show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                self.msg_width = ui.available_width();
                                ui.add_space(8.0);
                                // Строка догрузки вверх: место под неё берём
                                // всегда, иначе список дёргается.
                                ui.vertical_centered(|ui| {
                                    ui.set_min_height(MORE_ROW_H);
                                    if self.history_loading_more {
                                        ui.horizontal(|ui| {
                                            ui.spinner();
                                            ui.label(RichText::new("Loading older messages...").size(12.0).color(self.theme.text_secondary));
                                        });
                                    } else if let Some((_, reason)) = self.history_error.as_ref() {
                                        // Догрузка вверх не вышла. Спиннер здесь
                                        // горел бы вечно, а колесо перестало бы
                                        // работать: прокрутка вверх считается
                                        // запросом истории.
                                        ui.horizontal(|ui| {
                                            ui.label(RichText::new(format!("Не удалось догрузить: {reason}"))
                                                .size(12.0)
                                                .color(self.theme.text_secondary));
                                        });
                                    } else if self.trimmed_newest > 0 {
                                        // Потолок по памяти достигнут: чтобы втиснуть
                                        // загруженную страницу, пришлось убрать из
                                        // окна самые новые сообщения. Молча они
                                        // пропадали бы, и верх истории выглядел бы
                                        // просто концом переписки.
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
                                    // Рисуем только то, что попадает в окно, плюс
                                    // запас на быстрый скролл. Пропущенные
                                    // сообщения не рисуем совсем, но место под
                                    // них оставляем — иначе список схлопнется в
                                    // видимую часть, и полоса прокрутки будет
                                    // врать про длину истории.
                                    let (first, end) = visible_window(&offsets, offset, ui.available_height());
                                    // Срез, а не копия: `msgs` и так наш на время
                                    // кадра, отдельный Vec окна — лишняя
                                    // аллокация на каждый кадр (Т-3).
                                    let window = msgs_for_render.get(first..end).unwrap_or(&[]);
                                    // Где сейчас отрисован низ последнего
                                    // сообщения — от этого считаем зазоры.
                                    let mut drawn = 0.0_f32;
                                    // Буфер под ключ кэша аватарок: раньше
                                    // на каждое сообщение на каждом кадре
                                    // выделялась своя строка.
                                    let mut avatar_key = String::new();
                                    for (k, msg) in window.iter().enumerate() {
                                        // Только для тестов: сколько сильных
                                        // ссылок на сообщение в момент
                                        // отрисовки. Копия списка на кадр
                                        // удваивала бы счётчик (Т-3).
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
                                        // Высота почти всегда та же, что в
                                        // прошлом кадре: сначала спрашиваем и
                                        // копируем id в ключ только при
                                        // изменении. Отсутствующая запись тоже
                                        // считается изменением: даже если
                                        // оценка угадала, измеренная высота в
                                        // кэше нужна — на неё опирается якорь.
                                        if !msg.id.is_empty() && self.msg_heights.get(&msg.id) != Some(&h) {
                                            if self.msg_heights.len() > MAX_HEIGHT_CACHE {
                                                self.msg_heights.clear();
                                            }
                                            self.msg_heights.insert(msg.id.clone(), h);
                                        }
                                    }
                                    // Хвост докладываем до расчётной высоты,
                                    // чтобы полоса прокрутки считала весь
                                    // список, а не только нарисованную часть.
                                    let tail = (total - drawn).max(0.0);
                                    if tail > 0.0 {
                                        ui.add_space(tail);
                                    }
                                    // Дошли до верха — пора брать более старые
                                    // сообщения. Пока внизу, история не
                                    // грузится: внизу и так есть что читать. И
                                    // если список целиком влез в окно, прокрутить
                                    // вверх нечем — значит, и просить нечего.
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

                        // Доводим перемотку вниз руками только когда она была
                        // явной (`scroll_to_bottom`): egui считает содержимое
                        // к концу кадра, а перематывать надо было до его
                        // отрисовки.
                        //
                        // Когда чат просто держится внизу (пользователь внизу,
                        // длина списка меняется), состояние egui не трогаем
                        // совсем: удержание делается смещением, которое мы
                        // просим в начале кадра.
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
                        // Список ушёл и не туда, куда просили, и не на
                        // настоящий низ — значит прокрутил пользователь
                        // (колесом или перетаскиванием). Такой результат
                        // затирать на «дно» нельзя: колесо перестанет листать
                        // чат совсем, а это ровно то, на что жаловались. Если
                        // же мы просили примерно в ту же точку, куда список и
                        // встал (egui подрезал его по настоящей высоте), то
                        // это наш собственный промах с оценкой, а не прокрутка.
                        let moved_by_user = (offset_y - offset).abs() > Self::BOTTOM_SLACK
                            && (offset_y - max_off).abs() > Self::BOTTOM_SLACK;
                        // Где чат оказался на самом деле: когда держим низ и
                        // пользователь не трогал прокрутку — точно внизу (с
                        // поправкой на остаточную ошибку оценки), иначе там,
                        // куда увела прокрутка.
                        let settled = if (forced_bottom || stick) && !moved_by_user {
                            max_off
                        } else {
                            offset_y
                        };
                        self.chat_offset_y = settled;
                        self.last_scroll_offset_y = self.last_scroll_offset_y.max(settled);
                        // Прокрутка в самом низу — по этому новое сообщение
                        // будет тянуть чат вниз, а читающего историю выше не
                        // выбросит. Порог крошечный: стоит сделать его больше
                        // пары пикселей, и чат перестанет отпускать
                        // пользователя от низа — тот проскроллил на пять
                        // пикселей вверх, а его тут же вернуло.
                        self.chat_at_bottom = settled >= max_off - Self::BOTTOM_SLACK;
                        // Якорь — от того места, где чат встал на самом деле,
                        // а не от того, которое мы просили. Кадр рисуется на
                        // запрошенном смещении, а колесо egui применяет уже
                        // после отрисовки, поэтому «просили» на прошлом кадре.
                        // Якорь от «просили» закреплял каждый следующий кадр на
                        // прошлом смещении: колесо листало внутренний счётчик,
                        // а на экране было одно и то же — «чат копируется».
                        // Держать низ нечего: там смещение и так просится.
                        self.chat_anchor = if stick && !moved_by_user {
                            None
                        } else {
                            let top = visible_window(&offsets, settled, inner_h).0;
                            msgs.get(top).map(|m| (m.id.clone(), offsets[top] - settled))
                        };
                        scroll_out
                    });
                // Список нужен снова: `request_older_history` берёт из него
                // самый старый id. До этого шага кадр рисовал по своему Vec.
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
    /// Одно сообщение. Высота результата вызывающему не нужна: он меряет её
    /// по содержимому скролла.
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

    /// Префиксные суммы высот трёх сообщений по 100 пикселей.
    fn offsets(h: &[f32]) -> Vec<f32> {
        let mut o = vec![0.0];
        let mut s = 0.0;
        for v in h {
            s += v;
            o.push(s);
        }
        o
    }

    /// Виртуализация не должна терять сообщения: окно обязано накрывать всю
    /// видимую часть плюс запас, а выходить за пределы списка — нельзя.
    #[test]
    fn visible_window_covers_viewport() {
        let o = offsets(&[100.0; 100]);
        for offset in [0.0, 1.0, 500.0, 1000.0, 5000.0, 9_000.0, 9_900.0] {
            let (first, end) = visible_window(&o, offset, 800.0);
            assert!(first < end, "окно пустое при offset={offset}");
            assert!(end <= 100, "окно вышло за список: {end} при offset={offset}");
            // Всё, что пользователь видит, обязано быть нарисовано: верх
            // окна не ниже начала видимой части, низ — за её концом.
            assert!(
                o[first] <= offset,
                "начало видимого пропущено: first={first} offset={offset}"
            );
            assert!(
                end == 100 || o[end] >= offset + 800.0,
                "конец видимого не нарисован: end={end} offset={offset}"
            );
            // И при этом запас есть, чтобы при быстром скролле не было пустот.
            assert!(
                o[first + 1] > offset - OVERSCAN,
                "пропущено начало: first={first} offset={offset}"
            );
        }
    }

    /// Первое сообщение в списке не должно пропускаться, даже если прокрутка
    /// нулевая: сверху есть строка догрузки и пустое «начало канала».
    #[test]
    fn visible_window_keeps_first_message() {
        let o = offsets(&[40.0; 3]);
        let (first, end) = visible_window(&o, 0.0, 500.0);
        assert_eq!(first, 0);
        assert!(end >= 1);
    }

    /// Пустой список и список из одного сообщения не должны ломать расчёт.
    #[test]
    fn visible_window_handles_empty_list() {
        assert_eq!(visible_window(&[0.0], 0.0, 500.0), (0, 0));
        assert_eq!(visible_window(&[], 0.0, 500.0), (0, 0));
        let one = offsets(&[100.0]);
        assert_eq!(visible_window(&one, 0.0, 500.0), (0, 1));
    }
}

/// Проверки геометрии на настоящем egui: единичные тесты выше считают окно
/// виртуализации в отрыве от отрисовки, а здесь — на живом скролле с
/// настоящими сообщениями. Смысл: поймать то, что не видно в числах
/// аллокаций, — пустоты в списке, врущую полосу прокрутки и скачки вида при
/// подгрузке истории.
#[cfg(test)]
mod geometry_tests {
    use std::sync::Arc;

    use tokio::sync::mpsc;

    use super::*;
    use crate::messages::{ToApp, ToGateway};
    use crate::models::ChatChannel;

    /// Экран из замера: на нём числа замеров и этих тестов совпадают.
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
            // Тексты разной длины: одинаковые дали бы одинаковые высоты, и
            // виртуализацию было бы не на чём поймать.
            content: "сообщение номер ".to_string() + &i.to_string()
                + &" и ещё немного текста сверху, чтобы высота отличалась".repeat(i % 4),
            timestamp: "2026-01-01T00:00:00.000Z".into(),
            attachments: vec![],
            embeds: vec![],
            is_own: i % 11 == 0,
        }
    }

    /// Канал из `n` сообщений. Отдаём ещё и отправителя в приложение: новые
    /// сообщения приходят оттуда же, откуда и от настоящего гейтвея, иначе
    /// тест проверял бы не тот путь.
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
        /// Сообщения от гейтвея в приложение.
        tx: mpsc::UnboundedSender<ToApp>,
        /// Команды приложения в гейтвей.
        cmds: mpsc::UnboundedReceiver<ToGateway>,
    }

    /// Кадр не должен копировать список сообщений канала.
    ///
    /// Раньше `current_channel_messages` возвращал `.cloned()` — копию всех
    /// `Arc` канала на каждом кадре, а сверху ещё копировалось окно
    /// (`to_vec`). Пробник считает сильные ссылки на сообщение прямо в момент
    /// отрисовки: при копии их было бы больше одной, а у взятого из карты
    /// списка ссылка ровно одна.
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

    /// Колесо должно листать чат в обе стороны. Проверяем именно то, что
    /// нарисовано: кадр рисуется на смещении, которое мы попросили, а колесо
    /// egui применяет уже после отрисовки. Поэтому «счётчик смещения уехал»
    /// и «на экране что-то сдвинулось» — разные вещи, и проверять надо
    /// второе: иначе тест зелёный, а на экране всё то же самое
    /// («чат копируется, циклично вижу одно и то же»).
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

        // Вверх к началу истории.
        let up = wheel_until_stop(&mut h.app, &ctx, 120.0, true);
        let (_i, _c, ask) = scroll_sizes(&h.app);
        eprintln!("[TEST] наверх за {up} событий: на экране смещение {ask:.0}");
        assert!(
            ask < 1.0,
            "колесо вверх не довело список до начала: на экране смещение {ask:.0}"
        );

        // И обратно вниз — до последнего сообщения.
        let down = wheel_until_stop(&mut h.app, &ctx, -120.0, false);
        let (inner, content, ask) = scroll_sizes(&h.app);
        let bottom = content - inner;
        eprintln!("[TEST] вниз за {down} событий: на экране {ask:.0}, низ {bottom:.0}");
        assert!(
            (ask - bottom).abs() < 1.0,
            "колесо вниз не довело список до низа: на экране {ask:.0}, низ {bottom:.0}"
        );
    }

    /// Медленное колесо тоже должно двигать нарисованный список. С быстрым
    /// колесом поломка была не видна: там якорь успевали сбрасывать, и список
    /// дёргался через кадр. С медленным (трекпад, хвост сглаживания) якорь
    /// выживал и держал кадр на прошлом смещении — колесо крутится, а на
    /// экране всё то же самое, циклично. Проверяем по нарисованному кадру.
    #[test]
    fn slow_wheel_moves_the_drawn_list() {
        let mut h = app_with_messages(300);
        let ctx = egui::Context::default();
        frames(&mut h.app, &ctx, 3);
        let (_i, _c, start) = scroll_sizes(&h.app);
        assert!(start > 1000.0, "список должен быть заметно выше окна");

        // Мелкий шаг: именно на нём якорь доживал до следующего кадра.
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

    /// Крутить колесо, пока нарисованный список перестаёт двигаться.
    ///
    /// Смотрим на `ask` — смещение, по которому нарисован кадр, — и требуем,
    /// чтобы оно шло именно туда, куда крутят, без рывков обратно. egui
    /// сглаживает колесо по кадрам, поэтому «перестало двигаться» — это
    /// несколько одинаковых кадров подряд, а не первое же отсутствие сдвига.
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
                // Крутим вверх — список должен идти к началу, и наоборот. Если
                // он дёргается обратно, значит кадр рисуется не там, где мы
                // прокрутили: колесо листает счётчик, а картинка стоит.
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

    /// Кадр, который держит чат внизу, обязан нарисовать последнее сообщение
    /// целиком. Если просить низ по одной лишь оценке высот (на длинном списке
    /// она врёт на десятки пикселей), список откроется чуть выше настоящего
    /// низа — и нижний край последнего сообщения окажется за кромкой окна.
    /// Проверяем каждый кадр: в том числе первый, когда оценка ещё ни разу не
    /// сверялась с измеренной.
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
                .copied()
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

    /// Оценка высоты не должна врать. Пока числа в оценке держались на глаз,
    /// список был завышен на 14% на каждом сообщении: длина списка плыла по
    /// мере прокрутки, низ уезжал, и докрутить чат до конца было нельзя.
    /// Меряем оценку и настоящую высоту на живом egui и требуем, чтобы
    /// расхождение было в пределах допуска.
    #[test]
    fn estimate_height_is_close_to_the_real_one() {
        use crate::models::Attachment;
        let mut h = app_with_messages(1);
        let ctx = egui::Context::default();
        frames(&mut h.app, &ctx, 2);
        let width = h.app.msg_width;

        // Случаи, на которых оценка обычно и врет: чужие и свои сообщения от
        // одной строки до двадцати, одно длинное слово (его egui не
        // переносит) и сообщение с картинкой.
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
            // Рисуем по одному: список короткий и в окно влезает целиком.
            h.app.messages.insert("c0".into(), vec![Arc::new(m.clone())]);
            scroll_to(&mut h.app, 0.0);
            frame(&mut h.app, &ctx);
            frame(&mut h.app, &ctx);
            let real = h.app.msg_heights.get(&m.id).copied().unwrap_or(0.0);
            assert!(
                (est - real).abs() <= real * 0.15 + 12.0,
                "{name}: оценка {est:.0} против настоящих {real:.0}"
            );
            sum_est += est;
            sum_real += real;
        }
        // Общая длина списка тоже должна сходиться: полоса прокрутки рисует
        // длину по оценке, и ошибка в ней видна глазом.
        assert!(
            (sum_est - sum_real).abs() <= sum_real * 0.05,
            "длина списка по оценке {sum_est:.0} против настоящей {sum_real:.0}"
        );
    }

    /// Низ списка должен быть достижим и последнее сообщение должно стоять
    /// впритык к нижней кромке окна: на этом и стоит «я не могу проскроллить
    /// чат вниз».
    #[test]
    fn bottom_is_reachable_and_last_message_sits_at_the_edge() {
        let mut h = app_with_messages(300);
        let ctx = egui::Context::default();
        frames(&mut h.app, &ctx, 3);

        // Прокручиваем вниз подстановкой смещения — так же, как сообщает egui
        // после колеса, — и проверяем, что низ достижим и удерживается.
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
        // Последнее сообщение должно заканчиваться у низа окна, а не висеть
        // над ним (список не долистался) и не торчать за краем (докрутилось
        // лишнее).
        let n = h.app.messages["c0"].len();
        let last_bottom = h.app.msg_offsets[n] - h.app.chat_offset_y;
        assert!(
            (last_bottom - inner).abs() <= inner * 0.05,
            "последнее сообщение не у низа окна: {last_bottom:.1} при окне {inner:.0} (смещение {:.0}, низ {max:.0})",
            h.app.chat_offset_y
        );
    }

    /// Пока пользователь внизу, список обязан остаться внизу сам. Длина списка
    /// меняется на ходу: подгружается история вверх, у сообщений уточняется
    /// высота, приходят картинки. Если низ не пересчитывать каждый кадр, чат
    /// отползает от последнего сообщения и больше не возвращается — новое
    /// сообщение его уже не утянет.
    #[test]
    fn bottom_is_held_while_the_list_changes_under_it() {
        let mut h = app_with_messages(300);
        let ctx = egui::Context::default();
        frames(&mut h.app, &ctx, 3);
        assert!(h.app.chat_at_bottom, "после открытия чат должен быть внизу");

        // Пришло новое сообщение: ниже списка стало длиннее, смещение обязано
        // потянуться следом.
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

        // Под сообщением в кадре уточнилась высота (настоящая больше той, что
        // предполагала оценка). Список стал длиннее — низ должен поехать за ним.
        let grew = h
            .app
            .msg_heights
            .values_mut()
            .next()
            .map(|h| {
                *h += 40.0;
                *h
            })
            .expect("кэш высот не пуст после кадров");
        frame(&mut h.app, &ctx);
        // Размеры снимаем после кадра: высота сообщения выросла именно в нём.
        let (inner, content, _ask) = scroll_sizes(&h.app);
        let max = content - inner;
        assert!(
            (h.app.chat_offset_y - max).abs() < 1.0,
            "чат отполз от низа после роста сообщения: смещение {:.0}, низ {max:.0} (высота выросла на {grew:.0})",
            h.app.chat_offset_y
        );
        assert!(h.app.chat_at_bottom, "после уточнения высоты чат должен быть внизу");

        // А вот читатель истории: если пользователь ушёл вверх, его не должно
        // выбросить вниз.
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

    /// Один кадр приложения.
    fn frame(app: &mut App, ctx: &egui::Context) -> egui::FullOutput {
        ctx.run(screen(), |ctx| app.draw_chat(ctx))
    }

    /// Несколько кадров подряд: первый только прогревает кэш высот и
    /// отладочную строку, важен последний.
    fn frames(app: &mut App, ctx: &egui::Context, n: usize) {
        for _ in 0..n {
            frame(app, ctx);
        }
    }

    /// Кадр с настоящим колесом мыши над окном чата. `dy` — знак egui:
    /// положительное значение тянет список к началу, отрицательное — к новым
    /// сообщениям.
    ///
    /// Курсор ставим посередине чата и каждый кадр сдвигаем на полпикселя:
    /// egui считает указатель наведённым, только если он двигался за кадр, иначе
    /// колесо не доходит до скролла и тест врал бы в обратную сторону.
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
        /// Счётчик для дрожания указателя, см. `wheel_frame`.
        static NUDGE: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) };
    }

    /// Всё, что приложение отправило гейтвею, и очистить очередь.
    fn drain(cmds: &mut mpsc::UnboundedReceiver<ToGateway>) -> Vec<ToGateway> {
        let mut out = Vec::new();
        while let Ok(c) = cmds.try_recv() {
            out.push(c);
        }
        out
    }

    /// Поставить прокрутку в `off` и забыть якорь: так мы притворяемся, что
    /// пользователь промотал список сам. Заодно снимаем признак «чат внизу»
    /// — иначе приложение (справедливо) решит, что пользователь всё ещё внизу,
    /// и вернёт список вниз.
    fn scroll_to(app: &mut App, off: f32) {
        app.scroll_to_bottom = false;
        app.chat_at_bottom = false;
        app.chat_anchor = None;
        app.chat_offset_y = off;
    }

    /// Размеры скролла из отладочной строки `SCROLL: inner_h=… content_h=…
    /// offset_y=… ask=…`: высота окна, высота содержимого и смещение, которое
    /// мы просили (по нему нарисован кадр).
    ///
    /// Кэшируются они у нас только здесь, но зато всегда свежие.
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
            // Смещение, которое мы просили: по нему считалось окно сообщений
            // и по нему нарисован кадр. Именно его проверяем — «куда встал
            // чат после кадра» проверяется отдельно.
            if let Some(v) = part.strip_prefix("ask=") {
                ask = v.parse().expect("ask");
            }
        }
        (inner, content, ask)
    }

    /// Высота всего списка по префиксным суммам кадра.
    fn total_height(app: &App) -> f32 {
        *app.msg_offsets.last().expect("буфер префиксных сумм пуст")
    }

    /// Виртуализация должна экономить, а не просто усложнять код: в кадре
    /// отрисовывается десяток сообщений из трёхсот, и кэш высот остаётся
    /// маленьким. Если сюда попадёт весь канал — окно считается неправильно.
    #[test]
    fn virtualization_draws_only_what_is_on_screen() {
        let mut h = app_with_messages(300);
        let ctx = egui::Context::default();

        // Как при открытии канала: первая страница истории, чат внизу.
        frames(&mut h.app, &ctx, 3);

        let (inner, content, _ask) = scroll_sizes(&h.app);
        let measured = h.app.msg_heights.len();
        eprintln!("[TEST] inner={inner:.0} content={content:.0} измерено={measured}");
        assert!(measured > 0, "ни одно сообщение не нарисовано");
        assert!(
            measured < 100,
            "виртуализация не работает: измерено {measured} высот из 300"
        );
        // Полоса прокрутки обязана знать про всю историю: место под
        // невидимое отдаётся пустым отступом в конце списка.
        assert!(
            (content - total_height(&h.app)).abs() < 120.0,
            "полоса прокрутки врёт: content={content:.0}, список={:.0}",
            total_height(&h.app)
        );
        assert!(content > inner * 3.0, "история должна быть заметно выше экрана");
    }

    /// Главная беда виртуализации — пустоты. Прокручиваем длинный список в
    /// нескольких местах и проверяем, что каждое сообщение, попадающее в
    /// видимую часть, действительно нарисовано: его высота обязана попасть в
    /// кэш. Иначе в кадре висели бы серые полосы.
    #[test]
    fn scrolled_list_has_no_gaps() {
        let mut h = app_with_messages(300);
        let ctx = egui::Context::default();
        frames(&mut h.app, &ctx, 3);
        let (inner, _content, _ask) = scroll_sizes(&h.app);

        // Метки середины, начала и самого низа списка.
        let total = total_height(&h.app);
        for off in [0.0, inner, total * 0.5, total - inner, total - 1.0] {
            scroll_to(&mut h.app, off);
            // Два кадра: первый докладывает в кэш высоты окна, второй уже
            // считает по ним — иначе проверка мерила бы сама себя.
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

    /// Подгрузка истории вверх не должна выбрасывать читателя: сообщение, на
    /// котором он остановился, остаётся на том же месте экрана.
    #[test]
    fn prepending_history_keeps_the_view_in_place() {
        let mut h = app_with_messages(300);
        let ctx = egui::Context::default();
        frames(&mut h.app, &ctx, 3);

        // Пользователь читает середину истории.
        let half = total_height(&h.app) * 0.5;
        scroll_to(&mut h.app, half);
        frames(&mut h.app, &ctx, 3);
        let (anchor_id, dy) = h
            .app
            .chat_anchor
            .clone()
            .expect("вид должен держаться на сообщении, а не на номере строки");
        eprintln!("[TEST] якорь {anchor_id} на {dy:.1} пикселей выше верха окна");

        // Подгрузилась страница из 50 более старых сообщений.
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

    /// Новое сообщение тянет чат вниз только если пользователь был внизу.
    /// Читающий историю выше не должен быть выброшен в конец.
    #[test]
    fn new_message_pulls_down_only_from_the_bottom() {
        let mut h = app_with_messages(300);
        let ctx = egui::Context::default();
        frames(&mut h.app, &ctx, 3);
        assert!(h.app.chat_at_bottom, "после открытия канала чат должен быть внизу");

        // 1. Пользователь внизу: новое сообщение обязано его удержать.
        let mut m = message(300);
        m.id = "m0300".into();
        h.tx.send(ToApp::Message(m.clone())).unwrap();
        h.app.poll(&ctx);
        frames(&mut h.app, &ctx, 2);
        assert!(h.app.chat_at_bottom, "внизу новое сообщение должно тянуть вниз");
        // Сверяемся с высотой содержимого того же кадра: сумма в msg_offsets
        // посчитана до того, как высота нового сообщения была измерена, и
        // отстаёт на величину ошибки оценки.
        let (inner, content, _ask) = scroll_sizes(&h.app);
        let bottom = content - inner;
        eprintln!("[TEST] внизу: offset={:.0}, низ={bottom:.0}", h.app.chat_offset_y);
        assert!(
            (h.app.chat_offset_y - bottom).abs() < 2.0,
            "внизу чат должен остаться внизу: offset={:.0}, низ={bottom:.0}",
            h.app.chat_offset_y
        );

        // 2. Пользователь ушёл читать историю вверх: новое сообщение не
        // должно его выбрасывать.
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

    /// Догрузка вверх должна уйти ровно одним запросом, а не пачкой на каждый
    /// кадр, и `before` в нём — самое старое из уже показанных.
    #[test]
    fn older_history_is_requested_once_with_oldest_id() {
        let mut h = app_with_messages(300);
        let ctx = egui::Context::default();
        frames(&mut h.app, &ctx, 3);
        // Первые кадры открывают канал внизу и молчат.
        assert!(drain(&mut h.cmds).is_empty(), "внизу историю не просят");

        // Прокрутили к самому началу истории.
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
        // Ещё кадры в том же месте — второй запроса быть не должно.
        frames(&mut h.app, &ctx, 3);
        assert!(drain(&mut h.cmds).is_empty(), "повторный запрос истории");

        // Страница пришла с пометкой «дальше пусто» — и больше не просим.
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

    /// То же, но через настоящий путь открытия канала: `open_channel` сам
    /// просит первую страницу, ответ приходит от гейтвея отдельным событием.
    /// Проверяем, что в канале оказывается ровно одна страница и никто не
    /// просит следующую, пока пользователь не доскроллит.
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

        // Клик по каналу.
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

        // Гейтвей ответил страницей из 50 сообщений.
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

    /// приносить одну страницу, а не «читаем канал целиком». Дошли до
    /// начала, страница пришла — и всё: читатель сам решает, докручивать ли
    /// дальше. Иначе клиент в фоне долбит API, пока не упрётся в потолок.
    #[test]
    fn scrolling_up_loads_one_page_not_the_whole_channel() {
        let mut h = app_with_messages(50);
        let ctx = egui::Context::default();
        frames(&mut h.app, &ctx, 3);
        assert!(
            drain(&mut h.cmds).is_empty(),
            "при открытии канала история не должна грузиться сама"
        );

        // Читатель доскроллил до самого начала.
        scroll_to(&mut h.app, 0.0);
        frames(&mut h.app, &ctx, 3);
        let first_page = drain(&mut h.cmds).len();
        eprintln!("[TEST] доскроллил вверх: запросов {first_page}");

        // Discord отвечает страницей: «дальше есть». Читатель при этом
        // остаётся на тех же сообщениях, и новых запросов быть не должно.
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

    /// Пустой канал и канал из одного сообщения — обычное дело, и ни то, ни
    /// другое не должно ни падать, ни просить истории, которой нет.
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

    /// Смещение последнего нарисованного скролла: состояние egui приватно,
    /// поэтому забираем его сами — так же, как в тестах геометрии берём размеры
    /// из отладочной строки.
    static OFFSET: Mutex<Vec<f32>> = Mutex::new(Vec::new());

    /// Минимальный ScrollArea без нашего кода: доходит ли до него колесо,
    /// поданное через RawInput. Если и здесь не доходит — дело в том, как мы
    /// кормим egui событиями, а не в чате.
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
