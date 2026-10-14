//! WASAPI 假协商（仅 `cfg(test)`）：在 `qy_player::testing::FakeBackend` 上产生与真实后端
//! 同形的协商记录（独占 `IsFormatSupported`/`Initialize` 的 HRESULT、描述类型），渲染仍走
//! 真实整包循环与假平台 API。

use qy_core::{DeviceFormat, SourceFormat};
use qy_player::testing::{FakeBackend, Negotiator};

use crate::attempt::{Attempt, Descriptor, Negotiation, OpenFailure, attempts};

pub const AUDCLNT_E_UNSUPPORTED_FORMAT: i32 = 0x8889_0008_u32 as i32;

/// 按设备支持列表逐项协商。
#[derive(Debug, Clone, Copy, Default)]
pub struct WasapiNegotiator {
    /// 只尝试该 (C, V) 与描述类型（矩阵逐项测试用）。
    pub forced: Option<((u8, u8), Descriptor)>,
    /// 模拟只接受 WAVEFORMATEX 描述 16/16 的驱动：16/16 的 EXTENSIBLE 请求被拒。
    pub extensible_16_rejected: bool,
}

impl Negotiator for WasapiNegotiator {
    type Attempt = Attempt;

    fn negotiate(
        &mut self,
        source: &SourceFormat,
        supported: &[(u8, u8)],
    ) -> Result<(DeviceFormat, Negotiation), OpenFailure> {
        let mut negotiation = Negotiation::new(*source);
        let list: Vec<((u8, u8), Descriptor)> = match self.forced {
            Some((cv, d)) if cv.1 >= source.bits() => vec![(cv, d)],
            Some(_) => Vec::new(),
            None => attempts(source, None),
        };
        for ((c, v), descriptor) in list {
            let Ok(format) =
                DeviceFormat::wasapi_layout(source.sample_rate(), source.layout(), c, v)
            else {
                continue;
            };
            let ok = supported.contains(&(c, v))
                && !(self.extensible_16_rejected
                    && (c, v) == (16, 16)
                    && descriptor == Descriptor::Extensible);
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
        Err(OpenFailure {
            negotiation,
            message: "设备不支持该格式".into(),
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
