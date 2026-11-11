# Screenlist Generator and Uploader PRO

**Screenlist Generator and Uploader PRO** is a high-performance desktop utility written in **Rust** (`egui` / `eframe` + `rayon`) powered by a **Native C Libav Engine** (`FFmpeg C SDK`). It is designed for rapid batch generation of video contact sheets (screenlists) and instant inspection of video/audio stream metadata.

Unlike traditional wrappers that spawn hundreds of external `ffmpeg`/`ffprobe` CLI processes, **Screenlist Generator PRO** decodes frames and probes metadata directly in memory via native C bindings — delivering extreme speed and zero external setup for end users.

---

## Highlights & Features

### Contact Sheet (Screenlist) Generation
- **Customizable Grid:** Adjust frame count (`1–120`), columns (`1–10`), output width (`600–3840 px`), and timestamp overlays.
- **Zero-Wait RAM Cache:** Decoded thumbnails are cached in memory. Changing the column count, sheet width, or toggling timestamps re-renders all sheets in **milliseconds** without touching the disk or re-decoding video streams.
- **Interactive Frame Shifting:** Click any thumbnail directly on the generated preview to shift that specific frame forward by **`+1.0 second`**.

### Media Stream Info & One-Click Copy
Every video added to the queue is automatically analyzed and displays two ready-to-use specification lines:
- **Video Line:** `Видео: Кодек: AVC (High), Разрешение: 1920×1080 (16:9), Частота кадров: 29.970 fps, Битрейт: 4500 kbps`
- **Audio Line:** `Аудио: Кодек: AAC, Частота: 48 kHz, Каналы: L R, Битрейт: 192 kbps`
- **Click to Copy:** Clicking either line immediately copies the text to the system clipboard.

### Extreme Performance
- **Turbo (I-Frame) Mode:** Instructs `libavcodec` (`AVDISCARD_NONKEY`) to decode only keyframes, speeding up batch generation by up to **10×** (e.g., 14 videos × 32 frames in ~1.5 seconds).
- **Hybrid CPU Core Pinning (Windows):** Detects Performance (**P**) and Efficient (**E**) cores using the Win32 `GetLogicalProcessorInformationEx` API. Pin worker threads to **Auto (P + E)**, **P-Cores Only**, or **E-Cores Only** and view per-core-type frame statistics upon completion.
- **GPU Hardware Acceleration:** Supports `D3D11VA`, `DXVA2`, `CUDA`, `QSV`, `VAAPI`, `VideoToolbox`, and `Vulkan` hardware contexts with automatic fallback to software decoding.

### Batch Workflow & Export
- **Flexible Input:** Add individual videos, **recursively scan folders and subfolders**, or **Drag & Drop** files/folders into the application window.
  - Automatic Duplicate Video Detection
- **Multiple Export Options:**
  - Save the currently selected screenlist as **JPEG** or **PNG**.
  - Batch-export all generated screenlists into a target directory.
  - Export all screenlists into a single **`.zip` archive** using multi-threaded JPEG encoding.
  - Export and ZIP Archive Progress Bar

### English and Russian Localization
- **Localization Switch:** Can be switched between English or Russian localization.

---

## Installation (End Users)

No installation or external FFmpeg setup is required:

1. Download the latest release from the [**Releases**](../../releases) page.
2. Extract the archive into any folder (keep `screenlist_pro.exe` together with `avcodec-*.dll`, `avformat-*.dll`, `avutil-*.dll`, `swscale-*.dll`, and `swresample-*.dll`).
3. Launch **`screenlist_pro.exe`**.

---

## Building from Source (Windows x64)
### Prerequisites
1. **Rust Toolchain** (stable, `x86_64-pc-windows-msvc`)
2. **Visual Studio C++ Build Tools** (MSVC compiler for compiling the C bridge via the `cc` crate)
3. **FFmpeg Shared C SDK** (`include`, `lib`, `bin`):
   - Download `ffmpeg-n8.1-latest-win64-gpl-shared-8.1.zip` from [FFmpeg-Builds](https://github.com/FFmpeg-Builds/FFmpeg-Builds/releases).
   - Extract the SDK to `C:\ffmpeg-sdk` (or set the `FFMPEG_DIR` environment variable to your custom SDK path).

### Build & Run
```powershell
# Build optimized release binary (build.rs automatically links C libraries and copies DLLs)
cargo build --release

# Run the application
cargo run --release
```

After running cargo build --release, the required runtime files in target\release\ are:

    screenlist_pro.exe
    avcodec-62.dll
    avformat-62.dll
    avutil-60.dll
    swscale-9.dll
    swresample-6.dll

## Tech Stack
- **GUI Framework:** eframe / egui
- **Video Decoding & Probing:** Custom C11 bridge (libav_bridge.c) linked against libavformat, libavcodec, libswscale, and libavutil
- **Parallelism:** rayon + Win32 Thread Affinity API
- **Image Processing & Typography:** image, ab_glyph, zip

## License
**This project is distributed under GPLv3 License.**
