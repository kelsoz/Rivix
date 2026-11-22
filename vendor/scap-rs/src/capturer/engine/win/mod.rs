//! Windows 平台屏幕捕获：无光标抓屏，按每屏白电平把 HDR 浮点帧还原为 SDR。
use crate::{
    capturer::{Area, Options, Point, Resolution, Size},
    frame::{BGRAFrame, Frame, FrameType, RGBxFrame},
    targets::{self, Target},
};
mod hdr;
use hdr::{convert_hdr, hdr_lut};
use std::cmp;
use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};
use windows_capture::{
    capture::{CaptureControl, Context, GraphicsCaptureApiHandler},
    frame::Frame as WCFrame,
    graphics_capture_api::InternalCaptureControl,
    monitor::Monitor as WCMonitor,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings as WCSettings,
    },
    window::Window as WCWindow,
};

/// Windows 捕获器结构体
/// 实现 GraphicsCaptureApiHandler trait 处理帧数据
#[derive(Debug)]
struct Capturer {
    pub tx: mpsc::Sender<anyhow::Result<Frame>>, // 帧数据发送通道
    pub crop: Option<Area>,                      // 裁剪区域
    // 捕获开始时生成半精度到 sRGB 的查找表，避免逐像素执行幂运算。
    hdr_lut: Option<Vec<u8>>,
    output_bgra: bool,
}

/// 捕获设置枚举，支持窗口或显示器捕获
#[derive(Clone)]
enum Settings {
    Window(WCSettings<FlagStruct, WCWindow>),   // 窗口捕获设置
    Display(WCSettings<FlagStruct, WCMonitor>), // 显示器捕获设置
}

/// Windows 捕获流结构体
/// 管理捕获会话的生命周期
pub struct WCStream {
    settings: Settings, // 捕获设置
    capture_control: Option<CaptureControl<Capturer, Box<dyn std::error::Error + Send + Sync>>>, // 捕获控制句柄
}

/// 实现 GraphicsCaptureApiHandler trait
/// 处理帧到达和流关闭事件
impl GraphicsCaptureApiHandler for Capturer {
    type Flags = FlagStruct;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    /// 创建新的捕获器实例
    fn new(context: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            tx: context.flags.tx,
            crop: context.flags.crop,
            hdr_lut: context.flags.sdr_white_scale.map(hdr_lut),
            output_bgra: context.flags.output_bgra,
        })
    }

    /// 帧到达回调函数
    /// 处理捕获的帧数据并发送到通道
    fn on_frame_arrived(
        &mut self,
        frame: &mut WCFrame,
        _: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        // 获取帧的颜色格式
        let color_format = frame.color_format();

        // 根据是否裁剪获取帧数据
        let (width, height, data) = match &self.crop {
            Some(cropped_area) => {
                // 计算裁剪区域坐标
                let start_x = cropped_area.origin.x as u32;
                let start_y = cropped_area.origin.y as u32;
                let end_x = (cropped_area.origin.x + cropped_area.size.width) as u32;
                let end_y = (cropped_area.origin.y + cropped_area.size.height) as u32;

                // 裁剪帧缓冲区
                let cropped_buffer = frame.buffer_crop(start_x, start_y, end_x, end_y)?;

                // 获取原始帧数据
                let mut nopadding_buffer = Vec::new();
                let raw_frame_buffer = cropped_buffer.as_nopadding_buffer(&mut nopadding_buffer);

                (
                    cropped_area.size.width as i32,
                    cropped_area.size.height as i32,
                    raw_frame_buffer.to_vec(),
                )
            }
            None => {
                // 获取完整帧数据
                let width = frame.width() as i32;
                let height = frame.height() as i32;
                let frame_buffer = frame.buffer()?;
                let mut scratch = Vec::new();
                let raw_frame_buffer = frame_buffer.as_nopadding_buffer(&mut scratch);
                (width, height, raw_frame_buffer.to_vec())
            }
        };

        // 获取当前时间戳
        let display_time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as u64;

        // 根据颜色格式创建对应的帧对象
        let frame = match color_format {
            ColorFormat::Rgba16F => {
                let lut = self.hdr_lut.as_deref().ok_or("HDR 白电平转换表缺失")?;
                let data = convert_hdr(&data, lut, self.output_bgra)?;
                if self.output_bgra {
                    Frame::BGRA(BGRAFrame {
                        display_time,
                        width,
                        height,
                        data,
                    })
                } else {
                    Frame::RGBx(RGBxFrame {
                        display_time,
                        width,
                        height,
                        data,
                    })
                }
            }
            ColorFormat::Rgba8 => Frame::RGBx(RGBxFrame {
                display_time,
                width,
                height,
                data,
            }),
            ColorFormat::Bgra8 => Frame::BGRA(BGRAFrame {
                display_time,
                width,
                height,
                data,
            }),
        };

        // 发送帧数据到通道
        Ok(self.tx.send(Ok(frame))?)
    }

    /// 流关闭回调函数
    fn on_closed(&mut self) -> Result<(), Self::Error> {
        log::debug!("屏幕捕获流已关闭");
        Ok(())
    }
}

impl WCStream {
    /// 启动屏幕捕获
    pub fn start_capture(&mut self) {
        let cc = match &self.settings {
            Settings::Display(st) => Capturer::start_free_threaded(st.to_owned()).unwrap(),
            Settings::Window(st) => Capturer::start_free_threaded(st.to_owned()).unwrap(),
        };

        self.capture_control = Some(cc)
    }

    /// 停止屏幕捕获
    pub fn stop_capture(&mut self) {
        let capture_control = self.capture_control.take().unwrap();
        let _ = capture_control.stop();
    }
}

/// 查询目标显示器的 HDR 白电平倍率；普通 SDR 屏返回空值。
fn target_white_scale(target: &Target) -> anyhow::Result<Option<f32>> {
    use windows::Win32::{
        Devices::Display::*,
        Foundation::ERROR_INSUFFICIENT_BUFFER,
        Graphics::Gdi::{
            GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFOEXW, MonitorFromWindow,
        },
    };
    // HMONITOR 与 DisplayConfig 的适配器/目标 ID 不同，必须用 GDI 设备名配对。
    unsafe {
        let monitor = match target {
            Target::Display(display) => display.raw_handle,
            Target::Window(window) => {
                MonitorFromWindow(window.raw_handle, MONITOR_DEFAULTTONEAREST)
            }
        };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !GetMonitorInfoW(monitor, (&raw mut info).cast()).as_bool() {
            anyhow::bail!("查询显示器设备名失败");
        }
        // 拔插显示器可能改变缓冲区大小，最多重取三次，不缓存失效拓扑。
        for _ in 0..3 {
            let (mut path_count, mut mode_count) = (0, 0);
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
                .ok()?;
            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
            let result = QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                None,
            );
            if result == ERROR_INSUFFICIENT_BUFFER {
                continue;
            }
            result.ok()?;
            for path in paths.iter().take(path_count as usize) {
                let mut name = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
                name.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: std::mem::size_of_val(&name) as u32,
                    adapterId: path.sourceInfo.adapterId,
                    id: path.sourceInfo.id,
                };
                let result = DisplayConfigGetDeviceInfo(&mut name.header);
                if result != 0 {
                    anyhow::bail!("查询显示源失败：{result}");
                }
                if name.viewGdiDeviceName != info.szDevice {
                    continue;
                }
                let mut color = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO::default();
                color.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
                    size: std::mem::size_of_val(&color) as u32,
                    adapterId: path.targetInfo.adapterId,
                    id: path.targetInfo.id,
                };
                let result = DisplayConfigGetDeviceInfo(&mut color.header);
                if result != 0 {
                    anyhow::bail!("查询 HDR 状态失败：{result}");
                }
                if color.Anonymous.value & 2 == 0 {
                    return Ok(None);
                }
                let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL::default();
                white.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                    size: std::mem::size_of_val(&white) as u32,
                    adapterId: path.targetInfo.adapterId,
                    id: path.targetInfo.id,
                };
                let result = DisplayConfigGetDeviceInfo(&mut white.header);
                if result != 0 || white.SDRWhiteLevel == 0 {
                    anyhow::bail!("查询 SDR 白电平失败：{result}");
                }
                // Windows 的 1000 对应 80 nit，不能把屏幕增强后的白点直接当作 sRGB 白色。
                let scale = white.SDRWhiteLevel as f32 / 1000.0;
                log::info!("HDR 浮点捕获，SDR 白电平倍率 {scale}");
                return Ok(Some(scale));
            }
            anyhow::bail!("未找到捕获屏幕的显示配置");
        }
        anyhow::bail!("显示拓扑持续变化，无法读取 HDR 配置")
    }
}

/// 标志结构体，包含通道和裁剪区域
#[derive(Clone, Debug)]
struct FlagStruct {
    pub tx: mpsc::Sender<anyhow::Result<Frame>>, // 帧数据发送通道
    pub crop: Option<Area>,                      // 裁剪区域
    sdr_white_scale: Option<f32>,
    output_bgra: bool,
}

/// 创建 Windows 捕获器
/// 根据选项配置创建捕获流和目标
pub fn create_capturer(
    options: &Options,
    tx: mpsc::Sender<anyhow::Result<Frame>>,
) -> (WCStream, Target) {
    // 获取捕获目标，默认使用主显示器
    let target = options
        .target
        .clone()
        .unwrap_or_else(|| Target::Display(targets::get_main_display().expect("获取主显示器失败")));

    // 每次截图重新读取对应显示器，避免 HDR 开关、白电平和混合屏共用旧值。
    let sdr_white_scale = match target_white_scale(&target) {
        Ok(scale) => scale,
        Err(error) => {
            log::warn!("读取显示器 HDR 状态失败，保留八位捕获：{error}");
            None
        }
    };
    let output_bgra = matches!(options.output_type, FrameType::BGRAFrame);
    let color_format = match (sdr_white_scale, output_bgra) {
        (Some(_), _) => ColorFormat::Rgba16F,
        (None, true) => ColorFormat::Bgra8,
        (None, false) => ColorFormat::Rgba8,
    };

    // Rupix 只需要无光标截图，避免 rgpui 默认录入的指针固定在冻结帧上。
    let show_cursor = CursorCaptureSettings::WithoutCursor;

    // 根据目标类型创建捕获设置
    let settings = match target.clone() {
        Target::Display(display) => Settings::Display(WCSettings::new(
            WCMonitor::from_raw_hmonitor(display.raw_handle.0),
            show_cursor,
            DrawBorderSettings::Default,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            color_format,
            FlagStruct {
                tx,
                crop: Some(get_crop_area(options)),
                sdr_white_scale,
                output_bgra,
            },
        )),
        Target::Window(window) => Settings::Window(WCSettings::new(
            WCWindow::from_raw_hwnd(window.raw_handle.0),
            show_cursor,
            DrawBorderSettings::Default,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            color_format,
            FlagStruct {
                tx,
                crop: Some(get_crop_area(options)),
                sdr_white_scale,
                output_bgra,
            },
        )),
    };

    (
        WCStream {
            settings,
            capture_control: None,
        },
        target,
    )
}

/// 获取输出帧尺寸
/// 根据裁剪区域和分辨率选项计算最终输出尺寸
pub fn get_output_frame_size(options: &Options) -> [u32; 2] {
    let crop_area = get_crop_area(options);

    let mut output_width = (crop_area.size.width) as u32;
    let mut output_height = (crop_area.size.height) as u32;

    // 应用分辨率限制
    match options.output_resolution {
        Resolution::Captured => {} // 使用原始分辨率
        _ => {
            let [resolved_width, resolved_height] = options
                .output_resolution
                .value((crop_area.size.width as f32) / (crop_area.size.height as f32));
            // 取较小值以确保不超过目标分辨率
            output_width = cmp::min(output_width, resolved_width);
            output_height = cmp::min(output_height, resolved_height);
        }
    }

    // 确保尺寸为偶数
    output_width -= output_width % 2;
    output_height -= output_height % 2;

    [output_width, output_height]
}

/// 获取绝对值坐标（考虑缩放因子）
fn get_absolute_value(value: f64, scale_factor: f64) -> f64 {
    let value = (value * scale_factor).floor();
    value + value % 2.0
}

/// 获取裁剪区域
/// 根据选项和目标尺寸计算实际裁剪区域
pub fn get_crop_area(options: &Options) -> Area {
    // 获取捕获目标
    let target = options
        .target
        .clone()
        .unwrap_or_else(|| Target::Display(targets::get_main_display().expect("获取主显示器失败")));

    // 获取目标尺寸
    let (width, height) = targets::get_target_dimensions(&target);

    // 获取缩放因子
    let scale_factor = targets::get_scale_factor(&target);

    // 如果指定了裁剪区域，则应用缩放
    options
        .crop_area
        .as_ref()
        .map(|val| Area {
            origin: Point {
                x: get_absolute_value(val.origin.x, scale_factor),
                y: get_absolute_value(val.origin.y, scale_factor),
            },
            size: Size {
                width: get_absolute_value(val.size.width, scale_factor),
                height: get_absolute_value(val.size.height, scale_factor),
            },
        })
        // 否则使用完整目标区域
        .unwrap_or_else(|| Area {
            origin: Point { x: 0.0, y: 0.0 },
            size: Size {
                width: width as f64,
                height: height as f64,
            },
        })
}
