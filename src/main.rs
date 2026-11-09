#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use eframe::egui;
use image::{codecs::jpeg::JpegEncoder, Rgb, RgbImage};
use rayon::prelude::*;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc::{self, Receiver, Sender},
    Arc,
};
use std::time::{Duration, Instant};

const MAX_WORKERS: usize = 32;
const MAX_FRAMES_PER_DECODER_TASK: usize = 3;
const ZIP_ENCODE_BATCH_SIZE: usize = 4;

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

struct VideoItem {
    path: PathBuf,
    file_name: String,
    file_size: u64,
    duration: f64,
    width: u32,
    height: u32,
    video_info_line: String,
    audio_info_line: String,
    status: VideoStatus,
    frames: Vec<CapturedFrame>,
    sheet_image: Option<RgbImage>,
    sheet_texture: Option<egui::TextureHandle>,
    last_cols: usize,
    last_thumb_w: u32,
    last_thumb_h: u32,
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
    columns: usize,
    grid_width: u32,
    workers: usize,
    core_mode: CoreMode,
    show_timestamps: bool,
    turbo_keyframe: bool,
    hwaccel: bool,
    jpeg_quality: u8,
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

    fn description(&self) -> String {
        if !self.detected {
            return format!(
                "Топология P/E не определена; доступно логических потоков: {}",
                self.logical_processors
            );
        }

        if self.is_hybrid() {
            format!(
                "Обнаружено {} P + {} E ядер ({} логических потоков)",
                self.p_core_masks.len(),
                self.e_core_masks.len(),
                self.logical_processors
            )
        } else {
            format!(
                "Обнаружено {} физических ядер ({} логических потоков)",
                self.physical_core_count(),
                self.logical_processors
            )
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
            .map_err(|_| "Путь к файлу содержит недопустимый нулевой байт.".to_owned())
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

        let audio_info_line = if info.has_audio != 0 {
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

            format!(
                "Аудио: Кодек: {}, Частота: {}, Каналы: {}, Битрейт: {}",
                a_codec_str, sample_rate_str, channels_str, a_bitrate_str
            )
        } else {
            "Аудио: Отсутствует".to_owned()
        };

        Ok(ProbedMedia {
            duration: info.duration,
            width: info.width,
            height: info.height,
            video_info_line,
            audio_info_line,
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
                .ok_or_else(|| "Переполнение буфера кадра.".to_owned())?;

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
                .ok_or_else(|| "Не удалось создать RgbImage.".to_owned())
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
}

struct ScreenlistApp {
    videos: Vec<VideoItem>,
    active_idx: Option<usize>,
    params: RenderParams,
    topology: CpuTopology,

    is_processing: bool,
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

        Self {
            videos: Vec::new(),
            active_idx: None,
            params: RenderParams {
                frame_count: 32,
                columns: 4,
                grid_width: 1200,
                workers,
                core_mode: CoreMode::Auto,
                show_timestamps: true,
                turbo_keyframe: true,
                hwaccel: false,
                jpeg_quality: 85,
            },
            topology,
            is_processing: false,
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

    fn add_video_paths(&mut self, input_paths: Vec<PathBuf>) {
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
                self.status_line =
                    "В выбранной папке (и её подпапках) видеофайлы не найдены.".to_owned();
            }
            return;
        }

        let mut added_count = 0usize;
        let mut last_error = None;

        for path in discovered_files {
            if self.videos.iter().any(|video| video.path == path) {
                continue;
            }

            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("video.mp4")
                .to_owned();
            let file_size = fs::metadata(&path).map(|metadata| metadata.len()).unwrap_or(0);

            match probe_video(&path, file_size) {
                Ok(probed) => {
                    self.videos.push(VideoItem {
                        path,
                        file_name,
                        file_size,
                        duration: probed.duration,
                        width: probed.width,
                        height: probed.height,
                        video_info_line: probed.video_info_line,
                        audio_info_line: probed.audio_info_line,
                        status: VideoStatus::Ready,
                        frames: Vec::new(),
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
                    last_error = Some(format!("Ошибка чтения {}: {}", file_name, error));
                }
            }
        }

        if added_count > 0 {
            self.status_line = format!(
                "Добавлено видео: {} шт. (всего в очереди: {})",
                added_count,
                self.videos.len()
            );
        } else if let Some(error) = last_error {
            self.status_line = error;
        }
    }

    fn start_generation(&mut self, indices: Vec<usize>) {
        if self.is_processing || indices.is_empty() {
            return;
        }

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
            self.status_line = "Нет видео для обработки.".to_owned();
            return;
        }

        self.is_processing = true;
        self.batch_total_videos = tasks.len();
        self.batch_done_videos = 0;
        self.batch_error_count = 0;
        self.last_batch_error = None;
        self.current_done_frames = 0;
        self.current_total_frames = self.params.frame_count.max(1);
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
                            error: format!("Не удалось создать пул потоков: {error}"),
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
                                    total_frames,
                                    video_idx,
                                    &tx,
                                )
                                    .map(|frames| (slot.is_p_core, frames))
                            })
                            .collect()
                    });

                match extracted {
                    Err(error) => {
                        let _ = tx.send(WorkerMsg::VideoError { video_idx, error });
                    }
                    Ok(parts) => {
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
                                error: format!(
                                    "Получено {} кадров вместо {}.",
                                    indexed_frames.len(),
                                    total_frames
                                ),
                            });
                            continue;
                        }

                        indexed_frames.sort_by_key(|(frame_idx, _)| *frame_idx);
                        let frames: Vec<CapturedFrame> =
                            indexed_frames.into_iter().map(|(_, frame)| frame).collect();

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
                            sheet,
                            elapsed: video_t0.elapsed(),
                            cols: params.columns,
                            thumb_w: sheet_thumb_w,
                            thumb_h: sheet_thumb_h,
                        });
                    }
                }
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
                video.sheet_image = Some(sheet);
                video.last_cols = params.columns;
                video.last_thumb_w = thumb_w;
                video.last_thumb_h = thumb_h;
            }
        }

        if count > 0 {
            let elapsed = started.elapsed();
            self.last_elapsed = Some(elapsed);
            self.status_line = format!(
                "Сетка перестроена из RAM-кэша ({} видео) за {:.1} мс",
                count,
                elapsed.as_secs_f64() * 1000.0
            );
        }
    }

    fn release_inactive_sheet_textures(&mut self) {
        for (index, video) in self.videos.iter_mut().enumerate() {
            if self.active_idx != Some(index) {
                video.sheet_texture = None;
            }
        }
    }

    fn shift_single_frame(&mut self, video_idx: usize, frame_idx: usize) {
        if self.is_processing {
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
        while let Ok(message) = self.rx.try_recv() {
            match message {
                WorkerMsg::VideoStarted { video_idx } => {
                    self.active_idx = Some(video_idx);
                    self.release_inactive_sheet_textures();
                    self.current_done_frames = 0;
                    self.current_total_frames = self.params.frame_count.max(1);

                    if let Some(video) = self.videos.get_mut(video_idx) {
                        video.status = VideoStatus::Processing;
                        self.status_line =
                            format!("Декодирование (C libav): {} ...", video.file_name);
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
                        self.status_line = format!(
                            "Декодирование (C libav): {} ({}/{})",
                            video.file_name, self.current_done_frames, total_frames
                        );
                    }
                }
                WorkerMsg::VideoFinished {
                    video_idx,
                    frames,
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
                        video.sheet_image = Some(sheet);
                        video.frames = frames;
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
                        video.sheet_image = Some(sheet);
                        video.status = VideoStatus::Done(elapsed);
                        video.last_cols = cols;
                        video.last_thumb_w = thumb_w;
                        video.last_thumb_h = thumb_h;

                        self.status_line = format!(
                            "Кадр #{} сдвинут (+1.0с) за {:.0} мс",
                            frame_idx + 1,
                            elapsed.as_secs_f64() * 1000.0
                        );
                    }
                }
                WorkerMsg::VideoError { video_idx, error } => {
                    self.batch_done_videos += 1;
                    self.batch_error_count += 1;
                    self.last_batch_error = Some(error.clone());

                    if let Some(video) = self.videos.get_mut(video_idx) {
                        video.status = VideoStatus::Error(error.clone());
                    }

                    self.status_line = format!("Ошибка: {error}");
                }
                WorkerMsg::BatchComplete {
                    total_elapsed,
                    total_p_frames,
                    total_e_frames,
                } => {
                    self.is_processing = false;
                    self.last_elapsed = Some(total_elapsed);
                    self.batch_start_instant = None;

                    let mut summary = format!(
                        "Готово за {:.3} сек ({:.0} мс)",
                        total_elapsed.as_secs_f64(),
                        total_elapsed.as_secs_f64() * 1000.0
                    );

                    if self.topology.is_hybrid() {
                        summary.push_str(&format!(
                            " | P-ядра: {} кадров | E-ядра: {} кадров",
                            total_p_frames, total_e_frames
                        ));
                    }

                    self.status_line = if self.batch_error_count == 0 {
                        format!("✅ {summary}")
                    } else {
                        format!(
                            "⚠️ {summary} | Ошибок: {} | Последняя: {}",
                            self.batch_error_count,
                            self.last_batch_error
                                .as_deref()
                                .unwrap_or("неизвестная ошибка")
                        )
                    };
                }
            }
        }
    }

    fn show_video_queue(&mut self, ui: &mut egui::Ui) {
        ui.heading(format!("Очередь видео ({})", self.videos.len()));
        ui.separator();

        let processing = self.is_processing;
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
                            let btn_space = if processing { 0.0 } else { 28.0 };
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

                            if !processing
                                && ui
                                .small_button("✖")
                                .on_hover_text("Удалить из очереди")
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
                                format_file_size(video.file_size)
                            ))
                                .size(11.5)
                                .color(egui::Color32::GRAY),
                        );

                        // Сначала строка с информацией о видео (копируется при клике)
                        let video_resp = ui.add(
                            egui::Label::new(
                                egui::RichText::new(&video.video_info_line)
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
                            copied_line = Some(video.video_info_line.clone());
                        }
                        video_resp.on_hover_text("Нажмите, чтобы скопировать в буфер обмена");

                        // Затем строка с информацией об аудио (копируется при клике)
                        let audio_resp = ui.add(
                            egui::Label::new(
                                egui::RichText::new(&video.audio_info_line)
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
                            copied_line = Some(video.audio_info_line.clone());
                        }
                        audio_resp.on_hover_text("Нажмите, чтобы скопировать в буфер обмена");

                        match &video.status {
                            VideoStatus::Ready => {
                                ui.label("⏳ Ожидает");
                            }
                            VideoStatus::Processing => {
                                ui.label(
                                    egui::RichText::new("⚙ Обработка...")
                                        .color(egui::Color32::from_rgb(255, 200, 80)),
                                );
                            }
                            VideoStatus::Done(elapsed) => {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "✅ Готово за {:.3} сек",
                                        elapsed.as_secs_f64()
                                    ))
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
            self.status_line = format!("📋 Скопировано в буфер обмена: {}", text);
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

    fn show_active_sheet(&mut self, ui: &mut egui::Ui) -> Option<(usize, usize)> {
        let Some(active_idx) = self.active_idx else {
            ui.centered_and_justified(|ui| {
                ui.label("Добавьте видео или папку, чтобы создать скринлист.");
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
                ui.label("Видео не выбрано.");
            });
            return None;
        };

        let Some(texture) = &video.sheet_texture else {
            ui.centered_and_justified(|ui| {
                ui.label("Нажмите «Создать скринлист», чтобы начать обработку.");
            });
            return None;
        };

        ui.label(
            egui::RichText::new(
                "Подсказка: кликните по кадру на листе, чтобы сдвинуть его на +1 секунду.",
            )
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

    fn save_current_sheet(&mut self) {
        let Some(active_idx) = self.active_idx else {
            return;
        };
        let Some(video) = self.videos.get(active_idx) else {
            return;
        };
        if video.sheet_image.is_none() {
            return;
        }

        let default_name = format!("{}_screenlist.jpg", stem_name(&video.file_name));
        let jpeg_quality = self.params.jpeg_quality;

        if let Some(path) = rfd::FileDialog::new()
            .set_file_name(&default_name)
            .add_filter("JPEG Image", &["jpg", "jpeg"])
            .add_filter("PNG Image", &["png"])
            .save_file()
        {
            let result = self
                .videos
                .get(active_idx)
                .and_then(|video| video.sheet_image.as_ref())
                .ok_or_else(|| "Скринлист больше недоступен.".to_owned())
                .and_then(|image| save_rgb_image(image, &path, jpeg_quality));

            if let Err(error) = result {
                self.status_line = format!("Ошибка сохранения: {error}");
            } else {
                self.status_line = format!("Сохранено: {}", path.display());
            }
        }
    }

    fn save_all_to_folder(&mut self) {
        let ready_count = self
            .videos
            .iter()
            .filter(|video| video.sheet_image.is_some())
            .count();

        if ready_count == 0 {
            return;
        }

        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
            let mut saved = 0usize;

            for (idx, video) in self.videos.iter().enumerate() {
                if let Some(image) = &video.sheet_image {
                    let base_stem = stem_name(&video.file_name);
                    let mut output = folder.join(format!("{base_stem}_screenlist.jpg"));
                    if output.exists() {
                        output =
                            folder.join(format!("{:02}_{base_stem}_screenlist.jpg", idx + 1));
                    }

                    if save_rgb_image(image, &output, self.params.jpeg_quality).is_ok() {
                        saved += 1;
                    }
                }
            }

            self.status_line =
                format!("Сохранено {} скринлистов в {}", saved, folder.display());
        }
    }

    fn save_all_to_zip(&mut self) {
        let ready: Vec<(&str, &RgbImage)> = self
            .videos
            .iter()
            .filter_map(|video| {
                video
                    .sheet_image
                    .as_ref()
                    .map(|image| (video.file_name.as_str(), image))
            })
            .collect();

        if ready.is_empty() {
            return;
        }

        let default_name = format!("screenlists_{}_videos.zip", ready.len());

        if let Some(zip_path) = rfd::FileDialog::new()
            .set_file_name(&default_name)
            .add_filter("ZIP Archive", &["zip"])
            .save_file()
        {
            let started = Instant::now();

            match export_sheets_to_zip(&ready, &zip_path, self.params.jpeg_quality) {
                Ok(()) => {
                    self.status_line = format!(
                        "ZIP-архив ({} шт.) создан за {:.2} сек: {}",
                        ready.len(),
                        started.elapsed().as_secs_f64(),
                        zip_path.display()
                    );
                }
                Err(error) => {
                    self.status_line = format!("Ошибка создания ZIP: {error}");
                }
            }
        }
    }

    fn show_top_controls(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.heading(
                egui::RichText::new("⚡ Генератор скринлиста PRO")
                    .size(20.0)
                    .strong()
                    .color(egui::Color32::from_rgb(130, 170, 255)),
            );

            ui.label(
                egui::RichText::new("Rust + Native C Libav Engine")
                    .size(12.0)
                    .color(egui::Color32::GRAY),
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let elapsed = if let Some(started) = self.batch_start_instant {
                    started.elapsed().as_secs_f64()
                } else if let Some(last) = self.last_elapsed {
                    last.as_secs_f64()
                } else {
                    0.0
                };

                ui.label(
                    egui::RichText::new(format!("⏱ {:.3} сек", elapsed))
                        .font(egui::FontId::monospace(14.0))
                        .strong()
                        .color(egui::Color32::from_rgb(80, 240, 160)),
                );
            });
        });

        ui.label(
            egui::RichText::new(self.topology.description())
                .size(12.0)
                .color(egui::Color32::LIGHT_GRAY),
        );

        ui.separator();

        let mut layout_changed = false;

        ui.horizontal_wrapped(|ui| {
            ui.label("Кадров:");
            ui.add_enabled(
                !self.is_processing,
                egui::DragValue::new(&mut self.params.frame_count)
                    .clamp_range(1..=120)
                    .speed(1),
            );

            ui.separator();
            ui.label("Колонок:");
            if ui
                .add_enabled(
                    !self.is_processing,
                    egui::Slider::new(&mut self.params.columns, 1..=10),
                )
                .changed()
            {
                layout_changed = true;
            }

            ui.separator();
            ui.label("Ширина:");
            if ui
                .add_enabled(
                    !self.is_processing,
                    egui::DragValue::new(&mut self.params.grid_width)
                        .clamp_range(600..=3840)
                        .speed(20),
                )
                .changed()
            {
                layout_changed = true;
            }

            ui.separator();
            ui.label("Режим ядер:");

            if self.topology.is_hybrid() {
                if !self.is_processing {
                    ui.selectable_value(
                        &mut self.params.core_mode,
                        CoreMode::Auto,
                        "Авто P и E",
                    )
                        .on_hover_text("Сначала P-ядра, затем E-ядра.");

                    ui.selectable_value(
                        &mut self.params.core_mode,
                        CoreMode::POnly,
                        "Только P",
                    );

                    ui.selectable_value(
                        &mut self.params.core_mode,
                        CoreMode::EOnly,
                        "Только E",
                    );
                } else {
                    let mode_name = match self.params.core_mode {
                        CoreMode::Auto => "Авто P и E",
                        CoreMode::POnly => "Только P",
                        CoreMode::EOnly => "Только E",
                    };
                    ui.label(mode_name);
                }
            } else {
                ui.label("Авто");
            }

            let max_workers = self
                .topology
                .max_workers_for_mode(self.params.core_mode)
                .max(1);

            if !self.is_processing {
                self.params.workers = self.params.workers.clamp(1, max_workers);
            }

            ui.separator();
            ui.label(format!("Воркеры: {}", self.params.workers));
            ui.add_enabled(
                !self.is_processing,
                egui::Slider::new(&mut self.params.workers, 1..=max_workers),
            );

            if self.topology.is_hybrid() {
                let effective = self
                    .topology
                    .build_worker_slots(self.params.workers, self.params.core_mode)
                    .len();

                ui.label(format!("активно: {effective}"));
            }

            ui.separator();
            if ui
                .checkbox(&mut self.params.show_timestamps, "Тайм-коды")
                .changed()
            {
                layout_changed = true;
            }

            ui.checkbox(&mut self.params.turbo_keyframe, "Турбо (I-Frame)")
                .on_hover_text(
                    "AVDISCARD_NONKEY: декодирует только ключевые кадры напрямую через C libavcodec.",
                );

            ui.checkbox(&mut self.params.hwaccel, "GPU")
                .on_hover_text(
                    "Аппаратное декодирование через C libavutil hwcontext с откатом на CPU.",
                );
        });

        if layout_changed {
            self.pending_layout = true;
        }
        if self.pending_layout && !self.is_processing {
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
                .add_enabled(!self.is_processing, egui::Button::new("➕ Добавить видео"))
                .clicked()
            {
                if let Some(files) = rfd::FileDialog::new()
                    .add_filter("Видео файлы", VIDEO_EXTENSIONS)
                    .pick_files()
                {
                    self.add_video_paths(files);
                }
            }

            if ui
                .add_enabled(!self.is_processing, egui::Button::new("📁 Добавить папку"))
                .on_hover_text("Найти все видеофайлы в выбранной папке и во всех её подпапках")
                .clicked()
            {
                if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                    self.add_video_paths(vec![folder]);
                }
            }

            if ui
                .add_enabled(
                    !self.is_processing && !self.videos.is_empty(),
                    egui::Button::new(
                        egui::RichText::new("🚀 Создать ВСЕ скринлисты")
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
                        !self.is_processing && active < self.videos.len(),
                        egui::Button::new("🎬 Создать для выбранного"),
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
                .add_enabled(has_current_sheet, egui::Button::new("💾 Скачать текущий"))
                .clicked()
            {
                self.save_current_sheet();
            }

            if ui
                .add_enabled(
                    ready_sheets > 0,
                    egui::Button::new(format!("📂 Сохранить в папку ({ready_sheets})")),
                )
                .clicked()
            {
                self.save_all_to_folder();
            }

            if ui
                .add_enabled(
                    ready_sheets > 0,
                    egui::Button::new(format!("📦 Скачать ZIP ({ready_sheets})")),
                )
                .clicked()
            {
                self.save_all_to_zip();
            }

            if ui
                .add_enabled(
                    !self.is_processing && !self.videos.is_empty(),
                    egui::Button::new("🗑 Очистить список"),
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
                self.batch_start_instant = None;
                self.last_elapsed = None;
                self.status_line = "Список очищен.".to_owned();
            }
        });

        if self.is_processing || self.batch_total_videos > 0 {
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

            ui.add(
                egui::ProgressBar::new(overall_ratio)
                    .text(format!(
                        "Общий прогресс: {} / {} видео ({:.0}%)",
                        done_videos,
                        total_videos,
                        overall_ratio * 100.0
                    ))
                    .animate(self.is_processing),
            );

            let current_ratio = if self.is_processing && self.current_total_frames > 0 {
                (self.current_done_frames as f32 / self.current_total_frames as f32).clamp(0.0, 1.0)
            } else if !self.is_processing && self.last_elapsed.is_some() {
                1.0
            } else {
                0.0
            };

            ui.add(
                egui::ProgressBar::new(current_ratio)
                    .text(format!(
                        "{} (кадры: {} / {})",
                        self.status_line, self.current_done_frames, self.current_total_frames
                    ))
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

        if self.is_processing {
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
            self.add_video_paths(dropped_paths);
        }

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

        // Правая секция «Скринлист» — оставшиеся 75% ширины окна
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some((video_idx, frame_idx)) = self.show_active_sheet(ui) {
                self.shift_single_frame(video_idx, frame_idx);
            }
        });
    }
}

// ============================================================================
// ПОИСК ВИДЕОФАЙЛОВ В ПАПКАХ И ПОДПАПКАХ
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

        let info = format!(
            "{} | {}×{} | {} | {} кадров | Кадр: {}×{}px | Сетка: {}",
            format_duration(duration),
            video_w,
            video_h,
            format_file_size(file_size),
            frames.len(),
            thumb_w,
            thumb_h,
            format_columns_word(cols as usize)
        );

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

fn export_sheets_to_zip(
    sheets: &[(&str, &RgbImage)],
    zip_path: &Path,
    jpeg_quality: u8,
) -> Result<(), String> {
    let file = File::create(zip_path).map_err(|error| error.to_string())?;
    let mut zip = zip::ZipWriter::new(file);

    for (batch_index, batch) in sheets.chunks(ZIP_ENCODE_BATCH_SIZE).enumerate() {
        let encoded: Vec<Result<Vec<u8>, String>> = batch
            .par_iter()
            .map(|(_, image)| encode_jpeg(image, jpeg_quality))
            .collect();

        for (offset, ((video_name, _), jpeg_buffer)) in batch.iter().zip(encoded).enumerate() {
            let index = batch_index * ZIP_ENCODE_BATCH_SIZE + offset;
            let entry_name =
                format!("{:02}_{}_screenlist.jpg", index + 1, stem_name(video_name));

            let options = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);

            zip.start_file(entry_name, options)
                .map_err(|error| error.to_string())?;
            zip.write_all(&jpeg_buffer?)
                .map_err(|error| error.to_string())?;
        }
    }

    zip.finish().map_err(|error| error.to_string())?;
    Ok(())
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

fn format_file_size(bytes: u64) -> String {
    let megabytes = bytes as f64 / (1024.0 * 1024.0);

    if megabytes >= 1024.0 {
        format!("{:.2} ГБ", megabytes / 1024.0)
    } else {
        format!("{:.1} МБ", megabytes)
    }
}

fn format_columns_word(columns: usize) -> String {
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