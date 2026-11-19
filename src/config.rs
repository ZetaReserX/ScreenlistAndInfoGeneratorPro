// Конфигурация и константы приложения

pub const MAX_WORKERS: usize = 32;
pub const MAX_FRAMES_PER_DECODER_TASK: usize = 12;  // БЫЛО: 6
pub const ZIP_ENCODE_BATCH_SIZE: usize = 4;

// Upload constants - used by FastPic uploader functionality
#[allow(dead_code)]
pub const UPLOAD_URL: &str = "https://new.fastpic.org/upload";
#[allow(dead_code)]
pub const ANONYMOUS_UPLOAD_URL: &str = "https://fastpic.org/upload?api=1";
#[allow(dead_code)]
pub const MAX_CONCURRENT_UPLOADS: usize = 4;

pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "webm", "flv", "ts", "m2ts", "mts", "m4v", "wmv", "mpg", "mpeg",
    "vob", "3gp", "3g2", "ogv", "asf", "rm", "rmvb", "divx", "f4v", "mxf",
];
