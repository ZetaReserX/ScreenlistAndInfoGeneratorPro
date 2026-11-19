// Модуль работы с видео

mod types;
mod cpu;
pub mod ffmpeg;
pub mod processor;

pub use types::*;
pub use cpu::{win_cpu, *};
pub use processor::*;
