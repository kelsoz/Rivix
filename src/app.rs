//! 设置窗口模块：快捷键设置、截图入口、状态与最近保存记录。
//!
//! 设置窗口是 Rivix 的控制中心：平时隐藏到托盘常驻，
//! 截图本身发生在全屏浮层（`overlay`），预览标注在独立窗口（`viewer`），
//! 这里只负责发起流程、配置全局热键，以及展示保存历史。
//! 日常使用快捷键截图即可，无需打开本窗口。

use crate::{
    autostart,
    capture::{CaptureMode, begin_capture, begin_picker, cap_log},
    config, export,
    hotkey::{HotkeyKind, apply_global_hotkey, hotkey_value_to_string, save_hotkey_string},
    i18n::{available_langs, lang, set_lang, tr, tr_fmt},
    ocr, toast,
    viewer::{open_viewer, preview_from_path},
};
use rgpui::{
    Context, Entity, FocusHandle, KeyDownEvent, PathPromptOptions, Render, SharedString, Switch,
    Window, WindowControlArea,
    components::HotkeyValue,
    div, img,
    input_ui::{Input, InputState},
    prelude::*,
    px, rgb, rgba,
};
use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
};

/// 界面色板：统一管理深色 / 浅色两套角色颜色，供设置窗口与标注编辑窗共用。
#[derive(Clone, Copy)]
pub struct ThemePalette {
    /// 主内容区背景色。
    pub background: u32,
    /// 侧边栏外围背景色。
    pub outer: u32,
    /// 卡片与按钮默认底色。
    pub card: u32,
    /// 通用边框色。
    pub border: u32,
    /// 主文字颜色。
    pub text: u32,
    /// 次要说明文字颜色。
    pub muted: u32,
    /// 选中态底色。
    pub selected: u32,
    /// 选中态高亮文字颜色。
    pub selected_text: u32,
    /// 禁用态文字颜色。
    #[allow(dead_code)]
    pub disabled_text: u32,
    /// 链接文字颜色。
    pub link: u32,
    /// 输入框底色。
    pub input: u32,
    /// 弱化填充色。
    pub muted_bg: u32,
    /// 主题强调色。
    pub primary: u32,
    /// 主按钮激活态边框色。
    pub primary_active_border: u32,
    /// 弹出层底色。
    pub popover: u32,
    /// 滚动条滑块颜色。
    pub scrollbar_thumb: u32,
    /// 滚动条滑块悬停颜色。
    pub scrollbar_thumb_hover: u32,
    /// 开关未选中芯片底色。
    pub chip_off_bg: u32,
    /// 侧边导航选中底色。
    #[allow(dead_code)]
    pub tab_selected_bg: u32,
    /// 侧边导航悬停底色。
    pub tab_hover_bg: u32,
    /// 工具栏/浮条底色（标注编辑窗用）。
    pub toolbar_bg: u32,
}

/// 深色模式色板，依据个人 UI 配色标准 v1，避免大面积鲜亮强调。
const DARK_PALETTE: ThemePalette = ThemePalette {
    background: 0x181818,
    outer: 0x191A1F,
    card: 0x222222,
    border: 0x303030,
    text: 0xC0C0C0,
    muted: 0x929292,
    selected: 0x2B343C,
    selected_text: 0xE2E2E2,
    disabled_text: 0x686868,
    link: 0x9AB8D0,
    input: 0x383838,
    muted_bg: 0x343434,
    primary: 0x8EAAC0,
    primary_active_border: 0x44515D,
    popover: 0x202020,
    scrollbar_thumb: 0x3C3C3C,
    scrollbar_thumb_hover: 0x4A4A4A,
    chip_off_bg: 0x2A2A2A,
    tab_selected_bg: 0x313033,
    tab_hover_bg: 0x252529,
    toolbar_bg: 0x202020,
};

/// 浅色模式色板，保持柔和低饱和灰阶与统一蓝色强调。
const LIGHT_PALETTE: ThemePalette = ThemePalette {
    background: 0xF5F6F8,
    outer: 0xECEEF2,
    card: 0xFFFFFF,
    border: 0xD8DCE3,
    text: 0x1F242B,
    muted: 0x606875,
    selected: 0xDCE7F2,
    selected_text: 0x102A43,
    disabled_text: 0x9AA2AE,
    link: 0x2463A8,
    input: 0xFFFFFF,
    muted_bg: 0xE4E7EC,
    primary: 0x2D6AA8,
    primary_active_border: 0x8BB2D9,
    popover: 0xFFFFFF,
    scrollbar_thumb: 0xC6CCD6,
    scrollbar_thumb_hover: 0xAEB6C2,
    chip_off_bg: 0xE8EBF0,
    tab_selected_bg: 0xDFE6EF,
    tab_hover_bg: 0xE4E7ED,
    toolbar_bg: 0xFFFFFF,
};

/// 缓存设置界面品牌图标（内嵌 PNG 字节，只构造一次）。
fn settings_logo() -> Arc<rgpui::Image> {
    static LOGO: OnceLock<Arc<rgpui::Image>> = OnceLock::new();
    LOGO.get_or_init(|| {
        Arc::new(rgpui::Image::from_bytes(
            rgpui::ImageFormat::Png,
            include_bytes!("../assets/logo-settings.png").to_vec(),
        ))
    })
    .clone()
}

/// 查询当前是否处于深色模式（配置缺省时默认开启深色模式）。
pub fn is_dark_mode() -> bool {
    config::load().dark_mode.unwrap_or(true)
}

/// 返回当前主题对应的角色色板。
pub fn palette() -> &'static ThemePalette {
    if is_dark_mode() {
        &DARK_PALETTE
    } else {
        &LIGHT_PALETTE
    }
}

/// 为原生输入框、滚动条、菜单与状态栏应用当前深浅色主题。
pub fn apply_theme(cx: &mut rgpui::App) {
    let dark = is_dark_mode();
    let mode = if dark {
        rgpui::ThemeMode::Dark
    } else {
        rgpui::ThemeMode::Light
    };
    rgpui::Theme::change(mode, None, cx);
    let p = palette();
    let theme = rgpui::Theme::global_mut(cx);
    theme.colors.background = rgb(p.background).into();
    theme.colors.foreground = rgb(p.text).into();
    theme.colors.border = rgb(p.border).into();
    theme.colors.input = rgb(p.input).into();
    theme.colors.muted = rgb(p.muted_bg).into();
    theme.colors.muted_foreground = rgb(p.muted).into();
    theme.colors.primary = rgb(p.primary).into();
    theme.colors.primary_foreground = rgb(p.background).into();
    theme.colors.popover = rgb(p.popover).into();
    theme.colors.popover_foreground = rgb(p.text).into();
    theme.colors.scrollbar = rgb(p.background).into();
    theme.colors.scrollbar_thumb = rgb(p.scrollbar_thumb).into();
    theme.colors.scrollbar_thumb_hover = rgb(p.scrollbar_thumb_hover).into();
    theme.colors.link = rgb(p.link).into();
    theme.tokens = (&theme.colors).into();
    #[cfg(target_os = "windows")]
    sync_native_menu_theme(dark);
}

/// 同步 Windows 原生弹出菜单（托盘右键菜单 / 贴图右键菜单）的深浅色主题。
///
/// `rgpui-windows` 使用 Win32 `TrackPopupMenu` 构建原生 `HMENU`，不受 rgpui
/// 内部 `Theme` 控制，需通过 `uxtheme.dll` 导出序号 135/136 通知系统切换。
#[cfg(target_os = "windows")]
fn sync_native_menu_theme(dark: bool) {
    use windows::{
        Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW},
        core::{PCSTR, w},
    };

    // PreferredAppMode：2 = ForceDark，3 = ForceLight（Windows 10 1903+）。
    let mode: i32 = if dark { 2 } else { 3 };
    unsafe {
        let Ok(module) = LoadLibraryW(w!("uxtheme.dll")) else {
            return;
        };
        if let Some(set_mode) = GetProcAddress(module, PCSTR(135usize as *const u8)) {
            let f: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(set_mode);
            let _ = f(mode);
        }
        if let Some(flush) = GetProcAddress(module, PCSTR(136usize as *const u8)) {
            let f: unsafe extern "system" fn() = std::mem::transmute(flush);
            f();
        }
    }
}

/// 项目仓库地址（关于页展示、打开与复制共用）。
const REPO_URL: &str = "https://github.com/kelsoz/Rivix";

/// 当前程序版本号（编译期取自 `Cargo.toml`，关于页展示用）。
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 系统默认保存目录：用户「下载」文件夹（不存在或不可用时回退桌面或临时目录）。
pub fn default_save_dir() -> PathBuf {
    #[cfg(windows)]
    let candidates = [
        std::env::var("USERPROFILE")
            .map(|home| PathBuf::from(home).join("Downloads"))
            .ok(),
        std::env::var("USERPROFILE")
            .map(|home| PathBuf::from(home).join("Desktop"))
            .ok(),
    ];
    #[cfg(not(windows))]
    let candidates = {
        let home = std::env::var("HOME").ok();
        [
            std::env::var("XDG_DOWNLOAD_DIR").ok().map(PathBuf::from),
            home.as_deref()
                .map(|home| PathBuf::from(home).join("Downloads")),
            home.as_deref()
                .map(|home| PathBuf::from(home).join("Desktop")),
        ]
    };
    for dir in candidates.into_iter().flatten() {
        if dir.is_dir() || std::fs::create_dir_all(&dir).is_ok() {
            return dir;
        }
    }
    std::env::temp_dir()
}

/// 读取用户配置的截图默认保存目录；未配置或目录已失效时回退到系统默认。
pub fn load_save_dir() -> PathBuf {
    // 配置里的目录可能已被用户删除：失效即回退默认。
    config::load()
        .save_dir
        .filter(|dir| !dir.as_os_str().is_empty() && dir.is_dir())
        .unwrap_or_else(default_save_dir)
}

/// 持久化用户选择的截图默认保存目录（写入失败仅忽略，不阻断流程）。
///
/// # 参数
/// - `dir`: 用户在目录对话框中选中的真实目录。
pub fn store_save_dir(dir: &std::path::Path) {
    config::update(|cfg| cfg.save_dir = Some(dir.to_path_buf()));
}

/// 读取「截图后自动复制到剪贴板」设置（默认开：截完直接 `Ctrl+V` 就能用）。
pub fn load_auto_copy() -> bool {
    true
}

/// 持久化「截图后自动复制到剪贴板」设置（写入失败仅忽略，不阻断截图流程）。
///
/// # 参数
/// - `enabled`: 是否在截图保存后自动写剪贴板。
#[allow(dead_code)]
pub fn store_auto_copy(enabled: bool) {
    config::update(|cfg| cfg.shot.auto_copy = enabled);
}

/// 读取「截图后打开编辑窗」设置（默认关：截完只闪一下提示，需要时再手动编辑）。
pub fn load_open_editor() -> bool {
    config::load().shot.open_editor
}

/// 持久化「截图后打开编辑窗」设置（写入失败仅忽略，不阻断截图流程）。
///
/// # 参数
/// - `enabled`: 是否在截图保存后自动打开编辑窗。
pub fn store_open_editor(enabled: bool) {
    config::update(|cfg| cfg.shot.open_editor = enabled);
}

/// 后台完成的截图与复制结果，主线程只更新界面。
pub struct PreparedCapture {
    /// 临时图片，供剪贴板文件格式及手动编辑使用。
    pub preview: PreviewImage,
    /// 是否已复制；复制失败保留图片供手动另存。
    pub copied: Result<bool, String>,
}

/// 后台编码一次 PNG 并写剪贴板，主线程不做磁盘读写、PNG 解码或重试等待。
pub fn prepare_capture(img: &image::RgbaImage) -> Result<PreparedCapture, String> {
    let png = export::png_bytes(img).ok_or_else(|| "截图编码失败".to_string())?;
    let path = std::env::temp_dir().join(format!(
        "rivix-{}-{}",
        std::process::id(),
        timestamped_png_name()
    ));
    std::fs::write(&path, &png).map_err(|error| error.to_string())?;
    let copied = if load_auto_copy() {
        export::copy_capture_to_clipboard(&png, img, &path).map(|()| true)
    } else {
        Ok(false)
    };
    // 先反馈操作成功，再让主线程展示提示；不把 Toast 的渲染时机当成完成时机。
    if copied.is_ok() {
        crate::feedback::completed();
    }
    Ok(PreparedCapture {
        preview: PreviewImage {
            path,
            w: img.width(),
            h: img.height(),
        },
        copied,
    })
}

/// 显式保存选区到设置目录，不改剪贴板、不自动打开编辑器。
pub(crate) fn save_capture(img: &image::RgbaImage) -> Result<PreparedCapture, String> {
    save_capture_in(img, &load_save_dir())
}

/// 独占创建文件，避免毫秒文件名碰撞覆盖已有截图；失败时删除不完整文件。
fn save_capture_in(
    img: &image::RgbaImage,
    dir: &std::path::Path,
) -> Result<PreparedCapture, String> {
    use std::io::Write;
    let png = export::png_bytes(img).ok_or_else(|| "截图编码失败".to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join(timestamped_png_name());
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| e.to_string())?;
    if let Err(error) = file.write_all(&png).and_then(|()| file.flush()) {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(error.to_string());
    }
    crate::feedback::completed();
    Ok(PreparedCapture {
        preview: PreviewImage {
            path,
            w: img.width(),
            h: img.height(),
        },
        copied: Ok(false),
    })
}

#[cfg(test)]
mod explicit_save_tests {
    #[test]
    fn saves_real_png_and_reports_invalid_directory() {
        let dir = std::env::temp_dir().join(format!("rivix-save-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let image = image::RgbaImage::from_pixel(3, 2, image::Rgba([10, 20, 30, 255]));
        let saved = super::save_capture_in(&image, &dir).unwrap();
        assert_eq!(image::open(&saved.preview.path).unwrap().to_rgba8(), image);
        assert!(super::save_capture_in(&image, &saved.preview.path).is_err());
        std::fs::remove_file(saved.preview.path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}

/// 显式保存成功后登记最近文件和预览，仅显示完成提示。
pub(crate) fn finish_saved_capture(
    state: &mut RivixApp,
    cx: &mut Context<RivixApp>,
    capture: PreparedCapture,
) {
    let preview = capture.preview;
    let message = tr_fmt(
        "shot.saved_file",
        &[("path", &preview.path.display().to_string())],
    );
    state.last_preview = Some(preview.clone());
    state.push_recent(preview.path, message.clone(), cx);
    toast::show(cx, message);
}

/// 截图完成后只按设置复制到剪贴板，保留临时预览供用户手动保存。
/// 区域与全屏共用此入口，不向用户保存目录自动写入图片。
pub fn finish_capture(
    state: &mut RivixApp,
    cx: &mut Context<RivixApp>,
    capture: PreparedCapture,
) -> (PreviewImage, String) {
    let preview = capture.preview;
    let note = match capture.copied {
        Ok(true) => tr("shot.copied"),
        Ok(false) => tr("shot.not_saved"),
        Err(error) => {
            cap_log(&format!("截图复制失败：{error}"));
            tr_fmt("shot.copy_failed", &[("error", &error)])
        }
    };
    // 仅保留系统临时图片，编辑最近一张和手动保存仍可用。
    state.last_preview = Some(preview.clone());
    if load_open_editor() {
        open_viewer(cx.entity(), cx, preview.clone());
    } else {
        toast::show(cx, note.clone());
    }
    (preview.clone(), note)
}

/// 带毫秒时间戳的截图文件名（手动保存与 CLI 默认输出共用）。
pub(crate) fn timestamped_png_name() -> String {
    format!(
        "rivix-{}.png",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    )
}

/// 主窗口展示的截图预览（临时截图，或用户选择的本地图片）。
#[derive(Clone)]
pub struct PreviewImage {
    /// 预览图片路径（临时截图；本地编辑模式下是用户原图）。
    pub path: PathBuf,
    /// 原图宽度（图片像素）。
    pub w: u32,
    /// 原图高度（图片像素）。
    pub h: u32,
}

/// 设置页精简导航：截图与热键 / 录屏设置 / 通用与引擎。
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum SettingsTab {
    /// 截图与热键：呼出快捷键、内置单键速查与保存目录。
    #[default]
    Capture,
    /// 录屏设置：编码器状态、默认格式帧率与画面声音开关。
    Record,
    /// 通用与引擎：OCR 后端模式、语言、自启与关于。
    General,
}

impl SettingsTab {
    /// Tab 唯一标识。
    fn id(self) -> &'static str {
        match self {
            SettingsTab::Capture => "capture",
            SettingsTab::Record => "record",
            SettingsTab::General => "general",
        }
    }

    /// Tab 显示文案。
    fn label(self) -> String {
        match self {
            SettingsTab::Capture => tr("tab.capture"),
            SettingsTab::Record => tr("tab.record"),
            SettingsTab::General => tr("tab.general"),
        }
    }

    /// 从标识还原枚举。
    fn from_id(id: &str) -> Self {
        match id {
            "record" => SettingsTab::Record,
            "general" => SettingsTab::General,
            _ => SettingsTab::Capture,
        }
    }

    /// 左侧栏导航顺序。
    fn all() -> [SettingsTab; 3] {
        [
            SettingsTab::Capture,
            SettingsTab::Record,
            SettingsTab::General,
        ]
    }
}

/// 框选期间内置单键功能速查表（键帽、标题、说明）。
const OVERLAY_KEY_HINTS: [(&str, &str, &str); 8] = [
    ("F", "全屏 (Full)", "截图或录屏切至全屏范围"),
    ("B", "标注 (Brush)", "选定区域直接打开标注编辑窗"),
    ("V", "录屏 (Video)", "沿用当前选区进入录屏准备态"),
    ("T", "文字识别 (Text)", "拖框识别文字并自动复制"),
    ("C", "取色 (Color)", "复制光标像素 HEX 色值"),
    ("S", "保存 (Save)", "立即将选区保存为 PNG 文件"),
    ("P", "贴图 (Pin)", "将选区置顶悬浮贴在屏幕"),
    ("Q", "二维码 (QR)", "框住二维码复制或打开链接"),
];

/// 主视图状态。
pub struct RivixApp {
    /// 是否正在截取中（防止重复推流）。
    pub capturing: bool,
    /// 当前生效的呼出截图快捷键字符串。
    region_hotkey: String,
    /// 保留的全屏截图快捷键字符串（供后台热键兼容）。
    #[allow(dead_code)]
    full_hotkey: String,
    /// 保留的取色快捷键字符串（供后台热键兼容）。
    #[allow(dead_code)]
    picker_hotkey: String,
    /// 是否正在录制呼出快捷键（替代带 BoxShadow 渲染缺陷的内置 HotkeyInput）。
    recording_hotkey: bool,
    /// 快捷键录制框焦点句柄。
    hotkey_focus: FocusHandle,
    /// OCR 外部 HTTP API 地址输入框。
    ocr_api_input: Entity<InputState>,
    /// 状态栏文本。
    status: SharedString,
    /// 当前设置页 Tab。
    settings_tab: SettingsTab,
    /// 截图默认保存目录。
    save_dir: PathBuf,
    /// 截图后是否打开标注编辑窗（所有截图默认永久自动复制）。
    pub(crate) open_editor: bool,
    /// 最近一张截图（供托盘「编辑最近一张」与贴图复用）。
    last_preview: Option<PreviewImage>,
    /// 最近保存的文件记录。
    recents: Vec<(PathBuf, String)>,
    /// 开机启动当前状态。
    autostart: bool,
    /// 录屏默认设置缓存（与准备态共用 config.record）。
    record_cfg: config::RecordBehavior,
    /// OCR 后端模式：`"local"`（本机 RapidOCR 管道）或 `"api"`（外部 HTTP 接口）。
    ocr_mode: String,
    /// 是否正在检查 GitHub 更新。
    checking_update: bool,
    /// 检查到新版本时的 Release 页面地址。
    latest_release_url: Option<String>,
}

impl RivixApp {
    /// 创建设置中心主视图。
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        region_hotkey: String,
        full_hotkey: String,
        picker_hotkey: String,
    ) -> Self {
        let cfg = config::load();
        let api_default = cfg.ocr.api_url.clone().unwrap_or_default();
        let ocr_api_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("http://127.0.0.1:8000/ocr")
                .default_value(api_default)
        });
        Self {
            capturing: false,
            recording_hotkey: false,
            hotkey_focus: cx.focus_handle(),
            ocr_api_input,
            region_hotkey,
            full_hotkey,
            picker_hotkey,
            status: SharedString::from(tr("app.slogan")),
            settings_tab: SettingsTab::default(),
            save_dir: load_save_dir(),
            open_editor: load_open_editor(),
            last_preview: None,
            recents: Vec::new(),
            autostart: autostart::is_enabled(),
            record_cfg: cfg.record,
            ocr_mode: if cfg.ocr.mode.eq_ignore_ascii_case("api") {
                "api".to_string()
            } else {
                "local".to_string()
            },
            checking_update: false,
            latest_release_url: None,
        }
    }

    /// 设置左下角状态文本并刷新界面。
    pub fn set_status(&mut self, msg: &str, cx: &mut Context<Self>) {
        self.status = SharedString::from(msg.to_string());
        cx.notify();
    }

    /// 新增一条保存记录（标注窗口保存成功后回写）。
    #[allow(dead_code)]
    pub fn push_recent(&mut self, path: PathBuf, msg: String, cx: &mut Context<Self>) {
        self.recents.insert(0, (path, msg.clone()));
        self.recents.truncate(10);
        self.set_status(&msg, cx);
    }

    /// 发起区域截图（框选时可按 F/B/V/T/C/S/P/Q）。
    pub fn begin_region(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        begin_capture(self, window, cx, CaptureMode::Region);
    }

    /// 发起全屏截图。
    pub fn begin_full(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        begin_capture(self, window, cx, CaptureMode::Full);
    }

    /// 发起长截图。
    pub fn begin_long(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        begin_capture(self, window, cx, CaptureMode::Long);
    }

    /// 发起屏幕取色。
    pub fn begin_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        begin_picker(self, window, cx);
    }

    /// 打开本地图片文件进入标注编辑窗口。
    #[allow(dead_code)]
    pub fn open_image_for_edit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(tr("image.pick").into()),
        });
        cx.spawn(async move |weak, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let decoded = cx
                .background_executor()
                .spawn(async move { preview_from_path(&path) })
                .await;
            let _ = weak.update(cx, |this, cx| match decoded {
                Some(preview) => {
                    this.set_status(&tr("viewer.opened_image"), cx);
                    open_viewer(cx.entity(), cx, preview);
                }
                None => this.set_status(&tr("image.load_fail"), cx),
            });
        })
        .detach();
    }

    /// 按录制到的新快捷键应用并持久化（无 BoxShadow 渲染依赖）。
    fn apply_region_hotkey(&mut self, value: Option<HotkeyValue>, cx: &mut Context<Self>) {
        self.recording_hotkey = false;
        let text = value
            .as_ref()
            .map(hotkey_value_to_string)
            .unwrap_or_default();
        match apply_global_hotkey(cx, HotkeyKind::Region, &text) {
            Ok(()) => {
                self.region_hotkey = text.clone();
                save_hotkey_string(HotkeyKind::Region, &text);
                let msg = if text.is_empty() {
                    tr_fmt("hotkey.disabled", &[("label", &HotkeyKind::Region.label())])
                } else {
                    tr_fmt(
                        "hotkey.updated",
                        &[("label", &HotkeyKind::Region.label()), ("text", &text)],
                    )
                };
                self.set_status(&msg, cx);
            }
            Err(error) => self.set_status(&error, cx),
        }
    }

    /// 切换「截图后打开标注编辑窗」开关。
    fn toggle_open_editor(&mut self, cx: &mut Context<Self>) {
        self.open_editor = !self.open_editor;
        store_open_editor(self.open_editor);
        let state = tr(if self.open_editor {
            "behavior.on"
        } else {
            "behavior.off"
        });
        let msg = tr_fmt(
            "behavior.changed",
            &[
                ("title", &tr("behavior.open_editor.title")),
                ("state", &state),
            ],
        );
        self.set_status(&msg, cx);
    }

    /// 选择默认截图保存目录。
    fn pick_save_dir(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(tr("savedir.prompt").into()),
        });
        cx.spawn(async move |weak, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            store_save_dir(&dir);
            let _ = weak.update(cx, |this, cx| {
                this.save_dir = dir.clone();
                this.set_status(
                    &tr_fmt("savedir.updated", &[("dir", &dir.display().to_string())]),
                    cx,
                );
            });
        })
        .detach();
    }

    /// 重置保存目录为系统默认路径。
    fn reset_save_dir(&mut self, cx: &mut Context<Self>) {
        let dir = default_save_dir();
        store_save_dir(&dir);
        self.save_dir = dir.clone();
        self.set_status(
            &tr_fmt("savedir.reset_done", &[("dir", &dir.display().to_string())]),
            cx,
        );
    }

    /// 切换开机自动启动。
    fn toggle_autostart(&mut self, cx: &mut Context<Self>) {
        let next = !self.autostart;
        match autostart::set_enabled(next) {
            Ok(()) => {
                self.autostart = autostart::is_enabled();
                self.set_status(
                    &tr(if self.autostart {
                        "autostart.on"
                    } else {
                        "autostart.off"
                    }),
                    cx,
                );
            }
            Err(error) => {
                self.autostart = autostart::is_enabled();
                self.set_status(
                    &tr_fmt("autostart.fail", &[("error", &error.to_string())]),
                    cx,
                );
            }
        }
    }

    /// 保存当前录屏默认设置到配置文件并刷新界面。
    fn persist_record_cfg(&mut self, cx: &mut Context<Self>) {
        let cfg_copy = self.record_cfg.clone();
        config::update(|cfg| cfg.record = cfg_copy);
        cx.notify();
    }

    /// 保存 OCR 后端模式与外部 API 地址。
    fn save_ocr_settings(&mut self, mode: &str, cx: &mut Context<Self>) {
        self.ocr_mode = mode.to_string();
        let api_url = self.ocr_api_input.read(cx).value().trim().to_string();
        let mode_owned = self.ocr_mode.clone();
        config::update(|cfg| {
            cfg.ocr.mode = mode_owned;
            cfg.ocr.api_url = if api_url.is_empty() {
                None
            } else {
                Some(api_url)
            };
        });
        let msg = if self.ocr_mode == "api" {
            "已切换至外部 OCR API"
        } else {
            "已切换至本机 RapidOCR 管道"
        };
        self.set_status(msg, cx);
    }

    /// 检查 GitHub Releases 最新版本（异步不阻塞 UI；发现新版本时再次点击可打开下载页）。
    fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        if let Some(ref url) = self.latest_release_url {
            cx.open_url(url);
            return;
        }
        if self.checking_update {
            return;
        }
        self.checking_update = true;
        self.set_status(&tr("update.checking"), cx);
        cx.spawn(async move |weak, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move { fetch_latest_github_release() })
                .await;
            let _ = weak.update(cx, |this, cx| {
                this.checking_update = false;
                match outcome {
                    Ok((tag, html_url)) => {
                        let remote = tag.trim_start_matches('v');
                        if remote == APP_VERSION {
                            this.set_status(&tr_fmt("update.latest", &[("ver", APP_VERSION)]), cx);
                        } else {
                            this.latest_release_url = Some(html_url);
                            this.set_status(&tr_fmt("update.found", &[("tag", &tag)]), cx);
                        }
                    }
                    Err(_) => {
                        this.set_status(&tr("update.fail"), cx);
                    }
                }
            });
        })
        .detach();
    }

    /// 记忆最近一张截图。
    #[allow(dead_code)]
    pub(crate) fn remember_preview(&mut self, preview: PreviewImage, cx: &mut Context<Self>) {
        self.last_preview = Some(preview);
        cx.notify();
    }

    /// 是否存在仍可重新编辑的最近截图。
    #[allow(dead_code)]
    pub fn has_last_preview(&self) -> bool {
        self.last_preview
            .as_ref()
            .is_some_and(|preview| preview.path.is_file())
    }

    /// 重新打开最近一张截图进入编辑窗口；若剪贴板有用户新复制的图片文件或图像，优先加载剪贴板图片。
    pub fn open_last_for_edit(&mut self, cx: &mut Context<Self>) {
        if let Some(clip_path) = export::read_image_from_clipboard(self.last_preview.is_none())
            && let Some(preview) = preview_from_path(&clip_path)
        {
            self.last_preview = Some(preview.clone());
            open_viewer(cx.entity(), cx, preview);
            return;
        }
        let Some(preview) = self.last_preview.clone() else {
            self.set_status(&tr("image.no_recent"), cx);
            return;
        };
        if !preview.path.is_file() {
            self.last_preview = None;
            self.set_status(&tr("image.no_recent"), cx);
            cx.notify();
            return;
        }
        open_viewer(cx.entity(), cx, preview);
    }

    /// 将最近一次截图作为独立贴图窗口置顶打开。
    pub fn pin_last(&mut self, cx: &mut Context<Self>) {
        let Some(preview) = self.last_preview.clone() else {
            self.set_status(&tr("image.no_recent"), cx);
            return;
        };
        if !preview.path.is_file() {
            self.last_preview = None;
            self.set_status(&tr("image.no_recent"), cx);
            cx.notify();
            return;
        }
        let path = preview.path;
        let main = cx.entity();
        cx.spawn(async move |_, cx| {
            let decoded = cx
                .background_executor()
                .spawn(async move {
                    image::open(&path)
                        .map(|img| crate::pin::PinImage::new(img.to_rgba8()))
                        .map_err(|err| err.to_string())
                })
                .await;
            cx.update(|cx| match decoded {
                Ok(pin_image) => {
                    if let Err(err) = crate::pin::open(pin_image, None, 1.0, cx) {
                        main.update(cx, |view, cx| view.set_status(&err, cx));
                    }
                }
                Err(err) => {
                    main.update(cx, |view, cx| view.set_status(&err, cx));
                }
            });
        })
        .detach();
    }

    /// Tab 1：截图与热键（唯一全局呼出快捷键 + 8 键帽速查网格 + 打开编辑窗 + 保存目录）。
    fn capture_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let view = cx.entity();
        let save_dir_text = self.save_dir.display().to_string();

        div()
            .flex()
            .flex_col()
            .gap_3()
            // 1. 唯一全局呼出快捷键卡片
            .child(
                div()
                    .p_4()
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(rgb(p.border))
                    .bg(rgb(p.card))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(p.selected_text))
                                    .child("呼出截图快捷键"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(p.muted))
                                    .child("全局有效，呼出选区后可通过以下单键直达全部功能"),
                            ),
                    )
                    .child({
                        let recording = self.recording_hotkey;
                        let display = if recording {
                            tr("hotkey.recording")
                        } else if self.region_hotkey.is_empty() {
                            tr("hotkey.placeholder")
                        } else {
                            self.region_hotkey.to_uppercase()
                        };
                        let has_key = !self.region_hotkey.is_empty();
                        div()
                            .id("rivix-hotkey-recorder")
                            .track_focus(&self.hotkey_focus)
                            .w(px(164.0))
                            .h(px(34.0))
                            .px_3()
                            .flex_shrink_0()
                            .rounded(px(8.0))
                            .border_1()
                            .border_color(rgb(if recording { p.primary } else { p.border }))
                            .bg(rgb(if recording { p.selected } else { p.background }))
                            .flex()
                            .items_center()
                            .justify_between()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.recording_hotkey = true;
                                window.focus(&this.hotkey_focus, cx);
                                cx.notify();
                            }))
                            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                                if !this.recording_hotkey {
                                    return;
                                }
                                if ev.keystroke.key.as_str() == "escape" {
                                    this.recording_hotkey = false;
                                    cx.notify();
                                    return;
                                }
                                if let Some(val) = HotkeyValue::from_keystroke(&ev.keystroke) {
                                    this.apply_region_hotkey(Some(val), cx);
                                }
                            }))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(if recording || !has_key {
                                        p.muted
                                    } else {
                                        p.primary
                                    }))
                                    .child(display),
                            )
                            .when(has_key && !recording, |row| {
                                row.child(
                                    div()
                                        .id("rivix-hotkey-clear")
                                        .px_1()
                                        .rounded(px(4.0))
                                        .text_xs()
                                        .text_color(rgb(p.muted))
                                        .hover(|s| s.bg(rgb(p.chip_off_bg)))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.apply_region_hotkey(None, cx);
                                            cx.stop_propagation();
                                        }))
                                        .child("×"),
                                )
                            })
                    }),
            )
            // 2. 选区内置单键速查网格（每行两列）
            .child(
                div()
                    .p_4()
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(rgb(p.border))
                    .bg(rgb(p.card))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(p.muted))
                            .child("选区内置功能键（框选期间直接按下）"),
                    )
                    .children(OVERLAY_KEY_HINTS.chunks(2).map(|pair| {
                        div()
                            .flex()
                            .gap_2()
                            .children(pair.iter().map(|(key, title, desc)| {
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .p_2()
                                    .rounded(px(9.0))
                                    .bg(rgb(p.chip_off_bg))
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .child(
                                        div()
                                            .w(px(26.0))
                                            .h(px(24.0))
                                            .flex_shrink_0()
                                            .rounded(px(6.0))
                                            .border_1()
                                            .border_color(rgb(p.border))
                                            .bg(rgb(p.background))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_xs()
                                            .text_color(rgb(p.selected_text))
                                            .child(*key),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.0))
                                            .flex()
                                            .flex_col()
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(p.selected_text))
                                                    .child(*title),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(p.muted))
                                                    .child(*desc),
                                            ),
                                    )
                            }))
                    })),
            )
            // 3. 截图行为（永久自动复制）+ 保存目录
            .child(
                div()
                    .p_4()
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(rgb(p.border))
                    .bg(rgb(p.card))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(rgb(p.selected_text))
                                            .child(tr("behavior.open_editor.title")),
                                    )
                                    .child(div().text_xs().text_color(rgb(p.muted)).child(
                                        "所有截图默认自动复制到剪贴板；开启此项将自动弹出标注窗",
                                    )),
                            )
                            .child({
                                let v = view.clone();
                                Switch::new("rivix-switch-open-editor")
                                    .checked(self.open_editor)
                                    .color(rgb(p.primary))
                                    .on_change(move |_, _, cx| {
                                        v.update(cx, |this, cx| this.toggle_open_editor(cx));
                                    })
                            }),
                    )
                    .child(div().h(px(1.0)).w_full().bg(rgb(p.border)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(rgb(p.selected_text))
                                            .child(tr("savedir.title")),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(p.muted))
                                            .child(save_dir_text),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child({
                                        let v = view.clone();
                                        div()
                                            .id("rivix-btn-save-dir")
                                            .px_3()
                                            .py_1()
                                            .rounded(px(8.0))
                                            .text_xs()
                                            .cursor_pointer()
                                            .bg(rgb(p.selected))
                                            .text_color(rgb(p.selected_text))
                                            .child(tr("savedir.pick"))
                                            .on_click(move |_, _, cx| {
                                                v.update(cx, |this, cx| this.pick_save_dir(cx));
                                            })
                                    })
                                    .child({
                                        let v = view.clone();
                                        div()
                                            .id("rivix-btn-save-dir-reset")
                                            .px_3()
                                            .py_1()
                                            .rounded(px(8.0))
                                            .text_xs()
                                            .cursor_pointer()
                                            .border_1()
                                            .border_color(rgb(p.border))
                                            .bg(rgb(p.chip_off_bg))
                                            .text_color(rgb(p.text))
                                            .child(tr("savedir.reset"))
                                            .on_click(move |_, _, cx| {
                                                v.update(cx, |this, cx| this.reset_save_dir(cx));
                                            })
                                    }),
                            ),
                    ),
            )
    }

    /// Tab 2：录屏设置（FFmpeg · NVENC 状态徽章 + 默认参数 + 画面/声音开关）。
    fn record_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let (has_ffmpeg, has_nvenc) = crate::record::encoder_capabilities();
        let enc_label = if has_nvenc {
            "FFmpeg · NVENC 可用"
        } else if has_ffmpeg {
            "FFmpeg 就绪 (CPU)"
        } else {
            "未检测到 ffmpeg.exe"
        };
        let is_mp4 = !self.record_cfg.format.eq_ignore_ascii_case("gif");

        div()
            .flex()
            .flex_col()
            .gap_3()
            // 1. 编码器状态与默认格式/帧率
            .child(
                div()
                    .p_4()
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(rgb(p.border))
                    .bg(rgb(p.card))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(p.muted))
                                    .child("编码引擎与默认参数"),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded(px(999.0))
                                    .bg(rgba(if has_ffmpeg { 0x34D39922 } else { 0xE0313122 }))
                                    .text_xs()
                                    .text_color(rgb(if has_ffmpeg { 0x34D399 } else { 0xE03131 }))
                                    .child(enc_label),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(rgb(p.selected_text))
                                            .child("默认输出格式"),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(p.muted))
                                            .child("MP4 分段封装防崩溃并调用硬件编码；GIF 适合短演示"),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .p_1()
                                    .rounded(px(8.0))
                                    .bg(rgb(p.background))
                                    .border_1()
                                    .border_color(rgb(p.border))
                                    .child(seg_chip("rec-fmt-mp4", "MP4", is_mp4, p).on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.record_cfg.format = "mp4".to_string();
                                            this.persist_record_cfg(cx);
                                        }),
                                    ))
                                    .child(seg_chip("rec-fmt-gif", "GIF", !is_mp4, p).on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.record_cfg.format = "gif".to_string();
                                            this.persist_record_cfg(cx);
                                        }),
                                    )),
                            ),
                    )
                    .child(div().h(px(1.0)).w_full().bg(rgb(p.border)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(rgb(p.selected_text))
                                            .child("默认帧率"),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(p.muted))
                                            .child("日常演示推荐 30 fps，高刷操作可选 60 fps"),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .p_1()
                                    .rounded(px(8.0))
                                    .bg(rgb(p.background))
                                    .border_1()
                                    .border_color(rgb(p.border))
                                    .children([15u32, 30, 60].into_iter().map(|fps| {
                                        seg_chip(
                                            ("rec-fps", fps as usize),
                                            &format!("{fps} fps"),
                                            self.record_cfg.fps == fps,
                                            p,
                                        )
                                        .on_click(cx.listener(
                                            move |this, _, _, cx| {
                                                this.record_cfg.fps = fps;
                                                this.persist_record_cfg(cx);
                                            },
                                        ))
                                    })),
                            ),
                    ),
            )
            // 2. 画面与音频默认开关
            .child(
                div()
                    .p_4()
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(rgb(p.border))
                    .bg(rgb(p.card))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(p.muted))
                            .child("画面与声音默认开关"),
                    )
                    .child(setting_switch_row(
                        "rec-sw-cursor",
                        "显示鼠标光标",
                        "录制时实时叠加系统真实鼠标指针",
                        self.record_cfg.show_cursor,
                        p,
                        {
                            let v = cx.entity();
                            move |_, _, cx| {
                                v.update(cx, |this, cx| {
                                    this.record_cfg.show_cursor = !this.record_cfg.show_cursor;
                                    this.persist_record_cfg(cx);
                                });
                            }
                        },
                    ))
                    .child(div().h(px(1.0)).w_full().bg(rgb(p.border)))
                    .child(setting_switch_row(
                        "rec-sw-sys-audio",
                        "录制系统声音",
                        "WASAPI 内录，自动按系统主音量补偿响度并防爆音",
                        self.record_cfg.system_audio,
                        p,
                        {
                            let v = cx.entity();
                            move |_, _, cx| {
                                v.update(cx, |this, cx| {
                                    this.record_cfg.system_audio = !this.record_cfg.system_audio;
                                    this.persist_record_cfg(cx);
                                });
                            }
                        },
                    ))
                    .child(div().h(px(1.0)).w_full().bg(rgb(p.border)))
                    .child(setting_switch_row(
                        "rec-sw-mic",
                        "录制麦克风",
                        "与系统声音混音输出到 MP4 音轨",
                        self.record_cfg.mic_audio,
                        p,
                        {
                            let v = cx.entity();
                            move |_, _, cx| {
                                v.update(cx, |this, cx| {
                                    this.record_cfg.mic_audio = !this.record_cfg.mic_audio;
                                    this.persist_record_cfg(cx);
                                });
                            }
                        },
                    )),
            )
            // 3. 录屏输出说明
            .child(
                div()
                    .px_3()
                    .py_2()
                    .rounded(px(10.0))
                    .bg(rgb(p.chip_off_bg))
                    .text_xs()
                    .text_color(rgb(p.muted))
                    .child("录屏自动保存至系统「下载 (Downloads)」文件夹，按 Shift + Esc 或双击托盘红点即可结束。"),
            )
    }

    /// Tab 3：通用与引擎（OCR 后端模式、语言、开机自启、关于与配置目录）。
    fn general_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let view = cx.entity();
        let current_lang = lang();
        let is_api = self.ocr_mode == "api";
        let ocr_ready = ocr::models_available();

        div()
            .flex()
            .flex_col()
            .gap_3()
            // 1. OCR 文字识别引擎卡片
            .child(
                div()
                    .p_4()
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(rgb(p.border))
                    .bg(rgb(p.card))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(p.muted))
                                    .child("OCR 文字识别引擎"),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded(px(999.0))
                                    .bg(rgba(if ocr_ready { 0x34D39922 } else { 0xF59F0022 }))
                                    .text_xs()
                                    .text_color(rgb(if ocr_ready { 0x34D399 } else { 0xF59F00 }))
                                    .child(if ocr_ready { "已就绪" } else { "待配置" }),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(rgb(p.selected_text))
                                            .child("识别后端模式"),
                                    )
                                    .child(div().text_xs().text_color(rgb(p.muted)).child(
                                        "保留本机 RapidOCR CUDA 管道调用；也支持切换外部 HTTP API",
                                    )),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .p_1()
                                    .rounded(px(8.0))
                                    .bg(rgb(p.background))
                                    .border_1()
                                    .border_color(rgb(p.border))
                                    .child(
                                        seg_chip("ocr-mode-local", "本机 RapidOCR", !is_api, p)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.save_ocr_settings("local", cx);
                                            })),
                                    )
                                    .child(
                                        seg_chip("ocr-mode-api", "外部 HTTP API", is_api, p)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.save_ocr_settings("api", cx);
                                            })),
                                    ),
                            ),
                    )
                    .when(is_api, |card| {
                        card.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .child(Input::new(&self.ocr_api_input)),
                                )
                                .child(
                                    div()
                                        .id("ocr-api-save")
                                        .px_3()
                                        .py_1()
                                        .rounded(px(8.0))
                                        .text_xs()
                                        .cursor_pointer()
                                        .bg(rgb(p.selected))
                                        .text_color(rgb(p.selected_text))
                                        .child("保存")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.save_ocr_settings("api", cx);
                                        })),
                                ),
                        )
                    }),
            )
            // 2. 系统与外观（语言 + 开机自启）
            .child(
                div()
                    .p_4()
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(rgb(p.border))
                    .bg(rgb(p.card))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(rgb(p.selected_text))
                                            .child(tr("lang.title")),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(p.muted))
                                            .child("切换后同步刷新设置窗口与托盘菜单"),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .p_1()
                                    .rounded(px(8.0))
                                    .bg(rgb(p.background))
                                    .border_1()
                                    .border_color(rgb(p.border))
                                    .children(available_langs().into_iter().map(|item| {
                                        let v = view.clone();
                                        let selected = item == current_lang;
                                        let code = item.code();
                                        let name = item.display_name();
                                        seg_chip(
                                            SharedString::from(format!("rivix-lang-{code}")),
                                            &name,
                                            selected,
                                            p,
                                        )
                                        .on_click(
                                            move |_, _, cx| {
                                                set_lang(&code);
                                                crate::tray::refresh_labels(cx);
                                                v.update(cx, |this, cx| {
                                                    let msg =
                                                        tr_fmt("lang.switched", &[("lang", &name)]);
                                                    this.set_status(&msg, cx);
                                                });
                                            },
                                        )
                                    })),
                            ),
                    )
                    .child(div().h(px(1.0)).w_full().bg(rgb(p.border)))
                    .child(setting_switch_row(
                        "rivix-btn-autostart",
                        &tr("autostart.title"),
                        &tr("autostart.note"),
                        self.autostart,
                        p,
                        {
                            let v = view.clone();
                            move |_, _, cx| {
                                v.update(cx, |this, cx| this.toggle_autostart(cx));
                            }
                        },
                    )),
            )
            // 3. 关于与配置文件目录
            .child(
                div()
                    .p_4()
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(rgb(p.border))
                    .bg(rgb(p.card))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(p.selected_text))
                                    .child(format!("Rivix · 望川 v{APP_VERSION}")),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(p.muted))
                                    .child("基于 Rust + rgpui 1.8.1 · 配置：rivix-config.json"),
                            ),
                    )
                    .child(
                        div()
                            .id("rivix-open-cfg-dir")
                            .px_3()
                            .py_1()
                            .rounded(px(8.0))
                            .text_xs()
                            .cursor_pointer()
                            .border_1()
                            .border_color(rgb(p.border))
                            .bg(rgb(p.chip_off_bg))
                            .text_color(rgb(p.text))
                            .child("打开配置目录")
                            .on_click(|_, _, cx| {
                                if let Some(dir) = config::config_path().parent() {
                                    cx.open_with_system(dir);
                                }
                            }),
                    ),
            )
    }
}

/// 分段选择芯片按钮（共用组件）。
fn seg_chip(
    id: impl Into<rgpui::ElementId>,
    label: &str,
    active: bool,
    p: &ThemePalette,
) -> rgpui::Stateful<rgpui::Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded(px(6.0))
        .text_xs()
        .cursor_pointer()
        .bg(rgb(if active { p.selected } else { p.background }))
        .text_color(rgb(if active { p.selected_text } else { p.muted }))
        .child(label.to_string())
}

/// 带标题、副标题和开关的设置行（共用组件）。
fn setting_switch_row(
    id: &'static str,
    title: &str,
    desc: &str,
    checked: bool,
    p: &ThemePalette,
    on_change: impl Fn(bool, &mut Window, &mut rgpui::App) + Send + Sync + 'static,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(p.selected_text))
                        .child(title.to_string()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(p.muted))
                        .child(desc.to_string()),
                ),
        )
        .child(
            Switch::new(id)
                .checked(checked)
                .color(rgb(p.primary))
                .on_change(on_change),
        )
}

/// 请求 GitHub Releases API 获取最新版本号与发布页链接。
fn fetch_latest_github_release() -> Result<(String, String), String> {
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-sS", "-m", "10", "-H", "User-Agent: rivix"])
        .arg("https://api.github.com/repos/kelsoz/Rivix/releases/latest")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err("request failed".to_string());
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    let tag = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing tag_name".to_string())?
        .to_string();
    let html_url = json
        .get("html_url")
        .and_then(|v| v.as_str())
        .unwrap_or(REPO_URL)
        .to_string();
    Ok((tag, html_url))
}

impl Render for RivixApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let view = cx.entity();
        let tab = self.settings_tab;
        let status = self.status.clone();
        let dark = is_dark_mode();

        div()
            .size_full()
            .flex()
            .bg(rgb(p.background))
            .text_color(rgb(p.text))
            // 左侧一体化全高侧边栏（品牌 + 3 个导航 Tab + 检查更新 + 状态语）
            .child(
                div()
                    .w(px(196.0))
                    .h_full()
                    .flex_shrink_0()
                    .bg(rgb(p.outer))
                    .border_r_1()
                    .border_color(rgb(p.border))
                    .px_3()
                    .py_4()
                    .flex()
                    .flex_col()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_4()
                            // 品牌头（支持拖动窗口）
                            .child(
                                div()
                                    .window_control_area(WindowControlArea::Drag)
                                    .px_2()
                                    .py_1()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .child(img(settings_logo()).w(px(28.0)).h(px(28.0)))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .text_color(rgb(p.selected_text))
                                                    .child("Rivix · 望川"),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(p.muted))
                                                    .child(format!("v{APP_VERSION}")),
                                            ),
                                    ),
                            )
                            // 3 个现代导航 Tab
                            .child(div().flex().flex_col().gap_1().children(
                                SettingsTab::all().into_iter().map(|item| {
                                    let v = view.clone();
                                    let selected = item == tab;
                                    div()
                                        .id(format!("rivix-tab-{}", item.id()))
                                        .relative()
                                        .px_3()
                                        .py_2()
                                        .rounded(px(9.0))
                                        .text_sm()
                                        .cursor_pointer()
                                        .bg(rgb(if selected { p.selected } else { p.outer }))
                                        .text_color(rgb(if selected {
                                            p.selected_text
                                        } else {
                                            p.muted
                                        }))
                                        .hover(move |style| {
                                            style.bg(rgb(if selected {
                                                p.selected
                                            } else {
                                                p.tab_hover_bg
                                            }))
                                        })
                                        .when(selected, |row| {
                                            row.child(
                                                div()
                                                    .absolute()
                                                    .left(px(0.0))
                                                    .top(px(9.0))
                                                    .w(px(3.0))
                                                    .h(px(16.0))
                                                    .rounded(px(2.0))
                                                    .bg(rgb(p.primary)),
                                            )
                                        })
                                        .child(item.label())
                                        .on_click(move |_, _, cx| {
                                            v.update(cx, |this, cx| {
                                                this.settings_tab = SettingsTab::from_id(item.id());
                                                cx.notify();
                                            })
                                        })
                                }),
                            )),
                    )
                    // 左下角：检查更新按钮 + 「简洁,却不简单」状态提示
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .id("rivix-btn-check-update")
                                    .w_full()
                                    .h(px(34.0))
                                    .rounded(px(9.0))
                                    .border_1()
                                    .border_color(rgb(p.primary_active_border))
                                    .bg(rgb(p.selected))
                                    .text_color(rgb(p.selected_text))
                                    .text_xs()
                                    .cursor_pointer()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .hover(|s| s.bg(rgb(p.primary)))
                                    .child(if self.checking_update {
                                        tr("update.checking")
                                    } else {
                                        tr("update.check")
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.check_for_updates(cx);
                                    })),
                            )
                            .child(
                                div()
                                    .id("rivix-status")
                                    .px_1()
                                    .text_xs()
                                    .text_color(rgb(p.muted))
                                    .child(status),
                            ),
                    ),
            )
            // 右侧：融合式顶栏（拖拽 + 主题切换 + 最小化/关闭）+ 滚动内容区
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .window_control_area(WindowControlArea::Drag)
                            .h(px(48.0))
                            .flex_shrink_0()
                            .px_5()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_base()
                                    .text_color(rgb(p.selected_text))
                                    .child(tab.label()),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .id("rivix-btn-theme")
                                            .px_3()
                                            .py_1()
                                            .rounded(px(8.0))
                                            .border_1()
                                            .border_color(rgb(p.border))
                                            .bg(rgb(p.card))
                                            .text_xs()
                                            .text_color(rgb(p.text))
                                            .cursor_pointer()
                                            .on_click(|_, _, cx| {
                                                config::update(|cfg| {
                                                    let next = !cfg.dark_mode.unwrap_or(true);
                                                    cfg.dark_mode = Some(next);
                                                });
                                                apply_theme(cx);
                                                crate::tray::refresh_labels(cx);
                                                cx.refresh_windows();
                                            })
                                            .child(if dark { "深色" } else { "浅色" }),
                                    )
                                    .child(
                                        div()
                                            .id("rivix-btn-min")
                                            .w(px(28.0))
                                            .h(px(28.0))
                                            .rounded(px(8.0))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_xs()
                                            .text_color(rgb(p.muted))
                                            .cursor_pointer()
                                            .hover(|s| s.bg(rgb(p.chip_off_bg)))
                                            .child("─")
                                            .on_click(|_, window, _| {
                                                window.minimize_window();
                                            }),
                                    )
                                    .child(
                                        div()
                                            .id("rivix-btn-close")
                                            .w(px(28.0))
                                            .h(px(28.0))
                                            .rounded(px(8.0))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_xs()
                                            .text_color(rgb(p.muted))
                                            .cursor_pointer()
                                            .hover(|s| {
                                                s.bg(rgb(0xC92A2A)).text_color(rgb(0xFFFFFF))
                                            })
                                            .child("✕")
                                            .on_click(|_, window, _| {
                                                window.hide_window();
                                            }),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .id("settings-content")
                            .overflow_y_scroll()
                            .flex_1()
                            .min_h(px(0.0))
                            .px_5()
                            .pb_5()
                            .child(match tab {
                                SettingsTab::Capture => self.capture_tab(cx).into_any_element(),
                                SettingsTab::Record => self.record_tab(cx).into_any_element(),
                                SettingsTab::General => self.general_tab(cx).into_any_element(),
                            }),
                    ),
            )
    }
}
