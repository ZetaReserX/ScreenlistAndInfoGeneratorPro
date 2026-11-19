// Модуль загрузки на FastPic

mod types;
mod fastpic;

pub use types::*;
pub use fastpic::{FastPicSubApp, upload_all, summarize_server_response};
