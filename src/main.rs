#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use anyhow::{bail, Context, Result as AnyResult};
use eframe::egui;
use futures_util::stream::{self, StreamExt};
use image::{codecs::jpeg::JpegEncoder, Rgb, RgbImage};
use rayon::prelude::*;
use reqwest::{multipart, Client};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc::{self, Receiver, Sender},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use walkdir::WalkDir;

const MAX_WORKERS: usize = 32;
const MAX_FRAMES_PER_DECODER_TASK: usize = 3;
const ZIP_ENCODE_BATCH_SIZE: usize = 4;

const UPLOAD_URL: &str = "https://new.fastpic.org/upload";
const ANONYMOUS_UPLOAD_URL: &str = "https://fastpic.org/upload?api=1";
const MAX_CONCURRENT_UPLOADS: usize = 4;

const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "webm", "flv", "ts", "m2ts", "mts", "m4v", "wmv", "mpg", "mpeg",
    "vob", "3gp", "3g2", "ogv", "asf", "rm", "rmvb", "divx", "f4v", "mxf",
];

fn main() -> eframe::Result<()> {
    libav::init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
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

fn setup_custom_style(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(22, 24, 34);
    ctx.set_visuals(visuals);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Ru,
    En,
}

impl Language {
    #[inline]
    fn tr<'a>(self, ru: &'a str, en: &'a str) -> &'a str {
        match self {
            Self::Ru => ru,
            Self::En => en,
        }
    }

    fn text(self, key: &str) -> &'static str {
        match (self, key) {
            (Self::Ru, "subtitle") => "Скринлисты и скриншоты",
            (Self::En, "subtitle") => "Screenlists and screenshots",
            (Self::Ru, "add_images") => "Добавить изображения",
            (Self::En, "add_images") => "Add images",
            (Self::Ru, "add_folder") => "Добавить папку с изображениями",
            (Self::En, "add_folder") => "Add image folder",
            (Self::Ru, "clear") => "Очистить список",
            (Self::En, "clear") => "Clear list",
            (Self::Ru, "upload") => "Загрузить на FastPic",
            (Self::En, "upload") => "Upload to FastPic",
            (Self::Ru, "settings") => "Настройки FastPic",
            (Self::En, "settings") => "FastPic settings",
            (Self::Ru, "api_key") => "API-ключ",
            (Self::En, "api_key") => "API key",
            (Self::Ru, "own_key") => "Свой ключ",
            (Self::En, "own_key") => "Own key",
            (Self::Ru, "anonymous") => "Анонимная загрузка",
            (Self::En, "anonymous") => "Anonymous upload",
            (Self::Ru, "show") => "Показать",
            (Self::En, "show") => "Show",
            (Self::Ru, "hide") => "Скрыть",
            (Self::En, "hide") => "Hide",
            (Self::Ru, "thumbnail") => "Миниатюра",
            (Self::En, "thumbnail") => "Thumbnail",
            (Self::Ru, "make_thumbnail") => "Делать миниатюру",
            (Self::En, "make_thumbnail") => "Create thumbnail",
            (Self::Ru, "thumbnail_size") => "Большая сторона миниатюры, px",
            (Self::En, "thumbnail_size") => "Thumbnail longest side, px",
            (Self::Ru, "example_150") => "Например 150",
            (Self::En, "example_150") => "For example 150",
            (Self::Ru, "thumbnail_text") => "Текст миниатюры",
            (Self::En, "thumbnail_text") => "Thumbnail text",
            (Self::Ru, "no_text") => "Без текста",
            (Self::En, "no_text") => "No text",
            (Self::Ru, "size_mode") => "Разрешение и размер",
            (Self::En, "size_mode") => "Resolution and size",
            (Self::Ru, "custom_text") => "Кастомный текст",
            (Self::En, "custom_text") => "Custom text",
            (Self::Ru, "filename") => "Название файла",
            (Self::En, "filename") => "File name",
            (Self::Ru, "custom_thumbnail_text") => "Кастомный текст миниатюры",
            (Self::En, "custom_thumbnail_text") => "Custom thumbnail text",
            (Self::Ru, "delete_after") => "Удалить через, минут",
            (Self::En, "delete_after") => "Delete after, minutes",
            (Self::Ru, "no_delete") => "По умолчанию не удалять",
            (Self::En, "no_delete") => "Do not delete by default",
            (Self::Ru, "files") => "Файлы",
            (Self::En, "files") => "Files",
            (Self::Ru, "drop_files") => {
                "Перетащите сюда файлы или папки либо нажмите «Добавить изображения»."
            }
            (Self::En, "drop_files") => "Drop files or folders here, or click “Add images”.",
            (Self::Ru, "results") => "Результаты",
            (Self::En, "results") => "Results",
            (Self::Ru, "results_empty") => "После загрузки здесь появятся ссылки FastPic.",
            (Self::En, "results_empty") => "FastPic links will appear here after uploading.",
            (Self::Ru, "open_page") => "Открыть страницу",
            (Self::En, "open_page") => "Open page",
            (Self::Ru, "copy") => "Копировать",
            (Self::En, "copy") => "Copy",
            (Self::Ru, "copy_all") => "Копировать все BBCode",
            (Self::En, "copy_all") => "Copy all BBCode",
            (Self::Ru, "copy_video") => "Копировать BBCode видео",
            (Self::En, "copy_video") => "Copy video BBCode",
            (Self::Ru, "expand_all") => "Развернуть все подразделы",
            (Self::En, "expand_all") => "Expand all subsections",
            (Self::Ru, "collapse_all") => "Свернуть все подразделы",
            (Self::En, "collapse_all") => "Collapse all subsections",
            (Self::Ru, "screenlist") => "скринлист",
            (Self::En, "screenlist") => "screenlist",
            (Self::Ru, "screenshot") => "скриншот",
            (Self::En, "screenshot") => "screenshot",
            (Self::Ru, "image") => "изображение",
            (Self::En, "image") => "image",
            (Self::Ru, "russian") => "Русский",
            (Self::En, "russian") => "Russian",
            (Self::Ru, "english") => "Английский",
            (Self::En, "english") => "English",
            (Self::Ru, "copy_full_bb_template") => "📋 Скопировать готовый BB-код (Шаблон релиза)",
            (Self::En, "copy_full_bb_template") => "📋 Copy Ready BBCode (Release Template)",
            (Self::Ru, "preview_full_bb_template") => "Показать готовый BB-код шаблона",
            (Self::En, "preview_full_bb_template") => "Show generated BBCode template",
            _ => "",
        }
    }
}

#[derive(Clone)]
struct CapturedFrame {
    timestamp: f64,
    rgb: Arc<RgbImage>,
}

#[derive(Clone, PartialEq)]
enum VideoStatus {
    Ready,
    Processing,
    Done(Duration),
    Error(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PreviewTab {
    Screenlist,
    Screenshots,
}

struct VideoItem {
    path: PathBuf,
    canonical_path: Option<PathBuf>,
    content_hash: u64,
    file_name: String,
    file_size: u64,
    duration: f64,
    width: u32,
    height: u32,
    video_info_line: String,
    audio_info_line: String,
    video_info_line_en: String,
    audio_info_line_en: String,
    status: VideoStatus,
    frames: Vec<CapturedFrame>,
    screenshots: Vec<CapturedFrame>,
    active_screenshot_idx: usize,
    screenshot_texture: Option<(usize, egui::TextureHandle)>,
    sheet_image: Option<Arc<RgbImage>>,
    sheet_texture: Option<egui::TextureHandle>,
    last_cols: usize,
    last_thumb_w: u32,
    last_thumb_h: u32,
}

impl VideoItem {
    fn video_info(&self, lang: Language) -> &str {
        match lang {
            Language::Ru => &self.video_info_line,
            Language::En => &self.video_info_line_en,
        }
    }

    fn audio_info(&self, lang: Language) -> &str {
        match lang {
            Language::Ru => &self.audio_info_line,
            Language::En => &self.audio_info_line_en,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CoreMode {
    Auto,
    POnly,
    EOnly,
}

#[derive(Clone, Copy)]
struct RenderParams {
    frame_count: usize,
    screenshot_count: usize,
    columns: usize,
    grid_width: u32,
    workers: usize,
    core_mode: CoreMode,
    show_timestamps: true_or_bool::BoolType,
    turbo_keyframe: bool,
    hwaccel: bool,
    move_video_to_folder: bool,
    jpeg_quality: u8,
    lang: Language,
}

mod true_or_bool {
    pub type BoolType = bool;
}

#[derive(Clone, Copy, Default)]
struct WorkerSlot {
    affinity_mask: usize,
    is_p_core: bool,
}

#[derive(Clone)]
struct CpuTopology {
    p_core_masks: Vec<usize>,
    e_core_masks: Vec<usize>,
    logical_processors: usize,
    detected: bool,
    fallback_logical_processors: usize,
    fallback_worker_limit: usize,
}

impl CpuTopology {
    fn detect() -> Self {
        #[cfg(target_os = "windows")]
        {
            if let Some(raw_cores) = win_cpu::processor_core_masks() {
                if !raw_cores.is_empty() {
                    let max_smt = raw_cores
                        .iter()
                        .map(|(_, mask)| mask.count_ones())
                        .max()
                        .unwrap_or(1);

                    let min_smt = raw_cores
                        .iter()
                        .map(|(_, mask)| mask.count_ones())
                        .min()
                        .unwrap_or(1);

                    let max_efficiency = raw_cores
                        .iter()
                        .map(|(efficiency, _)| *efficiency)
                        .max()
                        .unwrap_or(0);

                    let min_efficiency = raw_cores
                        .iter()
                        .map(|(efficiency, _)| *efficiency)
                        .min()
                        .unwrap_or(0);

                    let mut p_core_masks = Vec::new();
                    let mut e_core_masks = Vec::new();

                    if max_smt > min_smt {
                        for (_, mask) in &raw_cores {
                            if mask.count_ones() == max_smt {
                                p_core_masks.push(*mask);
                            } else {
                                e_core_masks.push(*mask);
                            }
                        }
                    } else if max_efficiency > min_efficiency {
                        for (efficiency, mask) in &raw_cores {
                            if *efficiency == min_efficiency {
                                p_core_masks.push(*mask);
                            } else {
                                e_core_masks.push(*mask);
                            }
                        }
                    } else {
                        p_core_masks.extend(raw_cores.iter().map(|(_, mask)| *mask));
                    }

                    if p_core_masks.is_empty() && e_core_masks.is_empty() {
                        p_core_masks.extend(raw_cores.iter().map(|(_, mask)| *mask));
                    }

                    p_core_masks.sort_unstable_by_key(|mask| mask.trailing_zeros());
                    e_core_masks.sort_unstable_by_key(|mask| mask.trailing_zeros());

                    let logical_processors = raw_cores
                        .iter()
                        .map(|(_, mask)| mask.count_ones() as usize)
                        .sum::<usize>();

                    let physical_cores = p_core_masks.len() + e_core_masks.len();

                    if physical_cores > 0 {
                        return Self {
                            p_core_masks,
                            e_core_masks,
                            logical_processors,
                            detected: true,
                            fallback_logical_processors: physical_cores,
                            fallback_worker_limit: physical_cores.min(MAX_WORKERS).max(1),
                        };
                    }
                }
            }
        }

        Self::fallback()
    }

    fn fallback() -> Self {
        let logical_processors = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(8)
            .max(1);

        Self {
            p_core_masks: Vec::new(),
            e_core_masks: Vec::new(),
            logical_processors,
            detected: false,
            fallback_logical_processors: logical_processors,
            fallback_worker_limit: logical_processors.min(MAX_WORKERS).max(1),
        }
    }

    fn is_hybrid(&self) -> bool {
        self.detected && !self.p_core_masks.is_empty() && !self.e_core_masks.is_empty()
    }

    fn physical_core_count(&self) -> usize {
        if self.detected {
            self.p_core_masks.len() + self.e_core_masks.len()
        } else {
            self.fallback_logical_processors
        }
    }

    fn max_workers_for_mode(&self, mode: CoreMode) -> usize {
        if !self.detected {
            return self.fallback_worker_limit.max(1);
        }

        let count = match mode {
            CoreMode::Auto => self.p_core_masks.len() + self.e_core_masks.len(),
            CoreMode::POnly => self.p_core_masks.len(),
            CoreMode::EOnly => self.e_core_masks.len(),
        };

        count.min(MAX_WORKERS).max(1)
    }

    fn recommended_workers(&self) -> usize {
        if self.detected {
            return self.max_workers_for_mode(CoreMode::Auto);
        }

        (self.fallback_logical_processors / 2)
            .clamp(1, 12)
            .min(self.fallback_worker_limit)
            .max(1)
    }

    fn build_worker_slots(&self, requested: usize, mode: CoreMode) -> Vec<WorkerSlot> {
        if !self.detected {
            let count = requested
                .max(1)
                .min(self.fallback_worker_limit)
                .max(1);

            return vec![WorkerSlot::default(); count];
        }

        let mut available = Vec::new();

        match mode {
            CoreMode::Auto => {
                available.extend(
                    self.p_core_masks
                        .iter()
                        .copied()
                        .map(|mask| WorkerSlot {
                            affinity_mask: mask,
                            is_p_core: true,
                        }),
                );
                available.extend(
                    self.e_core_masks
                        .iter()
                        .copied()
                        .map(|mask| WorkerSlot {
                            affinity_mask: mask,
                            is_p_core: false,
                        }),
                );
            }
            CoreMode::POnly => {
                available.extend(
                    self.p_core_masks
                        .iter()
                        .copied()
                        .map(|mask| WorkerSlot {
                            affinity_mask: mask,
                            is_p_core: true,
                        }),
                );
            }
            CoreMode::EOnly => {
                available.extend(
                    self.e_core_masks
                        .iter()
                        .copied()
                        .map(|mask| WorkerSlot {
                            affinity_mask: mask,
                            is_p_core: false,
                        }),
                );
            }
        }

        if available.is_empty() {
            return vec![WorkerSlot::default()];
        }

        let count = requested.max(1).min(available.len());
        available.truncate(count);
        available
    }

    fn description(&self, lang: Language) -> String {
        if !self.detected {
            return match lang {
                Language::Ru => format!(
                    "Топология P/E не определена; доступно логических потоков: {}",
                    self.logical_processors
                ),
                Language::En => format!(
                    "P/E topology not detected; available logical threads: {}",
                    self.logical_processors
                ),
            };
        }

        if self.is_hybrid() {
            match lang {
                Language::Ru => format!(
                    "Обнаружено {} P + {} E ядер ({} логических потоков)",
                    self.p_core_masks.len(),
                    self.e_core_masks.len(),
                    self.logical_processors
                ),
                Language::En => format!(
                    "Detected {} P + {} E cores ({} logical threads)",
                    self.p_core_masks.len(),
                    self.e_core_masks.len(),
                    self.logical_processors
                ),
            }
        } else {
            match lang {
                Language::Ru => format!(
                    "Обнаружено {} физических ядер ({} логических потоков)",
                    self.physical_core_count(),
                    self.logical_processors
                ),
                Language::En => format!(
                    "Detected {} physical cores ({} logical threads)",
                    self.physical_core_count(),
                    self.logical_processors
                ),
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod win_cpu {
    use std::ffi::{c_void, OsStr};
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLogicalProcessorInformationEx(
            relationship_type: u32,
            buffer: *mut c_void,
            returned_length: *mut u32,
        ) -> i32;

        fn GetCurrentThread() -> *mut c_void;
        fn SetThreadAffinityMask(h_thread: *mut c_void, dw_thread_affinity_mask: usize) -> usize;

        fn GlobalAlloc(u_flags: u32, dw_bytes: usize) -> *mut c_void;
        fn GlobalLock(h_mem: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(h_mem: *mut c_void) -> i32;
        fn GlobalFree(h_mem: *mut c_void) -> *mut c_void;
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn OpenClipboard(h_wnd_new_owner: *mut c_void) -> i32;
        fn EmptyClipboard() -> i32;
        fn CloseClipboard() -> i32;
        fn SetClipboardData(u_format: u32, h_mem: *mut c_void) -> *mut c_void;
    }

    pub struct ThreadAffinityGuard {
        previous_mask: usize,
    }

    impl ThreadAffinityGuard {
        pub fn pin(mask: usize) -> Self {
            if mask == 0 {
                return Self { previous_mask: 0 };
            }

            let previous_mask = unsafe { SetThreadAffinityMask(GetCurrentThread(), mask) };
            Self { previous_mask }
        }
    }

    impl Drop for ThreadAffinityGuard {
        fn drop(&mut self) {
            if self.previous_mask != 0 {
                unsafe {
                    let _ = SetThreadAffinityMask(GetCurrentThread(), self.previous_mask);
                }
            }
        }
    }

    pub fn set_clipboard_text(text: &str) {
        const CF_UNICODETEXT: u32 = 13;
        const GMEM_MOVEABLE: u32 = 0x0002;

        let wide: Vec<u16> = OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let byte_len = wide.len() * size_of::<u16>();

        unsafe {
            if OpenClipboard(std::ptr::null_mut()) != 0 {
                EmptyClipboard();
                let hmem = GlobalAlloc(GMEM_MOVEABLE, byte_len);
                if !hmem.is_null() {
                    let locked = GlobalLock(hmem) as *mut u16;
                    if !locked.is_null() {
                        std::ptr::copy_nonoverlapping(wide.as_ptr(), locked, wide.len());
                        GlobalUnlock(hmem);
                        if SetClipboardData(CF_UNICODETEXT, hmem).is_null() {
                            GlobalFree(hmem);
                        }
                    } else {
                        GlobalFree(hmem);
                    }
                }
                CloseClipboard();
            }
        }
    }

    pub fn processor_core_masks() -> Option<Vec<(u8, usize)>> {
        const RELATION_PROCESSOR_CORE: u32 = 0;

        let mut required_length = 0u32;

        unsafe {
            let _ = GetLogicalProcessorInformationEx(
                RELATION_PROCESSOR_CORE,
                std::ptr::null_mut(),
                &mut required_length,
            );
        }

        if required_length == 0 {
            return None;
        }

        let mut buffer = vec![0u8; required_length as usize];
        let mut returned_length = required_length;

        let success = unsafe {
            GetLogicalProcessorInformationEx(
                RELATION_PROCESSOR_CORE,
                buffer.as_mut_ptr().cast::<c_void>(),
                &mut returned_length,
            )
        };

        if success == 0 {
            return None;
        }

        let valid_length = (returned_length as usize).min(buffer.len());
        buffer.truncate(valid_length);

        let pointer_size = size_of::<usize>();
        let group_affinity_size = pointer_size + 8;

        let mut result = Vec::new();
        let mut offset = 0usize;

        while offset.saturating_add(8) <= buffer.len() {
            let header = buffer.get(offset..offset + 8)?;
            let relationship_type = u32::from_ne_bytes(header[0..4].try_into().ok()?);
            let record_size = u32::from_ne_bytes(header[4..8].try_into().ok()?) as usize;

            if record_size < 8 {
                break;
            }

            let record_end = offset.checked_add(record_size)?;
            if record_end > buffer.len() {
                break;
            }

            if relationship_type == RELATION_PROCESSOR_CORE {
                let body_offset = offset + 8;

                if record_size >= 8 + 24 {
                    let efficiency_class = *buffer.get(body_offset + 1)?;

                    let group_count_bytes = buffer.get(body_offset + 22..body_offset + 24)?;
                    let group_count =
                        u16::from_ne_bytes(group_count_bytes.try_into().ok()?) as usize;

                    let group_mask_offset = body_offset + 24;
                    let mut core_mask = 0usize;

                    for group_index in 0..group_count {
                        let item_offset = group_mask_offset
                            .checked_add(group_index.checked_mul(group_affinity_size)?)?;
                        let item_end = item_offset.checked_add(group_affinity_size)?;

                        if item_end > record_end {
                            break;
                        }

                        let mask_bytes = buffer.get(item_offset..item_offset + pointer_size)?;
                        let mask = usize::from_ne_bytes(mask_bytes.try_into().ok()?);

                        let group_offset = item_offset + pointer_size;
                        let group_bytes = buffer.get(group_offset..group_offset + 2)?;
                        let group = u16::from_ne_bytes(group_bytes.try_into().ok()?);

                        if group == 0 {
                            core_mask |= mask;
                        }
                    }

                    if core_mask != 0 {
                        result.push((efficiency_class, core_mask));
                    }
                }
            }

            offset = record_end;
        }

        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }
}

#[cfg(target_os = "windows")]
fn pin_current_thread(mask: usize) -> win_cpu::ThreadAffinityGuard {
    win_cpu::ThreadAffinityGuard::pin(mask)
}

#[cfg(not(target_os = "windows"))]
struct DummyAffinityGuard;

#[cfg(not(target_os = "windows"))]
fn pin_current_thread(_mask: usize) -> DummyAffinityGuard {
    DummyAffinityGuard
}

fn copy_text_to_clipboard(ctx: &egui::Context, text: &str) {
    ctx.copy_text(text.to_owned());
    #[cfg(target_os = "windows")]
    win_cpu::set_clipboard_text(text);
}

// ============================================================================
// ОБЁРТКА НАД НАТИВНЫМ C-МОСТОМ LIBAV (libav_bridge.c)
// ============================================================================

mod libav {
    use image::RgbImage;
    use std::ffi::{c_char, c_int, c_void, CStr, CString};
    use std::path::Path;
    use std::sync::Once;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct SlProbeInfo {
        pub duration: f64,
        pub width: u32,
        pub height: u32,
        pub fps: f64,
        pub video_bitrate_bps: i64,
        pub container_bitrate_bps: i64,
        pub video_codec: [c_char; 64],
        pub video_profile: [c_char; 128],

        pub has_audio: i32,
        pub audio_sample_rate: i32,
        pub audio_channels: i32,
        pub audio_channel_mask: u64,
        pub audio_bitrate_bps: i64,
        pub audio_codec: [c_char; 64],
    }

    impl Default for SlProbeInfo {
        fn default() -> Self {
            unsafe { std::mem::zeroed() }
        }
    }

    #[derive(Clone, Debug)]
    pub struct ProbedMedia {
        pub duration: f64,
        pub width: u32,
        pub height: u32,
        pub video_info_line: String,
        pub audio_info_line: String,
        pub video_info_line_en: String,
        pub audio_info_line_en: String,
    }

    unsafe extern "C" {
        fn sl_libav_init();
        fn sl_probe_video(
            path: *const c_char,
            out_info: *mut SlProbeInfo,
            err_buf: *mut c_char,
            err_cap: usize,
        ) -> c_int;

        fn sl_decoder_open(
            path: *const c_char,
            use_hwaccel: c_int,
            err_buf: *mut c_char,
            err_cap: usize,
        ) -> *mut c_void;

        fn sl_decoder_decode_rgb(
            dec: *mut c_void,
            timestamp_sec: f64,
            target_w: u32,
            target_h: u32,
            turbo_keyframe: c_int,
            out_rgb: *mut u8,
            err_buf: *mut c_char,
            err_cap: usize,
        ) -> c_int;

        fn sl_decoder_close(dec: *mut c_void);
    }

    static INIT: Once = Once::new();

    pub fn init() {
        INIT.call_once(|| unsafe {
            sl_libav_init();
        });
    }

    fn path_to_cstring(path: &Path) -> Result<CString, String> {
        let path_str = path.to_string_lossy();
        CString::new(path_str.as_bytes())
            .map_err(|_| "File path contains an invalid null byte.".to_owned())
    }

    fn read_err_buf(buf: &[c_char]) -> String {
        unsafe { CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned() }
    }

    fn read_c_char_buf(buf: &[c_char]) -> String {
        unsafe {
            CStr::from_ptr(buf.as_ptr())
                .to_string_lossy()
                .trim()
                .to_owned()
        }
    }

    fn gcd(mut a: u32, mut b: u32) -> u32 {
        while b != 0 {
            let t = b;
            b = a % b;
            a = t;
        }
        a.max(1)
    }

    fn format_aspect_ratio(width: u32, height: u32) -> String {
        if width == 0 || height == 0 {
            return "N/A".to_owned();
        }
        let g = gcd(width, height);
        let rw = width / g;
        let rh = height / g;
        if rw <= 50 && rh <= 50 {
            return format!("{}:{}", rw, rh);
        }
        let ratio = width as f64 / height as f64;
        let common = [
            (16, 9),
            (4, 3),
            (3, 2),
            (21, 9),
            (1, 1),
            (9, 16),
            (3, 4),
            (2, 3),
            (16, 10),
            (5, 4),
        ];
        for (cw, ch) in common {
            let cr = cw as f64 / ch as f64;
            if (ratio - cr).abs() / cr < 0.018 {
                return format!("{}:{}", cw, ch);
            }
        }
        format!("{}:{}", rw, rh)
    }

    fn format_video_codec(raw_codec: &str, raw_profile: &str) -> String {
        let base = match raw_codec.to_ascii_lowercase().as_str() {
            "h264" | "avc" | "avc1" => "AVC".to_owned(),
            "hevc" | "h265" => "HEVC".to_owned(),
            "av1" => "AV1".to_owned(),
            "vp9" => "VP9".to_owned(),
            "vp8" => "VP8".to_owned(),
            "mpeg4" => "MPEG-4".to_owned(),
            "mpeg2video" => "MPEG-2".to_owned(),
            "mpeg1video" => "MPEG-1".to_owned(),
            "wmv3" => "WMV3".to_owned(),
            "vc1" => "VC-1".to_owned(),
            "mjpeg" => "MJPEG".to_owned(),
            "prores" => "ProRes".to_owned(),
            "theora" => "Theora".to_owned(),
            "" => "Unknown".to_owned(),
            other => other.to_uppercase(),
        };

        let prof = raw_profile.trim();
        if !prof.is_empty() && !prof.eq_ignore_ascii_case("unknown") {
            format!("{} ({})", base, prof)
        } else {
            base
        }
    }

    fn format_audio_codec(raw_codec: &str) -> String {
        let lower = raw_codec.to_ascii_lowercase();
        match lower.as_str() {
            "aac" => "AAC".to_owned(),
            "mp3" | "mp3float" => "MP3".to_owned(),
            "ac3" => "AC-3".to_owned(),
            "eac3" => "E-AC-3".to_owned(),
            "opus" => "Opus".to_owned(),
            "vorbis" => "Vorbis".to_owned(),
            "flac" => "FLAC".to_owned(),
            "dts" => "DTS".to_owned(),
            "alac" => "ALAC".to_owned(),
            "wmav1" | "wmav2" | "wmapro" | "wmalossless" => "WMA".to_owned(),
            "" => "Unknown".to_owned(),
            other if other.starts_with("pcm_") => "PCM".to_owned(),
            other => other.to_uppercase(),
        }
    }

    fn format_audio_channels(channels: i32, mask: u64) -> String {
        match channels {
            0 => "N/A".to_owned(),
            1 => "C".to_owned(),
            2 => "L R".to_owned(),
            3 => {
                if (mask & 0x8) != 0 {
                    "L R LFE".to_owned()
                } else {
                    "L R C".to_owned()
                }
            }
            4 => "L R Ls Rs".to_owned(),
            5 => "L R C Ls Rs".to_owned(),
            6 => "L R C LFE Ls Rs".to_owned(),
            7 => "L R C LFE Cs Ls Rs".to_owned(),
            8 => "L R C LFE Ls Rs Lrs Rrs".to_owned(),
            n => format!("{}", n),
        }
    }

    pub fn probe_video(path: &Path, file_size: u64) -> Result<ProbedMedia, String> {
        init();
        let c_path = path_to_cstring(path)?;
        let mut info = SlProbeInfo::default();
        let mut err_buf = [0 as c_char; 512];

        let ret = unsafe {
            sl_probe_video(
                c_path.as_ptr(),
                &mut info,
                err_buf.as_mut_ptr(),
                err_buf.len(),
            )
        };

        if ret != 0 {
            return Err(read_err_buf(&err_buf));
        }

        let mut total_bps = info.container_bitrate_bps;
        if total_bps <= 0 && file_size > 0 && info.duration > 0.0 {
            total_bps = ((file_size as f64 * 8.0) / info.duration) as i64;
        }

        let mut v_bps = info.video_bitrate_bps;
        let mut a_bps = info.audio_bitrate_bps;

        if v_bps <= 0 && total_bps > 0 {
            if a_bps > 0 && total_bps > a_bps {
                v_bps = total_bps - a_bps;
            } else {
                v_bps = total_bps;
            }
        }
        if info.has_audio != 0 && a_bps <= 0 && total_bps > 0 && v_bps > 0 && total_bps > v_bps {
            a_bps = total_bps - v_bps;
        }

        let v_codec_raw = read_c_char_buf(&info.video_codec);
        let v_prof_raw = read_c_char_buf(&info.video_profile);
        let v_codec_str = format_video_codec(&v_codec_raw, &v_prof_raw);
        let aspect_str = format_aspect_ratio(info.width, info.height);
        let fps_str = if info.fps > 0.0 {
            format!("{:.3} fps", info.fps)
        } else {
            "N/A".to_owned()
        };
        let v_bitrate_str = if v_bps > 0 {
            format!("{} kbps", (v_bps + 500) / 1000)
        } else {
            "N/A".to_owned()
        };

        let video_info_line = format!(
            "Видео: Кодек: {}, Разрешение: {}×{} ({}), Частота кадров: {}, Битрейт: {}",
            v_codec_str, info.width, info.height, aspect_str, fps_str, v_bitrate_str
        );

        let video_info_line_en = format!(
            "Video: Codec: {}, Resolution: {}×{} ({}), Frame rate: {}, Bitrate: {}",
            v_codec_str, info.width, info.height, aspect_str, fps_str, v_bitrate_str
        );

        let (audio_info_line, audio_info_line_en) = if info.has_audio != 0 {
            let a_codec_raw = read_c_char_buf(&info.audio_codec);
            let a_codec_str = format_audio_codec(&a_codec_raw);
            let sample_rate_str = if info.audio_sample_rate > 0 {
                format!("{} kHz", info.audio_sample_rate / 1000)
            } else {
                "N/A".to_owned()
            };
            let channels_str = format_audio_channels(info.audio_channels, info.audio_channel_mask);
            let a_bitrate_str = if a_bps > 0 {
                format!("{} kbps", (a_bps + 500) / 1000)
            } else {
                "N/A".to_owned()
            };

            let ru = format!(
                "Аудио: Кодек: {}, Частота: {}, Каналы: {}, Битрейт: {}",
                a_codec_str, sample_rate_str, channels_str, a_bitrate_str
            );
            let en = format!(
                "Audio: Codec: {}, Sample rate: {}, Channels: {}, Bitrate: {}",
                a_codec_str, sample_rate_str, channels_str, a_bitrate_str
            );
            (ru, en)
        } else {
            ("Аудио: Отсутствует".to_owned(), "Audio: None".to_owned())
        };

        Ok(ProbedMedia {
            duration: info.duration,
            width: info.width,
            height: info.height,
            video_info_line,
            audio_info_line,
            video_info_line_en,
            audio_info_line_en,
        })
    }

    pub struct LibavDecoder {
        raw: *mut c_void,
    }

    impl LibavDecoder {
        pub fn open(path: &Path, use_hwaccel: bool) -> Result<Self, String> {
            init();
            let c_path = path_to_cstring(path)?;
            let mut err_buf = [0 as c_char; 512];

            let raw = unsafe {
                sl_decoder_open(
                    c_path.as_ptr(),
                    c_int::from(use_hwaccel),
                    err_buf.as_mut_ptr(),
                    err_buf.len(),
                )
            };

            if raw.is_null() {
                return Err(read_err_buf(&err_buf));
            }

            Ok(Self { raw })
        }

        pub fn decode_frame_rgb(
            &mut self,
            timestamp_sec: f64,
            target_w: u32,
            target_h: u32,
            turbo_keyframe: bool,
        ) -> Result<RgbImage, String> {
            let frame_bytes = (target_w as usize)
                .checked_mul(target_h as usize)
                .and_then(|px| px.checked_mul(3))
                .ok_or_else(|| "Frame buffer overflow.".to_owned())?;

            let mut rgb_buf = vec![0u8; frame_bytes + 64];
            let mut err_buf = [0 as c_char; 512];

            let ret = unsafe {
                sl_decoder_decode_rgb(
                    self.raw,
                    timestamp_sec,
                    target_w,
                    target_h,
                    c_int::from(turbo_keyframe),
                    rgb_buf.as_mut_ptr(),
                    err_buf.as_mut_ptr(),
                    err_buf.len(),
                )
            };

            if ret != 0 {
                return Err(read_err_buf(&err_buf));
            }

            rgb_buf.truncate(frame_bytes);
            RgbImage::from_raw(target_w, target_h, rgb_buf)
                .ok_or_else(|| "Failed to create RgbImage.".to_owned())
        }
    }

    impl Drop for LibavDecoder {
        fn drop(&mut self) {
            if !self.raw.is_null() {
                unsafe {
                    sl_decoder_close(self.raw);
                }
            }
        }
    }
}

// ============================================================================
// ПОДПРОГРАММА FASTPIC UPLOADER
// ============================================================================

#[derive(Debug, Clone, Deserialize, Serialize)]
struct UploadResponse {
    big_link: Option<String>,
    thumb_link: Option<String>,
    view_link: Option<String>,
    short_link: Option<String>,
    delete_link: Option<String>,
    width: Option<u64>,
    height: Option<u64>,
    size: Option<u64>,
    hotlinked: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct AnonymousUploadResponse {
    status: Option<String>,
    error: Option<String>,
    imagepath: Option<String>,
    viewurl: Option<String>,
    viewfullurl: Option<String>,
    thumbpath: Option<String>,
}

impl UploadResponse {
    fn bbcode(&self) -> String {
        match (&self.view_link, &self.thumb_link) {
            (Some(view), Some(thumb)) => format!("[URL={view}][IMG]{thumb}[/IMG][/URL]"),
            _ => String::new(),
        }
    }
}

#[derive(Debug)]
struct UploadResult {
    path: PathBuf,
    response: AnyResult<UploadResponse>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageKind {
    Screenlist,
    Screenshot,
    Other,
}

impl ImageKind {
    fn label(self, language: Language) -> &'static str {
        match self {
            Self::Screenlist => language.text("screenlist"),
            Self::Screenshot => language.text("screenshot"),
            Self::Other => language.text("image"),
        }
    }
}

#[derive(Debug)]
struct AddProgress {
    path: PathBuf,
    processed: usize,
    is_image: bool,
}

#[derive(Debug, Clone)]
struct UploadOptions {
    key: Option<String>,
    language: Language,
    thumb_size: Option<String>,
    check_thumb: Option<String>,
    thumb_text: Option<String>,
    delete_after: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthMode {
    Anonymous,
    Own,
}

struct FastPicSubApp {
    language: Language,
    auth_mode: AuthMode,
    own_key: String,
    key_visible: bool,
    make_thumbnail: bool,
    thumb_size: String,
    check_thumb: usize,
    thumb_text: String,
    delete_after: String,
    files: Vec<PathBuf>,
    results: Vec<UploadResult>,
    sections_expanded: bool,
    expanded_sections: BTreeMap<String, bool>,
    status: String,
    uploading: bool,
    total_to_upload: usize,
    receiver: Option<mpsc::Receiver<UploadResult>>,
    adding: bool,
    add_processed: usize,
    add_total: usize,
    add_added: usize,
    add_rejected: usize,
    add_receiver: Option<mpsc::Receiver<AddProgress>>,
}

#[derive(Clone, Debug)]
struct VideoBbMeta {
    file_name: String,
    duration: f64,
    file_size: u64,
    width: u32,
    height: u32,
    video_info: String,
    audio_info: String,
}

impl VideoBbMeta {
    fn from_video_item(item: &VideoItem, lang: Language) -> Self {
        Self {
            file_name: item.file_name.clone(),
            duration: item.duration,
            file_size: item.file_size,
            width: item.width,
            height: item.height,
            video_info: item.video_info(lang).to_string(),
            audio_info: item.audio_info(lang).to_string(),
        }
    }

    fn format_spoiler_header(&self) -> String {
        let duration_str = format_duration(self.duration);
        let size_mb = self.file_size as f64 / (1024.0 * 1024.0);
        let size_str = format!("{:.2} Mb", size_mb).replace('.', ",");
        format!(
            "{} | {} | {}x{} | {}",
            duration_str, size_str, self.width, self.height, self.file_name
        )
    }
}

fn format_colored_tech_line(line: &str) -> String {
    if let Some((prefix, rest)) = line.split_once(':') {
        format!("[b][color=#cc66ff]{}:[/color][/b]{}", prefix.trim(), rest)
    } else {
        line.to_string()
    }
}

fn extract_screenshot_number(path: &Path) -> usize {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if let Some(sep) = stem.rfind('_') {
        let suffix = stem[sep + 1..].to_ascii_lowercase();
        if let Some(num_str) = suffix.strip_prefix("screenshot") {
            if let Ok(num) = num_str.parse::<usize>() {
                return num;
            }
        }
    }
    0
}

fn try_probe_video_near_images(
    video_name: &str,
    results: &[&UploadResult],
    lang: Language,
) -> Option<VideoBbMeta> {
    let mut candidate_dirs = Vec::new();
    for res in results {
        if let Some(parent) = res.path.parent() {
            let p_buf = parent.to_path_buf();
            if !candidate_dirs.contains(&p_buf) {
                candidate_dirs.push(p_buf);
            }
            if let Some(grand_parent) = parent.parent() {
                let gp_buf = grand_parent.to_path_buf();
                if !candidate_dirs.contains(&gp_buf) {
                    candidate_dirs.push(gp_buf);
                }
            }
        }
    }

    for dir in candidate_dirs {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && is_video_file(&path) {
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                if stem == video_name || stem.eq_ignore_ascii_case(video_name) {
                    let file_size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    if let Ok(meta) = libav::probe_video(&path, file_size) {
                        let file_name = path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or(video_name)
                            .to_string();
                        let video_info = match lang {
                            Language::Ru => meta.video_info_line,
                            Language::En => meta.video_info_line_en,
                        };
                        let audio_info = match lang {
                            Language::Ru => meta.audio_info_line,
                            Language::En => meta.audio_info_line_en,
                        };
                        return Some(VideoBbMeta {
                            file_name,
                            duration: meta.duration,
                            file_size,
                            width: meta.width,
                            height: meta.height,
                            video_info,
                            audio_info,
                        });
                    }
                }
            }
        }
    }
    None
}

impl FastPicSubApp {
    fn new(language: Language) -> Self {
        Self {
            language,
            auth_mode: AuthMode::Anonymous,
            own_key: String::new(),
            key_visible: false,
            make_thumbnail: true,
            thumb_size: String::new(),
            check_thumb: 1,
            thumb_text: String::new(),
            delete_after: String::new(),
            files: Vec::new(),
            results: Vec::new(),
            sections_expanded: false,
            expanded_sections: BTreeMap::new(),
            status: language.text("drop_files").to_owned(),
            uploading: false,
            total_to_upload: 0,
            receiver: None,
            adding: false,
            add_processed: 0,
            add_total: 0,
            add_added: 0,
            add_rejected: 0,
            add_receiver: None,
        }
    }

    #[inline]
    fn is_busy(&self) -> bool {
        self.uploading || self.adding
    }

    fn sync_language(&mut self, lang: Language) {
        if self.language != lang {
            self.language = lang;
            if self.files.is_empty() && !self.uploading && !self.adding && self.results.is_empty() {
                self.status = self.language.text("drop_files").to_owned();
            }
        }
    }

    fn add_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        if self.adding || self.uploading {
            return;
        }
        let mut candidates = Vec::new();
        for path in paths {
            let collected = if path.is_dir() {
                collect_directory(&path).unwrap_or_default()
            } else if path.is_file() {
                vec![path]
            } else {
                Vec::new()
            };
            candidates.extend(collected);
        }
        candidates.sort();
        candidates.dedup();
        if candidates.is_empty() {
            self.status = if self.language == Language::Ru {
                "Изображения не найдены"
            } else {
                "No images found"
            }
                .to_owned();
            return;
        }

        let total = candidates.len();
        let (sender, receiver) = mpsc::channel();
        self.add_receiver = Some(receiver);
        self.adding = true;
        self.add_processed = 0;
        self.add_total = total;
        self.add_added = 0;
        self.add_rejected = 0;
        self.status = if self.language == Language::Ru {
            format!("Проверка изображений: 0/{total}")
        } else {
            format!("Checking images: 0/{total}")
        };

        thread::spawn(move || {
            for (index, path) in candidates.into_iter().enumerate() {
                let _ = sender.send(AddProgress {
                    is_image: is_image_file(&path),
                    path,
                    processed: index + 1,
                });
            }
        });
    }

    fn poll_additions(&mut self) {
        let events = self
            .add_receiver
            .as_ref()
            .map(|receiver| receiver.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();

        for event in events {
            self.add_processed = event.processed;
            if event.is_image {
                if !self.files.contains(&event.path) {
                    self.files.push(event.path);
                    self.add_added += 1;
                }
            } else {
                self.add_rejected += 1;
            }
        }

        if self.adding && self.add_processed >= self.add_total {
            self.files.sort();
            self.adding = false;
            self.add_receiver = None;
            self.status = if self.language == Language::Ru {
                format!(
                    "Добавлено изображений: {}, отклонено не изображений: {}",
                    self.add_added, self.add_rejected
                )
            } else {
                format!(
                    "Images added: {}, non-images rejected: {}",
                    self.add_added, self.add_rejected
                )
            };
        }
    }

    fn start_upload(&mut self) {
        if self.uploading {
            return;
        }
        let key = match self.auth_mode {
            AuthMode::Anonymous => None,
            AuthMode::Own => Some(self.own_key.trim()),
        };
        if self.auth_mode == AuthMode::Own && key.map_or(true, str::is_empty) {
            self.status = if self.language == Language::Ru {
                "Укажите свой API-ключ FastPic"
            } else {
                "Enter your FastPic API key"
            }
                .to_owned();
            return;
        }
        if self.files.is_empty() {
            self.status = if self.language == Language::Ru {
                "Добавьте хотя бы один файл"
            } else {
                "Add at least one file"
            }
                .to_owned();
            return;
        }

        let delete_after = match parse_optional_u64(&self.delete_after, self.language) {
            Ok(value) => value,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        let thumb_size = self
            .make_thumbnail
            .then(|| optional_string(&self.thumb_size))
            .flatten();
        let check_thumb = if !self.make_thumbnail {
            Some("no".to_owned())
        } else {
            match self.check_thumb {
                1 => Some("size".to_owned()),
                2 => Some("text".to_owned()),
                3 => Some("filename".to_owned()),
                _ => None,
            }
        };
        let options = UploadOptions {
            key: key.map(str::to_owned),
            language: self.language,
            thumb_size,
            check_thumb,
            thumb_text: self
                .make_thumbnail
                .then(|| optional_string(&self.thumb_text))
                .flatten(),
            delete_after,
        };
        let files = std::mem::take(&mut self.files);
        let total = files.len();
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        self.results.clear();
        self.total_to_upload = total;
        self.uploading = true;
        self.status = if self.language == Language::Ru {
            format!("Загрузка началась: 0/{total}")
        } else {
            format!("Upload started: 0/{total}")
        };

        let language = self.language;
        thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    for path in files {
                        let _ = sender.send(UploadResult {
                            path,
                            response: Err(anyhow::anyhow!(
                                "{}",
                                localized_error(
                                    language,
                                    "Не удалось запустить загрузчик",
                                    "Failed to start uploader",
                                    &error
                                )
                            )),
                        });
                    }
                    return;
                }
            };
            runtime.block_on(upload_all(files, options, sender));
        });
    }

    fn poll_uploads(&mut self) {
        let Some(receiver) = &self.receiver else {
            return;
        };
        let mut received = 0;
        while let Ok(result) = receiver.try_recv() {
            self.results.push(result);
            received += 1;
        }
        if received > 0 {
            let completed = self.results.len();
            self.status = if self.language == Language::Ru {
                format!("Загружено: {completed}/{}", self.total_to_upload)
            } else {
                format!("Uploaded: {completed}/{}", self.total_to_upload)
            };
            if completed == self.total_to_upload {
                self.uploading = false;
                self.receiver = None;
                self.status = if self.language == Language::Ru {
                    "Загрузка завершена"
                } else {
                    "Upload completed"
                }
                    .to_owned();
            }
        }
    }

    fn draw_top_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("FastPic Uploader");
            ui.separator();
            ui.label(self.language.text("subtitle"));
            ui.separator();
            egui::ComboBox::from_id_source("fastpic_language")
                .selected_text(if self.language == Language::Ru {
                    self.language.text("russian")
                } else {
                    self.language.text("english")
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.language,
                        Language::Ru,
                        Language::Ru.text("russian"),
                    );
                    ui.selectable_value(
                        &mut self.language,
                        Language::En,
                        Language::En.text("english"),
                    );
                });
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !self.uploading && !self.adding,
                    egui::Button::new(self.language.text("add_images")),
                )
                .clicked()
            {
                if let Some(paths) = rfd::FileDialog::new().pick_files() {
                    self.add_paths(paths);
                }
            }
            if ui
                .add_enabled(
                    !self.uploading && !self.adding,
                    egui::Button::new(self.language.text("add_folder")),
                )
                .clicked()
            {
                if let Some(path) = rfd::FileDialog::new().pick_folder() {
                    self.add_paths([path]);
                }
            }
            if ui.button(self.language.text("clear")).clicked() && !self.uploading && !self.adding {
                self.files.clear();
                self.results.clear();
                self.status = if self.language == Language::Ru {
                    "Список очищен"
                } else {
                    "List cleared"
                }
                    .to_owned();
            }
            ui.add_space(8.0);
            if ui
                .add_enabled(
                    !self.uploading,
                    egui::Button::new(
                        egui::RichText::new(self.language.text("upload"))
                            .strong()
                            .color(egui::Color32::WHITE),
                    )
                        .fill(egui::Color32::from_rgb(75, 95, 210)),
                )
                .clicked()
            {
                self.start_upload();
            }
        });
        if self.adding {
            let progress = if self.add_total == 0 {
                0.0
            } else {
                self.add_processed as f32 / self.add_total as f32
            };
            ui.add(egui::ProgressBar::new(progress).text(format!(
                "{}: {}/{}",
                if self.language == Language::Ru {
                    "Проверка изображений"
                } else {
                    "Checking images"
                },
                self.add_processed,
                self.add_total
            )));
        } else if self.uploading {
            let progress = if self.total_to_upload == 0 {
                0.0
            } else {
                self.results.len() as f32 / self.total_to_upload as f32
            };
            ui.add(egui::ProgressBar::new(progress).text(format!(
                "{}: {}/{}",
                if self.language == Language::Ru {
                    "Загрузка на FastPic"
                } else {
                    "Uploading to FastPic"
                },
                self.results.len(),
                self.total_to_upload
            )));
        }
        if self.files.is_empty() && !self.uploading && !self.adding && self.results.is_empty() {
            self.status = self.language.text("drop_files").to_owned();
        }
        ui.label(&self.status);
    }

    fn draw_settings(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(self.language.text("settings"))
            .default_open(true)
            .show(ui, |ui| {
                egui::Grid::new("fastpic_settings_grid")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label(self.language.text("api_key"));
                        ui.horizontal(|ui| {
                            ui.selectable_value(
                                &mut self.auth_mode,
                                AuthMode::Anonymous,
                                self.language.text("anonymous"),
                            );
                            ui.selectable_value(
                                &mut self.auth_mode,
                                AuthMode::Own,
                                self.language.text("own_key"),
                            );
                        });
                        ui.end_row();

                        if self.auth_mode == AuthMode::Own {
                            ui.label(self.language.text("own_key"));
                            ui.horizontal(|ui| {
                                let mut edit = egui::TextEdit::singleline(&mut self.own_key)
                                    .desired_width(360.0);
                                if !self.key_visible {
                                    edit = edit.password(true);
                                }
                                ui.add(edit);
                                if ui
                                    .button(if self.key_visible {
                                        self.language.text("hide")
                                    } else {
                                        self.language.text("show")
                                    })
                                    .clicked()
                                {
                                    self.key_visible = !self.key_visible;
                                }
                            });
                            ui.end_row();
                        }
                        ui.label(self.language.text("thumbnail"));
                        ui.checkbox(
                            &mut self.make_thumbnail,
                            self.language.text("make_thumbnail"),
                        );
                        ui.end_row();
                        ui.label(self.language.text("thumbnail_size"));
                        ui.add_enabled(
                            self.make_thumbnail,
                            egui::TextEdit::singleline(&mut self.thumb_size)
                                .hint_text(self.language.text("example_150")),
                        );
                        ui.end_row();
                        ui.label(self.language.text("thumbnail_text"));
                        ui.add_enabled_ui(self.make_thumbnail, |ui| {
                            egui::ComboBox::from_id_source("fastpic_check_thumb")
                                .selected_text(check_thumb_name(self.language, self.check_thumb))
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut self.check_thumb,
                                        0,
                                        self.language.text("no_text"),
                                    );
                                    ui.selectable_value(
                                        &mut self.check_thumb,
                                        1,
                                        self.language.text("size_mode"),
                                    );
                                    ui.selectable_value(
                                        &mut self.check_thumb,
                                        2,
                                        self.language.text("custom_text"),
                                    );
                                    ui.selectable_value(
                                        &mut self.check_thumb,
                                        3,
                                        self.language.text("filename"),
                                    );
                                });
                        });
                        ui.end_row();
                        ui.label(self.language.text("custom_thumbnail_text"));
                        ui.add_enabled(
                            self.make_thumbnail && self.check_thumb == 2,
                            egui::TextEdit::singleline(&mut self.thumb_text),
                        );
                        ui.end_row();
                        ui.label(self.language.text("delete_after"));
                        ui.add(
                            egui::TextEdit::singleline(&mut self.delete_after)
                                .hint_text(self.language.text("no_delete")),
                        );
                        ui.end_row();
                    });
            });
    }

    fn draw_files(&mut self, ui: &mut egui::Ui) {
        ui.heading(format!(
            "{} ({})",
            self.language.text("files"),
            self.files.len()
        ));
        if self.files.is_empty() {
            ui.add_space(8.0);
            ui.label(self.language.text("drop_files"));
        } else {
            egui::ScrollArea::vertical()
                .id_source("fastpic_files_scroll")
                .max_height(190.0)
                .show(ui, |ui| {
                    let mut remove = None;
                    for (index, path) in self.files.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label(path.display().to_string());
                            if !self.uploading && ui.small_button("×").clicked() {
                                remove = Some(index);
                            }
                        });
                    }
                    if let Some(index) = remove {
                        self.files.remove(index);
                    }
                });
        }
    }

    fn draw_results(&mut self, ui: &mut egui::Ui, queue: &[VideoItem]) {
        ui.separator();
        ui.heading(self.language.text("results"));
        if self.results.is_empty() {
            ui.label(self.language.text("results_empty"));
            return;
        }
        let all_bbcode = self
            .results
            .iter()
            .filter_map(|result| result.response.as_ref().ok())
            .map(UploadResponse::bbcode)
            .filter(|bbcode| !bbcode.is_empty())
            .collect::<Vec<_>>()
            .join("\n");

        let mut groups: BTreeMap<String, Vec<&UploadResult>> = BTreeMap::new();
        for result in &self.results {
            let (video_name, _) = classify_image_path(&result.path);
            groups.entry(video_name).or_default().push(result);
        }
        // Сортируем файлы внутри каждой группы: сначала Скринлист, затем Скриншоты 1..N
        for group_files in groups.values_mut() {
            group_files.sort_by_key(|res| match classify_image_path(&res.path).1 {
                ImageKind::Screenlist => (0usize, 0usize),
                ImageKind::Screenshot => (1usize, extract_screenshot_number(&res.path)),
                ImageKind::Other => (2usize, 0usize),
            });
        }

        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(!all_bbcode.is_empty(), |ui| {
                if ui.button(self.language.text("copy_all")).clicked() {
                    copy_text_to_clipboard(ui.ctx(), &all_bbcode);
                }
                if ui
                    .button(
                        egui::RichText::new(self.language.text("copy_full_bb_template"))
                            .strong()
                            .color(egui::Color32::from_rgb(120, 230, 150)),
                    )
                    .clicked()
                {
                    let release_bbcode = self.generate_release_bbcode(&groups, queue);
                    if !release_bbcode.is_empty() {
                        copy_text_to_clipboard(ui.ctx(), &release_bbcode);
                    }
                }
            });
        });
        
        let all_expanded = !groups.is_empty()
            && groups
            .keys()
            .all(|name| self.expanded_sections.get(name).copied().unwrap_or(false));
        if ui
            .button(if all_expanded {
                self.language.text("collapse_all")
            } else {
                self.language.text("expand_all")
            })
            .clicked()
        {
            self.sections_expanded = !all_expanded;
            for name in groups.keys() {
                self.expanded_sections
                    .insert(name.clone(), self.sections_expanded);
            }
        }
        egui::ScrollArea::vertical()
            .id_source("fastpic_results_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (video_name, results) in groups {
                    let expanded = self
                        .expanded_sections
                        .get(&video_name)
                        .copied()
                        .unwrap_or(false);
                    let response = egui::CollapsingHeader::new(video_name.clone())
                        .open(Some(expanded))
                        .show(ui, |ui| {
                            let video_bbcode = results
                                .iter()
                                .filter_map(|result| result.response.as_ref().ok())
                                .map(UploadResponse::bbcode)
                                .filter(|bbcode| !bbcode.is_empty())
                                .collect::<Vec<_>>()
                                .join("\n");
                            ui.add_enabled_ui(!video_bbcode.is_empty(), |ui| {
                                if ui.button(self.language.text("copy_video")).clicked() {
                                    copy_text_to_clipboard(ui.ctx(), &video_bbcode);
                                }
                            });
                            for result in results {
                                let (_, image_kind) = classify_image_path(&result.path);
                                ui.group(|ui| match &result.response {
                                    Ok(response) => {
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "{} - {}",
                                                result.path.display(),
                                                image_kind.label(self.language)
                                            ))
                                                .strong(),
                                        );
                                        ui.horizontal(|ui| {
                                            ui.label("BBCode:");
                                            let mut bbcode = response.bbcode();
                                            ui.add(
                                                egui::TextEdit::singleline(&mut bbcode)
                                                    .desired_width(ui.available_width()),
                                            );
                                        });
                                        ui.horizontal(|ui| {
                                            if let Some(view_link) = &response.view_link {
                                                if ui
                                                    .button(self.language.text("open_page"))
                                                    .clicked()
                                                {
                                                    ui.ctx()
                                                        .open_url(egui::OpenUrl::new_tab(view_link));
                                                }
                                            }
                                            if ui.button(self.language.text("copy")).clicked() {
                                                copy_text_to_clipboard(
                                                    ui.ctx(),
                                                    &response.bbcode(),
                                                );
                                            }
                                        });
                                    }
                                    Err(error) => {
                                        ui.colored_label(
                                            egui::Color32::from_rgb(240, 100, 100),
                                            format!(
                                                "{} - {}: {error:#}",
                                                result.path.display(),
                                                image_kind.label(self.language)
                                            ),
                                        );
                                    }
                                });
                            }
                        });
                    if response.header_response.clicked() {
                        self.expanded_sections.insert(video_name, !expanded);
                    }
                }
            });
    }
    fn generate_release_bbcode(
        &self,
        groups: &BTreeMap<String, Vec<&UploadResult>>,
        queue: &[VideoItem],
    ) -> String {
        let lang = self.language;
        let mut full_screenshots_blocks: Vec<String> = Vec::new();
        let mut screenlists_blocks: Vec<String> = Vec::new();

        for (video_name, results) in groups {
            let mut screenlist_bb: Option<String> = None;
            let mut numbered_screenshots: Vec<(usize, String)> = Vec::new();
            let mut other_screenshots: Vec<String> = Vec::new();

            for result in results {
                let Ok(response) = &result.response else {
                    continue;
                };
                let bb = response.bbcode();
                if bb.is_empty() {
                    continue;
                }
                let (_, kind) = classify_image_path(&result.path);
                match kind {
                    ImageKind::Screenlist => {
                        if screenlist_bb.is_none() {
                            screenlist_bb = Some(bb);
                        }
                    }
                    ImageKind::Screenshot => {
                        let num = extract_screenshot_number(&result.path);
                        numbered_screenshots.push((num, bb));
                    }
                    ImageKind::Other => {
                        other_screenshots.push(bb);
                    }
                }
            }

            // Сортируем скриншоты по номеру (ScreenShot1, ScreenShot2, ...)
            numbered_screenshots.sort_by_key(|(num, _)| *num);
            let mut screenshot_bbs: Vec<String> = numbered_screenshots
                .into_iter()
                .map(|(_, bb)| bb)
                .collect();
            screenshot_bbs.extend(other_screenshots);

            // 1. Полноразмерные скриншоты — горизонтально по 4 штуки в строке для каждого видео
            if !screenshot_bbs.is_empty() {
                let rows: Vec<String> = screenshot_bbs
                    .chunks(4)
                    .map(|chunk| chunk.join(" "))
                    .collect();
                full_screenshots_blocks.push(rows.join("\n"));
            }

            // 2. Скринлисты и тех. данные
            if let Some(sl_code) = screenlist_bb {
                let meta = queue
                    .iter()
                    .find(|item| {
                        let item_stem = stem_name(&item.file_name);
                        item_stem == *video_name || item_stem.eq_ignore_ascii_case(video_name)
                    })
                    .map(|item| VideoBbMeta::from_video_item(item, lang))
                    .or_else(|| try_probe_video_near_images(video_name, results, lang));

                let spoiler_title = meta
                    .as_ref()
                    .map(|m| m.format_spoiler_header())
                    .unwrap_or_else(|| video_name.clone());

                let mut inner_lines: Vec<String> = Vec::new();
                if let Some(ref m) = meta {
                    if !m.video_info.is_empty() {
                        inner_lines.push(format_colored_tech_line(&m.video_info));
                    }
                    if !m.audio_info.is_empty() {
                        inner_lines.push(format_colored_tech_line(&m.audio_info));
                    }
                }
                inner_lines.push(sl_code);

                let video_spoiler = format!(
                    "[spoiler=\"{}\"]\n[align=center]{}[/align]\n[/spoiler]",
                    spoiler_title,
                    inner_lines.join("\n")
                );
                screenlists_blocks.push(video_spoiler);
            }
        }

        let screenshots_title = lang.tr("Полноразмерные скриншоты", "Full-size screenshots");
        let screenlists_title = lang.tr("Скринлисты и тех. данные", "Screenlists and tech. data");

        let mut sections: Vec<String> = Vec::new();
        if !full_screenshots_blocks.is_empty() {
            sections.push(format!(
                "[spoiler=\"{}\"]\n{}\n[/spoiler]",
                screenshots_title,
                full_screenshots_blocks.join("\n")
            ));
        }
        if !screenlists_blocks.is_empty() {
            sections.push(format!(
                "[spoiler=\"{}\"]\n{}\n[/spoiler]",
                screenlists_title,
                screenlists_blocks.join("\n")
            ));
        }

        sections.join("\n")
    }
}

fn check_thumb_name(language: Language, value: usize) -> &'static str {
    match value {
        1 => language.text("size_mode"),
        2 => language.text("custom_text"),
        3 => language.text("filename"),
        _ => language.text("no_text"),
    }
}

fn classify_image_path(path: &Path) -> (String, ImageKind) {
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("Other");
    let Some(separator) = stem.rfind('_') else {
        return (stem.to_owned(), ImageKind::Other);
    };

    let video_name = &stem[..separator];
    let suffix = stem[separator + 1..].to_ascii_lowercase();
    if suffix == "screenlist" {
        return (video_name.to_owned(), ImageKind::Screenlist);
    }
    if suffix == "screenshot"
        || (suffix.starts_with("screenshot")
        && suffix["screenshot".len()..]
        .chars()
        .all(|character| character.is_ascii_digit()))
    {
        return (video_name.to_owned(), ImageKind::Screenshot);
    }
    (stem.to_owned(), ImageKind::Other)
}

fn optional_string(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.trim().to_owned())
}

fn parse_optional_u64(value: &str, language: Language) -> Result<Option<u64>, String> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    value.trim().parse().map(Some).map_err(|_| {
        if language == Language::Ru {
            "Удаление через должно быть целым числом минут"
        } else {
            "Delete-after value must be a whole number of minutes"
        }
            .to_owned()
    })
}

fn localized_error(
    language: Language,
    russian: &str,
    english: &str,
    error: impl std::fmt::Display,
) -> String {
    let prefix = if language == Language::Ru {
        russian
    } else {
        english
    };
    let error = error.to_string();
    if error.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}: {error}")
    }
}

fn collect_directory(path: &Path) -> AnyResult<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in WalkDir::new(path).follow_links(false) {
        let entry = entry.with_context(|| format!("Не удалось обойти {}", path.display()))?;
        if entry.file_type().is_file() {
            files.push(entry.into_path());
        }
    }
    files.sort();
    Ok(files)
}

fn is_image_file(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase())
            .as_deref(),
        Some("jpg")
            | Some("jpeg")
            | Some("png")
            | Some("gif")
            | Some("webp")
            | Some("bmp")
            | Some("tif")
            | Some("tiff")
            | Some("avif")
            | Some("ico")
    )
}

async fn upload_all(
    files: Vec<PathBuf>,
    options: UploadOptions,
    sender: mpsc::Sender<UploadResult>,
) {
    let client = match Client::builder().user_agent("fastpic-uploader/0.1").build() {
        Ok(client) => client,
        Err(error) => {
            for path in files {
                let _ = sender.send(UploadResult {
                    path,
                    response: Err(anyhow::anyhow!(localized_error(
                        options.language,
                        "Не удалось создать HTTP-клиент",
                        "Failed to create HTTP client",
                        &error
                    ))),
                });
            }
            return;
        }
    };
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_UPLOADS));
    let options = Arc::new(options);

    stream::iter(files.into_iter().map(|path| {
        let client = client.clone();
        let options = Arc::clone(&options);
        let semaphore = Arc::clone(&semaphore);
        async move {
            let response = match semaphore.acquire_owned().await {
                Ok(_permit) => upload_file(&client, &options, &path).await,
                Err(error) => Err(anyhow::anyhow!(localized_error(
                    options.language,
                    "Ошибка планировщика загрузок",
                    "Upload scheduler error",
                    &error
                ))),
            };
            (path, response)
        }
    }))
        .buffer_unordered(MAX_CONCURRENT_UPLOADS)
        .for_each(|(path, response)| {
            let sender = sender.clone();
            async move {
                let _ = sender.send(UploadResult { path, response });
            }
        })
        .await;
}

async fn upload_file(
    client: &Client,
    options: &UploadOptions,
    path: &Path,
) -> AnyResult<UploadResponse> {
    let bytes = tokio::fs::read(path).await.with_context(|| {
        localized_error(
            options.language,
            &format!("Не удалось прочитать {}", path.display()),
            &format!("Failed to read {}", path.display()),
            "",
        )
    })?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("upload");
    let part = multipart::Part::bytes(bytes).file_name(filename.to_owned());
    let mut form = multipart::Form::new().part("file1", part);

    let request = if options.key.is_none() {
        let check_thumb = options.check_thumb.as_deref().unwrap_or("no");
        let thumb_size = options.thumb_size.as_deref().unwrap_or("");
        form = form
            .text("method", "file")
            .text("check_thumb", check_thumb.to_owned())
            .text("uploading", "1")
            .text("orig_rotate", "0")
            .text("thumb_size", thumb_size.to_owned());
        client
            .post(format!("{ANONYMOUS_UPLOAD_URL}&method=file"))
            .header(reqwest::header::USER_AGENT, "Mozilla/5.0")
            .multipart(form)
    } else {
        if let Some(value) = &options.thumb_size {
            form = form.text("thumb_size", value.clone());
        }
        if let Some(value) = &options.check_thumb {
            form = form.text("check_thumb", value.clone());
        }
        if let Some(value) = &options.thumb_text {
            form = form.text("thumb_text", value.clone());
        }
        if let Some(value) = options.delete_after {
            form = form.text("delete_after", value.to_string());
        }
        client.post(UPLOAD_URL).multipart(form)
    };
    let request = if let Some(key) = &options.key {
        request.bearer_auth(key)
    } else {
        request
    };
    let response = request.send().await.with_context(|| {
        localized_error(
            options.language,
            &format!("Ошибка запроса для {}", path.display()),
            &format!("Request failed for {}", path.display()),
            "",
        )
    })?;
    let status = response.status();
    let body = response.text().await.with_context(|| {
        localized_error(
            options.language,
            &format!("Не удалось прочитать ответ для {}", path.display()),
            &format!("Failed to read response for {}", path.display()),
            "",
        )
    })?;
    if !status.is_success() {
        bail!(
            "{}",
            localized_error(
                options.language,
                &format!(
                    "FastPic вернул HTTP {} для {}: {}",
                    status,
                    path.display(),
                    body
                ),
                &format!(
                    "FastPic returned HTTP {} for {}: {}",
                    status,
                    path.display(),
                    body
                ),
                "",
            )
        );
    }
    if options.key.is_none() {
        let anonymous_response: AnonymousUploadResponse = match quick_xml::de::from_str(&body) {
            Ok(response) => response,
            Err(_error) => {
                let summary = summarize_server_response(&body);
                bail!(
                    "{}",
                    localized_error(
                        options.language,
                        &format!(
                            "FastPic вернул неподдерживаемый ответ для {}: {}",
                            path.display(),
                            summary
                        ),
                        &format!(
                            "FastPic returned an unsupported response for {}: {}",
                            path.display(),
                            summary
                        ),
                        "",
                    )
                );
            }
        };
        if anonymous_response.status.as_deref() != Some("ok") {
            let error = anonymous_response
                .error
                .as_deref()
                .unwrap_or("Unknown FastPic error");
            bail!("{error}");
        }
        let view_link = anonymous_response
            .viewfullurl
            .or(anonymous_response.viewurl)
            .map(|url| url.replace("fastpic.org/view/", "fastpic.org/fullview/"));
        return Ok(UploadResponse {
            big_link: anonymous_response.imagepath.clone(),
            thumb_link: anonymous_response.thumbpath,
            view_link,
            short_link: None,
            delete_link: None,
            width: None,
            height: None,
            size: None,
            hotlinked: None,
        });
    }

    serde_json::from_str(&body).with_context(|| {
        localized_error(
            options.language,
            &format!("FastPic вернул некорректный JSON для {}", path.display()),
            &format!("FastPic returned invalid JSON for {}", path.display()),
            "",
        )
    })
}

fn summarize_server_response(body: &str) -> String {
    let lower = body.to_ascii_lowercase();
    let fragment = if lower.contains("<title") {
        extract_tag_content(body, "title")
    } else if lower.contains("<h1") {
        extract_tag_content(body, "h1")
    } else if lower.contains("<body") {
        extract_tag_content(body, "body")
    } else {
        body.to_owned()
    };
    let summary = fragment
        .replace("&nbsp;", " ")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut chars = summary.chars();
    let shortened = chars.by_ref().take(300).collect::<String>();
    if chars.next().is_some() {
        format!("{shortened}...")
    } else if shortened.is_empty() {
        "empty response".to_owned()
    } else {
        shortened
    }
}

fn extract_tag_content(body: &str, tag: &str) -> String {
    let lower = body.to_ascii_lowercase();
    let Some(start) = lower.find(&format!("<{tag}")) else {
        return body.to_owned();
    };
    let Some(content_start) = body[start..].find('>').map(|offset| start + offset + 1) else {
        return body.to_owned();
    };
    let Some(end) = lower[content_start..]
        .find(&format!("</{tag}>"))
        .map(|offset| content_start + offset)
    else {
        return body.to_owned();
    };
    body[content_start..end]
        .replace("<br>", " ")
        .replace("<br/>", " ")
        .replace("<br />", " ")
        .replace('<', " <")
        .split('<')
        .filter_map(|part| part.split_once('>').map(|(_, text)| text).or(Some(part)))
        .collect::<Vec<_>>()
        .join(" ")
}

// ============================================================================
// ОСНОВНАЯ ПРОГРАММА SCREENLIST PRO
// ============================================================================

enum WorkerMsg {
    VideoStarted {
        video_idx: usize,
    },
    FrameProgress {
        video_idx: usize,
        done_frames: usize,
        total_frames: usize,
    },
    VideoFinished {
        video_idx: usize,
        frames: Vec<CapturedFrame>,
        screenshots: Vec<CapturedFrame>,
        sheet: RgbImage,
        elapsed: Duration,
        cols: usize,
        thumb_w: u32,
        thumb_h: u32,
    },
    SingleFrameUpdated {
        video_idx: usize,
        frame_idx: usize,
        frame: CapturedFrame,
        sheet: RgbImage,
        elapsed: Duration,
        cols: usize,
        thumb_w: u32,
        thumb_h: u32,
    },
    VideoError {
        video_idx: usize,
        error: String,
    },
    BatchComplete {
        total_elapsed: Duration,
        total_p_frames: usize,
        total_e_frames: usize,
    },
    ExportProgress {
        done_items: usize,
        total_items: usize,
        status_line: String,
    },
    VideoPathUpdated {
        video_idx: usize,
        new_path: PathBuf,
    },
    ExportComplete {
        elapsed: Duration,
        status_line: String,
    },
}

struct ScreenlistApp {
    videos: Vec<VideoItem>,
    active_idx: Option<usize>,
    active_tab: PreviewTab,
    params: RenderParams,
    topology: CpuTopology,

    show_fastpic_uploader: bool,
    fastpic: FastPicSubApp,

    is_processing: bool,
    is_exporting: bool,
    export_done_items: usize,
    export_total_items: usize,

    pending_layout: bool,
    batch_total_videos: usize,
    batch_done_videos: usize,
    batch_error_count: usize,
    last_batch_error: Option<String>,
    current_done_frames: usize,
    current_total_frames: usize,

    batch_start_instant: Option<Instant>,
    last_elapsed: Option<Duration>,
    status_line: String,

    rx: Receiver<WorkerMsg>,
    tx: Sender<WorkerMsg>,
    font_bytes: Arc<Vec<u8>>,
}

impl ScreenlistApp {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let topology = CpuTopology::detect();
        let workers = topology.recommended_workers();
        let font_bytes = Arc::new(load_system_font_bytes());
        let lang = Language::Ru;

        Self {
            videos: Vec::new(),
            active_idx: None,
            active_tab: PreviewTab::Screenlist,
            params: RenderParams {
                frame_count: 32,
                screenshot_count: 4,
                columns: 4,
                grid_width: 1200,
                workers,
                core_mode: CoreMode::Auto,
                show_timestamps: true,
                turbo_keyframe: true,
                hwaccel: false,
                move_video_to_folder: false,
                jpeg_quality: 85,
                lang,
            },
            topology,
            show_fastpic_uploader: false,
            fastpic: FastPicSubApp::new(lang),
            is_processing: false,
            is_exporting: false,
            export_done_items: 0,
            export_total_items: 0,
            pending_layout: false,
            batch_total_videos: 0,
            batch_done_videos: 0,
            batch_error_count: 0,
            last_batch_error: None,
            current_done_frames: 0,
            current_total_frames: 0,
            batch_start_instant: None,
            last_elapsed: None,
            status_line:
            "Готов к работе. Добавьте видео, выберите папку или перетащите их в окно."
                .to_owned(),
            rx,
            tx,
            font_bytes,
        }
    }

    #[inline]
    fn lang(&self) -> Language {
        self.params.lang
    }

    #[inline]
    fn tr<'a>(&self, ru: &'a str, en: &'a str) -> &'a str {
        self.params.lang.tr(ru, en)
    }

    #[inline]
    fn is_busy(&self) -> bool {
        self.is_processing || self.is_exporting
    }

    fn is_duplicate_video(
        &self,
        path: &Path,
        canonical: Option<&Path>,
        file_size: u64,
        content_hash: u64,
    ) -> bool {
        self.videos.iter().any(|video| {
            if video.path == path {
                return true;
            }
            if let (Some(a), Some(b)) = (video.canonical_path.as_deref(), canonical) {
                if a == b {
                    return true;
                }
            } else if is_same_file_path(&video.path, path) {
                return true;
            }
            file_size > 0
                && content_hash != 0
                && video.file_size == file_size
                && video.content_hash == content_hash
        })
    }

    fn add_video_paths(&mut self, input_paths: Vec<PathBuf>) {
        let lang = self.lang();
        let mut discovered_files = Vec::new();
        let mut had_directory = false;

        for path in input_paths {
            if path.is_dir() {
                had_directory = true;
                collect_video_files_recursive(&path, &mut discovered_files);
            } else if path.is_file() && is_video_file(&path) {
                discovered_files.push(path);
            }
        }

        if discovered_files.is_empty() {
            if had_directory {
                self.status_line = lang
                    .tr(
                        "В выбранной папке (и её подпапках) видеофайлы не найдены.",
                        "No video files found in the selected folder (or its subfolders).",
                    )
                    .to_owned();
            }
            return;
        }

        let mut added_count = 0usize;
        let mut skipped_duplicates = 0usize;
        let mut last_error = None;

        for path in discovered_files {
            let file_size = fs::metadata(&path).map(|metadata| metadata.len()).unwrap_or(0);
            let canonical_path = fs::canonicalize(&path).ok();
            let content_hash = compute_video_fingerprint(&path, file_size);

            if self.is_duplicate_video(
                &path,
                canonical_path.as_deref(),
                file_size,
                content_hash,
            ) {
                skipped_duplicates += 1;
                continue;
            }

            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("video.mp4")
                .to_owned();

            match probe_video(&path, file_size) {
                Ok(probed) => {
                    self.videos.push(VideoItem {
                        path,
                        canonical_path,
                        content_hash,
                        file_name,
                        file_size,
                        duration: probed.duration,
                        width: probed.width,
                        height: probed.height,
                        video_info_line: probed.video_info_line,
                        audio_info_line: probed.audio_info_line,
                        video_info_line_en: probed.video_info_line_en,
                        audio_info_line_en: probed.audio_info_line_en,
                        status: VideoStatus::Ready,
                        frames: Vec::new(),
                        screenshots: Vec::new(),
                        active_screenshot_idx: 0,
                        screenshot_texture: None,
                        sheet_image: None,
                        sheet_texture: None,
                        last_cols: self.params.columns,
                        last_thumb_w: 0,
                        last_thumb_h: 0,
                    });

                    if self.active_idx.is_none() {
                        self.active_idx = Some(0);
                    }
                    added_count += 1;
                }
                Err(error) => {
                    last_error = Some(match lang {
                        Language::Ru => format!("Ошибка чтения {}: {}", file_name, error),
                        Language::En => format!("Error reading {}: {}", file_name, error),
                    });
                }
            }
        }

        if added_count > 0 {
            let dup_note = if skipped_duplicates > 0 {
                match lang {
                    Language::Ru => format!(", пропущено дубликатов: {skipped_duplicates}"),
                    Language::En => format!(", skipped duplicates: {skipped_duplicates}"),
                }
            } else {
                String::new()
            };

            self.status_line = match lang {
                Language::Ru => format!(
                    "Добавлено видео: {} шт.{} (всего в очереди: {})",
                    added_count,
                    dup_note,
                    self.videos.len()
                ),
                Language::En => format!(
                    "Added videos: {}{} (total in queue: {})",
                    added_count,
                    dup_note,
                    self.videos.len()
                ),
            };
        } else if skipped_duplicates > 0 {
            self.status_line = match lang {
                Language::Ru => format!(
                    "Дубликаты пропущены ({} шт.): выбранные видео уже есть в очереди.",
                    skipped_duplicates
                ),
                Language::En => format!(
                    "Duplicates skipped ({}): selected videos are already in the queue.",
                    skipped_duplicates
                ),
            };
        } else if let Some(error) = last_error {
            self.status_line = error;
        }
    }

    fn start_generation(&mut self, indices: Vec<usize>) {
        if self.is_busy() || indices.is_empty() {
            return;
        }

        let lang = self.lang();
        let mut unique_indices = indices;
        unique_indices.sort_unstable();
        unique_indices.dedup();

        let tasks: Vec<(usize, PathBuf, String, u64, f64, u32, u32)> = unique_indices
            .into_iter()
            .filter_map(|index| {
                self.videos.get_mut(index).map(|video| {
                    video.status = VideoStatus::Processing;

                    (
                        index,
                        video.path.clone(),
                        video.file_name.clone(),
                        video.file_size,
                        video.duration,
                        video.width,
                        video.height,
                    )
                })
            })
            .collect();

        if tasks.is_empty() {
            self.status_line = lang
                .tr("Нет видео для обработки.", "No videos to process.")
                .to_owned();
            return;
        }

        self.is_processing = true;
        self.export_done_items = 0;
        self.export_total_items = 0;
        self.batch_total_videos = tasks.len();
        self.batch_done_videos = 0;
        self.batch_error_count = 0;
        self.last_batch_error = None;
        self.current_done_frames = 0;
        self.current_total_frames =
            self.params.frame_count.max(1) + self.params.screenshot_count.min(10);
        self.batch_start_instant = Some(Instant::now());

        let params = self.params;
        let tx = self.tx.clone();
        let font_bytes = self.font_bytes.clone();
        let topology = self.topology.clone();

        std::thread::spawn(move || {
            let batch_t0 = Instant::now();
            let slots = topology.build_worker_slots(params.workers, params.core_mode);
            let worker_count = slots.len().max(1);

            let pool = match rayon::ThreadPoolBuilder::new()
                .num_threads(worker_count)
                .build()
            {
                Ok(pool) => pool,
                Err(error) => {
                    for task in &tasks {
                        let _ = tx.send(WorkerMsg::VideoError {
                            video_idx: task.0,
                            error: match params.lang {
                                Language::Ru => {
                                    format!("Не удалось создать пул потоков: {error}")
                                }
                                Language::En => format!("Failed to create thread pool: {error}"),
                            },
                        });
                    }

                    let _ = tx.send(WorkerMsg::BatchComplete {
                        total_elapsed: batch_t0.elapsed(),
                        total_p_frames: 0,
                        total_e_frames: 0,
                    });
                    return;
                }
            };

            let mut total_p_frames = 0usize;
            let mut total_e_frames = 0usize;

            for (video_idx, path, file_name, file_size, duration, width, height) in tasks {
                let _ = tx.send(WorkerMsg::VideoStarted { video_idx });
                let video_t0 = Instant::now();

                let cols = params.columns.max(1) as u32;
                let padding = 10u32;
                let gap = 2u32;

                let available_width = params
                    .grid_width
                    .saturating_sub(padding * 2 + gap * (cols - 1));

                let thumb_w = (available_width / cols).max(64);
                let aspect = height as f64 / width.max(1) as f64;
                let thumb_h = ((thumb_w as f64 * aspect).round() as u32).max(36);

                let decode_w = thumb_w.min(width).max(64);
                let decode_h = thumb_h.min(height).max(36);
                let total_frames = params.frame_count.max(1);
                let screenshot_count = params.screenshot_count.min(10);
                let total_work_frames = total_frames + screenshot_count;

                let safe_end = (duration - 0.05).max(duration * 0.90).max(0.0);
                let step = safe_end / (total_frames as f64 + 1.0);

                let timestamps: Vec<(usize, f64)> = (0..total_frames)
                    .map(|index| (index, step * (index as f64 + 1.0)))
                    .collect();

                let min_chunks =
                    (total_frames + MAX_FRAMES_PER_DECODER_TASK - 1) / MAX_FRAMES_PER_DECODER_TASK;

                let chunk_count = worker_count
                    .min(total_frames)
                    .max(min_chunks)
                    .min(total_frames);

                let base_size = total_frames / chunk_count;
                let remainder = total_frames % chunk_count;

                let mut chunks = Vec::with_capacity(chunk_count);
                let mut offset = 0usize;

                for chunk_idx in 0..chunk_count {
                    let chunk_size = base_size + usize::from(chunk_idx < remainder);
                    let end = offset + chunk_size;
                    chunks.push(timestamps[offset..end].to_vec());
                    offset = end;
                }

                let done_counter = AtomicUsize::new(0);

                let extracted: Result<Vec<(bool, Vec<(usize, CapturedFrame)>)>, String> =
                    pool.install(|| {
                        chunks
                            .par_iter()
                            .map(|chunk| {
                                let worker_idx = rayon::current_thread_index().unwrap_or(0);
                                let slot = slots[worker_idx % slots.len()];

                                extract_chunk_frames_rgb(
                                    &path,
                                    chunk,
                                    decode_w,
                                    decode_h,
                                    params.turbo_keyframe,
                                    params.hwaccel,
                                    slot,
                                    &done_counter,
                                    total_work_frames,
                                    video_idx,
                                    &tx,
                                )
                                    .map(|frames| (slot.is_p_core, frames))
                            })
                            .collect()
                    });

                let parts = match extracted {
                    Err(error) => {
                        let _ = tx.send(WorkerMsg::VideoError { video_idx, error });
                        continue;
                    }
                    Ok(parts) => parts,
                };

                let mut indexed_frames = Vec::with_capacity(total_frames);
                let mut p_frames = 0usize;
                let mut e_frames = 0usize;

                for (is_p_core, part) in parts {
                    if topology.is_hybrid() {
                        if is_p_core {
                            p_frames += part.len();
                        } else {
                            e_frames += part.len();
                        }
                    }

                    indexed_frames.extend(part);
                }

                if indexed_frames.len() != total_frames {
                    let _ = tx.send(WorkerMsg::VideoError {
                        video_idx,
                        error: match params.lang {
                            Language::Ru => format!(
                                "Получено {} кадров вместо {}.",
                                indexed_frames.len(),
                                total_frames
                            ),
                            Language::En => format!(
                                "Decoded {} frames instead of {}.",
                                indexed_frames.len(),
                                total_frames
                            ),
                        },
                    });
                    continue;
                }

                indexed_frames.sort_by_key(|(frame_idx, _)| *frame_idx);
                let frames: Vec<CapturedFrame> =
                    indexed_frames.into_iter().map(|(_, frame)| frame).collect();

                // Извлечение полноразмерных скриншотов (от 0 до 10)
                let mut screenshots = Vec::with_capacity(screenshot_count);
                if screenshot_count > 0 {
                    let full_w = width.max(64);
                    let full_h = height.max(36);
                    let ss_step = safe_end / (screenshot_count as f64 + 1.0);
                    let ss_timestamps: Vec<Vec<(usize, f64)>> = (0..screenshot_count)
                        .map(|index| vec![(index, ss_step * (index as f64 + 1.0))])
                        .collect();

                    let ss_extracted: Result<Vec<(bool, Vec<(usize, CapturedFrame)>)>, String> =
                        pool.install(|| {
                            ss_timestamps
                                .par_iter()
                                .map(|chunk| {
                                    let worker_idx = rayon::current_thread_index().unwrap_or(0);
                                    let slot = slots[worker_idx % slots.len()];

                                    extract_chunk_frames_rgb(
                                        &path,
                                        chunk,
                                        full_w,
                                        full_h,
                                        params.turbo_keyframe,
                                        params.hwaccel,
                                        slot,
                                        &done_counter,
                                        total_work_frames,
                                        video_idx,
                                        &tx,
                                    )
                                        .map(|ss_part| (slot.is_p_core, ss_part))
                                })
                                .collect()
                        });

                    match ss_extracted {
                        Err(error) => {
                            let _ = tx.send(WorkerMsg::VideoError { video_idx, error });
                            continue;
                        }
                        Ok(ss_parts) => {
                            let mut indexed_ss = Vec::with_capacity(screenshot_count);
                            for (is_p_core, part) in ss_parts {
                                if topology.is_hybrid() {
                                    if is_p_core {
                                        p_frames += part.len();
                                    } else {
                                        e_frames += part.len();
                                    }
                                }
                                indexed_ss.extend(part);
                            }
                            indexed_ss.sort_by_key(|(idx, _)| *idx);
                            screenshots = indexed_ss.into_iter().map(|(_, frame)| frame).collect();
                        }
                    }
                }

                let (sheet, sheet_thumb_w, sheet_thumb_h) = compose_contact_sheet(
                    &file_name,
                    file_size,
                    duration,
                    width,
                    height,
                    &frames,
                    params,
                    &font_bytes,
                );

                total_p_frames += p_frames;
                total_e_frames += e_frames;

                let _ = tx.send(WorkerMsg::VideoFinished {
                    video_idx,
                    frames,
                    screenshots,
                    sheet,
                    elapsed: video_t0.elapsed(),
                    cols: params.columns,
                    thumb_w: sheet_thumb_w,
                    thumb_h: sheet_thumb_h,
                });
            }

            let _ = tx.send(WorkerMsg::BatchComplete {
                total_elapsed: batch_t0.elapsed(),
                total_p_frames,
                total_e_frames,
            });
        });
    }

    fn rerender_cached_sheets(&mut self, ctx: &egui::Context) {
        let started = Instant::now();
        let params = self.params;
        let font_bytes = self.font_bytes.clone();
        let render_video = |(index, video): (usize, &VideoItem)| {
            if video.frames.is_empty() {
                return None;
            }

            let (sheet, thumb_w, thumb_h) = compose_contact_sheet(
                &video.file_name,
                video.file_size,
                video.duration,
                video.width,
                video.height,
                &video.frames,
                params,
                &font_bytes,
            );

            Some((index, sheet, thumb_w, thumb_h))
        };
        let worker_count = self
            .topology
            .build_worker_slots(params.workers, params.core_mode)
            .len()
            .clamp(1, MAX_WORKERS);
        let rendered = match rayon::ThreadPoolBuilder::new()
            .num_threads(worker_count)
            .build()
        {
            Ok(pool) => pool.install(|| {
                self.videos
                    .par_iter()
                    .enumerate()
                    .filter_map(&render_video)
                    .collect::<Vec<_>>()
            }),
            Err(_) => self
                .videos
                .iter()
                .enumerate()
                .filter_map(&render_video)
                .collect(),
        };
        let count = rendered.len();

        for (index, sheet, thumb_w, thumb_h) in rendered {
            if let Some(video) = self.videos.get_mut(index) {
                video.sheet_texture = if self.active_idx == Some(index) {
                    Some(rgb_to_texture(ctx, &video.path.to_string_lossy(), &sheet))
                } else {
                    None
                };
                video.sheet_image = Some(Arc::new(sheet));
                video.last_cols = params.columns;
                video.last_thumb_w = thumb_w;
                video.last_thumb_h = thumb_h;
            }
        }

        if count > 0 {
            let elapsed = started.elapsed();
            self.last_elapsed = Some(elapsed);
            self.status_line = match self.lang() {
                Language::Ru => format!(
                    "Сетка перестроена из RAM-кэша ({} видео) за {:.1} мс",
                    count,
                    elapsed.as_secs_f64() * 1000.0
                ),
                Language::En => format!(
                    "Grid rebuilt from RAM cache ({} videos) in {:.1} ms",
                    count,
                    elapsed.as_secs_f64() * 1000.0
                ),
            };
        }
    }

    fn release_inactive_sheet_textures(&mut self) {
        for (index, video) in self.videos.iter_mut().enumerate() {
            if self.active_idx != Some(index) {
                video.sheet_texture = None;
                video.screenshot_texture = None;
            }
        }
    }

    fn shift_single_frame(&mut self, video_idx: usize, frame_idx: usize) {
        if self.is_busy() {
            return;
        }

        let Some(video) = self.videos.get(video_idx) else {
            return;
        };
        let Some(old_frame) = video.frames.get(frame_idx) else {
            return;
        };

        let mut new_timestamp = old_frame.timestamp + 1.0;
        if new_timestamp >= video.duration - 0.15 {
            new_timestamp = (video.duration * 0.05).max(0.0);
        }

        let path = video.path.clone();
        let file_name = video.file_name.clone();
        let file_size = video.file_size;
        let duration = video.duration;
        let width = video.width;
        let height = video.height;
        let mut frames = video.frames.clone();
        let decode_w = old_frame.rgb.width();
        let decode_h = old_frame.rgb.height();

        let params = self.params;
        let tx = self.tx.clone();
        let font_bytes = self.font_bytes.clone();
        let topology = self.topology.clone();

        let slot = topology
            .build_worker_slots(params.workers, params.core_mode)
            .first()
            .copied()
            .unwrap_or_default();

        self.is_processing = true;
        self.export_done_items = 0;
        self.export_total_items = 0;
        self.batch_total_videos = 1;
        self.batch_done_videos = 0;
        self.batch_error_count = 0;
        self.last_batch_error = None;
        self.current_done_frames = 0;
        self.current_total_frames = 1;
        self.batch_start_instant = Some(Instant::now());

        std::thread::spawn(move || {
            let started = Instant::now();
            let mut p_frames = 0usize;
            let mut e_frames = 0usize;

            match extract_single_frame_rgb(
                &path,
                new_timestamp,
                decode_w,
                decode_h,
                params.turbo_keyframe,
                params.hwaccel,
                slot,
            ) {
                Ok(rgb) => {
                    if topology.is_hybrid() {
                        if slot.is_p_core {
                            p_frames = 1;
                        } else {
                            e_frames = 1;
                        }
                    }

                    let updated = CapturedFrame {
                        timestamp: new_timestamp,
                        rgb: Arc::new(rgb),
                    };
                    frames[frame_idx] = updated.clone();

                    let (sheet, thumb_w, thumb_h) = compose_contact_sheet(
                        &file_name,
                        file_size,
                        duration,
                        width,
                        height,
                        &frames,
                        params,
                        &font_bytes,
                    );

                    let _ = tx.send(WorkerMsg::SingleFrameUpdated {
                        video_idx,
                        frame_idx,
                        frame: updated,
                        sheet,
                        elapsed: started.elapsed(),
                        cols: params.columns,
                        thumb_w,
                        thumb_h,
                    });
                }
                Err(error) => {
                    let _ = tx.send(WorkerMsg::VideoError { video_idx, error });
                }
            }

            let _ = tx.send(WorkerMsg::BatchComplete {
                total_elapsed: started.elapsed(),
                total_p_frames: p_frames,
                total_e_frames: e_frames,
            });
        });
    }

    fn poll_worker_messages(&mut self, ctx: &egui::Context) {
        let lang = self.lang();

        while let Ok(message) = self.rx.try_recv() {
            match message {
                WorkerMsg::VideoStarted { video_idx } => {
                    self.active_idx = Some(video_idx);
                    self.release_inactive_sheet_textures();
                    self.current_done_frames = 0;
                    self.current_total_frames =
                        self.params.frame_count.max(1) + self.params.screenshot_count.min(10);

                    if let Some(video) = self.videos.get_mut(video_idx) {
                        video.status = VideoStatus::Processing;
                        self.status_line = match lang {
                            Language::Ru => {
                                format!("Декодирование (C libav): {} ...", video.file_name)
                            }
                            Language::En => {
                                format!("Decoding (C libav): {} ...", video.file_name)
                            }
                        };
                    }
                }
                WorkerMsg::FrameProgress {
                    video_idx,
                    done_frames,
                    total_frames,
                } => {
                    self.current_done_frames =
                        self.current_done_frames.max(done_frames.min(total_frames));
                    self.current_total_frames = total_frames;

                    if let Some(video) = self.videos.get(video_idx) {
                        self.status_line = match lang {
                            Language::Ru => format!(
                                "Декодирование (C libav): {} ({}/{})",
                                video.file_name, self.current_done_frames, total_frames
                            ),
                            Language::En => format!(
                                "Decoding (C libav): {} ({}/{})",
                                video.file_name, self.current_done_frames, total_frames
                            ),
                        };
                    }
                }
                WorkerMsg::VideoFinished {
                    video_idx,
                    frames,
                    screenshots,
                    sheet,
                    elapsed,
                    cols,
                    thumb_w,
                    thumb_h,
                } => {
                    self.batch_done_videos += 1;

                    if let Some(video) = self.videos.get_mut(video_idx) {
                        video.sheet_texture = if self.active_idx == Some(video_idx) {
                            Some(rgb_to_texture(ctx, &video.path.to_string_lossy(), &sheet))
                        } else {
                            None
                        };
                        video.sheet_image = Some(Arc::new(sheet));
                        video.frames = frames;
                        video.screenshots = screenshots;
                        video.active_screenshot_idx = 0;
                        video.screenshot_texture = None;
                        video.status = VideoStatus::Done(elapsed);
                        video.last_cols = cols;
                        video.last_thumb_w = thumb_w;
                        video.last_thumb_h = thumb_h;
                    }
                }
                WorkerMsg::SingleFrameUpdated {
                    video_idx,
                    frame_idx,
                    frame,
                    sheet,
                    elapsed,
                    cols,
                    thumb_w,
                    thumb_h,
                } => {
                    self.batch_done_videos = 1;
                    self.current_done_frames = 1;

                    if let Some(video) = self.videos.get_mut(video_idx) {
                        if let Some(slot) = video.frames.get_mut(frame_idx) {
                            *slot = frame;
                        }

                        video.sheet_texture = if self.active_idx == Some(video_idx) {
                            Some(rgb_to_texture(ctx, &video.path.to_string_lossy(), &sheet))
                        } else {
                            None
                        };
                        video.sheet_image = Some(Arc::new(sheet));
                        video.status = VideoStatus::Done(elapsed);
                        video.last_cols = cols;
                        video.last_thumb_w = thumb_w;
                        video.last_thumb_h = thumb_h;

                        self.status_line = match lang {
                            Language::Ru => format!(
                                "Кадр #{} сдвинут (+1.0с) за {:.0} мс",
                                frame_idx + 1,
                                elapsed.as_secs_f64() * 1000.0
                            ),
                            Language::En => format!(
                                "Frame #{} shifted (+1.0s) in {:.0} ms",
                                frame_idx + 1,
                                elapsed.as_secs_f64() * 1000.0
                            ),
                        };
                    }
                }
                WorkerMsg::VideoError { video_idx, error } => {
                    self.batch_done_videos += 1;
                    self.batch_error_count += 1;
                    self.last_batch_error = Some(error.clone());

                    if let Some(video) = self.videos.get_mut(video_idx) {
                        video.status = VideoStatus::Error(error.clone());
                    }

                    self.status_line = match lang {
                        Language::Ru => format!("Ошибка: {error}"),
                        Language::En => format!("Error: {error}"),
                    };
                }
                WorkerMsg::BatchComplete {
                    total_elapsed,
                    total_p_frames,
                    total_e_frames,
                } => {
                    self.is_processing = false;
                    self.last_elapsed = Some(total_elapsed);
                    self.batch_start_instant = None;

                    let mut summary = match lang {
                        Language::Ru => format!(
                            "Готово за {:.3} сек ({:.0} мс)",
                            total_elapsed.as_secs_f64(),
                            total_elapsed.as_secs_f64() * 1000.0
                        ),
                        Language::En => format!(
                            "Done in {:.3} sec ({:.0} ms)",
                            total_elapsed.as_secs_f64(),
                            total_elapsed.as_secs_f64() * 1000.0
                        ),
                    };

                    if self.topology.is_hybrid() {
                        match lang {
                            Language::Ru => summary.push_str(&format!(
                                " | P-ядра: {} кадров | E-ядра: {} кадров",
                                total_p_frames, total_e_frames
                            )),
                            Language::En => summary.push_str(&format!(
                                " | P-cores: {} frames | E-cores: {} frames",
                                total_p_frames, total_e_frames
                            )),
                        }
                    }

                    self.status_line = if self.batch_error_count == 0 {
                        format!("✅ {summary}")
                    } else {
                        match lang {
                            Language::Ru => format!(
                                "⚠️ {summary} | Ошибок: {} | Последняя: {}",
                                self.batch_error_count,
                                self.last_batch_error
                                    .as_deref()
                                    .unwrap_or("неизвестная ошибка")
                            ),
                            Language::En => format!(
                                "⚠️ {summary} | Errors: {} | Last: {}",
                                self.batch_error_count,
                                self.last_batch_error
                                    .as_deref()
                                    .unwrap_or("unknown error")
                            ),
                        }
                    };
                }
                WorkerMsg::ExportProgress {
                    done_items,
                    total_items,
                    status_line,
                } => {
                    self.export_done_items = done_items.min(total_items);
                    self.export_total_items = total_items;
                    self.status_line = status_line;
                }
                WorkerMsg::VideoPathUpdated {
                    video_idx,
                    new_path,
                } => {
                    if let Some(video) = self.videos.get_mut(video_idx) {
                        video.canonical_path = fs::canonicalize(&new_path).ok();
                        video.path = new_path;
                    }
                }
                WorkerMsg::ExportComplete {
                    elapsed,
                    status_line,
                } => {
                    self.is_exporting = false;
                    self.export_done_items = 0;
                    self.export_total_items = 0;
                    self.last_elapsed = Some(elapsed);
                    self.batch_start_instant = None;
                    self.status_line = status_line;
                }
            }
        }
    }

    fn show_video_queue(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang();
        ui.heading(match lang {
            Language::Ru => format!("Очередь видео ({})", self.videos.len()),
            Language::En => format!("Video Queue ({})", self.videos.len()),
        });
        ui.separator();

        let busy = self.is_busy();
        let active_idx = self.active_idx;
        let mut selected_idx = None;
        let mut remove_idx = None;
        let mut copied_line: Option<String> = None;

        egui::ScrollArea::vertical()
            .id_source("video_queue_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let card_width = (ui.available_width() - 8.0).max(120.0);

                for (index, video) in self.videos.iter().enumerate() {
                    let selected = active_idx == Some(index);

                    ui.group(|ui| {
                        ui.set_width(card_width);

                        ui.horizontal_top(|ui| {
                            let btn_space = if busy { 0.0 } else { 28.0 };
                            let text_width = (ui.available_width() - btn_space).max(60.0);

                            ui.allocate_ui_with_layout(
                                egui::vec2(text_width, 0.0),
                                egui::Layout::top_down_justified(egui::Align::Min),
                                |ui| {
                                    ui.style_mut().wrap = Some(true);
                                    if ui
                                        .selectable_label(
                                            selected,
                                            egui::RichText::new(&video.file_name).strong(),
                                        )
                                        .on_hover_text(video.path.display().to_string())
                                        .clicked()
                                    {
                                        selected_idx = Some(index);
                                    }
                                },
                            );

                            if !busy
                                && ui
                                .small_button("✖")
                                .on_hover_text(
                                    lang.tr("Удалить из очереди", "Remove from queue"),
                                )
                                .clicked()
                            {
                                remove_idx = Some(index);
                            }
                        });

                        ui.label(
                            egui::RichText::new(format!(
                                "{} | {}×{} | {}",
                                format_duration(video.duration),
                                video.width,
                                video.height,
                                format_file_size_lang(video.file_size, lang)
                            ))
                                .size(11.5)
                                .color(egui::Color32::GRAY),
                        );

                        // Сначала строка с информацией о видео (копируется при клике)
                        let v_line = video.video_info(lang);
                        let video_resp = ui.add(
                            egui::Label::new(
                                egui::RichText::new(v_line)
                                    .size(11.0)
                                    .color(egui::Color32::from_rgb(175, 195, 220)),
                            )
                                .wrap(true)
                                .sense(egui::Sense::click()),
                        );
                        if video_resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if video_resp.clicked() {
                            copied_line = Some(v_line.to_owned());
                        }
                        video_resp.on_hover_text(lang.tr(
                            "Нажмите, чтобы скопировать в буфер обмена",
                            "Click to copy to clipboard",
                        ));

                        // Затем строка с информацией об аудио (копируется при клике)
                        let a_line = video.audio_info(lang);
                        let audio_resp = ui.add(
                            egui::Label::new(
                                egui::RichText::new(a_line)
                                    .size(11.0)
                                    .color(egui::Color32::from_rgb(165, 185, 210)),
                            )
                                .wrap(true)
                                .sense(egui::Sense::click()),
                        );
                        if audio_resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if audio_resp.clicked() {
                            copied_line = Some(a_line.to_owned());
                        }
                        audio_resp.on_hover_text(lang.tr(
                            "Нажмите, чтобы скопировать в буфер обмена",
                            "Click to copy to clipboard",
                        ));

                        match &video.status {
                            VideoStatus::Ready => {
                                ui.label(lang.tr("⏳ Ожидает", "⏳ Queued"));
                            }
                            VideoStatus::Processing => {
                                ui.label(
                                    egui::RichText::new(
                                        lang.tr("⚙ Обработка...", "⚙ Processing..."),
                                    )
                                        .color(egui::Color32::from_rgb(255, 200, 80)),
                                );
                            }
                            VideoStatus::Done(elapsed) => {
                                let ss_info = if !video.screenshots.is_empty() {
                                    match lang {
                                        Language::Ru => {
                                            format!(" (+{} скринш.)", video.screenshots.len())
                                        }
                                        Language::En => {
                                            format!(" (+{} shots)", video.screenshots.len())
                                        }
                                    }
                                } else {
                                    String::new()
                                };
                                let done_text = match lang {
                                    Language::Ru => format!(
                                        "✅ Готово за {:.3} сек{}",
                                        elapsed.as_secs_f64(),
                                        ss_info
                                    ),
                                    Language::En => format!(
                                        "✅ Done in {:.3} sec{}",
                                        elapsed.as_secs_f64(),
                                        ss_info
                                    ),
                                };
                                ui.label(
                                    egui::RichText::new(done_text)
                                        .color(egui::Color32::from_rgb(80, 230, 145)),
                                );
                            }
                            VideoStatus::Error(error) => {
                                ui.label(
                                    egui::RichText::new(format!("❌ {}", error))
                                        .color(egui::Color32::LIGHT_RED),
                                );
                            }
                        }
                    });

                    ui.add_space(4.0);
                }
            });

        if let Some(text) = copied_line {
            copy_text_to_clipboard(ui.ctx(), &text);
            self.status_line = match lang {
                Language::Ru => format!("📋 Скопировано в буфер обмена: {}", text),
                Language::En => format!("📋 Copied to clipboard: {}", text),
            };
        }

        if let Some(index) = selected_idx {
            if index < self.videos.len() {
                self.active_idx = Some(index);
            }
        }

        if let Some(index) = remove_idx {
            if index < self.videos.len() {
                self.videos.remove(index);

                self.active_idx = match self.active_idx {
                    None => None,
                    Some(_) if self.videos.is_empty() => None,
                    Some(active) if active > index => Some(active - 1),
                    Some(active) if active == index => Some(index.min(self.videos.len() - 1)),
                    Some(active) => Some(active),
                };
            }
        }

        self.release_inactive_sheet_textures();
    }

    fn show_preview_panel(&mut self, ui: &mut egui::Ui) -> Option<(usize, usize)> {
        let lang = self.lang();
        let ss_count = self
            .active_idx
            .and_then(|idx| self.videos.get(idx))
            .map(|v| v.screenshots.len())
            .unwrap_or(0);

        // Вкладки выбора между Скринлистом и Скриншотами над превью
        ui.horizontal(|ui| {
            ui.selectable_value(
                &mut self.active_tab,
                PreviewTab::Screenlist,
                egui::RichText::new(lang.tr("📋 Скринлист", "📋 Screenlist"))
                    .size(14.5)
                    .strong(),
            );

            ui.add_space(6.0);

            let ss_tab_title = if ss_count > 0 {
                match lang {
                    Language::Ru => format!("🖼 Скриншоты ({})", ss_count),
                    Language::En => format!("🖼 Screenshots ({})", ss_count),
                }
            } else {
                lang.tr("🖼 Скриншоты", "🖼 Screenshots").to_owned()
            };

            ui.selectable_value(
                &mut self.active_tab,
                PreviewTab::Screenshots,
                egui::RichText::new(ss_tab_title).size(14.5).strong(),
            );
        });
        ui.separator();

        match self.active_tab {
            PreviewTab::Screenlist => self.show_active_sheet(ui),
            PreviewTab::Screenshots => {
                self.show_active_screenshots(ui);
                None
            }
        }
    }

    fn show_active_sheet(&mut self, ui: &mut egui::Ui) -> Option<(usize, usize)> {
        let lang = self.lang();
        let Some(active_idx) = self.active_idx else {
            ui.centered_and_justified(|ui| {
                ui.label(lang.tr(
                    "Добавьте видео или папку, чтобы создать скринлист.",
                    "Add videos or a folder to generate a screenlist.",
                ));
            });
            return None;
        };

        let texture_ctx = ui.ctx().clone();
        if let Some(video) = self.videos.get_mut(active_idx) {
            if video.sheet_texture.is_none() {
                if let Some(sheet) = &video.sheet_image {
                    video.sheet_texture = Some(rgb_to_texture(
                        &texture_ctx,
                        &video.path.to_string_lossy(),
                        sheet,
                    ));
                }
            }
        }

        let Some(video) = self.videos.get(active_idx) else {
            ui.centered_and_justified(|ui| {
                ui.label(lang.tr("Видео не выбрано.", "No video selected."));
            });
            return None;
        };

        let Some(texture) = &video.sheet_texture else {
            ui.centered_and_justified(|ui| {
                ui.label(lang.tr(
                    "Нажмите «Создать скринлист», чтобы начать обработку.",
                    "Click \"Generate All\" or \"Generate Selected\" to start processing.",
                ));
            });
            return None;
        };

        ui.label(
            egui::RichText::new(lang.tr(
                "Подсказка: кликните по кадру на листе, чтобы сдвинуть его на +1 секунду.",
                "Tip: click any frame on the sheet to shift it by +1 second.",
            ))
                .size(12.5)
                .color(egui::Color32::from_rgb(160, 180, 230)),
        );
        ui.separator();

        let mut frame_to_shift = None;

        egui::ScrollArea::both()
            .id_source("active_sheet_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let available_width = (ui.available_width() - 12.0).max(200.0);
                let texture_size = texture.size_vec2();
                let scale = (available_width / texture_size.x.max(1.0)).min(1.0);
                let display_size = texture_size * scale;

                let response = ui.add(
                    egui::Image::new((texture.id(), display_size)).sense(egui::Sense::click()),
                );

                if response.clicked() {
                    if let Some(position) = response.interact_pointer_pos() {
                        let relative_x = (position.x - response.rect.min.x) / scale;
                        let relative_y = (position.y - response.rect.min.y) / scale;

                        let header_height = 46.0;
                        let padding = 10.0;
                        let gap = 2.0;

                        if relative_y >= header_height + padding && relative_x >= padding {
                            let cell_w = video.last_thumb_w as f32 + gap;
                            let cell_h = video.last_thumb_h as f32 + gap;

                            if cell_w > 0.0 && cell_h > 0.0 {
                                let col = ((relative_x - padding) / cell_w).floor() as usize;
                                let row = ((relative_y - header_height - padding) / cell_h)
                                    .floor() as usize;

                                if col < video.last_cols {
                                    let frame_idx = row * video.last_cols + col;
                                    if frame_idx < video.frames.len() {
                                        frame_to_shift = Some((active_idx, frame_idx));
                                    }
                                }
                            }
                        }
                    }
                }
            });

        frame_to_shift
    }

    fn show_active_screenshots(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang();
        let Some(active_idx) = self.active_idx else {
            ui.centered_and_justified(|ui| {
                ui.label(lang.tr(
                    "Добавьте видео или папку, чтобы создать скриншоты.",
                    "Add videos or a folder to generate screenshots.",
                ));
            });
            return;
        };

        let screenshot_count_param = self.params.screenshot_count;

        let Some(video) = self.videos.get_mut(active_idx) else {
            ui.centered_and_justified(|ui| {
                ui.label(lang.tr("Видео не выбрано.", "No video selected."));
            });
            return;
        };

        if video.screenshots.is_empty() {
            ui.centered_and_justified(|ui| {
                if screenshot_count_param == 0 {
                    ui.label(lang.tr(
                        "Количество полноразмерных скриншотов установлено в 0. Укажите от 1 до 10 сверху и запустите генерацию.",
                        "Full-resolution screenshot count is set to 0. Set it to 1–10 in the top bar and run generation.",
                    ));
                } else {
                    ui.label(lang.tr(
                        "Нажмите «Создать скринлист», чтобы сгенерировать полноразмерные скриншоты.",
                        "Click \"Generate All\" or \"Generate Selected\" to generate full-resolution screenshots.",
                    ));
                }
            });
            return;
        }

        let total = video.screenshots.len();
        if video.active_screenshot_idx >= total {
            video.active_screenshot_idx = 0;
        }

        // Поддержка перелистывания клавишами влево/вправо
        if total > 1 {
            if ui.input(|i| i.key_pressed(egui::Key::ArrowLeft)) {
                video.active_screenshot_idx = (video.active_screenshot_idx + total - 1) % total;
            }
            if ui.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
                video.active_screenshot_idx = (video.active_screenshot_idx + 1) % total;
            }
        }

        let current_idx = video.active_screenshot_idx;
        let need_reload = match &video.screenshot_texture {
            Some((cached_idx, _)) => *cached_idx != current_idx,
            None => true,
        };

        if need_reload {
            if let Some(shot) = video.screenshots.get(current_idx) {
                let tex_name = format!(
                    "{}_screenshot_{}",
                    video.path.to_string_lossy(),
                    current_idx + 1
                );
                let handle = rgb_to_texture(ui.ctx(), &tex_name, &shot.rgb);
                video.screenshot_texture = Some((current_idx, handle));
            }
        }

        let shot_timestamp = video
            .screenshots
            .get(current_idx)
            .map(|s| s.timestamp)
            .unwrap_or(0.0);
        let shot_w = video
            .screenshots
            .get(current_idx)
            .map(|s| s.rgb.width())
            .unwrap_or(video.width);
        let shot_h = video
            .screenshots
            .get(current_idx)
            .map(|s| s.rgb.height())
            .unwrap_or(video.height);

        let nav_bar_height = 46.0;
        let image_area_height = (ui.available_height() - nav_bar_height).max(160.0);
        let image_area_width = (ui.available_width() - 8.0).max(200.0);

        if let Some((_, texture)) = &video.screenshot_texture {
            let tex_size = texture.size_vec2();
            let scale_x = image_area_width / tex_size.x.max(1.0);
            let scale_y = (image_area_height - 8.0).max(100.0) / tex_size.y.max(1.0);
            let scale = scale_x.min(scale_y).min(1.0);
            let display_size = tex_size * scale;

            ui.allocate_ui_with_layout(
                egui::vec2(image_area_width, image_area_height),
                egui::Layout::centered_and_justified(egui::Direction::TopDown),
                |ui| {
                    ui.add(egui::Image::new((texture.id(), display_size)));
                },
            );
        }

        ui.separator();

        // Нижняя панель навигации под скриншотом: Предыдущий | текущий / всего | Следующий
        ui.horizontal(|ui| {
            let prev_clicked = ui
                .add_enabled(
                    total > 1,
                    egui::Button::new(
                        egui::RichText::new(lang.tr("⬅ Предыдущий", "⬅ Previous"))
                            .size(14.0)
                            .strong(),
                    )
                        .min_size(egui::vec2(130.0, 30.0)),
                )
                .clicked();

            if prev_clicked {
                video.active_screenshot_idx = (video.active_screenshot_idx + total - 1) % total;
            }

            ui.add_space(12.0);

            ui.label(
                egui::RichText::new(format!("{} / {}", current_idx + 1, total))
                    .size(16.0)
                    .strong()
                    .color(egui::Color32::from_rgb(130, 195, 255)),
            );

            ui.add_space(8.0);

            ui.label(
                egui::RichText::new(format!(
                    "({}_ScreenShot{} | {} | {}×{})",
                    stem_name(&video.file_name),
                    current_idx + 1,
                    format_duration(shot_timestamp),
                    shot_w,
                    shot_h
                ))
                    .size(12.5)
                    .color(egui::Color32::GRAY),
            );

            ui.add_space(12.0);

            let next_clicked = ui
                .add_enabled(
                    total > 1,
                    egui::Button::new(
                        egui::RichText::new(lang.tr("Следующий ➡", "Next ➡"))
                            .size(14.0)
                            .strong(),
                    )
                        .min_size(egui::vec2(130.0, 30.0)),
                )
                .clicked();

            if next_clicked {
                video.active_screenshot_idx = (video.active_screenshot_idx + 1) % total;
            }
        });
    }

    fn save_current_sheet(&mut self) {
        if self.is_busy() {
            return;
        }

        let lang = self.lang();
        let Some(active_idx) = self.active_idx else {
            return;
        };
        let Some(video) = self.videos.get(active_idx) else {
            return;
        };
        let Some(sheet_image) = video.sheet_image.clone() else {
            return;
        };

        let base_stem = stem_name(&video.file_name);
        let folder_name = sanitize_folder_name(&base_stem);
        let default_name = format!("{}_screenlist.jpg", base_stem);
        let jpeg_quality = self.params.jpeg_quality;
        let move_video = self.params.move_video_to_folder;
        let screenshots = video.screenshots.clone();
        let video_path = video.path.clone();
        let video_file_name = video.file_name.clone();

        if let Some(path) = rfd::FileDialog::new()
            .set_file_name(&default_name)
            .add_filter("JPEG Image", &["jpg", "jpeg"])
            .add_filter("PNG Image", &["png"])
            .save_file()
        {
            let total_items = 1 + screenshots.len() + usize::from(move_video);
            self.is_exporting = true;
            self.export_done_items = 0;
            self.export_total_items = total_items;
            self.batch_start_instant = Some(Instant::now());
            self.status_line = match lang {
                Language::Ru => format!("Сохранение {} ...", video_file_name),
                Language::En => format!("Saving {} ...", video_file_name),
            };

            let tx = self.tx.clone();

            std::thread::spawn(move || {
                let started = Instant::now();
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("jpg")
                    .to_lowercase();
                let parent_dir = path.parent().unwrap_or_else(|| Path::new("."));

                let already_in_video_dir = parent_dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.eq_ignore_ascii_case(&folder_name))
                    .unwrap_or(false);

                let video_dir = if already_in_video_dir {
                    parent_dir.to_path_buf()
                } else {
                    parent_dir.join(&folder_name)
                };

                if let Err(error) = fs::create_dir_all(&video_dir) {
                    let msg = match lang {
                        Language::Ru => {
                            format!("Ошибка создания папки {}: {error}", video_dir.display())
                        }
                        Language::En => {
                            format!("Error creating folder {}: {error}", video_dir.display())
                        }
                    };
                    let _ = tx.send(WorkerMsg::ExportComplete {
                        elapsed: started.elapsed(),
                        status_line: msg,
                    });
                    return;
                }

                let sheet_file_name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_owned())
                    .unwrap_or_else(|| default_name.clone());
                let sheet_path = video_dir.join(&sheet_file_name);

                if let Err(error) = save_rgb_image(&sheet_image, &sheet_path, jpeg_quality) {
                    let msg = match lang {
                        Language::Ru => format!("Ошибка сохранения: {error}"),
                        Language::En => format!("Save error: {error}"),
                    };
                    let _ = tx.send(WorkerMsg::ExportComplete {
                        elapsed: started.elapsed(),
                        status_line: msg,
                    });
                    return;
                }

                let mut done_items = 1usize;
                let _ = tx.send(WorkerMsg::ExportProgress {
                    done_items,
                    total_items,
                    status_line: match lang {
                        Language::Ru => format!("Сохранено: {sheet_file_name}"),
                        Language::En => format!("Saved: {sheet_file_name}"),
                    },
                });

                let mut saved_shots = 0usize;
                for (shot_idx, shot) in screenshots.iter().enumerate() {
                    let shot_name = format!("{}_ScreenShot{}.{}", base_stem, shot_idx + 1, ext);
                    let shot_path = video_dir.join(&shot_name);
                    if save_rgb_image(&shot.rgb, &shot_path, jpeg_quality).is_ok() {
                        saved_shots += 1;
                    }
                    done_items += 1;
                    let _ = tx.send(WorkerMsg::ExportProgress {
                        done_items,
                        total_items,
                        status_line: match lang {
                            Language::Ru => format!("Сохранение скриншота: {shot_name}"),
                            Language::En => format!("Saving screenshot: {shot_name}"),
                        },
                    });
                }

                let mut move_note = String::new();
                if move_video {
                    let dst_video_path = video_dir.join(&video_file_name);
                    match move_video_file(&video_path, &dst_video_path, lang) {
                        Ok(()) => {
                            let _ = tx.send(WorkerMsg::VideoPathUpdated {
                                video_idx: active_idx,
                                new_path: dst_video_path,
                            });
                            move_note =
                                lang.tr(" + видео перенесено", " + video moved").to_owned();
                        }
                        Err(err) => {
                            move_note = match lang {
                                Language::Ru => format!(" (ошибка переноса видео: {err})"),
                                Language::En => format!(" (video move error: {err})"),
                            };
                        }
                    }
                    done_items += 1;
                    let _ = tx.send(WorkerMsg::ExportProgress {
                        done_items,
                        total_items,
                        status_line: match lang {
                            Language::Ru => format!("Перенос видео: {video_file_name}"),
                            Language::En => format!("Moving video: {video_file_name}"),
                        },
                    });
                }

                let final_status = if saved_shots > 0 {
                    match lang {
                        Language::Ru => format!(
                            "Сохранено в папку {}: скринлист + {} скриншотов{}",
                            video_dir.display(),
                            saved_shots,
                            move_note
                        ),
                        Language::En => format!(
                            "Saved to folder {}: screenlist + {} screenshots{}",
                            video_dir.display(),
                            saved_shots,
                            move_note
                        ),
                    }
                } else {
                    match lang {
                        Language::Ru => format!("Сохранено: {}{}", sheet_path.display(), move_note),
                        Language::En => format!("Saved: {}{}", sheet_path.display(), move_note),
                    }
                };

                let _ = tx.send(WorkerMsg::ExportComplete {
                    elapsed: started.elapsed(),
                    status_line: final_status,
                });
            });
        }
    }

    fn save_all_to_folder(&mut self) {
        if self.is_busy() {
            return;
        }

        let lang = self.lang();

        struct FolderExportTask {
            video_idx: usize,
            file_name: String,
            video_path: PathBuf,
            sheet: Arc<RgbImage>,
            screenshots: Vec<CapturedFrame>,
        }

        let tasks: Vec<FolderExportTask> = self
            .videos
            .iter()
            .enumerate()
            .filter_map(|(video_idx, video)| {
                video.sheet_image.as_ref().map(|sheet| FolderExportTask {
                    video_idx,
                    file_name: video.file_name.clone(),
                    video_path: video.path.clone(),
                    sheet: Arc::clone(sheet),
                    screenshots: video.screenshots.clone(),
                })
            })
            .collect();

        if tasks.is_empty() {
            return;
        }

        let jpeg_quality = self.params.jpeg_quality;
        let move_video = self.params.move_video_to_folder;

        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
            let total_items: usize = tasks
                .iter()
                .map(|t| 1 + t.screenshots.len() + usize::from(move_video))
                .sum::<usize>()
                .max(1);

            self.is_exporting = true;
            self.export_done_items = 0;
            self.export_total_items = total_items;
            self.batch_start_instant = Some(Instant::now());
            self.status_line = match lang {
                Language::Ru => format!("Сохранение в папку {} ...", folder.display()),
                Language::En => format!("Saving to folder {} ...", folder.display()),
            };

            let tx = self.tx.clone();

            std::thread::spawn(move || {
                let started = Instant::now();
                let mut saved_sheets = 0usize;
                let mut saved_screenshots = 0usize;
                let mut moved_videos = 0usize;
                let mut move_errors = 0usize;
                let mut done_items = 0usize;
                let mut used_stems = HashSet::new();

                for (idx, task) in tasks.into_iter().enumerate() {
                    let raw_stem = stem_name(&task.file_name);
                    let clean_stem = sanitize_folder_name(&raw_stem);
                    let unique_stem = if used_stems.insert(clean_stem.to_lowercase()) {
                        clean_stem
                    } else {
                        format!("{:02}_{}", idx + 1, clean_stem)
                    };

                    let video_folder = folder.join(&unique_stem);
                    if fs::create_dir_all(&video_folder).is_err() {
                        done_items += 1 + task.screenshots.len() + usize::from(move_video);
                        let _ = tx.send(WorkerMsg::ExportProgress {
                            done_items,
                            total_items,
                            status_line: match lang {
                                Language::Ru => {
                                    format!("Пропуск (ошибка папки): {}", task.file_name)
                                }
                                Language::En => format!("Skipped (folder error): {}", task.file_name),
                            },
                        });
                        continue;
                    }

                    let sheet_name = format!("{unique_stem}_screenlist.jpg");
                    let sheet_output = video_folder.join(&sheet_name);
                    if save_rgb_image(&task.sheet, &sheet_output, jpeg_quality).is_ok() {
                        saved_sheets += 1;
                    }
                    done_items += 1;
                    let _ = tx.send(WorkerMsg::ExportProgress {
                        done_items,
                        total_items,
                        status_line: match lang {
                            Language::Ru => format!("Сохранение: {sheet_name}"),
                            Language::En => format!("Saving: {sheet_name}"),
                        },
                    });

                    for (shot_idx, shot) in task.screenshots.iter().enumerate() {
                        let shot_name = format!("{}_ScreenShot{}.jpg", unique_stem, shot_idx + 1);
                        let shot_output = video_folder.join(&shot_name);
                        if save_rgb_image(&shot.rgb, &shot_output, jpeg_quality).is_ok() {
                            saved_screenshots += 1;
                        }
                        done_items += 1;
                        let _ = tx.send(WorkerMsg::ExportProgress {
                            done_items,
                            total_items,
                            status_line: match lang {
                                Language::Ru => format!("Сохранение: {shot_name}"),
                                Language::En => format!("Saving: {shot_name}"),
                            },
                        });
                    }

                    if move_video {
                        let dst_video_path = video_folder.join(&task.file_name);
                        match move_video_file(&task.video_path, &dst_video_path, lang) {
                            Ok(()) => {
                                let _ = tx.send(WorkerMsg::VideoPathUpdated {
                                    video_idx: task.video_idx,
                                    new_path: dst_video_path,
                                });
                                moved_videos += 1;
                            }
                            Err(_) => {
                                move_errors += 1;
                            }
                        }
                        done_items += 1;
                        let _ = tx.send(WorkerMsg::ExportProgress {
                            done_items,
                            total_items,
                            status_line: match lang {
                                Language::Ru => format!("Перенос видео: {}", task.file_name),
                                Language::En => format!("Moving video: {}", task.file_name),
                            },
                        });
                    }
                }

                let move_summary = if move_video {
                    if move_errors == 0 {
                        match lang {
                            Language::Ru => format!(", перенесено видео: {moved_videos}"),
                            Language::En => format!(", moved videos: {moved_videos}"),
                        }
                    } else {
                        match lang {
                            Language::Ru => format!(
                                ", перенесено видео: {moved_videos} (ошибок переноса: {move_errors})"
                            ),
                            Language::En => format!(
                                ", moved videos: {moved_videos} (move errors: {move_errors})"
                            ),
                        }
                    }
                } else {
                    String::new()
                };

                let final_status = if saved_screenshots > 0 {
                    match lang {
                        Language::Ru => format!(
                            "Сохранено {} скринлистов и {} скриншотов{} (по отдельным папкам) в {}",
                            saved_sheets,
                            saved_screenshots,
                            move_summary,
                            folder.display()
                        ),
                        Language::En => format!(
                            "Saved {} screenlists and {} screenshots{} (in separate folders) to {}",
                            saved_sheets,
                            saved_screenshots,
                            move_summary,
                            folder.display()
                        ),
                    }
                } else {
                    match lang {
                        Language::Ru => format!(
                            "Сохранено {} скринлистов{} (по отдельным папкам) в {}",
                            saved_sheets,
                            move_summary,
                            folder.display()
                        ),
                        Language::En => format!(
                            "Saved {} screenlists{} (in separate folders) to {}",
                            saved_sheets,
                            move_summary,
                            folder.display()
                        ),
                    }
                };

                let _ = tx.send(WorkerMsg::ExportComplete {
                    elapsed: started.elapsed(),
                    status_line: final_status,
                });
            });
        }
    }

    fn save_all_to_zip(&mut self) {
        if self.is_busy() {
            return;
        }

        let lang = self.lang();
        let mut entries: Vec<(String, Arc<RgbImage>)> = Vec::new();
        let mut used_stems = HashSet::new();
        let mut total_shots = 0usize;
        let mut ready_video_count = 0usize;

        for (idx, video) in self.videos.iter().enumerate() {
            let Some(sheet) = video.sheet_image.as_ref() else {
                continue;
            };
            ready_video_count += 1;

            let raw_stem = stem_name(&video.file_name);
            let clean_stem = sanitize_folder_name(&raw_stem);
            let unique_stem = if used_stems.insert(clean_stem.to_lowercase()) {
                clean_stem
            } else {
                format!("{:02}_{}", idx + 1, clean_stem)
            };

            entries.push((
                format!("{unique_stem}/{unique_stem}_screenlist.jpg"),
                Arc::clone(sheet),
            ));

            for (shot_idx, shot) in video.screenshots.iter().enumerate() {
                entries.push((
                    format!("{unique_stem}/{unique_stem}_ScreenShot{}.jpg", shot_idx + 1),
                    Arc::clone(&shot.rgb),
                ));
                total_shots += 1;
            }
        }

        if entries.is_empty() {
            return;
        }

        let default_name = format!("screenlists_{}_videos.zip", ready_video_count);
        let jpeg_quality = self.params.jpeg_quality;

        if let Some(zip_path) = rfd::FileDialog::new()
            .set_file_name(&default_name)
            .add_filter("ZIP Archive", &["zip"])
            .save_file()
        {
            let total_items = entries.len().max(1);
            self.is_exporting = true;
            self.export_done_items = 0;
            self.export_total_items = total_items;
            self.batch_start_instant = Some(Instant::now());
            self.status_line = match lang {
                Language::Ru => format!("Создание ZIP-архива {} ...", zip_path.display()),
                Language::En => format!("Creating ZIP archive {} ...", zip_path.display()),
            };

            let tx = self.tx.clone();

            std::thread::spawn(move || {
                let started = Instant::now();

                let result = (|| -> Result<(), String> {
                    let file = File::create(&zip_path).map_err(|error| error.to_string())?;
                    let mut zip = zip::ZipWriter::new(file);
                    let mut done_items = 0usize;

                    for batch in entries.chunks(ZIP_ENCODE_BATCH_SIZE) {
                        let encoded: Vec<Result<Vec<u8>, String>> = batch
                            .par_iter()
                            .map(|(_, image)| encode_jpeg(image.as_ref(), jpeg_quality))
                            .collect();

                        for ((entry_name, _), jpeg_buffer) in batch.iter().zip(encoded) {
                            let options = zip::write::FileOptions::default()
                                .compression_method(zip::CompressionMethod::Stored);

                            zip.start_file(entry_name.as_str(), options)
                                .map_err(|error| error.to_string())?;
                            zip.write_all(&jpeg_buffer?)
                                .map_err(|error| error.to_string())?;

                            done_items += 1;
                            let _ = tx.send(WorkerMsg::ExportProgress {
                                done_items,
                                total_items,
                                status_line: match lang {
                                    Language::Ru => format!("Упаковка в ZIP: {entry_name}"),
                                    Language::En => format!("Packing to ZIP: {entry_name}"),
                                },
                            });
                        }
                    }

                    zip.finish().map_err(|error| error.to_string())?;
                    Ok(())
                })();

                let final_status = match result {
                    Ok(()) => {
                        if total_shots > 0 {
                            match lang {
                                Language::Ru => format!(
                                    "ZIP-архив ({} папок: скринлисты + {} скриншотов) создан за {:.2} сек: {}",
                                    ready_video_count,
                                    total_shots,
                                    started.elapsed().as_secs_f64(),
                                    zip_path.display()
                                ),
                                Language::En => format!(
                                    "ZIP archive ({} folders: screenlists + {} screenshots) created in {:.2} sec: {}",
                                    ready_video_count,
                                    total_shots,
                                    started.elapsed().as_secs_f64(),
                                    zip_path.display()
                                ),
                            }
                        } else {
                            match lang {
                                Language::Ru => format!(
                                    "ZIP-архив ({} папок) создан за {:.2} сек: {}",
                                    ready_video_count,
                                    started.elapsed().as_secs_f64(),
                                    zip_path.display()
                                ),
                                Language::En => format!(
                                    "ZIP archive ({} folders) created in {:.2} sec: {}",
                                    ready_video_count,
                                    started.elapsed().as_secs_f64(),
                                    zip_path.display()
                                ),
                            }
                        }
                    }
                    Err(error) => match lang {
                        Language::Ru => format!("Ошибка создания ZIP: {error}"),
                        Language::En => format!("ZIP creation error: {error}"),
                    },
                };

                let _ = tx.send(WorkerMsg::ExportComplete {
                    elapsed: started.elapsed(),
                    status_line: final_status,
                });
            });
        }
    }

    fn show_header_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) -> bool {
        let mut layout_changed = false;
        let prev_lang = self.params.lang;
        let busy = self.is_busy();

        ui.horizontal(|ui| {
            ui.heading(
                egui::RichText::new(self.tr(
                    "⚡ Генератор скринлиста PRO",
                    "⚡ Screenlist Generator PRO",
                ))
                    .size(20.0)
                    .strong()
                    .color(egui::Color32::from_rgb(130, 170, 255)),
            );

            ui.label(
                egui::RichText::new("Rust + Native C Libav Engine")
                    .size(12.0)
                    .color(egui::Color32::GRAY),
            );

            ui.separator();
            ui.label("🌐");
            ui.selectable_value(&mut self.params.lang, Language::Ru, "RU")
                .on_hover_text("Русский язык");
            ui.selectable_value(&mut self.params.lang, Language::En, "EN")
                .on_hover_text("English language");

            ui.separator();

            // Кнопка переключения в подпрограмму FastPic Uploader и обратно в верхней панели
            let fastpic_btn_text = if self.show_fastpic_uploader {
                self.tr("⬅ Вернуться в Скринлист PRO", "⬅ Back to Screenlist PRO")
            } else {
                "☁ FastPic Uploader"
            };
            let fastpic_btn_fill = if self.show_fastpic_uploader {
                egui::Color32::from_rgb(55, 115, 90)
            } else {
                egui::Color32::from_rgb(45, 85, 145)
            };

            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new(fastpic_btn_text)
                            .strong()
                            .color(egui::Color32::WHITE),
                    )
                        .fill(fastpic_btn_fill),
                )
                .on_hover_text(self.tr(
                    "Открыть / закрыть подпрограмму загрузки изображений на FastPic",
                    "Open / close FastPic image uploader subprogram",
                ))
                .clicked()
            {
                self.show_fastpic_uploader = !self.show_fastpic_uploader;
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let elapsed = if let Some(started) = self.batch_start_instant {
                    started.elapsed().as_secs_f64()
                } else if let Some(last) = self.last_elapsed {
                    last.as_secs_f64()
                } else {
                    0.0
                };

                let timer_text = match self.params.lang {
                    Language::Ru => format!("⏱ {:.3} сек", elapsed),
                    Language::En => format!("⏱ {:.3} sec", elapsed),
                };

                ui.label(
                    egui::RichText::new(timer_text)
                        .font(egui::FontId::monospace(14.0))
                        .strong()
                        .color(egui::Color32::from_rgb(80, 240, 160)),
                );
            });
        });

        if self.params.lang != prev_lang {
            self.fastpic.sync_language(self.params.lang);

            ctx.send_viewport_cmd(egui::ViewportCommand::Title(
                self.params
                    .lang
                    .tr("⚡ Генератор скринлиста PRO", "⚡ Screenlist Generator PRO")
                    .to_owned(),
            ));

            if self.videos.is_empty() && !busy {
                self.status_line = self
                    .params
                    .lang
                    .tr(
                        "Готов к работе. Добавьте видео, выберите папку или перетащите их в окно.",
                        "Ready. Add videos, select a folder, or drag & drop files into the window.",
                    )
                    .to_owned();
            } else {
                layout_changed = true;
            }
        }

        layout_changed
    }

    fn show_top_controls(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut layout_changed = self.show_header_bar(ui, ctx);
        let busy = self.is_busy();
        let lang = self.lang();

        ui.label(
            egui::RichText::new(self.topology.description(lang))
                .size(12.0)
                .color(egui::Color32::LIGHT_GRAY),
        );

        ui.separator();

        ui.horizontal_wrapped(|ui| {
            ui.label(lang.tr("Кадров:", "Frames:"));
            ui.add_enabled(
                !busy,
                egui::DragValue::new(&mut self.params.frame_count)
                    .clamp_range(1..=120)
                    .speed(1),
            );

            ui.separator();
            ui.label(lang.tr("Скриншотов:", "Screenshots:"));
            ui.add_enabled(
                !busy,
                egui::Slider::new(&mut self.params.screenshot_count, 0..=10),
            )
                .on_hover_text(lang.tr(
                    "Количество отдельных полноразмерных скриншотов (от 0 до 10)",
                    "Number of standalone full-resolution screenshots (0 to 10)",
                ));

            ui.separator();
            ui.label(lang.tr("Колонок:", "Columns:"));
            if ui
                .add_enabled(
                    !busy,
                    egui::Slider::new(&mut self.params.columns, 1..=10),
                )
                .changed()
            {
                layout_changed = true;
            }

            ui.separator();
            ui.label(lang.tr("Ширина:", "Width:"));
            if ui
                .add_enabled(
                    !busy,
                    egui::DragValue::new(&mut self.params.grid_width)
                        .clamp_range(600..=3840)
                        .speed(20),
                )
                .changed()
            {
                layout_changed = true;
            }

            ui.separator();
            ui.label(lang.tr("Режим ядер:", "Core mode:"));

            if self.topology.is_hybrid() {
                if !busy {
                    ui.selectable_value(
                        &mut self.params.core_mode,
                        CoreMode::Auto,
                        lang.tr("Авто P и E", "Auto P & E"),
                    )
                        .on_hover_text(
                            lang.tr("Сначала P-ядра, затем E-ядра.", "P-cores first, then E-cores."),
                        );

                    ui.selectable_value(
                        &mut self.params.core_mode,
                        CoreMode::POnly,
                        lang.tr("Только P", "P Only"),
                    );

                    ui.selectable_value(
                        &mut self.params.core_mode,
                        CoreMode::EOnly,
                        lang.tr("Только E", "E Only"),
                    );
                } else {
                    let mode_name = match self.params.core_mode {
                        CoreMode::Auto => lang.tr("Авто P и E", "Auto P & E"),
                        CoreMode::POnly => lang.tr("Только P", "P Only"),
                        CoreMode::EOnly => lang.tr("Только E", "E Only"),
                    };
                    ui.label(mode_name);
                }
            } else {
                ui.label(lang.tr("Авто", "Auto"));
            }

            let max_workers = self
                .topology
                .max_workers_for_mode(self.params.core_mode)
                .max(1);

            if !busy {
                self.params.workers = self.params.workers.clamp(1, max_workers);
            }

            ui.separator();
            ui.label(match lang {
                Language::Ru => format!("Воркеры: {}", self.params.workers),
                Language::En => format!("Workers: {}", self.params.workers),
            });
            ui.add_enabled(
                !busy,
                egui::Slider::new(&mut self.params.workers, 1..=max_workers),
            );

            if self.topology.is_hybrid() {
                let effective = self
                    .topology
                    .build_worker_slots(self.params.workers, self.params.core_mode)
                    .len();

                ui.label(match lang {
                    Language::Ru => format!("активно: {effective}"),
                    Language::En => format!("active: {effective}"),
                });
            }

            ui.separator();
            if ui
                .add_enabled(
                    !busy,
                    egui::Checkbox::new(
                        &mut self.params.show_timestamps,
                        lang.tr("Тайм-коды", "Timestamps"),
                    ),
                )
                .changed()
            {
                layout_changed = true;
            }

            ui.add_enabled(
                !busy,
                egui::Checkbox::new(
                    &mut self.params.turbo_keyframe,
                    lang.tr("Турбо (I-Frame)", "Turbo (I-Frame)"),
                ),
            )
                .on_hover_text(lang.tr(
                    "AVDISCARD_NONKEY: декодирует только ключевые кадры напрямую через C libavcodec.",
                    "AVDISCARD_NONKEY: decodes keyframes only directly via C libavcodec.",
                ));

            ui.add_enabled(!busy, egui::Checkbox::new(&mut self.params.hwaccel, "GPU"))
                .on_hover_text(lang.tr(
                    "Аппаратное декодирование через C libavutil hwcontext с откатом на CPU.",
                    "Hardware decoding via C libavutil hwcontext with CPU fallback.",
                ));

            ui.add_enabled(
                !busy,
                egui::Checkbox::new(
                    &mut self.params.move_video_to_folder,
                    lang.tr("Переносить видео в папку", "Move video to folder"),
                ),
            )
                .on_hover_text(lang.tr(
                    "При сохранении переносить исходный видеофайл в его отдельную папку вместе со скринлистом и скриншотами.",
                    "When saving, move the source video file into its dedicated folder alongside the screenlist and screenshots.",
                ));
        });

        if layout_changed {
            self.pending_layout = true;
        }
        if self.pending_layout && !busy {
            if ctx.input(|input| input.pointer.any_down()) {
                ctx.request_repaint_after(Duration::from_millis(16));
            } else {
                self.pending_layout = false;
                self.rerender_cached_sheets(ctx);
            }
        }

        ui.add_space(6.0);

        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new(lang.tr("➕ Добавить видео", "➕ Add Videos")),
                )
                .clicked()
            {
                if let Some(files) = rfd::FileDialog::new()
                    .add_filter(lang.tr("Видео файлы", "Video Files"), VIDEO_EXTENSIONS)
                    .pick_files()
                {
                    self.add_video_paths(files);
                }
            }

            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new(lang.tr("📁 Добавить папку", "📁 Add Folder")),
                )
                .on_hover_text(lang.tr(
                    "Найти все видеофайлы в выбранной папке и во всех её подпапках",
                    "Find all video files in the selected folder and all its subfolders",
                ))
                .clicked()
            {
                if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                    self.add_video_paths(vec![folder]);
                }
            }

            if ui
                .add_enabled(
                    !busy && !self.videos.is_empty(),
                    egui::Button::new(
                        egui::RichText::new(
                            lang.tr("🚀 Создать для всех видео", "🚀 Generate All"),
                        )
                            .strong()
                            .color(egui::Color32::WHITE),
                    )
                        .fill(egui::Color32::from_rgb(75, 95, 210)),
                )
                .clicked()
            {
                let indices: Vec<usize> = (0..self.videos.len()).collect();
                self.start_generation(indices);
            }

            if let Some(active) = self.active_idx {
                if ui
                    .add_enabled(
                        !busy && active < self.videos.len(),
                        egui::Button::new(lang.tr(
                            "🎬 Создать для выбранного видео",
                            "🎬 Generate Selected",
                        )),
                    )
                    .clicked()
                {
                    self.start_generation(vec![active]);
                }
            }

            let has_current_sheet = self
                .active_idx
                .and_then(|index| self.videos.get(index))
                .map(|video| video.sheet_image.is_some())
                .unwrap_or(false);

            let ready_sheets = self
                .videos
                .iter()
                .filter(|video| video.sheet_image.is_some())
                .count();

            if ui
                .add_enabled(
                    !busy && has_current_sheet,
                    egui::Button::new(lang.tr("💾 Сохранить текущий", "💾 Save Current")),
                )
                .clicked()
            {
                self.save_current_sheet();
            }

            let save_folder_label = match lang {
                Language::Ru => format!("📂 Сохранить в папку ({ready_sheets})"),
                Language::En => format!("📂 Save to Folder ({ready_sheets})"),
            };
            if ui
                .add_enabled(!busy && ready_sheets > 0, egui::Button::new(save_folder_label))
                .clicked()
            {
                self.save_all_to_folder();
            }

            let save_zip_label = match lang {
                Language::Ru => format!("📦 Создать ZIP ({ready_sheets})"),
                Language::En => format!("📦 Create ZIP ({ready_sheets})"),
            };
            if ui
                .add_enabled(!busy && ready_sheets > 0, egui::Button::new(save_zip_label))
                .clicked()
            {
                self.save_all_to_zip();
            }

            if ui
                .add_enabled(
                    !busy && !self.videos.is_empty(),
                    egui::Button::new(lang.tr("🗑 Очистить список", "🗑 Clear Queue")),
                )
                .clicked()
            {
                self.videos.clear();
                self.active_idx = None;
                self.batch_total_videos = 0;
                self.batch_done_videos = 0;
                self.batch_error_count = 0;
                self.last_batch_error = None;
                self.current_done_frames = 0;
                self.current_total_frames = 0;
                self.export_done_items = 0;
                self.export_total_items = 0;
                self.batch_start_instant = None;
                self.last_elapsed = None;
                self.status_line = lang.tr("Список очищен.", "Queue cleared.").to_owned();
            }
        });

        if self.is_exporting {
            ui.add_space(6.0);

            let total_items = self.export_total_items.max(1);
            let done_items = self.export_done_items.min(total_items);
            let export_ratio = (done_items as f32 / total_items as f32).clamp(0.0, 1.0);

            let export_bar_text = match lang {
                Language::Ru => format!(
                    "Сохранение / ZIP: {} / {} ({:.0}%) — {}",
                    done_items,
                    total_items,
                    export_ratio * 100.0,
                    self.status_line
                ),
                Language::En => format!(
                    "Saving / ZIP: {} / {} ({:.0}%) — {}",
                    done_items,
                    total_items,
                    export_ratio * 100.0,
                    self.status_line
                ),
            };

            ui.add(
                egui::ProgressBar::new(export_ratio)
                    .text(export_bar_text)
                    .animate(self.is_exporting),
            );
        } else if self.is_processing || self.batch_total_videos > 0 {
            ui.add_space(6.0);

            let total_videos = self.batch_total_videos.max(1);
            let done_videos = self.batch_done_videos.min(total_videos);

            let current_fraction = if self.current_total_frames > 0 {
                (self.current_done_frames as f32 / self.current_total_frames as f32).clamp(0.0, 1.0)
            } else {
                0.0
            };

            let overall_ratio = if self.is_processing {
                (self.batch_done_videos as f32 + current_fraction) / total_videos as f32
            } else {
                done_videos as f32 / total_videos as f32
            }
                .clamp(0.0, 1.0);

            let overall_text = match lang {
                Language::Ru => format!(
                    "Общий прогресс: {} / {} видео ({:.0}%)",
                    done_videos,
                    total_videos,
                    overall_ratio * 100.0
                ),
                Language::En => format!(
                    "Overall progress: {} / {} videos ({:.0}%)",
                    done_videos,
                    total_videos,
                    overall_ratio * 100.0
                ),
            };

            ui.add(
                egui::ProgressBar::new(overall_ratio)
                    .text(overall_text)
                    .animate(self.is_processing),
            );

            let current_ratio = if self.is_processing && self.current_total_frames > 0 {
                (self.current_done_frames as f32 / self.current_total_frames as f32).clamp(0.0, 1.0)
            } else if !self.is_processing && self.last_elapsed.is_some() {
                1.0
            } else {
                0.0
            };

            let frame_progress_text = match lang {
                Language::Ru => format!(
                    "{} (кадры: {} / {})",
                    self.status_line, self.current_done_frames, self.current_total_frames
                ),
                Language::En => format!(
                    "{} (frames: {} / {})",
                    self.status_line, self.current_done_frames, self.current_total_frames
                ),
            };

            ui.add(
                egui::ProgressBar::new(current_ratio)
                    .text(frame_progress_text)
                    .animate(self.is_processing),
            );
        } else {
            ui.label(&self.status_line);
        }
    }
}

impl eframe::App for ScreenlistApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_worker_messages(ctx);
        self.fastpic.poll_additions();
        self.fastpic.poll_uploads();

        if self.is_busy() || self.fastpic.is_busy() {
            ctx.request_repaint_after(Duration::from_millis(16));
        }

        let dropped_paths: Vec<PathBuf> = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .collect()
        });

        if !dropped_paths.is_empty() {
            if self.show_fastpic_uploader {
                self.fastpic.add_paths(dropped_paths);
            } else if !self.is_busy() {
                self.add_video_paths(dropped_paths);
            }
        }

        if self.show_fastpic_uploader {
            egui::TopBottomPanel::top("top_controls_panel").show(ctx, |ui| {
                ui.add_space(4.0);
                let _ = self.show_header_bar(ui, ctx);
                ui.separator();
                self.fastpic.draw_top_panel(ui);
                if self.fastpic.language != self.params.lang {
                    self.params.lang = self.fastpic.language;
                }
                ui.add_space(4.0);
            });

            egui::CentralPanel::default().show(ctx, |ui| {
                self.fastpic.draw_settings(ui);
                ui.add_space(12.0);
                self.fastpic.draw_files(ui);
                self.fastpic.draw_results(ui, &self.videos);
            });
        } else {
            // Верхняя фиксированная панель управления
            egui::TopBottomPanel::top("top_controls_panel").show(ctx, |ui| {
                ui.add_space(4.0);
                self.show_top_controls(ui, ctx);
                ui.add_space(4.0);
            });

            // Левая секция «Очередь видео» — ровно 25% ширины окна
            let window_width = ctx.screen_rect().width().max(400.0);
            let queue_panel_width = (window_width * 0.25).max(200.0);

            egui::SidePanel::left("video_queue_panel")
                .resizable(false)
                .exact_width(queue_panel_width)
                .show(ctx, |ui| {
                    self.show_video_queue(ui);
                });

            // Правая секция («Скринлист» / «Скриншоты») — оставшиеся 75% ширины окна
            egui::CentralPanel::default().show(ctx, |ui| {
                if let Some((video_idx, frame_idx)) = self.show_preview_panel(ui) {
                    self.shift_single_frame(video_idx, frame_idx);
                }
            });
        }
    }
}

// ============================================================================
// ПОИСК ВИДЕОФАЙЛОВ В ПАПКАХ И ПОДПАПКАХ + ОТПЕЧАТОК ФАЙЛА ДЛЯ ДЕДУПЛИКАЦИИ
// ============================================================================

fn is_video_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            let lower = ext.to_ascii_lowercase();
            VIDEO_EXTENSIONS.iter().any(|&valid| valid == lower)
        })
        .unwrap_or(false)
}

fn compute_video_fingerprint(path: &Path, file_size: u64) -> u64 {
    if file_size == 0 {
        return 0;
    }

    let Ok(mut file) = File::open(path) else {
        return 0;
    };

    const SAMPLE_SIZE: usize = 64 * 1024;
    let mut buf = vec![0u8; SAMPLE_SIZE];
    let mut hash: u64 = 0xcbf29ce484222325;

    for &b in &file_size.to_le_bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }

    let offsets = [
        0u64,
        file_size / 2,
        file_size.saturating_sub(SAMPLE_SIZE as u64),
    ];

    for offset in offsets {
        if file.seek(SeekFrom::Start(offset)).is_ok() {
            if let Ok(read_bytes) = file.read(&mut buf) {
                for &b in &buf[..read_bytes] {
                    hash ^= b as u64;
                    hash = hash.wrapping_mul(0x100000001b3);
                }
            }
        }
    }

    hash
}

fn collect_video_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
    fn walk(current_dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
        if depth > 64 {
            return;
        }

        let Ok(read_dir) = fs::read_dir(current_dir) else {
            return;
        };

        let mut entries: Vec<PathBuf> = read_dir.flatten().map(|entry| entry.path()).collect();

        entries.sort_by(|a, b| {
            a.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase()
                .cmp(
                    &b.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_lowercase(),
                )
        });

        for path in entries {
            if path.is_dir() {
                walk(&path, out, depth + 1);
            } else if is_video_file(&path) {
                out.push(path);
            }
        }
    }

    walk(dir, out, 0);
}

// ============================================================================
// ЧТЕНИЕ МЕТАДАННЫХ И ПАКЕТНОЕ ДЕКОДИРОВАНИЕ ЧЕРЕЗ НАТИВНЫЙ C-МОСТ LIBAV
// ============================================================================

fn probe_video(path: &Path, file_size: u64) -> Result<libav::ProbedMedia, String> {
    libav::probe_video(path, file_size)
}

fn extract_chunk_frames_rgb(
    path: &Path,
    chunk: &[(usize, f64)],
    target_w: u32,
    target_h: u32,
    turbo_keyframe: bool,
    hwaccel: bool,
    slot: WorkerSlot,
    done_counter: &AtomicUsize,
    total_frames: usize,
    video_idx: usize,
    tx: &Sender<WorkerMsg>,
) -> Result<Vec<(usize, CapturedFrame)>, String> {
    if chunk.is_empty() {
        return Ok(Vec::new());
    }

    let _affinity_guard = pin_current_thread(slot.affinity_mask);

    let mut decoder = if hwaccel {
        libav::LibavDecoder::open(path, true).or_else(|_| libav::LibavDecoder::open(path, false))?
    } else {
        libav::LibavDecoder::open(path, false)?
    };

    let mut result = Vec::with_capacity(chunk.len());

    for &(frame_idx, timestamp) in chunk {
        let image = match decoder.decode_frame_rgb(timestamp, target_w, target_h, turbo_keyframe) {
            Ok(img) => img,
            Err(_) => {
                let mut fallback_dec = libav::LibavDecoder::open(path, false)?;
                fallback_dec.decode_frame_rgb(timestamp, target_w, target_h, false)?
            }
        };

        let done_frames = done_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = tx.send(WorkerMsg::FrameProgress {
            video_idx,
            done_frames,
            total_frames,
        });

        result.push((
            frame_idx,
            CapturedFrame {
                timestamp,
                rgb: Arc::new(image),
            },
        ));
    }

    Ok(result)
}

fn extract_single_frame_rgb(
    path: &Path,
    timestamp: f64,
    target_w: u32,
    target_h: u32,
    turbo_keyframe: bool,
    hwaccel: bool,
    slot: WorkerSlot,
) -> Result<RgbImage, String> {
    let _affinity_guard = pin_current_thread(slot.affinity_mask);

    let mut attempts = vec![(turbo_keyframe, hwaccel)];
    if hwaccel {
        attempts.push((turbo_keyframe, false));
    }
    if turbo_keyframe {
        attempts.push((false, hwaccel));
    }
    if hwaccel && turbo_keyframe {
        attempts.push((false, false));
    }

    let mut errors = Vec::new();

    for (use_turbo, use_hwaccel) in attempts {
        match libav::LibavDecoder::open(path, use_hwaccel)
            .and_then(|mut dec| dec.decode_frame_rgb(timestamp, target_w, target_h, use_turbo))
        {
            Ok(image) => return Ok(image),
            Err(error) => errors.push(error),
        }
    }

    Err(errors.join(" | "))
}

// ============================================================================
// СБОРКА СКРИНЛИСТА
// ============================================================================

fn compose_contact_sheet(
    file_name: &str,
    file_size: u64,
    duration: f64,
    video_w: u32,
    video_h: u32,
    frames: &[CapturedFrame],
    params: RenderParams,
    font_bytes: &[u8],
) -> (RgbImage, u32, u32) {
    let cols = params.columns.max(1) as u32;
    let rows = ((frames.len() as u32 + cols - 1) / cols).max(1);

    let padding = 10u32;
    let gap = 2u32;
    let header_height = 46u32;

    let available_width = params
        .grid_width
        .saturating_sub(padding * 2 + gap * (cols - 1));

    let thumb_w = (available_width / cols).max(64);
    let aspect = video_h as f64 / video_w.max(1) as f64;
    let thumb_h = ((thumb_w as f64 * aspect).round() as u32).max(36);

    let total_w = params.grid_width;
    let total_h = header_height + padding * 2 + rows * thumb_h + (rows - 1) * gap;

    let mut canvas = RgbImage::from_pixel(total_w, total_h, Rgb([255, 255, 255]));
    let font = FontRef::try_from_slice(font_bytes).ok();

    if let Some(ref font) = font {
        draw_text_rgb(
            &mut canvas,
            font,
            10,
            6,
            16.5,
            file_name,
            [26, 26, 26],
            true,
        );

        let info = match params.lang {
            Language::Ru => format!(
                "{} | {}×{} | {} | {} кадров | Кадр: {}×{}px | Сетка: {}",
                format_duration(duration),
                video_w,
                video_h,
                format_file_size_lang(file_size, params.lang),
                frames.len(),
                thumb_w,
                thumb_h,
                format_columns_word_lang(cols as usize, params.lang)
            ),
            Language::En => format!(
                "{} | {}×{} | {} | {} frames | Frame: {}×{}px | Grid: {}",
                format_duration(duration),
                video_w,
                video_h,
                format_file_size_lang(file_size, params.lang),
                frames.len(),
                thumb_w,
                thumb_h,
                format_columns_word_lang(cols as usize, params.lang)
            ),
        };

        draw_text_rgb(
            &mut canvas,
            font,
            10,
            26,
            13.5,
            &info,
            [85, 85, 85],
            false,
        );
    }

    let row_bytes = (thumb_w * 3) as usize;
    let canvas_stride = (total_w * 3) as usize;
    let resized_frames: Vec<Option<RgbImage>> = frames
        .par_iter()
        .map(|frame| {
            if frame.rgb.width() == thumb_w && frame.rgb.height() == thumb_h {
                None
            } else {
                Some(image::imageops::resize(
                    frame.rgb.as_ref(),
                    thumb_w,
                    thumb_h,
                    image::imageops::FilterType::Triangle,
                ))
            }
        })
        .collect();

    for (index, frame) in frames.iter().enumerate() {
        let col = index as u32 % cols;
        let row = index as u32 / cols;

        let x0 = padding + col * (thumb_w + gap);
        let y0 = header_height + padding + row * (thumb_h + gap);

        let resized = resized_frames[index]
            .as_ref()
            .unwrap_or_else(|| frame.rgb.as_ref());
        let source = resized.as_raw();
        let destination = canvas.as_mut();

        for py in 0..thumb_h as usize {
            let source_offset = py * row_bytes;
            let destination_offset = (y0 as usize + py) * canvas_stride + x0 as usize * 3;

            destination[destination_offset..destination_offset + row_bytes]
                .copy_from_slice(&source[source_offset..source_offset + row_bytes]);
        }

        if params.show_timestamps {
            if let Some(ref font) = font {
                let timestamp = format_duration(frame.timestamp);
                let font_px = (thumb_w as f32 / 15.0).clamp(11.0, 15.0);
                let text_width = measure_text_width(font, font_px, &timestamp) as u32;
                let box_w = text_width + 8;
                let box_h = font_px as u32 + 4;

                let box_x = (x0 + thumb_w).saturating_sub(box_w + 4);
                let box_y = (y0 + thumb_h).saturating_sub(box_h + 4);

                blend_rect(&mut canvas, box_x, box_y, box_w, box_h, [0, 0, 0], 0.72);
                draw_text_rgb(
                    &mut canvas,
                    font,
                    (box_x + 4) as i32,
                    (box_y + 1) as i32,
                    font_px,
                    &timestamp,
                    [255, 255, 255],
                    false,
                );
            }
        }
    }

    (canvas, thumb_w, thumb_h)
}

fn blend_rect(
    image: &mut RgbImage,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    color: [u8; 3],
    alpha: f32,
) {
    let inverse = 1.0 - alpha;
    let max_x = (x + width).min(image.width());
    let max_y = (y + height).min(image.height());

    for py in y..max_y {
        for px in x..max_x {
            let pixel = image.get_pixel_mut(px, py);
            pixel[0] = (pixel[0] as f32 * inverse + color[0] as f32 * alpha) as u8;
            pixel[1] = (pixel[1] as f32 * inverse + color[1] as f32 * alpha) as u8;
            pixel[2] = (pixel[2] as f32 * inverse + color[2] as f32 * alpha) as u8;
        }
    }
}

fn measure_text_width(font: &FontRef, scale_px: f32, text: &str) -> f32 {
    let scaled = font.as_scaled(PxScale::from(scale_px));
    let mut width = 0.0f32;
    let mut previous = None;

    for character in text.chars() {
        let glyph = scaled.glyph_id(character);

        if let Some(previous_glyph) = previous {
            width += scaled.kern(previous_glyph, glyph);
        }

        width += scaled.h_advance(glyph);
        previous = Some(glyph);
    }

    width
}

fn draw_text_rgb(
    image: &mut RgbImage,
    font: &FontRef,
    x: i32,
    y: i32,
    scale_px: f32,
    text: &str,
    color: [u8; 3],
    faux_bold: bool,
) {
    let scale = PxScale::from(scale_px);
    let scaled = font.as_scaled(scale);
    let mut caret_x = x as f32;
    let baseline_y = y as f32 + scaled.ascent();
    let mut previous = None;

    for character in text.chars() {
        let glyph_id = scaled.glyph_id(character);

        if let Some(previous_glyph) = previous {
            caret_x += scaled.kern(previous_glyph, glyph_id);
        }

        let passes: &[f32] = if faux_bold { &[0.0, 0.6] } else { &[0.0] };

        for &offset_x in passes {
            let glyph = glyph_id.with_scale_and_position(
                scale,
                ab_glyph::point(caret_x + offset_x, baseline_y),
            );

            if let Some(outlined) = scaled.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();

                outlined.draw(|gx, gy, coverage| {
                    let px = bounds.min.x as i32 + gx as i32;
                    let py = bounds.min.y as i32 + gy as i32;

                    if px >= 0
                        && py >= 0
                        && (px as u32) < image.width()
                        && (py as u32) < image.height()
                    {
                        let pixel = image.get_pixel_mut(px as u32, py as u32);
                        let alpha = coverage.clamp(0.0, 1.0);
                        let inverse = 1.0 - alpha;

                        pixel[0] = (pixel[0] as f32 * inverse + color[0] as f32 * alpha) as u8;
                        pixel[1] = (pixel[1] as f32 * inverse + color[1] as f32 * alpha) as u8;
                        pixel[2] = (pixel[2] as f32 * inverse + color[2] as f32 * alpha) as u8;
                    }
                });
            }
        }

        caret_x += scaled.h_advance(glyph_id);
        previous = Some(glyph_id);
    }
}

// ============================================================================
// ЭКСПОРТ И УТИЛИТЫ
// ============================================================================

fn is_same_file_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

fn move_video_file(src: &Path, dst: &Path, lang: Language) -> Result<(), String> {
    if is_same_file_path(src, dst) {
        return Ok(());
    }

    if !src.exists() {
        return Err(match lang {
            Language::Ru => format!("Исходный файл не найден: {}", src.display()),
            Language::En => format!("Source file not found: {}", src.display()),
        });
    }

    if fs::rename(src, dst).is_ok() {
        return Ok(());
    }

    fs::copy(src, dst).map_err(|error| match lang {
        Language::Ru => format!("Не удалось скопировать видео: {error}"),
        Language::En => format!("Failed to copy video: {error}"),
    })?;
    fs::remove_file(src).map_err(|error| match lang {
        Language::Ru => {
            format!("Видео скопировано, но не удалось удалить исходный файл: {error}")
        }
        Language::En => format!("Video copied, but failed to remove source file: {error}"),
    })?;

    Ok(())
}

fn rgb_to_texture(ctx: &egui::Context, name: &str, image: &RgbImage) -> egui::TextureHandle {
    let size = [image.width() as usize, image.height() as usize];
    let color_image = egui::ColorImage::from_rgb(size, image.as_raw());
    ctx.load_texture(name, color_image, egui::TextureOptions::LINEAR)
}

fn save_rgb_image(image: &RgbImage, path: &Path, jpeg_quality: u8) -> Result<(), String> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("jpg")
        .to_lowercase();

    if extension == "png" {
        image.save(path).map_err(|error| error.to_string())
    } else {
        let file = File::create(path).map_err(|error| error.to_string())?;
        let mut writer = BufWriter::new(file);
        let mut encoder = JpegEncoder::new_with_quality(&mut writer, jpeg_quality);

        encoder
            .encode(
                image.as_raw(),
                image.width(),
                image.height(),
                image::ExtendedColorType::Rgb8,
            )
            .map_err(|error| error.to_string())
    }
}

fn encode_jpeg(image: &RgbImage, jpeg_quality: u8) -> Result<Vec<u8>, String> {
    let mut buffer = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut buffer, jpeg_quality);
    encoder
        .encode(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|error| error.to_string())?;
    Ok(buffer)
}

fn load_system_font_bytes() -> Vec<u8> {
    let candidates = [
        r"C:\Windows\Fonts\arial.ttf",
        r"C:\Windows\Fonts\segoeui.ttf",
        r"C:\Windows\Fonts\tahoma.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/truetype/ubuntu/Ubuntu-R.ttf",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/System/Library/Fonts/Supplemental/Verdana.ttf",
        "/Library/Fonts/Arial.ttf",
    ];

    for path in candidates {
        if let Ok(bytes) = fs::read(path) {
            return bytes;
        }
    }

    Vec::new()
}

fn stem_name(name: &str) -> String {
    Path::new(name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("video")
        .to_owned()
}

fn sanitize_folder_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        "video".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn format_duration(seconds: f64) -> String {
    let total = seconds.round().max(0.0) as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;

    if hours > 0 {
        format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
    } else {
        format!("{:02}:{:02}", minutes, seconds)
    }
}

fn format_file_size_lang(bytes: u64, lang: Language) -> String {
    let megabytes = bytes as f64 / (1024.0 * 1024.0);

    if megabytes >= 1024.0 {
        match lang {
            Language::Ru => format!("{:.2} ГБ", megabytes / 1024.0),
            Language::En => format!("{:.2} GB", megabytes / 1024.0),
        }
    } else {
        match lang {
            Language::Ru => format!("{:.1} МБ", megabytes),
            Language::En => format!("{:.1} MB", megabytes),
        }
    }
}

fn format_columns_word_lang(columns: usize, lang: Language) -> String {
    match lang {
        Language::Ru => {
            let mod10 = columns % 10;
            let mod100 = columns % 100;

            let word = if mod10 == 1 && mod100 != 11 {
                "колонка"
            } else if (2..=4).contains(&mod10) && !(12..=14).contains(&mod100) {
                "колонки"
            } else {
                "колонок"
            };

            format!("{} {}", columns, word)
        }
        Language::En => {
            if columns == 1 {
                "1 column".to_owned()
            } else {
                format!("{columns} columns")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_image_path, summarize_server_response, ImageKind, UploadResponse};
    use std::path::Path;

    #[test]
    fn formats_fastpic_bbcode() {
        let response = UploadResponse {
            big_link: None,
            thumb_link: Some(
                "https://i128.fastpic.org/thumb/2026/0731/a6/example.jpeg".to_owned(),
            ),
            view_link: Some("https://fastpic.org/view/128/2026/0731/example.jpg.html".to_owned()),
            short_link: None,
            delete_link: None,
            width: None,
            height: None,
            size: None,
            hotlinked: None,
        };

        assert_eq!(
            response.bbcode(),
            "[URL=https://fastpic.org/view/128/2026/0731/example.jpg.html][IMG]https://i128.fastpic.org/thumb/2026/0731/a6/example.jpeg[/IMG][/URL]"
        );
    }

    #[test]
    fn summarizes_html_error_response() {
        assert_eq!(
            summarize_server_response("<html><head><title>Upload denied</title></head></html>"),
            "Upload denied"
        );
    }

    #[test]
    fn groups_video_image_names_by_suffix() {
        assert_eq!(
            classify_image_path(Path::new("Rude_Awakening_2_screenlist.jpg")),
            ("Rude_Awakening_2".to_owned(), ImageKind::Screenlist)
        );
        assert_eq!(
            classify_image_path(Path::new("Rude_Awakening_2_ScreenShot1.jpg")),
            ("Rude_Awakening_2".to_owned(), ImageKind::Screenshot)
        );
        assert_eq!(
            classify_image_path(Path::new("DDSC-050_ScreenShot4.jpg")),
            ("DDSC-050".to_owned(), ImageKind::Screenshot)
        );
    }
}