// Основное приложение ScreenlistApp
// Этот файл содержит главную логику приложения для генерации скринлистов

use eframe::egui;
use image::RgbImage;
use rayon::prelude::*;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::*;
use crate::i18n::Language;
use crate::upload::FastPicSubApp;
use crate::utils::{copy_text_to_clipboard, format_duration, format_file_size_lang, is_video_file};
use crate::video::*;

pub struct ScreenlistApp {
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
    pub fn new() -> Self {
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
                resize_quality: ResizeQuality::Normal,
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
            status_line: "Готов к работе. Добавьте видео, выберите папку или перетащите их в окно."
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
            let slots = topology.pick_worker_slots(params.workers, params.core_mode);
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

                // ОПТИМИЗАЦИЯ #4: Декодируем сразу в целевое разрешение (30-50% ускорение!)
                // Декодируем напрямую в размер thumbnail, избегая последующего resize
                // Это устраняет overhead на масштабирование после декодирования
                let decode_w = thumb_w;
                let decode_h = thumb_h;
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
            .pick_worker_slots(params.workers, params.core_mode)
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
            .pick_worker_slots(params.workers, params.core_mode)
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
                        "Количество полноразмерных скриншотов ус��ановлено в 0. Укажите от 1 до 10 сверху и запустите генерацию.",
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
                    .pick_worker_slots(self.params.workers, self.params.core_mode)
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

            ui.separator();
            ui.label(lang.tr("Качество Resize:", "Resize Quality:"));

            egui::ComboBox::from_id_source("resize_quality_combo")
                .selected_text(self.params.resize_quality.name(lang))
                .show_ui(ui, |ui| {
                    ui.set_enabled(!busy);

                    if ui.selectable_value(
                        &mut self.params.resize_quality,
                        ResizeQuality::Fast,
                        ResizeQuality::Fast.name(lang),
                    )
                    .changed()
                    {
                        layout_changed = true;
                    }

                    if ui.selectable_value(
                        &mut self.params.resize_quality,
                        ResizeQuality::Normal,
                        ResizeQuality::Normal.name(lang),
                    )
                    .changed()
                    {
                        layout_changed = true;
                    }

                    if ui.selectable_value(
                        &mut self.params.resize_quality,
                        ResizeQuality::Quality,
                        ResizeQuality::Quality.name(lang),
                    )
                    .changed()
                    {
                        layout_changed = true;
                    }
                });

            ui.separator();
            ui.add_enabled(
                !busy,
                egui::Checkbox::new(
                    &mut self.params.turbo_keyframe,
                    lang.tr("Турбо (I-Frame)", "Turbo (I-Frame)"),
                ),
            )
                .on_hover_text(lang.tr(
                    "AVDISCARD_NONKEY: декодирует только ключевые кадр�� напрямую через C libavcodec.",
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

pub fn setup_custom_style(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(22, 24, 34);
    ctx.set_visuals(visuals);
}
