// Модуль обработки видео

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use eframe::egui;
use image::{codecs::jpeg::JpegEncoder, Rgb, RgbImage};
use rayon::prelude::*;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use crate::i18n::Language;
use crate::utils::{format_duration, format_file_size_lang, is_video_file};
use crate::video::{ffmpeg, CapturedFrame, RenderParams, WorkerSlot};
use crate::video::cpu::pin_current_thread;

pub enum WorkerMsg {
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

pub fn probe_video(path: &Path, file_size: u64) -> Result<ffmpeg::ProbedMedia, String> {
    ffmpeg::probe_video(path, file_size)
}

pub fn extract_chunk_frames_rgb(
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
        ffmpeg::LibavDecoder::open(path, true)
            .or_else(|_| ffmpeg::LibavDecoder::open(path, false))?
    } else {
        ffmpeg::LibavDecoder::open(path, false)?
    };

    let mut result = Vec::with_capacity(chunk.len());

    for &(frame_idx, timestamp) in chunk {
        let image = match decoder.decode_frame_rgb(timestamp, target_w, target_h, turbo_keyframe) {
            Ok(img) => img,
            Err(_) => {
                let mut fallback_dec = ffmpeg::LibavDecoder::open(path, false)?;
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

pub fn extract_single_frame_rgb(
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
        match ffmpeg::LibavDecoder::open(path, use_hwaccel)
            .and_then(|mut dec| dec.decode_frame_rgb(timestamp, target_w, target_h, use_turbo))
        {
            Ok(image) => return Ok(image),
            Err(error) => errors.push(error),
        }
    }

    Err(errors.join(" | "))
}

pub fn compose_contact_sheet(
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

pub fn is_same_file_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

pub fn move_video_file(src: &Path, dst: &Path, lang: Language) -> Result<(), String> {
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

pub fn rgb_to_texture(ctx: &egui::Context, name: &str, image: &RgbImage) -> egui::TextureHandle {
    let size = [image.width() as usize, image.height() as usize];
    let color_image = egui::ColorImage::from_rgb(size, image.as_raw());
    ctx.load_texture(name, color_image, egui::TextureOptions::LINEAR)
}

pub fn save_rgb_image(image: &RgbImage, path: &Path, jpeg_quality: u8) -> Result<(), String> {
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

pub fn encode_jpeg(image: &RgbImage, jpeg_quality: u8) -> Result<Vec<u8>, String> {
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

pub fn load_system_font_bytes() -> Vec<u8> {
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

pub fn stem_name(name: &str) -> String {
    Path::new(name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("video")
        .to_owned()
}

pub fn sanitize_folder_name(name: &str) -> String {
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

pub fn compute_video_fingerprint(path: &Path, file_size: u64) -> u64 {
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

pub fn collect_video_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
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
        Language::En => format!("{} columns", columns),
    }
}
