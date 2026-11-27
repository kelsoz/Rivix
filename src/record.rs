//! 屏幕录制模块：支持框选后按比例/预设尺寸微调，以及基于 FFmpeg（NVENC 硬件加速 + 分段防崩溃 MP4 / GIF）的录制。

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    rc::Rc,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use image::RgbaImage;
use rgpui::{
    AnyWindowHandle, App, AppContext, Bounds, Context, DisplayId, FocusHandle, KeyDownEvent,
    Pixels, Render, ScreenCaptureSource, Window, WindowBackgroundAppearance, WindowBounds,
    WindowKind, WindowOptions, div, point, prelude::*, px, rgb, rgba, size,
};

use crate::{
    app::{RivixApp, load_save_dir, palette},
    feedback,
    i18n::{tr, tr_fmt},
    toast, tray,
    window::{
        BorderWindowHandle, close_border_window, exclude_from_capture, open_border_window,
        set_always_on_top,
    },
};

/// 录屏期间临时注册的全局停止组合键编号（Shift+Esc，避免与日常 Esc / F9 冲突）。
pub const STOP_HOTKEY_SHIFT_ESC_ID: u32 = 5;

/// 录屏输出格式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordFormat {
    /// H.264 + AAC 封装的分段防崩溃 MP4 视频。
    Mp4,
    /// 调色板优化循环动态 GIF。
    Gif,
}

impl RecordFormat {
    /// 从配置字符串解析输出格式，未知值回退为 MP4。
    pub fn from_config(value: &str) -> Self {
        if value.eq_ignore_ascii_case("gif") {
            Self::Gif
        } else {
            Self::Mp4
        }
    }

    /// 转为配置持久化字符串与扩展名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Gif => "gif",
        }
    }
}

/// 录屏选区比例预设。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AspectPreset {
    /// 自由比例。
    Free,
    /// 横屏 16:9。
    Ratio16x9,
    /// 竖屏 9:16。
    Ratio9x16,
    /// 横屏 4:3。
    Ratio4x3,
    /// 竖屏 3:4。
    Ratio3x4,
    /// 正方形 1:1。
    Ratio1x1,
}

impl AspectPreset {
    /// 全部可供切换的比例预设列表。
    pub const ALL: [Self; 6] = [
        Self::Free,
        Self::Ratio16x9,
        Self::Ratio9x16,
        Self::Ratio4x3,
        Self::Ratio3x4,
        Self::Ratio1x1,
    ];

    /// 返回目标宽高比（`Free` 返回 `None`）。
    pub fn ratio(self) -> Option<f32> {
        match self {
            Self::Free => None,
            Self::Ratio16x9 => Some(16.0 / 9.0),
            Self::Ratio9x16 => Some(9.0 / 16.0),
            Self::Ratio4x3 => Some(4.0 / 3.0),
            Self::Ratio3x4 => Some(3.0 / 4.0),
            Self::Ratio1x1 => Some(1.0),
        }
    }

    /// 按钮显示标签。
    pub fn label(self) -> String {
        match self {
            Self::Free => tr("rec.ratio_free"),
            Self::Ratio16x9 => "16:9".to_string(),
            Self::Ratio9x16 => "9:16".to_string(),
            Self::Ratio4x3 => "4:3".to_string(),
            Self::Ratio3x4 => "3:4".to_string(),
            Self::Ratio1x1 => "1:1".to_string(),
        }
    }
}

/// 标准物理分辨率预设（名称、物理宽、物理高）。
pub const SIZE_PRESETS: [(&str, u32, u32); 4] = [
    ("1080p", 1920, 1080),
    ("720p", 1280, 720),
    ("9:16", 1080, 1920),
    ("1:1", 800, 800),
];

/// 按目标宽高比调整物理选区矩形，尽量保持中心点不变并限制在屏幕边界内。
pub fn fit_rect_to_ratio(
    rect: (f32, f32, f32, f32),
    ratio: f32,
    bounds: (f32, f32),
) -> (f32, f32, f32, f32) {
    let (x, y, w, h) = rect;
    let (max_w, max_h) = (bounds.0.max(32.0), bounds.1.max(32.0));
    let cx = x + w * 0.5;
    let cy = y + h * 0.5;

    let area = (w.max(32.0) * h.max(32.0)).max(64.0 * 64.0);
    let mut new_w = (area * ratio).sqrt();
    let mut new_h = new_w / ratio;

    if new_w > max_w {
        new_w = max_w;
        new_h = new_w / ratio;
    }
    if new_h > max_h {
        new_h = max_h;
        new_w = new_h * ratio;
    }

    let nx = (cx - new_w * 0.5).clamp(0.0, (max_w - new_w).max(0.0));
    let ny = (cy - new_h * 0.5).clamp(0.0, (max_h - new_h).max(0.0));
    (nx, ny, new_w.max(16.0), new_h.max(16.0))
}

/// 按标准物理像素尺寸调整选区，若屏幕较小则按比例缩小以适配当前屏幕。
pub fn fit_rect_to_size(
    rect: (f32, f32, f32, f32),
    target_px: (u32, u32),
    scale: (f32, f32),
    bounds: (f32, f32),
) -> (f32, f32, f32, f32) {
    let (max_w, max_h) = (bounds.0.max(32.0), bounds.1.max(32.0));
    let mut lw = target_px.0 as f32 / scale.0.max(0.1);
    let mut lh = target_px.1 as f32 / scale.1.max(0.1);
    if lw > max_w || lh > max_h {
        let factor = (max_w / lw).min(max_h / lh);
        lw *= factor;
        lh *= factor;
    }
    let cx = rect.0 + rect.2 * 0.5;
    let cy = rect.1 + rect.3 * 0.5;
    let nx = (cx - lw * 0.5).clamp(0.0, (max_w - lw).max(0.0));
    let ny = (cy - lh * 0.5).clamp(0.0, (max_h - lh).max(0.0));
    (nx, ny, lw.max(16.0), lh.max(16.0))
}

/// 一次录屏会话的完整规格。
#[derive(Clone)]
pub struct RecordSpec {
    /// 目标显示器 ID。
    pub display_id: Option<DisplayId>,
    /// 显示器逻辑边界（用于放置录制边框与控制条）。
    pub display_bounds: Bounds<Pixels>,
    /// 选区逻辑矩形 `(x, y, w, h)`（相对所属显示器左上角）。
    pub view_rect: (f32, f32, f32, f32),
    /// 物理像素裁剪区 `(x, y, w, h)`（宽高已对齐偶数）。
    pub crop_px: (u32, u32, u32, u32),
    /// 红框在桌面全局物理坐标系中的外框矩形 `(x, y, w, h)`（已外扩 3px）。
    pub border_px: (i32, i32, i32, i32),
    /// 输出格式。
    pub format: RecordFormat,
    /// 目标帧率。
    pub fps: u32,
    /// 是否录制系统声音（仅 MP4 生效）。
    pub system_audio: bool,
    /// 是否录制麦克风（仅 MP4 生效）。
    pub mic_audio: bool,
    /// 是否在录屏画面中叠加系统鼠标光标。
    pub show_cursor: bool,
}

/// 正在进行的录屏共享控制状态。
struct ActiveSession {
    stop_flag: Arc<AtomicBool>,
    paused_flag: Arc<AtomicBool>,
    hud_windows: Arc<Mutex<Vec<AnyWindowHandle>>>,
    border_window: Arc<Mutex<Option<BorderWindowHandle>>>,
}

static ACTIVE: OnceLock<Mutex<Option<ActiveSession>>> = OnceLock::new();

fn active_slot() -> &'static Mutex<Option<ActiveSession>> {
    ACTIVE.get_or_init(|| Mutex::new(None))
}

/// 当前是否正在录屏（用于托盘菜单显示与防止重入）。
pub fn is_recording() -> bool {
    active_slot()
        .lock()
        .map(|guard| guard.is_some())
        .unwrap_or(false)
}

/// 当前录屏是否处于暂停态。
pub fn is_paused() -> bool {
    active_slot()
        .lock()
        .ok()
        .and_then(|guard| {
            guard
                .as_ref()
                .map(|s| s.paused_flag.load(Ordering::Relaxed))
        })
        .unwrap_or(false)
}

/// 切换当前录屏的暂停/继续状态。
pub fn toggle_pause() -> bool {
    if let Ok(guard) = active_slot().lock()
        && let Some(session) = guard.as_ref()
    {
        let prev = session.paused_flag.load(Ordering::Relaxed);
        session.paused_flag.store(!prev, Ordering::Relaxed);
        return !prev;
    }
    false
}

/// 请求停止当前录屏（控制条按钮、Shift+Esc、托盘菜单共用）。
pub fn stop_active() -> bool {
    if let Ok(guard) = active_slot().lock()
        && let Some(session) = guard.as_ref()
    {
        session.stop_flag.store(true, Ordering::SeqCst);
        return true;
    }
    false
}

/// 关闭全部录屏 HUD 窗口并清空全局会话占位。
fn cleanup_session(cx: &mut App) {
    let (handles, border) = if let Ok(mut guard) = active_slot().lock() {
        let taken = guard.take();
        let handles = taken
            .as_ref()
            .and_then(|s| s.hud_windows.lock().ok().map(|w| w.clone()))
            .unwrap_or_default();
        let border = taken.and_then(|s| s.border_window.lock().ok().and_then(|mut b| b.take()));
        (handles, border)
    } else {
        (Vec::new(), None)
    };
    if let Some(b) = border {
        close_border_window(b);
    }
    for handle in handles {
        let _ = cx.update_window(handle, |_, window, _| {
            window.remove_window();
        });
    }
    cx.unregister_global_hotkey(STOP_HOTKEY_SHIFT_ESC_ID);
    tray::refresh_labels(cx);
    cx.refresh_windows();
}

/// 查找可用的 `ffmpeg` 可执行文件：同目录优先，其次 PATH 与常见本机位置。
fn find_ffmpeg() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let name = if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        };
        let local = dir.join(name);
        if local.is_file() {
            return Some(local);
        }
    }
    // 检查全局 PATH。
    let path_bin = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    if let Ok(out) = hidden_command(Path::new(path_bin))
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        && out.success()
    {
        return Some(PathBuf::from(path_bin));
    }
    None
}

/// 探测 FFmpeg 是否可用 `h264_nvenc`（避免无 N 卡环境直接报错）。
fn supports_nvenc(ffmpeg: &Path) -> bool {
    let Ok(out) = hidden_command(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&out.stdout).contains("h264_nvenc")
}

/// 查询当前录屏编码器就绪状态（缓存结果，供设置页实时展示徽章）。
/// 返回 `(ffmpeg_found, nvenc_supported)`。
pub fn encoder_capabilities() -> (bool, bool) {
    static CAPS: OnceLock<(bool, bool)> = OnceLock::new();
    *CAPS.get_or_init(|| match find_ffmpeg() {
        Some(ff) => (true, supports_nvenc(&ff)),
        None => (false, false),
    })
}

/// 构造不弹出控制台黑框的命令对象。
fn hidden_command(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// 启动一次录屏会话（打开置顶空心红框与悬浮控制条，后台通过 FFmpeg 子进程编码）。
pub fn start_recording(
    spec: RecordSpec,
    main: rgpui::Entity<RivixApp>,
    cx: &mut App,
) -> Result<(), String> {
    if is_recording() {
        return Err("recording already in progress".to_string());
    }
    let Some(ffmpeg_path) = find_ffmpeg() else {
        return Err("未找到 ffmpeg.exe（请将其放到 rivix.exe 同目录或系统 PATH）".to_string());
    };

    let sources_rx = cx.screen_capture_sources();
    let out_path = next_recording_path(spec.format);
    let primary_display_id = cx.primary_display().map(|d| d.id());

    let stop_flag = Arc::new(AtomicBool::new(false));
    let paused_flag = Arc::new(AtomicBool::new(false));
    let elapsed_ms = Arc::new(AtomicU64::new(0));
    let hud_windows = Arc::new(Mutex::new(Vec::new()));
    let border_window = Arc::new(Mutex::new(None));

    {
        let mut guard = active_slot().lock().map_err(|e| e.to_string())?;
        *guard = Some(ActiveSession {
            stop_flag: stop_flag.clone(),
            paused_flag: paused_flag.clone(),
            hud_windows: hud_windows.clone(),
            border_window: border_window.clone(),
        });
    }

    open_hud_windows(&spec, &elapsed_ms, &hud_windows, &border_window, cx);
    // 注册临时全局 Shift+Esc 停止组合键，避免与常规 Esc 或 F9 冲突。
    if let Ok(key) = rgpui::Keystroke::parse("shift-escape") {
        cx.unregister_global_hotkey(STOP_HOTKEY_SHIFT_ESC_ID);
        let _ = cx.register_global_hotkey(STOP_HOTKEY_SHIFT_ESC_ID, &key);
    }
    tray::refresh_labels(cx);
    tray::start_recording_pulse(cx);

    let (frame_tx, frame_rx) = mpsc::sync_channel::<RgbaImage>(4);
    let stream_stop = stop_flag.clone();
    let stream_paused = paused_flag.clone();
    let fps = spec.fps.clamp(5, 60);
    let min_interval = Duration::from_secs_f64(1.0 / fps as f64);
    let crop = spec.crop_px;
    let show_cursor = spec.show_cursor;
    let crop_origin_px = (spec.border_px.0 + 3, spec.border_px.1 + 3);
    let last_sent = Arc::new(Mutex::new(None::<Instant>));
    let last_sent_cb = last_sent.clone();
    let foreground = cx.foreground_executor().clone();

    let stop_for_bg = stop_flag.clone();
    let paused_for_bg = paused_flag.clone();
    let elapsed_for_bg = elapsed_ms.clone();
    let out_for_bg = out_path.clone();
    let spec_for_bg = spec.clone();

    cx.spawn(async move |cx| {
        let sources = match sources_rx.await {
            Ok(Ok(list)) if !list.is_empty() => list,
            _ => {
                cx.update(|cx| {
                    cleanup_session(cx);
                    let msg = tr("cap.no_source");
                    main.update(cx, |state, cx| state.set_status(&msg, cx));
                    toast::show(cx, msg);
                });
                return;
            }
        };

        let source = pick_source_for_display(&sources, spec_for_bg.display_id, primary_display_id);
        let callback: Box<dyn Fn(rgpui::ScreenCaptureFrame) + Send> =
            Box::new(move |frame: rgpui::ScreenCaptureFrame| {
                if stream_stop.load(Ordering::Relaxed) || stream_paused.load(Ordering::Relaxed) {
                    return;
                }
                let now = Instant::now();
                if let Ok(mut guard) = last_sent_cb.lock() {
                    if let Some(prev) = *guard
                        && now.duration_since(prev) < min_interval
                    {
                        return;
                    }
                    *guard = Some(now);
                }
                if let Some(rgba) = frame.to_rgba() {
                    let mut cropped = crop_rgba_clamped(&rgba, crop);
                    if show_cursor {
                        cursor_overlay::draw_cursor_on_frame(&mut cropped, crop_origin_px);
                    }
                    let _ = frame_tx.try_send(cropped);
                }
            });

        let stream = match source.stream(&foreground, callback).await {
            Ok(Ok(s)) => s,
            _ => {
                cx.update(|cx| {
                    cleanup_session(cx);
                    let msg = tr_fmt("rec.failed", &[("msg", "stream start failed")]);
                    main.update(cx, |state, cx| state.set_status(&msg, cx));
                    toast::show(cx, msg);
                });
                return;
            }
        };

        // 保持 stream 在录制生命周期内存活，直到外部请求停止。
        let _keep_stream = stream;
        let result = cx
            .background_executor()
            .spawn(async move {
                run_ffmpeg_pipeline(
                    &ffmpeg_path,
                    &spec_for_bg,
                    &out_for_bg,
                    frame_rx,
                    &stop_for_bg,
                    &paused_for_bg,
                    &elapsed_for_bg,
                )
            })
            .await;

        drop(_keep_stream);
        cx.update(|cx| {
            cleanup_session(cx);
            match result {
                Ok(_saved_path) => {
                    feedback::completed();
                    let msg = tr("rec.saved");
                    main.update(cx, |state, cx| {
                        state.set_status(&msg, cx);
                    });
                    toast::show(cx, msg);
                }
                Err(err) => {
                    // 仅当文件为空或未产生有效片段时才删除；若已录有内容则保留以防丢片。
                    if std::fs::metadata(&out_path).is_ok_and(|m| m.len() < 1024) {
                        let _ = std::fs::remove_file(&out_path);
                    }
                    let msg = tr_fmt("rec.failed", &[("msg", &err)]);
                    main.update(cx, |state, cx| {
                        state.set_status(&msg, cx);
                    });
                    toast::show(cx, msg);
                }
            }
        });
    })
    .detach();

    Ok(())
}

/// 按 DisplayId 选取对应的系统捕获源（无精确匹配时回退主屏或首个源）。
pub(crate) fn pick_source_for_display(
    sources: &[Rc<dyn ScreenCaptureSource>],
    target: Option<DisplayId>,
    primary_id: Option<DisplayId>,
) -> Rc<dyn ScreenCaptureSource> {
    if let Some(target_id) = target {
        if let Some(found) = sources
            .iter()
            .find(|src| crate::capture::source_matches_display(&src.metadata().ok(), target_id))
        {
            return found.clone();
        }
        if primary_id == Some(target_id)
            && let Some(found) = sources
                .iter()
                .find(|src| src.metadata().ok().and_then(|m| m.is_main).unwrap_or(false))
        {
            return found.clone();
        }
    }
    sources[0].clone()
}

/// 系统下载文件夹路径（无法解析时回退到截图保存目录）。
fn downloads_dir() -> PathBuf {
    #[cfg(windows)]
    if let Ok(home) = std::env::var("USERPROFILE") {
        let dir = PathBuf::from(home).join("Downloads");
        if dir.is_dir() || std::fs::create_dir_all(&dir).is_ok() {
            return dir;
        }
    }
    #[cfg(not(windows))]
    {
        if let Ok(dir) = std::env::var("XDG_DOWNLOAD_DIR") {
            let p = PathBuf::from(dir);
            if p.is_dir() {
                return p;
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            let dir = PathBuf::from(home).join("Downloads");
            if dir.is_dir() || std::fs::create_dir_all(&dir).is_ok() {
                return dir;
            }
        }
    }
    load_save_dir()
}

/// 生成录屏输出文件路径：`Downloads/rivix-<毫秒时间戳>.<ext>`。
fn next_recording_path(format: RecordFormat) -> PathBuf {
    let dir = downloads_dir();
    let _ = std::fs::create_dir_all(&dir);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    dir.join(format!("rivix-{millis}.{}", format.as_str()))
}

/// 裁剪整屏 RGBA 到偶数对齐的目标矩形（越界时安全钳制）。
pub(crate) fn crop_rgba_clamped(src: &RgbaImage, crop: (u32, u32, u32, u32)) -> RgbaImage {
    let (sw, sh) = src.dimensions();
    let x = crop.0.min(sw.saturating_sub(2));
    let y = crop.1.min(sh.saturating_sub(2));
    let w = crop.2.min(sw.saturating_sub(x)).max(2);
    let h = crop.3.min(sh.saturating_sub(y)).max(2);
    image::imageops::crop_imm(src, x, y, w, h).to_image()
}

/// 将任意尺寸 RGBA 图像拷贝进固定 `(width, height)` 的连续 RGBA 缓冲区（不足补黑、超出裁剪）。
fn pack_rgba_exact(img: &RgbaImage, width: u32, height: u32, buf: &mut [u8]) {
    let (sw, sh) = img.dimensions();
    let copy_w = width.min(sw) as usize;
    let copy_h = height.min(sh) as usize;
    let dst_stride = width as usize * 4;
    let src_stride = sw as usize * 4;
    let raw = img.as_raw();
    if copy_w == width as usize && copy_h == height as usize {
        buf.copy_from_slice(&raw[..dst_stride * height as usize]);
        return;
    }
    buf.fill(0);
    for y in 0..copy_h {
        let s = &raw[y * src_stride..y * src_stride + copy_w * 4];
        let d = &mut buf[y * dst_stride..y * dst_stride + copy_w * 4];
        d.copy_from_slice(s);
    }
}

/// 通过 FFmpeg 独立子进程执行编码（视频经 stdin 写入，音频经命名管道写入）。
fn run_ffmpeg_pipeline(
    ffmpeg: &Path,
    spec: &RecordSpec,
    out_path: &Path,
    frame_rx: mpsc::Receiver<RgbaImage>,
    stop: &AtomicBool,
    paused: &Arc<AtomicBool>,
    elapsed_ms: &AtomicU64,
) -> Result<PathBuf, String> {
    let width = (spec.crop_px.2 & !1).max(16);
    let height = (spec.crop_px.3 & !1).max(16);
    let fps = spec.fps.clamp(5, 60);

    // 1. 若为 MP4 且开启了音频，先在 Windows 上创建命名管道并启动 WASAPI 写线程。
    let want_audio = cfg!(target_os = "windows")
        && spec.format == RecordFormat::Mp4
        && (spec.system_audio || spec.mic_audio);
    let audio_stop = Arc::new(AtomicBool::new(false));
    #[cfg(target_os = "windows")]
    let mut audio_pipe_name: Option<String> = None;
    #[cfg(target_os = "windows")]
    let mut audio_thread: Option<std::thread::JoinHandle<()>> = None;
    #[cfg(not(target_os = "windows"))]
    let audio_pipe_name: Option<String> = None;
    #[cfg(not(target_os = "windows"))]
    let audio_thread: Option<std::thread::JoinHandle<()>> = None;

    #[cfg(target_os = "windows")]
    if want_audio && let Some((pipe_name, handle)) = wasapi_audio::create_audio_pipe() {
        audio_pipe_name = Some(pipe_name);
        let sys_on = spec.system_audio;
        let mic_on = spec.mic_audio;
        let s_flag = audio_stop.clone();
        let p_flag = paused.clone();
        audio_thread = Some(std::thread::spawn(move || {
            wasapi_audio::serve_audio_pipe(handle, sys_on, mic_on, &s_flag, &p_flag);
        }));
    }

    // 2. 构建 FFmpeg 命令行（MP4 启用 NVENC + 分段防崩溃 moov；GIF 启用调色板优化）。
    let use_nvenc = spec.format == RecordFormat::Mp4 && supports_nvenc(ffmpeg);
    let mut cmd = hidden_command(ffmpeg);
    cmd.arg("-y")
        .args(["-f", "rawvideo"])
        .args(["-pixel_format", "rgba"])
        .args(["-video_size", &format!("{width}x{height}")])
        .args(["-framerate", &fps.to_string()])
        .args(["-i", "pipe:0"]);

    if let Some(ref pipe) = audio_pipe_name {
        cmd.args(["-f", "s16le"])
            .args(["-ar", "48000"])
            .args(["-ac", "2"])
            .args(["-i", pipe]);
    }

    match spec.format {
        RecordFormat::Mp4 => {
            if use_nvenc {
                cmd.args(["-c:v", "h264_nvenc", "-preset", "p4", "-cq", "23"]);
            } else {
                cmd.args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "23"]);
            }
            cmd.args(["-pix_fmt", "yuv420p"]);
            if audio_pipe_name.is_some() {
                // 使用软限幅保护峰值，避免多音源或自动增益后瞬间破音。
                cmd.args([
                    "-af",
                    "alimiter=limit=0.95:attack=5:release=50",
                    "-c:a",
                    "aac",
                    "-b:a",
                    "192k",
                ]);
            }
            // 分段式 MP4：每写出关键帧即落盘可播，意外崩溃也不丢前段视频。
            cmd.args(["-movflags", "+frag_keyframe+empty_moov+default_base_moof"]);
        }
        RecordFormat::Gif => {
            cmd.args([
                "-vf",
                "split[s0][s1];[s0]palettegen=stats_mode=diff[p];[s1][p]paletteuse=dither=bayer",
            ]);
        }
    }

    cmd.arg(out_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child: Child = cmd.spawn().map_err(|e| format!("启动 FFmpeg 失败：{e}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "无法打开 FFmpeg 标准输入".to_string())?;
    let stderr_handle = child.stderr.take().map(|mut err| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = err.read_to_string(&mut buf);
            buf
        })
    });

    // 3. 以恒定帧率向 FFmpeg stdin 输送裁剪后的 RGBA 帧。
    let frame_interval = Duration::from_secs_f64(1.0 / fps as f64);
    let mut frame_buf = vec![0u8; width as usize * height as usize * 4];
    let mut last_img: Option<RgbaImage> = None;
    let mut active_ns: u64 = 0;
    let mut last_tick = Instant::now();
    let mut next_frame_ns: u64 = 0;
    let interval_ns = frame_interval.as_nanos() as u64;
    let mut written_frames: u64 = 0;

    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if !paused.load(Ordering::Relaxed) {
            active_ns += now.duration_since(last_tick).as_nanos() as u64;
            elapsed_ms.store(active_ns / 1_000_000, Ordering::Relaxed);
        }
        last_tick = now;

        while let Ok(img) = frame_rx.try_recv() {
            last_img = Some(img);
        }

        if !paused.load(Ordering::Relaxed)
            && active_ns >= next_frame_ns
            && let Some(ref img) = last_img
        {
            pack_rgba_exact(img, width, height, &mut frame_buf);
            if stdin.write_all(&frame_buf).is_err() {
                break;
            }
            written_frames += 1;
            next_frame_ns += interval_ns;
        }

        std::thread::sleep(Duration::from_millis(6));
    }

    // 4. 优雅收尾：关闭视频 stdin + 停音频管道，等待 FFmpeg 写完尾帧退出。
    drop(stdin);
    audio_stop.store(true, Ordering::SeqCst);
    if let Some(handle) = audio_thread {
        let _ = handle.join();
    }

    let status = child
        .wait()
        .map_err(|e| format!("等待 FFmpeg 退出失败：{e}"))?;
    let stderr_text = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    if written_frames == 0 {
        return Err("no video frames captured".to_string());
    }
    if !status.success() {
        let tail = stderr_text
            .lines()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join(" | ");
        crate::capture::cap_log(&format!("FFmpeg 异常退出：{stderr_text}"));
        return Err(if tail.is_empty() {
            format!("ffmpeg exited with {status}")
        } else {
            tail
        });
    }
    Ok(out_path.to_path_buf())
}

// ============================================================================
// Windows WASAPI 音频采集 + 命名管道推流（系统内录 / 麦克风混音）
// ============================================================================

#[cfg(target_os = "windows")]
mod wasapi_audio {
    use super::*;
    use std::os::windows::io::FromRawHandle;
    use windows::{
        Win32::{
            Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
            Media::Audio::{
                AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
                Endpoints::IAudioEndpointVolume, IAudioCaptureClient, IAudioClient,
                IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eConsole, eRender,
            },
            Storage::FileSystem::PIPE_ACCESS_OUTBOUND,
            System::{
                Com::{
                    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
                    CoTaskMemFree, CoUninitialize,
                },
                Pipes::{
                    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
                    PIPE_WAIT,
                },
            },
        },
        core::HSTRING,
    };

    static PIPE_SEQ: AtomicU64 = AtomicU64::new(1);

    pub struct RawPipeHandle(pub HANDLE);
    unsafe impl Send for RawPipeHandle {}

    /// 在启动 FFmpeg 前预先创建命名管道，消除连接竞态。
    pub fn create_audio_pipe() -> Option<(String, RawPipeHandle)> {
        let seq = PIPE_SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!(r"\\.\pipe\rivix-audio-{}-{seq}", std::process::id());
        let wname = HSTRING::from(&name);
        let handle = unsafe {
            CreateNamedPipeW(
                &wname,
                PIPE_ACCESS_OUTBOUND,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                131_072,
                0,
                0,
                None,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }
        Some((name, RawPipeHandle(handle)))
    }

    struct AudioSource {
        client: IAudioClient,
        capture: IAudioCaptureClient,
        endpoint_volume: Option<IAudioEndpointVolume>,
        sample_rate: u32,
        channels: u16,
        is_float: bool,
    }

    impl AudioSource {
        unsafe fn open(loopback: bool) -> Option<Self> {
            let enumerator: IMMDeviceEnumerator =
                unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.ok()?;
            let flow = if loopback { eRender } else { eCapture };
            let device = unsafe { enumerator.GetDefaultAudioEndpoint(flow, eConsole) }.ok()?;
            let endpoint_volume: Option<IAudioEndpointVolume> = if loopback {
                unsafe { device.Activate(CLSCTX_ALL, None) }.ok()
            } else {
                None
            };
            let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }.ok()?;

            let fmt_ptr = unsafe { client.GetMixFormat() }.ok()?;
            let fmt = unsafe { *fmt_ptr };
            let sample_rate = fmt.nSamplesPerSec.max(8000);
            let channels = fmt.nChannels.max(1);
            let is_float =
                fmt.wFormatTag == 3 || (fmt.wFormatTag == 0xFFFE && fmt.wBitsPerSample == 32);

            let flags = if loopback {
                AUDCLNT_STREAMFLAGS_LOOPBACK
            } else {
                0
            };
            let hr = unsafe {
                client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 1_000_000, 0, fmt_ptr, None)
            };
            unsafe { CoTaskMemFree(Some(fmt_ptr.cast())) };
            if hr.is_err() {
                return None;
            }
            let capture: IAudioCaptureClient = unsafe { client.GetService() }.ok()?;
            unsafe { client.Start() }.ok()?;
            Some(Self {
                client,
                capture,
                endpoint_volume,
                sample_rate,
                channels,
                is_float,
            })
        }

        /// 读取系统当前主音量标量（0.0..=1.0），用于反向无损补偿。
        fn master_volume_scalar(&self) -> f32 {
            self.endpoint_volume
                .as_ref()
                .and_then(|v| unsafe { v.GetMasterVolumeLevelScalar().ok() })
                .unwrap_or(1.0)
        }

        /// 读取当前可用音频包并重采样为 48kHz 双声道 f32。
        unsafe fn read_48k_stereo(&self) -> Vec<f32> {
            let mut out = Vec::new();
            while let Ok(packet_len) = unsafe { self.capture.GetNextPacketSize() } {
                if packet_len == 0 {
                    break;
                }
                let mut data_ptr: *mut u8 = std::ptr::null_mut();
                let mut frames: u32 = 0;
                let mut flags: u32 = 0;
                if unsafe {
                    self.capture
                        .GetBuffer(&mut data_ptr, &mut frames, &mut flags, None, None)
                }
                .is_err()
                {
                    break;
                }
                let silent = (flags & 0x2) != 0;
                let mut stereo = Vec::with_capacity(frames as usize * 2);
                if silent || data_ptr.is_null() {
                    stereo.resize(frames as usize * 2, 0.0f32);
                } else if self.is_float {
                    let samples = unsafe {
                        std::slice::from_raw_parts(
                            data_ptr.cast::<f32>(),
                            frames as usize * self.channels as usize,
                        )
                    };
                    for frame in samples.chunks_exact(self.channels as usize) {
                        let l = frame[0];
                        let r = if self.channels >= 2 { frame[1] } else { l };
                        stereo.push(l);
                        stereo.push(r);
                    }
                } else {
                    let samples = unsafe {
                        std::slice::from_raw_parts(
                            data_ptr.cast::<i16>(),
                            frames as usize * self.channels as usize,
                        )
                    };
                    for frame in samples.chunks_exact(self.channels as usize) {
                        let l = frame[0] as f32 / 32768.0;
                        let r = if self.channels >= 2 {
                            frame[1] as f32 / 32768.0
                        } else {
                            l
                        };
                        stereo.push(l);
                        stereo.push(r);
                    }
                }
                let _ = unsafe { self.capture.ReleaseBuffer(frames) };
                resample_stereo(&stereo, self.sample_rate, 48_000, &mut out);
            }
            out
        }
    }

    impl Drop for AudioSource {
        fn drop(&mut self) {
            unsafe {
                let _ = self.client.Stop();
            }
        }
    }

    /// 线性重采样双声道 f32 到目标采样率。
    fn resample_stereo(src: &[f32], src_rate: u32, dst_rate: u32, dst: &mut Vec<f32>) {
        if src.is_empty() {
            return;
        }
        if src_rate == dst_rate {
            dst.extend_from_slice(src);
            return;
        }
        let src_frames = src.len() / 2;
        let dst_frames = ((src_frames as u64 * dst_rate as u64) / src_rate as u64).max(1) as usize;
        let ratio = src_rate as f64 / dst_rate as f64;
        for i in 0..dst_frames {
            let pos = i as f64 * ratio;
            let idx = (pos as usize).min(src_frames.saturating_sub(1));
            let next = (idx + 1).min(src_frames.saturating_sub(1));
            let frac = (pos - idx as f64) as f32;
            let l = src[idx * 2] * (1.0 - frac) + src[next * 2] * frac;
            let r = src[idx * 2 + 1] * (1.0 - frac) + src[next * 2 + 1] * frac;
            dst.push(l);
            dst.push(r);
        }
    }

    /// 柔和膝盖限幅：0.85 以下 1:1 直通无损，超出部分平滑压缩到 1.0 以内避免爆音。
    fn soft_limit(x: f32) -> f32 {
        let ax = x.abs();
        if ax <= 0.85 {
            x
        } else {
            let over = (ax - 0.85) / 0.15;
            x.signum() * (0.85 + 0.15 * over.tanh())
        }
    }

    /// 等待 FFmpeg 连上命名管道后持续写入混音后的 48kHz Stereo s16le PCM。
    pub fn serve_audio_pipe(
        raw: RawPipeHandle,
        system_audio: bool,
        mic_audio: bool,
        stop: &AtomicBool,
        paused: &AtomicBool,
    ) {
        let handle = raw.0;
        let connected = unsafe { ConnectNamedPipe(handle, None) };
        if connected.is_err() {
            let err = std::io::Error::last_os_error();
            // ERROR_PIPE_CONNECTED = 535，表示客户端在调用 ConnectNamedPipe 前已就绪。
            if err.raw_os_error() != Some(535) {
                unsafe {
                    let _ = CloseHandle(handle);
                }
                return;
            }
        }
        // 转交 std::fs::File 管理句柄关闭，避免手动 WriteFile。
        let mut pipe_file = unsafe { std::fs::File::from_raw_handle(handle.0) };

        let com_ok = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let sys = if system_audio {
            unsafe { AudioSource::open(true) }
        } else {
            None
        };
        let mic = if mic_audio {
            unsafe { AudioSource::open(false) }
        } else {
            None
        };

        let mut last_audio_at = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            let s_buf = sys
                .as_ref()
                .map(|s| unsafe { s.read_48k_stereo() })
                .unwrap_or_default();
            let m_buf = mic
                .as_ref()
                .map(|m| unsafe { m.read_48k_stereo() })
                .unwrap_or_default();

            let now = Instant::now();
            if !paused.load(Ordering::Relaxed) {
                if !s_buf.is_empty() || !m_buf.is_empty() {
                    last_audio_at = now;
                    // 读取系统主音量并做浮点域反向补偿（上限 6.0x 避免极低音量下放大底噪）。
                    let sys_vol = sys
                        .as_ref()
                        .map(AudioSource::master_volume_scalar)
                        .unwrap_or(1.0);
                    let sys_gain = (1.0 / sys_vol.max(0.16)).min(6.0);
                    let len = s_buf.len().max(m_buf.len());
                    let mut bytes = Vec::with_capacity(len * 2);
                    for i in 0..len {
                        let sv = s_buf.get(i).copied().unwrap_or(0.0) * sys_gain;
                        let mv = m_buf.get(i).copied().unwrap_or(0.0) * 1.2;
                        let v = soft_limit(sv + mv);
                        let sample = (v * 32767.0) as i16;
                        bytes.extend_from_slice(&sample.to_le_bytes());
                    }
                    if pipe_file.write_all(&bytes).is_err() {
                        break;
                    }
                } else {
                    let gap_ms = now.duration_since(last_audio_at).as_millis() as u64;
                    if gap_ms >= 60 {
                        last_audio_at = now;
                        let silent_frames = ((48_000 * gap_ms) / 1000).clamp(480, 4800) as usize;
                        let bytes = vec![0u8; silent_frames * 4];
                        if pipe_file.write_all(&bytes).is_err() {
                            break;
                        }
                    }
                }
            } else {
                last_audio_at = now;
            }
            std::thread::sleep(Duration::from_millis(12));
        }

        drop(sys);
        drop(mic);
        drop(pipe_file);
        if com_ok {
            unsafe { CoUninitialize() };
        }
    }
}

// ============================================================================
// 录屏鼠标光标叠加（不改底层抓屏补丁，支持按开关实时绘制系统真实光标）
// ============================================================================

#[cfg(target_os = "windows")]
mod cursor_overlay {
    use super::*;
    use windows::Win32::{
        Foundation::POINT,
        Graphics::Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection,
            DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
        },
        UI::WindowsAndMessaging::{
            CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo, HCURSOR,
            HICON, ICONINFO,
        },
    };

    struct CachedCursor {
        handle: isize,
        hotspot_x: i32,
        hotspot_y: i32,
        w: u32,
        h: u32,
        rgba: Vec<u8>,
    }

    static CURSOR_CACHE: OnceLock<Mutex<Option<CachedCursor>>> = OnceLock::new();

    /// 在裁剪后的录屏帧上叠加当前 Windows 系统鼠标指针（越界自动裁剪）。
    pub fn draw_cursor_on_frame(img: &mut RgbaImage, crop_origin_px: (i32, i32)) {
        let mut ci = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetCursorInfo(&mut ci) }.is_err()
            || (ci.flags.0 & CURSOR_SHOWING.0) == 0
            || ci.hCursor.is_invalid()
        {
            return;
        }
        let cursor_handle = ci.hCursor.0 as isize;
        let cache = CURSOR_CACHE.get_or_init(|| Mutex::new(None));
        let Ok(mut guard) = cache.lock() else {
            return;
        };
        let need_rebuild = guard.as_ref().is_none_or(|c| c.handle != cursor_handle);
        if need_rebuild {
            *guard = extract_cursor_rgba(ci.hCursor);
        }
        let Some(ref cached) = *guard else {
            return;
        };

        let draw_x = ci.ptScreenPos.x - crop_origin_px.0 - cached.hotspot_x;
        let draw_y = ci.ptScreenPos.y - crop_origin_px.1 - cached.hotspot_y;
        blend_cursor(img, cached, draw_x, draw_y);
    }

    /// 利用黑底 + 白底双次 `DrawIconEx` 提取带精确 Alpha 通道的光标 RGBA 位图。
    fn extract_cursor_rgba(hcursor: HCURSOR) -> Option<CachedCursor> {
        let mut info = ICONINFO::default();
        unsafe { GetIconInfo(HICON(hcursor.0), &mut info) }.ok()?;
        let hotspot_x = info.xHotspot as i32;
        let hotspot_y = info.yHotspot as i32;
        if !info.hbmMask.is_invalid() {
            unsafe {
                let _ = DeleteObject(info.hbmMask.into());
            }
        }
        if !info.hbmColor.is_invalid() {
            unsafe {
                let _ = DeleteObject(info.hbmColor.into());
            }
        }

        let size = 32u32;
        let black = draw_on_solid(hcursor, size, 0x00)?;
        let white = draw_on_solid(hcursor, size, 0xFF)?;

        let mut rgba = vec![0u8; (size * size * 4) as usize];
        for i in 0..(size * size) as usize {
            let idx = i * 4;
            let bb = black[idx] as i32;
            let bg = black[idx + 1] as i32;
            let br = black[idx + 2] as i32;
            let wb = white[idx] as i32;
            let wg = white[idx + 1] as i32;
            let wr = white[idx + 2] as i32;

            // 反色光标（如 I-beam 文本光标在黑底变白、白底变黑）按反相轮廓绘制为高对比黑芯白边。
            if br > wr || bg > wg || bb > wb {
                rgba[idx] = 255;
                rgba[idx + 1] = 255;
                rgba[idx + 2] = 255;
                rgba[idx + 3] = 230;
            } else {
                let alpha = (255 - ((wr - br + wg - bg + wb - bb) / 3)).clamp(0, 255);
                rgba[idx] = br.clamp(0, 255) as u8;
                rgba[idx + 1] = bg.clamp(0, 255) as u8;
                rgba[idx + 2] = bb.clamp(0, 255) as u8;
                rgba[idx + 3] = alpha as u8;
            }
        }

        Some(CachedCursor {
            handle: hcursor.0 as isize,
            hotspot_x,
            hotspot_y,
            w: size,
            h: size,
            rgba,
        })
    }

    fn draw_on_solid(hcursor: HCURSOR, size: u32, fill: u8) -> Option<Vec<u8>> {
        let screen_dc = unsafe { GetDC(None) };
        let mem_dc = unsafe { CreateCompatibleDC(Some(screen_dc)) };
        unsafe {
            ReleaseDC(None, screen_dc);
        }
        if mem_dc.is_invalid() {
            return None;
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size as i32,
                biHeight: -(size as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib =
            unsafe { CreateDIBSection(Some(mem_dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }
                .ok()?;
        let old = unsafe { SelectObject(mem_dc, dib.into()) };
        let byte_len = (size * size * 4) as usize;
        if !bits.is_null() {
            unsafe {
                std::ptr::write_bytes(bits.cast::<u8>(), fill, byte_len);
                let _ = DrawIconEx(
                    mem_dc,
                    0,
                    0,
                    HICON(hcursor.0),
                    size as i32,
                    size as i32,
                    0,
                    None,
                    DI_NORMAL,
                );
            }
        }
        let mut out = vec![0u8; byte_len];
        if !bits.is_null() {
            unsafe {
                std::ptr::copy_nonoverlapping(bits.cast::<u8>(), out.as_mut_ptr(), byte_len);
            }
        }
        unsafe {
            SelectObject(mem_dc, old);
            let _ = DeleteObject(dib.into());
            let _ = DeleteDC(mem_dc);
        }
        // 消除未使用 POINT 导入告警（与光标坐标系对齐留用）。
        let _ = POINT::default();
        Some(out)
    }

    fn blend_cursor(dst: &mut RgbaImage, cursor: &CachedCursor, x0: i32, y0: i32) {
        let (dw, dh) = dst.dimensions();
        for cy in 0..cursor.h as i32 {
            let dy = y0 + cy;
            if dy < 0 || dy >= dh as i32 {
                continue;
            }
            for cx in 0..cursor.w as i32 {
                let dx = x0 + cx;
                if dx < 0 || dx >= dw as i32 {
                    continue;
                }
                let idx = ((cy as u32 * cursor.w + cx as u32) * 4) as usize;
                let sa = cursor.rgba[idx + 3] as u32;
                if sa == 0 {
                    continue;
                }
                let p = dst.get_pixel_mut(dx as u32, dy as u32);
                let inv = 255 - sa;
                p[0] = ((cursor.rgba[idx] as u32 * sa + p[0] as u32 * inv) / 255) as u8;
                p[1] = ((cursor.rgba[idx + 1] as u32 * sa + p[1] as u32 * inv) / 255) as u8;
                p[2] = ((cursor.rgba[idx + 2] as u32 * sa + p[2] as u32 * inv) / 255) as u8;
                p[3] = 255;
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod cursor_overlay {
    pub fn draw_cursor_on_frame(_img: &mut image::RgbaImage, _crop_origin_px: (i32, i32)) {}
}

// ============================================================================
// 录屏期间的红框提示与悬浮控制条（排除出抓屏 + 全局置顶）
// ============================================================================

/// 录制区域外侧的置顶空心红框窗口与悬浮控制条。
fn open_hud_windows(
    spec: &RecordSpec,
    elapsed_ms: &Arc<AtomicU64>,
    hud_windows: &Arc<Mutex<Vec<AnyWindowHandle>>>,
    border_window: &Arc<Mutex<Option<BorderWindowHandle>>>,
    cx: &mut App,
) {
    let origin_x = spec.display_bounds.origin.x.as_f32();
    let origin_y = spec.display_bounds.origin.y.as_f32();
    let (rx, ry, rw, rh) = spec.view_rect;
    let x = origin_x + rx;
    let y = origin_y + ry;

    // 使用原生 Win32 分层窗口（ColorKey 抠黑 + 四边 3px 红线），避免 DirectComposition 裁切失真。
    let (bx, by, bw, bh) = spec.border_px;
    if let Some(handle) = open_border_window(bx, by, bw, bh)
        && let Ok(mut slot) = border_window.lock()
    {
        *slot = Some(handle);
    }

    // 悬浮控制条优先放在选区正下方；若靠近屏幕底部则放到选区正上方。
    let bar_w = 292.0f32;
    let bar_h = 38.0f32;
    let disp_h = spec.display_bounds.size.height.as_f32();
    let disp_w = spec.display_bounds.size.width.as_f32();
    let bar_x = (x + (rw - bar_w) * 0.5).clamp(
        origin_x + 8.0,
        (origin_x + disp_w - bar_w - 8.0).max(origin_x + 8.0),
    );
    let bar_y = if ry + rh + bar_h + 12.0 <= disp_h {
        y + rh + 8.0
    } else {
        (y - bar_h - 8.0).max(origin_y + 8.0)
    };

    if let Ok(handle) = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                point(px(bar_x), px(bar_y)),
                size(px(bar_w), px(bar_h)),
            ))),
            titlebar: None,
            focus: false,
            show: true,
            kind: WindowKind::PopUp,
            is_movable: true,
            is_resizable: false,
            window_background: WindowBackgroundAppearance::Opaque,
            ..Default::default()
        },
        |window, cx| {
            exclude_from_capture(window);
            set_always_on_top(window);
            let elapsed = elapsed_ms.clone();
            cx.new(|cx| RecordControlBar::new(elapsed, cx))
        },
    ) && let Ok(mut list) = hud_windows.lock()
    {
        list.push(handle.into());
    }
}

/// 录制中的悬浮控制条（计时器 + 暂停/继续 + 停止保存）。
struct RecordControlBar {
    elapsed_ms: Arc<AtomicU64>,
    focus_handle: FocusHandle,
}

impl RecordControlBar {
    fn new(elapsed_ms: Arc<AtomicU64>, cx: &mut Context<Self>) -> Self {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let ok = this.update(cx, |_, cx| {
                    cx.notify();
                });
                if ok.is_err() || !is_recording() {
                    break;
                }
            }
        })
        .detach();
        Self {
            elapsed_ms,
            focus_handle: cx.focus_handle(),
        }
    }
}

impl Render for RecordControlBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let total_secs = self.elapsed_ms.load(Ordering::Relaxed) / 1000;
        let mins = total_secs / 60;
        let secs = total_secs % 60;
        let paused = is_paused();
        let timer_text = format!("{mins:02}:{secs:02}");

        div()
            .size_full()
            .flex()
            .items_center()
            .justify_between()
            .px_3()
            .bg(rgb(p.toolbar_bg))
            .border_1()
            .border_color(rgb(p.border))
            .text_color(rgb(p.text))
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|_, ev: &KeyDownEvent, _, _| {
                if ev.keystroke.key.as_str() == "escape" && ev.keystroke.modifiers.shift {
                    stop_active();
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(8.0))
                            .h(px(8.0))
                            .rounded_full()
                            .bg(rgb(if paused { 0xF59F00 } else { 0xE03131 })),
                    )
                    .child(div().text_sm().child(timer_text)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("rec-pause-btn")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(rgb(p.card))
                            .border_1()
                            .border_color(rgb(p.border))
                            .text_xs()
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(p.selected)))
                            .on_click(cx.listener(|_, _, _, cx| {
                                toggle_pause();
                                cx.notify();
                            }))
                            .child(tr(if paused { "rec.resume" } else { "rec.pause" })),
                    )
                    .child(
                        div()
                            .id("rec-stop-btn")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(rgb(0xC92A2A))
                            .text_color(rgb(0xFFFFFF))
                            .text_xs()
                            .cursor_pointer()
                            .hover(|s| s.bg(rgba(0xE03131FF)))
                            .on_click(cx.listener(|_, _, _, _| {
                                stop_active();
                            }))
                            .child(format!("{} (Shift+Esc)", tr("rec.stop"))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    //! 录屏选区几何换算单测：比例锁定、标准尺寸适配与偶数对齐边界。

    use super::*;

    /// 比例预设：中心近似保持、不超出屏幕边界、宽高符合目标比。
    #[test]
    fn ratio_fit_stays_in_bounds() {
        let fitted = fit_rect_to_ratio((100.0, 100.0, 400.0, 400.0), 16.0 / 9.0, (1920.0, 1080.0));
        assert!(fitted.0 >= 0.0 && fitted.1 >= 0.0);
        assert!(fitted.0 + fitted.2 <= 1920.0);
        assert!(fitted.1 + fitted.3 <= 1080.0);
        let ratio = fitted.2 / fitted.3;
        assert!((ratio - 16.0 / 9.0).abs() < 0.02);
    }

    /// 预设尺寸超出屏幕时按比例缩小，不越界。
    #[test]
    fn size_preset_shrinks_on_small_screen() {
        let fitted = fit_rect_to_size(
            (0.0, 0.0, 200.0, 200.0),
            (1920, 1080),
            (1.0, 1.0),
            (1280.0, 720.0),
        );
        assert!(fitted.2 <= 1280.0 + 0.1);
        assert!(fitted.3 <= 720.0 + 0.1);
    }
}
