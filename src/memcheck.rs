//! Измерение памяти клиента под реальной нагрузкой.
//!
//! Файл копируется без правок в базовую и в новую версию, чтобы числа
//! сравнивались честно: одна и та же нагрузка, один и тот же код замера.
//!
//! Запуск: `cargo test --release -- --ignored --nocapture memcheck`

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use eframe::egui;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::app::App;
use crate::models::{test_guild, test_user, ChatChannel, ChatMessage};

// ───────────────────────── счётчик аллокаций ─────────────────────────

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicUsize = AtomicUsize::new(0);
static COUNT: AtomicUsize = AtomicUsize::new(0);

struct Counting;

impl Counting {
    fn add(n: usize) {
        TOTAL.fetch_add(n, Ordering::Relaxed);
        COUNT.fetch_add(1, Ordering::Relaxed);
        let live = LIVE.fetch_add(n, Ordering::Relaxed) + n;
        PEAK.fetch_max(live, Ordering::Relaxed);
    }
    fn sub(n: usize) {
        LIVE.fetch_sub(n, Ordering::Relaxed);
    }
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            Counting::add(l.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        Counting::sub(l.size());
        System.dealloc(p, l)
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = System.alloc_zeroed(l);
        if !p.is_null() {
            Counting::add(l.size());
        }
        p
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        let np = System.realloc(p, l, new_size);
        if !np.is_null() {
            Counting::sub(l.size());
            Counting::add(new_size);
        }
        np
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

// ───────────────────────── вспомогательное ─────────────────────────

fn live() -> usize {
    LIVE.load(Ordering::Relaxed)
}
fn total() -> usize {
    TOTAL.load(Ordering::Relaxed)
}
fn count() -> usize {
    COUNT.load(Ordering::Relaxed)
}
fn reset_peak() {
    PEAK.store(live(), Ordering::Relaxed);
}
fn peak() -> usize {
    PEAK.load(Ordering::Relaxed)
}
fn rss_mb() -> f64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: usize = rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse()
                .unwrap_or(0);
            return kb as f64 / 1024.0;
        }
    }
    0.0
}
fn mb(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Нагрузка: 300 сообщений, у каждого текст, картинка-вложение и
/// «богатый» эмбед (author/footer/provider/fields/thumbnail) — как в жизни.
fn fixture_json(n: usize) -> String {
    let mut arr: Vec<Value> = Vec::with_capacity(n);
    for i in 0..n {
        let id = format!("{:018}", 1_400_000_000_000_000_000i64 + i as i64);
        let author_id = format!("{:018}", 2_000_000_000_000_000_000i64 + (i % 30) as i64);
        arr.push(json!({
            "id": id,
            "content": format!("сообщение номер {} в канале", i),
            "timestamp": "2026-09-26T12:00:00.000000+00:00",
            "author": {
                "id": author_id,
                "username": format!("user_number_{}", i % 30),
                "avatar": Value::Null
            },
            "attachments": [{
                "filename": format!("photo_{}.png", i),
                "url": format!("https://cdn.discordapp.com/attachments/1/photo_{}.png", i),
                "content_type": "image/png",
                "width": 1600,
                "height": 1200,
                "size": 1_200_000,
                "description": "схема из чата"
            }],
            "embeds": [{
                "type": "rich",
                "title": format!("Заголовок {}", i),
                "description": "описание ".repeat(10),
                "url": "https://example.com/watch",
                "color": 0x3498db,
                "author": {
                    "name": "Автор эмбеда",
                    "url": "https://example.com",
                    "icon_url": "https://cdn.discordapp.com/embed/avatars/1.png"
                },
                "footer": {
                    "text": "подвал",
                    "icon_url": "https://cdn.discordapp.com/embed/avatars/2.png"
                },
                "image": {
                    "url": format!("https://cdn.discordapp.com/embeds/{}/picture.png", i),
                    "width": 1600,
                    "height": 1200
                },
                "thumbnail": {
                    "url": format!("https://cdn.discordapp.com/embeds/{}/thumb.png", i),
                    "width": 300,
                    "height": 300
                },
                "provider": { "name": "Провайдер", "url": "https://example.com" },
                "fields": [
                    { "name": "поле один", "value": "значение один", "inline": true },
                    { "name": "поле два", "value": "значение два", "inline": false },
                    { "name": "поле три", "value": "значение три", "inline": true }
                ],
                "timestamp": "2026-09-26T12:00:00.000Z"
            }]
        }));
    }
    serde_json::to_string(&arr).unwrap()
}

fn make_png(w: u32, h: u32) -> Vec<u8> {
    use image::{ImageEncoder, Rgb, RgbImage};
    let img = RgbImage::from_pixel(w, h, Rgb([12, 34, 56]));
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)
        .unwrap();
    png
}

fn make_gif(w: u32, h: u32, frames: usize) -> Vec<u8> {
    use image::{Rgba, RgbaImage};
    let mut out = Vec::new();
    {
        let mut enc = image::codecs::gif::GifEncoder::new(&mut out);
        for f in 0..frames {
            let frame = RgbaImage::from_pixel(w, h, Rgba([(f * 7) as u8, 40, 90, 255]));
            enc.encode_frame(image::Frame::from_parts(
                frame,
                0,
                0,
                image::Delay::from_numer_denom_ms(50, 1),
            ))
            .unwrap();
        }
    }
    out
}

fn image_urls(json: &str) -> Vec<String> {
    let arr: Vec<Value> = serde_json::from_str(json).unwrap();
    let mut urls = Vec::new();
    for m in &arr {
        if let Some(a) = m["attachments"].as_array() {
            for x in a {
                if let Some(u) = x["url"].as_str() {
                    urls.push(u.to_string());
                }
            }
        }
        if let Some(e) = m["embeds"].as_array() {
            for x in e {
                for f in ["image", "thumbnail"] {
                    if let Some(u) = x[f]["url"].as_str() {
                        urls.push(u.to_string());
                    }
                }
            }
        }
    }
    urls
}

fn report(name: &str, bytes: usize, allocs: usize) {
    eprintln!(
        "  {:<44} {:>9.2} МБ   {:>9} аллокаций",
        name,
        mb(bytes),
        allocs
    );
}

// ───────────────────────── сам замер ─────────────────────────

#[test]
#[ignore]
fn memcheck_report() {
    eprintln!("\n=== WYVERN: замер памяти ===");
    eprintln!("RSS в начале: {:.1} МБ", rss_mb());

    let (_, rx) = mpsc::unbounded_channel();
    let base0 = live();
    let allocs0 = count();
    let mut app = App::new(rx);
    app.connected = true;
    app.gw_started = true;
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1052.0, 1054.0),
        )),
        ..Default::default()
    };

    // 0. Пол: пустой клиент без сообщений и картинок. Всё, что дальше
    // прибавится к этому, — стоимость данных, а не egui, шрифтов и
    // аллокатора; ниже это и есть настоящая цена картинок.
    let _ = ctx.run(raw.clone(), |ctx| {
        app.draw_chat(ctx);
    });
    eprintln!(
        "пол: пустой клиент + 1 кадр — RSS {:.1} МБ, живых байт {:.2} МБ, {} аллокаций",
        rss_mb(),
        mb(live() - base0),
        count() - allocs0
    );

    let cid = "1234567890123456789";
    let json = fixture_json(300);

    // 1. Сообщения канала.
    let base = live();
    reset_peak();
    let c0 = count();
    let msgs: Vec<ChatMessage> =
        crate::gateway::parse_history_page(&json, cid).expect("фикстура обязана разбираться");
    let n_msgs = msgs.len();
    let peak_parse = peak() - base;
    let allocs_parse = count() - c0;
    let held = live() - base;
    let msgs_bytes = {
        let before = live();
        app.messages.insert(cid.to_string(), msgs.into_iter().map(Arc::new).collect());
        live() - before
    };
    report(
        &format!("разбор {} сообщений: пик (JSON + дерево)", n_msgs),
        peak_parse,
        allocs_parse,
    );
    eprintln!("  (исходный JSON сам по себе: {:.2} МБ)", mb(json.len()));
    report("разобранные сообщения в памяти", held, 0);
    report("их же в Vec<Arc<_>> (оверхейд)", msgs_bytes, 0);
    eprintln!(
        "  sizeof(ChatMessage) = {} байт; в куче на сообщение приходится {:.0} байт",
        std::mem::size_of::<ChatMessage>(),
        held / n_msgs.max(1)
    );

    // Выбираем этот канал, иначе чат рисует пустой экран приветствия и
    // десять кадров ничего не меряют.
    app.channels.push(ChatChannel {
        id: cid.to_string(),
        name: "основной".into(),
        guild_id: Some("111111111111111111".into()),
        channel_type: 0,
        topic: None,
        position: 0,
    });
    app.selected_channel = Some(0);

    // Чтобы рендер не лез в сеть, помечаем все картинки «не загружено».
    for u in image_urls(&json) {
        app.failed_images.insert(u);
    }
    drop(json);

    // 2. Кэш картинок: 32 обычных + 1 гифка (как сейчас разрешает политика).
    let png = make_png(1600, 1200);
    let gif = make_gif(640, 480, 24);
    let before = live();
    for i in 0..32 {
        app.cache_image_bytes(&ctx, &format!("https://cdn.discordapp.com/attachments/1/photo_{}.png", i), &png);
    }
    let statics_bytes = app.image_cache.bytes();
    report("32 картинки 1600x1200 (загружено в кэш)", live() - before, app.image_cache.len());
    eprintln!("  кэш картинок держит: {:.2} МБ ({} шт.)", mb(statics_bytes), app.image_cache.len());

    let before = live();
    app.cache_image_bytes(&ctx, "https://cdn.discordapp.com/attachments/1/anim.gif", &gif);
    let with_gif = app.image_cache.bytes();
    report("1 гифка 640x480 x24", live() - before, app.image_cache.len());
    eprintln!("  кэш картинок с гифкой: {:.2} МБ ({} шт.)", mb(with_gif), app.image_cache.len());

    // 3. Десять кадров отрисовки загруженного канала вместе со списками.
    app.guilds.push(test_guild("111111111111111111", "Test Guild"));
    for i in 0..189 {
        app.channels.push(ChatChannel {
            id: format!("g{}", i),
            name: format!("channel-{}", i),
            guild_id: Some("111111111111111111".into()),
            channel_type: 0,
            topic: None,
            position: i as i32,
        });
    }
    app.selected_guild = Some(0);
    app.selected_channel = Some(0);
    for f in 0..60 {
        app.friends.push(test_user(&format!("f{}", f), &format!("friend_{}", f)));
    }
    let t0 = total();
    let c0 = count();
    for _ in 0..10 {
        let _ = ctx.run(raw.clone(), |ctx| {
            app.draw_server_list(ctx);
            app.draw_channel_list(ctx);
            app.draw_input_bar(ctx);
            app.draw_chat(ctx);
        });
    }
    report("10 кадров отрисовки (чат + списки)", total() - t0, count() - c0);

    // Разбираем, сколько из оставшегося мусора — наш код, а сколько сам egui
    // (на каждый лейбл он всё равно верстает и кэширует текст). Кэши к этому
    // моменту прогреты, поэтому числа чуть ниже, чем у первых десяти кадров.
    let t1 = total();
    let c1 = count();
    for _ in 0..10 {
        let _ = ctx.run(raw.clone(), |ctx| {
            app.draw_chat(ctx);
        });
    }
    report("из них 10 кадров только чата", total() - t1, count() - c1);
    let t2 = total();
    let c2 = count();
    for _ in 0..10 {
        let _ = ctx.run(raw.clone(), |ctx| {
            app.draw_server_list(ctx);
            app.draw_channel_list(ctx);
        });
    }
    report("из них 10 кадров только списков", total() - t2, count() - c2);

    eprintln!("  RSS в конце: {:.1} МБ", rss_mb());
    eprintln!("  всего аллокаций за тест: {}", count());
    eprintln!("=== конец замера ===\n");
}
