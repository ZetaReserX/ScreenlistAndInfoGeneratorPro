use eframe::egui;

pub fn copy_text_to_clipboard(ctx: &egui::Context, text: &str) {
    ctx.copy_text(text.to_owned());
    #[cfg(target_os = "windows")]
    crate::video::win_cpu::set_clipboard_text(text);
}
