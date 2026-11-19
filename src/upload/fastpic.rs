// Модуль загрузки изображений на FastPic

use anyhow::{bail, Context, Result as AnyResult};
use futures_util::stream::{self, StreamExt};
use reqwest::{multipart, Client};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::thread;
use tokio::sync::Semaphore;

use crate::config::{ANONYMOUS_UPLOAD_URL, MAX_CONCURRENT_UPLOADS, UPLOAD_URL};
use crate::i18n::Language;
use crate::image::ImageKind;
use crate::upload::{AddProgress, AnonymousUploadResponse, AuthMode, UploadOptions, UploadResponse, UploadResult};
use crate::utils::{check_thumb_name, classify_image_path, collect_directory, copy_text_to_clipboard, extract_screenshot_number, format_colored_tech_line, is_image_file, localized_error, try_probe_video_near_images, VideoBbMeta};
use crate::video::processor::stem_name;
use crate::video::VideoItem;

pub struct FastPicSubApp {
    pub language: Language,
    pub auth_mode: AuthMode,
    pub own_key: String,
    #[allow(dead_code)]
    pub key_visible: bool,
    pub make_thumbnail: bool,
    pub thumb_size: String,
    pub check_thumb: usize,
    pub thumb_text: String,
    pub delete_after: String,
    pub files: Vec<PathBuf>,
    pub results: Vec<UploadResult>,
    #[allow(dead_code)]
    pub sections_expanded: bool,
    #[allow(dead_code)]
    pub expanded_sections: BTreeMap<String, bool>,
    pub status: String,
    pub uploading: bool,
    pub total_to_upload: usize,
    pub receiver: Option<mpsc::Receiver<UploadResult>>,
    pub adding: bool,
    pub add_processed: usize,
    pub add_total: usize,
    pub add_added: usize,
    pub add_rejected: usize,
    pub add_receiver: Option<mpsc::Receiver<AddProgress>>,
}

impl FastPicSubApp {
    pub fn new(language: Language) -> Self {
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
    pub fn is_busy(&self) -> bool {
        self.uploading || self.adding
    }

    pub fn sync_language(&mut self, lang: Language) {
        if self.language != lang {
            self.language = lang;
            if self.files.is_empty() && !self.uploading && !self.adding && self.results.is_empty() {
                self.status = self.language.text("drop_files").to_owned();
            }
        }
    }

    pub fn add_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
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

    pub fn poll_additions(&mut self) {
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

    #[allow(dead_code)]
    pub fn start_upload(&mut self) {
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
                "Список файлов пуст"
            } else {
                "File list is empty"
            }
                .to_owned();
            return;
        }

        let files = self.files.clone();
        let options = UploadOptions {
            key: key.map(str::to_owned),
            language: self.language,
            thumb_size: crate::utils::optional_string(&self.thumb_size),
            check_thumb: if self.make_thumbnail {
                crate::utils::optional_string(&self.check_thumb.to_string())
            } else {
                None
            },
            thumb_text: crate::utils::optional_string(&self.thumb_text),
            delete_after: crate::utils::parse_optional_u64(&self.delete_after, self.language).ok().flatten(),
        };

        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        self.uploading = true;
        self.total_to_upload = files.len();
        self.results.clear();
        self.status = if self.language == Language::Ru {
            format!("Загрузка: 0/{}", files.len())
        } else {
            format!("Uploading: 0/{}", files.len())
        };

        thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(upload_all(files, options, sender));
        });
    }

    pub fn poll_uploads(&mut self) {
        let events = self
            .receiver
            .as_ref()
            .map(|receiver| receiver.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();

        for result in events {
            self.results.push(result);
        }

        if self.uploading {
            self.status = if self.language == Language::Ru {
                format!("Загрузка: {}/{}", self.results.len(), self.total_to_upload)
            } else {
                format!("Uploading: {}/{}", self.results.len(), self.total_to_upload)
            };

            if self.results.len() >= self.total_to_upload {
                self.uploading = false;
                self.receiver = None;
                let success_count = self.results.iter().filter(|r| r.response.is_ok()).count();
                let fail_count = self.results.len() - success_count;
                self.status = if self.language == Language::Ru {
                    format!("Загрузка завершена: успешно {}, ошибок {}", success_count, fail_count)
                } else {
                    format!("Upload complete: {} succeeded, {} failed", success_count, fail_count)
                };
            }
        }
    }

    // UI drawing methods
    pub fn draw_top_panel(&mut self, ui: &mut eframe::egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("FastPic Uploader");
            ui.separator();
            ui.label(self.language.text("subtitle"));
            ui.separator();
            eframe::egui::ComboBox::from_id_source("fastpic_language")
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
                    eframe::egui::Button::new(self.language.text("add_images")),
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
                    eframe::egui::Button::new(self.language.text("add_folder")),
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
                    eframe::egui::Button::new(
                        eframe::egui::RichText::new(self.language.text("upload"))
                            .strong()
                            .color(eframe::egui::Color32::WHITE),
                    )
                        .fill(eframe::egui::Color32::from_rgb(75, 95, 210)),
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
            ui.add(eframe::egui::ProgressBar::new(progress).text(format!(
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
            ui.add(eframe::egui::ProgressBar::new(progress).text(format!(
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

    pub fn draw_settings(&mut self, ui: &mut eframe::egui::Ui) {
        eframe::egui::CollapsingHeader::new(self.language.text("settings"))
            .default_open(true)
            .show(ui, |ui| {
                eframe::egui::Grid::new("fastpic_settings_grid")
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
                                let mut edit = eframe::egui::TextEdit::singleline(&mut self.own_key)
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
                            eframe::egui::TextEdit::singleline(&mut self.thumb_size)
                                .hint_text(self.language.text("example_150")),
                        );
                        ui.end_row();
                        ui.label(self.language.text("thumbnail_text"));
                        ui.add_enabled_ui(self.make_thumbnail, |ui| {
                            eframe::egui::ComboBox::from_id_source("fastpic_check_thumb")
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
                            eframe::egui::TextEdit::singleline(&mut self.thumb_text),
                        );
                        ui.end_row();
                        ui.label(self.language.text("delete_after"));
                        ui.add(
                            eframe::egui::TextEdit::singleline(&mut self.delete_after)
                                .hint_text(self.language.text("no_delete")),
                        );
                        ui.end_row();
                    });
            });
    }

    pub fn draw_files(&mut self, ui: &mut eframe::egui::Ui) {
        ui.heading(format!(
            "{} ({})",
            self.language.text("files"),
            self.files.len()
        ));
        if self.files.is_empty() {
            ui.add_space(8.0);
            ui.label(self.language.text("drop_files"));
        } else {
            eframe::egui::ScrollArea::vertical()
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

    pub fn draw_results(&mut self, ui: &mut eframe::egui::Ui, queue: &[VideoItem]) {
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
                        eframe::egui::RichText::new(self.language.text("copy_full_bb_template"))
                            .strong()
                            .color(eframe::egui::Color32::from_rgb(120, 230, 150)),
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
        eframe::egui::ScrollArea::vertical()
            .id_source("fastpic_results_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (video_name, results) in groups {
                    let expanded = self
                        .expanded_sections
                        .get(&video_name)
                        .copied()
                        .unwrap_or(false);
                    let response = eframe::egui::CollapsingHeader::new(video_name.clone())
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
                                            eframe::egui::RichText::new(format!(
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
                                                eframe::egui::TextEdit::singleline(&mut bbcode)
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
                                                        .open_url(eframe::egui::OpenUrl::new_tab(view_link));
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
                                            eframe::egui::Color32::from_rgb(240, 100, 100),
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

#[allow(dead_code)]
pub async fn upload_all(
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

#[allow(dead_code)]
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

#[allow(dead_code)]
pub fn summarize_server_response(body: &str) -> String {
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

#[allow(dead_code)]
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
