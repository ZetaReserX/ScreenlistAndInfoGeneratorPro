use anyhow::Result as AnyResult;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use crate::i18n::Language;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UploadResponse {
    pub big_link: Option<String>,
    pub thumb_link: Option<String>,
    pub view_link: Option<String>,
    pub short_link: Option<String>,
    pub delete_link: Option<String>,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub size: Option<u64>,
    pub hotlinked: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct AnonymousUploadResponse {
    pub status: Option<String>,
    pub error: Option<String>,
    pub imagepath: Option<String>,
    pub viewurl: Option<String>,
    pub viewfullurl: Option<String>,
    pub thumbpath: Option<String>,
}

impl UploadResponse {
    pub fn bbcode(&self) -> String {
        match (&self.view_link, &self.thumb_link) {
            (Some(view), Some(thumb)) => format!("[URL={view}][IMG]{thumb}[/IMG][/URL]"),
            _ => String::new(),
        }
    }
}

#[derive(Debug)]
pub struct UploadResult {
    pub path: PathBuf,
    pub response: AnyResult<UploadResponse>,
}

#[derive(Debug)]
pub struct AddProgress {
    pub path: PathBuf,
    pub processed: usize,
    pub is_image: bool,
}

#[derive(Debug, Clone)]
pub struct UploadOptions {
    pub key: Option<String>,
    pub language: Language,
    pub thumb_size: Option<String>,
    pub check_thumb: Option<String>,
    pub thumb_text: Option<String>,
    pub delete_after: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    Anonymous,
    Own,
}
