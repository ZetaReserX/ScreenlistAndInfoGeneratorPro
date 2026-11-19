use anyhow::{Context, Result as AnyResult};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use crate::config::VIDEO_EXTENSIONS;

pub fn collect_directory(path: &Path) -> AnyResult<Vec<PathBuf>> {
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

pub fn is_image_file(path: &Path) -> bool {
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

pub fn is_video_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            let ext_lower = ext.to_ascii_lowercase();
            VIDEO_EXTENSIONS.contains(&ext_lower.as_str())
        })
        .unwrap_or(false)
}

#[allow(dead_code)]
pub fn classify_image_path(path: &Path) -> (String, crate::image::ImageKind) {
    use crate::image::ImageKind;

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    let lower = stem.to_ascii_lowercase();

    let kind = if lower.contains("screenlist") || lower.contains("sheet") {
        ImageKind::Screenlist
    } else if lower.contains("screenshot") || lower.contains("screen") {
        ImageKind::Screenshot
    } else {
        ImageKind::Other
    };

    let video_name = if let Some(pos) = lower.find("_screenlist") {
        stem[..pos].to_string()
    } else if let Some(pos) = lower.find("_sheet") {
        stem[..pos].to_string()
    } else if let Some(pos) = lower.rfind("_screenshot") {
        stem[..pos].to_string()
    } else if let Some(pos) = lower.rfind("_screen") {
        stem[..pos].to_string()
    } else {
        stem.to_string()
    };

    (video_name, kind)
}

#[allow(dead_code)]
pub fn optional_string(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.to_owned())
}

#[allow(dead_code)]
pub fn parse_optional_u64(value: &str, language: crate::i18n::Language) -> Result<Option<u64>, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed
        .parse::<u64>()
        .map(Some)
        .map_err(|_| match language {
            crate::i18n::Language::Ru => "Неверное число".to_owned(),
            crate::i18n::Language::En => "Invalid number".to_owned(),
        })
}

#[allow(dead_code)]
pub fn localized_error(
    language: crate::i18n::Language,
    ru_message: &str,
    en_message: &str,
    error: &(impl std::fmt::Display + ?Sized),
) -> String {
    let base_message = match language {
        crate::i18n::Language::Ru => ru_message,
        crate::i18n::Language::En => en_message,
    };
    let error_str = error.to_string();
    if error_str.is_empty() {
        base_message.to_owned()
    } else {
        format!("{}: {}", base_message, error_str)
    }
}

#[allow(dead_code)]
pub fn check_thumb_name(language: crate::i18n::Language, value: usize) -> &'static str {
    if value == 0 {
        language.text("no_text")
    } else if value == 1 {
        language.text("size_mode")
    } else if value == 2 {
        language.text("custom_text")
    } else {
        language.text("filename")
    }
}
