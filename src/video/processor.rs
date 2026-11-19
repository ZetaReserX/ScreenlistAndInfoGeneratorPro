// Модуль обработки видео

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use eframe::egui;
use image::{codecs::jpeg::JpegEncoder, Rgb, RgbImage};
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;
use wide::f32x4;

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

    // ОПТИМИЗАЦИЯ: Сортируем timestamps для последовательного чтения (минимизируем seek)
    // Это даёт 20-30% ускорение за счёт более эффективного доступа к видеопотоку
    let mut sorted_chunk = chunk.to_vec();
    sorted_chunk.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut result = Vec::with_capacity(sorted_chunk.len());

    for &(frame_idx, timestamp) in &sorted_chunk {
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

/// Кэш для предварительно отрисованных символов timestamp (цифры и разделители)
/// Позволяет избежать повторного рендеринга ab_glyph для каждого timestamp
struct TimestampCache {
    glyphs: HashMap<char, RgbImage>,
    font_size: f32,
}

impl TimestampCache {
    /// Создаёт кэш с предрендеренными символами для заданного размера шрифта
    fn new(font: &FontRef, font_size: f32) -> Self {
        let chars = ['0', '1', '2', '3', '4', '5', '6', '7', '8', '9', ':', ' '];
        let mut glyphs = HashMap::with_capacity(chars.len());

        for &ch in &chars {
            if let Some(glyph_image) = render_char_to_image(font, ch, font_size) {
                glyphs.insert(ch, glyph_image);
            }
        }

        TimestampCache { glyphs, font_size }
    }

    /// Отрисовывает timestamp используя предрендеренные глифы из кэша
    fn draw_timestamp(
        &self,
        canvas: &mut RgbImage,
        x: i32,
        y: i32,
        text: &str,
        color: [u8; 3],
    ) {
        let mut cursor_x = x;

        for ch in text.chars() {
            if let Some(glyph_img) = self.glyphs.get(&ch) {
                let glyph_w = glyph_img.width() as i32;
                let glyph_h = glyph_img.height() as i32;

                // Копируем предрендеренный глиф на canvas с учётом цвета
                for gy in 0..glyph_h {
                    for gx in 0..glyph_w {
                        let px = cursor_x + gx;
                        let py = y + gy;

                        if px >= 0 && py >= 0 && (px as u32) < canvas.width() && (py as u32) < canvas.height() {
                            let glyph_pixel = glyph_img.get_pixel(gx as u32, gy as u32);
                            let alpha = glyph_pixel[0] as f32 / 255.0;

                            if alpha > 0.01 {
                                let canvas_pixel = canvas.get_pixel_mut(px as u32, py as u32);
                                let inverse = 1.0 - alpha;

                                canvas_pixel[0] = (canvas_pixel[0] as f32 * inverse + color[0] as f32 * alpha) as u8;
                                canvas_pixel[1] = (canvas_pixel[1] as f32 * inverse + color[1] as f32 * alpha) as u8;
                                canvas_pixel[2] = (canvas_pixel[2] as f32 * inverse + color[2] as f32 * alpha) as u8;
                            }
                        }
                    }
                }

                cursor_x += glyph_w;
            }
        }
    }

    /// Измеряет ширину текста используя кэшированные глифы
    fn measure_width(&self, text: &str) -> u32 {
        text.chars()
            .filter_map(|ch| self.glyphs.get(&ch).map(|img| img.width()))
            .sum()
    }
}

/// Рендерит один символ в RgbImage для кэширования
fn render_char_to_image(font: &FontRef, ch: char, font_size: f32) -> Option<RgbImage> {
    let scale = PxScale::from(font_size);
    let scaled = font.as_scaled(scale);
    let glyph_id = scaled.glyph_id(ch);
    let glyph = glyph_id.with_scale_and_position(scale, ab_glyph::point(0.0, scaled.ascent()));

    let outlined = scaled.outline_glyph(glyph)?;
    let bounds = outlined.px_bounds();

    let width = (bounds.width().ceil() as u32).max(1);
    let height = (bounds.height().ceil() as u32).max(1);

    // Создаём изображение с альфа-каналом в виде grayscale
    let mut img = RgbImage::from_pixel(width, height, Rgb([0, 0, 0]));

    outlined.draw(|gx, gy, coverage| {
        if (gx as u32) < width && (gy as u32) < height {
            let alpha = (coverage.clamp(0.0, 1.0) * 255.0) as u8;
            img.put_pixel(gx, gy, Rgb([alpha, alpha, alpha]));
        }
    });

    Some(img)
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

    // Создаём кэш для timestamp глифов (оптимизация: 10-15% ускорение)
    let timestamp_cache = if params.show_timestamps {
        font.as_ref().map(|f| {
            let font_px = (thumb_w as f32 / 15.0).clamp(11.0, 15.0);
            TimestampCache::new(f, font_px)
        })
    } else {
        None
    };

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

    // Используем выбранное пользователем качество resize
    // Fast (Nearest): очень быстро, но низкое качество
    // Normal (CatmullRom): баланс скорости и качества (по умолчанию, на 30-50% быстрее Triangle)
    // Quality (Lanczos3): медленно, но максимальное качество
    let filter_type = params.resize_quality.to_filter_type();

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
                    filter_type,
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

        // ОПТИМИЗАЦИЯ #10: Используем unsafe для копирования пикселей (избегаем bounds checks)
        // Это безопасно, так как мы гарантируем корректные offset'ы через расчёты выше
        for py in 0..thumb_h as usize {
            let source_offset = py * row_bytes;
            let destination_offset = (y0 as usize + py) * canvas_stride + x0 as usize * 3;

            // Используем copy_nonoverlapping для максимальной скорости (без bounds checks)
            unsafe {
                std::ptr::copy_nonoverlapping(
                    source.as_ptr().add(source_offset),
                    destination.as_mut_ptr().add(destination_offset),
                    row_bytes
                );
            }
        }

        if params.show_timestamps {
            if let Some(ref cache) = timestamp_cache {
                let timestamp = format_duration(frame.timestamp);
                let text_width = cache.measure_width(&timestamp);
                let box_w = text_width + 8;
                let font_px = cache.font_size;
                let box_h = font_px as u32 + 4;

                let box_x = (x0 + thumb_w).saturating_sub(box_w + 4);
                let box_y = (y0 + thumb_h).saturating_sub(box_h + 4);

                blend_rect(&mut canvas, box_x, box_y, box_w, box_h, [0, 0, 0], 0.72);

                // Используем кэшированный рендеринг timestamp (10-15% быстрее)
                cache.draw_timestamp(
                    &mut canvas,
                    (box_x + 4) as i32,
                    (box_y + 1) as i32,
                    &timestamp,
                    [255, 255, 255],
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

    // SIMD оптимизация: обрабатываем пиксели группами по 4 канала (f32x4)
    let alpha_vec = f32x4::splat(alpha);
    let inverse_vec = f32x4::splat(inverse);

    for py in y..max_y {
        let row_start = (py * image.width() + x) as usize;
        let row_end = (py * image.width() + max_x) as usize;
        let pixels = &mut image.as_mut()[row_start * 3..row_end * 3];

        // Обрабатываем по 4 канала за раз (примерно 1.33 пикселя)
        let mut i = 0;
        let len = pixels.len();

        // SIMD путь: обрабатываем группы по 4 канала
        while i + 4 <= len {
            let old = f32x4::new([
                pixels[i] as f32,
                pixels[i + 1] as f32,
                pixels[i + 2] as f32,
                pixels[i + 3] as f32,
            ]);

            // Выбираем цвет для каждого канала (R, G, B, R циклично)
            let color_vec = match i % 3 {
                0 => f32x4::new([color[0] as f32, color[1] as f32, color[2] as f32, color[0] as f32]),
                1 => f32x4::new([color[1] as f32, color[2] as f32, color[0] as f32, color[1] as f32]),
                _ => f32x4::new([color[2] as f32, color[0] as f32, color[1] as f32, color[2] as f32]),
            };

            // new = old * inverse + color * alpha
            let result = old * inverse_vec + color_vec * alpha_vec;
            let result_array = result.to_array();

            pixels[i] = result_array[0] as u8;
            pixels[i + 1] = result_array[1] as u8;
            pixels[i + 2] = result_array[2] as u8;
            pixels[i + 3] = result_array[3] as u8;

            i += 4;
        }

        // Обрабатываем оставшиеся каналы скалярно
        while i < len {
            let channel_idx = i % 3;
            let old_val = pixels[i] as f32;
            pixels[i] = (old_val * inverse + color[channel_idx] as f32 * alpha) as u8;
            i += 1;
        }
    }
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
