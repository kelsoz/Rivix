//! 系统托盘模块：常驻后台、全局截图入口与窗口显隐。
//!
//! Rivix 设置窗口平时隐藏，关闭主窗口时不退出，而是隐藏到托盘（参考 rgpui `tray` 示例）：
//! 截图、图片编辑、设置、退出全部走托盘菜单或全局热键。

use rgpui::{App, AppContext, Entity, TrayIconEvent, TrayMenuItem, WindowHandle};

use crate::{app::RivixApp, i18n::tr, viewer::open_image_for_edit_in_app};

/// 托盘菜单项编号（与点击回调中的分支对应）。
const MENU_REGION: &str = "capture_region";
/// 全屏截图菜单项编号。
const MENU_FULL: &str = "capture_full";
/// 屏幕取色菜单项编号。
const MENU_PICKER: &str = "pick_color";
/// 打开图片编辑菜单项编号。
const MENU_OPEN_IMAGE: &str = "open_image";
/// 编辑最近一张截图菜单项编号。
const MENU_EDIT_LAST: &str = "edit_last";
/// 贴出最近截图。
const MENU_PIN_LAST: &str = "pin_last";
/// 停止当前录屏菜单项编号。
const MENU_STOP_RECORD: &str = "stop_record";
/// 深色模式切换菜单项编号。
const MENU_DARK_MODE: &str = "toggle_dark_mode";
/// 显示设置窗口菜单项编号。
const MENU_SHOW: &str = "show_window";
/// 退出菜单项编号。
const MENU_QUIT: &str = "quit";

/// 初始化托盘：常驻 + 菜单 + 点击行为。
///
/// # 参数
/// - `cx`: 应用上下文。
/// - `main_win`: 主窗口句柄（显隐与截图触发用）。
/// - `main`: 主视图实体（截图状态机所在）。
pub fn setup_tray(cx: &mut App, main_win: WindowHandle<RivixApp>, main: Entity<RivixApp>) {
    // 没有窗口也要保持运行，否则隐藏主窗口即退出进程。
    cx.set_keep_alive_without_windows(true);
    cx.set_tray_tooltip(&tr("tray.tooltip"));
    refresh_icon(cx);
    cx.set_tray_menu(tray_menu());

    // 录屏期间仅双击托盘结束录屏（避免左键单击误触）；空闲时双击复用最近截图编辑入口。
    let edit_main = main.clone();
    cx.on_tray_icon_event(move |event, cx| {
        if matches!(event, TrayIconEvent::DoubleClick) {
            if crate::record::is_recording() {
                crate::record::stop_active();
                return;
            }
            let edit_main = edit_main.clone();
            // 延后到当前 Windows 托盘 WndProc 回调退出后再开窗，避免事件循环重入崩溃。
            cx.spawn(async move |cx| {
                cx.update(|cx| {
                    refresh_icon(cx);
                    edit_main.update(cx, |view, cx| view.open_last_for_edit(cx));
                });
            })
            .detach();
        }
    });

    // 托盘菜单动作分发。
    cx.on_tray_menu_action(move |id, cx| match id.as_ref() {
        MENU_REGION => trigger_capture(cx, main_win, main.clone(), false),
        MENU_FULL => trigger_capture(cx, main_win, main.clone(), true),
        MENU_PICKER => trigger_picker(cx, main_win, main.clone()),
        MENU_OPEN_IMAGE => open_image_for_edit_in_app(main.clone(), cx),
        // 重新打开最近一张截图：不用再走文件对话框挑图。
        MENU_EDIT_LAST => main.update(cx, |view, cx| view.open_last_for_edit(cx)),
        MENU_PIN_LAST => main.update(cx, |view, cx| view.pin_last(cx)),
        MENU_STOP_RECORD => {
            crate::record::stop_active();
        }
        MENU_DARK_MODE => toggle_dark_mode(cx),
        MENU_SHOW => show_main_window(cx, main_win),
        MENU_QUIT => cx.quit(),
        _ => {}
    });
}

/// 托盘菜单项（按当前语言生成；初始化与语言切换后重建共用）。
fn tray_menu() -> Vec<TrayMenuItem> {
    let mut items = Vec::new();
    if crate::record::is_recording() {
        items.push(TrayMenuItem::Action {
            label: tr("tray.stop_record").into(),
            id: MENU_STOP_RECORD.into(),
        });
        items.push(TrayMenuItem::Separator);
    }
    items.extend([
        TrayMenuItem::Action {
            label: tr("tray.region").into(),
            id: MENU_REGION.into(),
        },
        TrayMenuItem::Action {
            label: tr("tray.full").into(),
            id: MENU_FULL.into(),
        },
        TrayMenuItem::Action {
            label: tr("tray.picker").into(),
            id: MENU_PICKER.into(),
        },
        TrayMenuItem::Separator,
        TrayMenuItem::Action {
            label: tr("tray.open_image").into(),
            id: MENU_OPEN_IMAGE.into(),
        },
        // 关掉「截图后打开编辑窗」后，托盘这里是找回上一张直接标注的快捷入口。
        TrayMenuItem::Action {
            label: tr("tray.edit_last").into(),
            id: MENU_EDIT_LAST.into(),
        },
        TrayMenuItem::Action {
            label: tr("pin.last").into(),
            id: MENU_PIN_LAST.into(),
        },
        TrayMenuItem::Separator,
        TrayMenuItem::Action {
            // 原生托盘菜单项无独立勾选框，用前缀对号直观标示当前深色模式状态。
            label: if crate::app::is_dark_mode() {
                format!("✓ {}", tr("tray.dark_mode"))
            } else {
                tr("tray.dark_mode")
            }
            .into(),
            id: MENU_DARK_MODE.into(),
        },
        TrayMenuItem::Action {
            label: tr("tray.show").into(),
            id: MENU_SHOW.into(),
        },
        TrayMenuItem::Action {
            label: tr("tray.quit").into(),
            id: MENU_QUIT.into(),
        },
    ]);
    items
}

/// 切换深色/浅色主题并立即同步到配置、托盘菜单与所有已打开窗口。
fn toggle_dark_mode(cx: &mut App) {
    crate::config::update(|cfg| {
        let next = !cfg.dark_mode.unwrap_or(true);
        cfg.dark_mode = Some(next);
    });
    crate::app::apply_theme(cx);
    refresh_labels(cx);
    cx.refresh_windows();
}

/// 按当前语言与录屏状态刷新托盘提示与菜单（设置页切换语言或录屏启停后调用）。
///
/// # 参数
/// - `cx`: 应用上下文。
pub fn refresh_labels(cx: &mut App) {
    if crate::record::is_recording() {
        cx.set_tray_tooltip(&tr("tray.recording_tooltip"));
    } else {
        refresh_icon(cx);
        cx.set_tray_tooltip(&tr("tray.tooltip"));
    }
    cx.set_tray_menu(tray_menu());
}

/// 预生成 10 帧 32×32 呼吸红点 ICO，走 Windows 原生 ICO 图标加载（绕开 PNG 分支的 R/B 通道颠倒与模糊）。
fn recording_pulse_frames() -> &'static Vec<Vec<u8>> {
    use image::{ImageEncoder, codecs::ico::IcoEncoder};
    use std::sync::OnceLock;
    static FRAMES: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    FRAMES.get_or_init(|| {
        let mut out = Vec::with_capacity(10);
        let size = 32u32;
        let center = 15.5f32;
        for step in 0..10 {
            // 0..1..0 平滑正弦呼吸相位：亮红核心 + 深红收边，消除高对比度白边毛刺。
            let phase = (step as f32 / 10.0 * std::f32::consts::TAU).sin() * 0.5 + 0.5;
            let core_r = 7.0 + phase * 3.0;
            let border_r = core_r + 2.2;
            let mut img = image::RgbaImage::new(size, size);
            for y in 0..size {
                for x in 0..size {
                    let dx = x as f32 - center;
                    let dy = y as f32 - center;
                    let dist = (dx * dx + dy * dy).sqrt();
                    let pixel = if dist <= core_r {
                        let r = 222 + (phase * 28.0) as u8;
                        image::Rgba([r, 42, 42, 255])
                    } else if dist <= border_r {
                        // 用更深的暗红色做外圈描边与 1px 抗锯齿收边，在深浅任务栏上都无白边毛刺。
                        let aa = (border_r - dist + 0.5).clamp(0.0, 1.0);
                        image::Rgba([148, 22, 22, (255.0 * aa) as u8])
                    } else {
                        image::Rgba([0, 0, 0, 0])
                    };
                    img.put_pixel(x, y, pixel);
                }
            }
            let mut buf = Vec::new();
            if IcoEncoder::new(&mut buf)
                .write_image(img.as_raw(), size, size, image::ExtendedColorType::Rgba8)
                .is_ok()
            {
                out.push(buf);
            }
        }
        out
    })
}

/// 录屏开始后启动托盘呼吸红点动画，录屏结束自动恢复常规托盘图标。
pub fn start_recording_pulse(cx: &mut App) {
    let frames = recording_pulse_frames();
    if frames.is_empty() {
        return;
    }
    cx.set_tray_icon(Some(&frames[0]));
    cx.spawn(async move |cx| {
        let mut idx = 1usize;
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(120))
                .await;
            let keep_going = cx.update(|cx| {
                if !crate::record::is_recording() {
                    refresh_icon(cx);
                    return false;
                }
                cx.set_tray_icon(Some(&frames[idx % frames.len()]));
                true
            });
            if !keep_going {
                break;
            }
            idx = idx.wrapping_add(1);
        }
    })
    .detach();
}

/// 单色线条图标按 Windows 系统任务栏主题选择深浅，不跟随应用自设深色主题。
pub fn refresh_icon(cx: &mut App) {
    #[cfg(target_os = "windows")]
    let light = {
        use windows::{
            Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
            core::w,
        };
        let mut value = 0u32;
        let mut bytes = 4u32;
        // 只读系统任务栏主题；读取失败时采用适合默认深色任务栏的浅线图标。
        let result = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
                w!("SystemUsesLightTheme"),
                RRF_RT_REG_DWORD,
                None,
                Some((&mut value as *mut u32).cast()),
                Some(&mut bytes),
            )
        };
        result.0 == 0 && value != 0
    };
    #[cfg(not(target_os = "windows"))]
    let light = matches!(cx.window_appearance(), rgpui::WindowAppearance::Light);
    let icon: &[u8] = if light {
        include_bytes!("../assets/tray-light.png")
    } else {
        include_bytes!("../assets/tray.png")
    };
    cx.set_tray_icon(Some(icon));
}

/// 从 App 上下文触发截图（托盘菜单 / 全局热键共用）。
///
/// 主窗口可能处于隐藏态：先借窗口发起捕获流程（`spawn_in` 需要窗口），
/// 捕获浮层是独立新窗口，不依赖主窗口可见。
pub fn trigger_capture(
    cx: &mut App,
    main_win: WindowHandle<RivixApp>,
    main: Entity<RivixApp>,
    fullscreen: bool,
) {
    if crate::record::is_recording() {
        return;
    }
    // update_window 借出主窗口，再 update 主实体拿到 Context 发起异步捕获。
    let _ = cx.update_window(main_win.into(), |_, window, cx| {
        cx.update_entity(&main, |view, viewcx| {
            if fullscreen {
                view.begin_full(window, viewcx);
            } else {
                view.begin_region(window, viewcx);
            }
        });
    });
}

/// 从 App 上下文触发长截图（全局热键入口）。
pub fn trigger_long(cx: &mut App, main_win: WindowHandle<RivixApp>, main: Entity<RivixApp>) {
    if crate::record::is_recording() {
        return;
    }
    if crate::overlay::request_long_selection(cx) {
        return;
    }
    let _ = cx.update_window(main_win.into(), |_, window, cx| {
        cx.update_entity(&main, |view, viewcx| {
            view.begin_long(window, viewcx);
        });
    });
}

/// 从 App 上下文触发取色（托盘菜单 / 全局热键共用）。
///
/// 与截图同理：借主窗口发起，后续浮层是独立新窗口。
pub fn trigger_picker(cx: &mut App, main_win: WindowHandle<RivixApp>, main: Entity<RivixApp>) {
    if crate::record::is_recording() {
        return;
    }
    // update_window 借出主窗口，再 update 主实体拿到 Context 发起异步取色。
    let _ = cx.update_window(main_win.into(), |_, window, cx| {
        cx.update_entity(&main, |view, viewcx| {
            view.begin_picker(window, viewcx);
        });
    });
}

/// 显示并激活设置窗口（托盘菜单 / 图标点击共用）。
fn show_main_window(cx: &mut App, main_win: WindowHandle<RivixApp>) {
    // 设置窗口只是被隐藏而非销毁，直接激活即可恢复显示。
    let _ = cx.update_window(main_win.into(), |_, window, _| {
        window.activate_window();
    });
}
