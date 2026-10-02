use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use eframe::egui::{self, TextureHandle};

use crate::app::{App, MAX_FAILED_IMAGES};
use crate::models::{ImagePayload, LoadedImage};

const CDN_BASE: &str = "https://cdn.discordapp.com";
const MAX_CONCURRENT_AVATAR_DOWNLOADS: usize = 4;
/// Сколько картинок качается одновременно. Без потолка канал с полсотней
/// картинок порождает полсотню потоков, и все они одновременно держат в
/// памяти декодированные пиксели — это сотни мегабайт на ровном месте.
const MAX_CONCURRENT_IMAGE_DOWNLOADS: usize = 3;
static AVATAR_DOWNLOADS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
static IMAGE_DOWNLOADS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
/// Большую картинку нет смысла хранить в полном разрешении: в чате она
/// рисуется максимум 360x360, то есть даже на HiDPI-экране (2x) 768 пикселей
/// хватает ровно. 1536 превращали одну фотку в 9 МБ текстуры.
const MAX_IMAGE_DIM: u32 = 768;
/// Гифка — это сразу много кадров, поэтому кадры мельче. 16 кадров по 448
/// пикселей — это 9 МБ на анимацию вместо 28.
const MAX_GIF_DIM: u32 = 448;
/// До какого размера ужимаем кадр, если кадров очень много: лучше мыльная
/// гифка целиком, чем красивая, обрывающаяся на середине.
const MIN_GIF_DIM: u32 = 64;
/// Сколько памяти готовы отдать под кадры ОДНОЙ гифки. Раньше здесь стоял
/// потолок в 16 кадров, и любая гифка длиннее шестнадцати «заканчивалась» на
/// одном и том же месте. Считаем по памяти, а не по штукам: у гифок разное
/// число кадров, а бюджет у клиента один.
const MAX_GIF_BYTES: usize = 32 * 1024 * 1024;
/// Страховка от абсурдной гифки: по памяти такая может и пройти, но тысячи
/// текстур — это уже перебор.
const MAX_GIF_FRAMES: usize = 2000;
/// Сколько пикселей в исходной картинке мы готовы распаковать. В чате она
/// всё равно ужимается до 768 px, но распаковка идёт по исходнику: Discord
/// принимает картинки до 10000×10000, а это 400 МБ в один момент, и на
/// трёх параллельных загрузках клиент на этом умирает. Обычное фото
/// (12–24 Мпикс) проходит без проблем.
const MAX_SOURCE_PIXELS: u64 = 40_000_000;
/// Сколько байт ответа вообще готовы принять. Распаковщик проверяет размер
/// пикселей ПОСЛЕ чтения тела, а ссылку на картинку в эмбеде задаёт чужой
/// сайт (models.rs) — он мог бы отдать гигабайты и занять память ещё до
/// проверки. Потолок с запасом покрывает MAX_SOURCE_PIXELS пикселей даже в
/// несжатом виде (4 байта на пиксель) плюс запас на контейнер.
const MAX_DOWNLOAD_BYTES: u64 = MAX_SOURCE_PIXELS * 8;

/// Прочитать тело ответа, но не больше `max` байт.
///
/// `None` — тело больше лимита (или чтение сорвалось): распаковывать такое
/// нельзя. Читаем по кускам через `Read::take`, поэтому лишнее не оседает в
/// памяти целиком.
fn read_limited(resp: reqwest::blocking::Response, max: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut buf = Vec::new();
    resp.take(max + 1).read_to_end(&mut buf).ok()?;
    if buf.len() as u64 > max {
        return None;
    }
    Some(buf)
}

/// Регистронезависимый поиск подстроки без создания новой строки.
///
/// `download_image` вызывается на каждом кадре для каждой видимой картинки, а
/// раньше тут делался `url.to_lowercase()` — при двадцати картинках на экране
/// это сотни аллокаций в секунду ради проверки «не аватарка ли это».
fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    !n.is_empty() && h.len() >= n.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// Один общий клиент на всё приложение: свой `Client` на каждую картинку —
/// это новый пул соединений и TLS-сессия на каждый запрос.
fn http() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(20))
            .pool_max_idle_per_host(8)
            .build()
            .expect("не собрать HTTP-клиент")
    })
}

/// Уменьшить картинку, если она заметно больше нужного.
fn shrink(img: image::DynamicImage, max_dim: u32) -> image::DynamicImage {
    if img.width() > max_dim || img.height() > max_dim {
        img.thumbnail(max_dim, max_dim)
    } else {
        img
    }
}

/// Помещается ли исходник такого размера в лимит: распаковка идёт по
/// исходным пикселям, а не по тем, что останутся на экране.
pub(crate) fn source_size_allowed(w: u32, h: u32) -> bool {
    u64::from(w) * u64::from(h) <= MAX_SOURCE_PIXELS
}

/// Пропустить цепочку под-блоков GIF (длина + данные, пока не ноль) и вернуть
/// позицию сразу за терминатором.
fn skip_gif_sub_blocks(bytes: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let len = *bytes.get(pos)? as usize;
        pos = pos.checked_add(1)?;
        if len == 0 {
            return Some(pos);
        }
        pos = pos.checked_add(len)?;
        if pos > bytes.len() {
            return None;
        }
    }
}

/// Сколько кадров в GIF — по структуре файла, без распаковки пикселей.
///
/// Нужно заранее: по числу кадров выбирается их размер, чтобы все кадры
/// влезли в `MAX_GIF_BYTES` и гифка играла целиком, а не обрывалась. Разбор
/// идёт по блокам и стоит копейки: заголовок, глобальная таблица цветов,
/// затем расширения (`0x21`), кадры (`0x2C`) и конец (`0x3B`).
pub(crate) fn gif_frame_count(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 13 || (&bytes[..6] != b"GIF89a" && &bytes[..6] != b"GIF87a") {
        return None;
    }
    // Биты 0–2 упакованного поля — размер глобальной таблицы цветов (2^(n+1)).
    let packed = bytes[10];
    let mut pos = 13usize;
    if packed & 0x80 != 0 {
        pos = pos.checked_add(3usize * (1usize << ((packed & 0x07) + 1)))?;
    }
    let mut count = 0usize;
    while pos < bytes.len() {
        match bytes[pos] {
            // Конец файла.
            0x3B => break,
            // Расширение: 0x21 + метка + под-блоки.
            0x21 => {
                pos = skip_gif_sub_blocks(bytes, pos.checked_add(2)?)?;
            }
            // Кадр: 0x2C + 9 байт дескриптора; за ним, возможно, локальная
            // таблица цветов, размер кода LZW и сжатые данные.
            0x2C => {
                count += 1;
                if pos + 10 > bytes.len() {
                    return None;
                }
                let ipacked = bytes[pos + 9];
                pos += 10;
                if ipacked & 0x80 != 0 {
                    pos = pos.checked_add(3usize * (1usize << ((ipacked & 0x07) + 1)))?;
                }
                pos = pos.checked_add(1)?; // размер минимального кода LZW
                pos = skip_gif_sub_blocks(bytes, pos)?;
            }
            // Нулевой байт-заполнитель между блоками.
            0x00 => pos += 1,
            // Что-то незнакомое — считаем, что структуру не поняли.
            _ => return None,
        }
    }
    Some(count)
}

/// Размер кадра, при котором все кадры гифки влезают в `MAX_GIF_BYTES`.
///
/// Типичная гифка на 30–60 кадров остаётся на `MAX_GIF_DIM`; очень длинную
/// ужимаем сильнее, но она играет целиком.
fn gif_frame_dim(frames: usize) -> u32 {
    let frames = frames.clamp(1, MAX_GIF_FRAMES);
    let per_frame = (MAX_GIF_BYTES / 4) / frames;
    let mut dim = (per_frame as f64).sqrt().floor() as u32;
    // sqrt на целых может дать на пиксель больше, чем влезает; подстрахуемся.
    while dim > MIN_GIF_DIM
        && (dim as usize) * (dim as usize) * 4 * frames > MAX_GIF_BYTES
    {
        dim -= 1;
    }
    dim.clamp(MIN_GIF_DIM, MAX_GIF_DIM)
}

/// Запомнить неудачу, не раздувая список.
///
/// При переполнении вытесняется ОДНА запись, а не весь список: полный сброс
/// приходился ровно на момент, когда сбои пошли потоком (сеть легла, CDN
/// отдал 429), и клиент тут же забывал всё, что уже признано нерабочим, и
/// начинал качать это заново. Какую именно запись потерять — неважно; важно,
/// что теряется одна, а не 512.
fn remember_failed(failed: &mut std::collections::HashSet<String>, key: String) {
    if failed.len() >= MAX_FAILED_IMAGES {
        if let Some(victim) = failed.iter().next().cloned() {
            failed.remove(&victim);
        }
    }
    failed.insert(key);
}

/// Занять место в лимите одновременных загрузок картинок. `false` — лимит
/// исчерпан, тогда загрузку лучше отложить до следующего кадра (картинка
/// попадёт в кэш и больше не будет качаться заново).
///
/// Счётчик трогаем только если слот реально достался: простое `fetch_add`
/// с последующей проверкой увеличивало счётчик на единицу даже при отказе,
/// и через несколько кадров он уезжал за любой предел навсегда — картинки
/// переставали грузиться вообще.
fn take_image_slot() -> bool {
    IMAGE_DOWNLOADS_IN_FLIGHT
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
            if v < MAX_CONCURRENT_IMAGE_DOWNLOADS {
                Some(v + 1)
            } else {
                None
            }
        })
        .is_ok()
}

fn release_image_slot() {
    // saturating: если счётчик когда-то уйдёт в ноль, лучше он останется
    // нулём, чем завертится и больше не ограничит ничего.
    let _ = IMAGE_DOWNLOADS_IN_FLIGHT.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
        Some(v.saturating_sub(1))
    });
}

/// Слот, который освобождается сам — в том числе если поток упал на
/// распаковке. Иначе падение в одном кадре тихо съедало слот навсегда.
struct ImageSlot;

impl Drop for ImageSlot {
    fn drop(&mut self) {
        release_image_slot();
    }
}

/// Ответ потока загрузки аватара. Различать надо: отказ CDN стоит запомнить,
/// а занятость загрузочного слота — нет (слот освободится через мгновение, и
/// запомнив «битый аватар», мы не показали бы его никогда).
#[derive(Debug, PartialEq)]
pub(crate) enum AvatarFetch {
    Ready(egui::ColorImage),
    /// Не вышло: сеть, битый хеш, 404. Пробовать больше не надо.
    Failed,
    /// Мест в очереди загрузки не было. Повторить можно сразу же.
    Busy,
}

impl App {
    pub(crate) fn download_avatar(&mut self, ctx: &egui::Context, user_id: &str, avatar_hash: &str) -> Option<TextureHandle> {
        let url = format!("{}/avatars/{}/{}.png?size=64", CDN_BASE, user_id, avatar_hash);
        self.fetch_avatar(ctx, format!("{}_{}", user_id, avatar_hash), url)
    }
    /// Забрать аватар или иконку сервера.
    ///
    /// Отказ запоминается: раньше его нигде не хранили, и список `pending`
    /// очищался по ответу потока, поэтому на следующем же кадре ключ снова
    /// не находился в кэше, не находился в списке загрузок — и порождался
    /// новый поток с новым HTTP-запросом. Двадцать раз в секунду на каждый
    /// невидимый аватар (удалённый аккаунт, битый хеш, 429 от CDN), а эти
    /// запросы сами съедали лимит CDN, на который клиент упирался, и
    /// порождали следующую волну.
    ///
    /// Отличать «не вышло» от «ещё не готово» нельзя было и раньше — оба
    /// ответа это `None`. Теперь запоминаем ключ и больше не пробуем, пока
    /// канал не откроется заново.
    fn fetch_avatar(&mut self, ctx: &egui::Context, cache_key: String, url: String) -> Option<TextureHandle> {
        if let Some(tex) = self.avatar_cache.get(&cache_key) {
            return Some(tex.clone());
        }
        if self.failed_avatars.contains(&cache_key) {
            return None;
        }

        let ctx2 = ctx.clone();
        let key = cache_key.clone();

        let pending = self.pending_avatars.entry(cache_key.clone()).or_insert_with(|| {
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let url_moved = url.clone();
            std::thread::spawn(move || {
                if AVATAR_DOWNLOADS_IN_FLIGHT.fetch_add(1, Ordering::SeqCst) >= MAX_CONCURRENT_AVATAR_DOWNLOADS {
                    AVATAR_DOWNLOADS_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
                    // Слоты заняты — это не «аватар битый», и запоминать такой
                    // отказ нельзя: иначе аватар не появился бы никогда, хотя
                    // слоты освободились бы уже на следующем кадре.
                    let _ = result_tx.send(AvatarFetch::Busy);
                    return;
                }
                let outcome = match http().get(&url_moved).send().ok()
                    .and_then(|resp| read_limited(resp, MAX_DOWNLOAD_BYTES))
                    .and_then(|bytes| image::load_from_memory(&bytes).ok())
                {
                    Some(img) => {
                        let rgba = img.to_rgba8();
                        let (w, h) = rgba.dimensions();
                        AvatarFetch::Ready(egui::ColorImage::from_rgba_unmultiplied(
                            [w as usize, h as usize],
                            &rgba.into_raw(),
                        ))
                    }
                    None => AvatarFetch::Failed,
                };
                AVATAR_DOWNLOADS_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
                let _ = result_tx.send(outcome);
            });
            result_rx
        });

        if let Ok(result) = pending.try_recv() {
            // Запись убираем всегда: поток отработал, и держать мёртвый
            // приёмник в таблице незачем. Дальше всё решает память об отказе.
            self.pending_avatars.remove(&key);
            return self.store_avatar(ctx2, key, result);
        }
        None
    }

    /// Принять ответ загрузки аватара. Отдельная функция, а не часть
    /// `fetch_avatar`, потому что именно тут решается судьба отказа, и
    /// проверять это правило нужно без похода в сеть.
    fn store_avatar(
        &mut self,
        ctx: egui::Context,
        key: String,
        answer: AvatarFetch,
    ) -> Option<TextureHandle> {
        match answer {
            AvatarFetch::Ready(color_image) => {
                let handle = ctx.load_texture(&key, color_image, egui::TextureOptions::default());
                self.avatar_cache.insert(key.clone(), handle.clone());
                ctx.request_repaint();
                Some(handle)
            }
            AvatarFetch::Failed => {
                // Отказ по-настоящему: запоминаем ключ, иначе следующий кадр
                // снова не найдёт его ни в кэше, ни в загрузках и породит новый
                // поток с новым запросом (Б-18).
                remember_failed(&mut self.failed_avatars, key);
                None
            }
            // Занятость слота отказом не считается: слот освободится через
            // мгновение, а запомнив ключ, мы не показали бы аватар никогда.
            AvatarFetch::Busy => None,
        }
    }
    pub(crate) fn download_guild_icon(&mut self, ctx: &egui::Context, guild_id: &str, icon_hash: &str) -> Option<TextureHandle> {
        let url = format!("{}/icons/{}/{}.png?size=64", CDN_BASE, guild_id, icon_hash);
        self.fetch_avatar(ctx, format!("guild_icon_{}_{}", guild_id, icon_hash), url)
    }
    /// Распаковать статичную картинку с ограничением по размеру. `Limits`
    /// проверяется до выделения буфера пикселей, поэтому недопустимо
    /// большая картинка отсекается, а не съедает память.
    fn decode_static(bytes: &[u8]) -> Option<image::DynamicImage> {
        use image::ImageReader;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(MAX_SOURCE_PIXELS as u32);
        limits.max_image_height = Some(MAX_SOURCE_PIXELS as u32);
        limits.max_alloc = Some(MAX_SOURCE_PIXELS * 4);
        let mut reader = ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
        reader.limits(limits);
        reader.decode().ok()
    }

    pub(crate) fn decode_image_payload(bytes: &[u8]) -> Option<ImagePayload> {
    if bytes.len() < 6 {
        return None;
    }
    let is_gif = &bytes[..6] == b"GIF89a" || &bytes[..6] == b"GIF87a";
    if is_gif {
        if let Ok(decoder) = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)) {
            use image::{AnimationDecoder, ImageDecoder};
            // Холст проверяем до распаковки: у гифки на 500 кадров по
            // 1000x1000 кадр — это 4 МБ, а все кадры разом (так было
            // раньше, через collect_frames) — 2 ГБ.
            let (cw, ch) = decoder.dimensions();
            if !source_size_allowed(cw, ch) {
                return None;
            }
            let mut frames = decoder.into_frames();
            // Размер кадра выбираем по числу кадров: длинную гифку ужимаем
            // сильнее, но показываем целиком. Раньше потолок был по штукам,
            // и любая гифка длиннее 16 кадров обрывалась на одном и том же
            // месте — при живом оригинале в обычном Discord.
            let frame_dim = gif_frame_count(bytes)
                .map(gif_frame_dim)
                .unwrap_or(MAX_GIF_DIM);
            let mut out = Vec::new();
            let mut bytes_used = 0usize;
            while out.len() < MAX_GIF_FRAMES {
                let Some(Ok(fr)) = frames.next() else { break };
                let (num, den) = fr.delay().numer_denom_ms();
                let secs = if den == 0 {
                    0.1
                } else {
                    (num as f64 / den as f64) / 1000.0
                };
                let buf = fr.into_buffer();
                let (w, h) = buf.dimensions();
                if w == 0 || h == 0 {
                    break;
                }
                let frame = image::DynamicImage::ImageRgba8(buf);
                let rgba = shrink(frame, frame_dim).into_rgba8();
                let (fw, fh) = rgba.dimensions();
                if fw == 0 || fh == 0 {
                    break;
                }
                // Подстраховка на случай, если число кадров по какой-то
                // причине определено неверно: дальше бюджета не пускаем.
                let frame_bytes = fw as usize * fh as usize * 4;
                if out.len() > 1 && bytes_used + frame_bytes > MAX_GIF_BYTES {
                    break;
                }
                bytes_used += frame_bytes;
                let pixels = rgba.into_raw();
                let ci = egui::ColorImage::from_rgba_unmultiplied(
                    [fw as usize, fh as usize],
                    &pixels,
                );
                out.push((ci, secs.max(0.02) as f32));
            }
            // Один кадр — это просто статичная картинка, её разберёт общий
            // путь ниже.
            if out.len() > 1 {
                return Some(ImagePayload::Animated { frames: out });
            }
        }
    }
    if let Some(img) = Self::decode_static(bytes) {
        let img = shrink(img, MAX_IMAGE_DIM);
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        if w == 0 || h == 0 {
            return None;
        }
        let pixels = rgba.into_raw();
        let color_image = egui::ColorImage::from_rgba_unmultiplied(
            [w as usize, h as usize],
            &pixels,
        );
        return Some(ImagePayload::Static(color_image));
    }
    None
}
pub(crate) fn download_image(&mut self, ctx: &egui::Context, url: &str) -> Option<LoadedImage> {
    let cache_key = url.to_string();
    if let Some(img) = self.image_cache.get(&cache_key) {
        return Some(img.clone());
    }
    if self.failed_images.contains(&cache_key) {
        return None;
    }

    let ctx2 = ctx.clone();
    let key = cache_key.clone();

    if contains_ignore_case(url, "/avatars/") || contains_ignore_case(url, "/users/") {
        remember_failed(&mut self.failed_images, key);
        return None;
    }

    // Не начинаем новую загрузку, если лимит уже выбран: лишний поток только
    // зря съест память на декодирование. На следующем кадре попробуем снова.
    if !self.pending_images.contains_key(&cache_key) && !take_image_slot() {
        return None;
    }

    let pending = self.pending_images.entry(cache_key.clone()).or_insert_with(|| {
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let url_moved = url.to_string();
        std::thread::spawn(move || {
            // Слот отпускается на любом выходе, включая падение.
            let _slot = ImageSlot;
            if let Ok(resp) = http().get(&url_moved).send() {
                if let Some(bytes) = read_limited(resp, MAX_DOWNLOAD_BYTES) {
                    if let Some(payload) = Self::decode_image_payload(&bytes) {
                        let _ = result_tx.send(Some(payload));
                        return;
                    }
                }
            }
            let _ = result_tx.send(None);
        });
        result_rx
    });

    match pending.try_recv() {
        Ok(Some(payload)) => {
            self.pending_images.remove(&key);
            return self.store_image_payload(&ctx2, key, payload);
        }
        Ok(None) => {
            self.pending_images.remove(&key);
            remember_failed(&mut self.failed_images, key);
        }
        // Поток ещё работает — подождём следующего кадра.
        Err(std::sync::mpsc::TryRecvError::Empty) => {}
        // Поток умер, не ответив. Слот он уже отпустил сам (в том числе при
        // падении), а запись из карты убираем, иначе картинка больше никогда
        // не попробует скачаться, и память под этот ключ утекает.
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            self.pending_images.remove(&key);
        }
    }

    None
}

/// Превратить готовые пиксели в текстуры и положить в кэш.
///
/// Отдельно от `download_image`, потому что результат нужно уметь забрать и
/// без отрисовки этой картинки: см. `reap_pending_images`.
fn store_image_payload(
    &mut self,
    ctx: &egui::Context,
    key: String,
    payload: ImagePayload,
) -> Option<LoadedImage> {
    match payload {
        ImagePayload::Static(color_image) => {
            let handle = ctx.load_texture(&key, color_image, egui::TextureOptions::LINEAR);
            let loaded = LoadedImage::Static(handle);
            self.image_cache.insert(key.clone(), loaded.clone());
            self.push_debug(format!(
                "IMG: {} в кэше — {} шт, {:.1} МБ текстур",
                key,
                self.image_cache.len(),
                self.image_cache.bytes() as f64 / (1024.0 * 1024.0)
            ));
            ctx.request_repaint();
            Some(loaded)
        }
        ImagePayload::Animated { frames } => {
            let mut handles = Vec::with_capacity(frames.len());
            let mut delays = Vec::with_capacity(frames.len());
            for (i, (ci, delay)) in frames.into_iter().enumerate() {
                let tkey = format!("{}#f{}", key, i);
                handles.push(ctx.load_texture(&tkey, ci, egui::TextureOptions::LINEAR));
                delays.push(delay);
            }
            if handles.len() > 1 {
                let frames_count = handles.len();
                let loaded = LoadedImage::Animated {
                    frames: handles,
                    delays,
                    started: std::time::Instant::now(),
                };
                self.image_cache.insert(key.clone(), loaded.clone());
                self.push_debug(format!(
                    "GIF: {} кадров в кэше — {} шт, {:.1} МБ текстур",
                    frames_count,
                    self.image_cache.len(),
                    self.image_cache.bytes() as f64 / (1024.0 * 1024.0)
                ));
                ctx.request_repaint();
                Some(loaded)
            } else {
                None
            }
        }
    }
}

/// Забрать готовые загрузки, даже если их картинок сейчас не видно.
///
/// Раньше готовый результат забирался только при отрисовке этой картинки.
/// Если картинка успевала прокрутиться за экран, её распакованные пиксели
/// навсегда оставались в `pending_images`: загрузок одновременно мало, но
/// каждая гифка — это мегабайты, и на длинном чате набегали сотни мегабайт.
/// Забираем всё готовое в кэш, где память ограничена бюджетом и LRU.
pub(crate) fn reap_pending_images(&mut self, ctx: &egui::Context) {
    if self.pending_images.is_empty() {
        return;
    }
    // Забираем карту целиком, чтобы не копировать ключи на каждом кадре;
    // незавершённые загрузки возвращаем на место.
    for (key, rx) in std::mem::take(&mut self.pending_images) {
        match rx.try_recv() {
            Ok(Some(payload)) => {
                self.store_image_payload(ctx, key, payload);
            }
            Ok(None) => {
                remember_failed(&mut self.failed_images, key);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.pending_images.insert(key, rx);
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
        }
    }
}
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageEncoder, Rgb, RgbImage, Rgba, RgbaImage};

    /// Тесты, которые трогают счётчик загрузок, идут по очереди: он общий
    /// на процесс, и параллельный прогон сломал бы проверки.
    static SLOTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn plain_app() -> App {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(rx)
    }

    /// Собрать настоящий GIF заданного размера и числа кадров — так же, как
    /// его собрал бы Discord, чтобы проверять распаковку на живых данных.
    fn encode_test_gif(w: u32, h: u32, n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = image::codecs::gif::GifEncoder::new(&mut out);
            for f in 0..n {
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

    /// Отказ по аватару не должен приводить к новому запросу на каждом кадре.
    ///
    /// Сценарий: аккаунт удалён, хеш битый или CDN отдал 429. Прежде ответ
    /// потока означал только «не вышло», запись убиралась из списка загрузок —
    /// и на следующем кадре ключ снова не находился ни в кэше, ни в загрузках,
    /// поэтому порождался новый поток с новым HTTP-запросом. Двадцать раз в
    /// секунду на каждый невидимый аватар, а эти запросы сами съедали лимит
    /// CDN, на который клиент упирался, и порождали следующую волну отказов.
    #[test]
    fn failed_avatar_is_not_requested_again_on_every_frame() {
        let ctx = egui::Context::default();
        let mut app = plain_app();
        let (tx, rx) = std::sync::mpsc::channel::<AvatarFetch>();
        tx.send(AvatarFetch::Failed).unwrap();
        app.pending_avatars.insert("u1_deadbeef".into(), rx);

        // Кадр, на котором поток отработал отказом: запись из загрузок ушла...
        let answer = app.pending_avatars["u1_deadbeef"].try_recv();
        assert_eq!(answer, Ok(AvatarFetch::Failed));
        app.pending_avatars.remove("u1_deadbeef");
        assert!(app.store_avatar(ctx.clone(), "u1_deadbeef".into(), AvatarFetch::Failed).is_none());

        // ...и больше нигде key не лежит: не в кэше, не в загрузках. Единственное,
        // что остановит новый поток на следующем кадре, — память об отказе.
        assert!(app.avatar_cache.get("u1_deadbeef").is_none());
        assert!(!app.pending_avatars.contains_key("u1_deadbeef"));
        assert!(
            app.failed_avatars.contains("u1_deadbeef"),
            "отказ не запомнен — следующий кадр снова спросит аватар: {:?}",
            app.failed_avatars
        );
    }

    /// Занятость загрузочного слота отказом считаться не должна. Если
    /// запомнить и её, то аватар, не поместившийся в лимит однажды, не
    /// появился бы уже никогда — а слот освобождается через мгновение.
    #[test]
    fn busy_avatar_slot_is_not_remembered_as_failure() {
        let ctx = egui::Context::default();
        let mut app = plain_app();
        assert!(app.store_avatar(ctx, "u1_cafe".into(), AvatarFetch::Busy).is_none());
        assert!(
            !app.failed_avatars.contains("u1_cafe"),
            "занятость слота — не отказ, ключ нельзя запоминать: {:?}",
            app.failed_avatars
        );
        assert!(!app.pending_avatars.contains_key("u1_cafe"));
    }

    /// Успех должен класть текстуру в кэш: иначе следующий кадр запросил бы
    /// тот же аватар заново, уже скачав его.
    #[test]
    fn ready_avatar_goes_to_cache_and_is_forgotten_as_pending() {
        let ctx = egui::Context::default();
        let mut app = plain_app();
        let img = egui::ColorImage::new([8, 8], egui::Color32::RED);
        assert!(app.store_avatar(ctx.clone(), "u1_ok".into(), AvatarFetch::Ready(img)).is_some());
        assert!(app.avatar_cache.get("u1_ok").is_some());
        assert!(!app.failed_avatars.contains("u1_ok"), "удавшийся аватар не в списке отказов");
    }

    /// Иконки серверов берутся по тому же пути, значит и отказ у них должен
    /// запоминаться. Раньше у аватарок и иконок не было ничего общего: две
    /// копии одной и той же функции, и починка одной другой не касалась бы.
    #[test]
    fn guild_icon_failure_is_remembered_too() {
        let ctx = egui::Context::default();
        let mut app = plain_app();
        assert!(app
            .store_avatar(ctx, "guild_icon_g1_abc".into(), AvatarFetch::Failed)
            .is_none());
        assert!(
            app.failed_avatars.contains("guild_icon_g1_abc"),
            "иконка сервера должна запоминаться так же, как аватар"
        );
    }

    /// Память об отказах не должна расти без предела: аватары приходят с
    /// новыми хешами, и ключи никогда не повторяются. При достижении предела
    /// вытесняется одна запись — старые ключи постепенно уходят.
    #[test]
    fn failed_avatars_are_bounded() {
        let ctx = egui::Context::default();
        let mut app = plain_app();
        for i in 0..(crate::app::MAX_FAILED_IMAGES + 10) {
            assert!(app
                .store_avatar(ctx.clone(), format!("u{i}_hash"), AvatarFetch::Failed)
                .is_none());
        }
        assert!(
            app.failed_avatars.len() <= crate::app::MAX_FAILED_IMAGES,
            "память об отказах выросла без предела: {}",
            app.failed_avatars.len()
        );
        // Старые ключи постепенно вытесняются — но свежие запомнены.
        assert!(app.failed_avatars.len() > 0, "список отказов не должен опустеть");
    }

    /// Переполнение памяти об отказах не должно стирать всё разом: иначе в
    /// самый разгар массового сбоя (сеть легла, CDN отдал 429) клиент забывает
    /// всё, что уже признано нерабочим, и начинает качать это заново.
    #[test]
    fn failed_avatars_are_evicted_one_by_one() {
        let ctx = egui::Context::default();
        let mut app = plain_app();
        let max = crate::app::MAX_FAILED_IMAGES;
        for i in 0..max {
            app.store_avatar(ctx.clone(), format!("u{i}_h"), AvatarFetch::Failed);
        }
        // Переполняем ровно на одну запись.
        app.store_avatar(ctx.clone(), "overflow_hash".into(), AvatarFetch::Failed);

        let survivors = (0..max)
            .filter(|i| app.failed_avatars.contains(&format!("u{i}_h")))
            .count();
        assert!(
            survivors >= max - 1,
            "переполнение стёрло больше одной записи: выжило {survivors} из {max}"
        );
    }

    /// «Это аватарка?» должно опознаваться независимо от регистра — как
    /// раньше через `to_lowercase().contains(...)` — но без создания строки
    /// на каждый кадр и каждую картинку.
    #[test]
    fn avatar_url_detection_is_case_insensitive() {
        let is_avatar =
            |u: &str| contains_ignore_case(u, "/avatars/") || contains_ignore_case(u, "/users/");
        for url in [
            "https://cdn.discordapp.com/avatars/1/hash.png",
            "https://cdn.discordapp.com/AVATARS/1/hash.png",
            "https://cdn.discordapp.com/Users/1/hash.png",
            "https://example.com/USERS/1/hash.png",
        ] {
            assert!(is_avatar(url), "ссылка должна опознаваться как аватарка: {url}");
            assert_eq!(
                is_avatar(url),
                url.to_lowercase().contains("/avatars/") || url.to_lowercase().contains("/users/"),
                "регистронезависимый поиск разошёлся со старым: {url}"
            );
        }
        assert!(!is_avatar("https://example.com/pic.png"), "обычная картинка — не аватарка");
    }

    /// Отказ занять слот не должен увеличивать счётчик: иначе он уезжает
    /// вверх на единицу за каждый отказ, за пару кадров уходит за любой
    /// предел и картинки не грузятся больше никогда.
    #[test]
    fn refused_download_does_not_eat_slot() {
        let _guard = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        while take_image_slot() {}
        let busy = IMAGE_DOWNLOADS_IN_FLIGHT.load(Ordering::SeqCst);
        for _ in 0..100 {
            assert!(!take_image_slot(), "слоты выдали сверх лимита");
        }
        assert_eq!(
            IMAGE_DOWNLOADS_IN_FLIGHT.load(Ordering::SeqCst),
            busy,
            "отказ занял слот — счётчик уехал вверх"
        );
        release_image_slot();
        assert!(take_image_slot(), "освобождённый слот не вернулся");
        // Тест не должен влиять на остальные: возвращаем счётчик в ноль.
        IMAGE_DOWNLOADS_IN_FLIGHT.store(0, Ordering::SeqCst);
    }

    /// Локальный сервер, отдающий картинку с задержкой: видно, что загрузок
    /// идёт больше, чем одновременных слотов.
    fn slow_png_server(bytes: Vec<u8>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("не занять порт");
        let addr = listener.local_addr().unwrap().to_string();
        let bytes = std::sync::Arc::new(bytes);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let bytes = bytes.clone();
                std::thread::spawn(move || {
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf);
                    std::thread::sleep(Duration::from_millis(40));
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&bytes);
                    let _ = stream.flush();
                });
            }
        });
        addr
    }

    /// Настоящий путь загрузки целиком: HTTP → распаковка → кэш. Картинок
    /// специально больше, чем лимит одновременных загрузок: когда отказ
    /// занимал слот, счётчик уезжал вверх и после первой тройки не
    /// грузилось вообще ничего.
    #[test]
    fn more_images_than_slots_all_load() {
        let _guard = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        let img = RgbImage::from_pixel(8, 8, Rgb([200, 30, 30]));
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(img.as_raw(), 8, 8, image::ExtendedColorType::Rgb8)
            .unwrap();
        let addr = slow_png_server(png);
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(rx);
        let ctx = egui::Context::default();
        let urls: Vec<String> = (0..MAX_CONCURRENT_IMAGE_DOWNLOADS + 3)
            .map(|i| format!("http://{}/{}.png", addr, i))
            .collect();

        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut loaded = 0;
        while std::time::Instant::now() < deadline {
            for u in &urls {
                let _ = app.download_image(&ctx, u);
            }
            loaded = urls
                .iter()
                .filter(|u| app.image_cache.get(u).is_some())
                .count();
            if loaded == urls.len() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(loaded, urls.len(), "докачались не все картинки");
        assert!(
            app.pending_images.is_empty(),
            "в очереди остались висящие загрузки: {:?}",
            app.pending_images.len()
        );
        assert_eq!(
            IMAGE_DOWNLOADS_IN_FLIGHT.load(Ordering::SeqCst),
            0,
            "слоты загрузки не вернулись"
        );
    }

    /// Готовая загрузка не должна висеть в очереди, если её картинку
    /// прокрутили за экран и больше не рисуют.
    ///
    /// Раньше результат забирался только при отрисовке. Картинка, которую
    /// успели пролистать, оставалась в `pending_images` вместе с
    /// распакованными пикселями: загрузок одновременно мало, но каждая гифка
    /// — это мегабайты, и на длинном чате набегали сотни мегабайт.
    #[test]
    fn completed_download_for_scrolled_away_image_is_reaped() {
        let ctx = egui::Context::default();
        let mut app = plain_app();
        let key = "https://cdn.discordapp.com/attachments/1/off.png".to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Some(ImagePayload::Static(egui::ColorImage::new(
            [8, 8],
            egui::Color32::RED,
        ))))
        .unwrap();
        app.pending_images.insert(key.clone(), rx);

        // Кадр, на котором эту картинку никто не рисует (канал не открыт).
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run(raw, |ctx| app.run_frame(ctx));

        assert!(
            !app.pending_images.contains_key(&key),
            "готовый результат остался висеть в очереди"
        );
        assert!(
            app.image_cache.contains_key(&key),
            "готовую картинку нужно забрать в кэш, где память ограничена"
        );
    }

    /// Большая картинка должна уменьшаться до лимита, иначе одна фотка
    /// в 12 МБ съедает столько же VRAM/RAM.
    #[test]
    fn big_image_is_shrunk() {
        let img = RgbImage::from_pixel(3000, 2000, Rgb([10, 20, 30]));
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(img.as_raw(), 3000, 2000, image::ExtendedColorType::Rgb8)
            .unwrap();
        eprintln!("[TEST] исходный PNG: {} КБ", png.len() / 1024);

        match App::decode_image_payload(&png) {
            Some(ImagePayload::Static(ci)) => {
                assert!(
                    ci.size[0] <= MAX_IMAGE_DIM as usize && ci.size[1] <= MAX_IMAGE_DIM as usize,
                    "картинка не ужата: {:?}",
                    ci.size
                );
                // Пропорции сохранены.
                let ratio = ci.size[0] as f32 / ci.size[1] as f32;
                assert!((ratio - 1.5).abs() < 0.05, "пропорции сломаны: {}", ratio);
                eprintln!("[TEST] после ужатия: {:?}", ci.size);
            }
            other => panic!("ожидалась статичная картинка, получено {:?}", other.is_some()),
        }
    }

    /// Маленькую картинку трогать не надо.
    #[test]
    fn small_image_keeps_size() {
        let img = RgbImage::new(64, 48);
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(img.as_raw(), 64, 48, image::ExtendedColorType::Rgb8)
            .unwrap();
        match App::decode_image_payload(&png) {
            Some(ImagePayload::Static(ci)) => assert_eq!(ci.size, [64, 48]),
            other => panic!("ожидалась статичная картинка, получено {:?}", other.is_some()),
        }
    }

    /// Гифка играет все свои кадры, а не обрывается на середине.
    ///
    /// Раньше был жёсткий потолок в 16 кадров, и любая гифка длиннее
    /// шестнадцати обрывалась на одном и том же месте, хотя оригинал в
    /// Discord доигрывал до конца. Границей должна быть память, а не число
    /// кадров: 30 кадров 240x160 — это меньше 5 МБ.
    #[test]
    fn gif_plays_all_its_frames() {
        let (w, h, n) = (240u32, 160u32, 30usize);
        let out = encode_test_gif(w, h, n);
        eprintln!("[TEST] GIF {}x{} {} кадров = {} КБ", w, h, n, out.len() / 1024);

        assert_eq!(gif_frame_count(&out), Some(n), "разбор структуры GIF врёт");

        match App::decode_image_payload(&out) {
            Some(ImagePayload::Animated { frames }) => {
                assert_eq!(frames.len(), n, "гифка обрезана: {} из {}", frames.len(), n);
                for (ci, _) in &frames {
                    assert!(
                        ci.size[0] <= MAX_GIF_DIM as usize && ci.size[1] <= MAX_GIF_DIM as usize,
                        "кадр не ужат: {:?}",
                        ci.size
                    );
                }
                eprintln!("[TEST] кадров: {}, размер {:?}", frames.len(), frames[0].0.size);
            }
            other => panic!("ожидалась анимация, получено {:?}", other.is_some()),
        }
    }

    /// Длинная гифка ужимается, но доигрывает до последнего кадра и остаётся
    /// в бюджете памяти.
    #[test]
    fn long_gif_is_downscaled_but_complete() {
        let (w, h, n) = (400u32, 400u32, 120usize);
        let out = encode_test_gif(w, h, n);
        let total: usize = match App::decode_image_payload(&out) {
            Some(ImagePayload::Animated { frames }) => {
                assert_eq!(frames.len(), n, "гифка обрезана: {} из {}", frames.len(), n);
                frames.iter().map(|(ci, _)| ci.size[0] * ci.size[1] * 4).sum()
            }
            other => panic!("ожидалась анимация, получено {:?}", other.is_some()),
        };
        assert!(
            total <= MAX_GIF_BYTES,
            "кадры гифки не влезли в бюджет: {} байт",
            total
        );
        eprintln!(
            "[TEST] 120 кадров 400x400: {} МБ, размер кадра {:?}",
            total as f64 / (1024.0 * 1024.0),
            gif_frame_dim(n)
        );
    }

    /// Размер кадра из формулы всегда держит все кадры в бюджете и не
    /// выходит за границы разумного.
    #[test]
    fn gif_frame_dim_stays_within_budget() {
        for n in [1usize, 10, 30, 60, 120, 300, 512, 1000, 2000, 100_000] {
            let dim = gif_frame_dim(n);
            assert!(
                (MIN_GIF_DIM..=MAX_GIF_DIM).contains(&dim),
                "размер {} вне границ при {} кадрах",
                dim,
                n
            );
            // Пока не упёрлись в минимум, в бюджет обязаны влезать.
            if dim > MIN_GIF_DIM {
                let effective = n.clamp(1, MAX_GIF_FRAMES);
                assert!(
                    effective * (dim as usize) * (dim as usize) * 4 <= MAX_GIF_BYTES,
                    "{} кадров по {} пикселей не влезают",
                    effective,
                    dim
                );
            }
        }
        // Обычные гифки не трогаем: полное разрешение.
        assert_eq!(gif_frame_dim(30), MAX_GIF_DIM);
    }

    /// Разбор структуры GIF не должен ломаться на не-GIF данных.
    #[test]
    fn gif_frame_count_rejects_garbage() {
        assert_eq!(gif_frame_count(b""), None);
        assert_eq!(gif_frame_count(b"not a gif at all"), None);
    }

    /// Гифка с одним кадром — это просто картинка, анимацией она не
    /// считается.
    #[test]
    fn single_frame_gif_is_static() {
        let mut out = Vec::new();
        {
            let mut enc = image::codecs::gif::GifEncoder::new(&mut out);
            enc.encode_frame(image::Frame::from_parts(
                RgbaImage::from_pixel(64, 64, Rgba([1, 2, 3, 255])),
                0,
                0,
                image::Delay::from_numer_denom_ms(50, 1),
            )).unwrap();
        }
        match App::decode_image_payload(&out) {
            Some(ImagePayload::Static(ci)) => assert_eq!(ci.size, [64, 64]),
            other => panic!("ожидалась статичная картинка, получено {:?}", other.is_some()),
        }
    }

    /// Гифка, холст которой больше лимита, не распаковывается вовсе: кадры
    /// читаются по одному, но и один кадр такой — это сотни мегабайт.
    #[test]
    fn oversized_gif_is_refused() {
        // Заголовок 10000x10000 = 100 Мпикс, больше MAX_SOURCE_PIXELS.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"GIF89a");
        bytes.extend_from_slice(&10000u16.to_le_bytes());
        bytes.extend_from_slice(&10000u16.to_le_bytes());
        bytes.push(0); // без глобальной таблицы
        bytes.push(0x21); // расширение
        bytes.push(0xF9); // Graphic Control
        bytes.extend_from_slice(&[4, 0, 0, 0, 0, 0, 0, 0]);
        bytes.push(0x3B); // конец
        assert!(
            App::decode_image_payload(&bytes).is_none(),
            "гифка неподходящего размера не должна распаковываться"
        );
    }

    /// Слишком большая статичная картинка отсекается до выделения памяти.
    #[test]
    fn static_size_limit_is_respected() {
        // Граница лимита: 39.9 Мпикс ещё можно, 42 — уже нет.
        assert!(source_size_allowed(7000, 5700), "39.9 Мпикс должны помещаться");
        assert!(!source_size_allowed(7000, 6000), "42 Мпикс уже не помещаются");
        assert!(source_size_allowed(64, 64));

        // PNG, который только заголовком обещает 10000x10000: распаковывать
        // его нельзя, лимит проверяется до выделения буфера пикселей.
        let mut png = Vec::new();
        png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&10000u32.to_be_bytes());
        ihdr.extend_from_slice(&10000u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // RGBA8
        push_chunk(&mut png, b"IHDR", &ihdr);
        push_chunk(&mut png, b"IDAT", &[0x78, 0x01, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01]);
        push_chunk(&mut png, b"IEND", &[]);
        assert!(
            App::decode_image_payload(&png).is_none(),
            "картинка недопустимого размера не должна распаковываться"
        );
    }

    fn push_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        // CRC считать не нужно: до данных дело не дойдёт, лимит отсечёт
        // картинку по заголовку.
        out.extend_from_slice(&[0, 0, 0, 0]);
    }

    /// Сервер, отдающий тело заданного размера. Нужен, чтобы проверить
    /// потолок на чтение ответа, не скачивая настоящие гигабайты.
    fn body_server(total: usize) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                total
            );
            if stream.write_all(head.as_bytes()).is_err() {
                return;
            }
            let chunk = vec![0u8; 64 * 1024];
            let mut sent = 0;
            while sent < total {
                let n = (total - sent).min(chunk.len());
                if stream.write_all(&chunk[..n]).is_err() {
                    return;
                }
                sent += n;
            }
            let _ = stream.flush();
        });
        addr
    }

    /// Тело ответа читается с потолком: ссылку на картинку в эмбеде задаёт
    /// чужой сайт, и без потолка его «картинка» на полгигабайта оседала бы в
    /// памяти ещё до того, как распаковщик проверит размер.
    #[test]
    fn oversized_response_body_is_refused() {
        let addr = body_server(200);
        let resp = http().get(format!("http://{}/small.png", addr)).send().unwrap();
        let small = read_limited(resp, 1024).expect("маленькое тело должно прочитаться");
        assert_eq!(small.len(), 200);

        let addr = body_server(8 * 1024 * 1024);
        let resp = http().get(format!("http://{}/huge.png", addr)).send().unwrap();
        assert!(
            read_limited(resp, 4096).is_none(),
            "тело сверх потолка должно отсекаться, а не читаться целиком"
        );
    }
}

#[cfg(test)]
impl App {
    /// Тот же путь, что и в `download_image` после получения ответа, только
    /// без сети: декодируем байты и кладём результат в кэш по его политике.
    pub(crate) fn cache_image_bytes(&mut self, ctx: &egui::Context, key: &str, bytes: &[u8]) -> bool {
        match Self::decode_image_payload(bytes) {
            Some(ImagePayload::Static(color_image)) => {
                let handle = ctx.load_texture(key, color_image, egui::TextureOptions::LINEAR);
                self.image_cache
                    .insert(key.to_string(), LoadedImage::Static(handle));
                true
            }
            Some(ImagePayload::Animated { frames }) => {
                let mut handles = Vec::with_capacity(frames.len());
                let mut delays = Vec::with_capacity(frames.len());
                for (i, (ci, delay)) in frames.into_iter().enumerate() {
                    handles.push(ctx.load_texture(format!("{}#f{}", key, i), ci, egui::TextureOptions::LINEAR));
                    delays.push(delay);
                }
                if handles.len() > 1 {
                    self.image_cache.insert(
                        key.to_string(),
                        LoadedImage::Animated { frames: handles, delays, started: std::time::Instant::now() },
                    );
                    true
                } else {
                    false
                }
            }
            None => false,
        }
    }
}
