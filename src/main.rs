// Hide the console window on Windows release builds; harmless elsewhere.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(test))]
use mimalloc::MiMalloc;

#[cfg(not(test))]
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[cfg(test)]
mod memcheck;

mod app;
mod crypto;
mod deleted;
mod gateway;
mod media;
mod messages;
mod models;
mod net;
mod paths;
mod settings;
mod ss;
mod proxybridge;
mod vless;
mod ui;
mod util;

use eframe::egui;
use tokio::sync::mpsc;

use crate::app::App;
use crate::deleted::DeletedStore;
use crate::settings::Settings;

/// Panic logs live in the platform cache dir (`paths::log_dir`), numbered so a
/// series of panics is kept instead of overwriting the previous one.
fn main() -> eframe::Result<()> {
    // Move pre-0.9.2 `~/.wyvern_*` files into the platform data dir before any
    // state is read (settings, vault, deleted history).
    crate::paths::migrate_from_home();
    // Panics go to panic_N.log (first free N) so a series is not reduced to the
    // last one. Keep the default hook for the usual hint and write the backtrace
    // ourselves, independent of RUST_BACKTRACE.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        let path = crate::paths::next_panic_log_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let backtrace = std::backtrace::Backtrace::force_capture();
        let _ = std::fs::write(&path, format!("{:?}\n{}", info, backtrace));
        eprintln!("[PANIC] записано в {}", path.display());
    }));

    // Version in the title: the repo-root binary is not always rebuilt.
    // Restore the window size from the last session, if it was saved.
    let settings_path = Settings::path();
    let settings = Settings::load_from(&settings_path);
    let mut viewport = egui::ViewportBuilder::default()
        .with_min_inner_size([700.0, 500.0])
        .with_title(format!("Wyvern {}", env!("CARGO_PKG_VERSION")));
    if let Some([w, h]) = settings.window {
        if w >= 700.0 && h >= 500.0 {
            viewport = viewport.with_inner_size([w, h]);
        }
    }
    let opts = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let (_, rx) = mpsc::unbounded_channel();
    let deleted_path = DeletedStore::path();
    let deleted = DeletedStore::load_from(&deleted_path);
    let app = App::new(rx)
        .with_settings(settings, Some(settings_path))
        .with_deleted_store(deleted, Some(deleted_path));

    eframe::run_native(
        "wyvern",
        opts,
        Box::new(|_cc| Ok(Box::new(app))),
    )
}

#[cfg(test)]
mod tests {
    use crate::paths::{next_panic_log_path, panic_log_path};

    /// Panic logs are numbered and live under the app's cache directory.
    #[test]
    fn panic_log_is_named_and_numbered() {
        let first = panic_log_path(1);
        assert_eq!(
            first.file_name().unwrap().to_str().unwrap(),
            "panic_1.log",
            "лог паники должен называться по номеру"
        );
        assert!(!first.to_string_lossy().contains("discord"), "имя осталось от прежнего проекта");
        assert_ne!(panic_log_path(1), panic_log_path(2), "номер не влияет на путь");

        let next = next_panic_log_path();
        assert_eq!(next.extension().unwrap().to_str().unwrap(), "log", "нужен путь к файлу, а не что-то ещё");
    }
}
