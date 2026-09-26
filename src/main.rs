use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

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

fn main() -> eframe::Result<()> {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("[PANIC] {}", info);
        let _ = std::fs::write("/tmp/discord_panic.log", format!("{:?}", info));
    }));

    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_min_inner_size([700.0, 500.0])
            .with_title("Wyvern"),
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
