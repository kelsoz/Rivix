//! 区域选择浮层：先冻结一张全屏图盖住屏幕，再在上面拖拽画矩形裁剪。
//!
//! 与系统截图 / Snipaste 一致：浮层显示的是截取瞬间的静态首帧（1:1 物理像素），
//! 而不是透明活背景——视频等动态内容会被定住，框选与裁剪严格对应同一帧。
//! 窗口吸附：未拖拽时悬停窗口显示高亮，点击即截该窗口，
//! 拖拽则按自由选区裁剪（独立窗口截图入口已移除，统一走区域截图）。
//! 界面只用 `img` + 纯色 `div`（遮罩条 / 边框 / 徽标），不用 `canvas`。

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
};

use rgpui::{
    AnyWindowHandle, AppContext, Bounds, Context, CursorStyle, Entity, FocusHandle, KeyDownEvent,
    KeyUpEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Render,
    RenderImage, Window, WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div,
    img, prelude::*, px, rgb, rgba,
};

use crate::{
    app::{RivixApp, finish_capture, palette},
    capture::{CapturedFrame, cap_log},
    config,
    i18n::{tr, tr_fmt},
    ocr, picker, qr, record,
};

/// 一轮多屏浮层的共享会话：任一处完成 / 取消 / 被系统关闭时，统一关掉同轮其余浮层。
pub struct OverlaySession {
    /// 每块屏的浮层窗口句柄（按打开顺序，未打开的位置为 None）。
    handles: RefCell<Vec<Option<AnyWindowHandle>>>,
    /// 本轮是否已结束（结束后其余浮层只关窗、不再重复收尾）。
    finished: Cell<bool>,
}

impl OverlaySession {
    /// 创建一轮浮层会话。
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            handles: RefCell::new(Vec::new()),
            finished: Cell::new(false),
        })
    }

    /// 登记第 `index` 块屏的浮层窗口句柄（句柄在开窗返回后才有）。
    pub(crate) fn register(&self, index: usize, handle: AnyWindowHandle) {
        let mut handles = self.handles.borrow_mut();
        if handles.len() <= index {
            handles.resize(index + 1, None);
        }
        handles[index] = Some(handle);
    }

    /// 本轮是否已结束。
    pub fn is_finished(&self) -> bool {
        self.finished.get()
    }

    /// 结束整轮并关闭除 `keep` 以外的浮层；返回是否由本次调用触发结束。
    pub(crate) fn finish<C: AppContext>(&self, keep: Option<usize>, cx: &mut C) -> bool {
        if self.finished.replace(true) {
            return false;
        }
        let handles = self.handles.borrow().clone();
        for (index, handle) in handles.into_iter().enumerate() {
            if Some(index) == keep {
                continue;
            }
            if let Some(handle) = handle {
                // 其余浮层直接移除；句柄失效（已被系统关掉）时忽略错误。
                let _ = handle.update(cx, |_, window, _| window.remove_window());
            }
        }
        true
    }
}

/// 在每块屏上各打开一层区域选择浮层（多显示器可分别在任意屏幕框选）。
///
/// # 参数
/// - `main`: 主视图实体（裁剪完成后回写预览图 / 取消时恢复状态）。
/// - `cx`: 调用方上下文（可解引用为 `App`）。
/// - `frames`: 每块屏的首帧图片与显示器边界。
///
/// # 返回
/// 至少成功打开一层返回 `true`（之后由浮层的完成 / 取消复位状态）；
/// 全部建窗失败返回 `false`，调用方需自行把「截取中」状态复位。
pub fn open_overlays(
    main: Entity<RivixApp>,
    cx: &mut Context<RivixApp>,
    frames: Vec<CapturedFrame>,
) -> bool {
    if frames.is_empty() {
        return false;
    }
    let session = OverlaySession::new();
    let mut opened = 0;
    for (index, frame) in frames.into_iter().enumerate() {
        // 极少数情况下前一块屏的浮层已结束整轮，不再补开。
        if session.is_finished() {
            break;
        }
        if open_overlay(main.clone(), cx, frame, index, session.clone()) {
            opened += 1;
        }
    }
    opened > 0
}

/// 打开单块屏的区域选择浮层。
fn open_overlay(
    main: Entity<RivixApp>,
    cx: &mut Context<RivixApp>,
    frame: CapturedFrame,
    index: usize,
    session: Rc<OverlaySession>,
) -> bool {
    let (img_w, img_h) = (frame.img_w, frame.img_h);
    let CapturedFrame {
        pixels,
        display_bounds,
        display_id,
        frame_image,
        ..
    } = frame;
    cap_log(&format!(
        "浮层打开：屏 {index} img={img_w}x{img_h} id={display_id:?} bounds={display_bounds:?}"
    ));
    let session_for_build = session.clone();
    let opened = cx.open_window(
        WindowOptions {
            // 无边框置顶浮层，盖住全屏做选区交互。
            // 背景必须是 Opaque：浮层本就用冻结截图铺满整屏，遮罩、选框、徽标都是在这张
            // 图上做的层内混合，窗口级逐像素 alpha 没有意义；而 X11 下一旦要求 Transparent
            // 就会取 32 位 ARGB visual，在 alpha 合成路径不可用的环境（xrdp / 软件 GL 等）
            // 里写进去的 alpha 恒为 0，整扇浮层被判成全透明——表现为没有遮罩也没有选框。
            // 注意：必须用 Fullscreen 而不是 Windowed——Windowed + Overlay
            // 在 Windows 下 placement 不生效，窗口保持 0x0 不可见。
            titlebar: None,
            window_bounds: Some(WindowBounds::Fullscreen(display_bounds)),
            // 指定目标显示器：Windows 的 Fullscreen 会铺到该显示器的物理边界，
            // 不指定时永远落在主屏，副屏就没有浮层可选。
            display_id,
            window_background: WindowBackgroundAppearance::Opaque,
            kind: WindowKind::Overlay,
            focus: true,
            show: true,
            ..Default::default()
        },
        move |window, cx| {
            // 浮层被系统直接关掉（Alt+F4 等）时也要结束整轮并复位状态机，
            // 否则 capturing 永远为 true，之后所有截图入口都会被挡住。
            let main_for_close = main.clone();
            let session_for_close = session_for_build.clone();
            window.on_window_should_close(cx, move |_, cx| {
                if session_for_close.finish(Some(index), cx) {
                    main_for_close.update(cx, |state, cx| {
                        state.capturing = false;
                        state.set_status(&tr("cap.cancelled"), cx);
                    });
                }
                true
            });
            let view = cx.new(|cx| {
                SelectView::new(
                    window,
                    cx,
                    main.clone(),
                    pixels.clone(),
                    img_w,
                    img_h,
                    display_id,
                    display_bounds,
                    frame_image.clone(),
                    session_for_build.clone(),
                    index,
                )
            });
            // 浮层打开即聚焦根视图并激活窗口，使 Esc 立即可用。
            let handle = view.read(cx).focus_handle.clone();
            window.focus(&handle, cx);
            window.activate_window();
            view
        },
    );
    match opened {
        Ok(handle) => {
            session.register(index, *handle);
            cap_log("浮层 open_window 成功");
            true
        }
        Err(error) => {
            cap_log(&format!("浮层 open_window 失败：{error:?}"));
            false
        }
    }
}

/// 区域选择浮层视图：薄遮罩 + 拖拽矩形，抬手裁剪；未拖拽时悬停窗口吸附高亮，点击截取。
struct SelectView {
    /// 主视图实体（回写预览 / 状态）。
    main: Entity<RivixApp>,
    /// 首帧内存像素（裁剪与色卡共用）。
    pixels: Arc<image::RgbaImage>,
    /// 预解码的首帧（浮层背景同步显示用）。
    frame_image: Arc<RenderImage>,
    /// 全图宽（图片像素）。
    img_w: u32,
    /// 全图高（图片像素）。
    img_h: u32,
    /// 浮层逻辑宽（显示器逻辑像素，鼠标坐标空间）。
    view_w: f32,
    /// 浮层逻辑高。
    view_h: f32,
    /// 本屏在桌面物理坐标系中的原点（像素）。
    /// 窗口枚举返回的是桌面全局物理坐标，副屏命中/高亮必须加上这个偏移，
    /// 否则会被当成主屏同位置的窗口（多屏偏移 bug）。
    origin_px: (f32, f32),
    /// 拖拽起点（视图逻辑像素），None 表示未按下。
    anchor: Option<(f32, f32)>,
    /// 拖拽当前点（视图逻辑像素）。
    current: Option<(f32, f32)>,
    /// 最近一次鼠标位置，空格移动与松开后继续缩放都按增量处理，避免选框跳动。
    last_pointer: (f32, f32),
    /// 按住空格时，鼠标移动平移整个选框。
    space_held: bool,
    /// 当前鼠标位置，独立于被空格平移过的选区终点。
    pointer: Option<(f32, f32)>,
    /// 根视图焦点（Esc 取消用）。
    focus_handle: FocusHandle,
    /// 当前吸附高亮的窗口（视图逻辑像素矩形 + 标题），未拖拽时悬停吸附，点击即截该窗口。
    hover: Option<((f32, f32, f32, f32), String)>,
    /// 冻结画面对应的窗口快照，鼠标移动时不再反复枚举系统窗口。
    windows: Vec<crate::window::CaptureWindow>,
    /// 本轮多屏浮层的共享会话（完成 / 取消时统一关闭其余屏的浮层）。
    session: Rc<OverlaySession>,
    /// 本浮层对应的屏序号（会话内唯一）。
    index: usize,
    /// 冻结帧后台扫描结果；鼠标移动仅做边界命中，不重复解码。
    qr_codes: Vec<qr::QrCode>,
    /// 所属显示器 ID（启动录屏时用于匹配捕获源）。
    display_id: Option<rgpui::DisplayId>,
    /// 所属显示器逻辑边界（放置录屏边框与控制条）。
    display_bounds: Bounds<Pixels>,
    /// 按 V 进入的录屏准备态（允许调整比例、标准尺寸、格式与音频后再开录）。
    recording_prep: Option<RecordPrepState>,
}

/// 按四边命中掩码返回对应的窗口调整大小光标样式。
fn resize_cursor(left: bool, top: bool, right: bool, bottom: bool) -> CursorStyle {
    match (left, top, right, bottom) {
        (true, true, false, false) | (false, false, true, true) => {
            CursorStyle::ResizeUpLeftDownRight
        }
        (false, true, true, false) | (true, false, false, true) => {
            CursorStyle::ResizeUpRightDownLeft
        }
        (true, false, false, false) | (false, false, true, false) => CursorStyle::ResizeLeftRight,
        _ => CursorStyle::ResizeUpDown,
    }
}

/// 录屏准备态下的选区拖动模式。
#[derive(Clone, Copy)]
enum RecDragKind {
    /// 拖动选区内部整体平移。
    Move,
    /// 拖动选区四边或四角进行缩放（带四边掩码：左、上、右、下）。
    Resize(bool, bool, bool, bool),
}

/// 录屏准备态下的选区拖动上下文。
#[derive(Clone, Copy)]
struct RecDragState {
    kind: RecDragKind,
    start_pt: (f32, f32),
    start_rect: (f32, f32, f32, f32),
}

/// 按 V 进入录屏准备态后的交互状态。
struct RecordPrepState {
    rect: (f32, f32, f32, f32),
    ratio: record::AspectPreset,
    format: record::RecordFormat,
    fps: u32,
    system_audio: bool,
    mic_audio: bool,
    show_cursor: bool,
    drag: Option<RecDragState>,
}

impl SelectView {
    /// 创建选择视图。
    #[allow(clippy::too_many_arguments)]
    fn new(
        _window: &mut Window,
        cx: &mut Context<Self>,
        main: Entity<RivixApp>,
        pixels: Arc<image::RgbaImage>,
        img_w: u32,
        img_h: u32,
        display_id: Option<rgpui::DisplayId>,
        display_bounds: Bounds<Pixels>,
        frame_image: Arc<RenderImage>,
        session: Rc<OverlaySession>,
        index: usize,
    ) -> Self {
        let view_w = display_bounds.size.width.as_f32();
        let view_h = display_bounds.size.height.as_f32();
        // 图片物理像素 / 视图逻辑像素 = 该屏缩放比；逻辑原点乘缩放比即物理原点。
        let sx = img_w as f32 / view_w.max(1.0);
        let sy = img_h as f32 / view_h.max(1.0);
        let scan_pixels = pixels.clone();
        // 截图冻结后只扫描一次，任务不阻塞拖框，也不为检测启动计时器。
        cx.spawn(async move |this, cx| {
            let codes = cx
                .background_executor()
                .spawn(async move { qr::detect(&scan_pixels) })
                .await;
            let _ = this.update(cx, |view, cx| {
                if !view.session.is_finished() {
                    view.qr_codes = codes;
                    cx.notify();
                }
            });
        })
        .detach();
        Self {
            main,
            pixels,
            frame_image,
            img_w,
            img_h,
            view_w,
            view_h,
            origin_px: (
                display_bounds.origin.x.as_f32() * sx,
                display_bounds.origin.y.as_f32() * sy,
            ),
            anchor: None,
            current: None,
            last_pointer: (0.0, 0.0),
            space_held: false,
            pointer: None,
            focus_handle: cx.focus_handle(),
            hover: None,
            windows: crate::window::list_windows(),
            session,
            index,
            qr_codes: Vec::new(),
            display_id,
            display_bounds,
            recording_prep: None,
        }
    }

    /// 当前选区（视图逻辑像素，起点终点归一化），未拖拽时为 None。
    fn sel_rect(&self) -> Option<(f32, f32, f32, f32)> {
        norm_rect(self.anchor?, self.current?, self.view_w, self.view_h)
    }

    /// 视图逻辑像素 → 图片物理像素的缩放比。
    fn scale(&self) -> (f32, f32) {
        (
            self.img_w as f32 / self.view_w.max(1.0),
            self.img_h as f32 / self.view_h.max(1.0),
        )
    }

    /// 当前选区完整包含的首个二维码；物理坐标对应原始冻结图，兼容混合 DPI。
    fn selected_qr(&self) -> Option<&qr::QrCode> {
        let (x, y, w, h) = self.sel_rect()?;
        let (sx, sy) = self.scale();
        self.qr_codes
            .iter()
            .find(|code| code.inside((x * sx, y * sy, w * sx, h * sy)))
    }

    /// Q 消费截图会话，复制二维码内容，仅网页地址交给浏览器打开。
    fn open_qr(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.is_finished() {
            return;
        }
        let Some(code) = self.selected_qr() else {
            return;
        };
        let text = code.text.clone();
        let url = qr::web_url(&text);
        // 与 T/C 一样清空拖拽，之后的鼠标松开不能覆盖已经复制的地址。
        self.anchor = None;
        self.current = None;
        self.session.finish(Some(self.index), cx);
        cx.write_to_clipboard(rgpui::ClipboardItem::new_string(text));
        crate::feedback::completed();
        self.main.update(cx, |state, cx| {
            state.capturing = false;
            let message = tr(if url.is_some() {
                "qr.opened"
            } else {
                "qr.copied"
            });
            state.set_status(&message, cx);
            crate::toast::show(cx, message);
        });
        window.remove_window();
        cx.refresh_windows();
        if let Some(url) = url {
            cx.open_url(url.as_str());
        }
    }

    /// 左键按下：开始新的拖拽选区（录屏准备态下改为调整或平移选区）。
    fn on_left_down(&mut self, ev: &MouseDownEvent, cx: &mut Context<Self>) {
        // 鼠标坐标即窗口坐标，根视图铺满窗口，直接可用。
        let p = (ev.position.x.as_f32(), ev.position.y.as_f32());
        self.pointer = Some(p);
        if self.recording_prep.is_some() {
            self.prep_left_down(p, cx);
            return;
        }
        self.anchor = Some(p);
        self.current = Some(p);
        self.last_pointer = p;
        cx.notify();
    }

    /// 鼠标移动：未拖拽时吸附窗口更新高亮，拖拽中更新选区当前点。
    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        let p = (ev.position.x.as_f32(), ev.position.y.as_f32());
        self.pointer = Some(p);
        if self.recording_prep.is_some() {
            self.prep_mouse_move(p, cx);
            return;
        }
        // 未按下时悬停吸附窗口（点击即截该窗口），拖拽中只更新选区。
        if self.anchor.is_none() {
            self.update_hover(p, cx);
            cx.notify();
            return;
        }
        self.update_drag(p);
        cx.notify();
    }

    /// 根据鼠标增量调整选区；空格平移时保持宽高，放开空格继续调整终点。
    fn update_drag(&mut self, p: (f32, f32)) {
        let (Some(anchor), Some(current)) = (self.anchor, self.current) else {
            return;
        };
        let delta = (p.0 - self.last_pointer.0, p.1 - self.last_pointer.1);
        self.last_pointer = p;
        if self.space_held {
            let (anchor, current) =
                translate_selection(anchor, current, delta, (self.view_w, self.view_h));
            self.anchor = Some(anchor);
            self.current = Some(current);
        } else {
            self.current = Some((
                (current.0 + delta.0).clamp(0.0, self.view_w),
                (current.1 + delta.1).clamp(0.0, self.view_h),
            ));
        }
    }

    /// 按视图坐标更新窗口吸附高亮（未拖拽时悬停调用）。
    fn update_hover(&mut self, p: (f32, f32), cx: &mut Context<Self>) {
        // 视图坐标换算到桌面全局物理像素做命中：先乘缩放比，再加本屏物理原点。
        let (sx, sy) = self.scale();
        let gx = (self.origin_px.0 + p.0 * sx).round() as i32;
        let gy = (self.origin_px.1 + p.1 * sy).round() as i32;
        let hit = self
            .windows
            .iter()
            .find(|w| w.contains(gx, gy))
            .and_then(|w| {
                view_rect_from_physical(
                    w.rect,
                    self.origin_px,
                    (sx, sy),
                    (self.view_w, self.view_h),
                )
                .map(|rect| (rect, w.title.clone()))
            });
        // 高亮不变时不刷新，避免每帧重排。
        if hit.as_ref().map(|(r, _)| *r) != self.hover.as_ref().map(|(r, _)| *r) {
            self.hover = hit;
            cx.notify();
        }
    }

    /// C 复制当前光标像素的 HEX 并结束整轮截图；不裁剪或保存图片。
    fn copy_color(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.anchor.is_none() || self.session.is_finished() {
            return;
        }
        let Some(pointer) = self.pointer else {
            return;
        };
        let sample = picker::pointer_pixel(pointer, self.scale(), (self.img_w, self.img_h));
        let Some((r, g, b)) = picker::sample_at(&self.pixels, sample.0, sample.1) else {
            return;
        };
        let hex = picker::rgb_to_hex(r, g, b);
        // 消费会话和拖拽，之后放开鼠标不能再走普通截图收尾。
        self.anchor = None;
        self.current = None;
        self.session.finish(Some(self.index), cx);
        cx.write_to_clipboard(rgpui::ClipboardItem::new_string(hex.clone()));
        crate::feedback::completed();
        self.main.update(cx, |state, cx| {
            state.capturing = false;
            let message = tr_fmt("picker.copied", &[("hex", &hex)]);
            state.set_status(&message, cx);
            crate::toast::show(cx, message);
        });
        window.remove_window();
        cx.refresh_windows();
    }

    /// 左键抬起：拖拽出选区则裁剪，点击窗口则截该窗口（录屏准备态下仅结束调整拖动）。
    fn on_left_up(&mut self, ev: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ref mut prep) = self.recording_prep {
            prep.drag = None;
            cx.notify();
            return;
        }
        self.update_drag((ev.position.x.as_f32(), ev.position.y.as_f32()));
        // anchor 取走后置空：拖出窗口松开会再来一次 mouse_up_out，
        // 不清空会导致 finish 重复执行（双裁剪、双预览窗口）。
        let Some(anchor) = self.anchor.take() else {
            return;
        };
        let cur = self.current.unwrap_or(anchor);
        // 用取出的起点直接算矩形（anchor 已清空防重入，不走 sel_rect）。
        let Some((x, y, w, h)) = norm_rect(anchor, cur, self.view_w, self.view_h) else {
            // 纯点击无位移：悬停在窗口上则截该窗口，否则不关闭允许重新拖拽。
            if let Some(((hx, hy, hw, hh), _)) = self.hover.clone() {
                self.finish(window, cx, (hx, hy, hw, hh));
            } else {
                self.current = None;
                cx.notify();
            }
            return;
        };
        // 极小拖动视为点击：悬停在窗口上则直接截该窗口（区域模式吸附窗口）；
        // 空白处点按仍视为误触，不关闭，允许重新拖拽。
        if w < 5.0 || h < 5.0 {
            if let Some(((hx, hy, hw, hh), _)) = self.hover.clone() {
                self.finish(window, cx, (hx, hy, hw, hh));
            } else {
                self.current = None;
                cx.notify();
            }
            return;
        }
        self.finish(window, cx, (x, y, w, h));
    }

    /// 按 V 键从常规截图切换至录屏准备态，沿用当前选区或窗口高亮（无选区时默认居中 1280×720）。
    fn enter_recording_prep(&mut self, cx: &mut Context<Self>) {
        if self.session.is_finished() || self.recording_prep.is_some() {
            return;
        }
        let (sx, sy) = self.scale();
        let initial_rect = self
            .sel_rect()
            .or_else(|| self.hover.as_ref().map(|(r, _)| *r))
            .unwrap_or_else(|| {
                let lw = (1280.0 / sx.max(0.1)).min(self.view_w * 0.8);
                let lh = (720.0 / sy.max(0.1)).min(self.view_h * 0.8);
                ((self.view_w - lw) * 0.5, (self.view_h - lh) * 0.5, lw, lh)
            });
        // 消费截图拖拽态，松开鼠标后不再走截图提交。
        self.anchor = None;
        self.current = None;
        self.hover = None;
        let cfg = config::load().record;
        self.recording_prep = Some(RecordPrepState {
            rect: initial_rect,
            ratio: record::AspectPreset::Free,
            format: record::RecordFormat::from_config(&cfg.format),
            fps: cfg.fps.clamp(15, 60),
            system_audio: cfg.system_audio,
            mic_audio: cfg.mic_audio,
            show_cursor: cfg.show_cursor,
            drag: None,
        });
        cx.notify();
    }

    /// 录屏准备态下按下左键：边缘/角点缩放（14px 热区），框内平移，框外按住拖动可重画新框。
    fn prep_left_down(&mut self, p: (f32, f32), cx: &mut Context<Self>) {
        let Some(ref mut prep) = self.recording_prep else {
            return;
        };
        let (rx, ry, rw, rh) = prep.rect;
        let tol = 14.0;
        let in_y = p.1 >= ry - tol && p.1 <= ry + rh + tol;
        let in_x = p.0 >= rx - tol && p.0 <= rx + rw + tol;
        let edge_l = in_y && (p.0 - rx).abs() <= tol;
        let edge_r = in_y && (p.0 - (rx + rw)).abs() <= tol;
        let edge_t = in_x && (p.1 - ry).abs() <= tol;
        let edge_b = in_x && (p.1 - (ry + rh)).abs() <= tol;

        let kind = if edge_l || edge_r || edge_t || edge_b {
            RecDragKind::Resize(edge_l, edge_t, edge_r, edge_b)
        } else if p.0 >= rx && p.0 <= rx + rw && p.1 >= ry && p.1 <= ry + rh {
            RecDragKind::Move
        } else {
            // 点击在选区外时先保持原选区，避免误触丢失；开始拖出新框后再替换。
            RecDragKind::Resize(false, false, true, true)
        };
        let start_rect = if matches!(kind, RecDragKind::Resize(false, false, true, true))
            && !(p.0 >= rx - tol && p.0 <= rx + rw + tol && p.1 >= ry - tol && p.1 <= ry + rh + tol)
        {
            (
                p.0.clamp(0.0, (self.view_w - 16.0).max(0.0)),
                p.1.clamp(0.0, (self.view_h - 16.0).max(0.0)),
                1.0,
                1.0,
            )
        } else {
            prep.rect
        };
        prep.drag = Some(RecDragState {
            kind,
            start_pt: p,
            start_rect,
        });
        cx.notify();
    }

    /// 录屏准备态下移动鼠标：按拖动模式平移或等比缩放选区。
    fn prep_mouse_move(&mut self, p: (f32, f32), cx: &mut Context<Self>) {
        let Some(ref mut prep) = self.recording_prep else {
            return;
        };
        let Some(drag) = prep.drag else {
            // 未按住拖动时也要触发重绘，使鼠标离开边缘后光标样式实时恢复。
            cx.notify();
            return;
        };
        let dx = p.0 - drag.start_pt.0;
        let dy = p.1 - drag.start_pt.1;
        let (sx0, sy0, sw0, sh0) = drag.start_rect;
        let min_size = 32.0;

        match drag.kind {
            RecDragKind::Move => {
                let nx = (sx0 + dx).clamp(0.0, (self.view_w - sw0).max(0.0));
                let ny = (sy0 + dy).clamp(0.0, (self.view_h - sh0).max(0.0));
                prep.rect = (nx, ny, sw0, sh0);
            }
            RecDragKind::Resize(el, et, er, eb) => {
                let mut left = sx0;
                let mut top = sy0;
                let mut right = sx0 + sw0;
                let mut bottom = sy0 + sh0;

                if el {
                    left = (sx0 + dx).clamp(0.0, right - min_size);
                }
                if er {
                    right = (sx0 + sw0 + dx).clamp(left + min_size, self.view_w);
                }
                if et {
                    top = (sy0 + dy).clamp(0.0, bottom - min_size);
                }
                if eb {
                    bottom = (sy0 + sh0 + dy).clamp(top + min_size, self.view_h);
                }

                let mut nw = (right - left).max(min_size);
                let mut nh = (bottom - top).max(min_size);
                if let Some(ratio) = prep.ratio.ratio() {
                    if el || er {
                        nh = (nw / ratio).max(min_size);
                        if et {
                            top = (bottom - nh).max(0.0);
                            nh = bottom - top;
                            nw = nh * ratio;
                        } else if top + nh > self.view_h {
                            nh = self.view_h - top;
                            nw = nh * ratio;
                        }
                    } else {
                        nw = (nh * ratio).max(min_size);
                        if left + nw > self.view_w {
                            nw = self.view_w - left;
                            nh = nw / ratio;
                        }
                    }
                    if el && right - nw >= 0.0 {
                        left = right - nw;
                    }
                }
                prep.rect = (left, top, nw.max(min_size), nh.max(min_size));
            }
        }
        cx.notify();
    }

    /// 根据当前鼠标位置返回录屏准备态下的调整大小/平移光标。
    fn prep_cursor(&self) -> CursorStyle {
        let Some(ref prep) = self.recording_prep else {
            return CursorStyle::Crosshair;
        };
        if let Some(drag) = prep.drag {
            return match drag.kind {
                RecDragKind::Move => CursorStyle::PointingHand,
                RecDragKind::Resize(l, t, r, b) => resize_cursor(l, t, r, b),
            };
        }
        let Some(p) = self.pointer else {
            return CursorStyle::Arrow;
        };
        let (rx, ry, rw, rh) = prep.rect;
        let tol = 14.0;
        let in_y = p.1 >= ry - tol && p.1 <= ry + rh + tol;
        let in_x = p.0 >= rx - tol && p.0 <= rx + rw + tol;
        let edge_l = in_y && (p.0 - rx).abs() <= tol;
        let edge_r = in_y && (p.0 - (rx + rw)).abs() <= tol;
        let edge_t = in_x && (p.1 - ry).abs() <= tol;
        let edge_b = in_x && (p.1 - (ry + rh)).abs() <= tol;
        if edge_l || edge_r || edge_t || edge_b {
            resize_cursor(edge_l, edge_t, edge_r, edge_b)
        } else if p.0 >= rx && p.0 <= rx + rw && p.1 >= ry && p.1 <= ry + rh {
            CursorStyle::PointingHand
        } else {
            CursorStyle::Crosshair
        }
    }

    /// 确认开始录屏：关闭全部浮层，保存录屏偏好并启动后台推流编码。
    fn start_recording_from_prep(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prep) = self.recording_prep.take() else {
            return;
        };
        if self.session.is_finished() {
            return;
        }
        let (sx, sy) = self.scale();
        let (x, y, w, h) = prep.rect;
        // H.264 YUV420 要求宽高为偶数，向下取整并确保不越界。
        let px_x = (x * sx).round().clamp(0.0, self.img_w as f32 - 16.0) as u32;
        let px_y = (y * sy).round().clamp(0.0, self.img_h as f32 - 16.0) as u32;
        let px_w = (((w * sx).round() as u32).min(self.img_w - px_x) & !1).max(16);
        let px_h = (((h * sy).round() as u32).min(self.img_h - px_y) & !1).max(16);

        let format = prep.format;
        let fps = prep.fps;
        let system_audio = prep.system_audio;
        let mic_audio = prep.mic_audio;
        let show_cursor = prep.show_cursor;
        config::update(|cfg| {
            cfg.record.format = format.as_str().to_string();
            cfg.record.fps = fps;
            cfg.record.system_audio = system_audio;
            cfg.record.mic_audio = mic_audio;
            cfg.record.show_cursor = show_cursor;
        });

        let spec = record::RecordSpec {
            display_id: self.display_id,
            display_bounds: self.display_bounds,
            view_rect: prep.rect,
            crop_px: (px_x, px_y, px_w, px_h),
            border_px: (
                self.origin_px.0.round() as i32 + px_x as i32 - 3,
                self.origin_px.1.round() as i32 + px_y as i32 - 3,
                px_w as i32 + 6,
                px_h as i32 + 6,
            ),
            format,
            fps,
            system_audio,
            mic_audio,
            show_cursor,
        };

        self.session.finish(Some(self.index), cx);
        let main = self.main.clone();
        main.update(cx, |state, cx| {
            state.capturing = false;
            if let Err(err) = record::start_recording(spec, cx.entity(), cx) {
                let msg = tr_fmt("rec.failed", &[("msg", &err)]);
                state.set_status(&msg, cx);
                crate::toast::show(cx, msg);
            }
        });
        window.remove_window();
        cx.refresh_windows();
    }

    /// 渲染录屏准备态的悬浮控制条（比例预设、标准尺寸、格式/帧率/声音、开始/取消）。
    fn render_record_prep_bar(
        &self,
        rect: (f32, f32, f32, f32),
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(ref prep) = self.recording_prep else {
            return div().id("rec-prep-bar-empty");
        };
        let p = palette();
        let (sx, sy) = self.scale();
        let phys_w = ((rect.2 * sx).round() as u32 & !1).max(16);
        let phys_h = ((rect.3 * sy).round() as u32 & !1).max(16);
        let bar_w = 604.0f32;
        let bar_h = 74.0f32;
        let bar_x =
            (rect.0 + (rect.2 - bar_w) * 0.5).clamp(8.0, (self.view_w - bar_w - 8.0).max(8.0));
        let bar_y = if rect.1 + rect.3 + bar_h + 12.0 <= self.view_h {
            rect.1 + rect.3 + 8.0
        } else {
            (rect.1 - bar_h - 8.0).max(8.0)
        };

        let mut row1 = div().flex().items_center().gap_1();
        for preset in record::AspectPreset::ALL {
            let active = prep.ratio == preset;
            row1 = row1.child(
                div()
                    .id(("rec-ratio", preset as usize))
                    .px_2()
                    .py_0p5()
                    .rounded_sm()
                    .text_xs()
                    .cursor_pointer()
                    .bg(rgb(if active { p.selected } else { p.card }))
                    .text_color(rgb(if active { p.selected_text } else { p.text }))
                    .border_1()
                    .border_color(rgb(p.border))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let bounds = (this.view_w, this.view_h);
                        if let Some(ref mut pr) = this.recording_prep {
                            pr.ratio = preset;
                            if let Some(ratio) = preset.ratio() {
                                pr.rect = record::fit_rect_to_ratio(pr.rect, ratio, bounds);
                            }
                            cx.notify();
                        }
                    }))
                    .child(preset.label()),
            );
        }
        for (idx, (label, tw, th)) in record::SIZE_PRESETS.iter().enumerate() {
            let (tw, th) = (*tw, *th);
            row1 = row1.child(
                div()
                    .id(("rec-size", idx))
                    .px_2()
                    .py_0p5()
                    .rounded_sm()
                    .text_xs()
                    .cursor_pointer()
                    .bg(rgb(p.card))
                    .text_color(rgb(p.muted))
                    .border_1()
                    .border_color(rgb(p.border))
                    .hover(|s| s.bg(rgb(p.selected)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let scale = this.scale();
                        let bounds = (this.view_w, this.view_h);
                        if let Some(ref mut pr) = this.recording_prep {
                            pr.ratio = record::AspectPreset::Free;
                            pr.rect = record::fit_rect_to_size(pr.rect, (tw, th), scale, bounds);
                            cx.notify();
                        }
                    }))
                    .child(*label),
            );
        }
        row1 = row1.child(
            div()
                .id("rec-size-fullscreen")
                .px_2()
                .py_0p5()
                .rounded_sm()
                .text_xs()
                .cursor_pointer()
                .bg(rgb(p.card))
                .text_color(rgb(p.muted))
                .border_1()
                .border_color(rgb(p.border))
                .hover(|s| s.bg(rgb(p.selected)))
                .on_click(cx.listener(|this, _, _, cx| {
                    let bounds = (this.view_w, this.view_h);
                    if let Some(ref mut pr) = this.recording_prep {
                        pr.ratio = record::AspectPreset::Free;
                        pr.rect = (0.0, 0.0, bounds.0, bounds.1);
                        cx.notify();
                    }
                }))
                .child(tr("rec.fullscreen")),
        );

        let mut row2 = div().flex().items_center().justify_between().gap_2().child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(p.muted))
                        .mr_1()
                        .child(format!("{phys_w}×{phys_h}")),
                )
                .child(
                    div()
                        .id("rec-fmt-btn")
                        .px_2()
                        .py_0p5()
                        .rounded_sm()
                        .text_xs()
                        .cursor_pointer()
                        .bg(rgb(p.card))
                        .border_1()
                        .border_color(rgb(p.border))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(ref mut pr) = this.recording_prep {
                                pr.format = match pr.format {
                                    record::RecordFormat::Mp4 => record::RecordFormat::Gif,
                                    record::RecordFormat::Gif => record::RecordFormat::Mp4,
                                };
                                cx.notify();
                            }
                        }))
                        .child(prep.format.as_str().to_uppercase()),
                )
                .child(
                    div()
                        .id("rec-fps-btn")
                        .px_2()
                        .py_0p5()
                        .rounded_sm()
                        .text_xs()
                        .cursor_pointer()
                        .bg(rgb(p.card))
                        .border_1()
                        .border_color(rgb(p.border))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(ref mut pr) = this.recording_prep {
                                pr.fps = match pr.fps {
                                    15 => 30,
                                    30 => 60,
                                    _ => 15,
                                };
                                cx.notify();
                            }
                        }))
                        .child(format!("{} fps", prep.fps)),
                ),
        );

        let mut actions = div().flex().items_center().gap_1().child(
            div()
                .id("rec-cursor-btn")
                .px_2()
                .py_0p5()
                .rounded_sm()
                .text_xs()
                .cursor_pointer()
                .bg(rgb(if prep.show_cursor { p.selected } else { p.card }))
                .text_color(rgb(if prep.show_cursor {
                    p.selected_text
                } else {
                    p.muted
                }))
                .border_1()
                .border_color(rgb(p.border))
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(ref mut pr) = this.recording_prep {
                        pr.show_cursor = !pr.show_cursor;
                        cx.notify();
                    }
                }))
                .child(tr("rec.cursor")),
        );
        if prep.format == record::RecordFormat::Mp4 {
            actions = actions
                .child(
                    div()
                        .id("rec-sys-audio")
                        .px_2()
                        .py_0p5()
                        .rounded_sm()
                        .text_xs()
                        .cursor_pointer()
                        .bg(rgb(if prep.system_audio {
                            p.selected
                        } else {
                            p.card
                        }))
                        .text_color(rgb(if prep.system_audio {
                            p.selected_text
                        } else {
                            p.muted
                        }))
                        .border_1()
                        .border_color(rgb(p.border))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(ref mut pr) = this.recording_prep {
                                pr.system_audio = !pr.system_audio;
                                cx.notify();
                            }
                        }))
                        .child(tr("rec.sys_audio")),
                )
                .child(
                    div()
                        .id("rec-mic-audio")
                        .px_2()
                        .py_0p5()
                        .rounded_sm()
                        .text_xs()
                        .cursor_pointer()
                        .bg(rgb(if prep.mic_audio { p.selected } else { p.card }))
                        .text_color(rgb(if prep.mic_audio {
                            p.selected_text
                        } else {
                            p.muted
                        }))
                        .border_1()
                        .border_color(rgb(p.border))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(ref mut pr) = this.recording_prep {
                                pr.mic_audio = !pr.mic_audio;
                                cx.notify();
                            }
                        }))
                        .child(tr("rec.mic_audio")),
                );
        }
        actions = actions
            .child(
                div()
                    .id("rec-cancel-btn")
                    .px_2()
                    .py_0p5()
                    .rounded_sm()
                    .text_xs()
                    .cursor_pointer()
                    .bg(rgb(p.card))
                    .border_1()
                    .border_color(rgb(p.border))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.cancel(window, cx);
                    }))
                    .child(tr("rec.cancel")),
            )
            .child(
                div()
                    .id("rec-start-btn")
                    .px_3()
                    .py_0p5()
                    .rounded_sm()
                    .text_xs()
                    .cursor_pointer()
                    .bg(rgb(0xC92A2A))
                    .text_color(rgb(0xFFFFFF))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.start_recording_from_prep(window, cx);
                    }))
                    .child(tr("rec.start")),
            );
        row2 = row2.child(actions);

        div()
            .id("rec-prep-bar")
            .absolute()
            .left(px(bar_x))
            .top(px(bar_y))
            .w(px(bar_w))
            .p_2()
            .rounded_md()
            .bg(rgb(p.toolbar_bg))
            .border_1()
            .border_color(rgb(p.border))
            .text_color(rgb(p.text))
            .flex()
            .flex_col()
            .gap_1()
            // 阻止点击控制条穿透到底层画布触发重新框选。
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(row1)
            .child(row2)
    }

    /// 左键拖拽期间按 T：锁定当前选区，关闭浮层后后台识别并直接复制文字。
    fn recognize_selection(
        &mut self,
        rect: (f32, f32, f32, f32),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session.is_finished() {
            return;
        }
        let (x, y, w, h) = rect;
        let (sx, sy) = self.scale();
        let x0 = (x * sx).round().clamp(0.0, self.img_w as f32 - 1.0) as u32;
        let y0 = (y * sy).round().clamp(0.0, self.img_h as f32 - 1.0) as u32;
        let width = ((w * sx).round() as u32).clamp(1, self.img_w - x0);
        let height = ((h * sy).round() as u32).clamp(1, self.img_h - y0);
        let pixels = self.pixels.clone();
        // 先消费拖拽与本轮会话，防止 T 连发或之后松开左键又保存普通截图。
        self.anchor = None;
        self.current = None;
        self.session.finish(Some(self.index), cx);
        self.main.update(cx, |state, cx| {
            state.set_status(&tr("viewer.ocr_running"), cx);
            cx.spawn(async move |weak, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        let image =
                            image::imageops::crop_imm(&*pixels, x0, y0, width, height).to_image();
                        ocr::recognize_image(&image)
                    })
                    .await;
                // 主窗口常驻托盘，浮层关闭后仍能接收结果；OCR 结束前阻止重复截图。
                let _ = weak.update(cx, |state, cx| {
                    state.capturing = false;
                    let message = match result {
                        Ok(text) if !text.is_empty() => {
                            cx.write_to_clipboard(rgpui::ClipboardItem::new_string(text));
                            crate::feedback::completed();
                            tr("ocr.text_copied")
                        }
                        Ok(_) => tr("viewer.ocr_empty"),
                        Err(error) => tr_fmt("viewer.ocr_fail", &[("error", &error.to_string())]),
                    };
                    state.set_status(&message, cx);
                    crate::toast::show(cx, message);
                });
            })
            .detach();
        });
        window.remove_window();
        cx.refresh_windows();
    }

    /// P 键只贴当前选区，立即释放截图浮层，后台裁剪并创建一次纹理。
    fn pin_selection(
        &mut self,
        rect: (f32, f32, f32, f32),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session.is_finished() {
            return;
        }
        let (x, y, w, h) = rect;
        let (sx, sy) = self.scale();
        let x0 = (x * sx).round().clamp(0.0, self.img_w as f32 - 1.0) as u32;
        let y0 = (y * sy).round().clamp(0.0, self.img_h as f32 - 1.0) as u32;
        let width = ((w * sx).round() as u32).clamp(1, self.img_w - x0);
        let height = ((h * sy).round() as u32).clamp(1, self.img_h - y0);
        let position = (self.origin_px.0 / sx + x, self.origin_px.1 / sy + y);
        let pixels = self.pixels.clone();
        self.anchor = None;
        self.current = None;
        self.session.finish(Some(self.index), cx);
        self.main.update(cx, |_, cx| {
            cx.spawn(async move |weak, cx| {
                let image = cx
                    .background_executor()
                    .spawn(async move {
                        crate::pin::PinImage::new(
                            image::imageops::crop_imm(&*pixels, x0, y0, width, height).to_image(),
                        )
                    })
                    .await;
                let _ = weak.update(cx, |state, cx| {
                    state.capturing = false;
                    if let Err(error) = crate::pin::open(image, Some(position), sx, cx) {
                        let message = tr_fmt("pin.failed", &[("error", &error)]);
                        state.set_status(&message, cx);
                        crate::toast::show(cx, message);
                    }
                });
            })
            .detach();
        });
        window.remove_window();
        cx.refresh_windows();
    }

    /// 完成选择：裁剪首帧选区，交统一收尾（保存 / 复制 / 编辑窗 / 提示）后关闭本轮浮层。
    fn finish(&mut self, window: &mut Window, cx: &mut Context<Self>, rect: (f32, f32, f32, f32)) {
        self.finish_selection(window, cx, rect, false, false);
    }

    /// 按 B 键完成框选并强制打开标注编辑窗。
    fn finish_to_editor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        rect: (f32, f32, f32, f32),
    ) {
        self.finish_selection(window, cx, rect, false, true);
    }

    /// S 键显式保存；普通松手仍仅复制；B 键强制打开标注编辑窗。
    fn finish_selection(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        rect: (f32, f32, f32, f32),
        save: bool,
        force_editor: bool,
    ) {
        // 本轮已在别的屏上收尾：这里只关自己，避免重复裁剪 / 重复保存。
        if self.session.is_finished() {
            window.remove_window();
            return;
        }
        let (x, y, w, h) = rect;
        // 换算到图片物理像素并钳制在图内。
        let (sx, sy) = self.scale();
        let cx0 = (x * sx).round().clamp(0.0, self.img_w as f32 - 1.0) as u32;
        let cy0 = (y * sy).round().clamp(0.0, self.img_h as f32 - 1.0) as u32;
        let cw = ((w * sx).round() as u32).clamp(1, self.img_w - cx0);
        let ch = ((h * sy).round() as u32).clamp(1, self.img_h - cy0);
        let pixels = self.pixels.clone();
        let main = self.main.clone();
        self.session.finish(Some(self.index), cx);
        // 立即移除浮层，裁剪、编码和剪贴板写入在后台完成，避免松手后画面卡住。
        main.update(cx, |_, cx| {
            cx.spawn(async move |weak, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        let image =
                            image::imageops::crop_imm(&*pixels, cx0, cy0, cw, ch).to_image();
                        if save {
                            crate::app::save_capture(&image)
                        } else {
                            crate::app::prepare_capture(&image)
                        }
                    })
                    .await;
                let _ = weak.update(cx, |state, cx| {
                    state.capturing = false;
                    match result {
                        Ok(capture) => {
                            if save {
                                crate::app::finish_saved_capture(state, cx, capture);
                            } else {
                                let (preview, _) = finish_capture(state, cx, capture);
                                if force_editor && !state.open_editor {
                                    crate::viewer::open_viewer(cx.entity(), cx, preview);
                                }
                            }
                        }
                        Err(error) => {
                            state.set_status(&tr_fmt("cap.crop_fail", &[("error", &error)]), cx)
                        }
                    }
                });
            })
            .detach();
        });
        window.remove_window();
        cx.refresh_windows();
    }

    /// 取消本次截图：复位状态机并关闭本轮所有浮层。
    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 本轮已在别处收尾：只关自己。
        if self.session.is_finished() {
            window.remove_window();
            return;
        }
        self.session.finish(Some(self.index), cx);
        let main = self.main.clone();
        main.update(cx, |state, cx| {
            state.capturing = false;
            state.set_status(&tr("cap.cancelled"), cx);
        });
        window.remove_window();
        // 同 finish：保证主窗口重画，不残留“截取中”灰按钮。
        cx.refresh_windows();
    }
}

/// 桌面全局物理矩形 → 本屏视图坐标矩形（只保留落在本屏内的部分）。
///
/// # 参数
/// - `rect`: 窗口在桌面物理坐标系中的矩形（左、上、右、下）。
/// - `origin_px`: 本屏在桌面物理坐标系中的原点。
/// - `scale`: 视图逻辑像素 → 图片物理像素的缩放比。
/// - `view`: 本屏视图的逻辑宽高。
///
/// # 返回
/// 裁剪到本屏范围内的视图矩形；完全不在本屏时返回 None。
fn view_rect_from_physical(
    rect: (i32, i32, i32, i32),
    origin_px: (f32, f32),
    scale: (f32, f32),
    view: (f32, f32),
) -> Option<(f32, f32, f32, f32)> {
    let (l, t, r, b) = rect;
    let sx = scale.0.max(f32::EPSILON);
    let sy = scale.1.max(f32::EPSILON);
    // 减去本屏原点再除以缩放比；跨屏窗口只保留落在本屏内的部分。
    let x0 = ((l as f32 - origin_px.0) / sx).max(0.0);
    let y0 = ((t as f32 - origin_px.1) / sy).max(0.0);
    let x1 = ((r as f32 - origin_px.0) / sx).min(view.0);
    let y1 = ((b as f32 - origin_px.1) / sy).min(view.1);
    if x1 - x0 < 1.0 || y1 - y0 < 1.0 {
        return None;
    }
    Some((x0, y0, x1 - x0, y1 - y0))
}

/// 两点归一化为视图内矩形（起点终点任意顺序），退化时返回 None。
fn norm_rect(a: (f32, f32), b: (f32, f32), vw: f32, vh: f32) -> Option<(f32, f32, f32, f32)> {
    let x = a.0.min(b.0).clamp(0.0, vw);
    let y = a.1.min(b.1).clamp(0.0, vh);
    let w = (a.0 - b.0).abs().min(vw - x);
    let h = (a.1 - b.1).abs().min(vh - y);
    if w < 1.0 || h < 1.0 {
        return None;
    }
    Some((x, y, w, h))
}

/// 平移选区并限制在屏幕内，保留反向拖拽的起终点和原有宽高。
fn translate_selection(
    a: (f32, f32),
    b: (f32, f32),
    delta: (f32, f32),
    view: (f32, f32),
) -> ((f32, f32), (f32, f32)) {
    let dx = delta.0.clamp(-a.0.min(b.0), view.0 - a.0.max(b.0));
    let dy = delta.1.clamp(-a.1.min(b.1), view.1 - a.1.max(b.1));
    ((a.0 + dx, a.1 + dy), (b.0 + dx, b.1 + dy))
}

/// 框选内的单键命令，不吞掉用户的 Ctrl/Alt/系统键组合。
fn plain_key(key: &rgpui::Keystroke, name: &str) -> bool {
    key.key.eq_ignore_ascii_case(name)
        && !key.modifiers.control
        && !key.modifiers.alt
        && !key.modifiers.platform
}

/// 只有拖出有效选区时按 T 才识别；组合快捷键和未拖拽状态不触发。
fn ocr_drag_rect(
    key: &rgpui::Keystroke,
    rect: Option<(f32, f32, f32, f32)>,
) -> Option<(f32, f32, f32, f32)> {
    if !plain_key(key, "t") {
        return None;
    }
    rect.filter(|(_, _, w, h)| *w >= 5.0 && *h >= 5.0)
}

impl Render for SelectView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 薄遮罩：透出真实屏幕，用户能看清又能感知是截图态。
        let mask = rgba(0x00000059);
        let border_blue = rgb(0x0078D4);
        let sel = self.sel_rect();
        // 徽标显示图片物理像素尺寸（与裁剪结果一致）。
        let (sx, sy) = self.scale();
        // 按显示器缩放折算，描边保持一个物理像素。
        let (border_x, border_y) = (1.0 / sx, 1.0 / sy);
        let prep_rect = self.recording_prep.as_ref().map(|p| p.rect);
        let show = prep_rect
            .or(sel)
            .or_else(|| self.hover.as_ref().map(|(rect, _)| *rect));

        let mut root = div()
            .id("region-overlay")
            .size_full()
            .relative()
            // 底层：冻结的首帧全屏图（逻辑尺寸铺满窗口即 1:1 物理像素，
            // 鼠标坐标 × scale 即图片像素，无近似）。
            // 预解码同步源首帧立现；解码失败才回退路径异步加载（会闪一下）。
            .child(
                div().absolute().size_full().child(
                    img(self.frame_image.clone())
                        .w(px(self.view_w))
                        .h(px(self.view_h)),
                ),
            )
            // 根视图可聚焦，使 Esc 在浮层打开后立即可用。
            .cursor(if prep_rect.is_some() {
                self.prep_cursor()
            } else {
                CursorStyle::Crosshair
            })
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                // 拖拽拉框期间不因按键引发的瞬时 hover 抖动隐藏小卡片。
                if !*hovered && this.anchor.is_none() {
                    this.pointer = None;
                    cx.notify();
                }
            }))
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                if ev.keystroke.key.as_str() == "escape" {
                    this.cancel(window, cx);
                } else if this.recording_prep.is_some() {
                    if ev.keystroke.key.as_str() == "enter" {
                        this.start_recording_from_prep(window, cx);
                    } else if plain_key(&ev.keystroke, "f") {
                        // 录屏准备态下按 F 一键切到全屏录制范围。
                        let bounds = (this.view_w, this.view_h);
                        if let Some(ref mut pr) = this.recording_prep {
                            pr.ratio = record::AspectPreset::Free;
                            pr.rect = (0.0, 0.0, bounds.0, bounds.1);
                            cx.notify();
                        }
                    }
                } else if ev.keystroke.key.as_str() == "space" {
                    this.space_held = true;
                } else if plain_key(&ev.keystroke, "f") {
                    // 截图模式下按 F 一键截取当前显示器全屏。
                    this.anchor = None;
                    this.current = None;
                    this.finish(window, cx, (0.0, 0.0, this.view_w, this.view_h));
                } else if plain_key(&ev.keystroke, "b") {
                    // 按 B（Brush）选定当前区域或悬停窗口，直接打开标注编辑界面。
                    if let Some(rect) = this
                        .sel_rect()
                        .or_else(|| this.hover.as_ref().map(|(r, _)| *r))
                    {
                        this.anchor = None;
                        this.current = None;
                        this.finish_to_editor(window, cx, rect);
                    }
                } else if plain_key(&ev.keystroke, "c") {
                    this.copy_color(window, cx);
                } else if plain_key(&ev.keystroke, "q") {
                    this.open_qr(window, cx);
                } else if plain_key(&ev.keystroke, "v") {
                    this.enter_recording_prep(cx);
                } else if plain_key(&ev.keystroke, "p") {
                    if let Some(rect) = this.sel_rect() {
                        this.pin_selection(rect, window, cx);
                    }
                } else if plain_key(&ev.keystroke, "s") {
                    if let Some(rect) = this.sel_rect() {
                        this.anchor = None;
                        this.current = None;
                        this.finish_selection(window, cx, rect, true, false);
                    }
                } else if let Some(rect) = ocr_drag_rect(&ev.keystroke, this.sel_rect()) {
                    this.recognize_selection(rect, window, cx);
                }
            }))
            .on_key_up(cx.listener(|this, ev: &KeyUpEvent, _, _| {
                if ev.keystroke.key.as_str() == "space" {
                    this.space_held = false;
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _window, cx| {
                    this.on_left_down(ev, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _window, cx| {
                this.on_mouse_move(ev, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseUpEvent, window, cx| {
                    this.on_left_up(ev, window, cx);
                }),
            )
            // 拖出窗口松开时 `on_mouse_up` 可能收不到，兜底提交。
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseUpEvent, window, cx| {
                    this.on_left_up(ev, window, cx);
                }),
            )
            // 右键任意处取消。
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _ev: &MouseDownEvent, window, cx| {
                    this.cancel(window, cx);
                }),
            );

        match show {
            None => {
                // 尚未框选：整屏薄遮罩。
                root = root.child(div().absolute().size_full().bg(mask));
            }
            Some((x, y, w, h)) => {
                // 选区外四条遮罩，选区内透出真实屏幕。
                root = root
                    .child(
                        div()
                            .absolute()
                            .left(px(0.))
                            .top(px(0.))
                            .w(px(self.view_w))
                            .h(px(y))
                            .bg(mask),
                    )
                    .child(
                        div()
                            .absolute()
                            .left(px(0.))
                            .top(px(y + h))
                            .w(px(self.view_w))
                            .h(px(self.view_h - y - h))
                            .bg(mask),
                    )
                    .child(
                        div()
                            .absolute()
                            .left(px(0.))
                            .top(px(y))
                            .w(px(x))
                            .h(px(h))
                            .bg(mask),
                    )
                    .child(
                        div()
                            .absolute()
                            .left(px(x + w))
                            .top(px(y))
                            .w(px(self.view_w - x - w))
                            .h(px(h))
                            .bg(mask),
                    )
                    // 常规截图使用蓝色描边，录屏准备态改用红色描边提示可调录制框。
                    .child(
                        div()
                            .absolute()
                            .left(px(x))
                            .top(px(y))
                            .w(px(w))
                            .h(px(border_y))
                            .bg(if prep_rect.is_some() {
                                rgb(0xE03131)
                            } else {
                                border_blue
                            }),
                    )
                    .child(
                        div()
                            .absolute()
                            .left(px(x))
                            .top(px(y + h - border_y))
                            .w(px(w))
                            .h(px(border_y))
                            .bg(if prep_rect.is_some() {
                                rgb(0xE03131)
                            } else {
                                border_blue
                            }),
                    )
                    .child(
                        div()
                            .absolute()
                            .left(px(x))
                            .top(px(y))
                            .w(px(border_x))
                            .h(px(h))
                            .bg(if prep_rect.is_some() {
                                rgb(0xE03131)
                            } else {
                                border_blue
                            }),
                    )
                    .child(
                        div()
                            .absolute()
                            .left(px(x + w - border_x))
                            .top(px(y))
                            .w(px(border_x))
                            .h(px(h))
                            .bg(if prep_rect.is_some() {
                                rgb(0xE03131)
                            } else {
                                border_blue
                            }),
                    );
            }
        }
        // 录屏准备态：在选区四角与四边中点绘制拖拽手柄，并在外侧展示控制条。
        if let Some(rect) = prep_rect {
            let (rx, ry, rw, rh) = rect;
            let handles = [
                (rx, ry),
                (rx + rw * 0.5, ry),
                (rx + rw, ry),
                (rx, ry + rh * 0.5),
                (rx + rw, ry + rh * 0.5),
                (rx, ry + rh),
                (rx + rw * 0.5, ry + rh),
                (rx + rw, ry + rh),
            ];
            for (hx, hy) in handles {
                root = root.child(
                    div()
                        .absolute()
                        .left(px(hx - 4.0))
                        .top(px(hy - 4.0))
                        .w(px(8.0))
                        .h(px(8.0))
                        .rounded_sm()
                        .bg(rgb(0xFFFFFF))
                        .border_1()
                        .border_color(rgb(0xE03131)),
                );
            }
            root = root.child(self.render_record_prep_bar(rect, cx));
        }
        // 提示点只加入渲染树，所有裁剪/取色/保存仍读取未修改的 self.pixels。
        if let Some(code) = self.selected_qr() {
            let x = (code.bounds[0] + code.bounds[2]) / (2.0 * sx);
            let y = (code.bounds[1] + code.bounds[3]) / (2.0 * sy);
            root = root.child(
                div()
                    .absolute()
                    .left(px(x - 10.0))
                    .top(px(y - 10.0))
                    .w(px(20.0))
                    .h(px(20.0))
                    .rounded_full()
                    .bg(rgba(0x0078D445))
                    .flex()
                    .items_center()
                    .justify_center()
                    // 仅保留小蓝点指示二维码位置，不额外绘制字母标签以免遮挡画面。
                    .child(
                        div()
                            .w(px(10.0))
                            .h(px(10.0))
                            .rounded_full()
                            .bg(rgb(0x0078D4))
                            .border_1()
                            .border_color(rgb(0xFFFFFF)),
                    ),
            );
        }
        if prep_rect.is_none()
            && let Some(pointer) = self.pointer
        {
            let sample = picker::pointer_pixel(pointer, (sx, sy), (self.img_w, self.img_h));
            let desktop = (
                (self.origin_px.0 + sample.0 as f32).round() as i32,
                (self.origin_px.1 + sample.1 as f32).round() as i32,
            );
            let dimensions =
                show.map(|(_, _, w, h)| ((w * sx).round() as u32, (h * sy).round() as u32));
            let (x, y) = picker::card_position(pointer, (self.view_w, self.view_h));
            root = root.child(
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .child(picker::color_card(
                        &self.pixels,
                        sample,
                        desktop,
                        dimensions,
                    )),
            );
        }
        root
    }
}

#[cfg(test)]
mod tests {
    //! 浮层坐标换算单测：副屏原点偏移与跨屏裁剪（纯函数，不依赖窗口）。

    use super::*;

    /// 平移时尺寸不变，反向框选及碰到屏幕四边都不能挤压或移出选区。
    #[test]
    fn move_selection_preserves_size_and_bounds() {
        let a = (60.0, 50.0);
        let b = (20.0, 10.0);
        assert_eq!(
            translate_selection(a, b, (5.0, 8.0), (100.0, 100.0)),
            ((65.0, 58.0), (25.0, 18.0))
        );
        assert_eq!(
            translate_selection(a, b, (-500.0, -500.0), (100.0, 100.0)),
            ((40.0, 40.0), (0.0, 0.0))
        );
        assert_eq!(
            translate_selection(a, b, (500.0, 500.0), (100.0, 100.0)),
            ((100.0, 100.0), (60.0, 60.0))
        );
    }

    /// C 只在无 Ctrl/Alt 的单键模式触发，不能覆盖常用复制快捷键。
    #[test]
    fn local_color_shortcut() {
        assert!(plain_key(&rgpui::Keystroke::parse("q").unwrap(), "q"));
        assert!(!plain_key(&rgpui::Keystroke::parse("ctrl-q").unwrap(), "q"));
        assert!(!plain_key(&rgpui::Keystroke::parse("alt-q").unwrap(), "q"));
        assert!(plain_key(&rgpui::Keystroke::parse("s").unwrap(), "s"));
        assert!(!plain_key(&rgpui::Keystroke::parse("ctrl-s").unwrap(), "s"));
        assert!(plain_key(&rgpui::Keystroke::parse("c").unwrap(), "c"));
        assert!(!plain_key(&rgpui::Keystroke::parse("ctrl-c").unwrap(), "c"));
        assert!(!plain_key(&rgpui::Keystroke::parse("alt-c").unwrap(), "c"));
        assert!(!plain_key(&rgpui::Keystroke::parse("t").unwrap(), "c"));
    }

    /// 拖框加 T 才识别，松手后无选区、极小框和其他组合键不能误触。
    #[test]
    fn drag_t_trigger() {
        let rect = Some((10.0, 20.0, 100.0, 30.0));
        let key = rgpui::Keystroke::parse("t").unwrap();
        assert_eq!(ocr_drag_rect(&key, rect), rect);
        assert_eq!(ocr_drag_rect(&key, None), None);
        assert_eq!(ocr_drag_rect(&key, Some((0.0, 0.0, 2.0, 10.0))), None);
        for value in ["ctrl-t", "alt-t", "r"] {
            assert_eq!(
                ocr_drag_rect(&rgpui::Keystroke::parse(value).unwrap(), rect),
                None
            );
        }
    }

    /// 副屏窗口：减去屏物理原点后换算到本屏视图坐标。
    #[test]
    fn secondary_origin_offset() {
        let rect = view_rect_from_physical(
            (3470, 66, 5065, 996),
            (3440.0, 0.0),
            (1.0, 1.0),
            (1920.0, 1080.0),
        );
        assert_eq!(rect, Some((30.0, 66.0, 1595.0, 930.0)));
    }

    /// 主屏 125% 缩放：物理矩形按缩放比换回逻辑视图坐标。
    #[test]
    fn primary_scaled_rect() {
        let rect = view_rect_from_physical(
            (151, 160, 1650, 930),
            (0.0, 0.0),
            (1.25, 1.25),
            (2752.0, 1152.0),
        );
        let (x, y, w, h) = rect.expect("主屏窗口应命中");
        assert!((x - 120.8).abs() < 0.01);
        assert!((y - 128.0).abs() < 0.01);
        assert!((w - 1199.2).abs() < 0.01);
        assert!((h - 616.0).abs() < 0.01);
    }

    /// 跨屏窗口裁剪到本屏范围；完全在屏外的窗口不产生高亮。
    #[test]
    fn cross_display_clamp() {
        let view = (1920.0, 1080.0);
        assert_eq!(
            view_rect_from_physical((3300, 100, 3800, 500), (3440.0, 0.0), (1.0, 1.0), view),
            Some((0.0, 100.0, 360.0, 400.0))
        );
        assert_eq!(
            view_rect_from_physical((100, 100, 500, 500), (3440.0, 0.0), (1.0, 1.0), view),
            None
        );
    }
}
