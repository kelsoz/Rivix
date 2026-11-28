//! 命令行截图与录屏模块：无界面全屏 / 区域截图直存 PNG，或按窗口 / 进程 / 区域录屏后退出。
//!
//! 不带参数启动图形界面（托盘常驻）；带截图参数时经 Windows GDI 同步截图，
//! 带 `record` 子命令时经 WGC 采集 BGRA 帧并交给 FFmpeg 编码。
//! 参数解析只用标准库（不引入 clap 等新依赖），像素操作复用 `image` 依赖。

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use image::RgbaImage;

use crate::record::{CliRecordRequest, CliRecordTarget, RecordFormat, run_cli_recording};

/// 命令行帮助文案（`--help` 与参数错误时展示）。
const HELP: &str = "Rivix 望川 · 命令行截图 / 录屏\n\
    \n\
    用法：\n\
    \x20 rivix                              启动图形界面（托盘常驻）\n\
    \x20 rivix -f [--output 路径] [--copy]             截取全屏\n\
    \x20 rivix -r x,y,w,h [--output 路径] [--copy]     截取指定区域\n\
    \x20 rivix record [目标] --duration 秒 [选项]      录制屏幕\n\
    \x20 rivix -h | --help                  显示本帮助\n\
    \x20 rivix -V | --version               显示版本\n\
    \n\
    说明：\n\
    \x20 - 未指定 --output 时，保存到与图形界面共用的默认目录，文件名 rivix-<毫秒>.<扩展名>。\n\
    \x20 - --output 可为文件或已存在的目录（目录则自动补文件名）；无扩展名时自动补。\n\
    \x20 - 截图坐标为虚拟屏幕物理像素（多显示器取并集），如 100,200,800,600。\n\
    \x20 - --copy 把保存的文件再复制到剪贴板（Windows）。\n\
    \x20 - 录屏详细参数见 rivix record --help。\n";

/// 录屏子命令帮助文案。
const RECORD_HELP: &str = "Rivix 望川 · 命令行录屏\n\
    \n\
    用法：\n\
    \x20 rivix record --window 标题或HWND --duration 秒 [选项]\n\
    \x20 rivix record --process PID或进程名 --duration 秒 [选项]\n\
    \x20 rivix record -r x,y,w,h --duration 秒 [选项]\n\
    \n\
    目标（三选一，必填）：\n\
    \x20 -w, --window <标题|HWND>    按窗口标题或完整 HWND 捕获真实窗口\n\
    \x20 -p, --process <PID|进程名>  选择该进程面积最大的可见窗口\n\
    \x20 -r, --region <x,y,w,h>      捕获单显示器物理像素区域，支持负坐标\n\
    \n\
    参数：\n\
    \x20 -t, --duration <秒>         录制时长，必填且大于 0\n\
    \x20 -F, --format <mp4|gif>      输出格式，默认 mp4\n\
    \x20 -b, --bitrate <Mbps>        MP4 平均视频码率，默认 8（GIF 不适用）\n\
    \x20     --fps <1..60>           帧率，默认 30\n\
    \x20 -o, --output <文件|目录>     输出路径，默认使用截图保存目录\n\
    \x20     --copy                  保存后复制文件到剪贴板（Windows）\n\
    \x20 -h, --help                  显示本帮助\n\
    \n\
    说明：\n\
    \x20 - 标题先精确匹配再子串匹配；多个结果取面积最大窗口。\n\
    \x20 - 区域部分越界会裁剪；跨多个显示器会报错。\n\
    \x20 - CLI 录屏仅视频，不录入鼠标或音频；MP4 使用 H.264 软件编码。\n\
    \x20 - 输出文件已存在时拒绝覆盖；录制中改变窗口尺寸会停止并保留已录片段。\n";

/// 命令行动具体作（参数解析结果）。
enum CliAction {
    /// 打印帮助。
    Help,
    /// 打印录屏帮助。
    RecordHelp,
    /// 打印版本。
    Version,
    /// 执行截图并保存。
    Capture(CaptureRequest),
    /// 执行录屏并保存。
    Record {
        /// 录屏请求。
        request: CliRecordRequest,
        /// 保存后是否复制文件到剪贴板。
        copy: bool,
    },
}

/// 一次命令行截图请求。
struct CaptureRequest {
    /// 截图区域（虚拟屏幕物理像素）；`None` 表示全屏。
    region: Option<Region>,
    /// 显式输出路径；`None` 表示用默认目录 + 时间戳文件名。
    output: Option<PathBuf>,
    /// 保存后是否再复制文件到剪贴板。
    copy: bool,
}

/// 截图区域（`--region x,y,w,h` 解析结果，虚拟屏幕物理像素）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Region {
    /// 左上角横坐标。
    x: i32,
    /// 左上角纵坐标。
    y: i32,
    /// 区域宽度（恒为正）。
    w: u32,
    /// 区域高度（恒为正）。
    h: u32,
}

/// 是否为命令行调用：带任意参数即走 CLI（截图 / 录屏后退出），无参数进图形界面。
pub fn is_cli_invocation() -> bool {
    std::env::args().len() > 1
}

/// 命令行入口：挂接父控制台 → 解析参数 → 执行截图或录屏，返回进程退出码。
///
/// 退出码：0 成功，1 参数错误，2 截图 / 录屏 / 保存失败。
pub fn run_cli() -> i32 {
    attach_console();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args) {
        Ok(CliAction::Help) => {
            print!("{HELP}");
            0
        }
        Ok(CliAction::RecordHelp) => {
            print!("{RECORD_HELP}");
            0
        }
        Ok(CliAction::Version) => {
            println!("rivix {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Ok(CliAction::Capture(request)) => match execute_capture(&request) {
            Ok(path) => {
                println!("已保存：{}", path.display());
                0
            }
            Err(error) => {
                eprintln!("截图失败：{error}");
                2
            }
        },
        Ok(CliAction::Record { request, copy }) => match execute_record(&request, copy) {
            Ok(path) => {
                println!("已保存：{}", path.display());
                0
            }
            Err(error) => {
                eprintln!("录屏失败：{error}");
                2
            }
        },
        Err(error) => {
            eprintln!("参数错误：{error}\n\n{HELP}");
            1
        }
    }
}

/// 解析命令行参数（纯函数，无副作用，便于单测）。
fn parse_args(args: &[String]) -> Result<CliAction, String> {
    if args.first().is_some_and(|arg| arg == "record") {
        return parse_record_args(&args[1..]);
    }
    parse_capture_args(args)
}

/// 解析原有截图参数，保持 `-f` / `-r` 行为不变。
fn parse_capture_args(args: &[String]) -> Result<CliAction, String> {
    let mut fullscreen = false;
    let mut region: Option<Region> = None;
    let mut output: Option<PathBuf> = None;
    let mut copy = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-h" | "--help" => return Ok(CliAction::Help),
            "-V" | "--version" => return Ok(CliAction::Version),
            "-f" | "--fullscreen" => {
                if region.is_some() {
                    return Err("--fullscreen 与 --region 不可同时使用".to_string());
                }
                fullscreen = true;
            }
            "-r" | "--region" => {
                if fullscreen {
                    return Err("--fullscreen 与 --region 不可同时使用".to_string());
                }
                index += 1;
                let spec = args
                    .get(index)
                    .ok_or("--region 缺少坐标（格式 x,y,w,h）".to_string())?;
                region = Some(parse_region(spec)?);
            }
            "-o" | "--output" => {
                index += 1;
                let path = args.get(index).ok_or("-o 缺少输出路径".to_string())?;
                if path.is_empty() {
                    return Err("-o 输出路径为空".to_string());
                }
                output = Some(PathBuf::from(path));
            }
            "--copy" => copy = true,
            unknown => return Err(format!("未知参数：{unknown}")),
        }
        index += 1;
    }
    if region.is_none() && !fullscreen {
        // 只有 -o / --copy 而没指定截图范围时，无法确定截什么。
        return Err("请指定 --fullscreen 或 --region x,y,w,h".to_string());
    }
    Ok(CliAction::Capture(CaptureRequest {
        region,
        output,
        copy,
    }))
}

/// 解析录屏子命令：目标三选一，时长必填，其余参数使用最小默认值。
fn parse_record_args(args: &[String]) -> Result<CliAction, String> {
    let mut target: Option<CliRecordTarget> = None;
    let mut duration = None;
    let mut format = RecordFormat::Mp4;
    let mut bitrate_mbps = None;
    let mut fps = 30;
    let mut output = None;
    let mut copy = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-h" | "--help" => return Ok(CliAction::RecordHelp),
            "-V" | "--version" => return Ok(CliAction::Version),
            "-w" | "--window" => {
                let value = next_arg(args, &mut index, "--window")?;
                if value.trim().is_empty() {
                    return Err("--window 不能为空".to_string());
                }
                set_record_target(&mut target, CliRecordTarget::Window(value.to_string()))?;
            }
            "-p" | "--process" => {
                let value = next_arg(args, &mut index, "--process")?;
                if value.trim().is_empty() {
                    return Err("--process 不能为空".to_string());
                }
                set_record_target(&mut target, CliRecordTarget::Process(value.to_string()))?;
            }
            "-r" | "--region" => {
                let value = next_arg(args, &mut index, "--region")?;
                let region = parse_region(value)?;
                set_record_target(
                    &mut target,
                    CliRecordTarget::Region {
                        x: region.x,
                        y: region.y,
                        w: region.w,
                        h: region.h,
                    },
                )?;
            }
            "-t" | "--duration" => {
                let value = next_arg(args, &mut index, "--duration")?;
                let seconds = value
                    .parse::<u64>()
                    .map_err(|_| format!("--duration 必须为整数秒：{value}"))?;
                if seconds == 0 {
                    return Err("--duration 必须大于 0".to_string());
                }
                duration = Some(Duration::from_secs(seconds));
            }
            "-F" | "--format" => {
                let value = next_arg(args, &mut index, "--format")?;
                format = match value.to_ascii_lowercase().as_str() {
                    "mp4" => RecordFormat::Mp4,
                    "gif" => RecordFormat::Gif,
                    _ => return Err(format!("--format 仅支持 mp4 或 gif：{value}")),
                };
            }
            "-b" | "--bitrate" => {
                let value = next_arg(args, &mut index, "--bitrate")?;
                let mbps = value
                    .parse::<u32>()
                    .map_err(|_| format!("--bitrate 必须为正整数 Mbps：{value}"))?;
                if mbps == 0 {
                    return Err("--bitrate 必须大于 0".to_string());
                }
                bitrate_mbps = Some(mbps);
            }
            "--fps" => {
                let value = next_arg(args, &mut index, "--fps")?;
                fps = value
                    .parse::<u32>()
                    .map_err(|_| format!("--fps 必须为整数：{value}"))?;
                if !(1..=60).contains(&fps) {
                    return Err("--fps 必须在 1..=60 之间".to_string());
                }
            }
            "-o" | "--output" => {
                let value = next_arg(args, &mut index, "--output")?;
                if value.is_empty() {
                    return Err("--output 不能为空".to_string());
                }
                output = Some(PathBuf::from(value));
            }
            "--copy" => copy = true,
            unknown => return Err(format!("未知录屏参数：{unknown}")),
        }
        index += 1;
    }
    let target = target.ok_or_else(|| "请指定 --window、--process 或 --region 之一".to_string())?;
    let duration = duration.ok_or_else(|| "请指定 --duration 秒".to_string())?;
    if format == RecordFormat::Gif && bitrate_mbps.is_some() {
        return Err("GIF 不支持 --bitrate，请仅在 MP4 时使用".to_string());
    }
    Ok(CliAction::Record {
        request: CliRecordRequest {
            target,
            duration,
            format,
            bitrate_mbps: bitrate_mbps.unwrap_or(8),
            fps,
            output,
        },
        copy,
    })
}

/// 读取当前选项的值并推进游标；统一处理缺值与空参数。
fn next_arg<'a>(args: &'a [String], index: &mut usize, flag: &str) -> Result<&'a str, String> {
    *index += 1;
    args.get(*index)
        .map(String::as_str)
        .ok_or_else(|| format!("{flag} 缺少参数值"))
}

/// 设置唯一录屏目标，重复或冲突目标直接报错。
fn set_record_target(
    slot: &mut Option<CliRecordTarget>,
    target: CliRecordTarget,
) -> Result<(), String> {
    if slot.is_some() {
        return Err("--window、--process、--region 只能指定一个".to_string());
    }
    *slot = Some(target);
    Ok(())
}

/// 解析 `x,y,w,h`（如 `100,200,800,600`，物理像素，宽高必须为正）。
fn parse_region(spec: &str) -> Result<Region, String> {
    let parts: Vec<&str> = spec.split(',').collect();
    if parts.len() != 4 {
        return Err(format!(
            "区域格式错误：{spec}（应为 x,y,w,h，如 100,200,800,600）"
        ));
    }
    let number = |text: &str| {
        text.trim()
            .parse::<i32>()
            .map_err(|_| format!("区域数值非法：{spec}"))
    };
    let (x, y, w, h) = (
        number(parts[0])?,
        number(parts[1])?,
        number(parts[2])?,
        number(parts[3])?,
    );
    if w <= 0 || h <= 0 {
        return Err(format!("区域宽高必须为正：{spec}"));
    }
    Ok(Region {
        x,
        y,
        w: w as u32,
        h: h as u32,
    })
}

/// 执行截图：全屏为唯一像素来源，区域模式只是事后裁剪（与 GUI 首帧共享思路一致）。
fn execute_capture(request: &CaptureRequest) -> Result<PathBuf, String> {
    let full = capture_fullscreen()?;
    let img = match &request.region {
        Some(region) => crop_region(&full, region)?,
        None => full,
    };
    let path = save_image(request.output.as_deref(), &img)?;
    if request.copy {
        // 复制失败不影响已落盘文件，只告警。
        if let Err(error) = crate::export::copy_file_to_clipboard(&path) {
            eprintln!("警告：复制到剪贴板失败：{error}");
        }
    }
    Ok(path)
}

/// 执行录屏；复制失败不影响已落盘视频，只告警。
fn execute_record(request: &CliRecordRequest, copy: bool) -> Result<PathBuf, String> {
    let path = run_cli_recording(request)?;
    if copy && let Err(error) = crate::export::copy_file_to_clipboard(&path) {
        eprintln!("警告：复制到剪贴板失败：{error}");
    }
    Ok(path)
}

/// 按区域裁剪（与全图求交：部分越界按交集裁剪，完全无交集报错）。
fn crop_region(full: &RgbaImage, region: &Region) -> Result<RgbaImage, String> {
    let (full_w, full_h) = (full.width() as i32, full.height() as i32);
    let x0 = region.x.max(0);
    let y0 = region.y.max(0);
    let x1 = (region.x.saturating_add(region.w as i32)).min(full_w);
    let y1 = (region.y.saturating_add(region.h as i32)).min(full_h);
    if x1 <= x0 || y1 <= y0 {
        return Err(format!("区域 {} 与屏幕无交集", format_region(region)));
    }
    Ok(image::imageops::crop_imm(
        full,
        x0 as u32,
        y0 as u32,
        (x1 - x0) as u32,
        (y1 - y0) as u32,
    )
    .to_image())
}

/// 区域的人类可读形式（报错展示用）。
fn format_region(region: &Region) -> String {
    format!("{},{},{},{}", region.x, region.y, region.w, region.h)
}

/// 保存截图并返回最终路径（显式目录自动补文件名，无扩展名补 `.png`）。
fn save_image(explicit: Option<&Path>, img: &RgbaImage) -> Result<PathBuf, String> {
    let mut path = match explicit {
        Some(dir) if dir.is_dir() => dir.join(crate::app::timestamped_png_name()),
        Some(file) => file.to_path_buf(),
        None => crate::app::load_save_dir().join(crate::app::timestamped_png_name()),
    };
    if path.extension().is_none() {
        path.set_extension("png");
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        // 输出到子目录时自动建目录，建失败直接报错（不静默落盘到别处）。
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("创建输出目录失败（{}）：{error}", parent.display()))?;
    }
    img.save(&path)
        .map_err(|error| format!("保存截图失败（{}）：{error}", path.display()))?;
    Ok(path)
}

/// 挂接父控制台（仅 Windows release）。
///
/// release 用 `windows_subsystem = "windows"` 编译，默认无控制台；
/// 从 cmd / PowerShell 启动时挂接父控制台，`println!` 才能被用户看到。
/// 挂接失败（如双击启动、CI 无头会话）则把标准输出 / 错误重定向到 NUL：
/// 此时根本没有可看的地方，保住退出码正确比直接崩溃更重要
///（`println!` 往无效句柄写会 panic，进程以 101 退出）。
#[cfg(target_os = "windows")]
fn attach_console() {
    use std::os::windows::io::IntoRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_ok() {
            return;
        }
        // NUL 设备用标准库打开即可，不新增 windows 特性依赖；
        // into_raw_handle 会接管句柄（不析构），随进程结束自动回收。
        if let Ok(file) = std::fs::OpenOptions::new().write(true).open("NUL") {
            let handle = HANDLE(file.into_raw_handle());
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, handle);
            let _ = SetStdHandle(STD_ERROR_HANDLE, handle);
        }
    }
}

/// 非 Windows 平台无需挂接控制台。
#[cfg(not(target_os = "windows"))]
fn attach_console() {}

/// Windows 实现：GDI 同步截取虚拟屏幕全屏（多显示器取并集）。
///
/// GUI 走 rgpui 推流取首帧（需事件循环）；CLI 无界面，改用 GDI `BitBlt` 一次拷贝，
/// 同步、无窗口、退出即释放，不与托盘进程的捕获会话冲突。
#[cfg(target_os = "windows")]
fn capture_fullscreen() -> Result<RgbaImage, String> {
    use windows::Win32::{
        Graphics::Gdi::{
            BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
            DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SRCCOPY,
            SelectObject,
        },
        UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN,
        },
    };

    // 虚拟屏幕（多显示器并集）：CLI 全屏即截全部显示器。
    let (left, top, width, height) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if width <= 0 || height <= 0 {
        return Err("获取屏幕尺寸失败".to_string());
    }

    unsafe {
        // 屏幕 DC（NULL 即全屏），后续 GDI 对象按逆序释放。
        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            return Err("获取屏幕设备上下文失败".to_string());
        }
        // 内存 DC + 兼容位图：BitBlt 拷贝目标。
        let mem_dc = CreateCompatibleDC(Some(screen_dc));
        if mem_dc.is_invalid() {
            ReleaseDC(None, screen_dc);
            return Err("创建内存设备上下文失败".to_string());
        }
        let bitmap = CreateCompatibleBitmap(screen_dc, width, height);
        if bitmap.is_invalid() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(None, screen_dc);
            return Err("创建兼容位图失败".to_string());
        }
        let old = SelectObject(mem_dc, bitmap.into());
        // 从虚拟屏幕原点拷贝整块到内存位图。
        let blit_ok = BitBlt(
            mem_dc,
            0,
            0,
            width,
            height,
            Some(screen_dc),
            left,
            top,
            SRCCOPY,
        )
        .is_ok();
        if !blit_ok {
            SelectObject(mem_dc, old);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(mem_dc);
            ReleaseDC(None, screen_dc);
            return Err("复制屏幕像素失败".to_string());
        }
        // 32 位自顶向下 DIB（高度取负），GetDIBits 直接给出正向 BGRA。
        let mut info = BITMAPINFO::default();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = width;
        info.bmiHeader.biHeight = -height;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = 0; // BI_RGB：无压缩。
        let mut buf = vec![0u8; width as usize * 4 * height as usize];
        let lines = GetDIBits(
            mem_dc,
            bitmap,
            0,
            height as u32,
            Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
            &mut info,
            DIB_RGB_COLORS,
        );
        // 用完即还：先恢复旧 GDI 对象，再删位图与 DC。
        SelectObject(mem_dc, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(mem_dc);
        ReleaseDC(None, screen_dc);
        if lines == 0 {
            return Err("读取屏幕像素失败".to_string());
        }
        // BGRA 转 RGBA（与 capture.rs 首帧解码一致：image crate 通道顺序）。
        let (chunks, _) = buf.as_chunks_mut::<4>();
        for pixel in chunks {
            pixel.swap(0, 2);
        }
        RgbaImage::from_raw(width as u32, height as u32, buf)
            .ok_or_else(|| "组装图片失败".to_string())
    }
}

/// 非 Windows 平台：命令行截图暂不支持（与剪贴板文件项策略一致）。
#[cfg(not(target_os = "windows"))]
fn capture_fullscreen() -> Result<RgbaImage, String> {
    Err("当前平台暂不支持命令行截图".to_string())
}

#[cfg(test)]
mod tests {
    //! 命令行模块单测：参数解析与区域裁剪纯逻辑（GDI 截屏依赖系统，不进单测）。

    use super::*;

    /// 把 `&str` 切片转成参数向量（模拟 `std::env::args().skip(1)`）。
    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    /// 全屏最小参数：`-f` 即合法，输出与复制均为空。
    #[test]
    fn fullscreen_minimal() {
        let action = parse_args(&args(&["-f"])).expect("应解析成功");
        let CaptureRequest {
            region,
            output,
            copy,
        } = assert_capture(action);
        assert!(region.is_none());
        assert!(output.is_none());
        assert!(!copy);
    }

    /// 区域 + 输出 + 复制组合解析。
    #[test]
    fn region_with_output_and_copy() {
        let action = parse_args(&args(&["-r", "100,200,800,600", "-o", "a.png", "--copy"]))
            .expect("应解析成功");
        let request = assert_capture(action);
        assert_eq!(
            request.region,
            Some(Region {
                x: 100,
                y: 200,
                w: 800,
                h: 600,
            })
        );
        assert_eq!(request.output, Some(PathBuf::from("a.png")));
        assert!(request.copy);
    }

    /// 断言解析结果为截图动作（非帮助 / 版本），并拆出请求。
    fn assert_capture(action: CliAction) -> CaptureRequest {
        match action {
            CliAction::Capture(request) => request,
            _ => panic!("应为截图动作"),
        }
    }

    /// 帮助与版本短长选项均可达。
    #[test]
    fn help_and_version() {
        assert!(matches!(
            parse_args(&args(&["--help"])),
            Ok(CliAction::Help)
        ));
        assert!(matches!(parse_args(&args(&["-h"])), Ok(CliAction::Help)));
        assert!(matches!(
            parse_args(&args(&["--version"])),
            Ok(CliAction::Version)
        ));
        assert!(matches!(parse_args(&args(&["-V"])), Ok(CliAction::Version)));
    }

    /// 录屏默认值：窗口 + 必填时长即可，MP4 / 8 Mbps / 30fps / 默认目录。
    #[test]
    fn record_defaults() {
        let action = parse_args(&args(&[
            "record",
            "--window",
            "Untitled - Notepad",
            "--duration",
            "2",
        ]))
        .expect("应解析成功");
        let (request, copy) = assert_record(action);
        assert_eq!(
            request.target,
            CliRecordTarget::Window("Untitled - Notepad".to_string())
        );
        assert_eq!(request.duration, Duration::from_secs(2));
        assert_eq!(request.format, RecordFormat::Mp4);
        assert_eq!(request.bitrate_mbps, 8);
        assert_eq!(request.fps, 30);
        assert!(request.output.is_none());
        assert!(!copy);
    }

    /// 区域录屏支持负坐标、完整参数与复制。
    #[test]
    fn record_region_with_all_options() {
        let action = parse_args(&args(&[
            "record",
            "-r",
            "-10,-20,640,480",
            "-t",
            "3",
            "-F",
            "mp4",
            "-b",
            "12",
            "--fps",
            "60",
            "-o",
            "out/clip",
            "--copy",
        ]))
        .expect("应解析成功");
        let (request, copy) = assert_record(action);
        assert_eq!(
            request.target,
            CliRecordTarget::Region {
                x: -10,
                y: -20,
                w: 640,
                h: 480,
            }
        );
        assert_eq!(request.duration, Duration::from_secs(3));
        assert_eq!(request.bitrate_mbps, 12);
        assert_eq!(request.fps, 60);
        assert_eq!(request.output, Some(PathBuf::from("out/clip")));
        assert!(copy);
    }

    /// 进程录屏可选择 GIF；GIF 保留默认码率字段但不允许显式 --bitrate。
    #[test]
    fn record_process_gif() {
        let action = parse_args(&args(&[
            "record",
            "--process",
            "Code.exe",
            "--duration",
            "1",
            "--format",
            "gif",
            "-o",
            "clip.gif",
        ]))
        .expect("应解析成功");
        let (request, copy) = assert_record(action);
        assert_eq!(
            request.target,
            CliRecordTarget::Process("Code.exe".to_string())
        );
        assert_eq!(request.format, RecordFormat::Gif);
        assert_eq!(request.bitrate_mbps, 8);
        assert!(!copy);
    }

    /// 录屏互斥、必填项与边界值均在解析阶段拒绝。
    #[test]
    fn record_rejects_invalid_arguments() {
        let invalid: &[&[&str]] = &[
            &["record", "--window", "Notepad"],
            &["record", "--duration", "1"],
            &["record", "--window", "Notepad", "--duration", "0"],
            &[
                "record",
                "--window",
                "Notepad",
                "--duration",
                "1",
                "--fps",
                "0",
            ],
            &[
                "record",
                "--window",
                "Notepad",
                "--duration",
                "1",
                "--fps",
                "61",
            ],
            &[
                "record",
                "--window",
                "Notepad",
                "--duration",
                "1",
                "--bitrate",
                "0",
            ],
            &[
                "record",
                "--window",
                "Notepad",
                "--duration",
                "1",
                "--format",
                "webm",
            ],
            &[
                "record",
                "--window",
                "Notepad",
                "--duration",
                "1",
                "--format",
                "gif",
                "--bitrate",
                "8",
            ],
            &[
                "record",
                "--window",
                "Notepad",
                "--region",
                "0,0,10,10",
                "--duration",
                "1",
            ],
            &["record", "--window"],
            &["record", "--window", "Notepad", "--duration", "1", "--wat"],
            &["record", "--window", "Notepad", "--duration", "1", "-o"],
        ];
        for case in invalid {
            assert!(parse_args(&args(case)).is_err(), "用例应拒绝：{case:?}");
        }
    }

    /// 录屏帮助与版本独立于必填目标 / 时长。
    #[test]
    fn record_help_and_version() {
        assert!(matches!(
            parse_args(&args(&["record", "--help"])),
            Ok(CliAction::RecordHelp)
        ));
        assert!(matches!(
            parse_args(&args(&["record", "--version"])),
            Ok(CliAction::Version)
        ));
    }

    /// 断言解析结果为录屏动作，并拆出请求与复制开关。
    fn assert_record(action: CliAction) -> (CliRecordRequest, bool) {
        match action {
            CliAction::Record { request, copy } => (request, copy),
            _ => panic!("应为录屏动作"),
        }
    }

    /// 区域格式用例：合法解析，非法（缺段 / 非数字 / 非正宽高）报错。
    #[test]
    fn region_format_cases() {
        assert!(parse_region("100,200,800,600").is_ok());
        assert!(parse_region(" -10 , -20 , 30 , 40 ").is_ok());
        assert!(parse_region("1,2,3").is_err());
        assert!(parse_region("a,b,c,d").is_err());
        assert!(parse_region("0,0,0,10").is_err());
        assert!(parse_region("0,0,10,-5").is_err());
    }

    /// 互斥与未知参数：全屏 + 区域同用、未知 flag、缺值均报错。
    #[test]
    fn conflicts_and_missing_values() {
        assert!(parse_args(&args(&["-f", "-r", "0,0,10,10"])).is_err());
        assert!(parse_args(&args(&["--wat"])).is_err());
        assert!(parse_args(&args(&["-r"])).is_err());
        assert!(parse_args(&args(&["-o"])).is_err());
        // 只有输出路径而没指定截什么，无法执行。
        assert!(parse_args(&args(&["-o", "a.png"])).is_err());
    }

    /// 区域裁剪：部分越界按交集裁剪，完全无交集报错。
    #[test]
    fn crop_clamps_to_screen() {
        // 合成底图 100x80，避免依赖真实屏幕。
        let full = RgbaImage::from_pixel(100, 80, image::Rgba([1, 2, 3, 255]));
        let partial = crop_region(
            &full,
            &Region {
                x: 90,
                y: 70,
                w: 20,
                h: 20,
            },
        )
        .expect("部分越界应按交集裁剪");
        assert_eq!(partial.dimensions(), (10, 10));
        assert!(
            crop_region(
                &full,
                &Region {
                    x: 200,
                    y: 200,
                    w: 10,
                    h: 10,
                },
            )
            .is_err()
        );
    }
}
