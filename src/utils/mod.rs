// Модуль утилит

mod clipboard;
mod files;
mod bbcode;

pub use clipboard::copy_text_to_clipboard;
pub use files::{collect_directory, is_image_file, is_video_file, classify_image_path, optional_string, parse_optional_u64, localized_error, check_thumb_name};
pub use bbcode::{VideoBbMeta, format_colored_tech_line, extract_screenshot_number, try_probe_video_near_images, format_duration, format_file_size_lang};
