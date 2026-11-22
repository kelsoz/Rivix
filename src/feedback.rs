//! 操作完成反馈：内嵌短快门声，异步播放，不依赖提示窗口或磁盘读取。

/// 图片、文字或颜色已成功复制时立即发声；音频设备不可用不阻断操作。
pub fn completed() {
    #[cfg(target_os = "windows")]
    {
        use windows::{
            Win32::Media::Audio::{PlaySoundA, SND_ASYNC, SND_MEMORY, SND_NODEFAULT},
            core::PCSTR,
        };
        // 异步播放要求缓冲区长期有效，因此直接引用编入程序的静态 WAV。
        static SOUND: &[u8] = include_bytes!("../assets/shutter.wav");
        unsafe {
            let _ = PlaySoundA(
                PCSTR(SOUND.as_ptr()),
                None,
                SND_ASYNC | SND_MEMORY | SND_NODEFAULT,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    //! 检查内嵌音效格式，测试不实际播放声音。
    #[test]
    fn shutter_is_short_pcm_wave() {
        let sound = include_bytes!("../assets/shutter.wav");
        assert_eq!(&sound[..4], b"RIFF");
        assert_eq!(&sound[8..12], b"WAVE");
        assert!(sound.len() > 44 && sound.len() < 20000);
    }
}
