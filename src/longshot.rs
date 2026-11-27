//! 长截图模块：保留单屏推流，把向下滚动的新内容纵向拼接为一张图片。
//!
//! 选区浮层关闭后，用户手动滚动目标页面；Rivix 只追加匹配到的新底部行。
//! 悬浮控制条与临时全局热键提供完成 / 取消，最终复用普通截图的复制与编辑收尾。

use std::{
    rc::Rc,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use image::RgbaImage;
use rgpui::{
    AnyWindowHandle, App, AppContext, Bounds, Context, DisplayId, Entity, FocusHandle,
    KeyDownEvent, Pixels, Render, ScreenCaptureFrame, ScreenCaptureSource, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, point, prelude::*,
    px, rgb, size,
};

use crate::{
    app::{RivixApp, finish_capture, finish_saved_capture, palette, prepare_capture, save_capture},
    capture::cap_log,
    i18n::{tr, tr_fmt},
    window::{
        BorderWindowHandle, close_border_window, exclude_from_capture, open_border_window,
        set_always_on_top,
    },
};

/// 长截图完成热键编号（Ctrl+Enter）。
pub const FINISH_HOTKEY_ID: u32 = 6;
/// 长截图取消热键编号（Shift+Esc）。
pub const CANCEL_HOTKEY_ID: u32 = 7;
/// 单张长截图的最大像素数，约 512MB RGBA，避免无限滚动耗尽内存。
const MAX_PIXELS: u64 = 128_000_000;
/// 抓帧回调最小间隔，约 30fps，避免屏幕流全速转换占用 CPU。
const FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// 一次长截图的选区与显示器信息。
pub struct LongSpec {
    /// 目标显示器 ID。
    pub display_id: Option<DisplayId>,
    /// 显示器逻辑边界（用于放置悬浮控制条）。
    pub display_bounds: Bounds<Pixels>,
    /// 选区逻辑矩形 `(x, y, w, h)`。
    pub view_rect: (f32, f32, f32, f32),
    /// 选区物理像素矩形 `(x, y, w, h)`。
    pub crop_px: (u32, u32, u32, u32),
    /// 红框在桌面全局物理坐标系中的外框矩形 `(x, y, width, height)`。
    pub border_px: (i32, i32, i32, i32),
    /// 浮层冻结帧中裁出的首屏图片。
    pub initial: RgbaImage,
}

/// 正在进行的长截图共享控制状态。
struct ActiveSession {
    /// 请求完成。
    stop: Arc<AtomicBool>,
    /// 请求取消。
    cancel: Arc<AtomicBool>,
    /// 保存按钮要求写入配置的保存目录，快捷键完成仍沿用普通截图收尾。
    save: Arc<AtomicBool>,
    /// 悬浮控制条句柄。
    hud_windows: Arc<Mutex<Vec<AnyWindowHandle>>>,
    /// 选区红框句柄。
    border_window: Arc<Mutex<Option<BorderWindowHandle>>>,
}

static ACTIVE: OnceLock<Mutex<Option<ActiveSession>>> = OnceLock::new();

fn active_slot() -> &'static Mutex<Option<ActiveSession>> {
    ACTIVE.get_or_init(|| Mutex::new(None))
}

/// 当前是否正在进行长截图。
pub fn is_active() -> bool {
    active_slot()
        .lock()
        .map(|guard| guard.is_some())
        .unwrap_or(false)
}

/// 请求完成当前长截图。
pub fn finish_active() -> bool {
    if let Ok(guard) = active_slot().lock()
        && let Some(session) = guard.as_ref()
    {
        session.stop.store(true, Ordering::SeqCst);
        cap_log("长截图请求完成");
        return true;
    }
    false
}

/// 请求结束长截图并保存 PNG 到设置的保存目录。
pub fn save_active() -> bool {
    if let Ok(guard) = active_slot().lock()
        && let Some(session) = guard.as_ref()
    {
        session.save.store(true, Ordering::SeqCst);
        session.stop.store(true, Ordering::SeqCst);
        return true;
    }
    false
}

/// 请求取消当前长截图。
pub fn cancel_active() -> bool {
    if let Ok(guard) = active_slot().lock()
        && let Some(session) = guard.as_ref()
    {
        session.cancel.store(true, Ordering::SeqCst);
        session.stop.store(true, Ordering::SeqCst);
        cap_log("长截图请求取消");
        return true;
    }
    false
}

/// 启动长截图会话：打开排除抓屏的控制条，并异步保持选中屏幕的捕获流。
///
/// # 参数
/// - `spec`: 选区、显示器与首屏图片。
/// - `main`: 主视图实体（最终回写预览与状态）。
/// - `cx`: 主视图上下文。
pub fn start(
    spec: LongSpec,
    main: Entity<RivixApp>,
    cx: &mut Context<RivixApp>,
) -> Result<(), String> {
    {
        let mut guard = active_slot().lock().map_err(|error| error.to_string())?;
        if guard.is_some() {
            return Err("已有一轮长截图正在进行".to_string());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let save = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(spec.initial.height() as u64));
        let hud_windows = Arc::new(Mutex::new(Vec::new()));
        let border_window = Arc::new(Mutex::new(None));
        register_hotkeys(cx)?;
        if let Err(error) = open_hud(&spec, &progress, &hud_windows, &border_window, cx) {
            cx.unregister_global_hotkey(FINISH_HOTKEY_ID);
            cx.unregister_global_hotkey(CANCEL_HOTKEY_ID);
            return Err(error);
        }
        *guard = Some(ActiveSession {
            stop: stop.clone(),
            cancel: cancel.clone(),
            save: save.clone(),
            hud_windows: hud_windows.clone(),
            border_window: border_window.clone(),
        });
        cap_log(&format!(
            "长截图会话启动：选区 {}x{}，显示器 {:?}",
            spec.crop_px.2, spec.crop_px.3, spec.display_id
        ));
        let primary_id = cx.primary_display().map(|display| display.id());
        let sources_rx = cx.screen_capture_sources();
        cx.spawn(async move |_, cx| {
            run_session(
                spec, main, sources_rx, primary_id, stop, cancel, save, progress, cx,
            )
            .await;
        })
        .detach();
    }
    Ok(())
}

/// 注册长截图临时完成 / 取消热键；任一失败都回滚，避免会话没有可靠退出入口。
fn register_hotkeys(cx: &mut Context<RivixApp>) -> Result<(), String> {
    for (id, text) in [
        (FINISH_HOTKEY_ID, "ctrl-enter"),
        (CANCEL_HOTKEY_ID, "shift-escape"),
    ] {
        let key = rgpui::Keystroke::parse(text)
            .map_err(|error| format!("长截图快捷键 {text} 解析失败：{error}"))?;
        cx.unregister_global_hotkey(id);
        if let Err(error) = cx.register_global_hotkey(id, &key) {
            cx.unregister_global_hotkey(FINISH_HOTKEY_ID);
            cx.unregister_global_hotkey(CANCEL_HOTKEY_ID);
            return Err(format!("长截图快捷键 {text} 注册失败：{error}"));
        }
    }
    Ok(())
}

/// 关闭控制条与红框，并注销临时热键。
fn cleanup_session(cx: &mut App) {
    let (handles, border) = if let Ok(mut guard) = active_slot().lock() {
        let taken = guard.take();
        let handles = taken
            .as_ref()
            .and_then(|session| session.hud_windows.lock().ok().map(|list| list.clone()))
            .unwrap_or_default();
        let border = taken.and_then(|session| {
            session
                .border_window
                .lock()
                .ok()
                .and_then(|mut slot| slot.take())
        });
        (handles, border)
    } else {
        (Vec::new(), None)
    };
    if let Some(border) = border {
        close_border_window(border);
    }
    for handle in handles {
        let _ = cx.update_window(handle, |_, window, _| window.remove_window());
    }
    cx.unregister_global_hotkey(FINISH_HOTKEY_ID);
    cx.unregister_global_hotkey(CANCEL_HOTKEY_ID);
    cx.refresh_windows();
}

/// 长截图主任务：持续抓帧、后台拼接，结束后编码并走普通截图收尾。
#[allow(clippy::too_many_arguments)]
async fn run_session<E>(
    spec: LongSpec,
    main: Entity<RivixApp>,
    sources_rx: impl IntoFuture<
        Output = Result<Result<Vec<Rc<dyn ScreenCaptureSource>>, anyhow::Error>, E>,
    >,
    primary_id: Option<DisplayId>,
    stop: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    save: Arc<AtomicBool>,
    progress: Arc<AtomicU64>,
    cx: &mut rgpui::AsyncApp,
) {
    let sources = match sources_rx.await {
        Ok(Ok(sources)) if !sources.is_empty() => sources,
        _ => {
            fail_session(&main, tr("cap.no_source"), cx).await;
            return;
        }
    };
    if cancel.load(Ordering::Relaxed) {
        finish_cancelled(&main, cx).await;
        return;
    }
    let source = crate::record::pick_source_for_display(&sources, spec.display_id, primary_id);
    // 保留有界的连续帧，不能在拼接期间覆盖中间画面，否则后续可能再无重叠。
    let (frame_tx, frame_rx) = mpsc::sync_channel::<RgbaImage>(8);
    let callback_stop = stop.clone();
    let last_sent = Arc::new(Mutex::new(None::<Instant>));
    let last_sent_cb = last_sent.clone();
    let first_frame_logged_cb = Arc::new(AtomicBool::new(false));
    let crop = spec.crop_px;
    let callback: Box<dyn Fn(ScreenCaptureFrame) + Send> =
        Box::new(move |frame: ScreenCaptureFrame| {
            if callback_stop.load(Ordering::Relaxed) {
                return;
            }
            let now = Instant::now();
            if let Ok(guard) = last_sent_cb.lock()
                && let Some(previous) = *guard
                && now.duration_since(previous) < FRAME_INTERVAL
            {
                return;
            }
            if let Some(rgba) = frame.to_rgba() {
                if first_frame_logged_cb
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    cap_log(&format!(
                        "长截图实时帧已到达：{}x{}",
                        rgba.width(),
                        rgba.height()
                    ));
                }
                if let Ok(mut guard) = last_sent_cb.lock() {
                    *guard = Some(now);
                }
                let cropped = crate::record::crop_rgba_clamped(&rgba, crop);
                // 队列满时背压等待，保持帧序；通道关闭会立即退出，避免无限积压。
                let _ = frame_tx.send(cropped);
            }
        });
    let foreground = cx.foreground_executor().clone();
    let stream = match source.stream(&foreground, callback).await {
        Ok(Ok(stream)) => stream,
        _ => {
            fail_session(&main, tr("cap.stream_fail"), cx).await;
            return;
        }
    };
    let stitch_stop = stop.clone();
    cap_log("长截图捕获流已启动");
    let stitch = cx
        .background_executor()
        .spawn(async move {
            let mut stitcher = LongStitcher::new(spec.initial)?;
            loop {
                match frame_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(frame) => {
                        match stitcher.push(&frame) {
                            StitchOutcome::Appended => {
                                progress.store(stitcher.height() as u64, Ordering::Relaxed);
                                cap_log(&format!("长截图拼接追加：高度 {}", stitcher.height()));
                            }
                            StitchOutcome::Limit => {
                                cap_log("长截图已达到像素上限，提前结束");
                                stitch_stop.store(true, Ordering::Relaxed);
                                // 达到内存边界仍交付已拼接的内容，避免整次截图丢失。
                                return Ok(stitcher);
                            }
                            StitchOutcome::Ignored => {}
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        // 完成时先消费队列中的最后一帧，再等待捕获回调真正停止。
                        if stitch_stop.load(Ordering::Relaxed) {
                            return Ok(stitcher);
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err("屏幕捕获流已断开".to_string());
                    }
                }
            }
        })
        .await;
    drop(stream);
    if cancel.load(Ordering::Relaxed) {
        finish_cancelled(&main, cx).await;
        return;
    }
    let stitcher = match stitch {
        Ok(stitcher) => stitcher,
        Err(error) => {
            fail_session(&main, tr_fmt("long.failed", &[("error", &error)]), cx).await;
            return;
        }
    };
    let limited = stitcher.limited;
    let image = match stitcher.finish() {
        Ok(image) => image,
        Err(error) => {
            fail_session(&main, tr_fmt("long.failed", &[("error", &error)]), cx).await;
            return;
        }
    };
    let save_requested = save.load(Ordering::SeqCst);
    let prepared = cx
        .background_executor()
        .spawn(async move {
            if save_requested {
                save_capture(&image)
            } else {
                prepare_capture(&image)
            }
        })
        .await;
    cx.update(|cx| {
        cleanup_session(cx);
        main.update(cx, |state, cx| {
            state.capturing = false;
            match prepared {
                Ok(capture) => {
                    if save_requested {
                        finish_saved_capture(state, cx, capture);
                    } else {
                        let (preview, note) = finish_capture(state, cx, capture);
                        state.set_status(
                            &tr_fmt(
                                "long.done",
                                &[
                                    ("w", &preview.w.to_string()),
                                    ("h", &preview.h.to_string()),
                                    ("note", &note),
                                ],
                            ),
                            cx,
                        );
                    }
                    if limited {
                        let message = "长截图已达到容量边界，已保留捕获内容；后续内容请分段截取";
                        state.set_status(message, cx);
                        crate::toast::show(cx, message.to_string());
                    }
                }
                Err(error) => state.set_status(&tr_fmt("long.failed", &[("error", &error)]), cx),
            }
        });
    });
}

/// 取消后统一复位主状态并清理会话。
async fn finish_cancelled(main: &Entity<RivixApp>, cx: &mut rgpui::AsyncApp) {
    cx.update(|cx| {
        cleanup_session(cx);
        main.update(cx, |state, cx| {
            state.capturing = false;
            state.set_status(&tr("cap.cancelled"), cx);
        });
    });
}

/// 失败后统一复位主状态并展示错误。
async fn fail_session(main: &Entity<RivixApp>, message: String, cx: &mut rgpui::AsyncApp) {
    cx.update(|cx| {
        cleanup_session(cx);
        main.update(cx, |state, cx| {
            state.capturing = false;
            state.set_status(&message, cx);
            crate::toast::show(cx, message.to_string());
        });
    });
}

/// 打开与录屏同款的排除抓屏红框和非抢焦点控制条。
fn open_hud(
    spec: &LongSpec,
    progress: &Arc<AtomicU64>,
    hud_windows: &Arc<Mutex<Vec<AnyWindowHandle>>>,
    border_window: &Arc<Mutex<Option<BorderWindowHandle>>>,
    cx: &mut Context<RivixApp>,
) -> Result<(), String> {
    let (bx, by, bw, bh) = spec.border_px;
    if let Some(handle) = open_border_window(bx, by, bw, bh)
        && let Ok(mut slot) = border_window.lock()
    {
        *slot = Some(handle);
    }
    let origin_x = spec.display_bounds.origin.x.as_f32();
    let origin_y = spec.display_bounds.origin.y.as_f32();
    let (rx, ry, rw, _) = spec.view_rect;
    let bar_w = 320.0_f32.min((spec.display_bounds.size.width.as_f32() - 16.0).max(1.0));
    let bar_h = 42.0;
    let display_w = spec.display_bounds.size.width.as_f32();
    let display_h = spec.display_bounds.size.height.as_f32();
    let x = origin_x + rx;
    let y = origin_y + ry;
    let bar_x = (x + (rw - bar_w) * 0.5).clamp(
        origin_x + 8.0,
        (origin_x + display_w - bar_w - 8.0).max(origin_x + 8.0),
    );
    let bar_y = if ry + spec.view_rect.3 + bar_h + 12.0 <= display_h {
        y + spec.view_rect.3 + 8.0
    } else {
        (y - bar_h - 8.0).max(origin_y + 8.0)
    };
    let progress = progress.clone();
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                point(px(bar_x), px(bar_y)),
                size(px(bar_w), px(bar_h)),
            ))),
            titlebar: None,
            focus: false,
            show: true,
            // 使用原生置顶 Overlay，确保切回滚动目标窗口后按钮仍可见。
            kind: WindowKind::Overlay,
            is_movable: true,
            is_resizable: false,
            window_background: WindowBackgroundAppearance::Opaque,
            display_id: spec.display_id,
            ..Default::default()
        },
        move |window, cx| {
            exclude_from_capture(window);
            set_always_on_top(window);
            // 首帧布局后再置顶，避免窗口初始化时的层级调整覆盖控制条。
            window.on_next_frame(|window, _| set_always_on_top(window));
            cx.new(|cx| LongControlBar::new(progress, spec.crop_px.2, cx))
        },
    );
    let handle = opened.map_err(|error| {
        if let Ok(mut slot) = border_window.lock()
            && let Some(border) = slot.take()
        {
            close_border_window(border);
        }
        format!("长截图控制条创建失败：{error}")
    })?;
    if let Ok(mut list) = hud_windows.lock() {
        list.push(handle.into());
    }
    Ok(())
}

/// 长截图悬浮控制条：显示当前高度，提供完成与取消按钮。
struct LongControlBar {
    /// 当前拼接高度。
    progress: Arc<AtomicU64>,
    /// 选区宽度（物理像素）。
    width: u32,
    /// 根视图焦点句柄。
    focus_handle: FocusHandle,
}

impl LongControlBar {
    /// 创建控制条并启动低频进度刷新。
    fn new(progress: Arc<AtomicU64>, width: u32, cx: &mut Context<Self>) -> Self {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                if this.update(cx, |_, cx| cx.notify()).is_err() || !is_active() {
                    break;
                }
            }
        })
        .detach();
        Self {
            progress,
            width,
            focus_handle: cx.focus_handle(),
        }
    }
}

impl Render for LongControlBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let height = self.progress.load(Ordering::Relaxed);
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
            .on_key_down(cx.listener(|_, event: &KeyDownEvent, _, _| {
                if event.keystroke.key.as_str() == "enter"
                    && event.keystroke.modifiers.control
                    && !event.keystroke.modifiers.alt
                {
                    finish_active();
                } else if event.keystroke.key.as_str() == "escape"
                    && event.keystroke.modifiers.shift
                    && !event.keystroke.modifiers.control
                {
                    cancel_active();
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(8.0)).h(px(8.0)).rounded_full().bg(rgb(0x0078D4)))
                    .child(div().text_xs().child(tr_fmt(
                        "long.progress",
                        &[("w", &self.width.to_string()), ("h", &height.to_string())],
                    ))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .id("long-cancel-btn")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(rgb(p.card))
                            .border_1()
                            .border_color(rgb(p.border))
                            .text_xs()
                            .cursor_pointer()
                            .on_click(cx.listener(|_, _, _, _| {
                                cancel_active();
                            }))
                            .child(tr("long.cancel")),
                    )
                    .child(
                        div()
                            .id("long-finish-btn")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(rgb(0x0078D4))
                            .text_color(rgb(0xFFFFFF))
                            .text_xs()
                            .cursor_pointer()
                            .on_click(cx.listener(|_, _, _, _| {
                                save_active();
                            }))
                            .child(tr("long.save")),
                    ),
            )
    }
}

/// 单帧拼接结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StitchOutcome {
    /// 已追加新底部行。
    Appended,
    /// 重复帧、上滚或不匹配。
    Ignored,
    /// 已达到像素上限。
    Limit,
}

/// 纵向向下拼接器：按固定选区宽度寻找重叠区域，只追加新底部行。
struct LongStitcher {
    /// 选区宽度。
    width: u32,
    /// 当前拼接高度。
    height: u32,
    /// 连续 RGBA 像素。
    pixels: Vec<u8>,
    /// 上一张已接受画面，只与它匹配以限制重复内容的搜索范围。
    previous: RgbaImage,
    /// 像素上限。
    max_pixels: u64,
    /// 是否因像素或内存边界提前结束，交付时明确提示用户。
    limited: bool,
}

impl LongStitcher {
    /// 用首屏图片创建拼接器。
    fn new(initial: RgbaImage) -> Result<Self, String> {
        Self::with_limit(initial, MAX_PIXELS)
    }

    /// 用指定像素上限创建拼接器（测试可缩小上限）。
    fn with_limit(initial: RgbaImage, max_pixels: u64) -> Result<Self, String> {
        let (width, height) = initial.dimensions();
        if width == 0 || height == 0 {
            return Err("长截图首帧为空".to_string());
        }
        if width as u64 * height as u64 > max_pixels {
            return Err("长截图首帧超过像素上限".to_string());
        }
        Ok(Self {
            width,
            height,
            pixels: initial.clone().into_raw(),
            previous: initial,
            max_pixels,
            limited: false,
        })
    }

    /// 当前拼接高度。
    fn height(&self) -> u32 {
        self.height
    }

    /// 消费一张实时帧；匹配成功时追加新底部行。
    fn push(&mut self, frame: &RgbaImage) -> StitchOutcome {
        // 固定选区不会改变分辨率；只和上一张已接受帧比较，避免历史重复行造成跨段错拼。
        if frame.dimensions() != self.previous.dimensions() || !has_detail(frame) {
            return StitchOutcome::Ignored;
        }
        if frame.as_raw() == self.previous.as_raw() {
            return StitchOutcome::Ignored;
        }
        let min_overlap = (frame.height() / 8)
            .max(24)
            .min(frame.height().saturating_sub(1).max(1));
        let max_shift = frame.height().saturating_sub(min_overlap);
        if max_shift == 0 {
            return StitchOutcome::Ignored;
        }
        let row_bytes = self.width as usize * 4;
        let sample_count = row_sample_count(self.width);
        let base_rows = row_signatures(
            self.previous.as_raw(),
            self.width,
            1,
            frame.height(),
            sample_count,
        );
        let frame_rows =
            row_signatures(frame.as_raw(), self.width, 0, frame.height(), sample_count);
        let candidates = row_vote_candidates(
            &base_rows,
            &frame_rows,
            sample_count,
            1,
            max_shift,
            min_overlap,
        );

        // 行投票只负责缩小范围；复核使用全宽像素，区分行号等稀疏但关键的细节。
        let mut verified: Vec<(u32, u32)> = candidates
            .into_iter()
            .map(|(_, _, shift)| {
                let error = sampled_row_error(self.previous.as_raw(), frame, self.width, shift, 4);
                (error, shift)
            })
            .collect();
        verified.sort_unstable();
        let Some(&(best_error, shift)) = verified.first() else {
            return StitchOutcome::Ignored;
        };
        let second_error = verified
            .iter()
            .filter(|(_, other)| other.abs_diff(shift) > 4)
            .map(|(error, _)| *error)
            .min()
            .unwrap_or(u32::MAX);
        // 相邻位移只是同一接缝的取整误差；真正独立的候选必须保留至少 20% 优势。
        if best_error > 12 || second_error.saturating_mul(100) <= best_error.saturating_mul(120) {
            return StitchOutcome::Ignored;
        }
        let new_height = self.height as u64 + shift as u64;
        if new_height > u32::MAX as u64 || new_height * self.width as u64 > self.max_pixels {
            self.limited = true;
            return StitchOutcome::Limit;
        }
        let start = (frame.height() - shift) as usize * row_bytes;
        // 分配失败也按容量上限收尾，不让超长截图因扩容失败直接崩溃。
        if self
            .pixels
            .try_reserve(frame.as_raw().len() - start)
            .is_err()
        {
            self.limited = true;
            return StitchOutcome::Limit;
        }
        self.pixels.extend_from_slice(&frame.as_raw()[start..]);
        self.height += shift;
        self.previous = frame.clone();
        StitchOutcome::Appended
    }

    /// 取出最终 RGBA 图片。
    fn finish(self) -> Result<RgbaImage, String> {
        RgbaImage::from_raw(self.width, self.height, self.pixels)
            .ok_or_else(|| "长截图像素缓冲区无效".to_string())
    }
}

/// 计算每行水平采样点数量，限制超宽选区的匹配开销。
fn row_sample_count(width: u32) -> usize {
    let step = (width as usize / 256).max(4);
    let start = step / 2;
    if width as usize <= start {
        1
    } else {
        (width as usize - start).div_ceil(step)
    }
}

/// 为指定行区间生成水平 RGB 采样特征，保留文字笔画与行内细节。
fn row_signatures(pixels: &[u8], width: u32, start: u32, end: u32, sample_count: usize) -> Vec<u8> {
    let step = (width as usize / 256).max(4);
    let row_bytes = width as usize * 4;
    let mut signatures = Vec::with_capacity((end - start) as usize * sample_count * 3);
    for row in start..end {
        let row_start = row as usize * row_bytes;
        for sample in 0..sample_count {
            let x = if sample + 1 == sample_count {
                width as usize - 1
            } else {
                step / 2 + sample * step
            };
            let pixel = row_start + x * 4;
            signatures.extend_from_slice(&pixels[pixel..pixel + 3]);
        }
    }
    signatures
}

/// 比较两行水平采样特征的平均 RGB 误差。
fn signature_row_error(base: &[u8], frame: &[u8]) -> u32 {
    let mut total = 0u64;
    let mut samples = 0u64;
    for (base, frame) in base
        .as_chunks::<3>()
        .0
        .iter()
        .zip(frame.as_chunks::<3>().0.iter())
    {
        for channel in 0..3 {
            total += (base[channel] as i32 - frame[channel] as i32).unsigned_abs() as u64;
            samples += 1;
        }
    }
    total
        .checked_div(samples)
        .map_or(u32::MAX, |average| average as u32)
}

/// 用逐行最佳匹配投票生成候选位移，避免重复文字在全局平均误差中淹没。
fn row_vote_candidates(
    base: &[u8],
    frame: &[u8],
    sample_count: usize,
    min_shift: u32,
    max_shift: u32,
    min_overlap: u32,
) -> Vec<(u32, u32, u32)> {
    let row_bytes = sample_count * 3;
    let range_len = (max_shift - min_shift + 1) as usize;
    let mut votes = vec![0u32; range_len];
    let mut total_errors = vec![0u64; range_len];
    for row in sample_indices(min_overlap as usize, 32) {
        let frame_start = row * row_bytes;
        let frame_row = &frame[frame_start..frame_start + row_bytes];
        let mut matches: Vec<(u32, u32)> = (min_shift..=max_shift)
            .map(|shift| {
                let base_row = (shift + row as u32 - min_shift) as usize;
                let base_start = base_row * row_bytes;
                let error =
                    signature_row_error(&base[base_start..base_start + row_bytes], frame_row);
                (error, shift)
            })
            .collect();
        matches.sort_unstable();
        let Some(&(best_error, _)) = matches.first() else {
            continue;
        };
        if matches
            .iter()
            .take(5)
            .filter(|(error, _)| *error == best_error)
            .count()
            > 4
        {
            // 超过四个并列最优说明该行没有区分度，弃权以避免小编号噪声候选挤掉真位移。
            continue;
        }
        // 重复行常有多处近似匹配；保留少量近优候选共同投票，再由全宽复核排除周期位移。
        for (error, shift) in matches.into_iter().take(4) {
            if error > 32 || error > best_error.saturating_add(2) {
                break;
            }
            let index = (shift - min_shift) as usize;
            votes[index] += 1;
            total_errors[index] += error as u64;
        }
    }
    let mut candidates: Vec<(u32, u32, u32)> = votes
        .into_iter()
        .enumerate()
        .filter(|(_, count)| *count >= 3)
        .map(|(index, count)| (count, total_errors[index] as u32, min_shift + index as u32))
        .collect();
    candidates.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    candidates.truncate(8);
    candidates
}

/// 用全宽像素复核候选位移，每隔 `row_step` 行取样以控制计算量。
fn sampled_row_error(
    base: &[u8],
    frame: &RgbaImage,
    width: u32,
    shift: u32,
    row_step: usize,
) -> u32 {
    let base_height = (base.len() / (width as usize * 4)) as u32;
    let overlap = frame.height().min(base_height.saturating_sub(shift));
    let row_bytes = width as usize * 4;
    let mut total = 0u64;
    let mut samples = 0u64;
    for row in (0..overlap as usize).step_by(row_step) {
        let base_row = (shift as usize + row) * row_bytes;
        let frame_row = row * row_bytes;
        for pixel in (0..width as usize * 4).step_by(4) {
            for channel in 0..3 {
                total += (base[base_row + pixel + channel] as i32
                    - frame.as_raw()[frame_row + pixel + channel] as i32)
                    .unsigned_abs() as u64;
                samples += 1;
            }
        }
    }
    total
        .checked_div(samples)
        .map_or(u32::MAX, |average| average as u32)
}

/// 生成包含首尾的稀疏采样下标。
fn sample_indices(len: usize, count: usize) -> Vec<usize> {
    if len <= count {
        return (0..len).collect();
    }
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        out.push(index * (len - 1) / (count - 1));
    }
    out
}

/// 稀疏采样判断画面是否有细节，纯色帧不追加，避免空白页被误判为滚动。
fn has_detail(frame: &RgbaImage) -> bool {
    let rows = sample_indices(frame.height() as usize, 16);
    let cols = sample_indices(frame.width() as usize, 16);
    let Some(first_row) = rows.first() else {
        return false;
    };
    let Some(first_col) = cols.first() else {
        return false;
    };
    let first = frame.get_pixel(*first_col as u32, *first_row as u32).0;
    rows.iter().any(|&row| {
        cols.iter().any(|&col| {
            let pixel = frame.get_pixel(col as u32, row as u32).0;
            pixel[0] != first[0] || pixel[1] != first[1] || pixel[2] != first[2]
        })
    })
}

#[cfg(test)]
mod tests {
    //! 拼接算法单测：重叠、重复帧、空白帧、大跳跃与容量上限。

    use super::*;

    /// 按行颜色构造测试图。
    fn rows(width: u32, colors: &[[u8; 3]]) -> RgbaImage {
        let mut image = RgbaImage::new(width, colors.len() as u32);
        for (y, color) in colors.iter().enumerate() {
            for x in 0..width {
                image.put_pixel(
                    x,
                    y as u32,
                    image::Rgba([color[0], color[1], color[2], 255]),
                );
            }
        }
        image
    }

    /// 构造重复文本行，仅在行尾留下随绝对行号变化的细节。
    fn repeated_line_image(width: u32, start: u32, len: u32) -> RgbaImage {
        let colors = pattern_colors((start + len) as usize);
        let mut image = RgbaImage::from_pixel(width, len, image::Rgba([230, 235, 240, 255]));
        for row in 0..len {
            let line = (start + row) / 10;
            let background = [200 + (line % 5) as u8, 210, 220];
            for x in 0..width {
                image.put_pixel(
                    x,
                    row,
                    image::Rgba([background[0], background[1], background[2], 255]),
                );
            }
            let marker = colors[(start + row) as usize];
            image.put_pixel(
                width - 1,
                row,
                image::Rgba([marker[0], marker[1], marker[2], 255]),
            );
        }
        image
    }

    /// 生成相邻行差异明显的伪随机颜色，避免平滑渐变被误判为多个候选。
    fn pattern_colors(len: usize) -> Vec<[u8; 3]> {
        (0..len)
            .map(|index| {
                let mut value = index as u32 + 0x9e37_79b9;
                value = (value ^ (value >> 16)).wrapping_mul(0x85eb_ca6b);
                value = (value ^ (value >> 13)).wrapping_mul(0xc2b2_ae35);
                value ^= value >> 16;
                [value as u8, (value >> 8) as u8, (value >> 16) as u8]
            })
            .collect()
    }

    /// 向下滚动三十行后，只追加三十行新内容。
    #[test]
    fn appends_only_new_bottom_rows() {
        let colors = pattern_colors(120);
        let base = rows(8, &colors[..90]);
        let frame = rows(8, &colors[30..120]);
        let mut stitcher = LongStitcher::new(base).unwrap();
        assert_eq!(stitcher.push(&frame), StitchOutcome::Appended);
        let result = stitcher.finish().unwrap();
        assert_eq!(result.dimensions(), (8, 120));
        assert_eq!(
            result.get_pixel(0, 119).0,
            [colors[119][0], colors[119][1], colors[119][2], 255]
        );
    }

    /// 拼接高度超过实时帧高后继续滚动，仍只追加新底部行。
    #[test]
    fn keeps_appending_after_base_grows_taller_than_frame() {
        let colors = pattern_colors(180);
        let mut stitcher = LongStitcher::new(rows(8, &colors[..90])).unwrap();
        assert_eq!(
            stitcher.push(&rows(8, &colors[30..120])),
            StitchOutcome::Appended
        );
        assert_eq!(
            stitcher.push(&rows(8, &colors[60..150])),
            StitchOutcome::Appended
        );
        assert_eq!(
            stitcher.push(&rows(8, &colors[60..150])),
            StitchOutcome::Ignored
        );
        let result = stitcher.finish().unwrap();
        assert_eq!(result.dimensions(), (8, 150));
        assert_eq!(
            result.get_pixel(0, 149).0,
            [colors[149][0], colors[149][1], colors[149][2], 255]
        );
    }

    /// 单帧向下跳过三分之二视口时，只要仍有重叠就继续追加。
    #[test]
    fn appends_large_shift_with_remaining_overlap() {
        let colors = pattern_colors(150);
        let base = rows(8, &colors[..90]);
        let frame = rows(8, &colors[60..150]);
        let mut stitcher = LongStitcher::new(base).unwrap();
        assert_eq!(stitcher.push(&frame), StitchOutcome::Appended);
        assert_eq!(stitcher.finish().unwrap().dimensions(), (8, 150));
    }

    /// 连续滚动超过五十屏后逐像素核对，防止累计高度导致后半段截断。
    #[test]
    fn stitches_many_screens_without_losing_rows() {
        let colors = pattern_colors(6090);
        let mut stitcher = LongStitcher::new(rows(32, &colors[..90])).unwrap();
        for start in (60..=6000).step_by(60) {
            assert_eq!(
                stitcher.push(&rows(32, &colors[start..start + 90])),
                StitchOutcome::Appended
            );
        }
        assert_eq!(stitcher.finish().unwrap(), rows(32, &colors));
    }

    /// 实际缓冲超过旧版 3200 万像素后仍可继续拼接并保持末行完整。
    #[test]
    fn continues_beyond_old_pixel_limit() {
        let colors = pattern_colors(120);
        let mut stitcher = LongStitcher::new(rows(8, &colors[..90])).unwrap();
        // 预填长图主体以节省几千次重复匹配，实际追加仍经过完整生产路径。
        stitcher.height = 4_000_000;
        stitcher
            .pixels
            .resize(stitcher.height as usize * 8 * 4, 255);
        assert_eq!(
            stitcher.push(&rows(8, &colors[30..120])),
            StitchOutcome::Appended
        );
        let result = stitcher.finish().unwrap();
        assert_eq!(result.dimensions(), (8, 4_000_030));
        assert_eq!(
            result.get_pixel(0, 4_000_029),
            rows(8, &colors[119..120]).get_pixel(0, 0)
        );
    }

    /// 快速滚动至仅剩八分之一视口重叠时仍能正确定位。
    #[test]
    fn appends_with_small_remaining_overlap() {
        let colors = pattern_colors(450);
        let mut stitcher = LongStitcher::new(rows(32, &colors[..240])).unwrap();
        assert_eq!(
            stitcher.push(&rows(32, &colors[210..450])),
            StitchOutcome::Appended
        );
        assert_eq!(stitcher.finish().unwrap(), rows(32, &colors));
    }

    /// 首版只支持向下滚动，上滚帧不能倒接进长图。
    #[test]
    fn ignores_upward_scroll() {
        let colors = pattern_colors(150);
        let base = rows(8, &colors[60..150]);
        let frame = rows(8, &colors[..90]);
        let mut stitcher = LongStitcher::new(base).unwrap();
        assert_eq!(stitcher.push(&frame), StitchOutcome::Ignored);
        assert_eq!(stitcher.finish().unwrap().dimensions(), (8, 90));
    }

    /// 重复文本行靠行尾的细小差异也能选中正确位移。
    #[test]
    fn matches_repeated_lines_with_sparse_unique_details() {
        let base = repeated_line_image(64, 0, 90);
        let frame = repeated_line_image(64, 30, 90);
        let mut stitcher = LongStitcher::new(base).unwrap();
        assert_eq!(stitcher.push(&frame), StitchOutcome::Appended);
        assert_eq!(stitcher.finish().unwrap().dimensions(), (64, 120));
    }

    /// 重复帧和纯色帧不会让图片无限增长。
    #[test]
    fn ignores_duplicate_and_blank_frames() {
        let base = rows(
            8,
            &[[10, 20, 30], [40, 50, 60], [70, 80, 90], [100, 110, 120]],
        );
        let mut stitcher = LongStitcher::new(base.clone()).unwrap();
        assert_eq!(stitcher.push(&base), StitchOutcome::Ignored);
        let blank = RgbaImage::from_pixel(8, 8, image::Rgba([255, 255, 255, 255]));
        assert_eq!(stitcher.push(&blank), StitchOutcome::Ignored);
        assert_eq!(stitcher.finish().unwrap().dimensions(), (8, 4));
    }

    /// 完全没有重叠的大跳跃不会错误拼接。
    #[test]
    fn ignores_unmatched_large_jump() {
        let base = rows(8, &[[10, 10, 10], [20, 20, 20], [30, 30, 30], [40, 40, 40]]);
        let frame = rows(
            8,
            &[
                [210, 210, 210],
                [220, 220, 220],
                [230, 230, 230],
                [240, 240, 240],
            ],
        );
        let mut stitcher = LongStitcher::new(base).unwrap();
        assert_eq!(stitcher.push(&frame), StitchOutcome::Ignored);
        assert_eq!(stitcher.finish().unwrap().dimensions(), (8, 4));
    }

    /// 周期纹理出现多个同样候选时拒绝拼接，避免重复内容错位。
    #[test]
    fn rejects_ambiguous_repeated_pattern() {
        let colors: Vec<[u8; 3]> = (0..120)
            .map(|index| [(index % 10 * 20) as u8, 30, 40])
            .collect();
        let mut stitcher = LongStitcher::new(rows(8, &colors[..90])).unwrap();
        assert_eq!(
            stitcher.push(&rows(8, &colors[30..120])),
            StitchOutcome::Ignored
        );
        assert_eq!(stitcher.finish().unwrap().dimensions(), (8, 90));
    }

    /// 超过像素上限时停止追加并保留已有内容。
    #[test]
    fn enforces_pixel_limit() {
        let colors = pattern_colors(90);
        let base = rows(2, &colors[..60]);
        let frame = rows(2, &colors[30..90]);
        let mut stitcher = LongStitcher::with_limit(base, 120).unwrap();
        assert_eq!(stitcher.push(&frame), StitchOutcome::Limit);
        assert_eq!(stitcher.finish().unwrap().dimensions(), (2, 60));
    }
}
