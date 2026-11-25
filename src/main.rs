// Закомментировать эту строку для профилирования (чтобы видеть консоль):
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod i18n;
mod image;
mod utils;
mod video;
mod upload;
mod app;

use app::{ScreenlistApp, setup_custom_style};

fn main() -> eframe::Result<()> {
    video::ffmpeg::init();

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("⚡ Генератор скринлиста PRO")
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([960.0, 620.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };

    eframe::run_native(
        "screenlist-rs",
        options,
        Box::new(|cc| {
            setup_custom_style(&cc.egui_ctx);
            Box::new(ScreenlistApp::new())
        }),
    )
}
