use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const LIBAV_BRIDGE_C: &str = r#"
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <errno.h>

#include <libavformat/avformat.h>
#include <libavcodec/avcodec.h>
#include <libswscale/swscale.h>
#include <libavutil/avutil.h>
#include <libavutil/pixfmt.h>
#include <libavutil/hwcontext.h>
#include <libavutil/error.h>
#include <libavutil/log.h>
#include <libavutil/dict.h>
#include <libavutil/channel_layout.h>

typedef struct SlProbeInfo {
    double duration;
    uint32_t width;
    uint32_t height;
    double fps;
    int64_t video_bitrate_bps;
    int64_t container_bitrate_bps;
    char video_codec[64];
    char video_profile[128];

    int32_t has_audio;
    int32_t audio_sample_rate;
    int32_t audio_channels;
    uint64_t audio_channel_mask;
    int64_t audio_bitrate_bps;
    char audio_codec[64];
} SlProbeInfo;

static void write_err(char *err_buf, size_t err_cap, const char *prefix, int err_code) {
    if (!err_buf || err_cap == 0) return;
    if (err_code < 0) {
        char av_buf[256];
        if (av_strerror(err_code, av_buf, sizeof(av_buf)) == 0) {
            snprintf(err_buf, err_cap, "%s: %s", prefix, av_buf);
            return;
        }
    }
    snprintf(err_buf, err_cap, "%s (code %d)", prefix, err_code);
}

static int64_t get_stream_bitrate(AVStream *stream) {
    if (!stream || !stream->codecpar) return 0;
    if (stream->codecpar->bit_rate > 0) {
        return stream->codecpar->bit_rate;
    }
    AVDictionaryEntry *tag = NULL;
    while ((tag = av_dict_get(stream->metadata, "", tag, AV_DICT_IGNORE_SUFFIX))) {
        if (tag->key && tag->value &&
            (strncmp(tag->key, "BPS", 3) == 0 || strcmp(tag->key, "BITRATE") == 0)) {
            int64_t val = strtoll(tag->value, NULL, 10);
            if (val > 0) return val;
        }
    }
    return 0;
}

void sl_libav_init(void) {
    av_log_set_level(AV_LOG_ERROR);
}

int sl_probe_video(
    const char *path,
    SlProbeInfo *out_info,
    char *err_buf,
    size_t err_cap
) {
    if (!out_info) {
        write_err(err_buf, err_cap, "Null SlProbeInfo pointer", 0);
        return -1;
    }
    memset(out_info, 0, sizeof(SlProbeInfo));

    AVFormatContext *fmt_ctx = NULL;
    int ret = avformat_open_input(&fmt_ctx, path, NULL, NULL);
    if (ret < 0) {
        write_err(err_buf, err_cap, "avformat_open_input failed", ret);
        return -1;
    }

    ret = avformat_find_stream_info(fmt_ctx, NULL);
    if (ret < 0) {
        write_err(err_buf, err_cap, "avformat_find_stream_info failed", ret);
        avformat_close_input(&fmt_ctx);
        return -1;
    }

    int stream_idx = av_find_best_stream(fmt_ctx, AVMEDIA_TYPE_VIDEO, -1, -1, NULL, 0);
    if (stream_idx < 0) {
        write_err(err_buf, err_cap, "Video stream not found", stream_idx);
        avformat_close_input(&fmt_ctx);
        return -1;
    }

    AVStream *stream = fmt_ctx->streams[stream_idx];
    if (!stream || !stream->codecpar) {
        write_err(err_buf, err_cap, "Missing codecpar", 0);
        avformat_close_input(&fmt_ctx);
        return -1;
    }

    int width = stream->codecpar->width;
    int height = stream->codecpar->height;
    double duration = 0.0;

    if (stream->duration > 0 && stream->duration != AV_NOPTS_VALUE) {
        double tb = av_q2d(stream->time_base);
        if (tb > 0.0) {
            duration = (double)stream->duration * tb;
        }
    }

    if (duration <= 0.0 && fmt_ctx->duration > 0 && fmt_ctx->duration != AV_NOPTS_VALUE) {
        duration = (double)fmt_ctx->duration / (double)AV_TIME_BASE;
    }

    if (width <= 0 || height <= 0 || duration <= 0.0) {
        avformat_close_input(&fmt_ctx);
        write_err(err_buf, err_cap, "Invalid dimensions or duration", 0);
        return -1;
    }

    out_info->duration = duration;
    out_info->width = (uint32_t)width;
    out_info->height = (uint32_t)height;
    out_info->container_bitrate_bps = fmt_ctx->bit_rate;
    out_info->video_bitrate_bps = get_stream_bitrate(stream);

    AVRational fr = av_guess_frame_rate(fmt_ctx, stream, NULL);
    if (fr.num > 0 && fr.den > 0) {
        out_info->fps = av_q2d(fr);
    } else if (stream->avg_frame_rate.num > 0 && stream->avg_frame_rate.den > 0) {
        out_info->fps = av_q2d(stream->avg_frame_rate);
    } else if (stream->r_frame_rate.num > 0 && stream->r_frame_rate.den > 0) {
        out_info->fps = av_q2d(stream->r_frame_rate);
    }

    const char *v_codec_name = avcodec_get_name(stream->codecpar->codec_id);
    if (v_codec_name) {
        snprintf(out_info->video_codec, sizeof(out_info->video_codec), "%s", v_codec_name);
    }

    const char *v_prof_name = avcodec_profile_name(stream->codecpar->codec_id, stream->codecpar->profile);
    if (v_prof_name) {
        snprintf(out_info->video_profile, sizeof(out_info->video_profile), "%s", v_prof_name);
    }

    int audio_idx = av_find_best_stream(fmt_ctx, AVMEDIA_TYPE_AUDIO, -1, stream_idx, NULL, 0);
    if (audio_idx >= 0) {
        AVStream *a_stream = fmt_ctx->streams[audio_idx];
        if (a_stream && a_stream->codecpar) {
            out_info->has_audio = 1;
            out_info->audio_sample_rate = a_stream->codecpar->sample_rate;
            out_info->audio_channels = a_stream->codecpar->ch_layout.nb_channels;
            if (a_stream->codecpar->ch_layout.order == AV_CHANNEL_ORDER_NATIVE) {
                out_info->audio_channel_mask = a_stream->codecpar->ch_layout.u.mask;
            } else {
                out_info->audio_channel_mask = 0;
            }
            out_info->audio_bitrate_bps = get_stream_bitrate(a_stream);

            const char *a_codec_name = avcodec_get_name(a_stream->codecpar->codec_id);
            if (a_codec_name) {
                snprintf(out_info->audio_codec, sizeof(out_info->audio_codec), "%s", a_codec_name);
            }
        }
    }

    avformat_close_input(&fmt_ctx);
    return 0;
}

typedef struct SlDecoder {
    AVFormatContext *fmt_ctx;
    AVCodecContext *codec_ctx;
    AVBufferRef *hw_device_ctx;
    struct SwsContext *sws_ctx;
    AVPacket *packet;
    AVFrame *frame;
    AVFrame *best_frame;
    AVFrame *sw_frame;
    int stream_index;
    AVRational time_base;
    int64_t start_time;
    enum AVPixelFormat hw_pix_fmt;
} SlDecoder;

static enum AVPixelFormat get_hw_format_cb(AVCodecContext *ctx, const enum AVPixelFormat *pix_fmts) {
    const SlDecoder *dec = (const SlDecoder *)ctx->opaque;
    if (dec && dec->hw_pix_fmt != AV_PIX_FMT_NONE) {
        for (const enum AVPixelFormat *p = pix_fmts; *p != AV_PIX_FMT_NONE; p++) {
            if (*p == dec->hw_pix_fmt) {
                return *p;
            }
        }
    }
    for (const enum AVPixelFormat *p = pix_fmts; *p != AV_PIX_FMT_NONE; p++) {
        return *p;
    }
    return AV_PIX_FMT_NONE;
}

static int try_init_hwaccel(const AVCodec *decoder, SlDecoder *dec) {
    const enum AVHWDeviceType preferred[] = {
        AV_HWDEVICE_TYPE_D3D11VA,
        AV_HWDEVICE_TYPE_DXVA2,
        AV_HWDEVICE_TYPE_CUDA,
        AV_HWDEVICE_TYPE_QSV,
        AV_HWDEVICE_TYPE_VAAPI,
        AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
        AV_HWDEVICE_TYPE_VULKAN
    };
    int count = (int)(sizeof(preferred) / sizeof(preferred[0]));

    for (int p = 0; p < count; p++) {
        for (int i = 0;; i++) {
            const AVCodecHWConfig *config = avcodec_get_hw_config(decoder, i);
            if (!config) break;

            if ((config->methods & AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX) &&
                config->device_type == preferred[p]) {
                AVBufferRef *hw_dev_ctx = NULL;
                if (av_hwdevice_ctx_create(&hw_dev_ctx, preferred[p], NULL, NULL, 0) >= 0 && hw_dev_ctx) {
                    dec->hw_device_ctx = hw_dev_ctx;
                    dec->hw_pix_fmt = config->pix_fmt;
                    dec->codec_ctx->opaque = dec;
                    dec->codec_ctx->get_format = get_hw_format_cb;
                    dec->codec_ctx->hw_device_ctx = av_buffer_ref(hw_dev_ctx);
                    return 1;
                }
            }
        }
    }
    return 0;
}

void sl_decoder_close(SlDecoder *dec) {
    if (!dec) return;
    if (dec->sws_ctx) sws_freeContext(dec->sws_ctx);
    if (dec->packet) av_packet_free(&dec->packet);
    if (dec->frame) av_frame_free(&dec->frame);
    if (dec->best_frame) av_frame_free(&dec->best_frame);
    if (dec->sw_frame) av_frame_free(&dec->sw_frame);
    if (dec->codec_ctx) avcodec_free_context(&dec->codec_ctx);
    if (dec->hw_device_ctx) av_buffer_unref(&dec->hw_device_ctx);
    if (dec->fmt_ctx) avformat_close_input(&dec->fmt_ctx);
    free(dec);
}

SlDecoder* sl_decoder_open(const char *path, int use_hwaccel, char *err_buf, size_t err_cap) {
    SlDecoder *dec = (SlDecoder *)calloc(1, sizeof(SlDecoder));
    if (!dec) {
        write_err(err_buf, err_cap, "Out of memory", 0);
        return NULL;
    }
    dec->hw_pix_fmt = AV_PIX_FMT_NONE;

    int ret = avformat_open_input(&dec->fmt_ctx, path, NULL, NULL);
    if (ret < 0) {
        write_err(err_buf, err_cap, "avformat_open_input failed", ret);
        sl_decoder_close(dec);
        return NULL;
    }

    ret = avformat_find_stream_info(dec->fmt_ctx, NULL);
    if (ret < 0) {
        write_err(err_buf, err_cap, "avformat_find_stream_info failed", ret);
        sl_decoder_close(dec);
        return NULL;
    }

    const AVCodec *decoder = NULL;
    dec->stream_index = av_find_best_stream(
        dec->fmt_ctx,
        AVMEDIA_TYPE_VIDEO,
        -1,
        -1,
        &decoder,
        0
    );

    if (dec->stream_index < 0 || !decoder) {
        write_err(err_buf, err_cap, "Video decoder not found", dec->stream_index);
        sl_decoder_close(dec);
        return NULL;
    }

    AVStream *stream = dec->fmt_ctx->streams[dec->stream_index];
    dec->time_base = stream->time_base;
    dec->start_time = stream->start_time;

    dec->codec_ctx = avcodec_alloc_context3(decoder);
    if (!dec->codec_ctx) {
        write_err(err_buf, err_cap, "avcodec_alloc_context3 failed", 0);
        sl_decoder_close(dec);
        return NULL;
    }

    ret = avcodec_parameters_to_context(dec->codec_ctx, stream->codecpar);
    if (ret < 0) {
        write_err(err_buf, err_cap, "avcodec_parameters_to_context failed", ret);
        sl_decoder_close(dec);
        return NULL;
    }

    dec->codec_ctx->thread_count = 1;
    dec->codec_ctx->flags2 |= AV_CODEC_FLAG2_FAST;

    if (use_hwaccel) {
        if (!try_init_hwaccel(decoder, dec)) {
            write_err(err_buf, err_cap, "Hardware acceleration unavailable", 0);
            sl_decoder_close(dec);
            return NULL;
        }
    }

    ret = avcodec_open2(dec->codec_ctx, decoder, NULL);
    if (ret < 0) {
        write_err(err_buf, err_cap, "avcodec_open2 failed", ret);
        sl_decoder_close(dec);
        return NULL;
    }

    dec->packet = av_packet_alloc();
    dec->frame = av_frame_alloc();
    dec->best_frame = av_frame_alloc();
    dec->sw_frame = av_frame_alloc();

    if (!dec->packet || !dec->frame || !dec->best_frame || !dec->sw_frame) {
        write_err(err_buf, err_cap, "Failed to allocate frame/packet", 0);
        sl_decoder_close(dec);
        return NULL;
    }

    return dec;
}

static int decode_Attempt(
    SlDecoder *dec,
    double timestamp_sec,
    uint32_t target_w,
    uint32_t target_h,
    int turbo_keyframe,
    uint8_t *out_rgb,
    char *err_buf,
    size_t err_cap
) {
    dec->codec_ctx->skip_frame = turbo_keyframe ? AVDISCARD_NONKEY : AVDISCARD_DEFAULT;

    double tb = av_q2d(dec->time_base);
    int64_t base_pts = (dec->start_time != AV_NOPTS_VALUE) ? dec->start_time : 0;
    int64_t offset_pts = (tb > 0.0 && timestamp_sec > 0.0)
        ? (int64_t)(timestamp_sec / tb + 0.5)
        : 0;
    int64_t target_pts = base_pts + offset_pts;

    avcodec_flush_buffers(dec->codec_ctx);
    if (av_seek_frame(dec->fmt_ctx, dec->stream_index, target_pts, AVSEEK_FLAG_BACKWARD) < 0) {
        av_seek_frame(dec->fmt_ctx, dec->stream_index, 0, AVSEEK_FLAG_BACKWARD);
    }
    avcodec_flush_buffers(dec->codec_ctx);

    av_frame_unref(dec->best_frame);
    int has_best_frame = 0;
    int decoded_count = 0;
    const int max_accurate_steps = 240;
    int eof_sent = 0;

    while (!eof_sent || 1) {
        if (!eof_sent) {
            int read_ret = av_read_frame(dec->fmt_ctx, dec->packet);
            if (read_ret < 0) {
                avcodec_send_packet(dec->codec_ctx, NULL);
                eof_sent = 1;
            } else {
                if (dec->packet->stream_index != dec->stream_index) {
                    av_packet_unref(dec->packet);
                    continue;
                }
                int send_ret = avcodec_send_packet(dec->codec_ctx, dec->packet);
                av_packet_unref(dec->packet);
                if (send_ret < 0 && send_ret != AVERROR(EAGAIN)) {
                    break;
                }
            }
        }

        int stop_outer = 0;
        while (1) {
            int recv_ret = avcodec_receive_frame(dec->codec_ctx, dec->frame);
            if (recv_ret == AVERROR(EAGAIN)) {
                if (eof_sent) stop_outer = 1;
                break;
            } else if (recv_ret < 0) {
                stop_outer = 1;
                break;
            }

            decoded_count++;
            int64_t pts = (dec->frame->best_effort_timestamp != AV_NOPTS_VALUE)
                ? dec->frame->best_effort_timestamp
                : dec->frame->pts;

            av_frame_unref(dec->best_frame);
            av_frame_move_ref(dec->best_frame, dec->frame);
            has_best_frame = 1;

            if (turbo_keyframe ||
                (pts != AV_NOPTS_VALUE && pts >= target_pts) ||
                decoded_count >= max_accurate_steps) {
                stop_outer = 1;
                break;
            }
        }

        if (stop_outer) break;
    }

    if (!has_best_frame) {
        write_err(err_buf, err_cap, "No frame decoded at timestamp", 0);
        return -1;
    }

    AVFrame *src_frame = dec->best_frame;
    int is_hw = (dec->hw_pix_fmt != AV_PIX_FMT_NONE && src_frame->format == (int)dec->hw_pix_fmt)
                || (src_frame->hw_frames_ctx != NULL);

    if (is_hw) {
        av_frame_unref(dec->sw_frame);
        int tr_ret = av_hwframe_transfer_data(dec->sw_frame, src_frame, 0);
        if (tr_ret < 0) {
            av_frame_unref(dec->best_frame);
            write_err(err_buf, err_cap, "av_hwframe_transfer_data failed", tr_ret);
            return -1;
        }
        src_frame = dec->sw_frame;
    }

    int src_w = src_frame->width;
    int src_h = src_frame->height;
    enum AVPixelFormat src_fmt = (enum AVPixelFormat)src_frame->format;

    if (src_w <= 0 || src_h <= 0 || src_fmt == AV_PIX_FMT_NONE) {
        if (is_hw) av_frame_unref(dec->sw_frame);
        av_frame_unref(dec->best_frame);
        write_err(err_buf, err_cap, "Invalid decoded frame format", 0);
        return -1;
    }

    dec->sws_ctx = sws_getCachedContext(
        dec->sws_ctx,
        src_w,
        src_h,
        src_fmt,
        (int)target_w,
        (int)target_h,
        AV_PIX_FMT_RGB24,
        SWS_FAST_BILINEAR,
        NULL,
        NULL,
        NULL
    );

    if (!dec->sws_ctx) {
        if (is_hw) av_frame_unref(dec->sw_frame);
        av_frame_unref(dec->best_frame);
        write_err(err_buf, err_cap, "sws_getCachedContext failed", 0);
        return -1;
    }

    uint8_t *dst_data[4] = { out_rgb, NULL, NULL, NULL };
    int dst_linesize[4] = { (int)(target_w * 3), 0, 0, 0 };

    int scaled_h = sws_scale(
        dec->sws_ctx,
        (const uint8_t *const *)src_frame->data,
        src_frame->linesize,
        0,
        src_h,
        dst_data,
        dst_linesize
    );

    if (is_hw) av_frame_unref(dec->sw_frame);
    av_frame_unref(dec->best_frame);

    if (scaled_h <= 0) {
        write_err(err_buf, err_cap, "sws_scale failed", scaled_h);
        return -1;
    }

    return 0;
}

int sl_decoder_decode_rgb(
    SlDecoder *dec,
    double timestamp_sec,
    uint32_t target_w,
    uint32_t target_h,
    int turbo_keyframe,
    uint8_t *out_rgb,
    char *err_buf,
    size_t err_cap
) {
    int res = decode_Attempt(dec, timestamp_sec, target_w, target_h, turbo_keyframe, out_rgb, err_buf, err_cap);
    if (res < 0 && turbo_keyframe) {
        res = decode_Attempt(dec, timestamp_sec, target_w, target_h, 0, out_rgb, err_buf, err_cap);
    }
    return res;
}
"#;

fn find_ffmpeg_sdk() -> Option<PathBuf> {
    if let Ok(dir) = env::var("FFMPEG_DIR") {
        let path = PathBuf::from(dir);
        if path.exists() {
            return Some(path);
        }
    }

    let default_win = PathBuf::from(r"C:\ffmpeg-sdk");
    if default_win.exists() {
        return Some(default_win);
    }

    None
}

fn copy_dlls_to_target(bin_dir: &Path, out_dir: &Path) {
    let Some(target_profile_dir) = out_dir.ancestors().nth(3) else {
        return;
    };

    let Ok(entries) = fs::read_dir(bin_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let is_dll = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("dll"))
            .unwrap_or(false);

        if is_dll {
            if let Some(name) = path.file_name() {
                let dst = target_profile_dir.join(name);
                if !dst.exists() {
                    let _ = fs::copy(&path, &dst);
                }
            }
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let c_file = out_dir.join("libav_bridge.c");
    fs::write(&c_file, LIBAV_BRIDGE_C).expect("Failed to write libav_bridge.c");

    let mut build = cc::Build::new();
    build.file(&c_file);
    build.warnings(false);

    if let Some(sdk) = find_ffmpeg_sdk() {
        let include_dir = sdk.join("include");
        let lib_dir = sdk.join("lib");
        let bin_dir = sdk.join("bin");

        if include_dir.exists() {
            build.include(&include_dir);
        }
        if lib_dir.exists() {
            println!("cargo:rustc-link-search=native={}", lib_dir.display());
        }
        if bin_dir.exists() {
            copy_dlls_to_target(&bin_dir, &out_dir);
        }
    }

    build.compile("libav_bridge");

    println!("cargo:rustc-link-lib=dylib=avformat");
    println!("cargo:rustc-link-lib=dylib=avcodec");
    println!("cargo:rustc-link-lib=dylib=swscale");
    println!("cargo:rustc-link-lib=dylib=avutil");
}