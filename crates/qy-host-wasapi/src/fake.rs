//! WASAPI 假协商（仅 `cfg(test)`）：在 `qy_player::testing::FakeBackend` 上产生与真实后端
//! 同形的协商记录（独占 `IsFormatSupported`/`Initialize` 的 HRESULT、描述类型），渲染仍走
//! 真实整包循环与假平台 API。

use qy_core::{DeviceFormat, SampleEncoding, SourceFormat};
use qy_player::PhysicalSpeakers;
use qy_player::testing::{FakeBackend, Negotiator};

pub use crate::attempt::AUDCLNT_E_UNSUPPORTED_FORMAT;
use crate::attempt::{
    Attempt, Descriptor, Negotiation, OpenFailure, attempts, device_format, failure_message,
    probe_rates,
};

/// 按设备支持列表逐项协商。
#[derive(Debug, Clone, Copy, Default)]
pub struct WasapiNegotiator {
    /// 只尝试该 (C, V) 与描述类型（矩阵逐项测试用）。
    pub forced: Option<((u8, u8), Descriptor)>,
    /// 模拟只接受 WAVEFORMATEX 描述 16/16 的驱动：16/16 的 EXTENSIBLE 请求被拒。
    pub extensible_16_rejected: bool,
    /// 端点物理扬声器属性的读取结果（O1 §2）。
    pub speakers: PhysicalSpeakers,
    /// 端点接受的采样率；`None` 为全部接受。
    pub rates: Option<&'static [u32]>,
    /// 模拟拒绝一切单声道格式的端点。
    pub mono_rejected: bool,
    /// 端点接受的浮点编码（F3）。
    pub floats: &'static [SampleEncoding],
}

impl WasapiNegotiator {
    fn accepts(&self, supported: &[(u8, u8)], f: &DeviceFormat, d: Descriptor) -> bool {
        let cv = (f.container_bits(), f.valid_bits());
        let format_ok = if f.encoding().is_float() {
            self.floats.contains(&f.encoding())
        } else {
            supported.contains(&cv)
        };
        format_ok
            && self.rates.is_none_or(|r| r.contains(&f.sample_rate()))
            && !(self.mono_rejected && f.channels() == 1)
            && !(self.extensible_16_rejected && cv == (16, 16) && d == Descriptor::Extensible)
    }
}

impl Negotiator for WasapiNegotiator {
    type Attempt = Attempt;

    fn negotiate(
        &mut self,
        source: &SourceFormat,
        supported: &[(u8, u8)],
    ) -> Result<(DeviceFormat, Negotiation), OpenFailure> {
        let mut negotiation = Negotiation::new(*source);
        negotiation.speakers = self.speakers;
        let list: Vec<((u8, u8), Descriptor)> = match self.forced {
            Some((cv, d)) if cv.1 >= source.bits() => vec![(cv, d)],
            Some(_) => Vec::new(),
            None => attempts(source, None),
        };
        for (cv, descriptor) in list {
            let Ok(format) = device_format(source, cv) else {
                continue;
            };
            let ok = self.accepts(supported, &format, descriptor);
            negotiation.attempts.push(Attempt {
                format,
                descriptor,
                is_format_supported: Some(if ok { 0 } else { AUDCLNT_E_UNSUPPORTED_FORMAT }),
                initialize: ok.then_some(0),
                aligned_retry: None,
                buffer_frames: ok.then_some(source.sample_rate() / 100),
                period_hns: ok.then_some(100_000),
            });
            if ok {
                negotiation.chosen = Some(negotiation.attempts.len() - 1);
                return Ok((format, negotiation));
            }
        }
        if self.forced.is_none() {
            negotiation.rate_probe =
                probe_rates(&negotiation, None, |f, d| self.accepts(supported, f, d));
        }
        let message = if negotiation.rate_probe.is_some() {
            failure_message(&negotiation)
        } else {
            "设备不支持该格式".into()
        };
        Err(OpenFailure {
            negotiation,
            message,
        })
    }
}

/// WASAPI 形状的假后端：整包假设备，优先级记录名为 MMCSS。
pub fn fake_backend(supported: &[(u8, u8)]) -> FakeBackend<WasapiNegotiator> {
    fake_backend_with(supported, WasapiNegotiator::default())
}

/// 同 [`fake_backend`]，协商行为由 `negotiator` 给出。
pub fn fake_backend_with(
    supported: &[(u8, u8)],
    negotiator: WasapiNegotiator,
) -> FakeBackend<WasapiNegotiator> {
    let mut b = FakeBackend::with_negotiator(supported, negotiator);
    b.priority.label = Some(crate::MMCSS_LABEL);
    b
}
