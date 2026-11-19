// Модуль загрузки изображений на FastPic

use anyhow::{bail, Context, Result as AnyResult};
use eframe::egui;
use futures_util::stream::{self, StreamExt};
use reqwest::{multipart, Client};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::thread;
use tokio::sync::Semaphore;

use crate::config::{ANONYMOUS_UPLOAD_URL, MAX_CONCURRENT_UPLOADS, UPLOAD_URL};
use crate::i18n::Language;
use crate::upload::{AddProgress, AnonymousUploadResponse, AuthMode, UploadOptions, UploadResponse, UploadResult};
use crate::utils::{collect_directory, is_image_file, localized_error};

pub struct FastPicSubApp {
    pub language: Language,
    pub auth_mode: AuthMode,
    pub own_key: String,
    pub key_visible: bool,
    pub make_thumbnail: bool,
    pub thumb_size: String,
    pub check_thumb: usize,
    pub thumb_text: String,
    pub delete_after: String,
    pub files: Vec<PathBuf>,
    pub results: Vec<UploadResult>,
    pub sections_expanded: bool,
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

    // UI drawing methods - stub implementations for now
    pub fn draw_top_panel(&self, ui: &mut eframe::egui::Ui) {
        ui.label(&self.status);
    }

    pub fn draw_settings(&mut self, ui: &mut eframe::egui::Ui) {
        ui.heading(if self.language == Language::Ru {
            "Настройки FastPic"
        } else {
            "FastPic Settings"
        });
        ui.label(if self.language == Language::Ru {
            "UI метод требует реализации - см. оригинальный main.rs"
        } else {
            "UI method needs implementation - see original main.rs"
        });
    }

    pub fn draw_files(&self, ui: &mut eframe::egui::Ui) {
        ui.heading(if self.language == Language::Ru {
            "Файлы"
        } else {
            "Files"
        });
        ui.label(format!("Files added: {}", self.files.len()));
    }

    pub fn draw_results(&self, ui: &mut eframe::egui::Ui, _videos: &[crate::video::VideoItem]) {
        ui.heading(if self.language == Language::Ru {
            "Результаты"
        } else {
            "Results"
        });
        ui.label(format!("Results: {}", self.results.len()));
    }

    // NOTE: Полный impl с UI кодом требует еще ~1000+ строк
    // Этот код должен быть извлечен из main.rs вручную
}

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
