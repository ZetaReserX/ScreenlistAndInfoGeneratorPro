// Обёртка над нативным C-мостом libav (libav_bridge.c)

use image::RgbImage;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::Path;
use std::sync::Once;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SlProbeInfo {
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub video_bitrate_bps: i64,
    pub container_bitrate_bps: i64,
    pub video_codec: [c_char; 64],
    pub video_profile: [c_char; 128],

    pub has_audio: i32,
    pub audio_sample_rate: i32,
    pub audio_channels: i32,
    pub audio_channel_mask: u64,
    pub audio_bitrate_bps: i64,
    pub audio_codec: [c_char; 64],
}

impl Default for SlProbeInfo {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

#[derive(Clone, Debug)]
pub struct ProbedMedia {
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub video_info_line: String,
    pub audio_info_line: String,
    pub video_info_line_en: String,
    pub audio_info_line_en: String,
}

unsafe extern "C" {
    fn sl_libav_init();
    fn sl_probe_video(
        path: *const c_char,
        out_info: *mut SlProbeInfo,
        err_buf: *mut c_char,
        err_cap: usize,
    ) -> c_int;

    fn sl_decoder_open(
        path: *const c_char,
        use_hwaccel: c_int,
        err_buf: *mut c_char,
        err_cap: usize,
    ) -> *mut c_void;

    fn sl_decoder_decode_rgb(
        dec: *mut c_void,
        timestamp_sec: f64,
        target_w: u32,
        target_h: u32,
        turbo_keyframe: c_int,
        out_rgb: *mut u8,
        err_buf: *mut c_char,
        err_cap: usize,
    ) -> c_int;

    fn sl_decoder_close(dec: *mut c_void);

    // Общий демуксер: контейнер разбирается один раз на видео.
    fn sl_demuxer_open(path: *const c_char, err_buf: *mut c_char, err_cap: usize) -> *mut c_void;
    fn sl_demuxer_close(dm: *mut c_void);
    fn sl_demuxer_fetch(
        dm: *mut c_void,
        timestamp_sec: f64,
        turbo_keyframe: c_int,
        err_buf: *mut c_char,
        err_cap: usize,
    ) -> *mut c_void;
    fn sl_batch_free(batch: *mut c_void);

    // Декодер, работающий только с пакетами из демуксера (по одному на поток).
    fn sl_batch_decoder_open(dm: *mut c_void, err_buf: *mut c_char, err_cap: usize) -> *mut c_void;
    fn sl_batch_decoder_close(bd: *mut c_void);
    fn sl_batch_decode_rgb(
        bd: *mut c_void,
        batch: *mut c_void,
        target_w: u32,
        target_h: u32,
        turbo_keyframe: c_int,
        out_rgb: *mut u8,
        err_buf: *mut c_char,
        err_cap: usize,
    ) -> c_int;
}

static INIT: Once = Once::new();

pub fn init() {
    INIT.call_once(|| unsafe {
        sl_libav_init();
    });
}

fn path_to_cstring(path: &Path) -> Result<CString, String> {
    let path_str = path.to_string_lossy();
    CString::new(path_str.as_bytes())
        .map_err(|_| "File path contains an invalid null byte.".to_owned())
}

fn read_err_buf(buf: &[c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned() }
}

fn read_c_char_buf(buf: &[c_char]) -> String {
    unsafe {
        CStr::from_ptr(buf.as_ptr())
            .to_string_lossy()
            .trim()
            .to_owned()
    }
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a.max(1)
}

fn format_aspect_ratio(width: u32, height: u32) -> String {
    if width == 0 || height == 0 {
        return "N/A".to_owned();
    }
    let g = gcd(width, height);
    let rw = width / g;
    let rh = height / g;
    if rw <= 50 && rh <= 50 {
        return format!("{}:{}", rw, rh);
    }
    let ratio = width as f64 / height as f64;
    let common = [
        (16, 9),
        (4, 3),
        (3, 2),
        (21, 9),
        (1, 1),
        (9, 16),
        (3, 4),
        (2, 3),
        (16, 10),
        (5, 4),
    ];
    for (cw, ch) in common {
        let cr = cw as f64 / ch as f64;
        if (ratio - cr).abs() / cr < 0.018 {
            return format!("{}:{}", cw, ch);
        }
    }
    format!("{}:{}", rw, rh)
}

fn format_video_codec(raw_codec: &str, raw_profile: &str) -> String {
    let base = match raw_codec.to_ascii_lowercase().as_str() {
        "h264" | "avc" | "avc1" => "AVC".to_owned(),
        "hevc" | "h265" => "HEVC".to_owned(),
        "av1" => "AV1".to_owned(),
        "vp9" => "VP9".to_owned(),
        "vp8" => "VP8".to_owned(),
        "mpeg4" => "MPEG-4".to_owned(),
        "mpeg2video" => "MPEG-2".to_owned(),
        "mpeg1video" => "MPEG-1".to_owned(),
        "wmv3" => "WMV3".to_owned(),
        "vc1" => "VC-1".to_owned(),
        "mjpeg" => "MJPEG".to_owned(),
        "prores" => "ProRes".to_owned(),
        "theora" => "Theora".to_owned(),
        "" => "Unknown".to_owned(),
        other => other.to_uppercase(),
    };

    let prof = raw_profile.trim();
    if !prof.is_empty() && !prof.eq_ignore_ascii_case("unknown") {
        format!("{} ({})", base, prof)
    } else {
        base
    }
}

fn format_audio_codec(raw_codec: &str) -> String {
    let lower = raw_codec.to_ascii_lowercase();
    match lower.as_str() {
        "aac" => "AAC".to_owned(),
        "mp3" | "mp3float" => "MP3".to_owned(),
        "ac3" => "AC-3".to_owned(),
        "eac3" => "E-AC-3".to_owned(),
        "opus" => "Opus".to_owned(),
        "vorbis" => "Vorbis".to_owned(),
        "flac" => "FLAC".to_owned(),
        "dts" => "DTS".to_owned(),
        "alac" => "ALAC".to_owned(),
        "wmav1" | "wmav2" | "wmapro" | "wmalossless" => "WMA".to_owned(),
        "" => "Unknown".to_owned(),
        other if other.starts_with("pcm_") => "PCM".to_owned(),
        other => other.to_uppercase(),
    }
}

fn format_audio_channels(channels: i32, mask: u64) -> String {
    match channels {
        0 => "N/A".to_owned(),
        1 => "C".to_owned(),
        2 => "L R".to_owned(),
        3 => {
            if (mask & 0x8) != 0 {
                "L R LFE".to_owned()
            } else {
                "L R C".to_owned()
            }
        }
        4 => "L R Ls Rs".to_owned(),
        5 => "L R C Ls Rs".to_owned(),
        6 => "L R C LFE Ls Rs".to_owned(),
        7 => "L R C LFE Cs Ls Rs".to_owned(),
        8 => "L R C LFE Ls Rs Lrs Rrs".to_owned(),
        n => format!("{}", n),
    }
}

pub fn probe_video(path: &Path, file_size: u64) -> Result<ProbedMedia, String> {
    init();
    let c_path = path_to_cstring(path)?;
    let mut info = SlProbeInfo::default();
    let mut err_buf = [0 as c_char; 512];

    let ret = unsafe {
        sl_probe_video(
            c_path.as_ptr(),
            &mut info,
            err_buf.as_mut_ptr(),
            err_buf.len(),
        )
    };

    if ret != 0 {
        return Err(read_err_buf(&err_buf));
    }

    let mut total_bps = info.container_bitrate_bps;
    if total_bps <= 0 && file_size > 0 && info.duration > 0.0 {
        total_bps = ((file_size as f64 * 8.0) / info.duration) as i64;
    }

    let mut v_bps = info.video_bitrate_bps;
    let mut a_bps = info.audio_bitrate_bps;

    if v_bps <= 0 && total_bps > 0 {
        if a_bps > 0 && total_bps > a_bps {
            v_bps = total_bps - a_bps;
        } else {
            v_bps = total_bps;
        }
    }
    if info.has_audio != 0 && a_bps <= 0 && total_bps > 0 && v_bps > 0 && total_bps > v_bps {
        a_bps = total_bps - v_bps;
    }

    let v_codec_raw = read_c_char_buf(&info.video_codec);
    let v_prof_raw = read_c_char_buf(&info.video_profile);
    let v_codec_str = format_video_codec(&v_codec_raw, &v_prof_raw);
    let aspect_str = format_aspect_ratio(info.width, info.height);
    let fps_str = if info.fps > 0.0 {
        format!("{:.3} fps", info.fps)
    } else {
        "N/A".to_owned()
    };
    let v_bitrate_str = if v_bps > 0 {
        format!("{} kbps", (v_bps + 500) / 1000)
    } else {
        "N/A".to_owned()
    };

    let video_info_line = format!(
        "Видео: Кодек: {}, Разрешение: {}×{} ({}), Частота кадров: {}, Битрейт: {}",
        v_codec_str, info.width, info.height, aspect_str, fps_str, v_bitrate_str
    );

    let video_info_line_en = format!(
        "Video: Codec: {}, Resolution: {}×{} ({}), Frame rate: {}, Bitrate: {}",
        v_codec_str, info.width, info.height, aspect_str, fps_str, v_bitrate_str
    );

    let (audio_info_line, audio_info_line_en) = if info.has_audio != 0 {
        let a_codec_raw = read_c_char_buf(&info.audio_codec);
        let a_codec_str = format_audio_codec(&a_codec_raw);
        let sample_rate_str = if info.audio_sample_rate > 0 {
            format!("{} kHz", info.audio_sample_rate / 1000)
        } else {
            "N/A".to_owned()
        };
        let channels_str = format_audio_channels(info.audio_channels, info.audio_channel_mask);
        let a_bitrate_str = if a_bps > 0 {
            format!("{} kbps", (a_bps + 500) / 1000)
        } else {
            "N/A".to_owned()
        };

        let ru = format!(
            "Аудио: Кодек: {}, Частота: {}, Каналы: {}, Битрейт: {}",
            a_codec_str, sample_rate_str, channels_str, a_bitrate_str
        );
        let en = format!(
            "Audio: Codec: {}, Sample rate: {}, Channels: {}, Bitrate: {}",
            a_codec_str, sample_rate_str, channels_str, a_bitrate_str
        );
        (ru, en)
    } else {
        ("Аудио: Отсутствует".to_owned(), "Audio: None".to_owned())
    };

    Ok(ProbedMedia {
        duration: info.duration,
        width: info.width,
        height: info.height,
        video_info_line,
        audio_info_line,
        video_info_line_en,
        audio_info_line_en,
    })
}

// SAFETY: контекст libav не разделяется между потоками одновременно: декодер
// принадлежит одной задаче, а между задачами передаётся только через Mutex-пул.
unsafe impl Send for LibavDecoder {}

pub struct LibavDecoder {
    raw: *mut c_void,
}

impl LibavDecoder {
    pub fn open(path: &Path, use_hwaccel: bool) -> Result<Self, String> {
        init();
        let c_path = path_to_cstring(path)?;
        let mut err_buf = [0 as c_char; 512];

        let raw = unsafe {
            sl_decoder_open(
                c_path.as_ptr(),
                c_int::from(use_hwaccel),
                err_buf.as_mut_ptr(),
                err_buf.len(),
            )
        };

        if raw.is_null() {
            return Err(read_err_buf(&err_buf));
        }

        Ok(Self { raw })
    }

    pub fn decode_frame_rgb(
        &mut self,
        timestamp_sec: f64,
        target_w: u32,
        target_h: u32,
        turbo_keyframe: bool,
    ) -> Result<RgbImage, String> {
        let frame_bytes = (target_w as usize)
            .checked_mul(target_h as usize)
            .and_then(|px| px.checked_mul(3))
            .ok_or_else(|| "Frame buffer overflow.".to_owned())?;

        let mut rgb_buf = vec![0u8; frame_bytes + 64];
        let mut err_buf = [0 as c_char; 512];

        let ret = unsafe {
            sl_decoder_decode_rgb(
                self.raw,
                timestamp_sec,
                target_w,
                target_h,
                c_int::from(turbo_keyframe),
                rgb_buf.as_mut_ptr(),
                err_buf.as_mut_ptr(),
                err_buf.len(),
            )
        };

        if ret != 0 {
            return Err(read_err_buf(&err_buf));
        }

        rgb_buf.truncate(frame_bytes);
        RgbImage::from_raw(target_w, target_h, rgb_buf)
            .ok_or_else(|| "Failed to create RgbImage.".to_owned())
    }
}

impl Drop for LibavDecoder {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                sl_decoder_close(self.raw);
            }
        }
    }
}

/// Контейнер, открытый один раз на видео. Чтение пакетов (seek + read_frame)
/// сериализуется Mutex'ом, а декодирование идёт параллельно в `BatchDecoder`.
#[allow(dead_code)]
pub struct SharedDemuxer {
    raw: *mut c_void,
    lock: std::sync::Mutex<()>,
}

// SAFETY: все обращения к AVFormatContext идут через `fetch` под Mutex,
// поэтому из нескольких потоков одновременно он не используется.
unsafe impl Send for SharedDemuxer {}
unsafe impl Sync for SharedDemuxer {}

#[allow(dead_code)]
impl SharedDemuxer {
    pub fn open(path: &Path) -> Result<Self, String> {
        init();
        let c_path = path_to_cstring(path)?;
        let mut err_buf = [0 as c_char; 512];

        let raw = unsafe { sl_demuxer_open(c_path.as_ptr(), err_buf.as_mut_ptr(), err_buf.len()) };
        if raw.is_null() {
            return Err(read_err_buf(&err_buf));
        }

        Ok(Self {
            raw,
            lock: std::sync::Mutex::new(()),
        })
    }

    /// Делает seek к ключевому кадру перед `timestamp_sec` и забирает пакеты,
    /// нужные декодеру. Вызовы сериализуются внутренним Mutex'ом.
    pub fn fetch(&self, timestamp_sec: f64, turbo_keyframe: bool) -> Result<PacketBatch, String> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut err_buf = [0 as c_char; 512];

        let raw = unsafe {
            sl_demuxer_fetch(
                self.raw,
                timestamp_sec,
                c_int::from(turbo_keyframe),
                err_buf.as_mut_ptr(),
                err_buf.len(),
            )
        };

        if raw.is_null() {
            return Err(read_err_buf(&err_buf));
        }

        Ok(PacketBatch { raw })
    }
}

impl Drop for SharedDemuxer {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                sl_demuxer_close(self.raw);
            }
        }
    }
}

/// Набор пакетов для одного кадра. Не привязан к демуксеру и после `fetch`
/// может быть передан в другой поток.
#[allow(dead_code)]
pub struct PacketBatch {
    raw: *mut c_void,
}

// SAFETY: пакеты полностью принадлежат батчу и используются одним потоком.
unsafe impl Send for PacketBatch {}

impl Drop for PacketBatch {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                sl_batch_free(self.raw);
            }
        }
    }
}

/// Декодер, который работает только с кодек-контекстом и готовыми пакетами,
/// без разбора контейнера. Создаётся по одному на поток.
#[allow(dead_code)]
pub struct BatchDecoder {
    raw: *mut c_void,
}

// SAFETY: контекст принадлежит одному владельцу, к AVFormatContext не обращается.
unsafe impl Send for BatchDecoder {}

#[allow(dead_code)]
impl BatchDecoder {
    pub fn open(demuxer: &SharedDemuxer) -> Result<Self, String> {
        let mut err_buf = [0 as c_char; 512];

        // Параметры кодека копируются при создании, поэтому декодер не держит ссылок на демуксер.
        let raw = unsafe { sl_batch_decoder_open(demuxer.raw, err_buf.as_mut_ptr(), err_buf.len()) };
        if raw.is_null() {
            return Err(read_err_buf(&err_buf));
        }

        Ok(Self { raw })
    }

    pub fn decode_rgb(
        &mut self,
        batch: &PacketBatch,
        target_w: u32,
        target_h: u32,
        turbo_keyframe: bool,
    ) -> Result<RgbImage, String> {
        let frame_bytes = (target_w as usize)
            .checked_mul(target_h as usize)
            .and_then(|px| px.checked_mul(3))
            .ok_or_else(|| "Frame buffer overflow.".to_owned())?;

        // +64 байта запаса: sws_scale может писать чуть за границу строки.
        let mut rgb_buf = vec![0u8; frame_bytes + 64];
        let mut err_buf = [0 as c_char; 512];

        let ret = unsafe {
            sl_batch_decode_rgb(
                self.raw,
                batch.raw,
                target_w,
                target_h,
                c_int::from(turbo_keyframe),
                rgb_buf.as_mut_ptr(),
                err_buf.as_mut_ptr(),
                err_buf.len(),
            )
        };

        if ret != 0 {
            return Err(read_err_buf(&err_buf));
        }

        rgb_buf.truncate(frame_bytes);
        RgbImage::from_raw(target_w, target_h, rgb_buf)
            .ok_or_else(|| "Failed to create RgbImage.".to_owned())
    }
}

impl Drop for BatchDecoder {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                sl_batch_decoder_close(self.raw);
            }
        }
    }
}
