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

/// 命令行录屏目标（窗口标题/HWND、进程 PID/名称，或显示器物理像素区域）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CliRecordTarget {
    /// 按窗口标题或十进制/十六进制 HWND 选择可见窗口。
    Window(String),
    /// 按进程 PID 或进程名（可省略 `.exe`）选择面积最大的窗口。
    Process(String),
    /// 虚拟屏幕物理像素区域 `(x, y, width, height)`，仅支持单显示器。
    Region {
        /// 左上角横坐标，可为负。
        x: i32,
        /// 左上角纵坐标，可为负。
        y: i32,
        /// 区域宽度。
        w: u32,
        /// 区域高度。
        h: u32,
    },
}

/// 一次命令行录屏请求。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CliRecordRequest {
    /// 捕获目标。
    pub target: CliRecordTarget,
    /// 录制时长，必须大于零。
    pub duration: Duration,
    /// 输出格式。
    pub format: RecordFormat,
    /// MP4 平均视频码率（Mbps）。
    pub bitrate_mbps: u32,
    /// 捕获与编码帧率。
    pub fps: u32,
    /// 显式输出文件或目录；`None` 使用截图默认目录。
    pub output: Option<PathBuf>,
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
    cancel_flag: Arc<AtomicBool>,
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

/// 请求取消当前录屏，停止编码并丢弃本次文件。
pub fn cancel_active() -> bool {
    if let Ok(guard) = active_slot().lock()
        && let Some(session) = guard.as_ref()
    {
        session.cancel_flag.store(true, Ordering::SeqCst);
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

/// 取消录屏：关闭控制条、删除本次文件并只提示取消状态。
fn finish_cancelled(main: &rgpui::Entity<RivixApp>, out_path: &Path, cx: &mut App) {
    cleanup_session(cx);
    let _ = std::fs::remove_file(out_path);
    let msg = tr("rec.cancelled");
    main.update(cx, |state, cx| state.set_status(&msg, cx));
    toast::show(cx, msg);
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
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let elapsed_ms = Arc::new(AtomicU64::new(0));
    let hud_windows = Arc::new(Mutex::new(Vec::new()));
    let border_window = Arc::new(Mutex::new(None));

    {
        let mut guard = active_slot().lock().map_err(|e| e.to_string())?;
        *guard = Some(ActiveSession {
            stop_flag: stop_flag.clone(),
            paused_flag: paused_flag.clone(),
            cancel_flag: cancel_flag.clone(),
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
    let cancel_for_bg = cancel_flag.clone();
    let elapsed_for_bg = elapsed_ms.clone();
    let out_for_bg = out_path.clone();
    let spec_for_bg = spec.clone();

    cx.spawn(async move |cx| {
        let sources = match sources_rx.await {
            Ok(Ok(list)) if !list.is_empty() => list,
            _ => {
                cx.update(|cx| {
                    if cancel_for_bg.load(Ordering::Relaxed) {
                        finish_cancelled(&main, &out_for_bg, cx);
                        return;
                    }
                    cleanup_session(cx);
                    let msg = tr("cap.no_source");
                    main.update(cx, |state, cx| state.set_status(&msg, cx));
                    toast::show(cx, msg);
                });
                return;
            }
        };

        if cancel_for_bg.load(Ordering::Relaxed) {
            cx.update(|cx| finish_cancelled(&main, &out_for_bg, cx));
            return;
        }

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
                    if cancel_for_bg.load(Ordering::Relaxed) {
                        finish_cancelled(&main, &out_for_bg, cx);
                        return;
                    }
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
        let out_for_pipeline = out_for_bg.clone();
        let result = cx
            .background_executor()
            .spawn(async move {
                run_ffmpeg_pipeline(
                    &ffmpeg_path,
                    &spec_for_bg,
                    &out_for_pipeline,
                    frame_rx,
                    &stop_for_bg,
                    &paused_for_bg,
                    &elapsed_for_bg,
                )
            })
            .await;

        drop(_keep_stream);
        cx.update(|cx| {
            if cancel_for_bg.load(Ordering::Relaxed) {
                finish_cancelled(&main, &out_for_bg, cx);
                return;
            }
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

/// Windows 命令行录屏实现：真实窗口/显示器 WGC 帧经 stdin 交给 FFmpeg。
#[cfg(target_os = "windows")]
mod cli_recording {
    use super::*;
    use scap_rs::{
        Display as ScapDisplay, Target, Window as ScapWindow,
        capturer::{Area, Capturer, Options, Point, Resolution, Size},
        frame::{BGRAFrame, Frame, FrameType, RGBxFrame},
    };
    use std::{
        ffi::c_void,
        panic::{AssertUnwindSafe, catch_unwind},
        process::{ChildStdin, Stdio},
        thread::JoinHandle,
    };
    use windows::Win32::{
        Foundation::{HWND, RECT},
        Graphics::{
            Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute},
            Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO},
        },
        System::Threading::GetCurrentProcessId,
        UI::{
            HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI},
            WindowsAndMessaging::IsIconic,
        },
    };
    use windows_capture::{monitor::Monitor, window::Window as CaptureWindow};

    /// 已解析的捕获目标与固定裁剪区。
    struct CapturePlan {
        /// scap 捕获目标。
        target: Target,
        /// 逻辑像素裁剪区；vendor 会按显示器 DPI 转回物理像素。
        crop: Area,
        /// 命令行展示用目标说明。
        label: String,
    }

    /// 显示器物理矩形与对应 scap 目标。
    #[derive(Clone, Debug)]
    struct MonitorInfo {
        /// 显示器矩形 `(left, top, right, bottom)`。
        rect: RECT,
        /// 显示器捕获目标。
        target: Target,
    }

    /// 窗口候选及其排序信息。
    #[derive(Clone, Debug)]
    struct WindowCandidate {
        /// 完整 HWND 对应的窗口对象。
        window: CaptureWindow,
        /// 窗口标题。
        title: String,
        /// 所属进程 PID。
        pid: u32,
        /// 进程映像名，读取失败时为空。
        process: String,
        /// 可见窗口面积。
        area: i64,
    }

    /// 裁剪后的物理像素区域。
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct PhysicalRegion {
        /// 左上角横坐标。
        x: i32,
        /// 左上角纵坐标。
        y: i32,
        /// 偶数宽度。
        w: u32,
        /// 偶数高度。
        h: u32,
    }

    /// 执行一次固定时长的命令行录屏，返回实际保存路径。
    pub(crate) fn run_cli_recording(request: &CliRecordRequest) -> Result<PathBuf, String> {
        let out_path = recording_output_path(request)?;
        let Some(ffmpeg) = find_ffmpeg() else {
            return Err("未找到 ffmpeg.exe（请将其放到 rivix.exe 同目录或系统 PATH）".to_string());
        };
        let plan = resolve_target(&request.target)?;
        println!("录屏目标：{}", plan.label);

        let mut capturer = Capturer::build(Options {
            fps: request.fps,
            show_cursor: false,
            show_highlight: false,
            target: Some(plan.target.clone()),
            crop_area: Some(plan.crop.clone()),
            output_type: FrameType::BGRAFrame,
            output_resolution: Resolution::Captured,
            excluded_targets: None,
        })
        .map_err(|error| format!("创建屏幕捕获器失败：{error}"))?;
        let [width, height] = capturer.get_output_frame_size();
        if width < 2 || height < 2 {
            return Err("目标尺寸过小，无法录屏".to_string());
        }

        let (mut child, mut stdin, stderr_handle) =
            spawn_encoder(&ffmpeg, request, width, height, &out_path)?;
        let (frame_tx, frame_rx) = mpsc::sync_channel::<Result<Vec<u8>, String>>(2);
        let stop = Arc::new(AtomicBool::new(false));
        let capture_stop = stop.clone();
        let capture_width = width as i32;
        let capture_height = height as i32;
        // 捕获回调不能阻塞 FFmpeg 写入，故独立线程取帧；主线程只负责计时与编码。
        std::thread::spawn(move || {
            let started = catch_unwind(AssertUnwindSafe(|| capturer.start_capture()));
            if started.is_err() {
                let _ = frame_tx.send(Err("启动屏幕捕获失败（目标可能不支持 WGC）".to_string()));
                return;
            }
            while !capture_stop.load(Ordering::Relaxed) {
                let frame = match capturer.get_next_frame() {
                    Ok(frame) => frame,
                    Err(error) => {
                        let _ = frame_tx.send(Err(format!("读取屏幕帧失败：{error}")));
                        break;
                    }
                };
                let data = match frame_to_bgra(frame, capture_width, capture_height) {
                    Ok(data) => data,
                    Err(error) => {
                        let _ = frame_tx.send(Err(error));
                        break;
                    }
                };
                if frame_tx.send(Ok(data)).is_err() {
                    break;
                }
            }
            // start_capture 成功后 capture_control 才存在；捕获意外关闭时停止仍安全。
            let _ = catch_unwind(AssertUnwindSafe(|| capturer.stop_capture()));
        });

        let (written_frames, capture_error) =
            pump_frames(frame_rx, &mut stdin, request.duration, request.fps);

        stop.store(true, Ordering::SeqCst);
        drop(stdin);
        let wait_result = child.wait();
        let stderr_text = stderr_handle
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();

        if written_frames == 0 {
            if std::fs::metadata(&out_path).is_ok_and(|meta| meta.len() < 1024) {
                let _ = std::fs::remove_file(&out_path);
            }
            return Err(capture_error.unwrap_or_else(|| "未捕获到视频帧".to_string()));
        }
        if let Some(error) = capture_error {
            return Err(format!("{error}；已保留 {}", out_path.display()));
        }
        let status = wait_result.map_err(|error| format!("等待 FFmpeg 退出失败：{error}"))?;
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
            let error = if tail.is_empty() {
                format!("FFmpeg 异常退出：{status}")
            } else {
                tail
            };
            return Err(format!("{error}；已保留 {}", out_path.display()));
        }
        Ok(out_path)
    }

    /// 按目标帧率输送帧；WGC 静止时只产出首帧，因此没有新帧也要重复最后一帧。
    fn pump_frames<W: std::io::Write>(
        frame_rx: mpsc::Receiver<Result<Vec<u8>, String>>,
        writer: &mut W,
        duration: Duration,
        fps: u32,
    ) -> (u64, Option<String>) {
        let fps = fps.max(1);
        let interval = Duration::from_secs_f64(1.0 / fps as f64);
        // 输出时长由帧数决定，避免写入耗时或帧到达抖动让成品少几帧。
        let target_frames = (duration.as_nanos() * fps as u128)
            .div_ceil(1_000_000_000)
            .min(u64::MAX as u128) as u64;
        if target_frames == 0 {
            return (0, None);
        }

        let first = match frame_rx.recv_timeout(duration) {
            Ok(Ok(data)) => data,
            Ok(Err(error)) => return (0, Some(error)),
            Err(mpsc::RecvTimeoutError::Timeout) => return (0, None),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return (
                    0,
                    Some("屏幕捕获流意外关闭（目标窗口可能已关闭）".to_string()),
                );
            }
        };
        let mut last_frame = Some(first);
        let mut next_frame_at = Instant::now();
        let mut written_frames = 0u64;

        while written_frames < target_frames {
            let now = Instant::now();
            if now >= next_frame_at {
                let Some(frame) = last_frame.as_ref() else {
                    break;
                };
                if let Err(error) = writer.write_all(frame) {
                    return (written_frames, Some(format!("写入 FFmpeg 失败：{error}")));
                }
                written_frames += 1;
                next_frame_at = now + interval;
                continue;
            }
            match frame_rx.recv_timeout(next_frame_at.saturating_duration_since(now)) {
                Ok(Ok(data)) => last_frame = Some(data),
                Ok(Err(error)) => return (written_frames, Some(error)),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return (
                        written_frames,
                        Some("屏幕捕获流意外关闭（目标窗口可能已关闭）".to_string()),
                    );
                }
            }
        }
        (written_frames, None)
    }

    /// 解析目标并固定裁剪尺寸；窗口按完整客户画面，区域按单显示器交集。
    fn resolve_target(target: &CliRecordTarget) -> Result<CapturePlan, String> {
        match target {
            CliRecordTarget::Window(query) => resolve_window_target(query),
            CliRecordTarget::Process(query) => resolve_process_target(query),
            CliRecordTarget::Region { x, y, w, h } => resolve_region_target(PhysicalRegion {
                x: *x,
                y: *y,
                w: *w,
                h: *h,
            }),
        }
    }

    /// 按 HWND 或标题匹配窗口；标题优先精确匹配，其次子串，多个结果取面积最大。
    fn resolve_window_target(query: &str) -> Result<CapturePlan, String> {
        let query = query.trim();
        if query.is_empty() {
            return Err("窗口标题或 HWND 不能为空".to_string());
        }
        if let Some(hwnd) = parse_hwnd(query) {
            let window = CaptureWindow::from_raw_hwnd(hwnd as *mut c_void);
            let candidate = window_candidate(window)
                .ok_or_else(|| "指定窗口不存在、不可见、已最小化或不可捕获".to_string())?;
            return window_plan(candidate);
        }

        let candidates = window_candidates()?;
        let index =
            select_window(&candidates, query).ok_or_else(|| format!("未找到可见窗口：{query}"))?;
        window_plan(candidates[index].clone())
    }

    /// 按 PID 或进程名匹配；多个可见窗口取面积最大。
    fn resolve_process_target(query: &str) -> Result<CapturePlan, String> {
        let query = query.trim();
        if query.is_empty() {
            return Err("进程 PID 或名称不能为空".to_string());
        }
        let candidates = window_candidates()?;
        let pid = query.parse::<u32>().ok();
        let mut best: Option<WindowCandidate> = None;
        for candidate in candidates {
            let matches = match pid {
                Some(pid) => candidate.pid == pid,
                None => process_matches(query, &candidate.process),
            };
            if matches
                && best
                    .as_ref()
                    .is_none_or(|current| candidate.area > current.area)
            {
                best = Some(candidate);
            }
        }
        let candidate = best.ok_or_else(|| format!("未找到带可见窗口的进程：{query}"))?;
        window_plan(candidate)
    }

    /// 将物理区域定位到唯一相交显示器，并转换为 scap 的逻辑像素裁剪区。
    fn resolve_region_target(region: PhysicalRegion) -> Result<CapturePlan, String> {
        let monitors = Monitor::enumerate().map_err(|error| format!("枚举显示器失败：{error}"))?;
        if monitors.is_empty() {
            return Err("未找到可用显示器".to_string());
        }
        let mut infos = Vec::with_capacity(monitors.len());
        for monitor in monitors {
            let raw = monitor.as_raw_hmonitor();
            let Some(rect) = monitor_rect(raw) else {
                continue;
            };
            let width = (rect.right - rect.left) as u32;
            let height = (rect.bottom - rect.top) as u32;
            let Ok(width) = u16::try_from(width) else {
                return Err("显示器宽度超出捕获接口范围".to_string());
            };
            let Ok(height) = u16::try_from(height) else {
                return Err("显示器高度超出捕获接口范围".to_string());
            };
            let title = monitor
                .name()
                .unwrap_or_else(|_| format!("显示器 {}", raw as usize));
            infos.push(MonitorInfo {
                rect,
                target: Target::Display(ScapDisplay {
                    id: raw as u32,
                    title,
                    raw_handle: HMONITOR(raw),
                    width,
                    height,
                }),
            });
        }
        let (index, clipped) = select_monitor_region(region, &infos)?;
        let monitor = &infos[index];
        let scale = monitor_scale(monitor.target.clone());
        let label = format!(
            "区域 {},{},{},{}（{}）",
            clipped.x,
            clipped.y,
            clipped.w,
            clipped.h,
            display_title(&monitor.target)
        );
        Ok(CapturePlan {
            target: monitor.target.clone(),
            crop: Area {
                origin: Point {
                    x: (clipped.x - monitor.rect.left) as f64 / scale,
                    y: (clipped.y - monitor.rect.top) as f64 / scale,
                },
                size: Size {
                    width: clipped.w as f64 / scale,
                    height: clipped.h as f64 / scale,
                },
            },
            label,
        })
    }

    /// 枚举可见、未最小化、未遮蔽且不属于自身的窗口。
    fn window_candidates() -> Result<Vec<WindowCandidate>, String> {
        let candidates = CaptureWindow::enumerate()
            .map_err(|error| format!("枚举窗口失败：{error}"))?
            .into_iter()
            .filter_map(window_candidate)
            .collect();
        Ok(candidates)
    }

    /// 筛选单个窗口并补充 PID、进程名与面积。
    fn window_candidate(window: CaptureWindow) -> Option<WindowCandidate> {
        if !window.is_valid() {
            return None;
        }
        let hwnd = HWND(window.as_raw_hwnd());
        if unsafe { IsIconic(hwnd) }.as_bool() || is_cloaked(hwnd) {
            return None;
        }
        let pid = window.process_id().ok()?;
        if pid == unsafe { GetCurrentProcessId() } {
            return None;
        }
        let rect = window.rect().ok()?;
        let title = window.title().unwrap_or_default();
        if title.trim().is_empty() {
            return None;
        }
        let area = (rect.right as i64 - rect.left as i64)
            .max(0)
            .saturating_mul((rect.bottom as i64 - rect.top as i64).max(0));
        if area < 60 * 60 {
            return None;
        }
        Some(WindowCandidate {
            title,
            process: window.process_name().unwrap_or_default(),
            window,
            pid,
            area,
        })
    }

    /// 为选中窗口构造完整画面裁剪区，并将奇数宽高向下对齐到偶数。
    fn window_plan(candidate: WindowCandidate) -> Result<CapturePlan, String> {
        let rect = candidate
            .window
            .rect()
            .map_err(|error| format!("读取窗口矩形失败：{error}"))?;
        let width = (rect.right - rect.left).max(2) & !1;
        let height = (rect.bottom - rect.top).max(2) & !1;
        let scale = window_scale(candidate.window.as_raw_hwnd());
        let process = if candidate.process.is_empty() {
            String::new()
        } else {
            format!("，{}", candidate.process)
        };
        let label = format!(
            "{}（PID {}{process}，{}x{}）",
            if candidate.title.is_empty() {
                "<无标题窗口>"
            } else {
                &candidate.title
            },
            candidate.pid,
            width,
            height
        );
        Ok(CapturePlan {
            target: Target::Window(ScapWindow {
                id: candidate.window.as_raw_hwnd() as u32,
                title: candidate.title,
                raw_handle: HWND(candidate.window.as_raw_hwnd()),
            }),
            crop: Area {
                origin: Point { x: 0.0, y: 0.0 },
                size: Size {
                    width: width as f64 / scale,
                    height: height as f64 / scale,
                },
            },
            label,
        })
    }

    /// 启动 FFmpeg 原始 BGRA 管道并单独收集错误输出，避免 stderr 缓冲区写满。
    fn spawn_encoder(
        ffmpeg: &Path,
        request: &CliRecordRequest,
        width: u32,
        height: u32,
        out_path: &Path,
    ) -> Result<(Child, ChildStdin, Option<JoinHandle<String>>), String> {
        let mut cmd = hidden_command(ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-n"])
            .args(["-f", "rawvideo"])
            .args(["-pixel_format", "bgra"])
            .args(["-video_size", &format!("{width}x{height}")])
            .args(["-framerate", &request.fps.to_string()])
            .args(["-i", "pipe:0"])
            .arg("-an");
        match request.format {
            RecordFormat::Mp4 => {
                cmd.args(["-c:v", "libx264", "-preset", "veryfast"])
                    .args(["-b:v", &format!("{}M", request.bitrate_mbps)])
                    .args(["-pix_fmt", "yuv420p"])
                    // 分段写入 moov，Ctrl+C 等异常退出时已录片段仍可播放。
                    .args(["-movflags", "+frag_keyframe+empty_moov+default_base_moof"]);
            }
            RecordFormat::Gif => {
                cmd.args([
                    "-vf",
                    "split[s0][s1];[s0]palettegen=stats_mode=diff[p];[s1][p]paletteuse=dither=bayer",
                    "-loop",
                    "0",
                ]);
            }
        }
        let mut child = cmd
            .arg(out_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("启动 FFmpeg 失败：{error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "无法打开 FFmpeg 标准输入".to_string())?;
        let stderr_handle = child.stderr.take().map(|mut stderr| {
            std::thread::spawn(move || {
                let mut text = String::new();
                let _ = stderr.read_to_string(&mut text);
                text
            })
        });
        Ok((child, stdin, stderr_handle))
    }

    /// 将 scap 帧统一为无 padding 的 BGRA，并校验录制期间尺寸未变化。
    fn frame_to_bgra(frame: Frame, width: i32, height: i32) -> Result<Vec<u8>, String> {
        let data = match frame {
            Frame::BGRA(BGRAFrame {
                width: frame_width,
                height: frame_height,
                data,
                ..
            }) => {
                validate_frame_size(frame_width, frame_height, data.len(), width, height)?;
                data
            }
            Frame::RGBx(RGBxFrame {
                width: frame_width,
                height: frame_height,
                mut data,
                ..
            }) => {
                validate_frame_size(frame_width, frame_height, data.len(), width, height)?;
                // vendor 对 Rgba8 源仍返回 RGBx，这里补回 FFmpeg 需要的 BGR 顺序。
                let (pixels, _) = data.as_chunks_mut::<4>();
                for pixel in pixels {
                    pixel.swap(0, 2);
                }
                data
            }
            _ => return Err("屏幕捕获器返回了不支持的帧格式".to_string()),
        };
        if data.len() != width as usize * height as usize * 4 {
            return Err("视频帧大小与编码尺寸不一致".to_string());
        }
        Ok(data)
    }

    /// 校验帧尺寸；窗口缩放后固定裁剪已失效，明确报错比写入错位像素安全。
    fn validate_frame_size(
        frame_width: i32,
        frame_height: i32,
        data_len: usize,
        width: i32,
        height: i32,
    ) -> Result<(), String> {
        if frame_width != width || frame_height != height {
            return Err("录制期间目标尺寸发生变化，请固定窗口大小后重试".to_string());
        }
        if data_len != width as usize * height as usize * 4 {
            return Err("视频帧大小与编码尺寸不一致".to_string());
        }
        Ok(())
    }

    /// 生成 CLI 录屏输出路径；与截图一致，显式目录自动补文件名，但绝不覆盖旧文件。
    fn recording_output_path(request: &CliRecordRequest) -> Result<PathBuf, String> {
        let mut path = match request.output.as_deref() {
            Some(dir) if dir.is_dir() => dir.join(recording_name(request.format)),
            Some(file) => file.to_path_buf(),
            None => load_save_dir().join(recording_name(request.format)),
        };
        if path.extension().is_none() {
            path.set_extension(request.format.as_str());
        }
        if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| !extension.eq_ignore_ascii_case(request.format.as_str()))
        {
            return Err(format!(
                "输出扩展名与 --format {} 不一致",
                request.format.as_str()
            ));
        }
        if path.exists() {
            return Err(format!("输出文件已存在：{}", path.display()));
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("创建输出目录失败（{}）：{error}", parent.display()))?;
        }
        Ok(path)
    }

    /// 生成默认录屏文件名。
    fn recording_name(format: RecordFormat) -> String {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0);
        format!("rivix-{millis}.{}", format.as_str())
    }

    /// 读取显示器物理矩形。
    fn monitor_rect(raw: *mut c_void) -> Option<RECT> {
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..MONITORINFO::default()
        };
        if !unsafe { GetMonitorInfoW(HMONITOR(raw), (&raw mut info).cast()) }.as_bool() {
            return None;
        }
        Some(info.rcMonitor)
    }

    /// 读取显示器水平 DPI 缩放比。
    fn monitor_scale(target: Target) -> f64 {
        let Target::Display(display) = target else {
            return 1.0;
        };
        let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
        let ok = unsafe {
            GetDpiForMonitor(
                display.raw_handle,
                MDT_EFFECTIVE_DPI,
                &mut dpi_x,
                &mut dpi_y,
            )
            .is_ok()
        };
        if ok && dpi_x > 0 {
            dpi_x as f64 / 96.0
        } else {
            1.0
        }
    }

    /// 读取窗口水平 DPI 缩放比。
    fn window_scale(raw: *mut c_void) -> f64 {
        let dpi = unsafe { GetDpiForWindow(HWND(raw)) };
        if dpi > 0 { dpi as f64 / 96.0 } else { 1.0 }
    }

    /// 判断窗口是否被 DWM 遮蔽。
    fn is_cloaked(hwnd: HWND) -> bool {
        let mut cloaked = 0u32;
        unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                &mut cloaked as *mut u32 as *mut c_void,
                std::mem::size_of::<u32>() as u32,
            )
        }
        .is_ok_and(|()| cloaked != 0)
    }

    /// 选择唯一相交显示器并裁剪越界部分；跨屏区域直接报错。
    fn select_monitor_region(
        region: PhysicalRegion,
        monitors: &[MonitorInfo],
    ) -> Result<(usize, PhysicalRegion), String> {
        let mut matches = Vec::new();
        for (index, monitor) in monitors.iter().enumerate() {
            let rect = monitor.rect;
            let x0 = (region.x as i64).max(rect.left as i64);
            let y0 = (region.y as i64).max(rect.top as i64);
            let x1 = (region.x as i64 + region.w as i64).min(rect.right as i64);
            let y1 = (region.y as i64 + region.h as i64).min(rect.bottom as i64);
            if x1 > x0 && y1 > y0 {
                matches.push((index, x0, y0, x1, y1));
            }
        }
        if matches.is_empty() {
            return Err("区域与任何显示器均无交集".to_string());
        }
        if matches.len() > 1 {
            return Err("区域跨越多个显示器，命令行录屏仅支持单显示器区域".to_string());
        }
        let (index, x0, y0, x1, y1) = matches[0];
        let w = u32::try_from(x1 - x0).map_err(|_| "区域宽度无效".to_string())? & !1;
        let h = u32::try_from(y1 - y0).map_err(|_| "区域高度无效".to_string())? & !1;
        if w < 2 || h < 2 {
            return Err("裁剪后的区域至少需要 2x2 物理像素".to_string());
        }
        Ok((
            index,
            PhysicalRegion {
                x: x0 as i32,
                y: y0 as i32,
                w,
                h,
            },
        ))
    }

    /// 标题匹配：精确优先于子串，同级按面积取最大。
    fn select_window(candidates: &[WindowCandidate], query: &str) -> Option<usize> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return None;
        }
        let mut best: Option<(usize, bool)> = None;
        for (index, candidate) in candidates.iter().enumerate() {
            let title = candidate.title.to_lowercase();
            let exact = title == query;
            if !exact && !title.contains(&query) {
                continue;
            }
            let replace = best.is_none_or(|(best_index, best_exact)| {
                (exact && !best_exact)
                    || (exact == best_exact && candidate.area > candidates[best_index].area)
            });
            if replace {
                best = Some((index, exact));
            }
        }
        best.map(|(index, _)| index)
    }

    /// 进程名匹配允许省略 `.exe`，忽略大小写。
    fn process_matches(query: &str, process: &str) -> bool {
        let query = query.trim().to_lowercase();
        let process = process.trim().to_lowercase();
        if query.is_empty() || process.is_empty() {
            return false;
        }
        let query_stem = query.strip_suffix(".exe").unwrap_or(&query);
        let process_stem = process.strip_suffix(".exe").unwrap_or(&process);
        query_stem == process_stem
    }

    /// 解析十进制或 `0x` 前缀的 HWND。
    fn parse_hwnd(value: &str) -> Option<isize> {
        if let Some(hex) = value
            .strip_prefix("0x")
            .or_else(|| value.strip_prefix("0X"))
        {
            u64::from_str_radix(hex, 16).ok()?.try_into().ok()
        } else {
            value.parse().ok()
        }
    }

    /// 获取显示器展示名。
    fn display_title(target: &Target) -> String {
        match target {
            Target::Display(display) => display.title.clone(),
            Target::Window(window) => window.title.clone(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// 构造不访问系统的窗口候选，供匹配逻辑单测。
        fn candidate(title: &str, process: &str, pid: u32, area: i64) -> WindowCandidate {
            WindowCandidate {
                window: CaptureWindow::from_raw_hwnd(std::ptr::null_mut()),
                title: title.to_string(),
                process: process.to_string(),
                pid,
                area,
            }
        }

        /// 静止画面只有一帧时，仍按目标帧率补齐到请求时长。
        #[test]
        fn static_frame_is_repeated_for_requested_duration() {
            let (tx, rx) = mpsc::sync_channel(1);
            tx.send(Ok(vec![7; 8])).expect("发送首帧");
            let mut output = Vec::new();
            let started = Instant::now();
            let (frames, error) = pump_frames(rx, &mut output, Duration::from_millis(200), 20);
            drop(tx);

            assert!(error.is_none(), "静止帧补齐不应报错：{error:?}");
            assert_eq!(frames, 4, "200ms / 20fps 应恰好写出 4 帧");
            assert_eq!(output.len(), frames as usize * 8);
            assert!(output.iter().all(|byte| *byte == 7));
            assert!(started.elapsed() >= Duration::from_millis(140));
        }

        /// 精确标题必须胜过面积更大的子串匹配，子串内部再按面积选择。
        #[test]
        fn window_match_prefers_exact_then_area() {
            let candidates = vec![
                candidate("Notes - Large", "notepad.exe", 10, 900),
                candidate("Notes", "notepad.exe", 10, 100),
                candidate("Other", "other.exe", 11, 1000),
            ];
            assert_eq!(select_window(&candidates, "notes"), Some(1));
            assert_eq!(select_window(&candidates, "note"), Some(0));
            assert_eq!(select_window(&candidates, "missing"), None);
        }

        /// 进程名忽略大小写与 `.exe` 后缀。
        #[test]
        fn process_name_match_accepts_stem() {
            assert!(process_matches("Code", "Code.exe"));
            assert!(process_matches("CODE.EXE", "code.exe"));
            assert!(!process_matches("Code", "CodeInsiders.exe"));
            assert!(!process_matches("", "Code.exe"));
        }

        /// 负坐标区域可落在左侧显示器，越界部分按交集钳制并对齐偶数。
        #[test]
        fn region_uses_negative_monitor_origin() {
            let monitors = vec![
                MonitorInfo {
                    rect: RECT {
                        left: 0,
                        top: 0,
                        right: 1920,
                        bottom: 1080,
                    },
                    target: Target::Display(ScapDisplay {
                        id: 1,
                        title: "主屏".to_string(),
                        raw_handle: HMONITOR(std::ptr::null_mut()),
                        width: 1920,
                        height: 1080,
                    }),
                },
                MonitorInfo {
                    rect: RECT {
                        left: -1280,
                        top: 0,
                        right: 0,
                        bottom: 1024,
                    },
                    target: Target::Display(ScapDisplay {
                        id: 2,
                        title: "左屏".to_string(),
                        raw_handle: HMONITOR(std::ptr::null_mut()),
                        width: 1280,
                        height: 1024,
                    }),
                },
            ];
            let (index, clipped) = select_monitor_region(
                PhysicalRegion {
                    x: -1300,
                    y: 10,
                    w: 101,
                    h: 51,
                },
                &monitors,
            )
            .expect("负坐标区域应命中左屏");
            assert_eq!(index, 1);
            assert_eq!(
                clipped,
                PhysicalRegion {
                    x: -1280,
                    y: 10,
                    w: 80,
                    h: 50,
                }
            );
        }

        /// 跨屏与完全屏外区域必须拒绝，避免静默漏录。
        #[test]
        fn region_rejects_cross_monitor_and_offscreen() {
            let monitors = vec![MonitorInfo {
                rect: RECT {
                    left: 0,
                    top: 0,
                    right: 100,
                    bottom: 100,
                },
                target: Target::Display(ScapDisplay {
                    id: 1,
                    title: "测试屏".to_string(),
                    raw_handle: HMONITOR(std::ptr::null_mut()),
                    width: 100,
                    height: 100,
                }),
            }];
            assert!(
                select_monitor_region(
                    PhysicalRegion {
                        x: 200,
                        y: 0,
                        w: 10,
                        h: 10,
                    },
                    &monitors,
                )
                .is_err()
            );
            let mut cross = monitors.clone();
            cross.push(MonitorInfo {
                rect: RECT {
                    left: 100,
                    top: 0,
                    right: 200,
                    bottom: 100,
                },
                target: Target::Display(ScapDisplay {
                    id: 2,
                    title: "右屏".to_string(),
                    raw_handle: HMONITOR(std::ptr::null_mut()),
                    width: 100,
                    height: 100,
                }),
            });
            assert!(
                select_monitor_region(
                    PhysicalRegion {
                        x: 50,
                        y: 0,
                        w: 100,
                        h: 10,
                    },
                    &cross,
                )
                .is_err()
            );
        }

        /// HWND 支持十进制与十六进制。
        #[test]
        fn hwnd_parses_decimal_and_hex() {
            assert_eq!(parse_hwnd("4660"), Some(4660));
            assert_eq!(parse_hwnd("0x1234"), Some(0x1234));
            assert_eq!(parse_hwnd("not-a-handle"), None);
        }

        /// BGRA 原样传递，RGBx 交换红蓝通道，尺寸不符则拒绝。
        #[test]
        fn frame_conversion_validates_and_swaps_rgbx() {
            let bgra = Frame::BGRA(BGRAFrame {
                display_time: 0,
                width: 1,
                height: 1,
                data: vec![1, 2, 3, 255],
            });
            assert_eq!(
                frame_to_bgra(bgra, 1, 1).expect("合法 BGRA"),
                vec![1, 2, 3, 255]
            );
            let rgbx = Frame::RGBx(RGBxFrame {
                display_time: 0,
                width: 1,
                height: 1,
                data: vec![3, 2, 1, 255],
            });
            assert_eq!(
                frame_to_bgra(rgbx, 1, 1).expect("合法 RGBx"),
                vec![1, 2, 3, 255]
            );
            let wrong = Frame::BGRA(BGRAFrame {
                display_time: 0,
                width: 2,
                height: 1,
                data: vec![0; 8],
            });
            assert!(frame_to_bgra(wrong, 1, 1).is_err());
        }
    }
}

/// Windows 命令行录屏入口。
#[cfg(target_os = "windows")]
pub(crate) use cli_recording::run_cli_recording;

/// 非 Windows 平台明确报错，避免解析成功后静默退出。
#[cfg(not(target_os = "windows"))]
pub(crate) fn run_cli_recording(_request: &CliRecordRequest) -> Result<PathBuf, String> {
    Err("当前平台暂不支持命令行录屏".to_string())
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
    let bar_h = 38.0f32;
    let disp_h = spec.display_bounds.size.height.as_f32();
    let disp_w = spec.display_bounds.size.width.as_f32();
    let bar_w = 356.0f32.min((disp_w - 16.0).max(1.0));
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

/// 录制中的悬浮控制条（计时器 + 取消 + 暂停/继续 + 停止保存）。
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
                            .id("rec-cancel-btn")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(rgb(p.card))
                            .border_1()
                            .border_color(rgb(p.border))
                            .text_xs()
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(p.selected)))
                            .on_click(cx.listener(|_, _, _, _| {
                                cancel_active();
                            }))
                            .child(tr("rec.cancel")),
                    )
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

    /// 取消必须同时置位停止与丢弃标记，避免编码继续写出成品。
    #[test]
    fn cancel_active_marks_session_cancelled_and_stopped() {
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut guard = active_slot().lock().expect("获取录屏会话锁");
            *guard = Some(ActiveSession {
                stop_flag: stop.clone(),
                paused_flag: Arc::new(AtomicBool::new(false)),
                cancel_flag: cancel.clone(),
                hud_windows: Arc::new(Mutex::new(Vec::new())),
                border_window: Arc::new(Mutex::new(None)),
            });
        }

        assert!(cancel_active());
        assert!(stop.load(Ordering::SeqCst));
        assert!(cancel.load(Ordering::SeqCst));

        let mut guard = active_slot().lock().expect("获取录屏会话锁");
        *guard = None;
    }

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
