//! 图片标注编辑器：自定义标题栏、矢量绘制、原图像素取色与滚轮缩放。
//! 绘制中按空格平移当前草稿，松开空格继续绘制；空白处空格拖动或中键平移画布。
//! 关闭时提交文字和有效草稿，后台复制结果，不显示二次确认；Ctrl+S 单独保存结果图。

use std::{
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use ab_glyph::FontVec;
use image::RgbaImage;
use rgpui::{
    Bounds, Context, CursorStyle, Div, Entity, FocusHandle, Focusable, KeyDownEvent, KeyUpEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Path, PathBuilder, Pixels, Render,
    Rgba, ScrollDelta, ScrollWheelEvent, SharedString, Slider, SliderEvent, SliderState,
    Subscription, Window, WindowBounds, WindowOptions, canvas, div, fill, img,
    input_ui::{Input, InputState},
    point,
    prelude::*,
    px, rgb, rgba, size,
};

use crate::{
    app::{PreviewImage, RivixApp, load_save_dir},
    capture::cap_log,
    export::{
        copy_image_and_file_to_clipboard, copy_png_to_clipboard, load_system_font, png_bytes,
        render_export,
    },
    i18n::{tr, tr_fmt},
    shapes::{AnnoColor, PixelRect, Shape, TEXT_LINE_HEIGHT},
};

/// 最小缩放（10%）。
const MIN_ZOOM: f32 = 0.1;
/// 最大缩放（800%）。
const MAX_ZOOM: f32 = 8.0;
/// 单次按钮缩放步进（×1.25）。
const ZOOM_STEP: f32 = 1.25;
/// 预览窗口最小宽（逻辑像素，保证左右悬浮面板与中央画布协调）。
const MIN_VIEWER_W: f32 = 860.0;
/// 预览窗口最小总高（逻辑像素）。
const MIN_VIEWER_H: f32 = 600.0;
/// 画笔默认线宽（图片像素）。
const DEFAULT_WIDTH: f32 = 4.0;
/// 橡皮擦半径（图片像素，画笔类额外叠加自身半宽）。
const ERASER_RADIUS: f32 = 8.0;
/// 文字标注默认字号（图片像素）。
const DEFAULT_TEXT_SIZE: f32 = 28.0;
/// Windows 明确使用微软雅黑，与离线导出读取的系统字体保持一致。
#[cfg(target_os = "windows")]
const TEXT_FONT_FAMILY: &str = "Microsoft YaHei";
/// 其他平台仍使用系统默认字体。
#[cfg(not(target_os = "windows"))]
const TEXT_FONT_FAMILY: &str = ".SystemUIFont";
/// 线宽最小值（图片像素，滑杆下限）。
const WIDTH_MIN: f32 = 1.0;
/// 线宽最大值（图片像素，滑杆上限）。
const WIDTH_MAX: f32 = 30.0;

/// 规范化线宽：钳制到滑杆范围并取整（图片像素，滑杆回调与步进共用）。
fn normalize_width(w: f32) -> f32 {
    w.clamp(WIDTH_MIN, WIDTH_MAX).round()
}

/// 标注工具（工具条标注排）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AnnoTool {
    /// 选择 / 拖拽平移。
    Select,
    /// 矩形框。
    Rect,
    /// 椭圆框。
    Ellipse,
    /// 直线。
    Line,
    /// 截图像素吸管。
    Eyedropper,
    /// 箭头。
    Arrow,
    /// 自由画笔。
    Pen,
    /// 荧光笔：半透明粗线高亮（导出时自动半透明加粗）。
    Highlighter,
    /// 橡皮擦：点击 / 拖动删除命中的标注。
    Eraser,
    /// 马赛克：矩形区域打码。
    Mosaic,
    /// 文字（点选落点，内容取输入框）。
    Text,
}

impl AnnoTool {
    /// 快捷键切工具：`V` 选择，`1-8` 按标注排顺序（矩形 / 椭圆 / 箭头 / 画笔 / 荧光笔 / 橡皮擦 / 马赛克 / 文字）。
    fn from_shortcut(key: &str) -> Option<Self> {
        match key {
            "v" => Some(Self::Select),
            "r" => Some(Self::Rect),
            "l" => Some(Self::Line),
            "o" => Some(Self::Ellipse),
            "i" => Some(Self::Eyedropper),
            "e" => Some(Self::Eraser),
            "t" => Some(Self::Text),
            "b" => Some(Self::Pen),
            "m" => Some(Self::Mosaic),
            "1" => Some(Self::Rect),
            "2" => Some(Self::Ellipse),
            "3" => Some(Self::Arrow),
            "4" => Some(Self::Pen),
            "5" => Some(Self::Highlighter),
            "6" => Some(Self::Eraser),
            "7" => Some(Self::Mosaic),
            "8" => Some(Self::Text),
            _ => None,
        }
    }
}

/// 多选对齐方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AlignMode {
    /// 顶对齐。
    Top,
    /// 左对齐。
    Left,
    /// 底对齐。
    Bottom,
    /// 右对齐。
    Right,
    /// 水平居中对齐。
    CenterH,
    /// 垂直居中对齐。
    CenterV,
}

/// 工具条动作（按钮与快捷键共用分发，Debug 用于诊断日志）。
#[derive(Clone, Debug)]
enum ViewerAction {
    /// 放大。
    ZoomIn,
    /// 缩小。
    ZoomOut,
    /// 回到适应窗口。
    Fit,
    /// 复制烧录图到剪贴板。
    Copy,
    /// 保存烧录图到桌面。
    Save,
    /// 切换标注工具。
    Tool(AnnoTool),
    /// 撤销一个标注。
    Undo,
    /// 重做一个标注。
    Redo,
    /// 切换标注颜色。
    Color(AnnoColor),
    /// 步进调节线宽（图片像素，滑杆旁 − / + 与 [ ] 键共用）。
    WidthNudge(f32),
    /// 切换文字背景显示。
    TextBg,
    /// 步进调节字体大小（仅对当前文字或选中的文字有效）。
    TextSizeNudge(f32),
    /// 切换文字字体家族（仅对当前文字或选中的文字有效）。
    TextFont(Option<String>),
    /// 展开 / 收起系统字体下拉列表。
    ToggleFontDropdown,
    /// 切换取色器目标（`false` = 填充，`true` = 描边）。
    PickBg(bool),
    /// Shift+X 互换填充与描边（仅矩形 / 椭圆 / 多边形有效）。
    SwapFillStroke,
    /// 清除当前目标的可选颜色（矩形/椭圆/多边形的填充或描边，或文字描边）。
    ClearOptionalColor,
    /// 步进调节矩形圆角半径。
    RectRadiusNudge(f32),
    /// 多选对齐（顶 / 左 / 底 / 右 / 水平居中 / 垂直居中）。
    Align(AlignMode),
    /// 展开 / 收起左侧工具箱名称标签。
    ToggleDock,
    /// 切换深浅色主题。
    ToggleTheme,
    /// 打开最近一次保存的文件（系统默认程序）。
    OpenSavedFile,
    /// 打开最近一次保存文件所在目录（系统文件管理器）。
    OpenSavedDir,
    /// 进入或退出变换模式（与 Ctrl+T 相同）。
    Transform,
    /// 完成当前编辑并将结果置顶钉在屏幕上。
    Pin,
    /// 复制结果并关闭编辑窗口。
    Close,
}

/// 预览窗口初始布局：窗口大小固定（跟随显示器），图片居中、仅缩小适配。
///
/// 小截图不再撑出小窗口，而是在固定图片区内居中 100% 显示；
/// 大截图按比例缩小，保证完整落入图片区（之后可用 + / Ctrl+滚轮放大查看）。
///
/// # 参数
/// - `disp_w` / `disp_h`：主显示器逻辑尺寸。
/// - `img_w` / `img_h`：图片像素尺寸（宽高均大于 0）。
///
/// # 返回
/// `(图片区宽, 图片区高, 适应缩放)`，缩放只缩小不放大。
fn viewer_layout(disp_w: f32, disp_h: f32, img_w: u32, img_h: u32) -> (f32, f32, f32) {
    // 窗口跟随显示器固定大小：宽约 75%、总高约 72%，上下左右留边框余量；
    // max/min 链式写法避免极小显示器上 clamp 出现 min > max 的 panic。
    let win_w = (disp_w * 0.75)
        .round()
        .max(MIN_VIEWER_W)
        .min(disp_w - 20.0)
        .max(480.0);
    let win_h = (disp_h * 0.72)
        .round()
        .max(MIN_VIEWER_H)
        .min(disp_h - 20.0)
        .max(360.0);
    let area_w = win_w;
    let area_h = win_h.max(200.0);
    // 适应缩放：完整落入图片区，只缩小不放大（小图居中 100% 显示更清晰）。
    let fit = (area_w / img_w as f32)
        .min(area_h / img_h as f32)
        .clamp(MIN_ZOOM, 1.0);
    (area_w, area_h, fit)
}

/// 打开图片预览窗口。
///
/// # 参数
/// - `main`: 主视图实体（复制 / 保存后回写状态用）。
/// - `cx`: 调用方上下文（全屏截图与区域浮层各调一次，泛型兼容）。
/// - `preview`: 要展示的图片（路径 + 原图尺寸）。
pub fn open_viewer<T: 'static>(main: Entity<RivixApp>, cx: &mut Context<T>, preview: PreviewImage) {
    // 固定窗口：按显示器算出全幅画布区，图片居中、仅缩小适配。
    let (disp_w, disp_h) = cx
        .primary_display()
        .map(|d| {
            let s = d.bounds().size;
            (s.width.as_f32(), s.height.as_f32())
        })
        .unwrap_or((1920.0, 1080.0));
    let (area_w, area_h, fit) = viewer_layout(disp_w, disp_h, preview.w, preview.h);
    cap_log(&format!(
        "预览窗口打开：img={}x{} area={}x{} fit={fit:.2}",
        preview.w, preview.h, area_w as u32, area_h as u32
    ));
    // 全幅底层画布，工具箱与属性栏悬浮在其上层。
    let bounds = Bounds::centered(None, size(px(area_w), px(area_h)), cx);
    let opened = cx.open_window(
        viewer_window_options(bounds, &preview),
        move |window, cx| {
            let view = cx.new(|cx| {
                ImageViewer::new(
                    window,
                    cx,
                    main.clone(),
                    preview.clone(),
                    fit,
                    area_w,
                    area_h,
                )
            });
            // 打开即聚焦，Esc / 快捷键立即可用。
            let handle = view.read(cx).focus_handle.clone();
            window.focus(&handle, cx);
            window.set_window_title(&tr("viewer.editor_title"));
            // Alt+F4 与自定义关闭按钮使用同一份复制收尾。
            guard_viewer_close(window, cx, view.clone());
            view
        },
    );
    if let Err(error) = opened {
        cap_log(&format!("预览窗口打开失败：{error:?}"));
    }
}

/// 所有关闭入口先复制结果，无确认弹窗；异步复制完成后显式移除窗口。
fn guard_viewer_close(window: &mut Window, cx: &mut rgpui::App, view: Entity<ImageViewer>) {
    window.on_window_should_close(cx, move |window, cx| {
        view.update(cx, |this, cx| this.copy_and_close(window, cx));
        false
    });
}

/// 保留系统移动、缩放和圆角能力，标题栏由客户区绘制。
fn viewer_window_options(bounds: Bounds<Pixels>, _preview: &PreviewImage) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: None,
        app_owns_titlebar_drag: true,
        window_min_size: Some(size(px(MIN_VIEWER_W), px(MIN_VIEWER_H))),
        ..Default::default()
    }
}

/// 从本地图片路径构建预览信息（校验文件有效性）。
///
/// # 参数
/// - `path`: 用户选择的图片文件路径。
///
/// # 返回
/// 有效图片返回 `Some(PreviewImage)`，非图片 / 不存在返回 `None`。
pub fn preview_from_path(path: &std::path::Path) -> Option<PreviewImage> {
    // 先确认是文件，再读图片尺寸（不支持的格式直接返回 None）。
    if !path.is_file() {
        return None;
    }
    let (w, h) = image::image_dimensions(path).ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    Some(PreviewImage {
        path: path.to_path_buf(),
        w,
        h,
    })
}

/// 从 `App` 上下文打开本地图片编辑（托盘菜单入口用）。
///
/// 原生文件对话框选图后，用预览窗口打开做标注；保存时另存新文件，不覆盖原图。
///
/// # 参数
/// - `main`: 设置窗口实体（保存历史回写用）。
/// - `cx`: 应用上下文。
pub fn open_image_for_edit_in_app(main: Entity<RivixApp>, cx: &mut rgpui::App) {
    // 原生打开对话框：只选单个图片文件。
    let receiver = cx.prompt_for_paths(rgpui::PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some(tr("image.pick_prompt").into()),
    });
    cx.spawn(async move |cx| {
        let Ok(Ok(Some(paths))) = receiver.await else {
            return;
        };
        let Some(path) = paths.into_iter().next() else {
            return;
        };
        let Some(preview) = preview_from_path(&path) else {
            // 非图片：回写设置窗口状态，提示用户。
            cx.update(|cx| {
                cx.update_entity(&main, |state, cx| {
                    state.set_status(&tr("image.invalid"), cx);
                });
            });
            return;
        };
        cx.update(|cx| {
            open_viewer_in_app(main.clone(), cx, preview);
        });
    })
    .detach();
}

/// 从 `App` 上下文直接按路径打开预览窗口（异步选图任务内部用）。
fn open_viewer_in_app(main: Entity<RivixApp>, cx: &mut rgpui::App, preview: PreviewImage) {
    // 固定窗口布局与 `open_viewer` 一致：窗口跟随显示器，图片居中适配。
    let (disp_w, disp_h) = cx
        .primary_display()
        .map(|d| {
            let s = d.bounds().size;
            (s.width.as_f32(), s.height.as_f32())
        })
        .unwrap_or((1920.0, 1080.0));
    let (area_w, area_h, fit) = viewer_layout(disp_w, disp_h, preview.w, preview.h);
    cap_log(&format!(
        "预览窗口打开（本地图片）：img={}x{} area={}x{} fit={fit:.2}",
        preview.w, preview.h, area_w as u32, area_h as u32
    ));
    let bounds = Bounds::centered(None, size(px(area_w), px(area_h)), cx);
    let opened = cx.open_window(
        viewer_window_options(bounds, &preview),
        move |window, cx| {
            let view = cx.new(|cx| {
                ImageViewer::new(
                    window,
                    cx,
                    main.clone(),
                    preview.clone(),
                    fit,
                    area_w,
                    area_h,
                )
            });
            let handle = view.read(cx).focus_handle.clone();
            window.focus(&handle, cx);
            // 本地图片入口同样拦截系统 X，避免未保存标注被静默丢弃。
            guard_viewer_close(window, cx, view.clone());
            view
        },
    );
    if let Err(error) = opened {
        cap_log(&format!("预览窗口打开失败：{error:?}"));
    }
}

/// Ctrl+拖动文字时的临时状态。
struct DragText {
    /// 被拖动的 shapes 索引。
    idx: usize,
    /// 鼠标落点相对文字 pos 的偏移（图片像素）。
    offset: (f32, f32),
}

/// 选择工具拖动标注时的临时状态。
struct DragShape {
    /// 被拖动的 shapes 索引。
    idx: usize,
    /// 上一次鼠标所在的图片像素坐标。
    last: (f32, f32),
}

/// 图片预览视图。
struct ImageViewer {
    /// 主视图实体（回写状态 / 最近文件）。
    main: Entity<RivixApp>,
    /// 展示的图片。
    preview: PreviewImage,
    /// 预览用底图像素（马赛克实时预览采样用）。
    base_image: Option<Arc<RgbaImage>>,
    /// 当前缩放。
    zoom: f32,
    /// 适应窗口的缩放（"适应"按钮恢复用）。
    fit_zoom: f32,
    /// 图片区逻辑宽（窗口固定后不变，平移钳制用）。
    area_w: f32,
    /// 图片区逻辑高。
    area_h: f32,
    /// 平移偏移（图片中心相对区域中心的位移，逻辑像素）。
    offset: (f32, f32),
    /// 平移中上一次的鼠标位置（窗口坐标），None 表示未按下。
    pan_last: Option<(f32, f32)>,
    /// 空格是否按住：绘制中移动草稿，否则左键拖动平移画布。
    space_held: bool,
    /// 当前标注工具。
    tool: AnnoTool,
    /// 已提交的标注（图片像素坐标）。
    shapes: Vec<Shape>,
    /// 重做栈（撤销的标注暂存于此）。
    redo_stack: Vec<Shape>,
    /// 拖拽中的图形（抬起鼠标才提交）。
    draft: Option<Shape>,
    /// 图形类工具的起点（图片像素），画笔工具不用。
    anchor: Option<(f32, f32)>,
    /// 绘制中的终点和上次鼠标位置，松开空格继续绘制不跳回原位置。
    draft_end: Option<(f32, f32)>,
    draft_last: Option<(f32, f32)>,
    /// 取色后恢复原来的绘图工具。
    sample_return_tool: AnnoTool,
    /// 正在复制关闭时防止重复收尾。
    closing: bool,
    /// 当前选择及 Ctrl+T 变换，保存取消时的全部原始标注（支持多选整体变换）。
    selected: Option<usize>,
    transform_originals: Vec<(usize, Shape)>,
    /// 保留本轮变换框，短线和小文字缩放时控制点也连续跟手。
    transform_frame: Option<PixelRect>,
    transform_drag: Option<TransformDrag>,
    /// 当前标注颜色。
    color: AnnoColor,
    /// 内置圆角方形取色器状态（色相 0..360，饱和度 0..1，明度 0..1）。
    picker_hue: f32,
    picker_sat: f32,
    picker_val: f32,
    /// SV 取色区和横向色相条最近一次绘制的屏幕边界，用于拖拽取色换算。
    sv_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    hue_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    /// 是否正在拖拽 SV 取色区或色相条。
    sv_dragging: bool,
    hue_dragging: bool,
    /// 左侧工具箱是否收起为紧凑纯图标模式。
    dock_compact: bool,
    /// 选择工具在空白处左键框选的临时矩形（图片像素坐标：起点 x, y, 当前 x, y）。
    marquee: Option<(f32, f32, f32, f32)>,
    /// 多选时选中的全部标注索引（与 `selected` 协同工作）。
    selected_many: Vec<usize>,
    /// 吸管工具是否正在按住拖动实时取色。
    eyedropper_dragging: bool,
    /// 当前文字默认字号（图片像素）。
    text_size: f32,
    /// 字号滑杆与订阅（居中相对滑杆：左拖缩小、右拖放大，松手归零）。
    text_size_slider: Entity<SliderState>,
    #[allow(dead_code)]
    text_size_subscription: Subscription,
    /// 本次拖动字号滑杆起始时的基准字号。
    text_size_drag_base: Option<f32>,
    /// 手动输入字号文本框。
    text_size_input: Entity<InputState>,
    /// 当前文字默认字体家族（None 表示系统默认字体）。
    text_font: Option<String>,
    /// 字体选择下拉列表是否展开。
    font_dropdown_open: bool,
    /// 文本工具按住拖出自动换行框的临时坐标。
    text_box_draft: Option<(f32, f32, f32, f32)>,
    /// 当前线宽（图片像素）。
    width: f32,
    /// 线宽滑杆状态（操作排，拖动在 1–30px 内自由调节）。
    width_slider: Entity<SliderState>,
    /// 线宽滑杆订阅（持有以保持有效；字段本身无需读取）。
    #[allow(dead_code)]
    width_subscription: Subscription,
    /// 正在编辑的文字标注（点击后出现，回车/失焦提交）。
    text_editing: Option<TextEdit>,
    /// 当前文字底色：编辑中实时预览；未编辑时切换最近一条并作为新建默认（旧文字各自独立）。
    text_bg: bool,
    /// 当前底色颜色（取色器底色目标改这里；未手动指定时跟随字色自适应）。
    text_bg_color: AnnoColor,
    /// 底色是否被手动指定过（指定后换字色不再自动适配底色）。
    bg_color_custom: bool,
    /// 当前填充颜色（矩形 / 椭圆 / 多边形使用，None 表示无填充）。
    anno_fill: Option<AnnoColor>,
    /// 当前是否启用描边（矩形 / 椭圆 / 多边形使用）。
    anno_has_stroke: bool,
    /// 当前矩形圆角半径（图片像素，0 表示直角）。
    anno_rect_radius: f32,
    /// 矩形圆角滑杆与订阅。
    rect_radius_slider: Entity<SliderState>,
    #[allow(dead_code)]
    rect_radius_subscription: Subscription,
    /// 当前文字描边颜色（None 表示无描边，圆形描边位于文字后方）。
    text_stroke_color: Option<AnnoColor>,
    /// 取色器当前目标：`false` 改填充，`true` 改描边。
    color_target_stroke: bool,
    /// Ctrl+拖动文字的临时状态。
    drag_text: Option<DragText>,
    /// 选择工具拖动普通标注的临时状态。
    drag_shape: Option<DragShape>,
    /// 选择工具悬停时是否命中已有标注（用于切换鼠标形状）。
    hover_shape: bool,
    /// 仅在变换控制点切换时刷新系统光标。
    transform_hover: Option<TransformHandle>,
    /// 橡皮擦拖动中（抬起鼠标结束，期间移动持续删除）。
    erasing: bool,
    /// 缓存的系统字体（文字烧录用，懒加载）。
    font: Option<FontVec>,
    /// 字体是否已尝试加载（避免每次保存都读盘）。
    font_loaded: bool,
    /// 根视图焦点（键盘快捷键用）。
    focus_handle: FocusHandle,
    /// 左下角状态提示文本（复制 / 保存 / 取色等操作反馈，3 秒后渐隐消失）。
    status: SharedString,
    /// 最近一次状态提示的触发时间。
    status_at: Option<Instant>,
    /// 状态提示递增序号（防止旧定时器覆盖新提示）。
    status_seq: u64,
    /// 最近一次保存的文件路径（状态栏“打开文件 / 打开目录”用）。
    last_saved: Option<PathBuf>,
}

/// 八个缩放控制点、框体移动与顶部旋转手柄。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransformHandle {
    Move,
    Rotate,
    Nw,
    N,
    Ne,
    E,
    Se,
    S,
    Sw,
    W,
}
/// 每次拖拽保留起始几何（支持多选整组协同缩放平移，避免累计误差）。
struct TransformDrag {
    handle: TransformHandle,
    bounds: PixelRect,
    sources: Vec<(usize, Shape, PixelRect)>,
    start: (f32, f32),
}

/// 文字工具正在编辑的状态：点击图片后出现，回车、Esc 或失焦提交。
struct TextEdit {
    /// 落点（图片像素坐标）。
    img_pos: (f32, f32),
    /// 输入框实体。
    input: Entity<InputState>,
    /// 原图字号，重新编辑或变换后不重置为默认字号。
    size: f32,
    /// 文本框宽度（None 表示单行自由文本；Some(w) 表示自动换行框）。
    max_w: Option<f32>,
    /// 字体家族（None 表示默认系统字体）。
    font_family: Option<String>,
    /// 重新编辑时原 shapes 索引（None 表示新建）。
    edit_index: Option<usize>,
}

/// 缓存标注界面左上角品牌图标（内嵌 PNG 字节，只构造一次）。
fn viewer_logo() -> Arc<rgpui::Image> {
    static LOGO: OnceLock<Arc<rgpui::Image>> = OnceLock::new();
    LOGO.get_or_init(|| {
        Arc::new(rgpui::Image::from_bytes(
            rgpui::ImageFormat::Png,
            include_bytes!("../assets/logo-viewer.png").to_vec(),
        ))
    })
    .clone()
}

impl ImageViewer {
    /// 创建预览视图。
    fn new(
        _window: &mut Window,
        cx: &mut Context<Self>,
        main: Entity<RivixApp>,
        preview: PreviewImage,
        fit_zoom: f32,
        area_w: f32,
        area_h: f32,
    ) -> Self {
        let (init_h, init_s, init_v) = rgb_to_hsv(AnnoColor::RED);
        // 线宽滑杆：1–30px 自由调节，拖动实时同步当前线宽。
        let width_slider = cx.new(|_| {
            SliderState::new()
                .min(WIDTH_MIN)
                .max(WIDTH_MAX)
                .step(1.0)
                .default_value(DEFAULT_WIDTH)
        });
        let width_subscription = cx.subscribe(
            &width_slider,
            |this: &mut Self, _state, event: &SliderEvent, cx| match event {
                SliderEvent::Change(v) | SliderEvent::Release(v) => {
                    this.width = normalize_width(v.start());
                    this.apply_width_to_selected();
                    cx.notify();
                }
            },
        );
        // 矩形圆角滑杆：0–64px 自由调节，拖动实时同步当前圆角半径。
        let rect_radius_slider = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(64.0)
                .step(1.0)
                .default_value(0.0)
        });
        let rect_radius_subscription = cx.subscribe(
            &rect_radius_slider,
            |this: &mut Self, _state, event: &SliderEvent, cx| match event {
                SliderEvent::Change(v) | SliderEvent::Release(v) => {
                    this.anno_rect_radius = v.start().clamp(0.0, 64.0).round();
                    this.apply_rect_radius_to_selected();
                    cx.notify();
                }
            },
        );
        // 字号滑杆：居中相对调节（-24..+24），左拖缩小、右拖放大，松手自动回中。
        let text_size_slider = cx.new(|_| {
            SliderState::new()
                .min(-24.0)
                .max(24.0)
                .step(1.0)
                .default_value(0.0)
        });
        let text_size_input = cx.new(|cx| {
            let mut st = InputState::new(_window, cx);
            st.set_value((DEFAULT_TEXT_SIZE as u32).to_string(), _window, cx);
            st
        });
        let text_size_input_for_slider = text_size_input.clone();
        let text_size_subscription = cx.subscribe_in(
            &text_size_slider,
            _window,
            move |this: &mut Self, state, event: &SliderEvent, window, cx| match event {
                SliderEvent::Change(v) => {
                    let base = *this.text_size_drag_base.get_or_insert(this.text_size);
                    let next_sz = (base + v.start()).clamp(12.0, 120.0).round();
                    this.text_size = next_sz;
                    if let Some(editing) = &mut this.text_editing {
                        editing.size = next_sz;
                    }
                    for idx in this.selected_indices() {
                        if let Some(Shape::Text { size, .. }) = this.shapes.get_mut(idx) {
                            *size = next_sz;
                            this.redo_stack.clear();
                        }
                    }
                    let txt = (next_sz as u32).to_string();
                    text_size_input_for_slider.update(cx, |st, cx| {
                        if !st.text().eq(txt.as_str()) {
                            st.set_value(txt, window, cx);
                        }
                    });
                    cx.notify();
                }
                SliderEvent::Release(_) => {
                    this.text_size_drag_base = None;
                    state.update(cx, |s, cx| s.set_value(0.0, window, cx));
                    cx.notify();
                }
            },
        );
        cx.observe_in(
            &text_size_input,
            _window,
            move |this: &mut Self, state, _window, cx| {
                let raw_text = state.read(cx).text().to_string();
                let raw = raw_text.trim();
                if let Ok(val) = raw.parse::<f32>()
                    && (12.0..=120.0).contains(&val)
                {
                    let next_sz = val.round();
                    if (this.text_size - next_sz).abs() > 0.1 {
                        this.text_size = next_sz;
                        if let Some(editing) = &mut this.text_editing {
                            editing.size = next_sz;
                        }
                        for idx in this.selected_indices() {
                            if let Some(Shape::Text { size, .. }) = this.shapes.get_mut(idx) {
                                *size = next_sz;
                                this.redo_stack.clear();
                            }
                        }
                        cx.notify();
                    }
                }
            },
        )
        .detach();
        // 马赛克预览直接采样底图；读取失败时仍保留导出能力，只是不显示实时像素化预览。
        let base_image = image::open(&preview.path)
            .ok()
            .map(|img| Arc::new(img.into_rgba8()));
        Self {
            main,
            preview,
            base_image,
            zoom: fit_zoom,
            fit_zoom,
            area_w,
            area_h,
            offset: (0.0, 0.0),
            pan_last: None,
            space_held: false,
            // 默认矩形：打开即画（单击不成形，误触成本低），纯查看按 V 切回选择。
            tool: AnnoTool::Rect,
            shapes: Vec::new(),
            redo_stack: Vec::new(),
            draft: None,
            anchor: None,
            draft_end: None,
            draft_last: None,
            sample_return_tool: AnnoTool::Rect,
            closing: false,
            selected: None,
            transform_originals: Vec::new(),
            transform_frame: None,
            transform_drag: None,
            color: AnnoColor::RED,
            picker_hue: init_h,
            picker_sat: init_s,
            picker_val: init_v,
            sv_bounds: Arc::new(Mutex::new(None)),
            hue_bounds: Arc::new(Mutex::new(None)),
            sv_dragging: false,
            hue_dragging: false,
            dock_compact: false,
            marquee: None,
            selected_many: Vec::new(),
            eyedropper_dragging: false,
            text_size: DEFAULT_TEXT_SIZE,
            text_size_slider,
            text_size_subscription,
            text_size_drag_base: None,
            text_size_input,
            text_font: None,
            font_dropdown_open: false,
            text_box_draft: None,
            width: DEFAULT_WIDTH,
            width_slider,
            width_subscription,
            text_editing: None,
            text_bg: false,
            // 默认红字配自适应底（浅色字黑底、深色字白底），用户取过底色后不再自动跟随。
            text_bg_color: AnnoColor::RED.auto_bg(),
            bg_color_custom: false,
            anno_fill: None,
            anno_has_stroke: true,
            anno_rect_radius: 0.0,
            rect_radius_slider,
            rect_radius_subscription,
            text_stroke_color: None,
            color_target_stroke: true,
            drag_text: None,
            drag_shape: None,
            hover_shape: false,
            transform_hover: None,
            erasing: false,
            font: None,
            font_loaded: false,
            focus_handle: cx.focus_handle(),
            status: SharedString::default(),
            status_at: None,
            status_seq: 0,
            last_saved: None,
        }
    }

    /// 显示左下角状态提示胶囊，保持 2.4 秒后用 0.6 秒渐隐（共 3 秒消失）。
    fn set_viewer_status(&mut self, msg: &str, cx: &mut Context<Self>) {
        self.status = SharedString::from(msg.to_string());
        self.status_at = Some(Instant::now());
        self.status_seq = self.status_seq.wrapping_add(1);
        let seq = self.status_seq;
        cx.notify();

        let view = cx.entity();
        cx.spawn(async move |_, cx| {
            // 先静默等待 2.4 秒（保持 100% 不透明，不空转刷新）。
            cx.background_executor()
                .timer(Duration::from_millis(2400))
                .await;
            // 最后 600ms 每 40ms 刷新一次渐隐透明度。
            for _ in 0..15 {
                let mut active = false;
                view.update(cx, |this, cx| {
                    if this.status_seq == seq {
                        active = true;
                        cx.notify();
                    }
                });
                if !active {
                    return;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(40))
                    .await;
            }
            view.update(cx, |this, cx| {
                if this.status_seq == seq {
                    this.status_at = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Ctrl+T 或工具栏「变换」按钮：提交当前草稿后对选中标注（支持多选整体）开启自由变换。
    fn begin_transform(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.transform_originals.is_empty() {
            self.end_transform(false);
            cx.notify();
            return;
        }
        if self.text_editing.is_some() {
            self.commit_text(window, cx);
        }
        self.on_anno_up(None, cx);
        let mut indices = self.selected_indices();
        if indices.is_empty()
            && let Some(last) = self.shapes.len().checked_sub(1)
        {
            indices.push(last);
            self.selected = Some(last);
            self.selected_many = vec![last];
            self.sync_inspector_from_selected(window, cx);
        }
        if indices.is_empty() {
            self.set_viewer_status(&tr("viewer.transform_empty"), cx);
            return;
        }
        self.transform_originals = indices
            .iter()
            .filter_map(|&i| self.shapes.get(i).cloned().map(|s| (i, s)))
            .collect();
        self.transform_frame = None;
        self.transform_frame = self.transform_group_bounds(window);
        self.tool = AnnoTool::Select;
        self.set_viewer_status(&tr("viewer.transform_hint"), cx);
    }

    /// Enter 确认、Esc 恢复进入变换前的数据；切换工具时保留当前结果。
    fn end_transform(&mut self, cancel: bool) {
        if !self.transform_originals.is_empty() {
            self.status = tr("viewer.ready").into();
        }
        self.transform_drag = None;
        self.transform_frame = None;
        self.transform_hover = None;
        let originals = std::mem::take(&mut self.transform_originals);
        if cancel {
            for (index, original) in originals {
                if let Some(shape) = self.shapes.get_mut(index) {
                    *shape = original;
                }
            }
        }
    }

    /// 计算单个标注在变换模式下的外接矩形。
    /// 使用当前窗口字体系统精确计算标注的外包矩形（段落文本按实际折行计算高度）。
    fn shape_ui_bbox(&self, shape: &Shape, window: &Window) -> PixelRect {
        match shape {
            Shape::Text {
                pos,
                content,
                size,
                max_w,
                font_family,
                ..
            } => {
                let family = font_family.as_deref().unwrap_or(TEXT_FONT_FAMILY);
                if let Some(box_w) = max_w {
                    let lines = wrap_lines_in_ui(window, content, *size, family, *box_w);
                    PixelRect::new(
                        pos.0,
                        pos.1,
                        *box_w,
                        (lines.len().max(1) as f32) * size * TEXT_LINE_HEIGHT,
                    )
                } else {
                    let lines: Vec<&str> = content.split('\n').collect();
                    let w = lines
                        .iter()
                        .map(|line| measure_line_width(window, line, *size, family))
                        .fold(0.0f32, f32::max);
                    PixelRect::new(
                        pos.0,
                        pos.1,
                        w.max(*size * 0.5),
                        (lines.len().max(1) as f32) * size * TEXT_LINE_HEIGHT,
                    )
                }
            }
            Shape::Rect { rect, .. } | Shape::Ellipse { rect, .. } | Shape::Mosaic { rect } => {
                *rect
            }
            _ => shape.bbox(),
        }
    }

    fn shape_transform_bounds(&self, index: usize, window: &Window) -> Option<PixelRect> {
        let shape = self.shapes.get(index)?;
        let mut bounds = self.shape_ui_bbox(shape, window);
        let minimum = 16.0 / self.zoom.max(MIN_ZOOM);
        if bounds.w < minimum {
            bounds.x -= (minimum - bounds.w) / 2.0;
            bounds.w = minimum;
        }
        if bounds.h < minimum {
            bounds.y -= (minimum - bounds.h) / 2.0;
            bounds.h = minimum;
        }
        Some(bounds)
    }

    /// 计算当前变换组（单选或多选整体）的联合控制框。
    fn transform_group_bounds(&self, window: &Window) -> Option<PixelRect> {
        if let Some(frame) = self.transform_frame {
            return Some(frame);
        }
        let mut min_x = f32::INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut max_y = f32::NEG_INFINITY;
        let mut any = false;
        for &(idx, _) in &self.transform_originals {
            if let Some(b) = self.shape_transform_bounds(idx, window) {
                min_x = min_x.min(b.x);
                min_y = min_y.min(b.y);
                max_x = max_x.max(b.x + b.w);
                max_y = max_y.max(b.y + b.h);
                any = true;
            }
        }
        any.then(|| {
            PixelRect::new(
                min_x,
                min_y,
                (max_x - min_x).max(1.0),
                (max_y - min_y).max(1.0),
            )
        })
    }

    /// 完成当前编辑，将原图与全部标注烧录后置顶钉在屏幕上并关闭编辑窗口。
    fn pin_and_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing {
            return;
        }
        self.end_transform(false);
        if self.text_editing.is_some() {
            self.commit_text(window, cx);
        }
        self.on_anno_up(None, cx);
        let Some(burned) = self.burn_shapes() else {
            self.set_viewer_status(&tr("viewer.copy_fail"), cx);
            return;
        };
        let pin_image = crate::pin::PinImage::new(burned);
        if let Err(err) = crate::pin::open(pin_image, None, 1.0, cx) {
            self.set_viewer_status(&err, cx);
            return;
        }
        self.closing = true;
        window.remove_window();
    }

    /// 点 X/Alt+F4 时先隐藏窗口，后台合成和复制；失败时恢复窗口保留全部标注。
    fn copy_and_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing {
            return;
        }
        if self.text_editing.is_some() {
            self.commit_text(window, cx);
        }
        self.on_anno_up(None, cx);
        self.end_transform(false);
        self.closing = true;
        let base = self.base_image.clone();
        let path = self.preview.path.clone();
        let shapes = self.shapes.clone();
        let view = cx.entity();
        let handle = window.window_handle();
        window.hide_window();
        self.main.update(cx, |_, cx| {
            cx.spawn(async move |weak, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        let (png, file) = annotation_png(base, &path, &shapes)?;
                        let copied = copy_image_and_file_to_clipboard(&png, &file);
                        Ok::<_, String>((png, copied))
                    })
                    .await;
                let _ = weak.update(cx, |state, cx| {
                    let copied = match result {
                        Ok((png, Ok(()))) => {
                            drop(png);
                            Ok(())
                        }
                        Ok((png, Err(error))) => {
                            // Windows 关闭必须确认原生复制成功；失败保留编辑结果，不能静默关窗。
                            #[cfg(target_os = "windows")]
                            {
                                drop(png);
                                Err(error)
                            }
                            #[cfg(not(target_os = "windows"))]
                            {
                                let _ = error;
                                copy_png_to_clipboard(cx, &png)
                            }
                        }
                        Err(error) => Err(error),
                    };
                    let message = match &copied {
                        Ok(()) => {
                            crate::feedback::completed();
                            tr("viewer.copy_ok")
                        }
                        Err(error) => tr_fmt("viewer.copy_fail", &[("error", error)]),
                    };
                    state.set_status(&message, cx);
                    view.update(cx, |this, cx| {
                        this.closing = false;
                        this.set_viewer_status(&message, cx);
                    });
                    let _ = cx.update_window(handle, |_, window, _| {
                        if copied.is_ok() {
                            window.remove_window();
                        } else {
                            window.activate_window();
                        }
                    });
                    if copied.is_err() {
                        crate::toast::show(cx, message);
                    }
                });
            })
            .detach();
        });
    }

    /// 当前工具/选中项的颜色能力：`(是否显示颜色卡, 是否支持填充, 是否支持描边)`。
    fn active_color_mode_flags(&self) -> (bool, bool, bool) {
        let indices = self.selected_indices();
        if !indices.is_empty() {
            let mut any_non_mosaic = false;
            let mut any_fill = false;
            for idx in indices {
                match self.shapes.get(idx) {
                    Some(Shape::Mosaic { .. }) | None => {}
                    Some(
                        Shape::Rect { .. }
                        | Shape::Ellipse { .. }
                        | Shape::Polygon { .. }
                        | Shape::Text { .. },
                    ) => {
                        any_non_mosaic = true;
                        any_fill = true;
                    }
                    Some(
                        Shape::Pen { .. }
                        | Shape::Highlighter { .. }
                        | Shape::Line { .. }
                        | Shape::Arrow { .. },
                    ) => {
                        any_non_mosaic = true;
                    }
                }
            }
            return (any_non_mosaic, any_fill, any_non_mosaic);
        }
        if self.text_editing.is_some() || self.tool == AnnoTool::Text {
            return (true, true, true);
        }
        match self.tool {
            AnnoTool::Mosaic => (false, false, false),
            AnnoTool::Line | AnnoTool::Arrow | AnnoTool::Pen | AnnoTool::Highlighter => {
                (true, false, true)
            }
            AnnoTool::Rect
            | AnnoTool::Ellipse
            | AnnoTool::Text
            | AnnoTool::Select
            | AnnoTool::Eraser
            | AnnoTool::Eyedropper => (true, true, true),
        }
    }

    /// 当前是否处于“文字上下文”（文字工具激活、正在输入文字、或选中项含文字且无图形）。
    fn is_text_color_context(&self) -> bool {
        if self.text_editing.is_some() || self.tool == AnnoTool::Text {
            return true;
        }
        let indices = self.selected_indices();
        !indices.is_empty()
            && indices
                .iter()
                .all(|&i| matches!(self.shapes.get(i), Some(Shape::Text { .. })))
    }

    /// 实际生效的取色目标（`false` = 填充，`true` = 描边）；无填充的工具强制返回 `true`。
    fn effective_target_stroke(&self) -> bool {
        let (_, sup_fill, sup_stroke) = self.active_color_mode_flags();
        if !sup_fill && sup_stroke {
            true
        } else if sup_fill && !sup_stroke {
            false
        } else {
            self.color_target_stroke
        }
    }

    /// 当前激活目标的颜色值（用于色板高亮与标题小色块）。
    fn active_target_color(&self) -> Option<AnnoColor> {
        let is_stroke = self.effective_target_stroke();
        if self.is_text_color_context() {
            if is_stroke {
                self.text_stroke_color
            } else {
                Some(self.color)
            }
        } else if is_stroke {
            if self.anno_has_stroke {
                Some(self.color)
            } else {
                None
            }
        } else {
            self.anno_fill
        }
    }

    /// 颜色块（右侧属性栏快捷色板）。
    fn color_swatch(&self, cx: &mut Context<Self>, color: AnnoColor) -> impl IntoElement {
        let view = cx.entity();
        let selected = self.active_target_color() == Some(color);
        let vt = ViewerTheme::current();
        div()
            .id(format!("viewer-color-{}-{}-{}", color.0, color.1, color.2))
            .w(px(22.0))
            .h(px(22.0))
            .rounded_full()
            .cursor_pointer()
            .bg(color.to_rgpui())
            .border_2()
            .border_color(if selected {
                vt.text_primary
            } else {
                vt.panel_border
            })
            .on_click(move |_, window, cx| {
                view.update(cx, |this, cx| {
                    this.handle(ViewerAction::Color(color), window, cx)
                });
            })
    }

    /// 应用标注颜色（根据当前是“填充”还是“描边”目标，分别作用于图形与文字）。
    fn apply_anno_color(&mut self, anno: AnnoColor) {
        let is_stroke = self.effective_target_stroke();
        if is_stroke {
            if self.is_text_color_context() {
                self.text_stroke_color = Some(anno);
            } else {
                self.color = anno;
                self.anno_has_stroke = true;
            }
        } else if self.is_text_color_context() {
            self.color = anno;
            if !self.bg_color_custom {
                self.text_bg_color = anno.auto_bg();
            }
        } else {
            self.anno_fill = Some(anno);
        }
        self.sync_hsv_from_color(anno);
        self.apply_color_to_selected(is_stroke);
    }

    /// 根据颜色反算更新取色器 HSV 坐标（纯灰/黑/白保留当前色相，避免跳变）。
    fn sync_hsv_from_color(&mut self, anno: AnnoColor) {
        let (h, s, v) = rgb_to_hsv(anno);
        if s > 0.001 {
            self.picker_hue = h;
        }
        self.picker_sat = s;
        self.picker_val = v;
    }

    /// 根据当前取色器 HSV 更新填充或描边色（不反向覆盖正在拖动的色相）。
    fn apply_picker_hsv(&mut self, cx: &mut Context<Self>) {
        let anno = hsv_to_anno(self.picker_hue, self.picker_sat, self.picker_val);
        let is_stroke = self.effective_target_stroke();
        if is_stroke {
            if self.is_text_color_context() {
                self.text_stroke_color = Some(anno);
            } else {
                self.color = anno;
                self.anno_has_stroke = true;
            }
        } else if self.is_text_color_context() {
            self.color = anno;
            if !self.bg_color_custom {
                self.text_bg_color = anno.auto_bg();
            }
        } else {
            self.anno_fill = Some(anno);
        }
        self.apply_color_to_selected(is_stroke);
        cx.notify();
    }

    /// 选择工具选中已有元素时，将该元素的颜色、线宽与圆角状态同步到右侧属性栏。
    fn sync_inspector_from_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(idx) = self.selected else {
            return;
        };
        let Some(shape) = self.shapes.get(idx).cloned() else {
            return;
        };
        match shape {
            Shape::Pen { color, width, .. }
            | Shape::Highlighter { color, width, .. }
            | Shape::Line { color, width, .. }
            | Shape::Arrow { color, width, .. } => {
                self.color = color;
                self.anno_has_stroke = true;
                self.color_target_stroke = true;
                self.width = normalize_width(width);
                self.sync_hsv_from_color(color);
                let w = self.width;
                self.width_slider
                    .update(cx, |state, cx| state.set_value(w, window, cx));
            }
            Shape::Rect {
                color,
                width,
                fill,
                has_stroke,
                radius,
                ..
            } => {
                self.color = color;
                self.anno_fill = fill;
                self.anno_has_stroke = has_stroke;
                self.anno_rect_radius = radius;
                self.width = normalize_width(width);
                if !has_stroke && fill.is_some() {
                    self.color_target_stroke = false;
                }
                if let Some(c) = self.active_target_color() {
                    self.sync_hsv_from_color(c);
                }
                let w = self.width;
                self.width_slider
                    .update(cx, |state, cx| state.set_value(w, window, cx));
                self.rect_radius_slider
                    .update(cx, |state, cx| state.set_value(radius, window, cx));
            }
            Shape::Ellipse {
                color,
                width,
                fill,
                has_stroke,
                ..
            }
            | Shape::Polygon {
                color,
                width,
                fill,
                has_stroke,
                ..
            } => {
                self.color = color;
                self.anno_fill = fill;
                self.anno_has_stroke = has_stroke;
                self.width = normalize_width(width);
                if !has_stroke && fill.is_some() {
                    self.color_target_stroke = false;
                }
                if let Some(c) = self.active_target_color() {
                    self.sync_hsv_from_color(c);
                }
                let w = self.width;
                self.width_slider
                    .update(cx, |state, cx| state.set_value(w, window, cx));
            }
            Shape::Text {
                color,
                size,
                bg,
                bg_color,
                font_family,
                stroke_color,
                stroke_width,
                ..
            } => {
                self.color = color;
                self.text_size = size;
                self.text_bg = bg;
                self.text_bg_color = bg_color;
                self.text_font = font_family;
                self.text_stroke_color = stroke_color;
                self.width = normalize_width(stroke_width);
                self.bg_color_custom = true;
                self.color_target_stroke = false;
                self.width_slider
                    .update(cx, |s, cx| s.set_value(self.width, window, cx));
                let sz_txt = (size.round() as u32).to_string();
                self.text_size_input.update(cx, |s, cx| {
                    if !s.text().eq(sz_txt.as_str()) {
                        s.set_value(sz_txt, window, cx);
                    }
                });
                self.sync_hsv_from_color(color);
            }
            Shape::Mosaic { .. } => {}
        }
    }

    /// 收集当前选中的全部有效索引（兼容单选 `selected` 与框选多选 `selected_many`）。
    fn selected_indices(&self) -> Vec<usize> {
        let len = self.shapes.len();
        let mut out: Vec<usize> = self
            .selected_many
            .iter()
            .copied()
            .filter(|&i| i < len)
            .collect();
        if let Some(i) = self.selected
            && i < len
            && !out.contains(&i)
        {
            out.push(i);
        }
        out
    }

    /// 清空单选与多选状态。
    fn clear_selection(&mut self) {
        self.selected = None;
        self.selected_many.clear();
    }

    /// 全选画布上所有已标注元素（Ctrl+A）。
    fn select_all_shapes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.shapes.is_empty() {
            self.clear_selection();
            cx.notify();
            return;
        }
        // 若此前处于局部变换模式，先结束旧变换再全选，避免变换快照只覆盖部分元素。
        if !self.transform_originals.is_empty() {
            self.end_transform(false);
        }
        self.tool = AnnoTool::Select;
        self.marquee = None;
        self.selected_many = (0..self.shapes.len()).collect();
        self.selected = self.selected_many.last().copied();
        self.sync_inspector_from_selected(window, cx);
        self.set_viewer_status(&format!("已全选 {} 个元素", self.selected_many.len()), cx);
        cx.notify();
    }

    /// 若当前有选中已有元素（支持多选），则将填充或描边实时写回全部选中元素。
    fn apply_color_to_selected(&mut self, is_stroke: bool) -> bool {
        let indices = self.selected_indices();
        if indices.is_empty() {
            return false;
        }
        let next_color = self.color;
        let next_fill = self.anno_fill;
        let next_text_stroke = self.text_stroke_color;
        let auto_bg = next_color.auto_bg();
        let custom_bg = self.bg_color_custom;
        let mut changed = false;
        for idx in indices {
            let Some(shape) = self.shapes.get_mut(idx) else {
                continue;
            };
            if is_stroke {
                match shape {
                    Shape::Pen { color, .. }
                    | Shape::Highlighter { color, .. }
                    | Shape::Line { color, .. }
                    | Shape::Arrow { color, .. } => {
                        *color = next_color;
                        changed = true;
                    }
                    Shape::Polygon {
                        color, has_stroke, ..
                    }
                    | Shape::Rect {
                        color, has_stroke, ..
                    }
                    | Shape::Ellipse {
                        color, has_stroke, ..
                    } => {
                        *color = next_color;
                        *has_stroke = true;
                        changed = true;
                    }
                    Shape::Text { stroke_color, .. } => {
                        *stroke_color = next_text_stroke;
                        changed = true;
                    }
                    Shape::Mosaic { .. } => {}
                }
            } else {
                match shape {
                    Shape::Polygon { fill, .. }
                    | Shape::Rect { fill, .. }
                    | Shape::Ellipse { fill, .. } => {
                        *fill = next_fill;
                        changed = true;
                    }
                    Shape::Text {
                        color, bg_color, ..
                    } => {
                        *color = next_color;
                        if !custom_bg {
                            *bg_color = auto_bg;
                        }
                        changed = true;
                    }
                    Shape::Pen { .. }
                    | Shape::Highlighter { .. }
                    | Shape::Line { .. }
                    | Shape::Arrow { .. }
                    | Shape::Mosaic { .. } => {}
                }
            }
        }
        if changed {
            self.redo_stack.clear();
        }
        changed
    }

    /// 若当前有选中已有图形（支持多选），则将当前线宽实时写回全部选中图形（含文字描边粗细）。
    fn apply_width_to_selected(&mut self) {
        let indices = self.selected_indices();
        let width_val = self.width;
        for idx in indices {
            let Some(shape) = self.shapes.get_mut(idx) else {
                continue;
            };
            match shape {
                Shape::Pen { width, .. }
                | Shape::Highlighter { width, .. }
                | Shape::Polygon { width, .. }
                | Shape::Rect { width, .. }
                | Shape::Ellipse { width, .. }
                | Shape::Line { width, .. }
                | Shape::Arrow { width, .. } => {
                    *width = width_val;
                }
                Shape::Text { stroke_width, .. } => {
                    *stroke_width = width_val;
                }
                Shape::Mosaic { .. } => {}
            }
        }
    }

    /// 若当前有选中矩形（支持多选），则将当前圆角半径实时写回全部选中矩形。
    fn apply_rect_radius_to_selected(&mut self) {
        let indices = self.selected_indices();
        let radius_val = self.anno_rect_radius;
        for idx in indices {
            if let Some(Shape::Rect { radius, .. }) = self.shapes.get_mut(idx) {
                *radius = radius_val;
                self.redo_stack.clear();
            }
        }
    }

    /// Shift+X 互换填充与描边（仅对矩形、椭圆与多边形有效）。
    fn swap_fill_stroke(&mut self, cx: &mut Context<Self>) {
        let indices = self.selected_indices();
        let mut swapped_any = false;
        if !indices.is_empty() {
            for idx in &indices {
                if let Some(
                    Shape::Rect {
                        color,
                        fill,
                        has_stroke,
                        ..
                    }
                    | Shape::Ellipse {
                        color,
                        fill,
                        has_stroke,
                        ..
                    }
                    | Shape::Polygon {
                        color,
                        fill,
                        has_stroke,
                        ..
                    },
                ) = self.shapes.get_mut(*idx)
                {
                    let old_fill = *fill;
                    let old_stroke = if *has_stroke { Some(*color) } else { None };
                    *fill = old_stroke;
                    if let Some(c) = old_fill {
                        *color = c;
                        *has_stroke = true;
                    } else {
                        *has_stroke = false;
                    }
                    swapped_any = true;
                }
            }
            if !swapped_any {
                return;
            }
            self.redo_stack.clear();
        } else if !matches!(self.tool, AnnoTool::Rect | AnnoTool::Ellipse) {
            return;
        }
        let old_fill = self.anno_fill;
        let old_stroke = if self.anno_has_stroke {
            Some(self.color)
        } else {
            None
        };
        self.anno_fill = old_stroke;
        if let Some(c) = old_fill {
            self.color = c;
            self.anno_has_stroke = true;
        } else {
            self.anno_has_stroke = false;
        }
        self.color_target_stroke = !self.color_target_stroke;
        if let Some(c) = self.active_target_color() {
            self.sync_hsv_from_color(c);
        }
        self.set_viewer_status("已互换填充与描边", cx);
        cx.notify();
    }

    /// 清除当前目标的可选颜色（填充或描边，或文字描边）。
    fn clear_optional_color(&mut self, cx: &mut Context<Self>) {
        let is_stroke = self.effective_target_stroke();
        let indices = self.selected_indices();
        if self.is_text_color_context() && is_stroke {
            self.text_stroke_color = None;
            for idx in indices {
                if let Some(Shape::Text { stroke_color, .. }) = self.shapes.get_mut(idx) {
                    *stroke_color = None;
                    self.redo_stack.clear();
                }
            }
            cx.notify();
            return;
        }
        if !is_stroke {
            self.anno_fill = None;
            for idx in indices {
                if let Some(
                    Shape::Rect { fill, .. }
                    | Shape::Ellipse { fill, .. }
                    | Shape::Polygon { fill, .. },
                ) = self.shapes.get_mut(idx)
                {
                    *fill = None;
                    self.redo_stack.clear();
                }
            }
            cx.notify();
        } else {
            self.anno_has_stroke = false;
            for idx in indices {
                if let Some(
                    Shape::Rect { has_stroke, .. }
                    | Shape::Ellipse { has_stroke, .. }
                    | Shape::Polygon { has_stroke, .. },
                ) = self.shapes.get_mut(idx)
                {
                    *has_stroke = false;
                    self.redo_stack.clear();
                }
            }
            cx.notify();
        }
    }

    /// 多选对齐：按选中元素的整体外包盒执行顶 / 左 / 底 / 右 / 水平居中 / 垂直居中对齐。
    fn align_selected(&mut self, mode: AlignMode, cx: &mut Context<Self>) {
        let indices = self.selected_indices();
        if indices.len() < 2 {
            return;
        }
        let mut min_x = f32::MAX;
        let mut min_y = f32::MAX;
        let mut max_x = f32::MIN;
        let mut max_y = f32::MIN;
        let boxes: Vec<(usize, PixelRect)> = indices
            .iter()
            .filter_map(|&idx| {
                let s = self.shapes.get(idx)?;
                let b = shape_align_bbox(s);
                min_x = min_x.min(b.x);
                min_y = min_y.min(b.y);
                max_x = max_x.max(b.x + b.w);
                max_y = max_y.max(b.y + b.h);
                Some((idx, b))
            })
            .collect();
        if boxes.len() < 2 {
            return;
        }
        let union_cx = (min_x + max_x) * 0.5;
        let union_cy = (min_y + max_y) * 0.5;
        for (idx, b) in boxes {
            let (dx, dy) = match mode {
                AlignMode::Top => (0.0, min_y - b.y),
                AlignMode::Left => (min_x - b.x, 0.0),
                AlignMode::Bottom => (0.0, max_y - (b.y + b.h)),
                AlignMode::Right => (max_x - (b.x + b.w), 0.0),
                AlignMode::CenterH => (union_cx - (b.x + b.w * 0.5), 0.0),
                AlignMode::CenterV => (0.0, union_cy - (b.y + b.h * 0.5)),
            };
            if (dx.abs() > 1e-3 || dy.abs() > 1e-3)
                && let Some(shape) = self.shapes.get_mut(idx)
            {
                translate_shape_by(shape, dx, dy);
            }
        }
        self.redo_stack.clear();
        let label = match mode {
            AlignMode::Top => "已顶对齐",
            AlignMode::Left => "已左对齐",
            AlignMode::Bottom => "已底对齐",
            AlignMode::Right => "已右对齐",
            AlignMode::CenterH => "已水平居中",
            AlignMode::CenterV => "已垂直居中",
        };
        self.set_viewer_status(label, cx);
        cx.notify();
    }

    /// 工具条动作分发（按钮与快捷键共用）。
    fn handle(&mut self, action: ViewerAction, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing {
            return;
        }
        match action {
            ViewerAction::ZoomIn => self.zoom_by(ZOOM_STEP, cx),
            ViewerAction::ZoomOut => self.zoom_by(1.0 / ZOOM_STEP, cx),
            ViewerAction::Fit => {
                self.zoom = self.fit_zoom;
                self.offset = (0.0, 0.0);
                cx.notify();
            }
            ViewerAction::Copy => {
                self.end_transform(false);
                if self.text_editing.is_some() {
                    self.commit_text(window, cx);
                }
                self.on_anno_up(None, cx);
                self.copy_image(cx);
            }
            ViewerAction::Save => {
                self.end_transform(false);
                if self.text_editing.is_some() {
                    self.commit_text(window, cx);
                }
                self.on_anno_up(None, cx);
                // 工具条保存只管落盘，返回值（成功与否）由状态栏反馈。
                let _ = self.save_image(cx);
            }
            ViewerAction::Tool(tool) => {
                // 切换工具时丢弃未提交的草稿，提交文字编辑，避免残留。
                if self.text_editing.is_some() {
                    self.commit_text(window, cx);
                }
                if tool == AnnoTool::Eyedropper && self.tool != AnnoTool::Eyedropper {
                    self.sample_return_tool = self.tool;
                }
                self.end_transform(false);
                self.clear_selection();
                self.tool = tool;
                // 直线 / 箭头 / 画笔 / 荧光笔没有填充，默认切到描边；文字默认切到填充。
                match tool {
                    AnnoTool::Line | AnnoTool::Arrow | AnnoTool::Pen | AnnoTool::Highlighter => {
                        self.color_target_stroke = true;
                    }
                    AnnoTool::Text => {
                        self.color_target_stroke = false;
                    }
                    _ => {}
                }
                if let Some(c) = self.active_target_color() {
                    self.sync_hsv_from_color(c);
                }
                self.draft = None;
                self.anchor = None;
                self.draft_end = None;
                self.draft_last = None;
                self.drag_text = None;
                self.drag_shape = None;
                self.erasing = false;
                self.hover_shape = false;
                cx.notify();
            }
            ViewerAction::Undo => self.undo(cx),
            ViewerAction::Redo => self.redo(cx),
            ViewerAction::Color(color) => {
                self.apply_anno_color(color);
                cx.notify();
            }
            ViewerAction::WidthNudge(delta) => self.nudge_width(delta, window, cx),
            ViewerAction::TextBg => {
                // 按条独立，但保证每次点击都可见生效：
                // 有选中文字时直接修改所选文字；否则编辑中预览或修改最近一条。
                self.text_bg = !self.text_bg;
                let indices = self.selected_indices();
                let mut updated_selected = false;
                for idx in indices {
                    if let Some(Shape::Text { bg, .. }) = self.shapes.get_mut(idx) {
                        *bg = self.text_bg;
                        updated_selected = true;
                    }
                }
                if updated_selected {
                    self.redo_stack.clear();
                } else if self.text_editing.is_none()
                    && let Some(idx) = latest_text_index(&self.shapes)
                    && let Shape::Text { bg, .. } = &mut self.shapes[idx]
                {
                    *bg = self.text_bg;
                    self.redo_stack.clear();
                    let msg = tr("viewer.text_bg_latest");
                    self.set_viewer_status(&msg, cx);
                } else if self.text_editing.is_none() {
                    let msg = tr("viewer.text_bg_default");
                    self.set_viewer_status(&msg, cx);
                }
                cx.notify();
            }
            ViewerAction::TextSizeNudge(delta) => {
                let next_sz = (self.text_size + delta).clamp(12.0, 120.0).round();
                self.text_size = next_sz;
                if let Some(editing) = &mut self.text_editing {
                    editing.size = next_sz;
                }
                for idx in self.selected_indices() {
                    if let Some(Shape::Text { size, .. }) = self.shapes.get_mut(idx) {
                        *size = next_sz;
                        self.redo_stack.clear();
                    }
                }
                let txt = (next_sz as u32).to_string();
                self.text_size_input.update(cx, |s, cx| {
                    if !s.text().eq(txt.as_str()) {
                        s.set_value(txt, window, cx);
                    }
                });
                cx.notify();
            }
            ViewerAction::TextFont(family) => {
                self.text_font = family.clone();
                self.font_dropdown_open = false;
                if let Some(editing) = &mut self.text_editing {
                    editing.font_family = family.clone();
                }
                for idx in self.selected_indices() {
                    if let Some(Shape::Text { font_family, .. }) = self.shapes.get_mut(idx) {
                        *font_family = family.clone();
                        self.redo_stack.clear();
                    }
                }
                cx.notify();
            }
            ViewerAction::ToggleFontDropdown => {
                self.font_dropdown_open = !self.font_dropdown_open;
                cx.notify();
            }
            ViewerAction::PickBg(is_stroke) => {
                self.color_target_stroke = is_stroke;
                if let Some(target) = self.active_target_color() {
                    self.sync_hsv_from_color(target);
                }
                cx.notify();
            }
            ViewerAction::SwapFillStroke => self.swap_fill_stroke(cx),
            ViewerAction::ClearOptionalColor => self.clear_optional_color(cx),
            ViewerAction::RectRadiusNudge(delta) => {
                self.anno_rect_radius = (self.anno_rect_radius + delta).clamp(0.0, 64.0).round();
                self.apply_rect_radius_to_selected();
                let r = self.anno_rect_radius;
                self.rect_radius_slider
                    .update(cx, |s, cx| s.set_value(r, window, cx));
                cx.notify();
            }
            ViewerAction::Align(mode) => self.align_selected(mode, cx),
            ViewerAction::ToggleDock => {
                self.dock_compact = !self.dock_compact;
                cx.notify();
            }
            ViewerAction::ToggleTheme => {
                let next_dark = !crate::app::is_dark_mode();
                crate::config::update(|cfg| cfg.dark_mode = Some(next_dark));
                crate::app::apply_theme(cx);
                self.main.update(cx, |_, cx| cx.notify());
                cx.notify();
            }
            ViewerAction::OpenSavedFile => {
                // 用系统默认程序打开最近保存的文件。
                if let Some(path) = self.last_saved.clone() {
                    cx.open_with_system(&path);
                }
            }
            ViewerAction::OpenSavedDir => {
                // 打开所在目录：文件管理器选中文件需平台命令，这里打开目录即可。
                if let Some(path) = self.last_saved.clone()
                    && let Some(dir) = path.parent()
                {
                    cx.open_with_system(dir);
                }
            }
            ViewerAction::Transform => self.begin_transform(window, cx),
            ViewerAction::Pin => self.pin_and_close(window, cx),
            ViewerAction::Close => self.copy_and_close(window, cx),
        }
    }

    /// 撤销一个标注（进重做栈）。
    fn undo(&mut self, cx: &mut Context<Self>) {
        self.end_transform(false);
        self.clear_selection();
        if let Some(shape) = self.shapes.pop() {
            self.redo_stack.push(shape);
            cx.notify();
        }
    }

    /// 重做一个标注（从重做栈取回）。
    fn redo(&mut self, cx: &mut Context<Self>) {
        self.end_transform(false);
        self.clear_selection();
        if let Some(shape) = self.redo_stack.pop() {
            self.shapes.push(shape);
            cx.notify();
        }
    }

    /// 橡皮擦删除落点命中的标注（一次删光重叠的，删后清空重做栈）。
    fn erase_at(&mut self, p: (f32, f32), cx: &mut Context<Self>) {
        self.end_transform(false);
        self.clear_selection();
        let mut removed = false;
        while let Some(idx) = erase_hit(&self.shapes, p, ERASER_RADIUS) {
            self.shapes.remove(idx);
            removed = true;
        }
        if removed {
            self.redo_stack.clear();
            cx.notify();
        }
    }

    /// 窗口坐标转图片像素。
    ///
    /// 鼠标 `position` 是窗口坐标；图片区域从工具栏下方开始，
    /// 图片在区域内居中 + 偏移。直接用布局参数换算。
    fn to_image(&self, pos: rgpui::Point<Pixels>) -> Option<(f32, f32)> {
        if self.zoom <= 0.0 {
            return None;
        }
        let dw = self.preview.w as f32 * self.zoom;
        let dh = self.preview.h as f32 * self.zoom;
        let left = (self.area_w - dw) / 2.0 + self.offset.0;
        let top = (self.area_h - dh) / 2.0 + self.offset.1;
        // 全幅画布直接从窗口原点起算。
        let img_left = left;
        let img_top = top;
        Some((
            (pos.x.as_f32() - img_left) / self.zoom,
            (pos.y.as_f32() - img_top) / self.zoom,
        ))
    }

    /// 点是否落在图片内（含边界）。
    fn in_image(&self, p: (f32, f32)) -> bool {
        p.0 >= 0.0 && p.1 >= 0.0 && p.0 <= self.preview.w as f32 && p.1 <= self.preview.h as f32
    }

    /// 当前缩放下图片是否可平移。
    fn can_pan(&self) -> bool {
        self.preview.w as f32 * self.zoom > self.area_w
            || self.preview.h as f32 * self.zoom > self.area_h
    }

    /// 切换到文字工具并进入指定文字元素的重新编辑模式。
    fn begin_edit_text_index(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if idx >= self.shapes.len() || !matches!(self.shapes.get(idx), Some(Shape::Text { .. })) {
            return false;
        }
        let old = self.shapes.remove(idx);
        self.tool = AnnoTool::Text;
        self.clear_selection();
        self.drag_shape = None;
        self.marquee = None;
        if let Shape::Text {
            pos,
            content,
            color,
            bg,
            bg_color,
            size,
            max_w,
            font_family,
            stroke_color,
            stroke_width,
        } = old
        {
            self.color = color;
            self.text_size = size;
            self.text_font = font_family.clone();
            self.text_stroke_color = stroke_color;
            self.width = normalize_width(stroke_width);
            self.text_bg = bg;
            self.text_bg_color = bg_color;
            self.bg_color_custom = true;
            self.color_target_stroke = false;
            self.width_slider
                .update(cx, |s, cx| s.set_value(self.width, window, cx));
            let sz_txt = (size.round() as u32).to_string();
            self.text_size_input.update(cx, |s, cx| {
                if !s.text().eq(sz_txt.as_str()) {
                    s.set_value(sz_txt, window, cx);
                }
            });
            self.sync_hsv_from_color(color);
            let input = cx.new(|cx| {
                // 无论点选还是拖框文字，均开启多行输入；仅拖框文字启用自动软折行。
                InputState::new(window, cx)
                    .auto_grow(1, 64)
                    .soft_wrap(max_w.is_some())
            });
            input.update(cx, |state, cx| {
                state.set_value(content, window, cx);
            });
            cx.observe(&input, |_, _, cx| cx.notify()).detach();
            self.text_editing = Some(TextEdit {
                img_pos: pos,
                size,
                max_w,
                font_family,
                input,
                edit_index: Some(idx),
            });
            self.focus_text_next_frame(window, cx);
            cx.notify();
            return true;
        }
        false
    }

    /// 查找最顶层命中指定图片像素点的文字标注索引（从后往前找，后画的在上层）。
    fn hit_test_text(&self, p: (f32, f32)) -> Option<usize> {
        self.shapes
            .iter()
            .enumerate()
            .rev()
            .filter_map(|(i, shape)| {
                if matches!(shape, Shape::Text { .. }) {
                    let bbox = shape.bbox();
                    if bbox.contains(p) { Some(i) } else { None }
                } else {
                    None
                }
            })
            .next()
    }

    /// 查找最顶层命中指定图片像素点的标注索引（从后往前找，后画的在上层）。
    fn hit_test_shape(&self, p: (f32, f32)) -> Option<usize> {
        self.shapes
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, shape)| shape.bbox().contains(p).then_some(i))
    }

    /// 左键按下：空格平移优先，选择工具优先移动标注，空白处平移；其他工具开始绘制。
    fn on_anno_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // 空格按住时左键拖拽一律平移，与当前工具无关（默认矩形下放大看图用）。
        if !self.transform_originals.is_empty()
            && let Some(point) = self.to_image(ev.position)
        {
            if let Some(bounds) = self.transform_group_bounds(window)
                && let Some(handle) = transform_hit(bounds, point, self.zoom)
            {
                let sources: Vec<(usize, Shape, PixelRect)> = self
                    .transform_originals
                    .iter()
                    .filter_map(|&(idx, _)| {
                        let shape = self.shapes.get(idx)?.clone();
                        let rect = self.shape_transform_bounds(idx, window)?;
                        Some((idx, shape, rect))
                    })
                    .collect();
                self.transform_drag = Some(TransformDrag {
                    handle,
                    bounds,
                    sources,
                    start: point,
                });
                return;
            }
            self.end_transform(false);
        }
        if self.space_held {
            self.on_pan_down(ev, cx);
            return;
        }
        if self.closing {
            return;
        }
        if self.tool == AnnoTool::Eyedropper {
            self.eyedropper_dragging = true;
            if let Some(p) = self.to_image(ev.position)
                && let Some(color) = self
                    .base_image
                    .as_ref()
                    .and_then(|image| sample_image_color(image, p))
            {
                self.apply_anno_color(color);
                self.set_viewer_status(&format!("{} {}", tr("viewer.sampled"), color.to_hex()), cx);
            }
            return;
        }
        if self.tool == AnnoTool::Select {
            // 双击文字元素：自动切换到文字工具并进入重新编辑模式（允许命中画布外元素）。
            if ev.click_count >= 2
                && let Some(p) = self.to_image(ev.position)
                && let Some(idx) = self.hit_test_text(p)
            {
                if self.text_editing.is_some() {
                    self.commit_text(window, cx);
                }
                if self.begin_edit_text_index(idx, window, cx) {
                    return;
                }
            }
            // 如果点中已有标注（含已移出画布外的元素），支持 Shift 切换多选或整体拖动已选组；点空白处框选。
            if let Some(p) = self.to_image(ev.position)
                && let Some(idx) = self.hit_test_shape(p)
            {
                if self.text_editing.is_some() {
                    self.commit_text(window, cx);
                }
                if ev.modifiers.shift {
                    if let Some(pos) = self.selected_many.iter().position(|&i| i == idx) {
                        self.selected_many.remove(pos);
                        self.selected = self.selected_many.last().copied();
                    } else {
                        if let Some(cur) = self.selected
                            && !self.selected_many.contains(&cur)
                        {
                            self.selected_many.push(cur);
                        }
                        self.selected_many.push(idx);
                        self.selected = Some(idx);
                    }
                    self.sync_inspector_from_selected(window, cx);
                    cx.notify();
                    return;
                }
                if !self.selected_many.contains(&idx) {
                    self.selected_many = vec![idx];
                }
                if ev.modifiers.alt {
                    // Alt+拖动：复制当前选中的元素组，并直接拖动新副本。
                    let mut group = self.selected_indices();
                    if group.is_empty() {
                        group.push(idx);
                    }
                    let mut new_indices = Vec::with_capacity(group.len());
                    for src_i in group {
                        if let Some(cloned) = self.shapes.get(src_i).cloned() {
                            self.shapes.push(cloned);
                            new_indices.push(self.shapes.len() - 1);
                        }
                    }
                    if let Some(&last_new) = new_indices.last() {
                        self.selected_many = new_indices;
                        self.selected = Some(last_new);
                        self.sync_inspector_from_selected(window, cx);
                        self.drag_shape = Some(DragShape {
                            idx: last_new,
                            last: p,
                        });
                        self.redo_stack.clear();
                        cx.notify();
                        return;
                    }
                }
                self.selected = Some(idx);
                self.sync_inspector_from_selected(window, cx);
                self.drag_shape = Some(DragShape { idx, last: p });
                self.redo_stack.clear();
                cx.notify();
                return;
            }
            self.clear_selection();
            if let Some(p) = self.to_image(ev.position) {
                self.marquee = Some((p.0, p.1, p.0, p.1));
            }
            cx.notify();
            return;
        }
        // 橡皮擦：按下即删落点标注，拖动持续删除（抬起结束）。
        if self.tool == AnnoTool::Eraser {
            if let Some(p) = self.to_image(ev.position).filter(|p| self.in_image(*p)) {
                self.erasing = true;
                self.erase_at(p, cx);
                return;
            }
            self.on_pan_down(ev, cx);
            return;
        }
        let img_pt = self.to_image(ev.position);
        let Some(p) = img_pt else {
            return;
        };
        if !self.in_image(p) {
            return;
        }
        // 如果正在编辑文字，先提交当前文字（点击别处 = 确认）。
        if self.text_editing.is_some() {
            self.commit_text(window, cx);
        }

        // ── 文字工具：Ctrl+拖动 / 点击已有文字重新编辑 / 新建文字 ──
        if self.tool == AnnoTool::Text {
            let has_ctrl = ev.modifiers.control;
            if has_ctrl {
                // Ctrl+点击：尝试拖动已有文字。
                if let Some(idx) = self.hit_test_text(p)
                    && let Shape::Text { pos, .. } = &self.shapes[idx]
                {
                    let offset = (p.0 - pos.0, p.1 - pos.1);
                    self.drag_text = Some(DragText { idx, offset });
                    cx.notify();
                    return;
                }
            }
            // 点击已有文字 → 进入重新编辑模式。
            if let Some(idx) = self.hit_test_text(p)
                && self.begin_edit_text_index(idx, window, cx)
            {
                return;
            }
            // 空白处按下：暂存拉框起点（松手时判断是单击单行输入还是拉出自动换行文本框）。
            self.clear_selection();
            self.text_box_draft = Some((p.0, p.1, p.0, p.1));
            cx.notify();
            return;
        }

        // ── 图形类工具：开始绘制草稿 ──
        self.clear_selection();
        let (color, width) = (self.color, self.width);
        self.anchor = Some(p);
        self.draft_end = Some(p);
        self.draft_last = Some(p);
        self.draft = Some(match self.tool {
            AnnoTool::Rect => Shape::Rect {
                rect: PixelRect::from_points(p, p),
                color,
                width,
                fill: self.anno_fill,
                has_stroke: self.anno_has_stroke,
                radius: self.anno_rect_radius,
            },
            AnnoTool::Ellipse => Shape::Ellipse {
                rect: PixelRect::from_points(p, p),
                color,
                width,
                fill: self.anno_fill,
                has_stroke: self.anno_has_stroke,
            },
            AnnoTool::Line => Shape::Line {
                from: p,
                to: p,
                color,
                width,
            },
            AnnoTool::Arrow => Shape::Arrow {
                from: p,
                to: p,
                color,
                width,
            },
            AnnoTool::Pen => Shape::Pen {
                pts: vec![p],
                color,
                width,
            },
            AnnoTool::Highlighter => Shape::Highlighter {
                pts: vec![p],
                color,
                width,
            },
            AnnoTool::Mosaic => Shape::Mosaic {
                rect: PixelRect::from_points(p, p),
            },
            AnnoTool::Select | AnnoTool::Text | AnnoTool::Eraser | AnnoTool::Eyedropper => return,
        });
        cx.notify();
    }

    /// 鼠标移动：移动标注、Ctrl+拖动文字、平移（选择 / 空格 / 中键）、其他工具更新草稿。
    fn on_anno_move(&mut self, ev: &MouseMoveEvent, window: &Window, cx: &mut Context<Self>) {
        if !self.transform_originals.is_empty() && self.transform_drag.is_none() {
            let hover = self
                .transform_group_bounds(window)
                .zip(self.to_image(ev.position))
                .and_then(|(bounds, point)| transform_hit(bounds, point, self.zoom));
            if self.transform_hover != hover {
                self.transform_hover = hover;
                cx.notify();
            }
        }
        if let Some(drag) = &self.transform_drag {
            if let Some(point) = self.to_image(ev.position) {
                if drag.handle == TransformHandle::Rotate {
                    let cx_box = drag.bounds.x + drag.bounds.w / 2.0;
                    let cy_box = drag.bounds.y + drag.bounds.h / 2.0;
                    let a0 = (drag.start.1 - cy_box).atan2(drag.start.0 - cx_box);
                    let a1 = (point.1 - cy_box).atan2(point.0 - cx_box);
                    let mut delta = a1 - a0;
                    if ev.modifiers.shift {
                        let step = std::f32::consts::PI / 12.0; // Shift 约束每 15° 一档（含 45°/90°/135°）
                        delta = (delta / step).round() * step;
                    }
                    for &(idx, ref src_shape, _) in &drag.sources {
                        if let Some(dst) = self.shapes.get_mut(idx) {
                            *dst = rotate_shape(src_shape, (cx_box, cy_box), delta);
                        }
                    }
                    self.transform_frame = None;
                    let deg = delta.to_degrees().round() as i32;
                    self.set_viewer_status(&format!("旋转：{deg}°"), cx);
                    self.redo_stack.clear();
                    cx.notify();
                    return;
                }
                let has_point_text = drag
                    .sources
                    .iter()
                    .any(|(_, s, _)| matches!(s, Shape::Text { max_w: None, .. }));
                let keep_aspect = ev.modifiers.shift
                    || (drag.sources.len() == 1
                        && matches!(drag.sources[0].1, Shape::Text { max_w: None, .. }));
                let target = resize_bounds(
                    drag.bounds,
                    (point.0 - drag.start.0, point.1 - drag.start.1),
                    drag.handle,
                    keep_aspect,
                );
                let sx = target.w / drag.bounds.w.max(1.0);
                let sy = target.h / drag.bounds.h.max(1.0);
                for &(idx, ref src_shape, src_rect) in &drag.sources {
                    let sub_target = PixelRect::new(
                        target.x + (src_rect.x - drag.bounds.x) * sx,
                        target.y + (src_rect.y - drag.bounds.y) * sy,
                        (src_rect.w * sx).max(1.0),
                        (src_rect.h * sy).max(1.0),
                    );
                    if let Some(dst) = self.shapes.get_mut(idx) {
                        *dst = transform_shape(src_shape, src_rect, sub_target, has_point_text);
                    }
                }
                self.transform_frame = Some(target);
                self.redo_stack.clear();
                cx.notify();
            }
            return;
        }
        // 吸管拖动中：实时取样原图像素色。
        if self.eyedropper_dragging {
            if let Some(p) = self.to_image(ev.position)
                && let Some(color) = self
                    .base_image
                    .as_ref()
                    .and_then(|image| sample_image_color(image, p))
            {
                self.apply_anno_color(color);
                self.set_viewer_status(&format!("{} {}", tr("viewer.sampled"), color.to_hex()), cx);
            }
            return;
        }
        // 橡皮擦拖动中：沿途删除命中的标注。
        if self.erasing {
            if let Some(p) = self.to_image(ev.position).filter(|p| self.in_image(*p)) {
                self.erase_at(p, cx);
            }
            return;
        }
        // 文本工具拖动中：更新自动换行文本框预览矩形。
        if let Some((sx, sy, _, _)) = self.text_box_draft {
            if let Some(p) = self.to_image(ev.position) {
                let p = (
                    p.0.clamp(0.0, self.preview.w as f32),
                    p.1.clamp(0.0, self.preview.h as f32),
                );
                self.text_box_draft = Some((sx, sy, p.0, p.1));
                cx.notify();
            }
            return;
        }
        // 选择工具框选拖动中：更新虚线选框终点。
        if let Some((sx, sy, _, _)) = self.marquee {
            if let Some(p) = self.to_image(ev.position) {
                self.marquee = Some((sx, sy, p.0, p.1));
                cx.notify();
            }
            return;
        }
        // 选择工具拖动标注：按图片像素增量自由平移（支持多选整组一起移出画布之外）。
        if self.drag_shape.is_some() {
            let Some(p) = self.to_image(ev.position) else {
                return;
            };
            let Some(drag) = &mut self.drag_shape else {
                return;
            };
            let delta = (p.0 - drag.last.0, p.1 - drag.last.1);
            let drag_idx = drag.idx;
            drag.last = p;
            if delta.0 != 0.0 || delta.1 != 0.0 {
                let mut indices = self.selected_indices();
                if !indices.contains(&drag_idx) {
                    indices = vec![drag_idx];
                }
                for i in indices {
                    if let Some(shape) = self.shapes.get_mut(i) {
                        move_shape(shape, delta);
                    }
                }
                cx.notify();
            }
            return;
        }
        // Ctrl+拖动文字：直接更新已有文字坐标（允许移出画布之外）。
        if let Some(drag) = &self.drag_text {
            let Some(p) = self.to_image(ev.position) else {
                return;
            };
            if let Some(Shape::Text { pos, .. }) = self.shapes.get_mut(drag.idx) {
                pos.0 = p.0 - drag.offset.0;
                pos.1 = p.1 - drag.offset.1;
            }
            cx.notify();
            return;
        }
        // 平移中（选择工具空白拖拽 / 空格临时平移 / 中键拖拽）：按位移累加偏移，与工具无关。
        if self.pan_last.is_some() {
            self.on_pan_move(ev, cx);
            return;
        }
        // 选择工具：未按下时仅更新悬停命中（含画布外元素），用于显示“可移动标注”的鼠标形状。
        if self.tool == AnnoTool::Select {
            let hover_shape = self
                .to_image(ev.position)
                .and_then(|p| self.hit_test_shape(p))
                .is_some();
            if self.hover_shape != hover_shape {
                self.hover_shape = hover_shape;
                cx.notify();
            }
            return;
        }
        let Some(mut p) = self.to_image(ev.position) else {
            return;
        };
        p.0 = p.0.clamp(0.0, self.preview.w as f32);
        p.1 = p.1.clamp(0.0, self.preview.h as f32);
        let Some(last) = self.draft_last.replace(p) else {
            return;
        };
        let delta = (p.0 - last.0, p.1 - last.1);
        if self.space_held
            && let Some(draft) = &mut self.draft
        {
            let moved =
                translate_draft(draft, delta, (self.preview.w as f32, self.preview.h as f32));
            if let Some(anchor) = &mut self.anchor {
                anchor.0 += moved.0;
                anchor.1 += moved.1;
            }
            if let Some(end) = &mut self.draft_end {
                end.0 += moved.0;
                end.1 += moved.1;
            }
            cx.notify();
            return;
        }
        let (Some(anchor), Some(end)) = (self.anchor, self.draft_end.as_mut()) else {
            return;
        };
        let raw_end = (
            (end.0 + delta.0).clamp(0.0, self.preview.w as f32),
            (end.1 + delta.1).clamp(0.0, self.preview.h as f32),
        );
        *end = raw_end;
        let p = if ev.modifiers.shift {
            constrain_draw_point(
                anchor,
                raw_end,
                self.tool,
                self.preview.w as f32,
                self.preview.h as f32,
            )
        } else {
            raw_end
        };
        match &mut self.draft {
            Some(Shape::Rect { rect, .. })
            | Some(Shape::Ellipse { rect, .. })
            | Some(Shape::Mosaic { rect }) => {
                *rect = PixelRect::from_points(anchor, p);
            }
            Some(Shape::Line { to, .. }) | Some(Shape::Arrow { to, .. }) => *to = p,
            Some(Shape::Pen { pts, .. }) | Some(Shape::Highlighter { pts, .. }) => {
                if ev.modifiers.shift {
                    // Shift 下按直线角度约束画笔 / 荧光笔，方便画水平/垂直/45° 直线高亮。
                    *pts = vec![anchor, p];
                } else if pts.last() != Some(&p) {
                    pts.push(p);
                }
            }
            _ => return,
        }
        cx.notify();
    }

    /// 左键抬起：结束移动 / 平移（选择 / 空格 / 中键），Ctrl+拖动文字结束，文本拉框或点击建框，其他工具提交草稿。
    fn on_anno_up(&mut self, window: Option<&mut Window>, cx: &mut Context<Self>) {
        if self.eyedropper_dragging {
            self.eyedropper_dragging = false;
            return;
        }
        if let Some((sx, sy, ex, ey)) = self.text_box_draft.take() {
            if let Some(w) = window {
                let rect = PixelRect::from_points((sx, sy), (ex, ey));
                if rect.w >= 24.0 || rect.h >= 16.0 {
                    let box_w = rect.w.max(60.0);
                    self.place_text((rect.x, rect.y), Some(box_w), w, cx);
                } else {
                    self.place_text((sx, sy), None, w, cx);
                }
            } else {
                cx.notify();
            }
            return;
        }
        if self.transform_drag.take().is_some() {
            cx.notify();
            return;
        }
        // 橡皮擦拖动结束。
        if self.erasing {
            self.erasing = false;
            cx.notify();
            return;
        }
        // 选择工具移动标注结束。
        if self.drag_shape.take().is_some() {
            cx.notify();
            return;
        }
        // Ctrl+拖动文字结束。
        if self.drag_text.take().is_some() {
            cx.notify();
            return;
        }
        // 平移结束（左键或中键发起的都在这里收尾，绘制草稿时 pan_last 为空，不影响提交）。
        if self.pan_last.is_some() {
            self.on_pan_up(cx);
            return;
        }
        // 选择工具框选结束：收集框内所有相交元素实现多选（单点空白保持取消选中）。
        if let Some((sx, sy, ex, ey)) = self.marquee.take() {
            let rect = PixelRect::from_points((sx, sy), (ex, ey));
            if rect.w > 3.0 || rect.h > 3.0 {
                let hits: Vec<usize> = self
                    .shapes
                    .iter()
                    .enumerate()
                    .filter_map(|(i, shape)| {
                        let b = shape.bbox();
                        let overlaps = b.x <= rect.x + rect.w
                            && b.x + b.w >= rect.x
                            && b.y <= rect.y + rect.h
                            && b.y + b.h >= rect.y;
                        overlaps.then_some(i)
                    })
                    .collect();
                self.selected_many = hits;
                self.selected = self.selected_many.last().copied();
                if let Some(idx) = self.selected
                    && let Some(shape) = self.shapes.get(idx).cloned()
                {
                    match shape {
                        Shape::Pen { color, width, .. }
                        | Shape::Highlighter { color, width, .. }
                        | Shape::Polygon { color, width, .. }
                        | Shape::Rect { color, width, .. }
                        | Shape::Ellipse { color, width, .. }
                        | Shape::Line { color, width, .. }
                        | Shape::Arrow { color, width, .. } => {
                            self.color = color;
                            self.width = normalize_width(width);
                            if self.effective_target_stroke() {
                                self.sync_hsv_from_color(color);
                            }
                        }
                        Shape::Text {
                            color,
                            bg,
                            bg_color,
                            stroke_color,
                            ..
                        } => {
                            self.color = color;
                            self.text_bg = bg;
                            self.text_bg_color = bg_color;
                            self.text_stroke_color = stroke_color;
                            self.bg_color_custom = true;
                            if let Some(target) = self.active_target_color() {
                                self.sync_hsv_from_color(target);
                            }
                        }
                        Shape::Mosaic { .. } => {}
                    }
                }
            }
            cx.notify();
            return;
        }
        if self.tool == AnnoTool::Select {
            return;
        }
        // 过小的图形视为误触，直接丢弃。
        if let Some(draft) = self.draft.take()
            && draft_valid(&draft)
        {
            self.shapes.push(draft);
            let idx = self.shapes.len() - 1;
            self.selected = Some(idx);
            self.selected_many = vec![idx];
            self.redo_stack.clear();
        }
        self.anchor = None;
        self.draft_end = None;
        self.draft_last = None;
        cx.notify();
    }

    /// 点击或拖框后进入文字编辑模式：在图片上创建 inline 输入框。
    fn place_text(
        &mut self,
        img_pos: (f32, f32),
        max_w: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| {
            // 无论点选还是拖框文字，均开启多行输入；仅拖框文字启用自动软折行。
            InputState::new(window, cx)
                .auto_grow(1, 64)
                .soft_wrap(max_w.is_some())
        });
        cx.observe(&input, |_, _, cx| cx.notify()).detach();
        self.text_editing = Some(TextEdit {
            img_pos,
            size: self.text_size,
            max_w,
            font_family: self.text_font.clone(),
            input,
            edit_index: None,
        });
        self.focus_text_next_frame(window, cx);
        cx.notify();
    }

    /// 新输入框先挂载再聚焦，避免创建它的鼠标事件把焦点退回画布。
    fn focus_text_next_frame(&self, window: &Window, cx: &Context<Self>) {
        let view = cx.entity().downgrade();
        window.on_next_frame(move |window, cx| {
            let _ = view.update(cx, |this, cx| {
                if let Some(editing) = &this.text_editing {
                    editing
                        .input
                        .update(cx, |state, cx| state.focus(window, cx));
                }
            });
        });
    }

    /// 提交正在编辑的文字：读取内容，放入 shapes。
    fn commit_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = self.text_editing.take() else {
            return;
        };
        let content = editing
            .input
            .read_with(cx, |state, _| state.text().to_string());
        let content = content.trim().to_string();
        if content.is_empty() {
            let fh = self.focus_handle.clone();
            window.focus(&fh, cx);
            cx.notify();
            return;
        }
        let shape = Shape::Text {
            pos: editing.img_pos,
            content,
            color: self.color,
            size: editing.size,
            // 提交时快照当前开关、字体与描边，新旧文字各自独立。
            bg: self.text_bg,
            bg_color: self.text_bg_color,
            max_w: editing.max_w,
            font_family: editing.font_family.clone(),
            stroke_color: self.text_stroke_color,
            stroke_width: self.width,
        };
        // 重新编辑时插回原位，否则追加到末尾。
        if let Some(idx) = editing.edit_index {
            let insert_at = idx.min(self.shapes.len());
            self.shapes.insert(insert_at, shape);
            self.selected = Some(insert_at);
            self.selected_many = vec![insert_at];
        } else {
            self.shapes.push(shape);
            let idx = self.shapes.len() - 1;
            self.selected = Some(idx);
            self.selected_many = vec![idx];
            self.redo_stack.clear();
        }
        let fh = self.focus_handle.clone();
        window.focus(&fh, cx);
        cx.notify();
    }

    /// 把底图与全部标注烧录为最终图片。
    fn burn_shapes(&mut self) -> Option<RgbaImage> {
        let base = self.base_image.clone().or_else(|| {
            image::open(&self.preview.path)
                .ok()
                .map(|image| Arc::new(image.to_rgba8()))
        })?;
        if !self.font_loaded
            && self
                .shapes
                .iter()
                .any(|shape| matches!(shape, Shape::Text { .. }))
        {
            self.font = load_system_font();
            self.font_loaded = true;
        }
        let full = PixelRect::new(0.0, 0.0, base.width() as f32, base.height() as f32);
        Some(render_export(&base, full, &self.shapes, &self.font))
    }

    /// 复制烧录图到系统剪贴板：图片与文件双格式，Word 粘贴为图片、文件夹粘贴为文件。
    fn copy_image(&mut self, cx: &mut Context<Self>) {
        let main = self.main.clone();
        // 先拿到 PNG 字节与对应的文件路径：无标注直接读底图文件，
        // 有标注则烧录后落到剪贴板缓存文件（HDROP 目标必须是一个真实存在的文件）。
        let (png, file_path) = if self.shapes.is_empty() {
            match std::fs::read(&self.preview.path) {
                Ok(bytes) => (bytes, self.preview.path.clone()),
                Err(error) => {
                    let msg = tr_fmt("viewer.copy_read_fail", &[("error", &error.to_string())]);
                    self.set_viewer_status(&msg, cx);
                    return;
                }
            }
        } else {
            let Some(img) = self.burn_shapes() else {
                self.set_viewer_status(&tr("viewer.export_fail"), cx);
                return;
            };
            let Some(bytes) = png_bytes(&img) else {
                self.set_viewer_status(&tr("viewer.export_fail"), cx);
                return;
            };
            let path = clipboard_png_path();
            if let Some(parent) = path.parent()
                && let Err(error) = std::fs::create_dir_all(parent)
            {
                let msg = tr_fmt("viewer.cache_dir_fail", &[("error", &error.to_string())]);
                self.set_viewer_status(&msg, cx);
                return;
            }
            if let Err(error) = std::fs::write(&path, &bytes) {
                let msg = tr_fmt("viewer.cache_write_fail", &[("error", &error.to_string())]);
                self.set_viewer_status(&msg, cx);
                return;
            }
            (bytes, path)
        };

        // 双格式写入：一次搞定图片粘贴（Word / 画图）与文件粘贴（资源管理器）。
        match copy_image_and_file_to_clipboard(&png, &file_path) {
            Ok(()) => {
                crate::feedback::completed();
                let msg = tr("viewer.copy_ok");
                self.set_viewer_status(&msg, cx);
                main.update(cx, |state, cx| state.set_status(&msg, cx));
            }
            Err(file_error) => {
                // 双格式失败时回退到纯图片复制，保证 Word 粘贴仍可用。
                match copy_png_to_clipboard(cx, &png) {
                    Ok(()) => {
                        crate::feedback::completed();
                        let msg = tr_fmt(
                            "viewer.copy_img_only",
                            &[("error", &file_error.to_string())],
                        );
                        self.set_viewer_status(&msg, cx);
                        main.update(cx, |state, cx| state.set_status(&msg, cx));
                    }
                    Err(error) => {
                        let msg = tr_fmt("viewer.copy_fail", &[("error", &error.to_string())]);
                        self.set_viewer_status(&msg, cx);
                        main.update(cx, |state, cx| {
                            state.set_status(&msg, cx);
                        });
                    }
                }
            }
        }
        cx.refresh_windows();
    }

    /// 保存烧录图到默认保存目录并记入最近文件（结果就近显示在预览底部状态栏）。
    ///
    /// # 返回
    fn save_image(&mut self, cx: &mut Context<Self>) -> bool {
        let main = self.main.clone();
        // 保存永远另存为带时间戳的新文件，不覆盖原图（截图自动保存的原图同样受保护）。
        // 目录被用户删掉时尝试重建，重建失败则本次保存会失败并提示（不崩溃）。
        let dir = load_save_dir();
        let _ = std::fs::create_dir_all(&dir);
        let dst = dir.join(format!(
            "rivix-{}.png",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ));
        let saved = if self.shapes.is_empty() {
            std::fs::copy(&self.preview.path, &dst).is_ok()
        } else {
            self.burn_shapes()
                .and_then(|img| image::DynamicImage::ImageRgba8(img).save(&dst).ok())
                .is_some()
        };
        if saved {
            crate::feedback::completed();
            // 状态栏就近显示保存位置，并提供打开文件 / 打开目录快捷入口。
            let name = dst
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&tr("viewer.image_fallback"))
                .to_string();
            self.last_saved = Some(dst.clone());
            // 保存成功：脏标记清零，关闭确认浮层也不再需要。
            // 状态栏显示文件名与所在目录名（自定义目录也能一眼看出存哪了）。
            let loc = dst
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("?");
            let status_msg = tr_fmt("viewer.saved", &[("name", &name), ("loc", loc)]);
            self.set_viewer_status(&status_msg, cx);
            let msg = tr_fmt("viewer.saved_full", &[("path", &dst.display().to_string())]);
            main.update(cx, |state, cx| {
                state.push_recent(dst, msg, cx);
            });
        } else {
            self.set_viewer_status(&tr("viewer.save_fail"), cx);
            main.update(cx, |state, cx| {
                state.set_status(&tr("viewer.save_fail"), cx);
            });
        }
        cx.refresh_windows();
        saved
    }

    /// 步进调节线宽并同步滑杆位置（− / + 按钮与 [ ] 快捷键共用）。
    fn nudge_width(&mut self, delta: f32, window: &mut Window, cx: &mut Context<Self>) {
        let width = normalize_width(self.width + delta);
        self.width = width;
        self.apply_width_to_selected();
        // 同步滑杆 thumb，避免按钮与滑杆显示不一致。
        self.width_slider.update(cx, |state, cx| {
            state.set_value(width, window, cx);
        });
        cx.notify();
    }

    /// 按系数缩放。
    fn zoom_by(&mut self, factor: f32, cx: &mut Context<Self>) {
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        self.clamp_offset();
        cx.notify();
    }

    /// 钳制平移：图比区域小时强制居中；图大时保证视口中心始终压在图上。
    fn clamp_offset(&mut self) {
        let dw = self.preview.w as f32 * self.zoom;
        let dh = self.preview.h as f32 * self.zoom;
        let (ox, oy) = self.offset;
        // 全幅底层画布支持在窗口范围内自由中键平移（至少保留 60px 在视口内，防止彻底拖丢）。
        let max_x = ((self.area_w + dw) / 2.0 - 60.0).max(0.0);
        let max_y = ((self.area_h + dh) / 2.0 - 60.0).max(0.0);
        self.offset = (ox.clamp(-max_x, max_x), oy.clamp(-max_y, max_y));
    }

    /// 左键按下：开始平移。
    fn on_pan_down(&mut self, ev: &MouseDownEvent, cx: &mut Context<Self>) {
        self.pan_last = Some((ev.position.x.as_f32(), ev.position.y.as_f32()));
        cx.notify();
    }

    /// 鼠标移动：平移中按位移累加偏移。
    fn on_pan_move(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some((lx, ly)) = self.pan_last else {
            return;
        };
        let (x, y) = (ev.position.x.as_f32(), ev.position.y.as_f32());
        let (ox, oy) = self.offset;
        self.offset = (ox + x - lx, oy + y - ly);
        self.pan_last = Some((x, y));
        self.clamp_offset();
        cx.notify();
    }

    /// 左键抬起：结束平移。
    fn on_pan_up(&mut self, cx: &mut Context<Self>) {
        self.pan_last = None;
        cx.notify();
    }
}

/// 找到最近一条文字标注的索引（底色开关未编辑时作用于它，保证每次点击可见生效）。
fn latest_text_index(shapes: &[Shape]) -> Option<usize> {
    shapes.iter().rposition(|s| matches!(s, Shape::Text { .. }))
}

/// 多个编辑窗口可同时后台关闭，序号避免同毫秒缓存文件互相覆盖。
static NEXT_CLIPBOARD_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 剪贴板缓存图片路径：保持文件存在，保证复制后资源管理器能异步读取。
fn clipboard_png_path() -> PathBuf {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    std::env::temp_dir()
        .join("rivix")
        .join("clipboard")
        .join(format!(
            "rivix-{}-{millis}-{}.png",
            std::process::id(),
            NEXT_CLIPBOARD_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
}

/// 光标最后绘制，始终盖在标注之上；直接复用输入布局，不新增计时器或像素采样。
fn paint_text_caret(input: &Entity<InputState>, window: &mut Window, cx: &rgpui::App) {
    let state = input.read(cx);
    if !state.is_focused(window) || !window.is_window_active() || !state.selected_range().is_empty()
    {
        return;
    }
    let cursor = state.cursor();
    if let Some(mut bounds) = state.range_to_bounds(&(cursor..cursor)) {
        bounds.origin += state.scroll_offset();
        let (outer, inner) = caret_layers(bounds, window.scale_factor());
        window.paint_quad(fill(outer, rgb(0xFFFFFF)));
        window.paint_quad(fill(inner, rgb(0x101010)));
    }
}

/// 双层文字光标使用输入组件的真实插入位置，不增加闪烁或采样定时器。
fn caret_layers(line: Bounds<Pixels>, scale: f32) -> (Bounds<Pixels>, Bounds<Pixels>) {
    let scale = scale.max(0.1);
    let dot = 1.0 / scale;
    let x = (line.origin.x.as_f32() * scale).round() / scale;
    let h = (line.size.height.as_f32() * 0.85).max(dot);
    let y = line.origin.y.as_f32() + (line.size.height.as_f32() - h) / 2.0;
    (
        Bounds::new(
            point(px(x - dot), px(y - dot)),
            size(px(dot * 3.0), px(h + dot * 2.0)),
        ),
        Bounds::new(point(px(x), px(y)), size(px(dot), px(h))),
    )
}

/// 控制点顺序与命中、绘制共用。
fn transform_handles(bounds: PixelRect) -> [(TransformHandle, (f32, f32)); 8] {
    let (x, y, w, h) = (bounds.x, bounds.y, bounds.w, bounds.h);
    [
        (TransformHandle::Nw, (x, y)),
        (TransformHandle::N, (x + w / 2.0, y)),
        (TransformHandle::Ne, (x + w, y)),
        (TransformHandle::E, (x + w, y + h / 2.0)),
        (TransformHandle::Se, (x + w, y + h)),
        (TransformHandle::S, (x + w / 2.0, y + h)),
        (TransformHandle::Sw, (x, y + h)),
        (TransformHandle::W, (x, y + h / 2.0)),
    ]
}

/// 为变换控制点选择对应方向的系统光标。
fn transform_cursor(handle: Option<TransformHandle>) -> CursorStyle {
    // 名称以 rgpui 的实际 Windows 映射为准，避免两组对角线光标颠倒。
    match handle {
        Some(TransformHandle::Nw | TransformHandle::Se) => CursorStyle::ResizeUpLeftDownRight,
        Some(TransformHandle::Ne | TransformHandle::Sw) => CursorStyle::ResizeUpRightDownLeft,
        Some(TransformHandle::N | TransformHandle::S) => CursorStyle::ResizeUpDown,
        Some(TransformHandle::E | TransformHandle::W) => CursorStyle::ResizeLeftRight,
        Some(TransformHandle::Move) => CursorStyle::PointingHand,
        Some(TransformHandle::Rotate) => CursorStyle::Crosshair,
        None => CursorStyle::Arrow,
    }
}

/// 优先命中八向缩放控制点与框内移动；鼠标位于四角外侧邻近区域时触发 Illustrator/Figma 式旋转。
fn transform_hit(bounds: PixelRect, p: (f32, f32), zoom: f32) -> Option<TransformHandle> {
    let z = zoom.max(MIN_ZOOM);
    let hit = transform_handles(bounds)
        .into_iter()
        .map(|(handle, q)| (handle, (p.0 - q.0).hypot(p.1 - q.1)))
        .min_by(|a, b| a.1.total_cmp(&b.1));
    if let Some((handle, distance)) = hit
        && distance <= 6.0 / z
    {
        return Some(handle);
    }
    if bounds.contains(p) {
        return Some(TransformHandle::Move);
    }
    let corners = [
        (bounds.x, bounds.y),
        (bounds.x + bounds.w, bounds.y),
        (bounds.x + bounds.w, bounds.y + bounds.h),
        (bounds.x, bounds.y + bounds.h),
    ];
    let near_corner = corners
        .into_iter()
        .any(|(cx, cy)| (p.0 - cx).hypot(p.1 - cy) <= 22.0 / z);
    near_corner.then_some(TransformHandle::Rotate)
}

/// 八向缩放保持对侧锚点，最小 2 个原图像素；文字和 Shift 拖动保持比例。
fn resize_bounds(
    from: PixelRect,
    delta: (f32, f32),
    handle: TransformHandle,
    uniform: bool,
) -> PixelRect {
    if handle == TransformHandle::Move || handle == TransformHandle::Rotate {
        return PixelRect::new(from.x + delta.0, from.y + delta.1, from.w, from.h);
    }
    let west = matches!(
        handle,
        TransformHandle::W | TransformHandle::Nw | TransformHandle::Sw
    );
    let east = matches!(
        handle,
        TransformHandle::E | TransformHandle::Ne | TransformHandle::Se
    );
    let north = matches!(
        handle,
        TransformHandle::N | TransformHandle::Nw | TransformHandle::Ne
    );
    let south = matches!(
        handle,
        TransformHandle::S | TransformHandle::Sw | TransformHandle::Se
    );
    let left = if west {
        (from.x + delta.0).min(from.x + from.w - 2.0)
    } else {
        from.x
    };
    let right = if east {
        (from.x + from.w + delta.0).max(from.x + 2.0)
    } else {
        from.x + from.w
    };
    let top = if north {
        (from.y + delta.1).min(from.y + from.h - 2.0)
    } else {
        from.y
    };
    let bottom = if south {
        (from.y + from.h + delta.1).max(from.y + 2.0)
    } else {
        from.y + from.h
    };
    let mut out = PixelRect::new(left, top, right - left, bottom - top);
    if uniform {
        let sx = out.w / from.w.max(1.0);
        let sy = out.h / from.h.max(1.0);
        let factor = if !(north || south) {
            sx
        } else if !(west || east) {
            sy
        } else if (sx - 1.0).abs() >= (sy - 1.0).abs() {
            sx
        } else {
            sy
        };
        out.w = from.w * factor;
        out.h = from.h * factor;
        out.x = if west {
            from.x + from.w - out.w
        } else if east {
            from.x
        } else {
            from.x + (from.w - out.w) / 2.0
        };
        out.y = if north {
            from.y + from.h - out.h
        } else if south {
            from.y
        } else {
            from.y + (from.h - out.h) / 2.0
        };
    }
    out
}

/// 只变更矢量坐标和字号，不栅格化图片；单行文本缩放字号，段落文本仅调整框宽（混合含单行文本时统一起缩字号）。
fn transform_shape(
    source: &Shape,
    from: PixelRect,
    to: PixelRect,
    scale_area_text_font: bool,
) -> Shape {
    let sx = to.w / from.w.max(1.0);
    let sy = to.h / from.h.max(1.0);
    let map = |p: (f32, f32)| (to.x + (p.0 - from.x) * sx, to.y + (p.1 - from.y) * sy);
    let rect = |r: PixelRect| {
        let p = map((r.x, r.y));
        PixelRect::new(p.0, p.1, r.w * sx, r.h * sy)
    };
    let mut shape = source.clone();
    match &mut shape {
        Shape::Rect { rect: r, width, .. } | Shape::Ellipse { rect: r, width, .. } => {
            *r = rect(*r);
            *width = (*width * (sx * sy).sqrt()).clamp(0.5, 128.0);
        }
        Shape::Mosaic { rect: r } => *r = rect(*r),
        Shape::Line {
            from: a,
            to: b,
            width,
            ..
        }
        | Shape::Arrow {
            from: a,
            to: b,
            width,
            ..
        } => {
            *a = map(*a);
            *b = map(*b);
            *width = (*width * (sx * sy).sqrt()).clamp(0.5, 128.0);
        }
        Shape::Pen { pts, width, .. }
        | Shape::Highlighter { pts, width, .. }
        | Shape::Polygon { pts, width, .. } => {
            for p in pts {
                *p = map(*p);
            }
            *width = (*width * (sx * sy).sqrt()).clamp(0.5, 128.0);
        }
        Shape::Text {
            pos, size, max_w, ..
        } => {
            *pos = map(*pos);
            if let Some(w) = max_w {
                *w = (*w * sx).max(20.0);
                if scale_area_text_font {
                    *size = (*size * sy).clamp(4.0, 512.0);
                }
            } else {
                *size = (*size * sy).clamp(4.0, 512.0);
            }
        }
    }
    shape
}

/// 蓝色细框与白色控制点只参与预览，不进入导出结果。
fn paint_transform(
    bounds: PixelRect,
    zoom: f32,
    origin: rgpui::Point<Pixels>,
    window: &mut Window,
) {
    let bounds = PixelRect::new(
        origin.x.as_f32() + bounds.x * zoom,
        origin.y.as_f32() + bounds.y * zoom,
        bounds.w * zoom,
        bounds.h * zoom,
    );
    let blue = rgb(0x0078D4);
    let dot = 1.0 / window.scale_factor();
    for (x, y, w, h) in [
        (bounds.x, bounds.y, bounds.w, dot),
        (bounds.x, bounds.y + bounds.h, bounds.w, dot),
        (bounds.x, bounds.y, dot, bounds.h),
        (bounds.x + bounds.w, bounds.y, dot, bounds.h),
    ] {
        window.paint_quad(fill(
            Bounds::new(point(px(x), px(y)), size(px(w), px(h))),
            blue,
        ));
    }
    for (_, p) in transform_handles(bounds) {
        window.paint_quad(fill(
            Bounds::new(point(px(p.0 - 4.0), px(p.1 - 4.0)), size(px(8.0), px(8.0))),
            blue,
        ));
        window.paint_quad(fill(
            Bounds::new(point(px(p.0 - 3.0), px(p.1 - 3.0)), size(px(6.0), px(6.0))),
            rgb(0xFFFFFF),
        ));
    }
}

/// 绘制时按住 `Shift` 的角度 / 比例约束：直线与箭头吸附 45° 倍数，矩形/椭圆/马赛克约束正方/正圆。
fn constrain_draw_point(
    anchor: (f32, f32),
    raw: (f32, f32),
    tool: AnnoTool,
    max_w: f32,
    max_h: f32,
) -> (f32, f32) {
    let dx = raw.0 - anchor.0;
    let dy = raw.1 - anchor.1;
    let (nx, ny) = match tool {
        AnnoTool::Rect | AnnoTool::Ellipse | AnnoTool::Mosaic => {
            let side = dx.abs().max(dy.abs());
            let sx = if dx >= 0.0 { 1.0 } else { -1.0 };
            let sy = if dy >= 0.0 { 1.0 } else { -1.0 };
            (anchor.0 + sx * side, anchor.1 + sy * side)
        }
        AnnoTool::Line | AnnoTool::Arrow | AnnoTool::Pen | AnnoTool::Highlighter => {
            let len = dx.hypot(dy);
            if len <= 1e-3 {
                return raw;
            }
            let step = std::f32::consts::FRAC_PI_4;
            let angle = (dy.atan2(dx) / step).round() * step;
            (anchor.0 + len * angle.cos(), anchor.1 + len * angle.sin())
        }
        _ => raw,
    };
    (nx.clamp(0.0, max_w), ny.clamp(0.0, max_h))
}

/// 将点绕 `center` 逆时针旋转 `angle` 弧度。
fn rotate_point(p: (f32, f32), center: (f32, f32), cos_a: f32, sin_a: f32) -> (f32, f32) {
    let dx = p.0 - center.0;
    let dy = p.1 - center.1;
    (
        center.0 + dx * cos_a - dy * sin_a,
        center.1 + dx * sin_a + dy * cos_a,
    )
}

/// 绕指定中心旋转单个标注；矩形与椭圆转为闭合 `Polygon` 以保留真实旋转几何。
fn rotate_shape(shape: &Shape, center: (f32, f32), angle: f32) -> Shape {
    if angle.abs() < 1e-4 {
        return shape.clone();
    }
    let (sin_a, cos_a) = angle.sin_cos();
    let rot = |p: (f32, f32)| rotate_point(p, center, cos_a, sin_a);
    match shape {
        Shape::Pen { pts, color, width } => Shape::Pen {
            pts: pts.iter().map(|&p| rot(p)).collect(),
            color: *color,
            width: *width,
        },
        Shape::Highlighter { pts, color, width } => Shape::Highlighter {
            pts: pts.iter().map(|&p| rot(p)).collect(),
            color: *color,
            width: *width,
        },
        Shape::Polygon {
            pts,
            color,
            width,
            fill,
            has_stroke,
        } => Shape::Polygon {
            pts: pts.iter().map(|&p| rot(p)).collect(),
            color: *color,
            width: *width,
            fill: *fill,
            has_stroke: *has_stroke,
        },
        Shape::Line {
            from,
            to,
            color,
            width,
        } => Shape::Line {
            from: rot(*from),
            to: rot(*to),
            color: *color,
            width: *width,
        },
        Shape::Arrow {
            from,
            to,
            color,
            width,
        } => Shape::Arrow {
            from: rot(*from),
            to: rot(*to),
            color: *color,
            width: *width,
        },
        Shape::Rect {
            rect,
            color,
            width,
            fill,
            has_stroke,
            ..
        } => {
            let corners = [
                (rect.x, rect.y),
                (rect.x + rect.w, rect.y),
                (rect.x + rect.w, rect.y + rect.h),
                (rect.x, rect.y + rect.h),
            ];
            Shape::Polygon {
                pts: corners.into_iter().map(rot).collect(),
                color: *color,
                width: *width,
                fill: *fill,
                has_stroke: *has_stroke,
            }
        }
        Shape::Ellipse {
            rect,
            color,
            width,
            fill,
            has_stroke,
        } => {
            let rx = rect.w / 2.0;
            let ry = rect.h / 2.0;
            let ecx = rect.x + rx;
            let ecy = rect.y + ry;
            let steps = 48usize;
            let pts = (0..steps)
                .map(|i| {
                    let t = (i as f32 / steps as f32) * std::f32::consts::TAU;
                    rot((ecx + rx * t.cos(), ecy + ry * t.sin()))
                })
                .collect();
            Shape::Polygon {
                pts,
                color: *color,
                width: *width,
                fill: *fill,
                has_stroke: *has_stroke,
            }
        }
        Shape::Mosaic { rect } => {
            let cx = rect.x + rect.w / 2.0;
            let cy = rect.y + rect.h / 2.0;
            let (ncx, ncy) = rot((cx, cy));
            Shape::Mosaic {
                rect: PixelRect::new(ncx - rect.w / 2.0, ncy - rect.h / 2.0, rect.w, rect.h),
            }
        }
        Shape::Text {
            pos,
            content,
            color,
            size,
            bg,
            bg_color,
            max_w,
            font_family,
            stroke_color,
            stroke_width,
        } => Shape::Text {
            pos: rot(*pos),
            content: content.clone(),
            color: *color,
            size: *size,
            bg: *bg,
            bg_color: *bg_color,
            max_w: *max_w,
            font_family: font_family.clone(),
            stroke_color: *stroke_color,
            stroke_width: *stroke_width,
        },
    }
}

/// 返回用于对齐计算的紧密外包矩形（不含线宽外扩）。
fn shape_align_bbox(shape: &Shape) -> PixelRect {
    match shape {
        Shape::Rect { rect, .. } | Shape::Ellipse { rect, .. } | Shape::Mosaic { rect } => *rect,
        Shape::Line { from, to, .. } | Shape::Arrow { from, to, .. } => {
            PixelRect::from_points(*from, *to)
        }
        Shape::Pen { pts, .. } | Shape::Highlighter { pts, .. } | Shape::Polygon { pts, .. } => {
            let mut min_x = f32::MAX;
            let mut min_y = f32::MAX;
            let mut max_x = f32::MIN;
            let mut max_y = f32::MIN;
            for &(x, y) in pts {
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
            if min_x <= max_x && min_y <= max_y {
                PixelRect::new(min_x, min_y, max_x - min_x, max_y - min_y)
            } else {
                PixelRect::new(0.0, 0.0, 0.0, 0.0)
            }
        }
        Shape::Text { .. } => shape.bbox(),
    }
}

/// 按给定增量平移单个标注（对齐与批量移动共用）。
fn translate_shape_by(shape: &mut Shape, dx: f32, dy: f32) {
    match shape {
        Shape::Rect { rect, .. } | Shape::Ellipse { rect, .. } | Shape::Mosaic { rect } => {
            rect.x += dx;
            rect.y += dy;
        }
        Shape::Line { from, to, .. } | Shape::Arrow { from, to, .. } => {
            from.0 += dx;
            from.1 += dy;
            to.0 += dx;
            to.1 += dy;
        }
        Shape::Pen { pts, .. } | Shape::Highlighter { pts, .. } | Shape::Polygon { pts, .. } => {
            for p in pts {
                p.0 += dx;
                p.1 += dy;
            }
        }
        Shape::Text { pos, .. } => {
            pos.0 += dx;
            pos.1 += dy;
        }
    }
}

/// 后台导出完整原图加标注，缓存文件供系统图片/文件两种粘贴方式使用。
fn annotation_png(
    base: Option<Arc<RgbaImage>>,
    path: &std::path::Path,
    shapes: &[Shape],
) -> Result<(Vec<u8>, PathBuf), String> {
    let base = match base {
        Some(image) => image,
        None => Arc::new(image::open(path).map_err(|e| e.to_string())?.to_rgba8()),
    };
    let needs_font = shapes
        .iter()
        .any(|shape| matches!(shape, Shape::Text { .. }));
    let font = if needs_font { load_system_font() } else { None };
    if needs_font && font.is_none() {
        return Err(tr("viewer.export_fail"));
    }
    let image = render_export(
        &base,
        PixelRect::new(0.0, 0.0, base.width() as f32, base.height() as f32),
        shapes,
        &font,
    );
    let png = png_bytes(&image).ok_or_else(|| tr("viewer.export_fail"))?;
    let file = clipboard_png_path();
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&file, &png).map_err(|e| e.to_string())?;
    Ok((png, file))
}

/// 仅采样截图像素，越界不改变当前绘制颜色。
fn sample_image_color(image: &RgbaImage, p: (f32, f32)) -> Option<AnnoColor> {
    if p.0 < 0.0 || p.1 < 0.0 || p.0 >= image.width() as f32 || p.1 >= image.height() as f32 {
        return None;
    }
    let pixel = image.get_pixel(p.0.floor() as u32, p.1.floor() as u32);
    Some(AnnoColor(pixel[0], pixel[1], pixel[2], 255))
}

/// 平移草稿并返回实际位移，供起终点同步，边缘钳制后放开空格也不跳变。
fn translate_draft(shape: &mut Shape, delta: (f32, f32), bounds: (f32, f32)) -> (f32, f32) {
    let before = shape.bbox();
    move_shape_clamped(shape, delta, bounds.0, bounds.1);
    let after = shape.bbox();
    (after.x - before.x, after.y - before.y)
}

/// 草稿有效性：过小的图形视为误触，不提交。
fn draft_valid(shape: &Shape) -> bool {
    match shape {
        Shape::Pen { pts, .. } | Shape::Highlighter { pts, .. } | Shape::Polygon { pts, .. } => {
            pts.len() >= 2 && pts.first() != pts.last()
        }
        Shape::Rect { rect, .. } | Shape::Ellipse { rect, .. } | Shape::Mosaic { rect } => {
            !rect.is_empty()
        }
        Shape::Line { from, to, .. } | Shape::Arrow { from, to, .. } => {
            (to.0 - from.0).hypot(to.1 - from.1) > 2.0
        }
        Shape::Text { .. } => true,
    }
}

/// 按图片像素增量移动标注。
fn move_shape(shape: &mut Shape, delta: (f32, f32)) {
    let (dx, dy) = delta;
    match shape {
        Shape::Pen { pts, .. } | Shape::Highlighter { pts, .. } | Shape::Polygon { pts, .. } => {
            for p in pts {
                p.0 += dx;
                p.1 += dy;
            }
        }
        Shape::Rect { rect, .. } | Shape::Ellipse { rect, .. } | Shape::Mosaic { rect } => {
            rect.x += dx;
            rect.y += dy;
        }
        Shape::Line { from, to, .. } | Shape::Arrow { from, to, .. } => {
            from.0 += dx;
            from.1 += dy;
            to.0 += dx;
            to.1 += dy;
        }
        Shape::Text { pos, .. } => {
            pos.0 += dx;
            pos.1 += dy;
        }
    }
}

/// 橡皮擦命中判定：返回最顶层被命中标注的索引（后画的在上层，先删它）。
///
/// - 画笔 / 荧光笔：点到笔迹折线的距离 ≤ `radius + 线宽/2` 即命中；
/// - 其余：落点在外包矩形内即命中。
fn erase_hit(shapes: &[Shape], p: (f32, f32), radius: f32) -> Option<usize> {
    shapes.iter().enumerate().rev().find_map(|(i, shape)| {
        let hit = match shape {
            Shape::Line {
                from, to, width, ..
            } => stroke_distance(&[*from, *to], p) <= radius + width / 2.0,
            Shape::Pen { pts, width, .. }
            | Shape::Highlighter { pts, width, .. }
            | Shape::Polygon { pts, width, .. } => stroke_distance(pts, p) <= radius + width / 2.0,
            _ => shape.bbox().contains(p),
        };
        hit.then_some(i)
    })
}

/// 点到折线的最短距离（单点退化为点距，空笔迹返回无穷大）。
fn stroke_distance(pts: &[(f32, f32)], p: (f32, f32)) -> f32 {
    if pts.is_empty() {
        return f32::INFINITY;
    }
    if pts.len() == 1 {
        return (p.0 - pts[0].0).hypot(p.1 - pts[0].1);
    }
    pts.windows(2)
        .map(|pair| {
            let (ax, ay) = pair[0];
            let (bx, by) = pair[1];
            let dx = bx - ax;
            let dy = by - ay;
            let t = (((p.0 - ax) * dx + (p.1 - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
            (p.0 - (ax + t * dx)).hypot(p.1 - (ay + t * dy))
        })
        .fold(f32::INFINITY, f32::min)
}

/// 移动标注并尽量限制在图片范围内，避免一拖就把标注完全拖出画面。
fn move_shape_clamped(shape: &mut Shape, delta: (f32, f32), img_w: f32, img_h: f32) {
    let bbox = shape.bbox();
    if matches!(shape, Shape::Text { .. }) || bbox.w >= img_w || bbox.h >= img_h {
        // 文字或超大图允许边缘超出画布裁切（至少保留 24px 可见，避免超长文本锁死左右移动）。
        let margin = 24.0;
        let min_x = margin - bbox.w;
        let max_x = img_w - margin;
        let min_y = margin - bbox.h;
        let max_y = img_h - margin;
        let nx = (bbox.x + delta.0).clamp(min_x.min(max_x), min_x.max(max_x));
        let ny = (bbox.y + delta.1).clamp(min_y.min(max_y), min_y.max(max_y));
        move_shape(shape, (nx - bbox.x, ny - bbox.y));
        return;
    }
    let mut dx = delta.0;
    let mut dy = delta.1;
    if bbox.x + dx < 0.0 {
        dx = -bbox.x;
    }
    if bbox.y + dy < 0.0 {
        dy = -bbox.y;
    }
    if bbox.x + bbox.w + dx > img_w {
        dx = img_w - bbox.x - bbox.w;
    }
    if bbox.y + bbox.h + dy > img_h {
        dy = img_h - bbox.y - bbox.h;
    }
    move_shape(shape, (dx, dy));
}

/// 用 `rgpui` 文本系统测量单行渲染宽度（逻辑像素）。
fn measure_line_width(window: &Window, text: &str, font_size: f32, family: &str) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    window
        .text_system()
        .shape_line(
            text.to_string().into(),
            px(font_size),
            &[rgpui::TextRun {
                len: text.len(),
                font: rgpui::font(family.to_string()),
                color: rgb(0xFFFFFF).into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        )
        .width
        .as_f32()
}

/// 按 `max_w` 宽度将文本自动折行为多行（编辑态与提交态共用，确保所见即所得）。
fn wrap_lines_in_ui(
    window: &Window,
    content: &str,
    font_size: f32,
    family: &str,
    max_w: f32,
) -> Vec<String> {
    let mut lines = Vec::new();
    for para in content.split('\n') {
        let mut cur = String::new();
        for ch in para.chars() {
            let mut next = cur.clone();
            next.push(ch);
            if !cur.is_empty() && measure_line_width(window, &next, font_size, family) > max_w {
                lines.push(std::mem::take(&mut cur));
            }
            cur.push(ch);
        }
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// 矢量标注转 canvas 绘制路径。
///
/// `paint_path` 使用窗口坐标，不会自动叠加 canvas 自身的布局偏移。
/// 因此必须使用 rgpui 在绘制回调里传入的 `bounds.origin` 作为图片容器真实左上角，
/// 避免手写工具栏高度或图片区居中偏移时漏算。
/// 向 `PathBuilder` 追加（可选圆角的）矩形闭合路径，供填充与描边共用。
fn append_rect_path(
    builder: &mut PathBuilder,
    rect: &PixelRect,
    radius: f32,
    zoom: f32,
    origin_x: f32,
    origin_y: f32,
) {
    let pt = |x: f32, y: f32| point(px(origin_x + x * zoom), px(origin_y + y * zoom));
    let r = radius.max(0.0).min(rect.w * 0.5).min(rect.h * 0.5);
    if r <= 0.5 {
        builder.move_to(pt(rect.x, rect.y));
        builder.line_to(pt(rect.x + rect.w, rect.y));
        builder.line_to(pt(rect.x + rect.w, rect.y + rect.h));
        builder.line_to(pt(rect.x, rect.y + rect.h));
        builder.close();
        return;
    }
    let k = 0.552_284_8 * r;
    let (x, y, w, h) = (rect.x, rect.y, rect.w, rect.h);
    builder.move_to(pt(x + r, y));
    builder.line_to(pt(x + w - r, y));
    builder.cubic_bezier_to(pt(x + w, y + r), pt(x + w - r + k, y), pt(x + w, y + r - k));
    builder.line_to(pt(x + w, y + h - r));
    builder.cubic_bezier_to(
        pt(x + w - r, y + h),
        pt(x + w, y + h - r + k),
        pt(x + w - r + k, y + h),
    );
    builder.line_to(pt(x + r, y + h));
    builder.cubic_bezier_to(pt(x, y + h - r), pt(x + r - k, y + h), pt(x, y + h - r + k));
    builder.line_to(pt(x, y + r));
    builder.cubic_bezier_to(pt(x + r, y), pt(x, y + r - k), pt(x + r - k, y));
    builder.close();
}

/// 生成图形填充路径（矩形 / 椭圆 / 多边形支持填充，需在描边之前绘制）。
fn shape_fill_path(shape: &Shape, zoom: f32, origin_x: f32, origin_y: f32) -> Option<(Path, Rgba)> {
    let pt = |x: f32, y: f32| point(px(origin_x + x * zoom), px(origin_y + y * zoom));
    match shape {
        Shape::Rect {
            rect,
            fill: Some(fill_color),
            radius,
            ..
        } => {
            let mut builder = PathBuilder::fill();
            append_rect_path(&mut builder, rect, *radius, zoom, origin_x, origin_y);
            Some((builder.build().ok()?, fill_color.to_rgpui()))
        }
        Shape::Ellipse {
            rect,
            fill: Some(fill_color),
            ..
        } => {
            let (cx, cy) = (rect.x + rect.w / 2.0, rect.y + rect.h / 2.0);
            let (rx, ry) = (rect.w / 2.0, rect.h / 2.0);
            if rx <= 0.0 || ry <= 0.0 {
                return None;
            }
            let mut builder = PathBuilder::fill();
            for i in 0..48 {
                let t = i as f32 / 48.0 * std::f32::consts::TAU;
                let p = pt(cx + rx * t.cos(), cy + ry * t.sin());
                if i == 0 {
                    builder.move_to(p);
                } else {
                    builder.line_to(p);
                }
            }
            builder.close();
            Some((builder.build().ok()?, fill_color.to_rgpui()))
        }
        Shape::Polygon {
            pts,
            fill: Some(fill_color),
            ..
        } => {
            if pts.len() < 3 {
                return None;
            }
            let mut builder = PathBuilder::fill();
            for (i, (x, y)) in pts.iter().enumerate() {
                if i == 0 {
                    builder.move_to(pt(*x, *y));
                } else {
                    builder.line_to(pt(*x, *y));
                }
            }
            builder.close();
            Some((builder.build().ok()?, fill_color.to_rgpui()))
        }
        _ => None,
    }
}

fn shape_path(shape: &Shape, zoom: f32, origin_x: f32, origin_y: f32) -> Option<(Path, Rgba)> {
    let pt = |x: f32, y: f32| point(px(origin_x + x * zoom), px(origin_y + y * zoom));
    let stroker = |width: f32| PathBuilder::stroke(px((width * zoom).max(1.0)));
    match shape {
        Shape::Pen { pts, color, width } | Shape::Highlighter { pts, color, width } => {
            if pts.len() < 2 {
                return None;
            }
            let mut builder = stroker(*width);
            for (i, (x, y)) in pts.iter().enumerate() {
                if i == 0 {
                    builder.move_to(pt(*x, *y));
                } else {
                    builder.line_to(pt(*x, *y));
                }
            }
            let path = builder.build().ok()?;
            let mut c = color.to_rgpui();
            if matches!(shape, Shape::Highlighter { .. }) {
                c.a = 0.4;
            }
            Some((path, c))
        }
        Shape::Polygon {
            pts,
            color,
            width,
            has_stroke,
            ..
        } => {
            if !*has_stroke || pts.len() < 2 {
                return None;
            }
            let mut builder = stroker(*width);
            for (i, (x, y)) in pts.iter().enumerate() {
                if i == 0 {
                    builder.move_to(pt(*x, *y));
                } else {
                    builder.line_to(pt(*x, *y));
                }
            }
            builder.close();
            Some((builder.build().ok()?, color.to_rgpui()))
        }
        Shape::Rect {
            rect,
            color,
            width,
            has_stroke,
            radius,
            ..
        } => {
            if !*has_stroke {
                return None;
            }
            let mut builder = stroker(*width);
            append_rect_path(&mut builder, rect, *radius, zoom, origin_x, origin_y);
            Some((builder.build().ok()?, color.to_rgpui()))
        }
        Shape::Ellipse {
            rect,
            color,
            width,
            has_stroke,
            ..
        } => {
            if !*has_stroke {
                return None;
            }
            let (cx, cy) = (rect.x + rect.w / 2.0, rect.y + rect.h / 2.0);
            let (rx, ry) = (rect.w / 2.0, rect.h / 2.0);
            if rx <= 0.0 || ry <= 0.0 {
                return None;
            }
            let mut builder = stroker(*width);
            for i in 0..=48 {
                let t = i as f32 / 48.0 * std::f32::consts::TAU;
                let p = pt(cx + rx * t.cos(), cy + ry * t.sin());
                if i == 0 {
                    builder.move_to(p);
                } else {
                    builder.line_to(p);
                }
            }
            Some((builder.build().ok()?, color.to_rgpui()))
        }
        Shape::Line {
            from,
            to,
            color,
            width,
        } => {
            let mut builder = stroker(*width);
            builder.move_to(pt(from.0, from.1));
            builder.line_to(pt(to.0, to.1));
            Some((builder.build().ok()?, color.to_rgpui()))
        }
        Shape::Arrow {
            from,
            to,
            color,
            width,
        } => {
            let (dx, dy) = (to.0 - from.0, to.1 - from.1);
            let len = dx.hypot(dy);
            if len < 2.0 {
                return None;
            }
            let ang = dy.atan2(dx);
            let head = 12.0 + width * 2.0;
            let wing = 0.42;
            let base = ang + std::f32::consts::PI;
            let w1 = (
                to.0 + head * (base - wing).cos(),
                to.1 + head * (base - wing).sin(),
            );
            let w2 = (
                to.0 + head * (base + wing).cos(),
                to.1 + head * (base + wing).sin(),
            );
            let mut builder = stroker(*width);
            builder.move_to(pt(from.0, from.1));
            builder.line_to(pt(to.0, to.1));
            builder.move_to(pt(w1.0, w1.1));
            builder.line_to(pt(to.0, to.1));
            builder.line_to(pt(w2.0, w2.1));
            Some((builder.build().ok()?, color.to_rgpui()))
        }
        // 马赛克：像素块本身已可见，不额外绘制边框描边。
        Shape::Mosaic { .. } => None,
        // 文字走 div 层，画布跳过。
        Shape::Text { .. } => None,
    }
}

/// 绘制马赛克实时预览：按导出逻辑的 12px 图片块采样底图并平铺到 canvas。
fn paint_mosaic_preview(
    window: &mut Window,
    base: &RgbaImage,
    rect: PixelRect,
    zoom: f32,
    origin_x: f32,
    origin_y: f32,
) {
    const BLOCK: i32 = 12;
    let x0 = rect.x.floor().max(0.0) as i32;
    let y0 = rect.y.floor().max(0.0) as i32;
    let x1 = (rect.x + rect.w).ceil().min(base.width() as f32) as i32;
    let y1 = (rect.y + rect.h).ceil().min(base.height() as f32) as i32;
    if x0 >= x1 || y0 >= y1 {
        return;
    }

    let mut by = y0;
    while by < y1 {
        let mut bx = x0;
        while bx < x1 {
            // 直接采样图片块左上角，预览与导出保持同一马赛克颗粒度。
            let sample = base.get_pixel(bx as u32, by as u32);
            let color = rgpui::rgba(
                ((sample[0] as u32) << 24)
                    | ((sample[1] as u32) << 16)
                    | ((sample[2] as u32) << 8)
                    | (sample[3] as u32),
            );
            let block_x1 = (bx + BLOCK).min(x1);
            let block_y1 = (by + BLOCK).min(y1);
            let bounds = Bounds::new(
                point(
                    px(origin_x + bx as f32 * zoom),
                    px(origin_y + by as f32 * zoom),
                ),
                size(
                    px((block_x1 - bx) as f32 * zoom),
                    px((block_y1 - by) as f32 * zoom),
                ),
            );
            window.paint_quad(fill(bounds, color));
            bx += BLOCK;
        }
        by += BLOCK;
    }
}

impl Render for ImageViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        self.area_w = viewport.width.as_f32().max(1.0);
        self.area_h = viewport.height.as_f32().max(1.0);
        self.fit_zoom = (self.area_w / self.preview.w as f32)
            .min(self.area_h / self.preview.h as f32)
            .clamp(MIN_ZOOM, 1.0);
        let transform_box = if !self.transform_originals.is_empty() {
            self.transform_group_bounds(window)
        } else {
            None
        };
        let (w, h) = (self.preview.w, self.preview.h);
        let dw = (w as f32 * self.zoom).round().max(1.0);
        let dh = (h as f32 * self.zoom).round().max(1.0);
        let percent = format!("{}%", (self.zoom * 100.0).round() as u32);
        let vt = ViewerTheme::current();

        div()
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(vt.canvas_bg)
            .text_color(vt.text_primary)
            .track_focus(&self.focus_handle)
            // 根视图监听鼠标移动/抬起，确保在右侧 SV/色相条拖拽滑出边界时仍跟手。
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _window, cx| {
                if this.sv_dragging {
                    let sv_b = this.sv_bounds.lock().ok().and_then(|g| *g);
                    if let Some(b) = sv_b {
                        let w = b.size.width.as_f32().max(1.0);
                        let h = b.size.height.as_f32().max(1.0);
                        this.picker_sat =
                            ((ev.position.x.as_f32() - b.origin.x.as_f32()) / w).clamp(0.0, 1.0);
                        this.picker_val = (1.0
                            - (ev.position.y.as_f32() - b.origin.y.as_f32()) / h)
                            .clamp(0.0, 1.0);
                        this.apply_picker_hsv(cx);
                    }
                } else if this.hue_dragging {
                    let hue_b = this.hue_bounds.lock().ok().and_then(|g| *g);
                    if let Some(b) = hue_b {
                        let w = b.size.width.as_f32().max(1.0);
                        let t =
                            ((ev.position.x.as_f32() - b.origin.x.as_f32()) / w).clamp(0.0, 1.0);
                        this.picker_hue = t * 360.0;
                        this.apply_picker_hsv(cx);
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _ev: &MouseUpEvent, _window, _cx| {
                    this.sv_dragging = false;
                    this.hue_dragging = false;
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _ev: &MouseUpEvent, _window, _cx| {
                    this.sv_dragging = false;
                    this.hue_dragging = false;
                }),
            )
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                if this.closing {
                    return;
                }
                let modi = &ev.keystroke.modifiers;
                if modi.control && ev.keystroke.key.as_str() == "t" {
                    this.begin_transform(window, cx);
                    cx.stop_propagation();
                    return;
                }
                if !this.transform_originals.is_empty() {
                    match ev.keystroke.key.as_str() {
                        "escape" => {
                            this.end_transform(true);
                            cx.notify();
                            return;
                        }
                        "enter" => {
                            this.end_transform(false);
                            cx.notify();
                            return;
                        }
                        _ => {}
                    }
                }
                if modi.control && ev.keystroke.key.as_str() == "s" {
                    this.handle(ViewerAction::Save, window, cx);
                    cx.stop_propagation();
                    return;
                }
                if modi.control
                    && ev.keystroke.key.as_str() == "v"
                    && this.text_editing.is_none()
                    && let Some(clip_path) = crate::export::read_image_from_clipboard(true)
                    && let Some(preview) = preview_from_path(&clip_path)
                {
                    open_viewer(this.main.clone(), cx, preview);
                    cx.stop_propagation();
                    return;
                }
                if this
                    .text_size_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window)
                {
                    if matches!(ev.keystroke.key.as_str(), "enter" | "escape") {
                        let fh = this.focus_handle.clone();
                        window.focus(&fh, cx);
                        cx.stop_propagation();
                    }
                    return;
                }
                if this.text_editing.is_some() {
                    match ev.keystroke.key.as_str() {
                        // 单次 Esc 提交文字并直接切回选择工具；Ctrl+Enter 提交文字，普通 Enter 保留换行。
                        "escape" => {
                            this.commit_text(window, cx);
                            this.tool = AnnoTool::Select;
                            cx.notify();
                            cx.stop_propagation();
                        }
                        "enter" if modi.control => {
                            this.commit_text(window, cx);
                            cx.stop_propagation();
                        }
                        _ => {}
                    }
                    return;
                }
                // Ctrl+A：全选所有已标注元素（非文本编辑状态下生效）。
                if modi.control
                    && !modi.shift
                    && !modi.alt
                    && !modi.platform
                    && ev.keystroke.key.eq_ignore_ascii_case("a")
                {
                    this.select_all_shapes(window, cx);
                    cx.stop_propagation();
                    return;
                }
                // Delete / Backspace 删除选中的元素。
                if matches!(ev.keystroke.key.as_str(), "delete" | "backspace") {
                    let mut indices = this.selected_indices();
                    if !indices.is_empty() {
                        indices.sort_unstable_by(|a, b| b.cmp(a));
                        for idx in indices {
                            if idx < this.shapes.len() {
                                this.shapes.remove(idx);
                            }
                        }
                        this.end_transform(false);
                        this.clear_selection();
                        this.redo_stack.clear();
                        this.set_viewer_status("已删除选中元素", cx);
                        cx.notify();
                        cx.stop_propagation();
                        return;
                    }
                }
                // Shift+X：互换填充与描边（矩形 / 椭圆 / 多边形）。
                if modi.shift
                    && !modi.control
                    && !modi.alt
                    && ev.keystroke.key.eq_ignore_ascii_case("x")
                {
                    this.handle(ViewerAction::SwapFillStroke, window, cx);
                    cx.stop_propagation();
                    return;
                }
                // 多选对齐快捷键：VIA 功能键（F14-F18 / Pause）与 Alt+W/A/S/D/H/V。
                let key_l = ev.keystroke.key.as_str();
                let align_hit = match key_l {
                    "f14" => Some(AlignMode::Top),
                    "f15" => Some(AlignMode::Left),
                    "f16" => Some(AlignMode::Bottom),
                    "f17" => Some(AlignMode::Right),
                    "f18" => Some(AlignMode::CenterH),
                    "pause" => Some(AlignMode::CenterV),
                    "w" if modi.alt && !modi.control => Some(AlignMode::Top),
                    "a" if modi.alt && !modi.control => Some(AlignMode::Left),
                    "s" if modi.alt && !modi.control => Some(AlignMode::Bottom),
                    "d" if modi.alt && !modi.control => Some(AlignMode::Right),
                    "h" if modi.alt && !modi.control => Some(AlignMode::CenterH),
                    "v" if modi.alt && !modi.control => Some(AlignMode::CenterV),
                    _ => None,
                };
                if let Some(mode) = align_hit {
                    this.handle(ViewerAction::Align(mode), window, cx);
                    cx.stop_propagation();
                    return;
                }
                if !modi.control
                    && !modi.alt
                    && !modi.platform
                    && let Some(tool) = AnnoTool::from_shortcut(ev.keystroke.key.as_str())
                {
                    this.handle(ViewerAction::Tool(tool), window, cx);
                    return;
                }
                match ev.keystroke.key.as_str() {
                    "escape" => {
                        if this.tool != AnnoTool::Select {
                            this.handle(ViewerAction::Tool(AnnoTool::Select), window, cx);
                        } else if this.marquee.is_some()
                            || this.selected.is_some()
                            || !this.selected_many.is_empty()
                        {
                            this.marquee = None;
                            this.clear_selection();
                            cx.notify();
                        }
                    }
                    "+" | "=" => this.zoom_by(ZOOM_STEP, cx),
                    "-" | "_" => this.zoom_by(1.0 / ZOOM_STEP, cx),
                    "0" => this.handle(ViewerAction::Fit, window, cx),
                    // [ / ] 步进调节线宽（与滑杆双向同步）。
                    "[" => this.handle(ViewerAction::WidthNudge(-1.0), window, cx),
                    "]" => this.handle(ViewerAction::WidthNudge(1.0), window, cx),
                    // 空格按住平移（文字输入框里打空格时不拦截，留给输入法）。
                    "space" if this.text_editing.is_none() => {
                        if !this.space_held {
                            this.space_held = true;
                            cx.notify();
                        }
                    }
                    "z" if modi.control && !modi.shift => {
                        this.handle(ViewerAction::Undo, window, cx);
                    }
                    "z" if modi.control && modi.shift => {
                        this.handle(ViewerAction::Redo, window, cx);
                    }
                    "y" if modi.control => {
                        this.handle(ViewerAction::Redo, window, cx);
                    }
                    _ => {}
                }
            }))
            // 空格松开：结束临时平移（拖一半松空格也停，避免无空格平移继续跟手）。
            .on_key_up(cx.listener(|this, ev: &KeyUpEvent, _window, cx| {
                if ev.keystroke.key.as_str() == "space" && this.space_held {
                    this.space_held = false;
                    this.pan_last = None;
                    cx.notify();
                }
            }))
            // 底层全幅画布区（放大后可自然延伸至左右面板与顶部胶囊下层）。
            .child({
                let left = (self.area_w - dw) / 2.0 + self.offset.0;
                let top = (self.area_h - dh) / 2.0 + self.offset.1;
                let zoom = self.zoom;
                let area = div().absolute().inset_0().overflow_hidden();
                // Windows 后端的抓手会退化成箭头，移动统一借用系统手形；
                // 其余按工具显示。
                let area = if self.pan_last.is_some() {
                    area.cursor(CursorStyle::PointingHand)
                } else if !self.transform_originals.is_empty() {
                    area.cursor(transform_cursor(
                        self.transform_drag
                            .as_ref()
                            .map(|drag| drag.handle)
                            .or(self.transform_hover),
                    ))
                } else if self.space_held {
                    area.cursor(CursorStyle::PointingHand)
                } else {
                    match self.tool {
                        AnnoTool::Select => {
                            if self.drag_text.is_some()
                                || self.drag_shape.is_some()
                                || self.hover_shape
                                || self.can_pan()
                            {
                                area.cursor(CursorStyle::PointingHand)
                            } else {
                                area.cursor(CursorStyle::Arrow)
                            }
                        }
                        AnnoTool::Text => area.cursor(CursorStyle::IBeam),
                        AnnoTool::Eyedropper => area.cursor(CursorStyle::Crosshair),
                        AnnoTool::Eraser => area.cursor(CursorStyle::OperationNotAllowed),
                        AnnoTool::Rect
                        | AnnoTool::Line
                        | AnnoTool::Ellipse
                        | AnnoTool::Arrow
                        | AnnoTool::Pen
                        | AnnoTool::Highlighter
                        | AnnoTool::Mosaic => area.cursor(CursorStyle::Crosshair),
                    }
                };
                let area = area
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                            this.on_anno_down(ev, window, cx);
                        }),
                    )
                    // 中键拖拽平移（与工具无关，默认矩形下放大看图用）。
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(|this, ev: &MouseDownEvent, _window, cx| {
                            this.on_pan_down(ev, cx);
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
                        this.on_anno_move(ev, window, cx);
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _ev, window, cx| {
                            this.on_anno_up(Some(window), cx);
                        }),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, _ev, window, cx| {
                            this.on_anno_up(Some(window), cx);
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Middle,
                        cx.listener(|this, _ev: &MouseUpEvent, _window, cx| {
                            this.on_pan_up(cx);
                        }),
                    )
                    .on_mouse_up_out(
                        MouseButton::Middle,
                        cx.listener(|this, _ev: &MouseUpEvent, _window, cx| {
                            this.on_pan_up(cx);
                        }),
                    )
                    .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _window, cx| {
                        if this.draft.is_some() || this.transform_drag.is_some() {
                            return;
                        }
                        let dy = match ev.delta {
                            ScrollDelta::Pixels(p) => p.y.as_f32(),
                            ScrollDelta::Lines(p) => p.y * 40.0,
                        };
                        if dy > 0.0 {
                            this.zoom_by(ZOOM_STEP, cx);
                        } else if dy < 0.0 {
                            this.zoom_by(1.0 / ZOOM_STEP, cx);
                        }
                    }));
                // 图片与标注放进同一个坐标容器，不裁切容器边界以支持将元素移出画布并继续操作（导出时再按底图裁切）。
                let container = div()
                    .absolute()
                    .left(px(left))
                    .top(px(top))
                    .w(px(dw))
                    .h(px(dh))
                    .relative()
                    // 底图。
                    .child(img(self.preview.path.clone()).w(px(dw)).h(px(dh)))
                    // 文字标注层（每条独立底色，开关只决定新建默认值）。
                    .children({
                        let mut marks: Vec<Div> = Vec::new();
                        for shape in &self.shapes {
                            if let Shape::Text {
                                pos,
                                content,
                                color,
                                size,
                                bg,
                                bg_color,
                                max_w,
                                font_family,
                                stroke_color,
                                stroke_width,
                            } = shape
                            {
                                let sz = size * zoom;
                                let family: &str =
                                    font_family.as_deref().unwrap_or(TEXT_FONT_FAMILY);
                                let lh = (sz * TEXT_LINE_HEIGHT).ceil().max(1.0);
                                let build_text_node = |text_col: Rgba| {
                                    let mut node = div()
                                        .text_color(text_col)
                                        .font_family(family.to_string())
                                        .font_weight(rgpui::FontWeight::NORMAL)
                                        .text_size(px(sz))
                                        .line_height(px(lh));
                                    if let Some(box_w) = max_w {
                                        let lines = wrap_lines_in_ui(
                                            window,
                                            content,
                                            sz,
                                            family,
                                            box_w * zoom,
                                        );
                                        node = node.w(px(box_w * zoom)).flex().flex_col().children(
                                            lines.into_iter().map(|line| {
                                                div().h(px(lh)).whitespace_nowrap().child(line)
                                            }),
                                        );
                                    } else {
                                        let lines = content.split('\n');
                                        node = node.flex().flex_col().children(lines.map(|line| {
                                            div()
                                                .h(px(lh))
                                                .whitespace_nowrap()
                                                .child(line.to_string())
                                        }));
                                    }
                                    node
                                };
                                let mut d = div()
                                    .absolute()
                                    .left(px(pos.0 * zoom))
                                    .top(px(pos.1 * zoom));
                                if let Some(sc) = stroke_color
                                    && *stroke_width > 0.1
                                {
                                    let r = (stroke_width * zoom).max(0.8);
                                    let sc_rgpui = sc.to_rgpui();
                                    let mut wrapper = div().relative();
                                    // 12 方向圆形偏移描边，置于文字填充后方模拟圆角描边。
                                    for k in 0..12 {
                                        let ang = k as f32 / 12.0 * std::f32::consts::TAU;
                                        let ox = r * ang.cos();
                                        let oy = r * ang.sin();
                                        wrapper = wrapper.child(
                                            build_text_node(sc_rgpui)
                                                .absolute()
                                                .left(px(ox))
                                                .top(px(oy)),
                                        );
                                    }
                                    d = d.child(wrapper.child(build_text_node(color.to_rgpui())));
                                } else {
                                    d = d.child(build_text_node(color.to_rgpui()));
                                }
                                // 按条底色：用该条自己的底色，透明度统一。
                                let d = if *bg {
                                    d.bg(bg_color.as_text_bg_rgpui())
                                } else {
                                    d
                                };
                                marks.push(d);
                            }
                        }
                        // 正在编辑的 inline 输入框（段落文本显示清晰框体并实时多行自动换行，单行文本保持自由横向延伸）。
                        if let Some(editing) = &self.text_editing {
                            let input_text_size = editing.size * zoom;
                            let line_height = (input_text_size * TEXT_LINE_HEIGHT).ceil().max(1.0);
                            let edit_family: String = editing
                                .font_family
                                .clone()
                                .unwrap_or_else(|| TEXT_FONT_FAMILY.to_string());
                            let value = editing.input.read(cx).text().to_string();
                            let mut input = Input::new(&editing.input)
                                .appearance(false)
                                .bordered(false)
                                .focus_bordered(false)
                                .p_0()
                                .font_family(edit_family.clone())
                                .font_weight(rgpui::FontWeight::NORMAL)
                                .text_color(self.color.to_rgpui())
                                .text_size(px(input_text_size))
                                .line_height(px(line_height));

                            if let Some(mw) = editing.max_w {
                                let box_w_px = (mw * zoom).max(input_text_size * 2.0);
                                let area_box = div()
                                    .absolute()
                                    .left(px(editing.img_pos.0 * zoom))
                                    .top(px(editing.img_pos.1 * zoom))
                                    .w(px(box_w_px))
                                    .min_h(px(line_height))
                                    .border_1()
                                    .border_color(rgba(0x0A84FFE0))
                                    .bg(if self.text_bg && !value.is_empty() {
                                        self.text_bg_color.as_text_bg_rgpui()
                                    } else {
                                        rgba(0x0A84FF14)
                                    })
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, _ev, window, cx| {
                                            if let Some(editing) = &this.text_editing {
                                                editing.input.update(cx, |state, cx| {
                                                    state.focus(window, cx)
                                                });
                                            }
                                            cx.stop_propagation();
                                        }),
                                    )
                                    .child(input);
                                marks.push(area_box);
                            } else {
                                let lines_count = value.split('\n').count().max(1);
                                let box_height = lines_count as f32 * line_height;
                                input.style().size.height = Some(px(box_height).into());
                                let measured = value
                                    .split('\n')
                                    .map(|line| {
                                        measure_line_width(
                                            window,
                                            line,
                                            input_text_size,
                                            &edit_family,
                                        )
                                    })
                                    .fold(0.0f32, f32::max);
                                let box_width =
                                    (measured + input_text_size + 4.0).max(input_text_size * 2.0);
                                let mut editor = div()
                                    .absolute()
                                    .left(px(editing.img_pos.0 * zoom))
                                    .top(px(editing.img_pos.1 * zoom))
                                    .w(px(box_width))
                                    .h(px(box_height))
                                    .text_color(self.color.to_rgpui())
                                    .text_size(px(input_text_size))
                                    .line_height(px(line_height));
                                // 默认只显示原位文字和光标；用户主动打开文字底色时才预览实际底色。
                                if self.text_bg && !value.is_empty() {
                                    editor = editor.bg(self.text_bg_color.as_text_bg_rgpui());
                                }
                                marks.push(
                                    editor
                                        // 阻止输入框点击冒泡到图片区，避免触发 commit + place。
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|_, _ev, _, cx| {
                                                cx.stop_propagation();
                                            }),
                                        )
                                        .child(input),
                                );
                            }
                        }
                        marks
                    })
                    // 矢量标注画布。
                    .child({
                        let paint_shapes = self.shapes.clone();
                        let paint_draft = self.draft.clone();
                        let caret_input = self
                            .text_editing
                            .as_ref()
                            .map(|editing| editing.input.clone());
                        let base_image = self.base_image.clone();
                        let paint_marquee = self.marquee;
                        let paint_text_box = self.text_box_draft;
                        let selected_bboxes: Vec<PixelRect> = if transform_box.is_none() {
                            self.selected_indices()
                                .into_iter()
                                .filter_map(|i| {
                                    let s = self.shapes.get(i)?;
                                    // 马赛克画完后不留多余边框；仅在选择工具下高亮方便拖动。
                                    if matches!(s, Shape::Mosaic { .. })
                                        && self.tool != AnnoTool::Select
                                    {
                                        None
                                    } else {
                                        Some(self.shape_ui_bbox(s, window))
                                    }
                                })
                                .collect()
                        } else {
                            Vec::new()
                        };
                        canvas(
                            move |bounds, _, _| bounds,
                            move |_bounds, paint_bounds, window, cx| {
                                let origin = paint_bounds.origin;
                                for shape in paint_shapes.iter().chain(paint_draft.iter()) {
                                    if let Shape::Mosaic { rect } = shape
                                        && let Some(base) = &base_image
                                    {
                                        paint_mosaic_preview(
                                            window,
                                            base,
                                            *rect,
                                            zoom,
                                            origin.x.as_f32(),
                                            origin.y.as_f32(),
                                        );
                                    }
                                    if let Some((fill_path, fill_col)) = shape_fill_path(
                                        shape,
                                        zoom,
                                        origin.x.as_f32(),
                                        origin.y.as_f32(),
                                    ) {
                                        window.paint_path(fill_path, fill_col);
                                    }
                                    if let Some((path, color)) = shape_path(
                                        shape,
                                        zoom,
                                        origin.x.as_f32(),
                                        origin.y.as_f32(),
                                    ) {
                                        window.paint_path(path, color);
                                    }
                                }
                                // 选中态细线指示框（提示当前可直接用右侧属性栏批量调整）。
                                for b in &selected_bboxes {
                                    let pad = 4.0;
                                    let sx = origin.x.as_f32() + b.x * zoom - pad;
                                    let sy = origin.y.as_f32() + b.y * zoom - pad;
                                    let sw = (b.w * zoom + pad * 2.0).max(8.0);
                                    let sh = (b.h * zoom + pad * 2.0).max(8.0);
                                    let mut pb = PathBuilder::stroke(px(1.2));
                                    pb.move_to(point(px(sx), px(sy)));
                                    pb.line_to(point(px(sx + sw), px(sy)));
                                    pb.line_to(point(px(sx + sw), px(sy + sh)));
                                    pb.line_to(point(px(sx), px(sy + sh)));
                                    pb.line_to(point(px(sx), px(sy)));
                                    if let Ok(path) = pb.build() {
                                        window.paint_path(path, rgba(0x0A84FFD8));
                                    }
                                }
                                // Illustrator 风格左键框选或文本框拉框预览矩形。
                                for box_pts in paint_marquee.into_iter().chain(paint_text_box) {
                                    let (sx, sy, ex, ey) = box_pts;
                                    let r = PixelRect::from_points((sx, sy), (ex, ey));
                                    let rx = origin.x.as_f32() + r.x * zoom;
                                    let ry = origin.y.as_f32() + r.y * zoom;
                                    let rw = (r.w * zoom).max(1.0);
                                    let rh = (r.h * zoom).max(1.0);
                                    let box_bounds = Bounds {
                                        origin: point(px(rx), px(ry)),
                                        size: size(px(rw), px(rh)),
                                    };
                                    window.paint_quad(fill(box_bounds, rgba(0x0A84FF28)));
                                    let mut pb = PathBuilder::stroke(px(1.2));
                                    pb.move_to(point(px(rx), px(ry)));
                                    pb.line_to(point(px(rx + rw), px(ry)));
                                    pb.line_to(point(px(rx + rw), px(ry + rh)));
                                    pb.line_to(point(px(rx), px(ry + rh)));
                                    pb.line_to(point(px(rx), px(ry)));
                                    if let Ok(path) = pb.build() {
                                        window.paint_path(path, rgba(0x0A84FFE0));
                                    }
                                }
                                if let Some(bounds) = transform_box {
                                    paint_transform(bounds, zoom, origin, window);
                                }
                                if let Some(input) = &caret_input {
                                    paint_text_caret(input, window, cx);
                                }
                            },
                        )
                        .absolute()
                        // canvas 必须显式锚定到图片容器左上角，否则会按静态位置排到 img 后面。
                        .left(px(0.0))
                        .top(px(0.0))
                        .w(px(dw))
                        .h(px(dh))
                    });
                area.child(container)
            })
            // 顶部双胶囊栏（融合式拖拽 + 撤销重做 + 保存 / 复制完成 + 窗口控制）。
            .child(self.render_top_capsules(window, cx))
            // 左侧自适应高度工具箱（贴左居中，带图标 + 中文名称 + 快捷键）。
            .child(self.render_left_dock(cx))
            // 右侧自适应高度属性检查器（贴右居中，含圆角 SV 取色区 + 下方色相条）。
            .child(self.render_right_inspector(cx, &percent))
            // 左下角 3 秒渐隐状态提示胶囊。
            .children(self.render_bottom_status())
    }
}

/// 标注编辑器专属半透明视觉色板（跟随深/浅色模式）。
struct ViewerTheme {
    canvas_bg: Rgba,
    panel_bg: Rgba,
    card_bg: Rgba,
    hover_bg: Rgba,
    panel_border: Rgba,
    inner_border: Rgba,
    active_pill_bg: Rgba,
    active_pill_text: Rgba,
    accent: Rgba,
    text_primary: Rgba,
    text_secondary: Rgba,
    text_muted: Rgba,
}

impl ViewerTheme {
    /// 根据全局深浅色配置返回当前标注界面配色。
    fn current() -> Self {
        if crate::app::is_dark_mode() {
            Self {
                canvas_bg: rgb(0x0F1014),
                panel_bg: rgba(0x1C1C20D8),
                card_bg: rgba(0xFFFFFF10),
                hover_bg: rgba(0xFFFFFF1A),
                panel_border: rgba(0xFFFFFF24),
                inner_border: rgba(0xFFFFFF14),
                active_pill_bg: rgb(0xF5F5F7),
                active_pill_text: rgb(0x111216),
                accent: rgb(0xFF2D55),
                text_primary: rgb(0xF5F5F7),
                text_secondary: rgba(0xF5F5F7B0),
                text_muted: rgba(0xF5F5F76E),
            }
        } else {
            Self {
                canvas_bg: rgb(0xE6E9EF),
                panel_bg: rgba(0xFFFFFFDC),
                card_bg: rgba(0x0000000B),
                hover_bg: rgba(0x00000014),
                panel_border: rgba(0x0000001A),
                inner_border: rgba(0x00000010),
                active_pill_bg: rgb(0x1C1C1E),
                active_pill_text: rgb(0xFFFFFF),
                accent: rgb(0xFF2D55),
                text_primary: rgb(0x1C1C1E),
                text_secondary: rgba(0x1C1C1EB0),
                text_muted: rgba(0x1C1C1E6E),
            }
        }
    }
}

/// HSV（`h`: 0..360, `s`: 0..1, `v`: 0..1）转 `AnnoColor`。
fn hsv_to_anno(h: f32, s: f32, v: f32) -> AnnoColor {
    let h = h.rem_euclid(360.0);
    let s = s.clamp(0.0, 1.0);
    let v = v.clamp(0.0, 1.0);
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = if h < 60.0 {
        (c, x, 0.0)
    } else if h < 120.0 {
        (x, c, 0.0)
    } else if h < 180.0 {
        (0.0, c, x)
    } else if h < 240.0 {
        (0.0, x, c)
    } else if h < 300.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    AnnoColor(
        ((r + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((g + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((b + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        255,
    )
}

/// `AnnoColor` 反算 HSV（`h`: 0..360, `s`: 0..1, `v`: 0..1）。
fn rgb_to_hsv(color: AnnoColor) -> (f32, f32, f32) {
    let r = color.0 as f32 / 255.0;
    let g = color.1 as f32 / 255.0;
    let b = color.2 as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let h = if delta <= 1e-5 {
        0.0
    } else if (max - r).abs() < 1e-5 {
        60.0 * (((g - b) / delta).rem_euclid(6.0))
    } else if (max - g).abs() < 1e-5 {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    let s = if max <= 1e-5 { 0.0 } else { delta / max };
    (h, s, max)
}

/// 在给定 16×16 区域内绘制标注工具线性矢量图标。
fn paint_tool_icon(tool: AnnoTool, bounds: Bounds<Pixels>, color: Rgba, window: &mut Window) {
    let ox = bounds.origin.x.as_f32();
    let oy = bounds.origin.y.as_f32();
    let w = bounds.size.width.as_f32();
    let h = bounds.size.height.as_f32();
    let pt = |nx: f32, ny: f32| point(px(ox + nx * w), px(oy + ny * h));

    let mut b = PathBuilder::stroke(px(1.5));
    match tool {
        AnnoTool::Select => {
            b.move_to(pt(0.22, 0.16));
            b.line_to(pt(0.22, 0.80));
            b.line_to(pt(0.42, 0.62));
            b.line_to(pt(0.60, 0.88));
            b.line_to(pt(0.70, 0.82));
            b.line_to(pt(0.52, 0.56));
            b.line_to(pt(0.78, 0.56));
            b.line_to(pt(0.22, 0.16));
        }
        AnnoTool::Rect => {
            b.move_to(pt(0.18, 0.22));
            b.line_to(pt(0.82, 0.22));
            b.line_to(pt(0.82, 0.78));
            b.line_to(pt(0.18, 0.78));
            b.line_to(pt(0.18, 0.22));
        }
        AnnoTool::Ellipse => {
            let steps = 20;
            for i in 0..=steps {
                let a = (i as f32 / steps as f32) * std::f32::consts::TAU;
                let p = pt(0.5 + 0.34 * a.cos(), 0.5 + 0.26 * a.sin());
                if i == 0 {
                    b.move_to(p);
                } else {
                    b.line_to(p);
                }
            }
        }
        AnnoTool::Line => {
            b.move_to(pt(0.20, 0.80));
            b.line_to(pt(0.80, 0.20));
        }
        AnnoTool::Arrow => {
            b.move_to(pt(0.20, 0.80));
            b.line_to(pt(0.80, 0.20));
            b.move_to(pt(0.44, 0.20));
            b.line_to(pt(0.80, 0.20));
            b.line_to(pt(0.80, 0.56));
        }
        AnnoTool::Pen => {
            b.move_to(pt(0.22, 0.78));
            b.line_to(pt(0.32, 0.54));
            b.line_to(pt(0.70, 0.16));
            b.line_to(pt(0.84, 0.30));
            b.line_to(pt(0.46, 0.68));
            b.line_to(pt(0.22, 0.78));
        }
        AnnoTool::Highlighter => {
            b.move_to(pt(0.18, 0.76));
            b.line_to(pt(0.52, 0.76));
            b.move_to(pt(0.28, 0.64));
            b.line_to(pt(0.66, 0.22));
            b.line_to(pt(0.82, 0.38));
            b.line_to(pt(0.44, 0.72));
        }
        AnnoTool::Mosaic => {
            // 2×2 网格代表马赛克。
            b.move_to(pt(0.20, 0.20));
            b.line_to(pt(0.80, 0.20));
            b.line_to(pt(0.80, 0.80));
            b.line_to(pt(0.20, 0.80));
            b.line_to(pt(0.20, 0.20));
            b.move_to(pt(0.50, 0.20));
            b.line_to(pt(0.50, 0.80));
            b.move_to(pt(0.20, 0.50));
            b.line_to(pt(0.80, 0.50));
        }
        AnnoTool::Text => {
            b.move_to(pt(0.22, 0.22));
            b.line_to(pt(0.78, 0.22));
            b.move_to(pt(0.50, 0.22));
            b.line_to(pt(0.50, 0.80));
            b.move_to(pt(0.36, 0.80));
            b.line_to(pt(0.64, 0.80));
        }
        AnnoTool::Eyedropper => {
            b.move_to(pt(0.20, 0.80));
            b.line_to(pt(0.34, 0.66));
            b.line_to(pt(0.68, 0.32));
            b.move_to(pt(0.56, 0.20));
            b.line_to(pt(0.80, 0.44));
        }
        AnnoTool::Eraser => {
            b.move_to(pt(0.22, 0.62));
            b.line_to(pt(0.56, 0.22));
            b.line_to(pt(0.82, 0.48));
            b.line_to(pt(0.48, 0.82));
            b.line_to(pt(0.22, 0.62));
            b.move_to(pt(0.44, 0.82));
            b.line_to(pt(0.84, 0.82));
        }
    }
    if let Ok(path) = b.build() {
        window.paint_path(path, color);
    }
}

/// 左侧工具箱中显示的工具名称（中文简洁名称 + 快捷键提示）。
fn tool_label_and_key(tool: AnnoTool) -> (&'static str, &'static str) {
    match tool {
        AnnoTool::Select => ("选择", "V"),
        AnnoTool::Rect => ("矩形", "R"),
        AnnoTool::Ellipse => ("椭圆", "O"),
        AnnoTool::Line => ("直线", "L"),
        AnnoTool::Arrow => ("箭头", "3"),
        AnnoTool::Pen => ("画笔", "B"),
        AnnoTool::Highlighter => ("荧光笔", "5"),
        AnnoTool::Mosaic => ("马赛克", "M"),
        AnnoTool::Text => ("文字", "T"),
        AnnoTool::Eyedropper => ("吸管", "I"),
        AnnoTool::Eraser => ("橡皮擦", "E"),
    }
}

impl ImageViewer {
    /// 顶部双悬浮胶囊栏（左：拖拽/品牌/尺寸/撤销重做；右：主题/保存/复制完成/窗口控制）。
    fn render_top_capsules(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let vt = ViewerTheme::current();
        let view = cx.entity();
        let dark = crate::app::is_dark_mode();
        let maximized = crate::window::is_maximized(window);

        div()
            .id("viewer-top-title-strip")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .right(px(0.0))
            .h(px(54.0))
            .px(px(12.0))
            .pt(px(12.0))
            .flex()
            .items_center()
            .justify_between()
            // 顶部整条透明标题栏空白处（含左右胶囊之间）均可拖动窗口与双击最大化。
            .on_mouse_down(MouseButton::Left, |event, window, cx| {
                cx.stop_propagation();
                if event.click_count == 2 {
                    crate::window::toggle_maximize(window);
                } else {
                    window.start_window_move();
                }
            })
            // 左侧胶囊：拖拽区（品牌 + 尺寸）+ 撤销/重做 + 当前状态（按钮区不触发 start_window_move）
            .child(
                div()
                    .id("viewer-top-left-pill")
                    .occlude()
                    .h(px(40.0))
                    .px_3()
                    .rounded_full()
                    .bg(vt.panel_bg)
                    .border_1()
                    .border_color(vt.panel_border)
                    .flex()
                    .items_center()
                    .gap_2()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _ev, _, cx| cx.stop_propagation()),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|_, _ev, _, cx| cx.stop_propagation()),
                    )
                    .on_scroll_wheel(cx.listener(|_, _ev, _, cx| cx.stop_propagation()))
                    .child(
                        div()
                            .id("viewer-top-drag-zone")
                            .h_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .on_mouse_down(MouseButton::Left, |event, window, cx| {
                                cx.stop_propagation();
                                if event.click_count == 2 {
                                    crate::window::toggle_maximize(window);
                                } else {
                                    window.start_window_move();
                                }
                            })
                            .child(img(viewer_logo()).w(px(22.0)).h(px(22.0)))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(vt.text_primary)
                                    .child(tr("viewer.editor_title")),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(vt.text_muted)
                                    .child(format!("{} × {}", self.preview.w, self.preview.h)),
                            ),
                    )
                    .child(div().w(px(1.0)).h(px(14.0)).bg(vt.inner_border))
                    .child(self.capsule_btn(cx, "undo", tr("viewer.undo"), ViewerAction::Undo))
                    .child(self.capsule_btn(cx, "redo", tr("viewer.redo"), ViewerAction::Redo)),
            )
            // 右侧胶囊：深浅主题切换 + 保存 + 复制完成 + 窗口控制
            .child(
                div()
                    .id("viewer-top-right-pill")
                    .occlude()
                    .h(px(40.0))
                    .px_3()
                    .rounded_full()
                    .bg(vt.panel_bg)
                    .border_1()
                    .border_color(vt.panel_border)
                    .flex()
                    .items_center()
                    .gap_2()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _ev, _, cx| cx.stop_propagation()),
                    )
                    .on_scroll_wheel(cx.listener(|_, _ev, _, cx| cx.stop_propagation()))
                    .child(self.capsule_btn(
                        cx,
                        "theme",
                        if dark { "浅色" } else { "深色" },
                        ViewerAction::ToggleTheme,
                    ))
                    .child(div().w(px(1.0)).h(px(14.0)).bg(vt.inner_border))
                    .child(self.capsule_btn(cx, "save", tr("viewer.save"), ViewerAction::Save))
                    .child(self.capsule_btn(cx, "copy", tr("viewer.copy"), ViewerAction::Copy))
                    .child({
                        let view_p = view.clone();
                        div()
                            .id("viewer-pin-finish")
                            .h(px(28.0))
                            .px_3()
                            .rounded_full()
                            .bg(vt.active_pill_bg)
                            .text_color(vt.active_pill_text)
                            .text_xs()
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .child("钉图")
                            .on_click(move |_, window, cx| {
                                view_p.update(cx, |this, cx| {
                                    this.handle(ViewerAction::Pin, window, cx)
                                });
                            })
                    })
                    .child(div().w(px(1.0)).h(px(14.0)).bg(vt.inner_border))
                    .children(
                        [(0, "─"), (1, if maximized { "❐" } else { "□" }), (2, "✕")]
                            .into_iter()
                            .map(|(id, lbl)| {
                                div()
                                    .id(format!("viewer-win-{id}"))
                                    .w(px(24.0))
                                    .h(px(24.0))
                                    .rounded_full()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_xs()
                                    .text_color(vt.text_secondary)
                                    .cursor_pointer()
                                    .hover(move |s| {
                                        s.bg(if id == 2 {
                                            rgba(0xFF3B30D8)
                                        } else {
                                            vt.hover_bg
                                        })
                                    })
                                    .child(lbl)
                                    .on_click(cx.listener(move |this, _, window, cx| match id {
                                        0 => window.minimize_window(),
                                        1 => crate::window::toggle_maximize(window),
                                        _ => this.handle(ViewerAction::Close, window, cx),
                                    }))
                            }),
                    ),
            )
    }

    /// 顶部胶囊通用按钮。
    fn capsule_btn(
        &self,
        cx: &mut Context<Self>,
        id: &str,
        label: impl Into<String>,
        action: ViewerAction,
    ) -> impl IntoElement {
        let vt = ViewerTheme::current();
        let view = cx.entity();
        div()
            .id(format!("viewer-cap-{id}"))
            .h(px(28.0))
            .px_2()
            .rounded_full()
            .flex()
            .items_center()
            .text_xs()
            .text_color(vt.text_secondary)
            .cursor_pointer()
            .hover(move |s| s.bg(vt.hover_bg))
            .child(label.into())
            .on_click(move |_, window, cx| {
                let act = action.clone();
                view.update(cx, |this, cx| this.handle(act, window, cx));
            })
    }

    /// 左侧自适应高度工具箱（贴左居中，带图标 + 中文文本 + 快捷键，可折叠）。
    fn render_left_dock(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let vt = ViewerTheme::current();
        let view = cx.entity();
        let compact = self.dock_compact;

        let groups: &[&[AnnoTool]] = &[
            &[AnnoTool::Select],
            &[
                AnnoTool::Rect,
                AnnoTool::Ellipse,
                AnnoTool::Line,
                AnnoTool::Arrow,
            ],
            &[AnnoTool::Pen, AnnoTool::Highlighter, AnnoTool::Mosaic],
            &[AnnoTool::Text, AnnoTool::Eyedropper, AnnoTool::Eraser],
        ];

        let mut panel = div()
            .id("viewer-left-dock")
            .occlude()
            .w(px(if compact { 54.0 } else { 154.0 }))
            .p_2()
            .rounded(px(18.0))
            .bg(vt.panel_bg)
            .border_1()
            .border_color(vt.panel_border)
            .flex()
            .flex_col()
            .gap(px(2.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _ev, _, cx| cx.stop_propagation()),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|_, _ev, _, cx| cx.stop_propagation()),
            )
            .on_mouse_move(cx.listener(|_, _ev, _, cx| cx.stop_propagation()))
            .on_scroll_wheel(cx.listener(|_, _ev, _, cx| cx.stop_propagation()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_1()
                    .pb_1()
                    .children(
                        (!compact).then(|| div().text_xs().text_color(vt.text_muted).child("工具")),
                    )
                    .child({
                        let view_t = view.clone();
                        div()
                            .id("viewer-dock-toggle")
                            .w(px(22.0))
                            .h(px(22.0))
                            .rounded_full()
                            .bg(vt.card_bg)
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_xs()
                            .text_color(vt.text_secondary)
                            .cursor_pointer()
                            .hover(move |s| s.bg(vt.hover_bg))
                            .child(if compact { "›" } else { "‹" })
                            .on_click(move |_, window, cx| {
                                view_t.update(cx, |this, cx| {
                                    this.handle(ViewerAction::ToggleDock, window, cx);
                                });
                            })
                    }),
            );

        for (gi, group) in groups.iter().enumerate() {
            if gi > 0 {
                panel = panel.child(div().h(px(1.0)).mx_1().my_1().bg(vt.inner_border));
            }
            for &tool in *group {
                let active = self.tool == tool
                    && !(tool == AnnoTool::Select && !self.transform_originals.is_empty());
                let (name, key) = tool_label_and_key(tool);
                let icon_color = if active {
                    vt.active_pill_text
                } else {
                    vt.text_secondary
                };
                let view_tool = view.clone();
                let mut row = div()
                    .id(format!("viewer-dock-{key}"))
                    .h(px(32.0))
                    .rounded(px(11.0))
                    .flex()
                    .items_center()
                    .when(compact, |d| d.px_0().justify_center())
                    .when(!compact, |d| d.px_2().justify_between())
                    .cursor_pointer()
                    .bg(if active {
                        vt.active_pill_bg
                    } else {
                        rgba(0x00000000)
                    })
                    .hover(move |s| if active { s } else { s.bg(vt.card_bg) })
                    .on_click(move |_, window, cx| {
                        view_tool.update(cx, |this, cx| {
                            this.handle(ViewerAction::Tool(tool), window, cx);
                        });
                    });

                let mut left = div().flex().items_center().gap_2().child(
                    canvas(
                        move |bounds, _, _| bounds,
                        move |_bounds, paint_bounds, window, _| {
                            paint_tool_icon(tool, paint_bounds, icon_color, window);
                        },
                    )
                    .w(px(16.0))
                    .h(px(16.0)),
                );
                if !compact {
                    left = left.child(
                        div()
                            .text_xs()
                            .text_color(if active {
                                vt.active_pill_text
                            } else {
                                vt.text_primary
                            })
                            .child(name),
                    );
                }
                row = row.child(left);
                if !compact {
                    row = row.child(
                        div()
                            .text_xs()
                            .text_color(if active {
                                vt.active_pill_text
                            } else {
                                vt.text_muted
                            })
                            .child(key),
                    );
                }
                panel = panel.child(row);

                // 在「选择」工具正下方显式提供「变换」入口（对应 Ctrl+T 自由缩放变形）。
                if tool == AnnoTool::Select {
                    let tr_active = !self.transform_originals.is_empty();
                    let tr_icon_color = if tr_active {
                        vt.active_pill_text
                    } else {
                        vt.text_secondary
                    };
                    let view_tr = view.clone();
                    let mut tr_row = div()
                        .id("viewer-dock-transform")
                        .h(px(32.0))
                        .rounded(px(11.0))
                        .flex()
                        .items_center()
                        .when(compact, |d| d.px_0().justify_center())
                        .when(!compact, |d| d.px_2().justify_between())
                        .cursor_pointer()
                        .bg(if tr_active {
                            vt.active_pill_bg
                        } else {
                            rgba(0x00000000)
                        })
                        .hover(move |s| if tr_active { s } else { s.bg(vt.card_bg) })
                        .on_click(move |_, window, cx| {
                            view_tr.update(cx, |this, cx| {
                                this.handle(ViewerAction::Transform, window, cx);
                            });
                        });
                    let mut tr_left = div().flex().items_center().gap_2().child(
                        canvas(
                            move |bounds, _, _| bounds,
                            move |_bounds, paint_bounds, window, _| {
                                let ox = paint_bounds.origin.x.as_f32();
                                let oy = paint_bounds.origin.y.as_f32();
                                let w = paint_bounds.size.width.as_f32();
                                let h = paint_bounds.size.height.as_f32();
                                let pt = |nx: f32, ny: f32| point(px(ox + nx * w), px(oy + ny * h));
                                let mut b = PathBuilder::stroke(px(1.4));
                                // 外框 + 四角控制点小框示意自由变换
                                b.move_to(pt(0.24, 0.24));
                                b.line_to(pt(0.76, 0.24));
                                b.line_to(pt(0.76, 0.76));
                                b.line_to(pt(0.24, 0.76));
                                b.line_to(pt(0.24, 0.24));
                                for (cx_n, cy_n) in
                                    [(0.20, 0.20), (0.80, 0.20), (0.80, 0.80), (0.20, 0.80)]
                                {
                                    b.move_to(pt(cx_n - 0.06, cy_n - 0.06));
                                    b.line_to(pt(cx_n + 0.06, cy_n - 0.06));
                                    b.line_to(pt(cx_n + 0.06, cy_n + 0.06));
                                    b.line_to(pt(cx_n - 0.06, cy_n + 0.06));
                                    b.line_to(pt(cx_n - 0.06, cy_n - 0.06));
                                }
                                if let Ok(path) = b.build() {
                                    window.paint_path(path, tr_icon_color);
                                }
                            },
                        )
                        .w(px(16.0))
                        .h(px(16.0)),
                    );
                    if !compact {
                        tr_left = tr_left.child(
                            div()
                                .text_xs()
                                .text_color(if tr_active {
                                    vt.active_pill_text
                                } else {
                                    vt.text_primary
                                })
                                .child("变换"),
                        );
                    }
                    tr_row = tr_row.child(tr_left);
                    if !compact {
                        tr_row = tr_row.child(
                            div()
                                .text_xs()
                                .text_color(if tr_active {
                                    vt.active_pill_text
                                } else {
                                    vt.text_muted
                                })
                                .child("^T"),
                        );
                    }
                    panel = panel.child(tr_row);
                }
            }
        }

        // 外层居中容器不拦截空白处鼠标事件，确保画布可自由交互。
        div()
            .absolute()
            .left(px(12.0))
            .top(px(64.0))
            .bottom(px(12.0))
            .flex()
            .flex_col()
            .justify_center()
            .child(panel)
    }

    /// 右侧自适应高度属性检查器面板（颜色方形 SV + 下方色相条 + 粗细 + 文字底色 + 缩放）。
    fn render_right_inspector(&self, cx: &mut Context<Self>, percent: &str) -> impl IntoElement {
        let vt = ViewerTheme::current();
        let view = cx.entity();
        let (tool_name, _) = tool_label_and_key(self.tool);
        let selected_list = self.selected_indices();
        let badge_text: String = if selected_list.len() > 1 {
            format!("已选 {} 项", selected_list.len())
        } else if let Some(&idx) = selected_list.first()
            && let Some(shape) = self.shapes.get(idx)
        {
            let kind = match shape {
                Shape::Rect { .. } => "矩形",
                Shape::Ellipse { .. } => "椭圆",
                Shape::Line { .. } => "直线",
                Shape::Arrow { .. } => "箭头",
                Shape::Pen { .. } => "画笔",
                Shape::Polygon { .. } => "多边形",
                Shape::Highlighter { .. } => "荧光笔",
                Shape::Mosaic { .. } => "马赛克",
                Shape::Text { .. } => "文字",
            };
            format!("已选 · {kind}")
        } else {
            tool_name.to_string()
        };
        let sv_width = self
            .sv_bounds
            .lock()
            .ok()
            .and_then(|g| *g)
            .map(|b| b.size.width.as_f32())
            .unwrap_or(194.0);
        let hue_width = self
            .hue_bounds
            .lock()
            .ok()
            .and_then(|g| *g)
            .map(|b| b.size.width.as_f32())
            .unwrap_or(194.0);
        let (supports_color, supports_fill, _supports_stroke) = self.active_color_mode_flags();
        let target_is_stroke = self.effective_target_stroke();
        let active_opt_color = self.active_target_color();
        let can_clear_target = if self.is_text_color_context() {
            target_is_stroke && self.text_stroke_color.is_some()
        } else if target_is_stroke {
            self.anno_has_stroke
                && (selected_list.is_empty()
                    && matches!(self.tool, AnnoTool::Rect | AnnoTool::Ellipse)
                    || selected_list.iter().any(|&i| {
                        matches!(
                            self.shapes.get(i),
                            Some(
                                Shape::Rect { .. } | Shape::Ellipse { .. } | Shape::Polygon { .. }
                            )
                        )
                    }))
        } else {
            self.anno_fill.is_some()
        };
        let show_rect_radius = self.tool == AnnoTool::Rect
            || selected_list
                .iter()
                .any(|&i| matches!(self.shapes.get(i), Some(Shape::Rect { .. })));
        let hue = self.picker_hue;
        let sat = self.picker_sat;
        let val = self.picker_val;
        let sv_bounds_ref = self.sv_bounds.clone();
        let hue_bounds_ref = self.hue_bounds.clone();

        let mut panel = div()
            .id("viewer-right-inspector")
            .occlude()
            .w(px(244.0))
            .p_3()
            .rounded(px(18.0))
            .bg(vt.panel_bg)
            .border_1()
            .border_color(vt.panel_border)
            .flex()
            .flex_col()
            .gap(px(10.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _ev, _, cx| cx.stop_propagation()),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _ev, _, _cx| {
                    this.sv_dragging = false;
                    this.hue_dragging = false;
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _window, cx| {
                if this.sv_dragging {
                    let sv_b = this.sv_bounds.lock().ok().and_then(|g| *g);
                    if let Some(b) = sv_b {
                        let w = b.size.width.as_f32().max(1.0);
                        let h = b.size.height.as_f32().max(1.0);
                        this.picker_sat =
                            ((ev.position.x.as_f32() - b.origin.x.as_f32()) / w).clamp(0.0, 1.0);
                        this.picker_val = (1.0
                            - (ev.position.y.as_f32() - b.origin.y.as_f32()) / h)
                            .clamp(0.0, 1.0);
                        this.apply_picker_hsv(cx);
                    }
                } else if this.hue_dragging {
                    let hue_b = this.hue_bounds.lock().ok().and_then(|g| *g);
                    if let Some(b) = hue_b {
                        let w = b.size.width.as_f32().max(1.0);
                        let t =
                            ((ev.position.x.as_f32() - b.origin.x.as_f32()) / w).clamp(0.0, 1.0);
                        this.picker_hue = t * 360.0;
                        this.apply_picker_hsv(cx);
                    }
                }
                cx.stop_propagation();
            }))
            .on_scroll_wheel(cx.listener(|_, _ev, _, cx| cx.stop_propagation()))
            // 顶部标题 + 当前选中工具/元素徽章
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_1()
                    .child(div().text_sm().text_color(vt.text_primary).child("属性"))
                    .child(
                        div()
                            .px_2()
                            .py(px(2.0))
                            .rounded_full()
                            .bg(vt.card_bg)
                            .border_1()
                            .border_color(vt.inner_border)
                            .text_xs()
                            .text_color(vt.text_primary)
                            .child(badge_text),
                    ),
            );
        if selected_list.len() >= 2 {
            let align_btn = |id: &'static str, label: &'static str, mode: AlignMode| {
                let v = view.clone();
                div()
                    .id(id)
                    .flex_1()
                    .py(px(4.0))
                    .rounded(px(7.0))
                    .bg(vt.panel_bg)
                    .border_1()
                    .border_color(vt.inner_border)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .text_color(vt.text_primary)
                    .cursor_pointer()
                    .hover(|s| s.bg(vt.hover_bg))
                    .child(label)
                    .on_click(move |_, window, cx| {
                        v.update(cx, |this, cx| {
                            this.handle(ViewerAction::Align(mode), window, cx);
                        });
                    })
            };
            panel = panel.child(
                div()
                    .p_3()
                    .rounded(px(14.0))
                    .bg(vt.card_bg)
                    .border_1()
                    .border_color(vt.inner_border)
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(div().text_xs().text_color(vt.text_secondary).child("对齐"))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .child(align_btn("align-top", "顶对齐", AlignMode::Top))
                            .child(align_btn("align-left", "左对齐", AlignMode::Left))
                            .child(align_btn("align-bottom", "底对齐", AlignMode::Bottom)),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .child(align_btn("align-right", "右对齐", AlignMode::Right))
                            .child(align_btn("align-ch", "水平居中", AlignMode::CenterH))
                            .child(align_btn("align-cv", "垂直居中", AlignMode::CenterV)),
                    ),
            );
        }
        if supports_color {
            panel = panel.child(
                div()
                    .p_3()
                    .rounded(px(14.0))
                    .bg(vt.card_bg)
                    .border_1()
                    .border_color(vt.inner_border)
                    .flex()
                    .flex_col()
                    .gap(px(9.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(div().text_xs().text_color(vt.text_secondary).child("颜色"))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(5.0))
                                    .child({
                                        let v_fill = view.clone();
                                        let v_stroke = view.clone();
                                        let v_clear = view.clone();
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(3.0))
                                            .when(supports_fill, |d| {
                                                d.child(
                                                    div()
                                                        .id("pick-target-fill")
                                                        .px_2()
                                                        .py(px(1.0))
                                                        .rounded_full()
                                                        .text_xs()
                                                        .cursor_pointer()
                                                        .bg(if !target_is_stroke {
                                                            vt.active_pill_bg
                                                        } else {
                                                            rgba(0x00000000)
                                                        })
                                                        .text_color(if !target_is_stroke {
                                                            vt.active_pill_text
                                                        } else {
                                                            vt.text_muted
                                                        })
                                                        .child("填充")
                                                        .on_click(move |_, window, cx| {
                                                            v_fill.update(cx, |this, cx| {
                                                                this.handle(
                                                                    ViewerAction::PickBg(false),
                                                                    window,
                                                                    cx,
                                                                );
                                                            });
                                                        }),
                                                )
                                            })
                                            .child(
                                                div()
                                                    .id("pick-target-stroke")
                                                    .px_2()
                                                    .py(px(1.0))
                                                    .rounded_full()
                                                    .text_xs()
                                                    .cursor_pointer()
                                                    .bg(if target_is_stroke {
                                                        vt.active_pill_bg
                                                    } else {
                                                        rgba(0x00000000)
                                                    })
                                                    .text_color(if target_is_stroke {
                                                        vt.active_pill_text
                                                    } else {
                                                        vt.text_muted
                                                    })
                                                    .child("描边")
                                                    .on_click(move |_, window, cx| {
                                                        v_stroke.update(cx, |this, cx| {
                                                            this.handle(
                                                                ViewerAction::PickBg(true),
                                                                window,
                                                                cx,
                                                            );
                                                        });
                                                    }),
                                            )
                                            .when(can_clear_target, |d| {
                                                d.child(
                                                    div()
                                                        .id("pick-target-clear")
                                                        .px(px(5.0))
                                                        .py(px(1.0))
                                                        .rounded_full()
                                                        .text_xs()
                                                        .cursor_pointer()
                                                        .bg(vt.hover_bg)
                                                        .text_color(vt.text_secondary)
                                                        .child("无")
                                                        .on_click(move |_, window, cx| {
                                                            v_clear.update(cx, |this, cx| {
                                                                this.handle(
                                                                    ViewerAction::ClearOptionalColor,
                                                                    window,
                                                                    cx,
                                                                );
                                                            });
                                                        }),
                                                )
                                            })
                                    })
                                    .child(
                                        div()
                                            .w(px(13.0))
                                            .h(px(13.0))
                                            .rounded(px(4.0))
                                            .border_1()
                                            .border_color(vt.panel_border)
                                            .bg(if let Some(c) = active_opt_color {
                                                c.to_rgpui()
                                            } else {
                                                rgba(0x00000000)
                                            }),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(vt.text_primary)
                                            .child(
                                                active_opt_color
                                                    .map(|c| c.to_hex())
                                                    .unwrap_or_else(|| "无".to_string()),
                                            ),
                                    ),
                            ),
                    )
                    // 圆角方形 SV 取色区
                    .child(
                        div()
                            .id("viewer-sv-box")
                            .relative()
                            .w_full()
                            .h(px(116.0))
                            .rounded(px(10.0))
                            .overflow_hidden()
                            .border_1()
                            .border_color(vt.inner_border)
                            .cursor(CursorStyle::Crosshair)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, ev: &MouseDownEvent, _window, cx| {
                                    cx.stop_propagation();
                                    this.sv_dragging = true;
                                    let sv_b = this.sv_bounds.lock().ok().and_then(|g| *g);
                                    if let Some(b) = sv_b {
                                        let w = b.size.width.as_f32().max(1.0);
                                        let h = b.size.height.as_f32().max(1.0);
                                        this.picker_sat =
                                            ((ev.position.x.as_f32() - b.origin.x.as_f32()) / w)
                                                .clamp(0.0, 1.0);
                                        this.picker_val = (1.0
                                            - (ev.position.y.as_f32() - b.origin.y.as_f32()) / h)
                                            .clamp(0.0, 1.0);
                                        this.apply_picker_hsv(cx);
                                    }
                                }),
                            )
                            .child(
                                canvas(
                                    move |bounds, _, _| bounds,
                                    move |_bounds, paint_bounds, window, _| {
                                        if let Ok(mut guard) = sv_bounds_ref.lock() {
                                            *guard = Some(paint_bounds);
                                        }
                                        let cols = 96usize;
                                        let rows = 58usize;
                                        let cw = paint_bounds.size.width.as_f32() / cols as f32;
                                        let ch = paint_bounds.size.height.as_f32() / rows as f32;
                                        let ox = paint_bounds.origin.x.as_f32();
                                        let oy = paint_bounds.origin.y.as_f32();
                                        for r in 0..rows {
                                            let v = 1.0 - (r as f32 + 0.5) / rows as f32;
                                            for c in 0..cols {
                                                let s = (c as f32 + 0.5) / cols as f32;
                                                let cell = Bounds {
                                                    origin: point(
                                                        px(ox + c as f32 * cw),
                                                        px(oy + r as f32 * ch),
                                                    ),
                                                    size: size(px(cw + 0.6), px(ch + 0.6)),
                                                };
                                                window.paint_quad(fill(
                                                    cell,
                                                    hsv_to_anno(hue, s, v).to_rgpui(),
                                                ));
                                            }
                                        }
                                    },
                                )
                                .size_full(),
                            )
                            // SV 准星圆圈
                            .child(
                                div()
                                    .absolute()
                                    .left(px((sat * sv_width - 6.0)
                                        .clamp(0.0, (sv_width - 12.0).max(0.0))))
                                    .top(px(((1.0 - val) * 116.0 - 6.0).clamp(0.0, 104.0)))
                                    .w(px(12.0))
                                    .h(px(12.0))
                                    .rounded_full()
                                    .border_2()
                                    .border_color(rgb(0xFFFFFF)),
                            ),
                    )
                    // 下方横向圆角色相条
                    .child(
                        div()
                            .id("viewer-hue-bar")
                            .relative()
                            .w_full()
                            .h(px(12.0))
                            .rounded_full()
                            .overflow_hidden()
                            .border_1()
                            .border_color(vt.inner_border)
                            .cursor(CursorStyle::PointingHand)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, ev: &MouseDownEvent, _window, cx| {
                                    cx.stop_propagation();
                                    this.hue_dragging = true;
                                    let hue_b = this.hue_bounds.lock().ok().and_then(|g| *g);
                                    if let Some(b) = hue_b {
                                        let w = b.size.width.as_f32().max(1.0);
                                        let t = ((ev.position.x.as_f32() - b.origin.x.as_f32())
                                            / w)
                                            .clamp(0.0, 1.0);
                                        this.picker_hue = t * 360.0;
                                        this.apply_picker_hsv(cx);
                                    }
                                }),
                            )
                            .child(
                                canvas(
                                    move |bounds, _, _| bounds,
                                    move |_bounds, paint_bounds, window, _| {
                                        if let Ok(mut guard) = hue_bounds_ref.lock() {
                                            *guard = Some(paint_bounds);
                                        }
                                        let segs = 100usize;
                                        let sw = paint_bounds.size.width.as_f32() / segs as f32;
                                        let ox = paint_bounds.origin.x.as_f32();
                                        let oy = paint_bounds.origin.y.as_f32();
                                        let h = paint_bounds.size.height;
                                        for i in 0..segs {
                                            let hh = (i as f32 + 0.5) / segs as f32 * 360.0;
                                            let seg = Bounds {
                                                origin: point(px(ox + i as f32 * sw), px(oy)),
                                                size: size(px(sw + 0.6), h),
                                            };
                                            window.paint_quad(fill(
                                                seg,
                                                hsv_to_anno(hh, 1.0, 1.0).to_rgpui(),
                                            ));
                                        }
                                    },
                                )
                                .size_full(),
                            )
                            // 色相滑块指示圆点
                            .child(
                                div()
                                    .absolute()
                                    .left(px(((hue / 360.0) * hue_width - 6.0)
                                        .clamp(0.0, (hue_width - 12.0).max(0.0))))
                                    .top(px(0.0))
                                    .w(px(12.0))
                                    .h(px(12.0))
                                    .rounded_full()
                                    .bg(rgb(0xFFFFFF))
                                    .border_1()
                                    .border_color(rgba(0x00000055)),
                            ),
                    )
                    // 6 快捷预设色圆点
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(self.color_swatch(cx, AnnoColor::RED))
                            .child(self.color_swatch(cx, AnnoColor::YELLOW))
                            .child(self.color_swatch(cx, AnnoColor::GREEN))
                            .child(self.color_swatch(cx, AnnoColor::BLUE))
                            .child(self.color_swatch(cx, AnnoColor::WHITE))
                            .child(self.color_swatch(cx, AnnoColor::BLACK)),
                    ),
            );
            // 2. 描边粗细卡片（马赛克时不显示）
            panel = panel.child(
                div()
                    .p_3()
                    .rounded(px(14.0))
                    .bg(vt.card_bg)
                    .border_1()
                    .border_color(vt.inner_border)
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(vt.text_secondary)
                                    .child(tr("viewer.line_width")),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(vt.text_primary)
                                    .child(format!("{} px", self.width.round() as u32)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(self.step_pill(
                                cx,
                                "w-down",
                                "−",
                                ViewerAction::WidthNudge(-1.0),
                            ))
                            .child(div().flex_1().child(Slider::new(&self.width_slider)))
                            .child(self.step_pill(cx, "w-up", "+", ViewerAction::WidthNudge(1.0))),
                    ),
            );
        }
        if show_rect_radius {
            panel = panel.child(
                div()
                    .p_3()
                    .rounded(px(14.0))
                    .bg(vt.card_bg)
                    .border_1()
                    .border_color(vt.inner_border)
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(div().text_xs().text_color(vt.text_secondary).child("圆角"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(vt.text_primary)
                                    .child(format!("{} px", self.anno_rect_radius.round() as u32)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(self.step_pill(
                                cx,
                                "rad-down",
                                "−",
                                ViewerAction::RectRadiusNudge(-2.0),
                            ))
                            .child(div().flex_1().child(Slider::new(&self.rect_radius_slider)))
                            .child(self.step_pill(
                                cx,
                                "rad-up",
                                "+",
                                ViewerAction::RectRadiusNudge(2.0),
                            )),
                    ),
            );
        }
        if self.tool == AnnoTool::Text
            || self.text_editing.is_some()
            || selected_list
                .iter()
                .any(|&i| matches!(self.shapes.get(i), Some(Shape::Text { .. })))
        {
            panel = panel.child(
                div()
                    .p_3()
                    .rounded(px(14.0))
                    .bg(vt.card_bg)
                    .border_1()
                    .border_color(vt.inner_border)
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .w(px(14.0))
                                            .h(px(14.0))
                                            .rounded(px(4.0))
                                            .bg(self.text_bg_color.as_text_bg_rgpui())
                                            .border_1()
                                            .border_color(vt.panel_border),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(vt.text_primary)
                                            .child(tr("viewer.text_bg")),
                                    ),
                            )
                            .child({
                                let view_sw = view.clone();
                                let on = self.text_bg;
                                div()
                                    .id("viewer-text-bg-switch")
                                    .w(px(36.0))
                                    .h(px(20.0))
                                    .p(px(2.0))
                                    .rounded_full()
                                    .cursor_pointer()
                                    .bg(if on { vt.accent } else { vt.hover_bg })
                                    .flex()
                                    .items_center()
                                    .justify_end()
                                    .when(!on, |d| d.justify_start())
                                    .child(
                                        div()
                                            .w(px(16.0))
                                            .h(px(16.0))
                                            .rounded_full()
                                            .bg(rgb(0xFFFFFF)),
                                    )
                                    .on_click(move |_, window, cx| {
                                        view_sw.update(cx, |this, cx| {
                                            this.handle(ViewerAction::TextBg, window, cx);
                                        });
                                    })
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(div().text_xs().text_color(vt.text_secondary).child("字号"))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        div()
                                            .w(px(44.0))
                                            .h(px(22.0))
                                            .px_1()
                                            .rounded(px(6.0))
                                            .bg(vt.hover_bg)
                                            .border_1()
                                            .border_color(vt.inner_border)
                                            .flex()
                                            .items_center()
                                            .child(
                                                Input::new(&self.text_size_input)
                                                    .appearance(false)
                                                    .bordered(false)
                                                    .focus_bordered(false)
                                                    .p_0()
                                                    .text_xs()
                                                    .text_color(vt.text_primary),
                                            ),
                                    )
                                    .child(div().text_xs().text_color(vt.text_muted).child("px")),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(self.step_pill(
                                cx,
                                "ts-down",
                                "−",
                                ViewerAction::TextSizeNudge(-2.0),
                            ))
                            .child(div().flex_1().child(Slider::new(&self.text_size_slider)))
                            .child(self.step_pill(
                                cx,
                                "ts-up",
                                "+",
                                ViewerAction::TextSizeNudge(2.0),
                            )),
                    )
                    .child({
                        let active_font_label = self
                            .text_font
                            .as_deref()
                            .unwrap_or("默认 (微软雅黑)")
                            .to_string();
                        let sys_fonts = crate::export::system_font_list();
                        let view_dd = view.clone();
                        let view_def = view.clone();
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(5.0))
                            .child(div().text_xs().text_color(vt.text_secondary).child("字体"))
                            .child(
                                div()
                                    .id("viewer-font-dropdown-trigger")
                                    .h(px(28.0))
                                    .px_2()
                                    .rounded(px(8.0))
                                    .bg(vt.hover_bg)
                                    .border_1()
                                    .border_color(vt.inner_border)
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .cursor_pointer()
                                    .child(
                                        div()
                                            .flex_1()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_xs()
                                            .text_color(vt.text_primary)
                                            .child(active_font_label),
                                    )
                                    .child(div().text_xs().text_color(vt.text_muted).child(
                                        if self.font_dropdown_open {
                                            "▴"
                                        } else {
                                            "▾"
                                        },
                                    ))
                                    .on_click(move |_, window, cx| {
                                        view_dd.update(cx, |this, cx| {
                                            this.handle(
                                                ViewerAction::ToggleFontDropdown,
                                                window,
                                                cx,
                                            );
                                        });
                                    }),
                            )
                            .when(self.font_dropdown_open, |col| {
                                let mut inner =
                                    div().w_full().flex().flex_col().gap(px(2.0)).child(
                                        div()
                                            .id("viewer-font-opt-default")
                                            .w_full()
                                            .flex_shrink_0()
                                            .px_2()
                                            .py_1()
                                            .rounded(px(6.0))
                                            .text_xs()
                                            .cursor_pointer()
                                            .bg(if self.text_font.is_none() {
                                                vt.active_pill_bg
                                            } else {
                                                rgba(0x00000000)
                                            })
                                            .text_color(if self.text_font.is_none() {
                                                vt.active_pill_text
                                            } else {
                                                vt.text_primary
                                            })
                                            .hover(move |s| s.bg(vt.hover_bg))
                                            .child("默认 (微软雅黑)")
                                            .on_click(move |_, window, cx| {
                                                view_def.update(cx, |this, cx| {
                                                    this.handle(
                                                        ViewerAction::TextFont(None),
                                                        window,
                                                        cx,
                                                    );
                                                });
                                            }),
                                    );
                                for (idx, (name, _)) in sys_fonts.iter().enumerate() {
                                    let is_sel = self.text_font.as_deref() == Some(name.as_str());
                                    let fam = name.clone();
                                    let view_item = view.clone();
                                    inner = inner.child(
                                        div()
                                            .id(format!("viewer-font-opt-{idx}"))
                                            .w_full()
                                            .flex_shrink_0()
                                            .px_2()
                                            .py_1()
                                            .rounded(px(6.0))
                                            .text_xs()
                                            .whitespace_nowrap()
                                            .overflow_hidden()
                                            .cursor_pointer()
                                            .bg(if is_sel {
                                                vt.active_pill_bg
                                            } else {
                                                rgba(0x00000000)
                                            })
                                            .text_color(if is_sel {
                                                vt.active_pill_text
                                            } else {
                                                vt.text_primary
                                            })
                                            .hover(move |s| s.bg(vt.hover_bg))
                                            .child(name.clone())
                                            .on_click(move |_, window, cx| {
                                                view_item.update(cx, |this, cx| {
                                                    this.handle(
                                                        ViewerAction::TextFont(Some(fam.clone())),
                                                        window,
                                                        cx,
                                                    );
                                                });
                                            }),
                                    );
                                }
                                col.child(
                                    div()
                                        .id("viewer-font-dropdown-list")
                                        .w_full()
                                        .h(px(170.0))
                                        .overflow_y_scroll()
                                        .p_1()
                                        .rounded(px(10.0))
                                        .bg(vt.panel_bg)
                                        .border_1()
                                        .border_color(vt.panel_border)
                                        .child(inner),
                                )
                            })
                    }),
            );
        }
        // 4. 画布缩放卡片
        panel = panel.child(
            div()
                .p_3()
                .rounded(px(14.0))
                .bg(vt.card_bg)
                .border_1()
                .border_color(vt.inner_border)
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_xs()
                                .text_color(vt.text_secondary)
                                .child("画布视图"),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(vt.text_primary)
                                .child(percent.to_string()),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(self.step_pill(cx, "z-out", "−", ViewerAction::ZoomOut))
                        .child({
                            let view_fit = view.clone();
                            div()
                                .id("viewer-fit-pill")
                                .flex_1()
                                .h(px(26.0))
                                .rounded_full()
                                .bg(vt.hover_bg)
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_xs()
                                .text_color(vt.text_primary)
                                .cursor_pointer()
                                .hover(move |s| {
                                    s.bg(vt.active_pill_bg).text_color(vt.active_pill_text)
                                })
                                .child(tr("viewer.fit"))
                                .on_click(move |_, window, cx| {
                                    view_fit.update(cx, |this, cx| {
                                        this.handle(ViewerAction::Fit, window, cx);
                                    });
                                })
                        })
                        .child(self.step_pill(cx, "z-in", "+", ViewerAction::ZoomIn)),
                ),
        );

        // 保存成功后出现快捷打开入口卡片。
        if self.last_saved.is_some() {
            panel = panel.child(
                div()
                    .p_2()
                    .rounded(px(14.0))
                    .bg(vt.card_bg)
                    .border_1()
                    .border_color(vt.inner_border)
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(self.capsule_btn(
                        cx,
                        "open-saved-file",
                        tr("viewer.open_file"),
                        ViewerAction::OpenSavedFile,
                    ))
                    .child(self.capsule_btn(
                        cx,
                        "open-saved-dir",
                        tr("viewer.open_dir"),
                        ViewerAction::OpenSavedDir,
                    )),
            );
        }

        // 外层居中容器不拦截空白处鼠标事件。
        div()
            .absolute()
            .right(px(12.0))
            .top(px(64.0))
            .bottom(px(12.0))
            .flex()
            .flex_col()
            .justify_center()
            .child(panel)
    }

    /// 渲染左下角 3 秒渐隐状态提示胶囊（前 2.4 秒完全不透明，后 0.6 秒渐隐至 0）。
    fn render_bottom_status(&self) -> Option<impl IntoElement> {
        let start = self.status_at?;
        let elapsed = start.elapsed().as_secs_f32();
        if elapsed >= 3.0 {
            return None;
        }
        let alpha = if elapsed <= 2.4 {
            1.0
        } else {
            ((3.0 - elapsed) / 0.6).clamp(0.0, 1.0)
        };
        let vt = ViewerTheme::current();
        let fade = |mut c: Rgba| -> Rgba {
            c.a *= alpha;
            c
        };
        Some(
            div()
                .absolute()
                .left(px(12.0))
                .bottom(px(12.0))
                .max_w(px(720.0))
                .whitespace_nowrap()
                .overflow_hidden()
                .px_3()
                .py_1()
                .rounded_full()
                .bg(fade(vt.panel_bg))
                .border_1()
                .border_color(fade(vt.panel_border))
                .text_xs()
                .text_color(fade(vt.text_primary))
                .child(self.status.to_string()),
        )
    }

    /// 圆形小步进按钮（线宽、缩放通用）。
    fn step_pill(
        &self,
        cx: &mut Context<Self>,
        id: &str,
        label: &'static str,
        action: ViewerAction,
    ) -> impl IntoElement {
        let vt = ViewerTheme::current();
        let view = cx.entity();
        div()
            .id(format!("viewer-step-{id}"))
            .w(px(24.0))
            .h(px(24.0))
            .rounded_full()
            .bg(vt.hover_bg)
            .flex()
            .items_center()
            .justify_center()
            .text_xs()
            .text_color(vt.text_primary)
            .cursor_pointer()
            .child(label)
            .on_click(move |_, window, cx| {
                let act = action.clone();
                view.update(cx, |this, cx| this.handle(act, window, cx));
            })
    }
}

#[cfg(test)]
mod tests {
    //! 预览模块单测：橡皮擦命中判定（纯函数，不依赖窗口）。

    use super::*;
    use crate::shapes::PixelRect;

    /// 控制点的缩放方向必须与 Windows 系统光标一致。
    #[test]
    fn transform_cursor_directions() {
        assert_eq!(
            transform_cursor(Some(TransformHandle::Nw)),
            CursorStyle::ResizeUpLeftDownRight
        );
        assert_eq!(
            transform_cursor(Some(TransformHandle::Ne)),
            CursorStyle::ResizeUpRightDownLeft
        );
        assert_eq!(
            transform_cursor(Some(TransformHandle::S)),
            CursorStyle::ResizeUpDown
        );
        assert_eq!(
            transform_cursor(Some(TransformHandle::W)),
            CursorStyle::ResizeLeftRight
        );
        assert_eq!(transform_cursor(None), CursorStyle::Arrow);
    }

    /// 橡皮擦：矩形按外包矩形命中，画笔按笔迹距离命中，未命中返回 None。
    #[test]
    fn erase_hit_cases() {
        let shapes = vec![
            Shape::Rect {
                rect: PixelRect::new(10.0, 10.0, 50.0, 40.0),
                color: AnnoColor::RED,
                width: 3.0,
                fill: None,
                has_stroke: true,
                radius: 0.0,
            },
            Shape::Pen {
                pts: vec![(100.0, 100.0), (200.0, 100.0)],
                color: AnnoColor::BLUE,
                width: 4.0,
            },
        ];
        // 矩形内命中第一项。
        assert_eq!(erase_hit(&shapes, (20.0, 20.0), 8.0), Some(0));
        // 笔迹正上方 5px：半径 8 + 半宽 2 = 10 ≥ 5，命中第二项。
        assert_eq!(erase_hit(&shapes, (150.0, 105.0), 8.0), Some(1));
        // 笔迹上方 20px：超出范围，两项都不命中。
        assert_eq!(erase_hit(&shapes, (150.0, 120.0), 8.0), None);
        // 空白处不命中。
        assert_eq!(erase_hit(&shapes, (300.0, 300.0), 8.0), None);
    }

    /// 明暗双层光标在不同 DPI 下分别保持 3/1 个物理像素的宽度。
    #[test]
    fn caret_has_light_outline_and_dark_core_at_any_dpi() {
        for dpi in [1.0, 1.5, 2.0] {
            let (outer, inner) = caret_layers(
                Bounds::new(point(px(20.25), px(30.0)), size(px(0.0), px(42.0))),
                dpi,
            );
            assert!((outer.size.width.as_f32() * dpi - 3.0).abs() < 0.001);
            assert!((inner.size.width.as_f32() * dpi - 1.0).abs() < 0.001);
            assert!(outer.left() < inner.left() && outer.right() > inner.right());
            assert!(outer.top() < inner.top() && outer.bottom() > inner.bottom());
        }
    }

    /// 变换八个点与框内命中，移动保持尺寸，缩放不穿越对侧形成负数。
    #[test]
    fn transform_handles_resize_and_move() {
        let bounds = PixelRect::new(10.0, 20.0, 100.0, 60.0);
        assert_eq!(
            transform_hit(bounds, (110.0, 80.0), 1.0),
            Some(TransformHandle::Se)
        );
        assert_eq!(
            transform_hit(bounds, (60.0, 50.0), 1.0),
            Some(TransformHandle::Move)
        );
        assert_eq!(transform_hit(bounds, (300.0, 200.0), 1.0), None);
        let moved = resize_bounds(bounds, (5.0, -10.0), TransformHandle::Move, false);
        assert_eq!(
            (moved.x, moved.y, moved.w, moved.h),
            (15.0, 10.0, 100.0, 60.0)
        );
        let resized = resize_bounds(bounds, (100.0, 60.0), TransformHandle::Se, true);
        assert_eq!(
            (resized.x, resized.y, resized.w, resized.h),
            (10.0, 20.0, 200.0, 120.0)
        );
        let smaller = resize_bounds(bounds, (-50.0, 0.0), TransformHandle::Se, true);
        assert_eq!((smaller.w, smaller.h), (50.0, 30.0));
        let limited = resize_bounds(bounds, (500.0, 500.0), TransformHandle::Nw, false);
        assert_eq!((limited.w, limited.h), (2.0, 2.0));
        assert_eq!(
            (limited.x + limited.w, limited.y + limited.h),
            (110.0, 80.0)
        );
    }

    /// 文字缩放真正改变字号，源快照保持不变，可用于 Esc 恢复。
    #[test]
    fn transform_text_preserves_scaled_font_size_and_source() {
        let source = Shape::Text {
            pos: (10.0, 20.0),
            content: "测试".into(),
            color: AnnoColor::RED,
            size: 28.0,
            bg: false,
            bg_color: AnnoColor::WHITE,
            max_w: None,
            font_family: None,
            stroke_color: None,
            stroke_width: 3.0,
        };
        let from = PixelRect::new(10.0, 20.0, 56.0, 42.0);
        let result = transform_shape(
            &source,
            from,
            PixelRect::new(30.0, 40.0, 112.0, 84.0),
            false,
        );
        let Shape::Text {
            pos, size, color, ..
        } = result
        else {
            panic!("Expected text")
        };
        assert_eq!(pos, (30.0, 40.0));
        assert_eq!(size, 56.0);
        assert_eq!(color, AnnoColor::RED);
        assert!(matches!(source, Shape::Text { size: 28.0, .. }));
        let rect = Shape::Rect {
            rect: PixelRect::new(10.0, 20.0, 56.0, 42.0),
            color: AnnoColor::BLUE,
            width: 2.0,
            fill: None,
            has_stroke: true,
            radius: 0.0,
        };
        assert!(matches!(
            transform_shape(&rect, from, PixelRect::new(30.0, 40.0, 112.0, 84.0), false),
            Shape::Rect { width: 4.0, .. }
        ));
    }

    /// 新快捷键逐项覆盖，旧数字键仍可兼容。
    #[test]
    fn named_annotation_shortcuts() {
        for (key, tool) in [
            ("r", AnnoTool::Rect),
            ("l", AnnoTool::Line),
            ("o", AnnoTool::Ellipse),
            ("i", AnnoTool::Eyedropper),
            ("e", AnnoTool::Eraser),
            ("t", AnnoTool::Text),
            ("v", AnnoTool::Select),
            ("b", AnnoTool::Pen),
            ("m", AnnoTool::Mosaic),
        ] {
            assert_eq!(AnnoTool::from_shortcut(key), Some(tool));
        }
    }

    /// 草稿平移保持尺寸，碰边返回实际位移，供起终点同步后继续绘制。
    #[test]
    fn moving_draft_preserves_geometry_and_clamps_edges() {
        let mut shape = Shape::Rect {
            rect: PixelRect::new(10.0, 20.0, 40.0, 30.0),
            color: AnnoColor::RED,
            width: 2.0,
            fill: None,
            has_stroke: true,
            radius: 0.0,
        };
        let original = shape.bbox();
        assert_eq!(
            translate_draft(&mut shape, (30.0, -10.0), (200.0, 100.0)),
            (30.0, -10.0)
        );
        assert_eq!((shape.bbox().w, shape.bbox().h), (original.w, original.h));
        let moved = translate_draft(&mut shape, (-999.0, 999.0), (200.0, 100.0));
        assert_eq!(moved, (-38.0, 58.0));
        assert_eq!(shape.bbox().x, 0.0);
        assert_eq!(shape.bbox().y + shape.bbox().h, 100.0);
        let line = Shape::Line {
            from: (10.0, 10.0),
            to: (50.0, 10.0),
            color: AnnoColor::BLUE,
            width: 2.0,
        };
        assert!(draft_valid(&line));
        assert_eq!(erase_hit(&[line], (30.0, 10.0), 2.0), Some(0));
    }

    /// 已有元素自由移动时不受画布边界限制，可移至负坐标或超出底图范围。
    #[test]
    fn move_shape_allows_moving_outside_canvas() {
        let mut rect = Shape::Rect {
            rect: PixelRect::new(10.0, 20.0, 40.0, 30.0),
            color: AnnoColor::RED,
            width: 2.0,
            fill: None,
            has_stroke: true,
            radius: 0.0,
        };
        let original = rect.bbox();
        move_shape(&mut rect, (-100.0, -50.0));
        assert_eq!(rect.bbox().x, original.x - 100.0);
        assert_eq!(rect.bbox().y, original.y - 50.0);

        move_shape(&mut rect, (400.0, 300.0));
        assert_eq!(rect.bbox().x, original.x + 300.0);
        assert_eq!(rect.bbox().y, original.y + 250.0);
    }

    /// 取色只读原图，边界不会越界采样，也不使用红蓝互换的渲染缓存。
    #[test]
    fn eyedropper_samples_original_rgba() {
        let image = RgbaImage::from_pixel(2, 3, image::Rgba([15, 90, 200, 123]));
        assert_eq!(
            sample_image_color(&image, (1.9, 2.9)),
            Some(AnnoColor(15, 90, 200, 255))
        );
        assert_eq!(sample_image_color(&image, (-0.1, 0.0)), None);
        assert_eq!(sample_image_color(&image, (2.0, 0.0)), None);
    }

    /// 关闭导出烧录真正直线，保持原图不变；生成有效 PNG，包含完整尺寸。
    #[test]
    fn close_export_contains_annotations_without_modifying_source() {
        let base = Arc::new(RgbaImage::from_pixel(
            100,
            80,
            image::Rgba([255, 255, 255, 255]),
        ));
        let shape = Shape::Line {
            from: (10.0, 20.0),
            to: (80.0, 20.0),
            color: AnnoColor::RED,
            width: 2.0,
        };
        let (png, path) =
            annotation_png(Some(base.clone()), std::path::Path::new("unused"), &[shape]).unwrap();
        let output = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(output.dimensions(), (100, 80));
        assert_eq!(output.get_pixel(40, 20).0, [231, 76, 60, 255]);
        assert_eq!(output.get_pixel(78, 15).0, [255, 255, 255, 255]);
        assert_eq!(base.get_pixel(40, 20).0, [255, 255, 255, 255]);
        std::fs::remove_file(path).unwrap();
    }

    /// 橡皮擦：重叠时删最顶层（后画的）。
    #[test]
    fn erase_hit_topmost() {
        let shapes = vec![
            Shape::Rect {
                rect: PixelRect::new(0.0, 0.0, 100.0, 100.0),
                color: AnnoColor::RED,
                width: 3.0,
                fill: None,
                has_stroke: true,
                radius: 0.0,
            },
            Shape::Rect {
                rect: PixelRect::new(50.0, 50.0, 100.0, 100.0),
                color: AnnoColor::BLUE,
                width: 3.0,
                fill: None,
                has_stroke: true,
                radius: 0.0,
            },
        ];
        assert_eq!(erase_hit(&shapes, (75.0, 75.0), 8.0), Some(1));
    }

    /// 固定窗口布局：小截图不放大（居中 100% 显示），窗口保持最小尺寸。
    #[test]
    fn viewer_layout_small_image_keeps_full_size() {
        let (area_w, area_h, fit) = viewer_layout(1920.0, 1080.0, 300, 200);
        // 小图不放大，窗口不随图片缩小。
        assert!((fit - 1.0).abs() < f32::EPSILON);
        assert!(area_w >= MIN_VIEWER_W);
        assert!(area_h >= 200.0);
    }

    /// 固定窗口布局：大截图按比例缩小，完整落入图片区。
    #[test]
    fn viewer_layout_large_image_shrinks_to_fit() {
        let (area_w, area_h, fit) = viewer_layout(1920.0, 1080.0, 3840, 2160);
        assert!(fit < 1.0);
        assert!(3840.0 * fit <= area_w + 1.0);
        assert!(2160.0 * fit <= area_h + 1.0);
    }

    /// 固定窗口布局：极小显示器也不 panic，缩放有下限。
    #[test]
    fn viewer_layout_tiny_display_no_panic() {
        let (area_w, area_h, fit) = viewer_layout(800.0, 600.0, 1920, 1080);
        assert!(area_w > 0.0 && area_h > 0.0);
        assert!(fit >= MIN_ZOOM);
    }

    /// 线宽规范化：钳制到滑杆范围并取整。
    #[test]
    fn normalize_width_cases() {
        assert_eq!(normalize_width(0.2), WIDTH_MIN);
        assert_eq!(normalize_width(4.6), 5.0);
        assert_eq!(normalize_width(99.0), WIDTH_MAX);
    }

    /// 快捷键映射：V 选择、1-8 按标注排顺序，其余返回 None。
    #[test]
    fn tool_shortcut_cases() {
        assert_eq!(AnnoTool::from_shortcut("v"), Some(AnnoTool::Select));
        assert_eq!(AnnoTool::from_shortcut("1"), Some(AnnoTool::Rect));
        assert_eq!(AnnoTool::from_shortcut("8"), Some(AnnoTool::Text));
        assert_eq!(AnnoTool::from_shortcut("9"), None);
        assert_eq!(AnnoTool::from_shortcut("z"), None);
    }

    /// 最近文字索引：取最后一条文字，无文字返回 None（底色开关无文字时只切默认）。
    #[test]
    fn latest_text_index_cases() {
        let shapes = vec![
            Shape::Rect {
                rect: PixelRect::new(0.0, 0.0, 10.0, 10.0),
                color: AnnoColor::RED,
                width: 2.0,
                fill: None,
                has_stroke: true,
                radius: 0.0,
            },
            Shape::Text {
                pos: (0.0, 0.0),
                content: "旧".to_string(),
                color: AnnoColor::WHITE,
                size: 20.0,
                bg: true,
                bg_color: AnnoColor::BLACK,
                max_w: None,
                font_family: None,
                stroke_color: None,
                stroke_width: 3.0,
            },
            Shape::Text {
                pos: (5.0, 5.0),
                content: "新".to_string(),
                color: AnnoColor::BLACK,
                size: 20.0,
                bg: false,
                bg_color: AnnoColor::WHITE,
                max_w: None,
                font_family: None,
                stroke_color: None,
                stroke_width: 3.0,
            },
        ];
        // 取最后一条文字，而不是第一条。
        assert_eq!(latest_text_index(&shapes), Some(2));
        assert_eq!(latest_text_index(&shapes[..1]), None);
        assert_eq!(latest_text_index(&[]), None);
    }
}
