//! OCR 模块：后台调用独立 RapidOCR 3.10 CUDA 管道接口，直接返回可复制文本。

use image::RgbaImage;
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Mutex, OnceLock},
    time::Duration,
};

/// 独立 OCR 项目，不向截图软件目录复制环境或模型。
const OCR_ROOT: &str = "F:/Agent_Workspace/02_projects/RapidOCR";
/// 复用独立项目验证过的 CUDA12/cuDNN9 动态库目录。
const CUDA_DLL_DIR: &str =
    "F:/Agent_Workspace/02_projects/Unlimited-OCR/.venv/Lib/site-packages/torch/lib";

/// 使用已验证的 RapidOCR 3.10.0 GPU 环境，保持旧 CPU 环境不变。
fn python_path() -> PathBuf {
    PathBuf::from(OCR_ROOT).join(".venv-gpu/Scripts/python.exe")
}

/// 外部管道接口沿用 RGBA 输入与逐行 UTF-8 JSON 输出。
fn worker_path() -> PathBuf {
    PathBuf::from(OCR_ROOT).join("rapidocr_worker.py")
}

/// 当前是否配置为外部 HTTP OCR API 模式。
pub fn is_api_mode() -> bool {
    let cfg = crate::config::load().ocr;
    cfg.mode.eq_ignore_ascii_case("api")
        && cfg.api_url.as_deref().is_some_and(|u| !u.trim().is_empty())
}

/// 仅检查外部依赖位置或 API 地址；实际 CUDA 后端由接口在识别时严格核验。
pub fn models_available() -> bool {
    if is_api_mode() {
        return true;
    }
    python_path().is_file()
        && worker_path().is_file()
        && std::path::Path::new(CUDA_DLL_DIR).is_dir()
}

/// 按需启动的子进程缓存；使用独立 CUDA OCR 环境，空闲 180 秒由子进程退出。
static WORKER: OnceLock<Mutex<Option<Worker>>> = OnceLock::new();
/// 单个 OCR 进程及其输入输出管道，不创建系统服务。
struct Worker {
    child: Child,
    io: Option<(ChildStdin, BufReader<ChildStdout>)>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        // 退出、错误或超时都回收子进程，避免遗留推理进程。
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Worker {
    /// 创建无控制台的子进程，模型仍在第一次请求时才加载。
    fn start() -> anyhow::Result<Self> {
        let mut command = Command::new(python_path());
        command
            .arg("-u")
            .arg(worker_path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command.spawn()?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("OCR 输入管道不可用"))?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("OCR 输出管道不可用"))?;
        Ok(Self {
            child,
            io: Some((input, BufReader::new(output))),
        })
    }
    /// 将原始像素送入已有引擎；独立 IO 线程让超时可以真正终止卡住的写入或识别。
    fn recognize(&mut self, image: &RgbaImage) -> anyhow::Result<String> {
        let (mut input, mut output) = self
            .io
            .take()
            .ok_or_else(|| anyhow::anyhow!("OCR 管道已关闭"))?;
        let (width, height) = image.dimensions();
        let pixels = image.as_raw().clone();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<String> {
                let size = u32::try_from(pixels.len() + 8)?;
                input.write_all(&size.to_le_bytes())?;
                input.write_all(&width.to_le_bytes())?;
                input.write_all(&height.to_le_bytes())?;
                input.write_all(&pixels)?;
                input.flush()?;
                let mut line = String::new();
                // 限制响应上限，异常输出不会无限占用内存。
                let count = output
                    .by_ref()
                    .take(16 * 1024 * 1024)
                    .read_line(&mut line)?;
                anyhow::ensure!(
                    count > 0 && line.ends_with('\n'),
                    "OCR 进程已退出或响应不完整"
                );
                let response: serde_json::Value = serde_json::from_str(&line)?;
                if let Some(error) = response.get("error").and_then(|v| v.as_str()) {
                    anyhow::bail!("{error}");
                }
                Ok(response
                    .get("text")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("OCR 响应格式错误"))?
                    .trim()
                    .to_string())
            })();
            let _ = tx.send((input, output, result));
        });
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok((input, output, result)) => {
                self.io = Some((input, output));
                result
            }
            Err(_) => {
                let _ = self.child.kill();
                anyhow::bail!("OCR 超时，已停止本次识别");
            }
        }
    }
}

/// 后台识别：若配置为外部 API 则走 HTTP POST；否则走本机 RapidOCR 3.10 CUDA 原始像素管道。
pub fn recognize_image(image: &RgbaImage) -> anyhow::Result<String> {
    let cfg = crate::config::load().ocr;
    if cfg.mode.eq_ignore_ascii_case("api")
        && let Some(url) = cfg
            .api_url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
    {
        return recognize_via_api(image, url);
    }
    anyhow::ensure!(
        python_path().is_file()
            && worker_path().is_file()
            && std::path::Path::new(CUDA_DLL_DIR).is_dir(),
        "RapidOCR CUDA 依赖缺失，请检查独立项目的 GPU Python、rapidocr_worker.py 和 CUDA DLL 目录"
    );
    anyhow::ensure!(
        image.width() > 0 && image.height() > 0 && image.as_raw().len() <= 256 * 1024 * 1024 - 8,
        "OCR 图片尺寸无效或过大"
    );
    let mut slot = WORKER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| anyhow::anyhow!("OCR 状态锁不可用"))?;
    if let Some(worker) = slot.as_mut()
        && worker.child.try_wait()?.is_some()
    {
        *slot = None;
    }
    if slot.is_none() {
        *slot = Some(Worker::start()?);
    }
    let result = slot
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("OCR 未就绪"))?
        .recognize(image);
    if result.is_err() {
        *slot = None;
    }
    result
}

/// 通过外部 HTTP API 识别图片文字（POST JSON `{"image": "<base64_png>"}`）。
fn recognize_via_api(image: &RgbaImage, url: &str) -> anyhow::Result<String> {
    use image::{ImageEncoder, codecs::png::PngEncoder};
    let mut png = Vec::new();
    PngEncoder::new(&mut png).write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        image::ExtendedColorType::Rgba8,
    )?;
    let b64 = encode_base64(&png);
    let payload = serde_json::json!({ "image": b64 }).to_string();
    let tmp_path = std::env::temp_dir().join(format!("rivix-ocr-{}.json", std::process::id()));
    std::fs::write(&tmp_path, payload)?;

    let mut cmd = Command::new("curl");
    cmd.args(["-sS", "-m", "20", "-X", "POST"])
        .args(["-H", "Content-Type: application/json"])
        .arg("-d")
        .arg(format!("@{}", tmp_path.display()))
        .arg(url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output();
    let _ = std::fs::remove_file(&tmp_path);
    let out = out.map_err(|e| anyhow::anyhow!("调用 HTTP OCR 失败：{e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("HTTP OCR 请求异常：{}", err.trim());
    }
    let body = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
        if let Some(s) = json
            .get("text")
            .or_else(|| json.get("result"))
            .or_else(|| json.get("data"))
            .and_then(|v| v.as_str())
        {
            return Ok(s.trim().to_string());
        }
        if let Some(arr) = json.get("data").and_then(|v| v.as_array()) {
            let joined = arr
                .iter()
                .filter_map(|item| {
                    item.as_str()
                        .or_else(|| item.get("text").and_then(|t| t.as_str()))
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !joined.is_empty() {
                return Ok(joined);
            }
        }
    }
    Ok(body)
}

/// 标准 Base64 编码（用于 HTTP OCR API 图片传输，避免额外依赖）。
fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((triple >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(triple & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// 正常退出时回收缓存子进程，及时释放 CUDA 模型；由应用退出后台任务调用。
pub fn shutdown() {
    if let Some(cache) = WORKER.get() {
        let mut slot = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = None;
    }
}

#[cfg(test)]
mod tests {
    /// 显式运行的本机 GPU 集成验证，普通单测不加载模型或占用显卡。
    #[test]
    #[ignore = "需要本机独立 RapidOCR CUDA 环境"]
    fn cuda_worker_real_roundtrip() {
        assert!(super::models_available());
        let sample = image::open(std::path::Path::new(super::OCR_ROOT).join("samples/small.png"))
            .unwrap()
            .into_rgba8();
        let blank = image::RgbaImage::from_pixel(300, 100, image::Rgba([255, 255, 255, 255]));
        assert!(super::recognize_image(&image::RgbaImage::new(0, 0)).is_err());
        for (index, image) in [&sample, &sample, &sample, &blank].into_iter().enumerate() {
            let started = std::time::Instant::now();
            let text = super::recognize_image(image).unwrap();
            eprintln!(
                "CUDA request {}: {:.2}ms",
                index + 1,
                started.elapsed().as_secs_f64() * 1000.0
            );
            if index < 3 {
                assert!(text.contains("12345"));
                assert!(text.contains("截图文字识别测试"));
            } else {
                assert!(text.is_empty());
            }
        }
        // 模拟空闲进程已退出，下一次请求必须重新创建进程而非复用断管道。
        {
            let mut slot = super::WORKER.get().unwrap().lock().unwrap();
            let worker = slot.as_mut().unwrap();
            worker.child.kill().unwrap();
            worker.child.wait().unwrap();
        }
        assert!(super::recognize_image(&sample).unwrap().contains("12345"));
        // 释放缓存并触发 Drop，测试完成不保留 CUDA 子进程和模型。
        super::shutdown();
        super::shutdown();
        assert!(super::WORKER.get().unwrap().lock().unwrap().is_none());
    }
}
