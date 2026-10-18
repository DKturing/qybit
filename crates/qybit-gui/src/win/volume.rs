//! 设备硬件音量的显示（GUI T5）：Windows 端点音量的读数与滑块位置。

/// 输出设备的音量状态（GUI T5）。只调节设备在硬件上实现的音量，送到设备的数据不变。
#[derive(Debug, Clone, PartialEq)]
pub enum VolumeView {
    /// 设备在硬件上实现了音量：位置 0.0–1.0、读回的 dB 值、硬件静音。
    Hardware { scalar: f32, db: f32, muted: bool },
    /// 设备没有硬件音量（系统的软件音量对独占流无效，QyBit 也不使用它）。
    NoHardware,
    /// 读不到（无设备、模拟设备、系统接口失败）。
    Unavailable(String),
}

impl VolumeView {
    /// 标签文字。
    pub fn text(&self) -> String {
        match self {
            Self::Hardware { db, muted, .. } => {
                let mute = if *muted { "（已静音）" } else { "" };
                format!("设备硬件音量 {db:.1} dB{mute}")
            }
            Self::NoHardware => "设备没有硬件音量，请在 DAC 或功放上调节".to_string(),
            Self::Unavailable(why) => format!("设备音量不可用：{why}"),
        }
    }

    /// 滑块位置（0–100）；不可调节时为 `None`（滑块不可用）。
    pub fn position(&self) -> Option<u32> {
        match self {
            Self::Hardware { scalar, .. } if scalar.is_finite() => {
                Some((scalar.clamp(0.0, 1.0) * 100.0).round() as u32)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::VolumeView;

    #[test]
    fn volume_view_text_and_position() {
        let v = VolumeView::Hardware {
            scalar: 0.456,
            db: -12.34,
            muted: false,
        };
        assert_eq!(v.text(), "设备硬件音量 -12.3 dB");
        assert_eq!(v.position(), Some(46));
        let m = VolumeView::Hardware {
            scalar: 1.5,
            db: 0.0,
            muted: true,
        };
        assert_eq!(m.text(), "设备硬件音量 0.0 dB（已静音）");
        assert_eq!(m.position(), Some(100));
        let nan = VolumeView::Hardware {
            scalar: f32::NAN,
            db: 0.0,
            muted: false,
        };
        assert_eq!(nan.position(), None);
        assert_eq!(VolumeView::NoHardware.position(), None);
        assert!(VolumeView::NoHardware.text().contains("没有硬件音量"));
        let u = VolumeView::Unavailable("模拟设备".into());
        assert_eq!(u.text(), "设备音量不可用：模拟设备");
        assert_eq!(u.position(), None);
    }
}
