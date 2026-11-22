//! 桌面提示：低饱和深色胶囊，底部居中，上升淡入、停留后下降淡出。
//! 使用原生 rgpui 动画与独立透明窗口，不抢焦点，且排除出截图。

use crate::window::exclude_from_capture;
use rgpui::{
    Animation, AnimationExt as _, App, Bounds, Context, Render, SharedString, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, point, prelude::*,
    px, rgb, rgba, size,
};
use std::time::Duration;

/// 一轮提示总时长：200ms 进入、1400ms 停留、200ms 退出。
const TOAST_MS: u64 = 1800;
/// 胶囊逻辑宽高，与字体缩放无关的基础尺寸。
const TOAST_W: f32 = 280.0;
const TOAST_H: f32 = 64.0;
/// 透明窗口留边，供描边及动画移动，避免被窗口裁切。
const PAD: f32 = 12.0;
const SLIDE: f32 = 24.0;
/// 胶囊停留位置距屏幕底部，留出任务栏空间。
const BOTTOM: f32 = 96.0;

/// 提示视图：图标与单行状态，不为不存在的副标题留空。
struct SaveToast {
    /// 状态文字，出错时仍展示实际错误。
    title: SharedString,
}

impl Render for SaveToast {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let card = div()
            .absolute()
            .left(px(PAD))
            .w(px(TOAST_W))
            .h(px(TOAST_H))
            .flex()
            .items_center()
            .justify_center()
            .gap_3()
            .px_5()
            .rounded(px(TOAST_H / 2.0))
            .border_1()
            .border_color(rgb(0x383838))
            .bg(rgba(0x202020F5))
            .text_color(rgb(0xC0C0C0))
            // 用几何轮廓画剪贴板，避免额外字体或图标资源依赖。
            .child(
                div()
                    .relative()
                    .flex_shrink_0()
                    .w(px(15.0))
                    .h(px(18.0))
                    .rounded(px(3.0))
                    .border_1()
                    .border_color(rgb(0xC0C0C0))
                    .child(
                        div()
                            .absolute()
                            .left(px(3.0))
                            .top(px(-3.0))
                            .w(px(7.0))
                            .h(px(5.0))
                            .rounded(px(2.0))
                            .border_1()
                            .border_color(rgb(0xC0C0C0))
                            .bg(rgb(0x202020)),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(self.title.clone()),
            );
        div().size_full().child(card.with_animation(
            "toast-motion",
            Animation::new(Duration::from_millis(TOAST_MS)),
            |card, phase| {
                let (offset, opacity) = motion(phase);
                card.top(px(PAD + offset)).opacity(opacity)
            },
        ))
    }
}

/// 入场减速、退场加速；中段保持静止，动画结束时已完全透明。
fn motion(phase: f32) -> (f32, f32) {
    let elapsed = phase.clamp(0.0, 1.0) * TOAST_MS as f32;
    let visible = if elapsed < 200.0 {
        1.0 - (1.0 - elapsed / 200.0).powi(3)
    } else if elapsed > 1600.0 {
        1.0 - ((elapsed - 1600.0) / 200.0).powi(3)
    } else {
        1.0
    };
    (SLIDE * (1.0 - visible), visible)
}

/// 在屏幕底部居中开一枚提示，到时自动关掉。
///
/// # 参数
/// - `title`: 主行文案。
pub(crate) fn show(cx: &mut App, title: String) {
    let (x, y) = toast_pos(cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: point(px(x), px(y)),
                size: size(px(TOAST_W + PAD * 2.0), px(TOAST_H + PAD * 2.0 + SLIDE)),
            })),
            titlebar: None,
            // 置顶浮层。Windows 用透明背景让圆角外真正透出桌面；
            // X11 上 Transparent 会换 32 位 visual，在软件合成分支上整窗变全透明
            // （见 CHANGELOG 里浮层那条修复），所以非 Windows 退化为不透明底。
            kind: WindowKind::Overlay,
            window_background: if cfg!(target_os = "windows") {
                WindowBackgroundAppearance::Transparent
            } else {
                WindowBackgroundAppearance::Opaque
            },
            // 不抢焦点：截完要接着在原程序里粘贴，焦点不能被提示抢走。
            focus: false,
            show: true,
            ..Default::default()
        },
        move |window, cx| {
            // 连按两次快捷键时，不要把自己截进下一张图。
            exclude_from_capture(window);
            // 这里**不要**调 `set_mouse_passthrough(true)`：它会给窗口补上
            // `WS_EX_LAYERED`，而本窗口走的是 DirectComposition（无重定向位图），
            // 两者一起用时窗口只剩内容、不参与屏幕合成——提示会彻底看不见。
            // 提示只活 1.6 秒且不抢焦点，被点到也不影响截图结果，不值得为穿透牺牲可见性。
            cx.new(|_| SaveToast {
                title: SharedString::from(title),
            })
        },
    );
    // 建窗失败只影响这条提示，不打断截图流程（剪贴板内容不受影响）。
    let Ok(handle) = opened else {
        return;
    };
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(Duration::from_millis(TOAST_MS))
            .await;
        // 跨窗口移除要在 App 上下文里做（与托盘 / 浮层的关闭同一模式）。
        cx.update(|cx| {
            let _ = cx.update_window(handle.into(), |_, window, _| window.remove_window());
        });
    })
    .detach();
}

/// 根据显示器原点计算底部居中位置，兼容主屏不在桌面原点的情况。
fn toast_pos(cx: &App) -> (f32, f32) {
    let (ox, oy, w, h) = cx
        .primary_display()
        .map(|display| {
            let bounds = display.bounds();
            (
                bounds.origin.x.as_f32(),
                bounds.origin.y.as_f32(),
                bounds.size.width.as_f32(),
                bounds.size.height.as_f32(),
            )
        })
        .unwrap_or((0.0, 0.0, 1920.0, 1080.0));
    (
        ox + (w - TOAST_W - PAD * 2.0) / 2.0,
        oy + h - BOTTOM - TOAST_H - PAD,
    )
}

#[cfg(test)]
mod tests {
    //! 验证提示完整进出，不在关闭窗口前突然截断。
    use super::*;
    #[test]
    fn motion_enters_holds_and_exits() {
        assert_eq!(motion(0.0), (SLIDE, 0.0));
        assert_eq!(motion(0.5), (0.0, 1.0));
        assert_eq!(motion(1.0), (SLIDE, 0.0));
        let entry = motion(0.05);
        let exit = motion(0.95);
        assert!(entry.0 > 0.0 && entry.0 < SLIDE && entry.1 > 0.0);
        assert!(exit.0 > 0.0 && exit.0 < SLIDE && exit.1 > 0.0);
    }
}
