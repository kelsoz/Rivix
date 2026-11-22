//! 屏幕贴图：独立置顶圆角窗口，复用静态纹理，移动与缩放不编码图片。

use crate::i18n::{tr, tr_fmt};
use image::RgbaImage;
use rgpui::{
    App, AppContext, Bounds, Context, CursorStyle, MouseButton, MouseDownEvent, Render,
    RenderImage, ScrollDelta, ScrollWheelEvent, Window, WindowBackgroundAppearance, WindowBounds,
    WindowKind, WindowOptions, div, img, point, prelude::*, px, rgb, rgba, size,
};
use std::sync::Arc;

/// 为阴影留出的透明边缘，不改变原始图片像素。
const PAD: f32 = 6.0;

/// 后台准备的贴图数据，只保留一份 BGRA 像素和可复用纹理。
#[derive(Clone)]
pub(crate) struct PinImage {
    width: u32,
    height: u32,
    frame: Arc<RenderImage>,
}
impl PinImage {
    /// 一次转换供整个贴图窗口复用，不经过磁盘或 PNG 往返。
    pub(crate) fn new(mut pixels: RgbaImage) -> Self {
        let (width, height) = pixels.dimensions();
        // 所有权转移后原地交换通道，贴图不再额外常驻一份 RGBA 原图。
        for pixel in pixels.as_flat_samples_mut().samples.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        Self {
            width,
            height,
            frame: Arc::new(RenderImage::new(vec![image::Frame::new(pixels)])),
        }
    }

    /// 仅在用户复制时恢复 RGBA，不因窗口移动或缩放分配图片。
    fn rgba(&self) -> Option<RgbaImage> {
        let mut image =
            RgbaImage::from_raw(self.width, self.height, self.frame.as_bytes(0)?.to_vec())?;
        for pixel in image.as_flat_samples_mut().samples.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        Some(image)
    }
}

/// 打开一张独立贴图；有位置时保持选区原位，否则在当前主屏居中。
pub(crate) fn open(
    image: PinImage,
    position: Option<(f32, f32)>,
    scale: f32,
    cx: &mut App,
) -> Result<(), String> {
    if image.width == 0 || image.height == 0 {
        return Err(tr("pin.empty"));
    }
    let width = image.width as f32 / scale.max(0.1);
    let height = image.height as f32 / scale.max(0.1);
    let dimensions = size(px(width + PAD * 2.0), px(height + PAD * 2.0));
    let bounds = position
        .map(|(x, y)| Bounds::new(point(px(x - PAD), px(y - PAD)), dimensions))
        .unwrap_or_else(|| Bounds::centered(None, dimensions, cx));
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: None,
            kind: WindowKind::Overlay,
            focus: false,
            show: true,
            is_resizable: false,
            is_minimizable: false,
            window_background: WindowBackgroundAppearance::Transparent,
            ..Default::default()
        },
        move |window, cx| {
            window.set_window_title(&tr("pin.title"));
            // 最近截图入口没有来源屏的 DPI，建窗后按实际目标屏恢复一物理像素。
            if position.is_none() {
                let scale = window.scale_factor().max(0.1);
                let dimensions = size(
                    px(image.width as f32 / scale + PAD * 2.0),
                    px(image.height as f32 / scale + PAD * 2.0),
                );
                window.resize(dimensions);
                if let Some(display) = window.display(cx) {
                    let bounds = display.bounds();
                    window.set_position(point(
                        bounds.origin.x + (bounds.size.width - dimensions.width) / 2.0,
                        bounds.origin.y + (bounds.size.height - dimensions.height) / 2.0,
                    ));
                }
            }
            cx.new(|_| PinnedImage {
                image,
                zoom: 1.0,
                hovered: false,
                drag: None,
            })
        },
    )
    .map_err(|e| e.to_string())?;
    crate::feedback::completed();
    Ok(())
}

/// 关闭当前所有贴图，扫描应用现有窗口，不保存已失效句柄。
fn close_all(cx: &mut App) {
    for handle in cx.windows() {
        if handle.downcast::<PinnedImage>().is_some() {
            let _ = cx.update_window(handle, |_, window, _| window.remove_window());
        }
    }
}

/// 贴图实体仅在交互时重绘，不订阅时钟或下一帧回调。
struct PinnedImage {
    image: PinImage,
    zoom: f32,
    hovered: bool,
    drag: Option<(f32, f32)>,
}

impl PinnedImage {
    /// 保持鼠标下的图片像素不动；缩放只改变窗口和纹理显示尺寸。
    fn zoom_to(
        &mut self,
        zoom: f32,
        pointer: (f32, f32),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 限制超大交换链：最多约 1600 万显示像素，原尺寸始终可恢复。
        let zoom = zoom.clamp(0.1, max_zoom(self.image.width, self.image.height));
        if (zoom - self.zoom).abs() < 0.0001 {
            return;
        }
        let bounds = window.bounds();
        let origin = zoom_origin(
            (bounds.origin.x.as_f32(), bounds.origin.y.as_f32()),
            pointer,
            self.zoom,
            zoom,
        );
        let scale = window.scale_factor().max(0.1);
        self.zoom = zoom;
        window.resize(size(
            px(self.image.width as f32 / scale * zoom + PAD * 2.0),
            px(self.image.height as f32 / scale * zoom + PAD * 2.0),
        ));
        window.set_position(point(px(origin.0), px(origin.1)));
        cx.notify();
    }

    /// 按需在后台编码，再复制完整原图，圆角和窗口按钮不会写入图片。
    fn copy(&self, cx: &mut Context<Self>) {
        let image = self.image.clone();
        cx.spawn(async move |_, cx| {
            let png = cx
                .background_executor()
                .spawn(async move {
                    image
                        .rgba()
                        .and_then(|pixels| crate::export::png_bytes(&pixels))
                })
                .await;
            cx.update(|cx| {
                let message = match png {
                    Some(png) => match crate::export::copy_png_to_clipboard(cx, &png) {
                        Ok(()) => {
                            crate::feedback::completed();
                            tr("shot.copied")
                        }
                        Err(error) => tr_fmt("shot.copy_failed", &[("error", &error)]),
                    },
                    None => tr("pin.copy_failed"),
                };
                crate::toast::show(cx, message);
            });
        })
        .detach();
    }
}

/// 限制窗口表面分配，允许小图放大至八倍，大图至少保留原尺寸。
fn max_zoom(width: u32, height: u32) -> f32 {
    (16_777_216.0 / (width as f32 * height as f32).max(1.0))
        .sqrt()
        .clamp(1.0, 8.0)
}

/// 缩放前后的同一图片点映射至同一桌面位置，包含外侧阴影留白。
fn zoom_origin(origin: (f32, f32), pointer: (f32, f32), old: f32, new: f32) -> (f32, f32) {
    let ratio = new / old;
    (
        origin.0 + (pointer.0 - PAD) * (1.0 - ratio),
        origin.1 + (pointer.1 - PAD) * (1.0 - ratio),
    )
}

impl Render for PinnedImage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.entity().downgrade();
        let card = div()
            .id("pinned-image")
            .relative()
            .size_full()
            .rounded(px(8.0))
            .overflow_hidden()
            .bg(rgb(0x202020))
            .cursor(CursorStyle::Arrow)
            .child(img(self.image.frame.clone()).size_full().rounded(px(8.0)))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(rgba(0xFFFFFF28)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    if event.click_count == 2 {
                        this.drag = None;
                        this.zoom_to(
                            1.0,
                            (event.position.x.as_f32(), event.position.y.as_f32()),
                            window,
                            cx,
                        );
                    } else {
                        this.drag = Some((event.position.x.as_f32(), event.position.y.as_f32()));
                    }
                }),
            )
            .on_mouse_move(
                cx.listener(|this, event: &rgpui::MouseMoveEvent, window, _| {
                    if event.pressed_button != Some(MouseButton::Left) {
                        this.drag = None;
                        return;
                    }
                    if let Some((x, y)) = this.drag {
                        let origin = window.bounds().origin;
                        window.set_position(point(
                            origin.x + event.position.x - px(x),
                            origin.y + event.position.y - px(y),
                        ));
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.drag = None),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.drag = None),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                let dy = match event.delta {
                    ScrollDelta::Pixels(p) => p.y.as_f32(),
                    ScrollDelta::Lines(p) => p.y * 40.0,
                };
                if dy != 0.0 {
                    this.zoom_to(
                        this.zoom * 1.15_f32.powf((dy / 40.0).clamp(-3.0, 3.0)),
                        (event.position.x.as_f32(), event.position.y.as_f32()),
                        window,
                        cx,
                    );
                }
                cx.stop_propagation();
            }))
            .on_mouse_down(MouseButton::Right, move |_, window, cx| {
                let weak = weak.clone();
                // 系统菜单可超出小贴图窗口，延后打开以免嵌套借用当前实体。
                window.defer(cx, move |window, cx| {
                    let action = popup_menu(window);
                    if action == 4 {
                        close_all(cx);
                        window.remove_window();
                    } else {
                        let _ = weak.update(cx, |this, cx| match action {
                            1 => this.copy(cx),
                            2 => {
                                let p = window.mouse_position();
                                this.zoom_to(1.0, (p.x.as_f32(), p.y.as_f32()), window, cx);
                            }
                            3 => window.remove_window(),
                            _ => {}
                        });
                    }
                });
            });
        let card = if self.hovered {
            card.child(
                div()
                    .id("close-pin")
                    .absolute()
                    .top(px(5.0))
                    .right(px(5.0))
                    .w(px(22.0))
                    .h(px(22.0))
                    .rounded(px(6.0))
                    .bg(rgba(0x202020E8))
                    .text_color(rgb(0xEEEEEE))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child("×")
                    .on_mouse_down(MouseButton::Left, |_, window, cx| {
                        cx.stop_propagation();
                        window.remove_window();
                    }),
            )
        } else {
            card
        };
        div()
            .id("pin-root")
            .size_full()
            .p(px(PAD))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.hovered = *hovered;
                cx.notify();
            }))
            .child(card.shadow(vec![rgpui::BoxShadow {
                color: rgba(0x00000050).into(),
                inset: false,
                offset: point(px(0.0), px(2.0)),
                blur_radius: px(4.0),
                spread_radius: px(0.0),
            }]))
    }
}

/// Windows 原生菜单不受贴图尺寸裁剪，也无需渲染循环。
#[cfg(target_os = "windows")]
fn popup_menu(window: &Window) -> i32 {
    use raw_window_handle::RawWindowHandle;
    use windows::{
        Win32::{
            Foundation::{HWND, POINT},
            UI::WindowsAndMessaging::*,
        },
        core::PCWSTR,
    };
    let Ok(handle) = raw_window_handle::HasWindowHandle::window_handle(window) else {
        return 0;
    };
    let RawWindowHandle::Win32(raw) = handle.as_raw() else {
        return 0;
    };
    let hwnd = HWND(raw.hwnd.get() as *mut core::ffi::c_void);
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return 0;
        };
        let mut valid = true;
        for (id, key) in [
            (1, "pin.copy"),
            (2, "pin.reset"),
            (3, "pin.close"),
            (4, "pin.close_all"),
        ] {
            let text: Vec<u16> = tr(key).encode_utf16().chain(Some(0)).collect();
            valid &= AppendMenuW(menu, MF_STRING, id, PCWSTR(text.as_ptr())).is_ok();
        }
        let mut point = POINT::default();
        let result = if valid && GetCursorPos(&mut point).is_ok() {
            TrackPopupMenuEx(
                menu,
                TPM_RETURNCMD.0 | TPM_RIGHTBUTTON.0,
                point.x,
                point.y,
                hwnd,
                None,
            )
            .0
        } else {
            0
        };
        let _ = DestroyMenu(menu);
        result
    }
}

/// shortcut: 当前贴图菜单面向 Windows；移植其他系统时补接对应系统菜单。
#[cfg(not(target_os = "windows"))]
fn popup_menu(_: &Window) -> i32 {
    0
}

#[cfg(test)]
mod tests {
    #[test]
    fn one_bgra_buffer_copies_back_exact_original_pixels() {
        let pixels = image::RgbaImage::from_pixel(3, 2, image::Rgba([10, 50, 190, 123]));
        let pin = super::PinImage::new(pixels.clone());
        assert_eq!(&pin.frame.as_bytes(0).unwrap()[..4], &[190, 50, 10, 123]);
        assert_eq!(pin.rgba().unwrap(), pixels);
        assert!(std::sync::Arc::ptr_eq(&pin.clone().frame, &pin.frame));
    }

    #[test]
    fn large_pins_do_not_allocate_unbounded_surfaces() {
        assert_eq!(super::max_zoom(100, 100), 8.0);
        let zoom = super::max_zoom(3840, 2160);
        assert!(zoom > 1.0 && zoom < 2.0);
        assert!(3840.0 * 2160.0 * zoom * zoom <= 16_777_218.0);
        assert_eq!(super::max_zoom(7680, 4320), 1.0);
    }

    #[test]
    fn zoom_keeps_mouse_pixel_in_place() {
        let origin = (-100.0, 80.0);
        let pointer = (86.0, 46.0);
        let next = super::zoom_origin(origin, pointer, 1.0, 2.0);
        assert_eq!(next, (-180.0, 40.0));
        assert_eq!(
            origin.0 + pointer.0,
            next.0 + super::PAD + (pointer.0 - super::PAD) * 2.0
        );
        assert_eq!(
            origin.1 + pointer.1,
            next.1 + super::PAD + (pointer.1 - super::PAD) * 2.0
        );
        assert_eq!(super::zoom_origin(next, (166.0, 86.0), 2.0, 1.0), origin);
    }
}
