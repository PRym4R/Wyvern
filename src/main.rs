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

/// Numbered panic log path: `/tmp/wyvern_panic_N.log`. Numbering keeps a
/// series of panics instead of overwriting the previous one.
fn panic_log_path(n: usize) -> String {
    format!("/tmp/wyvern_panic_{}.log", n)
}

/// First free panic log number (up to 99).
fn next_panic_log_path() -> String {
    (1..=99)
        .map(panic_log_path)
        .find(|p| !std::path::Path::new(p).exists())
        .unwrap_or_else(|| panic_log_path(99))
}

fn main() -> eframe::Result<()> {
    // Panics go to /tmp/wyvern_panic_N.log (first free N) so a series is not
    // reduced to the last one. Keep the default hook for the usual hint and
    // write the backtrace ourselves, independent of RUST_BACKTRACE.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        let path = next_panic_log_path();
        let backtrace = std::backtrace::Backtrace::force_capture();
        let _ = std::fs::write(&path, format!("{:?}\n{}", info, backtrace));
        eprintln!("[PANIC] записано в {}", path);
    }));

    // Version in the title: the repo-root binary is not always rebuilt.
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

    /// Panic log name must be project-specific and numbered.
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
