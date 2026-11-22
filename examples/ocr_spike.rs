//! 本机 RapidOCR 接入验证：输出可直接复制的文本。
//! 用法：cargo run --release --example ocr_spike -- 图片路径

#![allow(dead_code)]
#[path = "../src/config.rs"]
mod config;
#[path = "../src/ocr.rs"]
mod ocr;

/// 验证 Rust → Python → RapidOCR → Rust 的完整链路。
fn main() -> anyhow::Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("请指定图片路径"))?;
    let image = image::open(path)?.into_rgba8();
    let started = std::time::Instant::now();
    let result = ocr::recognize_image(&image);
    ocr::shutdown();
    println!("{}", result?);
    eprintln!("耗时 {:.2}s", started.elapsed().as_secs_f64());
    Ok(())
}
