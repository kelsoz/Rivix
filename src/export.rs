//! 导出模块：把选区底图与矢量标注合成为最终 PNG，并负责保存 / 复制。
//!
//! 屏幕看到的标注是矢量绘制的，落盘时必须逐像素栅格化：
//! 直线 / 矩形 / 椭圆 / 箭头用笔刷印章法绘制，马赛克用分块取色，
//! 文字用系统字体的字形位图逐字拼排（依赖 `ab_glyph`，不引入重量级排版库）。

use std::path::{Path, PathBuf};

use ab_glyph::{Font, FontVec, PxScale, ScaleFont as _, point};
use image::{Rgba, RgbaImage};

use crate::{
    i18n::tr,
    shapes::{AnnoColor, PixelRect, Shape},
};
// `tr_fmt` 只出现在 Windows 剪贴板分支里，非 Windows 走独立实现用不到，
// 按平台引入，否则 Linux/macOS 构建会报 unused import（`-D warnings` 下直接失败）。
#[cfg(target_os = "windows")]
use crate::i18n::tr_fmt;

/// 把底图按选区裁剪，并把落在选区内的标注全部烧录，返回最终图片。
///
/// # 参数
/// - `base`: 截屏首帧全图。
/// - `sel`: 选区（全图图片像素坐标），为空时视为全图。
/// - `shapes`: 全部矢量标注（全图图片像素坐标）。
/// - `font`: 系统字体（文字烧录用），缺失时跳过文字。
pub fn render_export(
    base: &RgbaImage,
    sel: PixelRect,
    shapes: &[Shape],
    font: &Option<FontVec>,
) -> RgbaImage {
    // 选区钳制到图片范围内，避免越界 panic。
    let (bw, bh) = (base.width() as f32, base.height() as f32);
    let x0 = sel.x.clamp(0.0, bw.max(1.0)) as u32;
    let y0 = sel.y.clamp(0.0, bh.max(1.0)) as u32;
    let x1 = (sel.x + sel.w).clamp(1.0, bw) as u32;
    let y1 = (sel.y + sel.h).clamp(1.0, bh) as u32;
    let (x1, y1) = (x1.max(x0 + 1), y1.max(y0 + 1));

    // 先裁出底图，后续绘制全部换算到裁剪后的局部坐标。
    let cropped = image::imageops::crop_imm(base, x0, y0, x1 - x0, y1 - y0).to_image();
    let mut out = cropped;
    let clip = PixelRect::new(0.0, 0.0, (x1 - x0) as f32, (y1 - y0) as f32);

    for shape in shapes {
        draw_shape(&mut out, shape, x0 as f32, y0 as f32, &clip, font);
    }
    out
}

/// 把图片编码为 PNG 字节（保存文件与复制剪贴板共用）。
pub fn png_bytes(img: &RgbaImage) -> Option<Vec<u8>> {
    use image::{
        ImageEncoder,
        codecs::png::{CompressionType, FilterType, PngEncoder},
    };
    let mut buf = Vec::new();
    // 截图只需无损像素，快速 PNG 编码减少等待，不克隆整张图片。
    PngEncoder::new_with_quality(&mut buf, CompressionType::Fast, FilterType::Adaptive)
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgba8,
        )
        .ok()?;
    Some(buf)
}

/// 把 PNG 字节写入剪贴板图片项。
///
/// # 参数
/// - `cx`: 应用上下文（`Context` 可解引用为 `App`）。
/// - `png`: PNG 文件字节。
pub fn copy_png_to_clipboard(cx: &mut rgpui::App, png: &[u8]) -> Result<(), String> {
    let image = rgpui::Image::from_bytes(rgpui::ImageFormat::Png, png.to_vec());
    cx.write_to_clipboard(rgpui::ClipboardItem::new_image(&image));
    Ok(())
}

/// 把 PNG 图片字节转换为 Windows DIB 字节（BITMAPINFOHEADER + 自下而上 BGRA）。
///
/// Word / 画图等优先认 CF_DIB，直接给 PNG（`PNG` 注册格式）部分旧程序认不出，
/// 因此双格式复制时同时提供 DIB，兼容性最好。
///
/// 生产代码只在 Windows 剪贴板路径里调用；同时在测试里跨平台编译，
/// 保证 Linux / macOS CI 也能跑 DIB 布局单测（否则非 Windows 构建报 dead_code）。
#[cfg(any(target_os = "windows", test))]
fn png_to_dib_bytes(png: &[u8]) -> Option<Vec<u8>> {
    // 先解码为 RGBA，再逐像素转 BGRA 并上下翻转。
    let img = image::load_from_memory(png).ok()?.into_rgba8();
    rgba_to_dib_bytes(&img)
}

/// 从已有像素生成 DIB，避免截图刚编码成 PNG 又立刻解码。
#[cfg(any(target_os = "windows", test))]
fn rgba_to_dib_bytes(img: &RgbaImage) -> Option<Vec<u8>> {
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return None;
    }
    let mut dib = Vec::with_capacity(40 + (w as usize) * (h as usize) * 4);
    // BITMAPINFOHEADER（40 字节）：宽、高、面数、位深、压缩、图像大小等。
    dib.extend_from_slice(&40u32.to_le_bytes());
    dib.extend_from_slice(&(w as i32).to_le_bytes());
    dib.extend_from_slice(&(h as i32).to_le_bytes());
    dib.extend_from_slice(&1u16.to_le_bytes());
    dib.extend_from_slice(&32u16.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes());
    dib.extend_from_slice(&(w * h * 4).to_le_bytes());
    dib.extend_from_slice(&0i32.to_le_bytes());
    dib.extend_from_slice(&0i32.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes());
    // 像素区：DIB 要求自下而上存储，通道顺序为 BGRA。
    for y in (0..h).rev() {
        for x in 0..w {
            let p = img.get_pixel(x, y);
            dib.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    Some(dib)
}

/// 按 Windows DROPFILES 结构拼出 CF_HDROP 字节。
///
/// 布局：pFiles(4) + pt.x(4) + pt.y(4) + fNC(4) + fWide(4) = 20 字节头，
/// 之后是 UTF-16 文件路径（单 `\0` 分隔，末尾双 `\0` 结束）。
///
/// 与 [`png_to_dib_bytes`] 同理：生产只在 Windows 用，测试里跨平台编译。
#[cfg(any(target_os = "windows", test))]
fn hdrop_bytes(path: &Path) -> Option<Vec<u8>> {
    // 路径转 UTF-16（Windows 用系统编码，非 Windows 用 lossy 兜底，单元测试可跑）。
    #[cfg(target_os = "windows")]
    let wide: Vec<u16> = {
        use std::os::windows::ffi::OsStrExt as _;
        path.as_os_str().encode_wide().chain([0, 0]).collect()
    };
    #[cfg(not(target_os = "windows"))]
    let wide: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain([0, 0])
        .collect();
    if wide.len() < 2 {
        return None;
    }
    let mut buf = Vec::with_capacity(20 + wide.len() * 2);
    buf.extend_from_slice(&20u32.to_le_bytes());
    buf.extend_from_slice(&0i32.to_le_bytes());
    buf.extend_from_slice(&0i32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    for unit in wide {
        buf.extend_from_slice(&unit.to_le_bytes());
    }
    Some(buf)
}

/// 往剪贴板写字节，成功移交系统所有权，失败释放内存。
#[cfg(target_os = "windows")]
fn set_clipboard_bytes(data: &[u8], format: u32) -> Result<(), String> {
    use windows::Win32::{
        // windows 0.62 把 GlobalFree 放在 Foundation，分配 / 加锁仍在 System::Memory。
        Foundation::{GlobalFree, HANDLE},
        System::{
            DataExchange::SetClipboardData,
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
        },
    };
    if data.is_empty() {
        return Err(tr("export.empty_clip"));
    }
    unsafe {
        let global = GlobalAlloc(GMEM_MOVEABLE, data.len())
            .map_err(|e| tr_fmt("export.alloc_fail", &[("e", &format!("{e:?}"))]))?;
        let ptr = GlobalLock(global);
        if ptr.is_null() {
            // 加锁失败不会再有人接管这块内存，就地释放，避免泄漏。
            let _ = GlobalFree(Some(global));
            return Err(tr("export.lock_fail"));
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr as *mut u8, data.len());
        let _ = GlobalUnlock(global);
        if let Err(error) = SetClipboardData(format, Some(HANDLE(global.0))) {
            // 写入失败时内存仍归本进程，必须显式释放。
            let _ = GlobalFree(Some(global));
            return Err(tr_fmt("export.write_fail", &[("e", &format!("{error:?}"))]));
        }
        // HGLOBAL 是纯句柄（Copy 类型，无 RAII）：成功后内存归系统所有，
        // 此处不再释放，函数返回即完成所有权移交。
        Ok(())
    }
}

/// 把 PNG 图片与图片文件同时写入剪贴板（Windows）：Word 等粘贴为图片，资源管理器粘贴为文件。
///
/// rgpui 内建 `write_to_clipboard` 在 Windows 后端会忽略文件条目，
/// 一次只能二选一；此处直接调系统剪贴板 API，在同一次 `EmptyClipboard` 会话里
/// 同时写入 PNG / DIB / HDROP 三种格式，两类粘贴目标各取所需。
#[cfg(target_os = "windows")]
pub fn copy_image_and_file_to_clipboard(png: &[u8], path: &Path) -> Result<(), String> {
    write_image_clipboard(png, path, png_to_dib_bytes(png))
}

/// 截图专用复制入口，复用内存中的 RGBA，保留 PNG、DIB 与文件三种格式。
#[cfg(target_os = "windows")]
pub fn copy_capture_to_clipboard(png: &[u8], img: &RgbaImage, path: &Path) -> Result<(), String> {
    write_image_clipboard(png, path, rgba_to_dib_bytes(img))
}

/// 同一剪贴板会话写入所有格式，沿用原有重试和内存所有权处理。
#[cfg(target_os = "windows")]
fn write_image_clipboard(png: &[u8], path: &Path, dib: Option<Vec<u8>>) -> Result<(), String> {
    use windows::Win32::System::{
        DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW},
        Ole::{CF_DIB, CF_HDROP},
    };

    if png.is_empty() {
        return Err(tr("export.empty_image"));
    }
    if !path.is_file() {
        return Err(tr("export.no_clip_file"));
    }
    // DIB 可选：生成失败仍可继续（PNG 格式多数程序也认），HDROP 必须成功。
    let hdrop = hdrop_bytes(path).ok_or_else(|| tr("export.hdrop_fail"))?;

    // 剪贴板可能被其他程序短暂占用，重试几次。
    let mut opened = false;
    for _ in 0..8 {
        if unsafe { OpenClipboard(None) }.is_ok() {
            opened = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if !opened {
        return Err(tr("export.open_fail"));
    }
    // 无论成功失败都要关闭剪贴板，用闭包包裹保证 CloseClipboard 一定执行。
    let result = (|| {
        unsafe {
            EmptyClipboard().map_err(|e| tr_fmt("export.empty_fail", &[("e", &format!("{e:?}"))]))
        }?;
        // 注册的 `PNG` 格式：Word / 浏览器认它，比 DIB 保真。
        let png_format = unsafe { RegisterClipboardFormatW(windows::core::w!("PNG")) };
        if png_format == 0 {
            return Err(tr("export.reg_fail"));
        }
        set_clipboard_bytes(png, png_format)?;
        // DIB：画图等旧程序认它，失败不阻断（PNG 已写入）。
        if let Some(dib) = &dib {
            let _ = set_clipboard_bytes(dib, CF_DIB.0 as u32);
        }
        // HDROP：资源管理器认它，直接粘贴为文件。
        set_clipboard_bytes(&hdrop, CF_HDROP.0 as u32)?;
        Ok(())
    })();
    let _ = unsafe { CloseClipboard() };
    if result.is_ok() {
        record_self_clipboard_seq();
    }
    result
}

/// 把 PNG 图片与图片文件同时写入剪贴板，非 Windows 平台暂时回退为不支持。
#[cfg(not(target_os = "windows"))]
pub fn copy_image_and_file_to_clipboard(_png: &[u8], _path: &Path) -> Result<(), String> {
    Err(tr("export.unsupported"))
}

/// 非 Windows 复用既有格式复制与回退行为。
#[cfg(not(target_os = "windows"))]
pub fn copy_capture_to_clipboard(png: &[u8], _img: &RgbaImage, path: &Path) -> Result<(), String> {
    copy_image_and_file_to_clipboard(png, path)
}

/// 把图片文件作为“文件项”写入系统剪贴板，便于在资源管理器目录中直接粘贴。
///
/// # 参数
/// - `path`: 已存在的 PNG 文件路径。
#[cfg(target_os = "windows")]
pub fn copy_file_to_clipboard(path: &Path) -> Result<(), String> {
    use windows::Win32::System::{
        DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard},
        Ole::CF_HDROP,
    };
    if !path.is_file() {
        return Err(tr("export.no_clip_file"));
    }
    let data = hdrop_bytes(path).ok_or_else(|| tr("export.hdrop_fail"))?;
    let mut opened = false;
    for _ in 0..8 {
        if unsafe { OpenClipboard(None) }.is_ok() {
            opened = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if !opened {
        return Err(tr("export.open_fail"));
    }
    let result = (|| {
        unsafe { EmptyClipboard() }
            .map_err(|e| tr_fmt("export.empty_fail", &[("e", &format!("{e:?}"))]))?;
        set_clipboard_bytes(&data, CF_HDROP.0 as u32)
    })();
    let _ = unsafe { CloseClipboard() };
    if result.is_ok() {
        record_self_clipboard_seq();
    }
    result
}

/// 把图片文件作为“文件项”写入系统剪贴板，非 Windows 平台暂时回退为不支持。
///
/// # 参数
/// - `path`: 已存在的 PNG 文件路径。
#[cfg(not(target_os = "windows"))]
pub fn copy_file_to_clipboard(_path: &Path) -> Result<(), String> {
    Err(tr("export.unsupported"))
}

/// 记录本进程最近一次写入系统剪贴板的序列号，避免把自身刚复制的截图重复当成外部复制图。
#[cfg(target_os = "windows")]
static LAST_SELF_CLIPBOARD_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[cfg(target_os = "windows")]
fn record_self_clipboard_seq() {
    use std::sync::atomic::Ordering;
    use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;
    let seq = unsafe { GetClipboardSequenceNumber() };
    LAST_SELF_CLIPBOARD_SEQ.store(seq, Ordering::Relaxed);
}

/// 从系统剪贴板读取用户复制的图片文件（`CF_HDROP`）或图像数据（`PNG` / `CF_DIB`）。
///
/// - `force = false`：仅当剪贴板序列号不同于本进程最近写入时才读取，避免覆盖刚截完的预览状态。
/// - `force = true`：无视序列号直接尝试读取当前剪贴板中的图片（如 `Ctrl+V` 或无最近记录时）。
#[cfg(target_os = "windows")]
pub fn read_image_from_clipboard(force: bool) -> Option<PathBuf> {
    use std::sync::atomic::Ordering;
    use windows::Win32::{
        Foundation::HGLOBAL,
        System::{
            DataExchange::{
                CloseClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard,
                RegisterClipboardFormatW,
            },
            Memory::{GlobalLock, GlobalSize, GlobalUnlock},
            Ole::{CF_DIB, CF_HDROP},
        },
    };

    let seq = unsafe { GetClipboardSequenceNumber() };
    if !force && seq != 0 && seq == LAST_SELF_CLIPBOARD_SEQ.load(Ordering::Relaxed) {
        return None;
    }

    if unsafe { OpenClipboard(None) }.is_err() {
        return None;
    }

    // 读取指定剪贴板格式的原始字节，确保锁在返回前解锁。
    let read_format = |fmt: u32| -> Option<Vec<u8>> {
        let handle = unsafe { GetClipboardData(fmt) }.ok()?;
        let hglobal = HGLOBAL(handle.0);
        let size = unsafe { GlobalSize(hglobal) };
        if size == 0 {
            return None;
        }
        let ptr = unsafe { GlobalLock(hglobal) };
        if ptr.is_null() {
            return None;
        }
        let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, size) };
        let bytes = slice.to_vec();
        let _ = unsafe { GlobalUnlock(hglobal) };
        Some(bytes)
    };

    let hdrop_bytes = read_format(CF_HDROP.0 as u32);
    let png_fmt = unsafe { RegisterClipboardFormatW(windows::core::w!("PNG")) };
    let png_bytes_data = (png_fmt != 0).then(|| read_format(png_fmt)).flatten();
    let dib_bytes = read_format(CF_DIB.0 as u32);

    let _ = unsafe { CloseClipboard() };

    // 1. 优先识别资源管理器复制的图片文件（CF_HDROP）。
    if let Some(bytes) = hdrop_bytes
        && let Some(path) = parse_hdrop_first_path(&bytes)
        && crate::viewer::preview_from_path(&path).is_some()
    {
        return Some(path);
    }

    // 2. 识别剪贴板原始 PNG 数据。
    if let Some(bytes) = png_bytes_data
        && let Ok(img) = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
    {
        return write_clipboard_temp_png(seq, &img.to_rgba8());
    }

    // 3. 识别传统位图 DIB（24 / 32 bpp）。
    if let Some(bytes) = dib_bytes
        && let Some(rgba) = decode_dib_to_rgba(&bytes)
    {
        return write_clipboard_temp_png(seq, &rgba);
    }

    None
}

/// 非 Windows 平台暂不从系统剪贴板读取图片文件。
#[cfg(not(target_os = "windows"))]
pub fn read_image_from_clipboard(_force: bool) -> Option<PathBuf> {
    None
}

/// 解析 `DROPFILES` 结构并提取第一个文件路径（支持 UTF-16LE 与本地单字节）。
#[cfg(target_os = "windows")]
fn parse_hdrop_first_path(bytes: &[u8]) -> Option<PathBuf> {
    if bytes.len() < 20 {
        return None;
    }
    let offset = u32::from_le_bytes(bytes[0..4].try_into().ok()?) as usize;
    let f_wide = u32::from_le_bytes(bytes[16..20].try_into().ok()?) != 0;
    if offset >= bytes.len() {
        return None;
    }
    let payload = &bytes[offset..];
    if f_wide {
        let mut units = Vec::new();
        for chunk in payload.as_chunks::<2>().0 {
            let u = u16::from_le_bytes([chunk[0], chunk[1]]);
            if u == 0 {
                break;
            }
            units.push(u);
        }
        if units.is_empty() {
            return None;
        }
        Some(PathBuf::from(String::from_utf16_lossy(&units)))
    } else {
        let end = payload
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(payload.len());
        if end == 0 {
            return None;
        }
        Some(PathBuf::from(
            String::from_utf8_lossy(&payload[..end]).into_owned(),
        ))
    }
}

/// 将常见 24 / 32 位无压缩 `CF_DIB` 解码为 RGBA 图像。
#[cfg(target_os = "windows")]
fn decode_dib_to_rgba(bytes: &[u8]) -> Option<RgbaImage> {
    if bytes.len() < 40 {
        return None;
    }
    let header_size = u32::from_le_bytes(bytes[0..4].try_into().ok()?) as usize;
    if header_size < 40 || bytes.len() < header_size {
        return None;
    }
    let w = i32::from_le_bytes(bytes[4..8].try_into().ok()?);
    let h_raw = i32::from_le_bytes(bytes[8..12].try_into().ok()?);
    let bpp = u16::from_le_bytes(bytes[14..16].try_into().ok()?);
    let compression = u32::from_le_bytes(bytes[16..20].try_into().ok()?);
    if w <= 0 || h_raw == 0 || (compression != 0 && compression != 3) || (bpp != 24 && bpp != 32) {
        return None;
    }
    let w_u = w as u32;
    let h_u = h_raw.unsigned_abs();
    let top_down = h_raw < 0;
    let mask_extra = if compression == 3 && header_size == 40 {
        12
    } else {
        0
    };
    let pixel_offset = header_size + mask_extra;
    let stride = (w_u as usize * bpp as usize).div_ceil(32) * 4;
    if bytes.len() < pixel_offset + stride * h_u as usize {
        return None;
    }
    let pixels = &bytes[pixel_offset..];
    let mut out = RgbaImage::new(w_u, h_u);
    for y in 0..h_u {
        let src_y = if top_down { y } else { h_u - 1 - y };
        let row = &pixels[src_y as usize * stride..];
        for x in 0..w_u {
            match bpp {
                32 => {
                    let i = x as usize * 4;
                    out.put_pixel(x, y, image::Rgba([row[i + 2], row[i + 1], row[i], 255]));
                }
                24 => {
                    let i = x as usize * 3;
                    out.put_pixel(x, y, image::Rgba([row[i + 2], row[i + 1], row[i], 255]));
                }
                _ => return None,
            }
        }
    }
    Some(out)
}

/// 将剪贴板图像落盘为临时 PNG 供预览标注窗口加载。
#[cfg(target_os = "windows")]
fn write_clipboard_temp_png(seq: u32, rgba: &RgbaImage) -> Option<PathBuf> {
    let path = std::env::temp_dir().join(format!("rivix-clip-{seq}.png"));
    rgba.save_with_format(&path, image::ImageFormat::Png).ok()?;
    Some(path)
}

/// 加载系统字体（供文字烧录），按平台常见中文字体依次尝试。
///
/// # 返回
/// 成功返回字体，全部缺失返回 `None`（此时导出跳过文字标注）。
pub fn load_system_font() -> Option<FontVec> {
    // Windows：微软雅黑优先（Win10/11 自带，中文覆盖齐全），宋黑备用，英文保底。
    #[cfg(target_os = "windows")]
    const CANDIDATES: &[&str] = &[
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\simsun.ttc",
        "C:\\Windows\\Fonts\\simhei.ttf",
        "C:\\Windows\\Fonts\\segoeui.ttf",
    ];
    // macOS：苹方为系统默认中文字体，黑体与 Helvetica 依次兜底。
    #[cfg(target_os = "macos")]
    const CANDIDATES: &[&str] = &[
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/System/Library/Fonts/Helvetica.ttc",
    ];
    // Linux：Noto CJK 是发行版最常见的预装中文字体，文泉驿与 DejaVu 兜底。
    #[cfg(all(unix, not(target_os = "macos")))]
    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ];
    for path in CANDIDATES {
        if let Ok(data) = std::fs::read(path) {
            // ttc 是字体集合，按索引 0 取第一个；ttf 同样兼容该接口。
            if let Ok(font) = FontVec::try_from_vec_and_index(data, 0) {
                return Some(font);
            }
        }
    }
    None
}

/// 枚举当前系统已安装的常规字体列表（名称 + 字体文件路径），只扫描一次并缓存。
pub fn system_font_list() -> &'static [(String, PathBuf)] {
    use std::sync::OnceLock;
    static FONTS: OnceLock<Vec<(String, PathBuf)>> = OnceLock::new();
    FONTS.get_or_init(|| {
        #[cfg(target_os = "windows")]
        {
            collect_windows_fonts()
        }
        #[cfg(not(target_os = "windows"))]
        {
            Vec::new()
        }
    })
}

/// 扫描 Windows 注册表（系统级 + 当前用户级）中的常规 TrueType/OpenType 字体。
#[cfg(target_os = "windows")]
fn collect_windows_fonts() -> Vec<(String, PathBuf)> {
    use std::collections::BTreeMap;
    use windows::{
        Win32::{
            Foundation::ERROR_NO_MORE_ITEMS,
            System::Registry::{
                HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ, RegCloseKey,
                RegEnumValueW, RegOpenKeyExW,
            },
        },
        core::w,
    };

    let mut map: BTreeMap<String, PathBuf> = BTreeMap::new();
    let fonts_dir = PathBuf::from("C:\\Windows\\Fonts");

    let mut read_hive = |root: HKEY| {
        let mut hkey = HKEY::default();
        let subkey = w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Fonts");
        if unsafe { RegOpenKeyExW(root, subkey, Some(0), KEY_READ, &mut hkey) }.is_err() {
            return;
        }
        let mut idx = 0u32;
        loop {
            let mut name_buf = [0u16; 260];
            let mut name_len = name_buf.len() as u32;
            let mut kind = 0u32;
            let mut data_buf = [0u8; 520];
            let mut data_len = data_buf.len() as u32;
            let status = unsafe {
                RegEnumValueW(
                    hkey,
                    idx,
                    Some(windows::core::PWSTR(name_buf.as_mut_ptr())),
                    &mut name_len,
                    None,
                    Some(&mut kind),
                    Some(data_buf.as_mut_ptr()),
                    Some(&mut data_len),
                )
            };
            idx += 1;
            if status == ERROR_NO_MORE_ITEMS {
                break;
            }
            if status.is_err() || kind != REG_SZ.0 {
                continue;
            }
            let raw_name = String::from_utf16_lossy(&name_buf[..name_len as usize]);
            // 仅保留 TrueType / OpenType 字体，过滤 Bold/Italic 变体以免列表过长。
            if !(raw_name.contains("(TrueType)") || raw_name.contains("(OpenType)")) {
                continue;
            }
            let clean = raw_name
                .replace(" (TrueType)", "")
                .replace(" (OpenType)", "");
            let primary = clean.split(" & ").next().unwrap_or(&clean).trim();
            if primary.is_empty()
                || primary.ends_with(" Bold")
                || primary.ends_with(" Italic")
                || primary.ends_with(" Light")
                || primary.ends_with(" Semibold")
            {
                continue;
            }
            let u16_slice: Vec<u16> = data_buf[..data_len as usize]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|&u| u != 0)
                .collect();
            let raw_path = String::from_utf16_lossy(&u16_slice);
            let p = PathBuf::from(raw_path.trim());
            let full_path = if p.is_absolute() {
                p
            } else {
                fonts_dir.join(p)
            };
            if full_path.is_file() {
                map.entry(primary.to_string()).or_insert(full_path);
            }
        }
        let _ = unsafe { RegCloseKey(hkey) };
    };

    read_hive(HKEY_LOCAL_MACHINE);
    read_hive(HKEY_CURRENT_USER);
    map.into_iter().collect()
}

/// 按指定字体家族名称加载系统字体文件；未指定或找不到时回退到默认系统字体。
pub fn load_font_for_family(family: Option<&str>) -> Option<FontVec> {
    if let Some(name) = family {
        if let Some((_, path)) = system_font_list().iter().find(|(n, _)| n == name)
            && let Ok(data) = std::fs::read(path)
            && let Ok(font) = FontVec::try_from_vec_and_index(data, 0)
        {
            return Some(font);
        }
        #[cfg(target_os = "windows")]
        {
            let fallback = match name {
                "Microsoft YaHei" => Some("C:\\Windows\\Fonts\\msyh.ttc"),
                "SimHei" => Some("C:\\Windows\\Fonts\\simhei.ttf"),
                "SimSun" => Some("C:\\Windows\\Fonts\\simsun.ttc"),
                "KaiTi" => Some("C:\\Windows\\Fonts\\simkai.ttf"),
                "Arial" => Some("C:\\Windows\\Fonts\\arial.ttf"),
                "Consolas" => Some("C:\\Windows\\Fonts\\consola.ttf"),
                _ => None,
            };
            if let Some(p) = fallback
                && let Ok(data) = std::fs::read(p)
                && let Ok(font) = FontVec::try_from_vec_and_index(data, 0)
            {
                return Some(font);
            }
        }
    }
    load_system_font()
}

/// 绘制单个标注（坐标从全图换算到裁剪局部坐标）。
fn draw_shape(
    img: &mut RgbaImage,
    shape: &Shape,
    off_x: f32,
    off_y: f32,
    clip: &PixelRect,
    font: &Option<FontVec>,
) {
    // 局部换算小工具：全图坐标减去裁剪原点。
    let local = |x: f32, y: f32| (x - off_x, y - off_y);
    match shape {
        Shape::Pen { pts, color, width } => {
            let local_pts: Vec<(f32, f32)> = pts.iter().map(|(x, y)| local(*x, *y)).collect();
            stroke_polyline(img, &local_pts, *width, *color, clip);
        }
        Shape::Polygon {
            pts,
            color,
            width,
            fill,
            has_stroke,
        } => {
            let mut local_pts: Vec<(f32, f32)> = pts.iter().map(|(x, y)| local(*x, *y)).collect();
            if let Some(fill_color) = *fill {
                fill_polygon(img, &local_pts, fill_color, clip);
            }
            if *has_stroke {
                if let Some(&first) = local_pts.first() {
                    local_pts.push(first);
                }
                stroke_polyline(img, &local_pts, *width, *color, clip);
            }
        }
        Shape::Highlighter { pts, color, width } => {
            // 荧光笔：加粗 + 强制半透明，模拟记号笔覆盖效果。
            let translucent = AnnoColor(color.0, color.1, color.2, 110);
            let local_pts: Vec<(f32, f32)> = pts.iter().map(|(x, y)| local(*x, *y)).collect();
            stroke_polyline(img, &local_pts, width * 3.0, translucent, clip);
        }
        Shape::Rect {
            rect,
            color,
            width,
            fill,
            has_stroke,
            radius,
        } => {
            let (x, y) = local(rect.x, rect.y);
            let local_rect = PixelRect::new(x, y, rect.w, rect.h);
            if let Some(fill_color) = *fill {
                fill_rounded_rect(img, local_rect, *radius, fill_color, clip);
            }
            if *has_stroke {
                rounded_rect_border(img, local_rect, *radius, *width, *color, clip);
            }
        }
        Shape::Ellipse {
            rect,
            color,
            width,
            fill,
            has_stroke,
        } => {
            let (x, y) = local(rect.x, rect.y);
            let local_rect = PixelRect::new(x, y, rect.w, rect.h);
            if let Some(fill_color) = *fill {
                fill_ellipse(img, local_rect, fill_color, clip);
            }
            if *has_stroke {
                ellipse_border(img, local_rect, *width, *color, clip);
            }
        }
        Shape::Line {
            from,
            to,
            color,
            width,
        } => {
            stroke_polyline(
                img,
                &[local(from.0, from.1), local(to.0, to.1)],
                *width,
                *color,
                clip,
            );
        }
        Shape::Arrow {
            from,
            to,
            color,
            width,
        } => {
            let a = local(from.0, from.1);
            let b = local(to.0, to.1);
            draw_arrow(img, a, b, *width, *color, clip);
        }
        Shape::Mosaic { rect } => {
            let (x, y) = local(rect.x, rect.y);
            pixelate(img, x, y, rect.w, rect.h, clip);
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
        } => {
            let custom_font = font_family
                .as_deref()
                .and_then(|f| load_font_for_family(Some(f)));
            let active_font = custom_font.as_ref().or(font.as_ref());
            // 无字体时跳过，保证“至少能导出底图 + 图形”。
            if let Some(font) = active_font {
                let bg_fill = if *bg { Some(*bg_color) } else { None };
                let text_stroke = stroke_color.map(|sc| (sc, *stroke_width));
                let (x, y) = local(pos.0, pos.1);
                if let Some(box_w) = *max_w {
                    let lines = wrap_lines_with_font(font, content, *size, box_w);
                    let line_step = *size * crate::shapes::TEXT_LINE_HEIGHT;
                    for (idx, line) in lines.iter().enumerate() {
                        draw_text(
                            img,
                            font,
                            (x, y + idx as f32 * line_step),
                            line,
                            *color,
                            *size,
                            bg_fill,
                            text_stroke,
                            clip,
                        );
                    }
                } else {
                    let line_step = *size * crate::shapes::TEXT_LINE_HEIGHT;
                    for (idx, line) in content.split('\n').enumerate() {
                        draw_text(
                            img,
                            font,
                            (x, y + idx as f32 * line_step),
                            line,
                            *color,
                            *size,
                            bg_fill,
                            text_stroke,
                            clip,
                        );
                    }
                }
            }
        }
    }
}

/// 带裁剪的 alpha 混合写像素（越界直接丢弃）。
fn blend_px(img: &mut RgbaImage, x: i32, y: i32, src: AnnoColor, clip: &PixelRect) {
    if x < clip.x as i32
        || y < clip.y as i32
        || x >= (clip.x + clip.w) as i32
        || y >= (clip.y + clip.h) as i32
    {
        return;
    }
    if x < 0 || y < 0 || x >= img.width() as i32 || y >= img.height() as i32 {
        return;
    }
    // 标准的 source-over 混合。
    let dst = img.get_pixel(x as u32, y as u32);
    let sa = src.3 as f32 / 255.0;
    let da = dst[3] as f32 / 255.0;
    let out_a = sa + da * (1.0 - sa);
    if out_a <= 0.0 {
        return;
    }
    let mix = |s: u8, d: u8| ((s as f32 * sa + d as f32 * da * (1.0 - sa)) / out_a) as u8;
    img.put_pixel(
        x as u32,
        y as u32,
        Rgba([
            mix(src.0, dst[0]),
            mix(src.1, dst[1]),
            mix(src.2, dst[2]),
            (out_a * 255.0) as u8,
        ]),
    );
}

/// 实心圆印章（粗线条的笔触基本单元）。
fn stamp(img: &mut RgbaImage, cx: f32, cy: f32, r: f32, color: AnnoColor, clip: &PixelRect) {
    // 半径小于 1 时退化为单像素，避免细线断裂。
    if r < 1.0 {
        blend_px(img, cx.round() as i32, cy.round() as i32, color, clip);
        return;
    }
    let r2 = r * r;
    let x0 = (cx - r).floor() as i32;
    let x1 = (cx + r).ceil() as i32;
    let y0 = (cy - r).floor() as i32;
    let y1 = (cy + r).ceil() as i32;
    for y in y0..=y1 {
        for x in x0..=x1 {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            if dx * dx + dy * dy <= r2 {
                blend_px(img, x, y, color, clip);
            }
        }
    }
}

/// 折线描边：按弧长均匀取点盖印章，保证拐弯不断线。
fn stroke_polyline(
    img: &mut RgbaImage,
    pts: &[(f32, f32)],
    width: f32,
    color: AnnoColor,
    clip: &PixelRect,
) {
    if pts.is_empty() {
        return;
    }
    if pts.len() == 1 {
        stamp(img, pts[0].0, pts[0].1, width / 2.0, color, clip);
        return;
    }
    for pair in pts.windows(2) {
        let (ax, ay) = pair[0];
        let (bx, by) = pair[1];
        let dist = ((bx - ax).powi(2) + (by - ay).powi(2)).sqrt();
        // 步数按半像素取整，细线不露缝，粗线不浪费。
        let steps = (dist * 2.0).ceil().max(1.0) as usize;
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            stamp(
                img,
                ax + (bx - ax) * t,
                ay + (by - ay) * t,
                width / 2.0,
                color,
                clip,
            );
        }
    }
}

/// 矩形边框：四条实心条带拼合。
fn rect_border(
    img: &mut RgbaImage,
    rect: PixelRect,
    width: f32,
    color: AnnoColor,
    clip: &PixelRect,
) {
    let (x, y, w, h) = (rect.x, rect.y, rect.w, rect.h);
    let t = width.max(1.0);
    fill_rect(img, x, y, w, t, color, clip);
    fill_rect(img, x, y + h - t, w, t, color, clip);
    fill_rect(img, x, y, t, h, color, clip);
    fill_rect(img, x + w - t, y, t, h, color, clip);
}

/// 实心矩形填充。
fn fill_rect(
    img: &mut RgbaImage,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    color: AnnoColor,
    clip: &PixelRect,
) {
    for yy in (y.floor() as i32)..((y + h).ceil() as i32) {
        for xx in (x.floor() as i32)..((x + w).ceil() as i32) {
            blend_px(img, xx, yy, color, clip);
        }
    }
}

/// 实心圆角矩形填充：圆角半径自动裁剪到宽高一半以内。
fn fill_rounded_rect(
    img: &mut RgbaImage,
    rect: PixelRect,
    radius: f32,
    color: AnnoColor,
    clip: &PixelRect,
) {
    let r = radius.max(0.0).min(rect.w * 0.5).min(rect.h * 0.5);
    if r <= 0.5 {
        fill_rect(img, rect.x, rect.y, rect.w, rect.h, color, clip);
        return;
    }
    let (x0, y0) = (rect.x.floor() as i32, rect.y.floor() as i32);
    let (x1, y1) = (
        (rect.x + rect.w).ceil() as i32,
        (rect.y + rect.h).ceil() as i32,
    );
    let r2 = r * r;
    for yy in y0..y1 {
        let py = yy as f32 + 0.5;
        let dy = if py < rect.y + r {
            rect.y + r - py
        } else if py > rect.y + rect.h - r {
            py - (rect.y + rect.h - r)
        } else {
            0.0
        };
        for xx in x0..x1 {
            let px = xx as f32 + 0.5;
            let dx = if px < rect.x + r {
                rect.x + r - px
            } else if px > rect.x + rect.w - r {
                px - (rect.x + rect.w - r)
            } else {
                0.0
            };
            if dx * dx + dy * dy <= r2 {
                blend_px(img, xx, yy, color, clip);
            }
        }
    }
}

/// 圆角矩形描边：四条直边 + 四个四分之一圆弧用圆印章平滑描绘。
fn rounded_rect_border(
    img: &mut RgbaImage,
    rect: PixelRect,
    radius: f32,
    width: f32,
    color: AnnoColor,
    clip: &PixelRect,
) {
    let r = radius.max(0.0).min(rect.w * 0.5).min(rect.h * 0.5);
    if r <= 0.5 {
        rect_border(img, rect, width, color, clip);
        return;
    }
    let (x, y, w, h) = (rect.x, rect.y, rect.w, rect.h);
    let stamp_r = width / 2.0;
    // 四条直边。
    stroke_polyline(img, &[(x + r, y), (x + w - r, y)], width, color, clip);
    stroke_polyline(
        img,
        &[(x + w, y + r), (x + w, y + h - r)],
        width,
        color,
        clip,
    );
    stroke_polyline(
        img,
        &[(x + w - r, y + h), (x + r, y + h)],
        width,
        color,
        clip,
    );
    stroke_polyline(img, &[(x, y + h - r), (x, y + r)], width, color, clip);
    // 四个圆角弧。
    let corners = [
        (
            x + r,
            y + r,
            std::f32::consts::PI,
            std::f32::consts::FRAC_PI_2 * 3.0,
        ),
        (x + w - r, y + r, -std::f32::consts::FRAC_PI_2, 0.0),
        (x + w - r, y + h - r, 0.0, std::f32::consts::FRAC_PI_2),
        (
            x + r,
            y + h - r,
            std::f32::consts::FRAC_PI_2,
            std::f32::consts::PI,
        ),
    ];
    let steps = (r * 2.0).ceil().max(10.0) as usize;
    for (cx, cy, a0, a1) in corners {
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            let theta = a0 + (a1 - a0) * t;
            stamp(
                img,
                cx + r * theta.cos(),
                cy + r * theta.sin(),
                stamp_r,
                color,
                clip,
            );
        }
    }
}

/// 实心椭圆填充。
fn fill_ellipse(img: &mut RgbaImage, rect: PixelRect, color: AnnoColor, clip: &PixelRect) {
    let (rx, ry) = (rect.w * 0.5, rect.h * 0.5);
    if rx <= 0.5 || ry <= 0.5 {
        return;
    }
    let (cx, cy) = (rect.x + rx, rect.y + ry);
    let (x0, y0) = (rect.x.floor() as i32, rect.y.floor() as i32);
    let (x1, y1) = (
        (rect.x + rect.w).ceil() as i32,
        (rect.y + rect.h).ceil() as i32,
    );
    for yy in y0..y1 {
        let ny = (yy as f32 + 0.5 - cy) / ry;
        for xx in x0..x1 {
            let nx = (xx as f32 + 0.5 - cx) / rx;
            if nx * nx + ny * ny <= 1.0 {
                blend_px(img, xx, yy, color, clip);
            }
        }
    }
}

/// 实心多边形填充（扫描线奇偶规则）。
fn fill_polygon(img: &mut RgbaImage, pts: &[(f32, f32)], color: AnnoColor, clip: &PixelRect) {
    if pts.len() < 3 {
        return;
    }
    let mut min_y = f32::MAX;
    let mut max_y = f32::MIN;
    for &(_, y) in pts {
        min_y = min_y.min(y);
        max_y = max_y.max(y);
    }
    let y0 = min_y.floor() as i32;
    let y1 = max_y.ceil() as i32;
    let n = pts.len();
    let mut xs = Vec::with_capacity(n);
    for yy in y0..=y1 {
        let py = yy as f32 + 0.5;
        xs.clear();
        for i in 0..n {
            let (x1, y1) = pts[i];
            let (x2, y2) = pts[(i + 1) % n];
            if (y1 <= py && y2 > py) || (y2 <= py && y1 > py) {
                let x = x1 + (py - y1) * (x2 - x1) / (y2 - y1);
                xs.push(x);
            }
        }
        xs.sort_by(|a, b| a.total_cmp(b));
        for pair in xs.as_chunks::<2>().0 {
            let sx = pair[0].floor() as i32;
            let ex = pair[1].ceil() as i32;
            for xx in sx..ex {
                blend_px(img, xx, yy, color, clip);
            }
        }
    }
}

/// 椭圆边框：参数方程取点盖印章。
fn ellipse_border(
    img: &mut RgbaImage,
    rect: PixelRect,
    width: f32,
    color: AnnoColor,
    clip: &PixelRect,
) {
    let (x, y, w, h) = (rect.x, rect.y, rect.w, rect.h);
    let (rx, ry) = (w / 2.0, h / 2.0);
    if rx <= 0.5 || ry <= 0.5 {
        return;
    }
    let (cx, cy) = (x + rx, y + ry);
    // 步数按周长估算，保证曲线光滑。
    let steps = ((rx + ry) * 3.0).ceil().max(24.0) as usize;
    for i in 0..=steps {
        let theta = i as f32 / steps as f32 * std::f32::consts::TAU;
        stamp(
            img,
            cx + rx * theta.cos(),
            cy + ry * theta.sin(),
            width / 2.0,
            color,
            clip,
        );
    }
}

/// 箭头：主干 + 两个 25 度夹角的头部。
fn draw_arrow(
    img: &mut RgbaImage,
    from: (f32, f32),
    to: (f32, f32),
    width: f32,
    color: AnnoColor,
    clip: &PixelRect,
) {
    stroke_polyline(img, &[from, to], width, color, clip);
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 2.0 {
        return;
    }
    // 头部长度随线宽放大，太短的箭头不画头。
    let head = (12.0 + width * 2.0).min(len);
    let angle = dy.atan2(dx);
    // 头部与主干夹角 25 度。
    let spread = 25.0_f32.to_radians();
    for sign in [-1.0, 1.0] {
        let a = angle + std::f32::consts::PI + sign * spread;
        let tip = (to.0 + head * a.cos(), to.1 + head * a.sin());
        stroke_polyline(img, &[to, tip], width, color, clip);
    }
}

/// 马赛克：分块取左上角像素平铺，块大小 12px（Snipaste 观感）。
fn pixelate(img: &mut RgbaImage, x: f32, y: f32, w: f32, h: f32, clip: &PixelRect) {
    const BLOCK: i32 = 12;
    let x0 = x.floor() as i32;
    let y0 = y.floor() as i32;
    let x1 = (x + w).ceil() as i32;
    let y1 = (y + h).ceil() as i32;
    let mut by = y0;
    while by < y1 {
        let mut bx = x0;
        while bx < x1 {
            // 取块左上角颜色（带裁剪检查，越界用黑代替）。
            let sample = if in_clip(bx, by, clip)
                && bx >= 0
                && by >= 0
                && bx < img.width() as i32
                && by < img.height() as i32
            {
                *img.get_pixel(bx as u32, by as u32)
            } else {
                Rgba([0, 0, 0, 255])
            };
            // 整块平铺（不透明，直接覆盖）。
            for yy in by..(by + BLOCK).min(y1) {
                for xx in bx..(bx + BLOCK).min(x1) {
                    if in_clip(xx, yy, clip)
                        && xx >= 0
                        && yy >= 0
                        && xx < img.width() as i32
                        && yy < img.height() as i32
                    {
                        img.put_pixel(xx as u32, yy as u32, sample);
                    }
                }
            }
            bx += BLOCK;
        }
        by += BLOCK;
    }
}

/// 点是否落在裁剪区内。
fn in_clip(x: i32, y: i32, clip: &PixelRect) -> bool {
    x >= clip.x as i32
        && y >= clip.y as i32
        && x < (clip.x + clip.w) as i32
        && y < (clip.y + clip.h) as i32
}

/// UI 字号以 em 像素计，ab_glyph 则按字体总高度缩放，转换后避免保存文字变小。
fn text_pixel_scale(font: &FontVec, size: f32) -> PxScale {
    let size = size.max(8.0);
    let scaled = font
        .units_per_em()
        .filter(|em| *em > 0.0)
        .map(|em| size * font.height_unscaled() / em)
        .unwrap_or(size);
    PxScale::from(scaled)
}

/// 按指定字体测量并把文本按 `max_w` 折行拆分为多行（支持 `\n` 显式换行）。
fn wrap_lines_with_font(font: &FontVec, content: &str, size: f32, max_w: f32) -> Vec<String> {
    let scale = text_pixel_scale(font, size);
    let scaled = font.as_scaled(scale);
    let mut lines = Vec::new();
    for para in content.split('\n') {
        let mut cur = String::new();
        let mut cur_w = 0.0f32;
        for ch in para.chars() {
            let cw = scaled.h_advance(scaled.glyph_id(ch));
            if !cur.is_empty() && cur_w + cw > max_w {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0.0;
            }
            cur.push(ch);
            cur_w += cw;
        }
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// 单行文字绘制：逐字形取位图拼排（支持底色与文字背后的圆角描边）。
// 参数多为烧录必需（位置 / 内容 / 颜色 / 字号 / 底色 / 描边缺一不可），此处允许超限。
#[allow(clippy::too_many_arguments)]
fn draw_text(
    img: &mut RgbaImage,
    font: &FontVec,
    pos: (f32, f32),
    content: &str,
    color: AnnoColor,
    size: f32,
    bg: Option<AnnoColor>,
    stroke: Option<(AnnoColor, f32)>,
    clip: &PixelRect,
) {
    let (x, y) = pos;
    let scale = text_pixel_scale(font, size);
    let scaled = font.as_scaled(scale);
    // 先估算整行宽度（底色矩形用），与下方逐字推进共用同一 advance。
    let text_w: f32 = content
        .chars()
        .map(|ch| scaled.h_advance(scaled.glyph_id(ch)))
        .sum();
    let glyph_h = scaled.ascent() - scaled.descent();
    let text_h = size * crate::shapes::TEXT_LINE_HEIGHT;
    // 带底色时先铺该条自己的底色（统一半透明），内边距随字号缩放。
    // 预览 div 也有 px_2 / py_1 内边距，这里按比例还原，保证所见即所得。
    if let Some(bg_color) = bg {
        let pad_x = size * 0.25;
        let pad_y = size * 0.18;
        fill_rect(
            img,
            x - pad_x,
            y - pad_y,
            text_w + pad_x * 2.0,
            text_h + pad_y * 2.0,
            bg_color.as_text_bg(),
            clip,
        );
    }
    let baseline = y + (text_h - glyph_h) * 0.5 + scaled.ascent();

    // 若启用描边：先对所有字形位图做圆形膨胀（圆角描边），统一画在文字下方。
    if let Some((stroke_color, stroke_w)) = stroke
        && stroke_w > 0.1
    {
        let r = stroke_w.ceil() as i32;
        let r2 = stroke_w * stroke_w;
        let bx0 = (x - stroke_w - 4.0).floor() as i32;
        let bx1 = (x + text_w + stroke_w + 16.0).ceil() as i32;
        let by0 = (y - stroke_w - 8.0).floor() as i32;
        let by1 = (y + text_h + stroke_w + 8.0).ceil() as i32;
        let bw = (bx1 - bx0).max(1) as usize;
        let bh = (by1 - by0).max(1) as usize;
        let mut cov = vec![0.0f32; bw * bh];
        let mut caret_x = x;
        for ch in content.chars() {
            let glyph_id = scaled.glyph_id(ch);
            let glyph = glyph_id.with_scale_and_position(scale, point(caret_x, baseline));
            if let Some(outlined) = scaled.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|gx, gy, v| {
                    if v <= 0.0 {
                        return;
                    }
                    let px = bounds.min.x as i32 + gx as i32;
                    let py = bounds.min.y as i32 + gy as i32;
                    for dy in -r..=r {
                        for dx in -r..=r {
                            if (dx * dx + dy * dy) as f32 <= r2 {
                                let tx = px + dx - bx0;
                                let ty = py + dy - by0;
                                if tx >= 0 && ty >= 0 && (tx as usize) < bw && (ty as usize) < bh {
                                    let idx = ty as usize * bw + tx as usize;
                                    if v > cov[idx] {
                                        cov[idx] = v;
                                    }
                                }
                            }
                        }
                    }
                });
            }
            caret_x += scaled.h_advance(glyph_id);
        }
        for ty in 0..bh {
            for tx in 0..bw {
                let v = cov[ty * bw + tx];
                if v > 0.0 {
                    let faded = AnnoColor(
                        stroke_color.0,
                        stroke_color.1,
                        stroke_color.2,
                        (stroke_color.3 as f32 * v) as u8,
                    );
                    blend_px(img, bx0 + tx as i32, by0 + ty as i32, faded, clip);
                }
            }
        }
    }

    // 笔头从行首开始，绘制文字填充在描边之上。
    let mut caret_x = x;
    for ch in content.chars() {
        let glyph_id = scaled.glyph_id(ch);
        // 字形先定位再勾边，`OutlinedGlyph` 自带像素位图。
        let glyph = glyph_id.with_scale_and_position(scale, point(caret_x, baseline));
        if let Some(outlined) = scaled.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, v| {
                if v <= 0.0 {
                    return;
                }
                let px = bounds.min.x as i32 + gx as i32;
                let py = bounds.min.y as i32 + gy as i32;
                // 字形覆盖度调制 alpha，边缘抗锯齿。
                let faded = AnnoColor(color.0, color.1, color.2, (color.3 as f32 * v) as u8);
                blend_px(img, px, py, faded, clip);
            });
        }
        // 无轮廓字形（如空格）只推进笔头。
        caret_x += scaled.h_advance(glyph_id);
    }
}

#[cfg(test)]
mod tests {
    //! 导出模块单测：用合成底图覆盖全部标注类型，保证栅格化不 panic。

    use super::*;

    /// 快速 PNG 与直接 DIB 路径必须保持像素、通道和上下方向完全一致。
    /// Windows 系统中文字体的全角字 advance 应等于 UI 的 em 字号。
    #[cfg(target_os = "windows")]
    #[test]
    fn exported_text_uses_ui_pixel_font_size() {
        let font = load_system_font().expect("Windows system font required");
        let scaled = font.as_scaled(text_pixel_scale(&font, 28.0));
        let glyph = scaled.glyph_id('测');
        assert_ne!(glyph.0, 0);
        assert!((scaled.h_advance(glyph) - 28.0).abs() < 1.0);
    }

    #[test]
    fn fast_clipboard_preserves_pixels() {
        let image =
            RgbaImage::from_fn(7, 5, |x, y| Rgba([(x * 31) as u8, (y * 47) as u8, 93, 255]));
        let png = png_bytes(&image).unwrap();
        assert_eq!(image::load_from_memory(&png).unwrap().into_rgba8(), image);
        assert_eq!(rgba_to_dib_bytes(&image), png_to_dib_bytes(&png));
    }

    /// 全类型标注导出冒烟测试（尺寸与 PNG 编码断言）。
    #[test]
    fn export_all_shapes() {
        // 合成底图 200x150，避免依赖外部截图文件。
        let base = RgbaImage::from_pixel(200, 150, Rgba([10, 20, 30, 255]));
        let sel = PixelRect::new(10.0, 10.0, 100.0, 80.0);
        let shapes = vec![
            Shape::Pen {
                pts: vec![(0.0, 0.0), (50.0, 50.0), (120.0, 30.0)],
                color: AnnoColor::RED,
                width: 3.0,
            },
            Shape::Highlighter {
                pts: vec![(20.0, 20.0), (90.0, 60.0)],
                color: AnnoColor::YELLOW,
                width: 4.0,
            },
            Shape::Rect {
                rect: PixelRect::new(15.0, 15.0, 60.0, 40.0),
                color: AnnoColor::GREEN,
                width: 2.0,
                fill: None,
                has_stroke: true,
                radius: 4.0,
            },
            Shape::Ellipse {
                rect: PixelRect::new(30.0, 30.0, 50.0, 30.0),
                color: AnnoColor::BLUE,
                width: 2.0,
                fill: Some(AnnoColor::RED),
                has_stroke: true,
            },
            Shape::Arrow {
                from: (10.0, 70.0),
                to: (80.0, 20.0),
                color: AnnoColor::BLACK,
                width: 3.0,
            },
            Shape::Mosaic {
                rect: PixelRect::new(40.0, 40.0, 30.0, 20.0),
            },
            Shape::Text {
                pos: (20.0, 20.0),
                content: "望川test".to_string(),
                color: AnnoColor::WHITE,
                size: 20.0,
                bg: true,
                bg_color: AnnoColor::BLACK,
                max_w: None,
                font_family: None,
                stroke_color: Some(AnnoColor::BLACK),
                stroke_width: 3.0,
            },
        ];
        // 有字体则连文字一起烧录，无字体也应正常导出底图 + 图形。
        let font = load_system_font();
        let out = render_export(&base, sel, &shapes, &font);
        assert_eq!(out.dimensions(), (100, 80));
        assert!(png_bytes(&out).is_some());
    }

    /// 文字底色按条独立：默认自适应，自定义底色原样保留（只统一透明度）。
    #[test]
    fn text_bg_per_shape() {
        // 自适应默认：浅色字配黑底、深色字配白底，透明度统一。
        assert_eq!(
            AnnoColor::WHITE.text_bg_fill(),
            AnnoColor(0, 0, 0, AnnoColor::TEXT_BG_ALPHA)
        );
        assert_eq!(
            AnnoColor::BLACK.text_bg_fill(),
            AnnoColor(255, 255, 255, AnnoColor::TEXT_BG_ALPHA)
        );
        // 自定义底色：原样保留 RGB，只统一压透明度。
        assert_eq!(
            AnnoColor::RED.as_text_bg(),
            AnnoColor(231, 76, 60, AnnoColor::TEXT_BG_ALPHA)
        );
        // 同一底图上两条文字各用各的底色，导出不 panic 且尺寸正确。
        let base = RgbaImage::from_pixel(120, 80, Rgba([200, 200, 200, 255]));
        let sel = PixelRect::new(0.0, 0.0, 120.0, 80.0);
        let shapes = vec![
            Shape::Text {
                pos: (10.0, 10.0),
                content: "亮".to_string(),
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
                pos: (10.0, 40.0),
                content: "暗".to_string(),
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
        let font = load_system_font();
        let out = render_export(&base, sel, &shapes, &font);
        assert_eq!(out.dimensions(), (120, 80));
    }

    /// PNG 转 DIB：头尺寸与像素区断言（2x1 小图手算验证）。
    #[test]
    fn png_to_dib_layout() {
        // 2x1 纯红图，注意 DIB 自下而上、BGR 通道。
        let img = RgbaImage::from_pixel(2, 1, Rgba([255, 0, 0, 255]));
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        let dib = png_to_dib_bytes(&buf).expect("应生成 DIB");
        assert_eq!(dib.len(), 40 + 2 * 4);
        assert_eq!(&dib[0..4], &40u32.to_le_bytes());
        assert_eq!(&dib[4..8], &2i32.to_le_bytes());
        assert_eq!(&dib[8..12], &1i32.to_le_bytes());
        // 第一像素 BGR 应为 [0, 0, 255, 255]。
        assert_eq!(&dib[40..44], &[0, 0, 255, 255]);
    }

    /// HDROP 字节：头字段与双零结尾断言。
    #[test]
    fn hdrop_layout() {
        let path = std::path::Path::new("C:\\temp\\a.png");
        let bytes = hdrop_bytes(path).expect("应生成 HDROP");
        assert_eq!(&bytes[0..4], &20u32.to_le_bytes());
        assert_eq!(&bytes[16..20], &1u32.to_le_bytes());
        // 末尾必须是 UTF-16 双零（4 个零字节）。
        assert_eq!(&bytes[bytes.len() - 4..], &[0, 0, 0, 0]);
    }
}
