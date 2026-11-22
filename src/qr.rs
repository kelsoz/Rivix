//! 冻结截图的本地二维码解码、选区命中与网页地址校验，不修改截图像素。
use image::RgbaImage;

/// 已解码二维码，边界使用所属屏幕图片的物理像素坐标。
#[derive(Clone, Debug)]
pub(crate) struct QrCode {
    /// 原始 UTF-8 内容；非网页二维码也允许复制。
    pub(crate) text: String,
    /// 左、上、右、下边界。
    pub(crate) bounds: [f32; 4],
}

impl QrCode {
    /// 仅完整落入物理像素选区的二维码可触发，避免打开框外或残缺的二维码。
    pub(crate) fn inside(&self, rect: (f32, f32, f32, f32)) -> bool {
        let (x, y, w, h) = rect;
        self.bounds[0] >= x
            && self.bounds[1] >= y
            && self.bounds[2] <= x + w
            && self.bounds[3] <= y + h
    }
}

/// 后台扫描一次原始冻结画面，按从上到下、从左到右排序，不落盘或联网。
pub(crate) fn detect(pixels: &RgbaImage) -> Vec<QrCode> {
    let gray = image::imageops::grayscale(pixels);
    let mut decoder = quircs::Quirc::default();
    let mut found = Vec::new();
    for code in decoder
        .identify(gray.width() as usize, gray.height() as usize, gray.as_raw())
        .flatten()
    {
        let Ok(data) = code.decode() else {
            continue;
        };
        let Ok(text) = String::from_utf8(data.payload) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        let bounds = [
            code.corners.iter().map(|p| p.x).min().unwrap_or(0) as f32,
            code.corners.iter().map(|p| p.y).min().unwrap_or(0) as f32,
            code.corners.iter().map(|p| p.x).max().unwrap_or(0) as f32,
            code.corners.iter().map(|p| p.y).max().unwrap_or(0) as f32,
        ];
        found.push(QrCode { text, bounds });
    }
    found.sort_by(|a, b| {
        a.bounds[1]
            .total_cmp(&b.bounds[1])
            .then(a.bounds[0].total_cmp(&b.bounds[0]))
    });
    found
}

/// 只允许 HTTP/HTTPS 网页交给系统浏览器，其他内容仍可作为文本复制。
pub(crate) fn web_url(text: &str) -> Option<url::Url> {
    let url = url::Url::parse(text.trim()).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.host_str().is_some()).then_some(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_4k_frame_finds_multiple_codes_in_reading_order() {
        let code = image::load_from_memory(include_bytes!("../assets/qr-test.png"))
            .unwrap()
            .into_rgba8();
        let mut frame = RgbaImage::from_pixel(3840, 2160, image::Rgba([255, 255, 255, 255]));
        image::imageops::overlay(&mut frame, &code, 2500, 1300);
        image::imageops::overlay(&mut frame, &code, 80, 100);
        let started = std::time::Instant::now();
        let codes = detect(&frame);
        println!(
            "4K QR scan: {:.2}ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
        assert_eq!(codes.len(), 2);
        assert!(codes[0].bounds[0] < 400.0 && codes[0].bounds[1] < 400.0);
        assert!(codes[1].bounds[0] > 2500.0 && codes[1].bounds[1] > 1300.0);
    }

    #[test]
    fn selection_must_contain_whole_code() {
        let code = QrCode {
            text: "test".into(),
            bounds: [10.0, 20.0, 110.0, 120.0],
        };
        assert!(code.inside((10.0, 20.0, 100.0, 100.0)));
        assert!(!code.inside((11.0, 20.0, 100.0, 100.0)));
        assert!(!code.inside((0.0, 0.0, 109.0, 120.0)));
    }

    #[test]
    fn only_web_addresses_can_launch() {
        assert!(web_url(" https://example.com/path?q=test ").is_some());
        assert!(web_url("http://example.com").is_some());
        for text in [
            "file:///C:/Windows",
            "javascript:alert(1)",
            "ms-settings:test",
            "data:text/html,test",
            "WIFI:S:test;",
            "plain text",
            "https://",
        ] {
            assert!(web_url(text).is_none(), "{text}");
        }
    }

    #[test]
    fn real_qr_and_blank_image_leave_original_pixels_unchanged() {
        let pixels = image::load_from_memory(include_bytes!("../assets/qr-test.png"))
            .unwrap()
            .into_rgba8();
        let original = pixels.clone();
        let codes = detect(&pixels);
        assert_eq!(codes.len(), 1);
        assert_eq!(codes[0].text, "https://example.com/rupix-qr-test");
        assert_eq!(pixels, original);
        assert!(
            detect(&RgbaImage::from_pixel(
                200,
                200,
                image::Rgba([255, 255, 255, 255])
            ))
            .is_empty()
        );
    }
}
