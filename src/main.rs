#[cfg(not(test))]
use mimalloc::MiMalloc;

#[cfg(not(test))]
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[cfg(test)]
mod memcheck;

mod app;
mod crypto;
mod gateway;
mod media;
mod messages;
mod models;
mod ui;
mod util;

use eframe::egui;
use tokio::sync::mpsc;

use crate::app::App;

/// Путь к логу паники с номером: `/tmp/wyvern_panic_N.log`.
///
/// Номер нужен, потому что один и тот же файл затирал предыдущее падение, а
/// серия падений подряд — как раз то, ради чего лог и открывают. Имя с
/// «wyvern»: по нему должно находиться, когда ищешь следы этого клиента.
fn panic_log_path(n: usize) -> String {
    format!("/tmp/wyvern_panic_{}.log", n)
}

/// Первый свободный номер для лога паники (до 99).
fn next_panic_log_path() -> String {
    (1..=99)
        .map(panic_log_path)
        .find(|p| !std::path::Path::new(p).exists())
        .unwrap_or_else(|| panic_log_path(99))
}

fn main() -> eframe::Result<()> {
    // Панику пишем в /tmp/wyvern_panic_N.log (N — первый свободный): серия
    // падений подряд не должна оставлять только последнее. Имя с «wyvern»,
    // а не «discord»: лог паники ищут вместе с остальными следами клиента.
    // Стандартный хук сохраняем — он печатает привычную подсказку, а бэктрейс
    // кладём в файл сами, чтобы он был независимо от RUST_BACKTRACE.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        let path = next_panic_log_path();
        let backtrace = std::backtrace::Backtrace::force_capture();
        let _ = std::fs::write(&path, format!("{:?}\n{}", info, backtrace));
        eprintln!("[PANIC] записано в {}", path);
    }));

    // Версия в заголовке: бинарник в корне репозитория пересобирают не
    // всегда, и по заголовку сразу видно, какой именно запущен.
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_min_inner_size([700.0, 500.0])
            .with_title(format!("Wyvern {}", env!("CARGO_PKG_VERSION"))),
        ..Default::default()
    };

    let (_, rx) = mpsc::unbounded_channel();
    let app = App::new(rx);

    eframe::run_native(
        "wyvern",
        opts,
        Box::new(|_cc| Ok(Box::new(app))),
    )
}

#[cfg(test)]
mod tests {
    use super::{next_panic_log_path, panic_log_path};

    /// Имя лога паники — «wyvern», а не «discord», и с номером: иначе серия
    /// падений подряд оставляет только последнее.
    #[test]
    fn panic_log_is_named_and_numbered() {
        let first = panic_log_path(1);
        assert!(
            first.ends_with("wyvern_panic_1.log"),
            "лог паники должен называться по проекту и номеру: {first}"
        );
        assert!(!first.contains("discord"), "имя осталось от прежнего проекта: {first}");
        assert_ne!(panic_log_path(1), panic_log_path(2), "номер не влияет на путь");

        let next = next_panic_log_path();
        assert!(next.ends_with(".log"), "нужен путь к файлу, а не что-то ещё: {next}");
    }
}
