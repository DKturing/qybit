//! ALSA 协商记录：每个候选格式一次尝试，记录请求、alsa-lib 返回值、读回参数与核对结果。

use qy_core::DeviceFormat;
use qy_pcm_format::alsa::{AlsaFormat, HwApplied, HwRequest};
use qy_player::{AppliedParams, AttemptRecord};

use crate::pcm::PcmError;

/// 一次格式尝试。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub alsa_format: AlsaFormat,
    /// 请求的设备格式（由 ALSA 格式、源采样率与声道构成）。
    pub format: DeviceFormat,
    /// 设置参数时失败的 alsa-lib 调用。
    pub error: Option<PcmError>,
    /// 实际请求（含设备接受的周期/缓冲）。
    pub request: Option<HwRequest>,
    /// `snd_pcm_hw_params_current` 读回值。
    pub applied: Option<HwApplied>,
    /// 读回与请求不一致项的说明。
    pub mismatch: Option<String>,
    /// 声道映射的设置与读回结果（F1）。
    pub chmap: Option<String>,
}

/// ALSA 协商记录。
pub type Negotiation = qy_player::Negotiation<Attempt>;
/// ALSA 打开失败。
pub type OpenFailure = qy_player::OpenFailure<Attempt>;
/// ALSA 播放报告。
pub type PlaybackReport = qy_player::PlaybackReport<Attempt>;

impl Attempt {
    /// 设置成功且读回一致。
    pub fn accepted(&self) -> bool {
        self.error.is_none() && self.applied.is_some() && self.mismatch.is_none()
    }
}

impl AttemptRecord for Attempt {
    fn requested(&self) -> DeviceFormat {
        self.format
    }

    fn describe(&self) -> String {
        let mut s = format!("（{}）", self.alsa_format);
        match (&self.error, &self.applied, &self.mismatch) {
            (Some(e), _, _) => s.push_str(&format!("设置失败：{e}")),
            (None, Some(a), None) => {
                s.push_str("hw_params 成功，读回一致");
                s.push_str(&format!("：精确采样率 {}/{}", a.rate_num, a.rate_den));
                if let Some(b) = a.significant_bits {
                    s.push_str(&format!("，有效位 {b}"));
                }
                if let Some(c) = &self.chmap {
                    s.push_str(&format!("；{c}"));
                }
            }
            (None, _, Some(m)) => s.push_str(&format!("读回不一致：{m}")),
            (None, None, None) => s.push_str("未设置"),
        }
        s
    }

    fn applied(&self) -> Option<AppliedParams> {
        let a = self.applied?;
        let format = AlsaFormat::from_code(a.format_code)
            .ok()
            .filter(|_| a.rate_den == 1)
            .and_then(|f| {
                let ch = u16::try_from(a.channels).ok()?;
                // 布局取已读回核对的设备映射（即请求布局），声道数须与读回一致。
                let layout = self.request.map(|r| r.layout)?;
                (layout.channels() == ch)
                    .then(|| f.device_format_layout(a.rate_num, layout).ok())
                    .flatten()
            });
        Some(AppliedParams {
            format,
            buffer_frames: Some(a.buffer_frames),
            period_frames: Some(a.period_frames),
            note: Some("由 snd_pcm_hw_params_current 读回"),
        })
    }
}
