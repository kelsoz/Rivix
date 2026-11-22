//! 屏幕取色模块：在冻结首帧上移动取色，点击复制 HEX，右键 / Esc 取消。
//!
//! 与区域截图同一帧来源：看到的是截取瞬间的静态首帧（1:1 物理像素），
//! 采样坐标严格按“视图逻辑像素 × 缩放比 = 图片物理像素”换算。
//! 多显示器时每块屏各开一层取色浮层，可在任意屏幕上取色。

use std::{rc::Rc, sync::Arc};

use image::RgbaImage;
use rgpui::{
    Bounds, ClipboardItem, Context, CursorStyle, Div, Entity, FocusHandle, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Render, Rgba, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, canvas, div, fill, img,
    point, prelude::*, px, rgb, rgba, size,
};

use crate::{
    app::RivixApp,
    capture::{CapturedFrame, cap_log},
    i18n::{tr, tr_fmt},
    overlay::OverlaySession,
};

/// 在每块屏上各打开一层取色浮层（多显示器可分别在任意屏幕取色）。
///
/// # 参数
/// - `main`: 设置窗口实体（复制成功后回写状态）。
/// - `cx`: 调用方上下文。
/// - `frames`: 每块屏的首帧图片与显示器边界。
///
/// # 返回
/// 至少成功打开一层返回 `true`（之后由取色完成 / 取消复位状态）；
/// 全部建窗失败返回 `false`，调用方需自行复位「截取中」状态。
pub fn open_pickers(
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
        if session.is_finished() {
            break;
        }
        if open_picker(main.clone(), cx, frame, index, session.clone()) {
            opened += 1;
        }
    }
    opened > 0
}

/// 打开单块屏的取色浮层。
fn open_picker(
    main: Entity<RivixApp>,
    cx: &mut Context<RivixApp>,
    frame: CapturedFrame,
    index: usize,
    session: Rc<OverlaySession>,
) -> bool {
    let (img_w, img_h) = (frame.img_w, frame.img_h);
    let display_bounds = frame.display_bounds;
    let display_id = frame.display_id;
    let pixels = frame.pixels;
    let frame_image = frame.frame_image;
    cap_log(&format!(
        "取色浮层打开：屏 {index} img={img_w}x{img_h} id={display_id:?}"
    ));
    let session_for_build = session.clone();
    let opened = cx.open_window(
        WindowOptions {
            // 无边框置顶浮层，与区域选择浮层同规格（Windows 下必须 Fullscreen）。
            // 背景同样取 Opaque：底色是铺满全屏的冻结截图，不需要窗口级 alpha，
            // 而 X11 上的 ARGB visual 在 alpha 合成不可用时会让浮层整体不可见。
            titlebar: None,
            window_bounds: Some(WindowBounds::Fullscreen(display_bounds)),
            // 指定目标显示器，副屏才能在自己的物理边界上铺开。
            display_id,
            window_background: WindowBackgroundAppearance::Opaque,
            kind: WindowKind::Overlay,
            focus: true,
            show: true,
            ..Default::default()
        },
        move |window, cx| {
            // 浮层被系统直接关掉（Alt+F4 等）时结束整轮并复位状态机，避免卡在「截取中」。
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
                PickerView::new(
                    window,
                    cx,
                    main.clone(),
                    pixels.clone(),
                    img_w,
                    img_h,
                    display_bounds,
                    frame_image.clone(),
                    session_for_build.clone(),
                    index,
                )
            });
            // 打开即聚焦根视图并激活窗口，使 Esc 立即可用。
            let handle = view.read(cx).focus_handle.clone();
            window.focus(&handle, cx);
            window.activate_window();
            view
        },
    );
    match opened {
        Ok(handle) => {
            session.register(index, *handle);
            true
        }
        Err(error) => {
            cap_log(&format!("取色浮层打开失败：{error:?}"));
            false
        }
    }
}

/// 取色浮层视图：冻结帧 + 十字线 + 放大镜，点击复制。
struct PickerView {
    /// 设置窗口实体（回写复制状态）。
    main: Entity<RivixApp>,
    /// 首帧图片路径（浮层背景显示用）。
    /// 预解码的首帧（背景同步显示用）。
    frame_image: Arc<rgpui::RenderImage>,
    /// 当前显示器的桌面物理原点。
    origin_px: (f32, f32),
    /// 采样用底图像素（None 时取色不可用，直接取消）。
    img: Option<Arc<RgbaImage>>,
    /// 全图宽（图片像素）。
    img_w: u32,
    /// 全图高（图片像素）。
    img_h: u32,
    /// 浮层逻辑宽（显示器逻辑像素，鼠标坐标空间）。
    view_w: f32,
    /// 浮层逻辑高。
    view_h: f32,
    /// 当前鼠标位置（视图逻辑像素），None 表示尚未移入。
    cursor: Option<(f32, f32)>,
    /// 根视图焦点（Esc 取消用）。
    focus_handle: FocusHandle,
    /// 本轮多屏取色浮层的共享会话。
    session: Rc<OverlaySession>,
    /// 本浮层对应的屏序号（会话内唯一）。
    index: usize,
}

impl PickerView {
    /// 创建取色视图。
    #[allow(clippy::too_many_arguments)]
    fn new(
        _window: &mut Window,
        cx: &mut Context<Self>,
        main: Entity<RivixApp>,
        pixels: Arc<image::RgbaImage>,
        img_w: u32,
        img_h: u32,
        display_bounds: Bounds<Pixels>,
        frame_image: Arc<rgpui::RenderImage>,
        session: Rc<OverlaySession>,
        index: usize,
    ) -> Self {
        let img = Some(pixels.clone());
        Self {
            main,
            frame_image,
            origin_px: (
                display_bounds.origin.x.as_f32() * img_w as f32
                    / display_bounds.size.width.as_f32(),
                display_bounds.origin.y.as_f32() * img_h as f32
                    / display_bounds.size.height.as_f32(),
            ),
            img,
            img_w,
            img_h,
            view_w: display_bounds.size.width.as_f32(),
            view_h: display_bounds.size.height.as_f32(),
            cursor: None,
            focus_handle: cx.focus_handle(),
            session,
            index,
        }
    }

    /// 视图逻辑像素 → 图片物理像素的缩放比。
    fn scale(&self) -> (f32, f32) {
        (
            self.img_w as f32 / self.view_w.max(1.0),
            self.img_h as f32 / self.view_h.max(1.0),
        )
    }

    /// 当前光标处的采样颜色（越界返回 None）。
    fn sample(&self) -> Option<(u8, u8, u8)> {
        let img = self.img.as_ref()?;
        let (cx, cy) = self.cursor?;
        let (sx, sy) = self.scale();
        let x = (cx * sx).round() as i32;
        let y = (cy * sy).round() as i32;
        sample_at(img, x, y)
    }

    /// 鼠标移动：更新光标位置。
    fn on_move(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        let p = (ev.position.x.as_f32(), ev.position.y.as_f32());
        self.cursor = Some(p);
        cx.notify();
    }

    /// 左键点击：复制当前颜色 HEX 到剪贴板并关闭本轮所有取色浮层。
    fn on_pick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 本轮已在别的屏上取过色：只关自己，避免重复写剪贴板 / 重复回状态。
        if self.session.is_finished() {
            window.remove_window();
            return;
        }
        let main = self.main.clone();
        self.session.finish(Some(self.index), cx);
        match self.sample() {
            Some((r, g, b)) => {
                let hex = rgb_to_hex(r, g, b);
                cx.write_to_clipboard(ClipboardItem::new_string(hex.clone()));
                crate::feedback::completed();
                main.update(cx, |state, cx| {
                    // 取色结束，复位状态机（浮层期间一直是截取中）。
                    state.capturing = false;
                    let msg = tr_fmt("picker.copied", &[("hex", &hex)]);
                    state.set_status(&msg, cx);
                });
            }
            None => {
                main.update(cx, |state, cx| {
                    state.capturing = false;
                    state.set_status(&tr("picker.fail"), cx);
                });
            }
        }
        window.remove_window();
        // 跨窗口 update 的重画请求随浮层关闭可能丢失，强制刷新落定。
        cx.refresh_windows();
    }

    /// 取消取色：复位状态机并关闭本轮所有取色浮层。
    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        cx.refresh_windows();
    }
}

/// 采样图片指定物理像素（越界返回 None）。
pub(crate) fn sample_at(img: &RgbaImage, x: i32, y: i32) -> Option<(u8, u8, u8)> {
    if x < 0 || y < 0 || x >= img.width() as i32 || y >= img.height() as i32 {
        return None;
    }
    let p = img.get_pixel(x as u32, y as u32);
    Some((p[0], p[1], p[2]))
}

/// RGB 转 `#RRGGBB` 大写 HEX。
pub(crate) fn rgb_to_hex(r: u8, g: u8, b: u8) -> String {
    format!("#{r:02X}{g:02X}{b:02X}")
}

/// 放大镜某格的颜色（中心为光标像素，向外扩散，越界按边缘钳制）。
fn mag_cell(img: &RgbaImage, cx: i32, cy: i32, dx: i32, dy: i32) -> Rgba {
    let x = (cx + dx).clamp(0, img.width() as i32 - 1);
    let y = (cy + dy).clamp(0, img.height() as i32 - 1);
    let p = img.get_pixel(x as u32, y as u32);
    Rgba {
        r: p[0] as f32 / 255.0,
        g: p[1] as f32 / 255.0,
        b: p[2] as f32 / 255.0,
        a: 1.0,
    }
}

/// 竖卡的固定逻辑尺寸，翻边定位与渲染使用同一组值。
pub(crate) const CARD_W: f32 = 174.0;
pub(crate) const CARD_H: f32 = 224.0;

/// 光标右下优先，碰到屏幕边缘就翻边，最后钳制在当前显示器内。
pub(crate) fn card_position(pointer: (f32, f32), view: (f32, f32)) -> (f32, f32) {
    let gap = 20.0;
    let x = if pointer.0 + gap + CARD_W <= view.0 - 8.0 {
        pointer.0 + gap
    } else {
        pointer.0 - gap - CARD_W
    };
    let y = if pointer.1 + gap + CARD_H <= view.1 - 8.0 {
        pointer.1 + gap
    } else {
        pointer.1 - gap - CARD_H
    };
    (
        x.clamp(8.0, (view.0 - CARD_W - 8.0).max(8.0)),
        y.clamp(8.0, (view.1 - CARD_H - 8.0).max(8.0)),
    )
}

/// 物理像素坐标，向边缘钳制，确保色卡中心格和 C 键采样完全一致。
pub(crate) fn pointer_pixel(
    pointer: (f32, f32),
    scale: (f32, f32),
    dimensions: (u32, u32),
) -> (i32, i32) {
    (
        (pointer.0 * scale.0)
            .round()
            .clamp(0.0, dimensions.0.saturating_sub(1) as f32) as i32,
        (pointer.1 * scale.1)
            .round()
            .clamp(0.0, dimensions.1.saturating_sub(1) as f32) as i32,
    )
}

/// 统一的信息行：左侧标签，右侧等宽数值，与 HTML 定稿版对齐。
fn mini_row(label: String, value: String) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .h(px(14.0))
        .text_size(px(11.0))
        .child(div().text_color(rgb(0x929292)).child(label))
        .child(
            div()
                .font_family("Consolas")
                .text_color(rgb(0xC0C0C0))
                .child(value),
        )
}

/// 原生竖向迷你色卡，直接画 19×11 个像素格，不编码图片、不创建新窗口。
pub(crate) fn color_card(
    image: &RgbaImage,
    sample: (i32, i32),
    desktop: (i32, i32),
    selection: Option<(u32, u32)>,
) -> Div {
    let (r, g, b) = sample_at(image, sample.0, sample.1).unwrap_or((0, 0, 0));
    let hex = rgb_to_hex(r, g, b);
    let swatch = rgb(((r as u32) << 16) | ((g as u32) << 8) | b as u32);
    let cells: Vec<_> = (-5..=5)
        .flat_map(|dy| (-9..=9).map(move |dx| mag_cell(image, sample.0, sample.1, dx, dy)))
        .collect();
    let magnifier = canvas(
        move |bounds, _, _| bounds,
        move |_, paint_bounds, window, _| {
            let origin = paint_bounds.origin;
            for (i, color) in cells.iter().enumerate() {
                window.paint_quad(fill(
                    Bounds::new(
                        point(
                            px(origin.x.as_f32() + (i % 19) as f32 * 8.0),
                            px(origin.y.as_f32() + (i / 19) as f32 * 8.0),
                        ),
                        size(px(8.0), px(8.0)),
                    ),
                    *color,
                ));
            }
            // 中心采样格为唯一白色细框；位置与 19×11 网格的实际中心重合。
            for (x, y, w, h) in [
                (72., 40., 8., 1.),
                (72., 47., 8., 1.),
                (72., 40., 1., 8.),
                (79., 40., 1., 8.),
            ] {
                window.paint_quad(fill(
                    Bounds::new(
                        point(px(origin.x.as_f32() + x), px(origin.y.as_f32() + y)),
                        size(px(w), px(h)),
                    ),
                    rgb(0xE2E2E2),
                ));
            }
        },
    )
    .w(px(152.0))
    .h(px(88.0));
    let selection = selection
        .map(|(w, h)| format!("{w} × {h} px"))
        .unwrap_or_else(|| "— × —".into());
    div()
        .w(px(CARD_W))
        .h(px(CARD_H))
        .p(px(10.0))
        .rounded(px(12.0))
        .border_1()
        .border_color(rgb(0x383838))
        .bg(rgb(0x202020))
        .text_color(rgb(0xC0C0C0))
        .flex()
        .flex_col()
        .child(
            div()
                .relative()
                .w(px(152.0))
                .h(px(88.0))
                .flex_shrink_0()
                .rounded(px(8.0))
                .overflow_hidden()
                .child(magnifier)
                .child(
                    div()
                        .absolute()
                        .right(px(4.0))
                        .bottom(px(3.0))
                        .px_1()
                        .rounded(px(3.0))
                        .bg(rgba(0x181818C0))
                        .text_size(px(11.0))
                        .font_family("Consolas")
                        .child("8×"),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .mt(px(12.0))
                .h(px(20.0))
                .child(div().w(px(14.0)).h(px(14.0)).rounded(px(4.0)).bg(swatch))
                .child(
                    div()
                        .font_family("Consolas")
                        .text_size(px(15.0))
                        .text_color(rgb(0xE5E5E5))
                        .child(hex),
                ),
        )
        .child(div().mt(px(9.0)).child(mini_row(
            tr("mini.coordinates"),
            format!("x {}  y {}", desktop.0, desktop.1),
        )))
        .child(
            div()
                .mt(px(7.0))
                .child(mini_row(tr("mini.selection"), selection)),
        )
        .child(
            div()
                .flex()
                .justify_between()
                .mt(px(10.0))
                .pt(px(8.0))
                .border_t_1()
                .border_color(rgb(0x303030))
                .children(
                    [
                        ("C", tr("mini.pick")),
                        ("T", tr("mini.ocr")),
                        ("S", tr("mini.save")),
                    ]
                    .into_iter()
                    .map(|(key, label)| {
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_size(px(11.0))
                            .text_color(rgb(0x949494))
                            .child(
                                div()
                                    .px_1()
                                    .h(px(18.0))
                                    .rounded(px(4.0))
                                    .border_1()
                                    .border_color(rgb(0x414141))
                                    .bg(rgb(0x282828))
                                    .font_family("Consolas")
                                    .child(key),
                            )
                            .child(label)
                    }),
                ),
        )
}

impl Render for PickerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div()
            .id("picker-overlay")
            .size_full()
            .relative()
            .child(
                img(self.frame_image.clone())
                    .w(px(self.view_w))
                    .h(px(self.view_h)),
            )
            .cursor(CursorStyle::Crosshair)
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered {
                    this.cursor = None;
                    cx.notify();
                }
            }))
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => this.cancel(window, cx),
                    "c" if !event.keystroke.modifiers.control
                        && !event.keystroke.modifiers.alt
                        && !event.keystroke.modifiers.platform =>
                    {
                        this.on_pick(window, cx)
                    }
                    _ => {}
                }
            }))
            .on_mouse_move(
                cx.listener(|this, event: &MouseMoveEvent, _, cx| this.on_move(event, cx)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| this.on_pick(window, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _: &MouseDownEvent, window, cx| this.cancel(window, cx)),
            );
        if let (Some(pointer), Some(image)) = (self.cursor, self.img.as_ref()) {
            let sample = pointer_pixel(
                pointer,
                (
                    self.img_w as f32 / self.view_w,
                    self.img_h as f32 / self.view_h,
                ),
                (self.img_w, self.img_h),
            );
            let (x, y) = card_position(pointer, (self.view_w, self.view_h));
            root = root.child(div().absolute().left(px(x)).top(px(y)).child(color_card(
                image,
                sample,
                (
                    (self.origin_px.0 + sample.0 as f32).round() as i32,
                    (self.origin_px.1 + sample.1 as f32).round() as i32,
                ),
                None,
            )));
        }
        root
    }
}

#[cfg(test)]
mod tests {
    //! 取色模块单测：采样与 HEX 格式化（纯函数，不依赖窗口）。

    use super::*;

    /// 高 DPI、边缘采样与色卡翻边，不能读错像素或跑出屏幕。
    #[test]
    fn mini_card_pixel_and_placement() {
        assert_eq!(
            pointer_pixel((10.0, 20.0), (1.5, 1.5), (100, 100)),
            (15, 30)
        );
        assert_eq!(
            pointer_pixel((-4.0, 100.0), (1.0, 1.0), (100, 100)),
            (0, 99)
        );
        for point in [(0.0, 0.0), (999.0, 799.0), (500.0, 400.0)] {
            let (x, y) = card_position(point, (1000.0, 800.0));
            assert!(x >= 8.0 && y >= 8.0 && x + CARD_W <= 992.0 && y + CARD_H <= 792.0);
        }
        assert!(card_position((999.0, 799.0), (1000.0, 800.0)).0 < 999.0 - CARD_W);
    }

    /// 采样越界与 HEX 格式断言。
    #[test]
    fn sample_and_hex() {
        let mut img = RgbaImage::new(3, 2);
        img.put_pixel(1, 0, image::Rgba([255, 0, 0, 255]));
        assert_eq!(sample_at(&img, 1, 0), Some((255, 0, 0)));
        assert_eq!(sample_at(&img, 0, 0), Some((0, 0, 0)));
        assert_eq!(sample_at(&img, 3, 0), None);
        assert_eq!(sample_at(&img, -1, 0), None);
        assert_eq!(rgb_to_hex(255, 0, 0), "#FF0000");
        assert_eq!(rgb_to_hex(1, 2, 3), "#010203");
    }

    /// 放大镜越界钳制：角落采样不 panic 且返回边缘色。
    #[test]
    fn mag_cell_clamp() {
        let mut img = RgbaImage::new(2, 2);
        img.put_pixel(0, 0, image::Rgba([10, 20, 30, 255]));
        let c = mag_cell(&img, 0, 0, -4, -4);
        assert!((c.r - 10.0 / 255.0).abs() < 1e-6);
    }
}
