use eframe::egui;
use image::RgbImage;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use crate::i18n::Language;

#[derive(Clone)]
pub struct CapturedFrame {
    pub timestamp: f64,
    pub rgb: Arc<RgbImage>,
}

#[derive(Clone, PartialEq)]
pub enum VideoStatus {
    Ready,
    Processing,
    Done(Duration),
    Error(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PreviewTab {
    Screenlist,
    Screenshots,
}

pub struct VideoItem {
    pub path: PathBuf,
    pub canonical_path: Option<PathBuf>,
    pub content_hash: u64,
    pub file_name: String,
    pub file_size: u64,
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub video_info_line: String,
    pub audio_info_line: String,
    pub video_info_line_en: String,
    pub audio_info_line_en: String,
    pub status: VideoStatus,
    pub frames: Vec<CapturedFrame>,
    pub screenshots: Vec<CapturedFrame>,
    pub active_screenshot_idx: usize,
    pub screenshot_texture: Option<(usize, egui::TextureHandle)>,
    pub sheet_image: Option<Arc<RgbImage>>,
    pub sheet_texture: Option<egui::TextureHandle>,
    pub last_cols: usize,
    pub last_thumb_w: u32,
    pub last_thumb_h: u32,
}

impl VideoItem {
    pub fn video_info(&self, lang: Language) -> &str {
        match lang {
            Language::Ru => &self.video_info_line,
            Language::En => &self.video_info_line_en,
        }
    }

    pub fn audio_info(&self, lang: Language) -> &str {
        match lang {
            Language::Ru => &self.audio_info_line,
            Language::En => &self.audio_info_line_en,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CoreMode {
    Auto,
    POnly,
    EOnly,
}

#[derive(Clone, Copy)]
pub struct RenderParams {
    pub frame_count: usize,
    pub screenshot_count: usize,
    pub columns: usize,
    pub grid_width: u32,
    pub workers: usize,
    pub core_mode: CoreMode,
    pub show_timestamps: true_or_bool::BoolType,
    pub turbo_keyframe: bool,
    pub hwaccel: bool,
    pub move_video_to_folder: bool,
    pub jpeg_quality: u8,
    pub lang: Language,
}

pub mod true_or_bool {
    pub type BoolType = bool;
}

#[derive(Clone, Copy, Default)]
pub struct WorkerSlot {
    pub affinity_mask: usize,
    pub is_p_core: bool,
}
