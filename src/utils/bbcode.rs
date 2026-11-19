use std::fs;
use std::path::Path;
use crate::i18n::Language;
use crate::upload::UploadResult;
use crate::utils::files::is_video_file;

// VideoBbMeta - used for generating BB-code templates for video releases
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct VideoBbMeta {
    pub file_name: String,
    pub duration: f64,
    pub file_size: u64,
    pub width: u32,
    pub height: u32,
    pub video_info: String,
    pub audio_info: String,
}

impl VideoBbMeta {
    #[allow(dead_code)]
    pub fn from_video_item(item: &crate::video::VideoItem, lang: Language) -> Self {
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

    #[allow(dead_code)]
    pub fn format_spoiler_header(&self) -> String {
        let duration_str = format_duration(self.duration);
        let size_mb = self.file_size as f64 / (1024.0 * 1024.0);
        let size_str = format!("{:.2} Mb", size_mb).replace('.', ",");
        format!(
            "{} | {} | {}x{} | {}",
            duration_str, size_str, self.width, self.height, self.file_name
        )
    }
}

#[allow(dead_code)]
pub fn format_colored_tech_line(line: &str) -> String {
    if let Some((prefix, rest)) = line.split_once(':') {
        format!("[b][color=#cc66ff]{}:[/color][/b]{}", prefix.trim(), rest)
    } else {
        line.to_string()
    }
}

#[allow(dead_code)]
pub fn extract_screenshot_number(path: &Path) -> usize {
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

#[allow(dead_code)]
pub fn try_probe_video_near_images(
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
                    if let Ok(meta) = crate::video::probe_video(&path, file_size) {
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

pub fn format_duration(seconds: f64) -> String {
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

pub fn format_file_size_lang(bytes: u64, lang: Language) -> String {
    let megabytes = bytes as f64 / (1024.0 * 1024.0);
    let formatted = format!("{:.2}", megabytes);
    match lang {
        Language::Ru => format!("{} Мб", formatted.replace('.', ",")),
        Language::En => format!("{} MB", formatted),
    }
}
