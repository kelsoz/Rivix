//! HDR 浮点像素到 SDR sRGB 的白电平换算与格式校验。

/// 解码 IEEE 754 半精度分量，不额外引入数值依赖。
fn half_float(bits: u16) -> f32 {
    let sign = if bits & 0x8000 == 0 { 1.0 } else { -1.0 };
    let exponent = (bits >> 10) & 31;
    let mantissa = (bits & 1023) as f32;
    match exponent {
        0 => sign * mantissa * 2.0_f32.powi(-24),
        31 if mantissa == 0.0 => sign * f32::INFINITY,
        31 => f32::NAN,
        _ => sign * (1.0 + mantissa / 1024.0) * 2.0_f32.powi(exponent as i32 - 15),
    }
}

/// 把所有半精度编码预先映射到八位 sRGB，SDR 白点以下保持原始层次。
pub(super) fn hdr_lut(white_scale: f32) -> Vec<u8> {
    (0..=u16::MAX)
        .map(|bits| {
            let value = half_float(bits) / white_scale;
            let encoded = if value <= 0.0031308 {
                value * 12.92
            } else {
                1.055 * value.powf(1.0 / 2.4) - 0.055
            };
            // shortcut: 当前交付标准 SDR 截图，超出 SDR 白点的真实 HDR 高光截断；需保留 HDR 素材时接入完整色调映射。
            (encoded.clamp(0.0, 1.0) * 255.0).round() as u8
        })
        .collect()
}

/// 按原输出约定转换浮点 RGBA，拒绝不完整像素而不发布损坏帧。
pub(super) fn convert_hdr(data: &[u8], lut: &[u8], bgra: bool) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        data.len().is_multiple_of(8) && lut.len() == 65536,
        "HDR 像素数据或转换表长度无效"
    );
    let mut output = Vec::with_capacity(data.len() / 2);
    for pixel in data.as_chunks::<8>().0 {
        let channel = |index| lut[u16::from_le_bytes([pixel[index], pixel[index + 1]]) as usize];
        let (red, green, blue) = (channel(0), channel(2), channel(4));
        // 显示器截图是最终合成画面；统一不透明，避免浮点 alpha 的舍入误差影响贴图。
        let pixel = if bgra {
            [blue, green, red, 255]
        } else {
            [red, green, blue, 255]
        };
        output.extend_from_slice(&pixel);
    }
    Ok(output)
}

#[cfg(test)]
mod hdr_tests {
    use super::*;

    #[test]
    fn restores_measured_hdr_gray_levels() {
        let lut = hdr_lut(3.55);
        // 本机 FP16 固定灰阶实测值，覆盖旧路径会截断的高于 1.0 的分量。
        let raw = [
            0x0000_u16, 0x2a90, 0x31d2, 0x36a4, 0x3a21, 0x3cfd, 0x3f7c, 0x414a, 0x422f, 0x4319,
        ];
        let expected = [0_u8, 32, 64, 96, 128, 160, 192, 224, 240, 255];
        for (bits, value) in raw.into_iter().zip(expected) {
            assert!((lut[bits as usize] as i16 - value as i16).abs() <= 1);
        }
        assert_eq!(hdr_lut(1.0)[0x3c00], 255);
        assert_eq!(hdr_lut(2.0)[0x4000], 255);
    }

    #[test]
    fn preserves_channel_order_and_rejects_invalid_frames() {
        let lut = hdr_lut(1.0);
        let pixel = [0x00, 0x3c, 0x00, 0x38, 0, 0, 0, 0x3c];
        assert_eq!(
            convert_hdr(&pixel, &lut, false).unwrap(),
            [255, 188, 0, 255]
        );
        assert_eq!(convert_hdr(&pixel, &lut, true).unwrap(), [0, 188, 255, 255]);
        assert!(convert_hdr(&pixel[..7], &lut, false).is_err());
        assert!(convert_hdr(&pixel, &[], false).is_err());
        assert_eq!(lut[0xbc00], 0);
        assert_eq!(lut[0x7c00], 255);
        assert_eq!(lut[0x7e00], 0);
        assert_eq!(half_float(1), 2.0_f32.powi(-24));
    }
}
