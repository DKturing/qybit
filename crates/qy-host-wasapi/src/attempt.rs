//! WASAPI 独占协商记录（M0 T5 起；M2 T1 起实现 `qy_player::AttemptRecord`）。
//!
//! 协商依据只记录独占 `IsFormatSupported`/`Initialize` 的 HRESULT，不使用共享混音格式。
//! WASAPI 独占流没有读回实际格式的接口，因此已应用参数只含 `GetBufferSize` 的端点缓冲
//! 帧数，格式标为不可查询——`GetMixFormat` 是共享混音格式，不得冒充独占流格式。

use qy_core::{DeviceFormat, SourceFormat};
use qy_player::{AppliedParams, AttemptRecord, RateProbe};

/// 独占 `IsFormatSupported` 的 `AUDCLNT_E_UNSUPPORTED_FORMAT`（格式不受支持）。
pub const AUDCLNT_E_UNSUPPORTED_FORMAT: i32 = 0x8889_0008_u32 as i32;

/// 采样率探测查询的常见采样率。
pub const PROBE_RATES: [u32; 13] = [
    8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000,
    352_800, 384_000,
];

/// 全部尝试的 `IsFormatSupported` 都是 [`AUDCLNT_E_UNSUPPORTED_FORMAT`] 时，对 [`PROBE_RATES`] 中
/// 其他采样率，按同一声道布局与同样的候选（[`attempts`]，`container` 同协商）逐个只读查询，
/// `supported` 为一次独占格式查询，每个采样率找到一个被接受的格式即止。其他失败（设备忙、初始化
/// 失败等）不探测，返回 `None`。探测只用于说明原因，不改用其他采样率。
pub fn probe_rates(
    n: &Negotiation,
    container: Option<u8>,
    mut supported: impl FnMut(&DeviceFormat, Descriptor) -> bool,
) -> Option<Box<RateProbe>> {
    if n.attempts.is_empty()
        || n.attempts
            .iter()
            .any(|a| a.is_format_supported != Some(AUDCLNT_E_UNSUPPORTED_FORMAT))
    {
        return None;
    }
    let src = n.source;
    let mut probe = RateProbe {
        checked: Vec::new(),
        accepted: Vec::new(),
        stereo_accepted: None,
    };
    let mut accepts = |rate: u32, layout: qy_core::ChannelLayout| -> bool {
        let at = match src.encoding() {
            qy_core::SampleEncoding::Int => SourceFormat::with_layout(rate, layout, src.bits()),
            e => SourceFormat::float(rate, layout, e),
        };
        let Ok(at) = at else {
            return false;
        };
        attempts(&at, container)
            .into_iter()
            .any(|(cv, d)| device_format(&at, cv).is_ok_and(|f| supported(&f, d)))
    };
    for rate in PROBE_RATES {
        if rate == src.sample_rate() {
            continue;
        }
        probe.checked.push(rate);
        if accepts(rate, src.layout()) {
            probe.accepted.push(rate);
        }
    }
    // 单声道在所有采样率都被拒：按立体声再查一遍（含源采样率），说明 --mono-to-stereo 能否播放。
    if probe.accepted.is_empty() && src.layout() == qy_core::ChannelLayout::MONO {
        let mut rates: Vec<u32> = PROBE_RATES.to_vec();
        if !rates.contains(&src.sample_rate()) {
            rates.push(src.sample_rate());
            rates.sort_unstable();
        }
        probe.stereo_accepted = Some(
            rates
                .into_iter()
                .filter(|&r| accepts(r, qy_core::ChannelLayout::STEREO))
                .collect(),
        );
    }
    Some(Box::new(probe))
}

/// 协商全部失败时的说明：探测表明其他采样率可用时，写明是采样率不受支持；单声道在所有采样率都被
/// 拒而立体声可用时，写明是不接受单声道。
pub fn failure_message(n: &Negotiation) -> String {
    match &n.rate_probe {
        Some(p) if !p.accepted.is_empty() => format!(
            "端点在独占模式下拒绝了 {} Hz 的全部候选格式：该采样率不受支持（本程序不重采样）",
            n.source.sample_rate()
        ),
        Some(p) if p.stereo_accepted.as_ref().is_some_and(|st| !st.is_empty()) => {
            let rate = n.source.sample_rate();
            if p.stereo_accepted
                .as_ref()
                .is_some_and(|st| st.contains(&rate))
            {
                "端点在独占模式下不接受单声道 [FC]（同采样率的立体声可用，见声道探测）".into()
            } else {
                format!("端点在独占模式下不接受单声道 [FC]，也不支持 {rate} Hz（本程序不重采样）")
            }
        }
        _ if n.source.encoding().is_dsd() => format!(
            "端点在独占模式下不接受 DoP 所需的 PCM {} Hz（32/24、24/24）；WASAPI 没有原生 DSD，本程序不把 DSD 转为 PCM",
            n.source.sample_rate() * 2
        ),
        _ if n.source.encoding().is_float() => format!(
            "端点在独占模式下不接受 {} 位浮点（IEEE_FLOAT）；严格路径不转换（可明确开启浮点转整数，样本会改变）",
            n.source.bits()
        ),
        _ => "设备不支持任何可无损承载该源格式的独占格式".into(),
    }
}

/// 候选 (C, V) 对应的设备格式：整数源为 WASAPI PCM（小端、高位对齐），浮点源为同一浮点编码的
/// IEEE_FLOAT（F3）。
pub fn device_format(
    src: &SourceFormat,
    (c, v): (u8, u8),
) -> Result<DeviceFormat, qy_pcm_format::FormatError> {
    // D1：WASAPI 没有原生 DSD，DSD 源只用 DoP：C32/V24（高位对齐）或 C24/V24。
    if src.encoding().is_dsd() {
        let container = match (c, v) {
            (32, 24) => qy_core::DopContainer::S32LeMsb,
            (24, 24) => qy_core::DopContainer::S24_3Le,
            _ => return Err(qy_pcm_format::FormatError::ContainerBits),
        };
        if !src.layout().is_wave_order() {
            return Err(qy_pcm_format::FormatError::LayoutOrder);
        }
        return DeviceFormat::dsd(
            src.sample_rate(),
            src.layout(),
            qy_core::DsdTransport::Dop(container),
        );
    }
    if src.encoding().is_float() {
        DeviceFormat::wasapi_float(src.sample_rate(), src.layout(), src.encoding())
    } else {
        DeviceFormat::wasapi_layout(src.sample_rate(), src.layout(), c, v)
    }
}

/// 设备格式候选 (C, V)：V ≥ N 中 V 最小者优先，同 V 先小容器。
/// 只列出不改变样本值的表示（pack 只做 N ≤ V 的放置）。浮点源（F3）只有同一浮点编码一项
/// （C = V = 浮点位宽）。
pub fn candidates(src: &SourceFormat) -> Vec<(u8, u8)> {
    // D1：DSD 只用 DoP 的两种容器（候选的 (C, V) 指 DoP 的 PCM 容器）。
    if src.encoding().is_dsd() {
        return vec![(32, 24), (24, 24)];
    }
    if src.encoding().is_float() {
        return vec![(src.bits(), src.bits())];
    }
    [(16u8, 16u8), (24, 24), (32, 24), (32, 32)]
        .into_iter()
        .filter(|&(_, v)| v >= src.bits())
        .collect()
}

/// 格式描述类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Descriptor {
    Extensible,
    WaveFormatEx,
}

/// 协商尝试顺序：每个候选 (C, V) 先用 EXTENSIBLE；16/16 且源为标准立体声 `[FL FR]` 时，紧接着用
/// 同参数的 WAVEFORMATEX 再试一次。`container` 为 `Some(k)` 时只保留容器位宽为 k 的候选。
///
/// 两种描述承载的字节完全相同（同一 pack 结果），只是格式结构不同；有的驱动只接受其中一种，所以都试。
/// 单声道不用 WAVEFORMATEX：它没有声道掩码，声音送到哪个扬声器由驱动决定。真机上出现过驱动拒绝
/// 单声道 EXTENSIBLE `[FC]`、接受 WAVEFORMATEX 单声道并消耗全部数据、却没有可闻输出的情况，
/// 按成功报告比明确报错更糟。单声道只用 EXTENSIBLE `[FC]`，被拒即按“设备不支持”报告。
pub fn attempts(src: &SourceFormat, container: Option<u8>) -> Vec<((u8, u8), Descriptor)> {
    let ex16 = src.layout() == qy_core::ChannelLayout::STEREO;
    let mut v = Vec::new();
    for cv in candidates(src) {
        if container.is_some_and(|k| k != cv.0) {
            continue;
        }
        v.push((cv, Descriptor::Extensible));
        if cv == (16, 16) && ex16 {
            v.push((cv, Descriptor::WaveFormatEx));
        }
    }
    v
}

/// 一次格式尝试的记录（每次尝试使用新的音频客户端）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub format: DeviceFormat,
    pub descriptor: Descriptor,
    /// 独占模式 `IsFormatSupported` 的 HRESULT。
    pub is_format_supported: Option<i32>,
    /// `Initialize` 的 HRESULT。
    pub initialize: Option<i32>,
    /// 缓冲未对齐后按对齐周期在新客户端上重试 `Initialize` 的 HRESULT。
    pub aligned_retry: Option<i32>,
    /// 成功初始化后 `GetBufferSize` 的端点缓冲帧数。
    pub buffer_frames: Option<u32>,
    /// 使用的周期（100 ns）。
    pub period_hns: Option<i64>,
}

/// WASAPI 协商记录。
pub type Negotiation = qy_player::Negotiation<Attempt>;
/// WASAPI 打开失败。
pub type OpenFailure = qy_player::OpenFailure<Attempt>;
/// WASAPI 播放报告。
pub type PlaybackReport = qy_player::PlaybackReport<Attempt>;

fn hr_text(v: Option<i32>) -> String {
    v.map_or_else(|| "—".into(), |x| format!("0x{:08X}", x as u32))
}

pub fn descriptor_text(d: Descriptor) -> &'static str {
    match d {
        Descriptor::Extensible => "WAVEFORMATEXTENSIBLE",
        Descriptor::WaveFormatEx => "WAVEFORMATEX",
    }
}

impl AttemptRecord for Attempt {
    fn requested(&self) -> DeviceFormat {
        self.format
    }

    fn describe(&self) -> String {
        let mut s = format!(
            "（{}）IsFormatSupported(独占) {}，Initialize {}",
            descriptor_text(self.descriptor),
            hr_text(self.is_format_supported),
            hr_text(self.initialize),
        );
        if self.aligned_retry.is_some() {
            s.push_str(&format!("，对齐重试 {}", hr_text(self.aligned_retry)));
        }
        if let Some(b) = self.buffer_frames {
            s.push_str(&format!("，端点缓冲 {b} 帧"));
        }
        if let Some(p) = self.period_hns {
            s.push_str(&format!("，周期 {p}×100ns"));
        }
        s
    }

    fn applied(&self) -> Option<AppliedParams> {
        Some(AppliedParams {
            format: None,
            buffer_frames: self.buffer_frames,
            period_frames: None,
            note: Some("独占流无读回格式的接口，不以共享混音格式 GetMixFormat 代替"),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use qy_core::Fault;
    use qy_player::testing::{data_chunk, fixture, fixture_path};
    use qy_player::{HostResources, play};

    use super::*;
    use crate::attempt::PROBE_RATES;
    use crate::fake::{
        AUDCLNT_E_UNSUPPORTED_FORMAT, WasapiNegotiator, fake_backend, fake_backend_with,
    };

    const ALL: [(u8, u8); 4] = [(16, 16), (24, 24), (32, 24), (32, 32)];

    fn run(supported: &[(u8, u8)], names: &[&str]) -> PlaybackReport {
        let Ok(mut res) = HostResources::new() else {
            panic!("res");
        };
        let mut b = fake_backend(supported);
        let files: Vec<PathBuf> = names.iter().map(|n| fixture_path(n)).collect();
        let r = play(&mut b, &mut res, &files);
        let outs = b.outputs();
        if let Some(o) = outs.first() {
            assert_eq!(o.report.priority.reverted, Some(Ok(())));
        }
        if r.fault.is_some() {
            // 失败前的曲目已完整排空。
            if let Some(o) = outs.first() {
                let raw = data_chunk(&fixture(names[0]));
                assert_eq!(&o.rendered[..raw.len()], &raw[..]);
            }
        }
        r
    }

    fn src(rate: u32, mask: u32, bits: u8) -> SourceFormat {
        let Some(l) = qy_core::ChannelLayout::from_mask(mask) else {
            panic!("{mask:#x}");
        };
        match SourceFormat::with_layout(rate, l, bits) {
            Ok(f) => f,
            Err(e) => panic!("{e:?}"),
        }
    }

    /// 尝试顺序：16/16 的 EXTENSIBLE 之后紧跟 WAVEFORMATEX，仅限标准立体声。
    /// F3：浮点源只请求同一浮点编码的 EXTENSIBLE IEEE_FLOAT；端点不接受时说明原因（严格路径
    /// 不转换），不请求整数格式。
    #[test]
    fn float_source_requests_ieee_float_only() {
        use qy_core::SampleEncoding;
        let name = "float/f32_stereo_44k_tag3.wav";
        let files = [fixture_path(name)];
        let accept = WasapiNegotiator {
            floats: &[SampleEncoding::Float32],
            ..WasapiNegotiator::default()
        };
        let Ok(mut res) = HostResources::new() else {
            panic!("res");
        };
        let mut b = fake_backend_with(&ALL, accept);
        let r = play(&mut b, &mut res, &files);
        assert!(r.strict_path_intact(), "{}", r.summary());
        let n = &r.streams[0].negotiation;
        assert_eq!(n.attempts.len(), 1);
        let a = &n.attempts[0];
        assert_eq!(a.format.encoding(), SampleEncoding::Float32);
        assert_eq!(a.descriptor, Descriptor::Extensible);
        let mut b = fake_backend(&ALL);
        let r = play(&mut b, &mut res, &files);
        assert!(r.streams.is_empty(), "{}", r.summary());
        let f = &r.open_failures[0];
        assert!(
            f.negotiation
                .attempts
                .iter()
                .all(|a| a.format.encoding() == SampleEncoding::Float32),
            "{f:?}"
        );
        assert!(f.message.contains("不接受 32 位浮点"), "{}", f.message);
    }

    #[test]
    fn attempts_add_waveformatex_for_standard_stereo_16_only() {
        use Descriptor::{Extensible as X, WaveFormatEx as W};
        assert_eq!(
            attempts(&src(44_100, 0x3, 16), None),
            vec![
                ((16, 16), X),
                ((16, 16), W),
                ((24, 24), X),
                ((32, 24), X),
                ((32, 32), X)
            ]
        );
        assert_eq!(
            attempts(&src(48_000, 0x3, 16), Some(16)),
            vec![((16, 16), X), ((16, 16), W)]
        );
        assert_eq!(
            attempts(&src(48_000, 0x3, 16), Some(32)),
            vec![((32, 24), X), ((32, 32), X)]
        );
        // 单声道（扬声器位置由驱动决定）、非 [FL FR] 的双声道、多声道：不用 WAVEFORMATEX。
        for mask in [0x4, 0x1, 0x600, 0x3F] {
            assert!(
                attempts(&src(48_000, mask, 16), None)
                    .iter()
                    .all(|&(_, d)| d == X),
                "{mask:#x}"
            );
        }
        // 源超过 16 位：没有 16/16 候选，也就没有 WAVEFORMATEX。
        assert_eq!(
            attempts(&src(96_000, 0x3, 24), None),
            vec![((24, 24), X), ((32, 24), X), ((32, 32), X)]
        );
    }

    /// 驱动拒绝 16/16 EXTENSIBLE、接受 WAVEFORMATEX：立体声改用 WAVEFORMATEX 播放，设备字节与
    /// data 块相同；单声道不改用 WAVEFORMATEX，按无可用格式明确失败。
    #[test]
    fn waveformatex_fallback_plays_same_bytes() {
        let Ok(mut res) = HostResources::new() else {
            panic!("res");
        };
        let backend = || {
            fake_backend_with(
                &[(16, 16)],
                WasapiNegotiator {
                    extensible_16_rejected: true,
                    ..WasapiNegotiator::default()
                },
            )
        };
        let name = "pcm16_stereo_44k.wav";
        let mut b = backend();
        let r = play(&mut b, &mut res, &[fixture_path(name)]);
        assert!(r.finished && r.software_path_intact(), "{r:?}");
        let n = &r.streams[0].negotiation;
        let d: Vec<_> = n.attempts.iter().map(|a| a.descriptor).collect();
        assert_eq!(d, [Descriptor::Extensible, Descriptor::WaveFormatEx]);
        assert_eq!(
            n.attempts[0].is_format_supported,
            Some(AUDCLNT_E_UNSUPPORTED_FORMAT)
        );
        assert_eq!(n.chosen, Some(1));
        let s = r.summary();
        assert!(
            s.contains("（WAVEFORMATEX）IsFormatSupported(独占) 0x00000000"),
            "{s}"
        );
        let outs = b.outputs();
        let raw = data_chunk(&fixture(name));
        assert_eq!(outs.len(), 1);
        assert_eq!(&outs[0].rendered[..raw.len()], &raw[..]);

        for mono in ["pcm16_mono_8k_fmt18.wav", "ext16_mono_left_mask.wav"] {
            let mut b = backend();
            let r = play(&mut b, &mut res, &[fixture_path(mono)]);
            assert!(!r.finished, "{mono}");
            let n = &r.open_failures[0].negotiation;
            assert!(
                n.attempts
                    .iter()
                    .all(|a| a.descriptor == Descriptor::Extensible),
                "{mono}"
            );
        }
    }

    /// 端点只接受 44.1/48 kHz：16 kHz 单声道（开启单声道复制）全部被拒，探测其他采样率后写明是
    /// 采样率不受支持并列出接受的采样率；没有曲目播放，因此不记“声道已改变”；协商记录写明源文件
    /// 实际为单声道。48 kHz 的同一类文件照常播放，字节为每样本两份。
    #[test]
    fn unsupported_rate_is_explained_by_probe() {
        static RATES: [u32; 2] = [44_100, 48_000];
        let backend = || {
            fake_backend_with(
                &ALL,
                WasapiNegotiator {
                    rates: Some(&RATES),
                    ..WasapiNegotiator::default()
                },
            )
        };
        let Ok(mut res) = HostResources::new() else {
            panic!("res");
        };
        res.engine
            .set_channel_policy(qy_core::engine::ChannelPolicy::DuplicateMono);
        // 8 kHz 单声道 16 位夹具（按单声道复制以 2 声道请求）。
        let mut b = backend();
        let r = play(&mut b, &mut res, &[fixture_path("pcm16_mono_8k_fmt18.wav")]);
        assert!(!r.finished);
        assert!(r.channels_altered.is_empty(), "{r:?}");
        let f = &r.open_failures[0];
        assert_eq!(
            f.message,
            "端点在独占模式下拒绝了 8000 Hz 的全部候选格式：该采样率不受支持（本程序不重采样）"
        );
        let Some(p) = &f.negotiation.rate_probe else {
            panic!("应探测");
        };
        assert_eq!(p.accepted, vec![44_100, 48_000]);
        assert_eq!(p.checked.len(), PROBE_RATES.len() - 1);
        assert!(!p.checked.contains(&8_000));
        let s = r.summary();
        assert!(
            s.contains("源格式：8000 Hz / 1 声道 / 16 位有效（单声道复制到左右两路：按 2 声道 [FL FR] 请求设备）"),
            "{s}"
        );
        assert!(
            s.contains("端点接受 44100、48000 Hz，拒绝 11025、16000"),
            "{s}"
        );
        assert!(s.contains("源的 8000 Hz 不受支持；本程序不重采样"), "{s}");
        assert!(!s.contains("声道已改变"), "{s}");

        // 位深不支持（只接受 16/16，源 24 位）：所有采样率都被拒，说明问题不只在采样率。
        let mut b = fake_backend_with(
            &[(16, 16)],
            WasapiNegotiator {
                rates: Some(&RATES),
                ..WasapiNegotiator::default()
            },
        );
        let r = play(
            &mut b,
            &mut res,
            &[fixture_path("pcm24_mono_48k_odd_list.wav")],
        );
        let f = &r.open_failures[0];
        assert_eq!(f.message, "设备不支持任何可无损承载该源格式的独占格式");
        assert!(
            f.negotiation
                .rate_probe
                .as_ref()
                .is_some_and(|p| p.accepted.is_empty())
        );
        assert!(r.summary().contains("问题不只在采样率"));
    }

    /// 端点拒绝一切单声道、立体声只接受 44.1/48 kHz（严格路径，未开启复制）：48 kHz 单声道的报告
    /// 写明不接受单声道、立体声可用并提示 --mono-to-stereo；8 kHz 单声道写明即使开启也不能播放。
    #[test]
    fn mono_rejection_is_explained_by_stereo_probe() {
        static RATES: [u32; 2] = [44_100, 48_000];
        let Ok(mut res) = HostResources::new() else {
            panic!("res");
        };
        let backend = || {
            fake_backend_with(
                &ALL,
                WasapiNegotiator {
                    rates: Some(&RATES),
                    mono_rejected: true,
                    ..WasapiNegotiator::default()
                },
            )
        };
        let mut b = backend();
        let r = play(
            &mut b,
            &mut res,
            &[fixture_path("pcm24_mono_48k_odd_list.wav")],
        );
        let f = &r.open_failures[0];
        assert_eq!(
            f.message,
            "端点在独占模式下不接受单声道 [FC]（同采样率的立体声可用，见声道探测）"
        );
        let p = f.negotiation.rate_probe.as_ref();
        assert_eq!(
            p.and_then(|p| p.stereo_accepted.clone()),
            Some(vec![44_100, 48_000])
        );
        let s = r.summary();
        assert!(
            s.contains("按立体声 [FL FR] 查询，端点接受 44100、48000 Hz（含源的 48000 Hz）。可明确开启“单声道复制到左右两路”"),
            "{s}"
        );

        let mut b = backend();
        let r = play(&mut b, &mut res, &[fixture_path("pcm16_mono_8k_fmt18.wav")]);
        assert_eq!(
            r.open_failures[0].message,
            "端点在独占模式下不接受单声道 [FC]，也不支持 8000 Hz（本程序不重采样）"
        );
        let s = r.summary();
        assert!(
            s.contains("不含源的 8000 Hz。即使开启单声道复制到左右两路也不能播放"),
            "{s}"
        );

        // 立体声源被拒时不做声道探测。
        let mut b = fake_backend_with(
            &[(16, 16)],
            WasapiNegotiator {
                rates: Some(&RATES),
                ..WasapiNegotiator::default()
            },
        );
        let r = play(&mut b, &mut res, &[fixture_path("pcm24_stereo_large.wav")]);
        let p = r.open_failures[0].negotiation.rate_probe.as_ref();
        assert!(p.is_some_and(|p| p.accepted.is_empty() && p.stereo_accepted.is_none()));
    }

    /// O1 §2：端点物理扬声器只作提示。掩码包含设备流的位置：只记录掩码；缺少位置：提示可能听不到；
    /// 属性缺失：写明“端点未报告”；掩码 0（DIRECTOUT）：写明含义。四种情况都照常协商与播放，设备
    /// 字节与 data 块相同。
    #[test]
    fn physical_speakers_only_warn() {
        use qy_player::PhysicalSpeakers::{Mask, NotReported};
        let cases = [
            (
                "pcm16_stereo_44k.wav",
                Mask(0x3F),
                "0x3F [FL FR FC LFE BL BR]",
                false,
            ),
            ("pcm16_mono_8k_fmt18.wav", Mask(0x3), "0x3 [FL FR]", true),
            ("pcm16_mono_8k_fmt18.wav", Mask(0x4), "0x4 [FC]", false),
            ("pcm16_stereo_44k.wav", Mask(0x4), "0x4 [FC]", true),
            ("pcm16_mono_8k_fmt18.wav", NotReported, "端点未报告", false),
            (
                "pcm16_stereo_44k.wav",
                Mask(0),
                "KSAUDIO_SPEAKER_DIRECTOUT",
                false,
            ),
        ];
        let Ok(mut res) = HostResources::new() else {
            panic!("res");
        };
        for (name, speakers, shown, warn) in cases {
            let mut b = fake_backend_with(
                &ALL,
                WasapiNegotiator {
                    speakers,
                    ..WasapiNegotiator::default()
                },
            );
            let r = play(&mut b, &mut res, &[fixture_path(name)]);
            let what = format!("{name} {speakers:?}");
            assert!(r.finished && r.software_path_intact(), "{what}: {r:?}");
            assert_eq!(r.streams[0].negotiation.speakers, speakers, "{what}");
            let s = r.summary();
            assert!(s.contains(shown), "{what}: {s}");
            assert_eq!(s.contains("可能听不到"), warn, "{what}: {s}");
            if warn && name.contains("mono") {
                assert!(
                    s.contains("端点报告的物理扬声器为 [FL FR]，设备流的 [FC] 不在其中"),
                    "{s}"
                );
            }
            let raw = data_chunk(&fixture(name));
            let outs = b.outputs();
            assert_eq!(&outs[0].rendered[..raw.len()], &raw[..], "{what}");
        }
    }

    #[test]
    fn unsupported_format_records_exclusive_hresult_without_fallback() {
        // 设备只支持 16/16 与 32/24：32 位源无可无损承载的格式 → 配置失败，不降精度。
        let r = run(
            &[(16, 16), (32, 24)],
            &[
                "pcm16_stereo_44k.wav",
                "pcm32_stereo_96k.wav",
                "pcm16_stereo_44k.wav",
            ],
        );
        assert_eq!(
            r.fault,
            Some((1, qy_core::engine::EngineFault::Core(Fault::Configure)))
        );
        assert!(!r.finished && !r.software_path_intact());
        assert_eq!(r.open_failures.len(), 1);
        let n = &r.open_failures[0].negotiation;
        assert_eq!(n.chosen, None);
        assert_eq!(n.attempts.len(), 1);
        assert_eq!(
            n.attempts[0].is_format_supported,
            Some(AUDCLNT_E_UNSUPPORTED_FORMAT)
        );
        assert_eq!(r.streams.len(), 1);
    }

    #[test]
    fn summary_reports_negotiation_and_conclusion() {
        let r = run(&ALL, &["ext20in24_mono_center.wav"]);
        let s = r.summary();
        assert!(s.contains("源格式：96000 Hz / 1 声道 / 20 位有效"), "{s}");
        assert!(s.contains("请求 #0：96000 Hz / 1 声道 / 容器 24 位 / 有效 24 位（WAVEFORMATEXTENSIBLE）IsFormatSupported(独占) 0x00000000，Initialize 0x00000000"), "{s}");
        assert!(s.contains("← 已协商"));
        // 已应用参数：只有端点缓冲帧数；格式明确标为不可查询，不用 GetMixFormat 代替。
        assert!(
            s.contains("已应用参数：格式不可查询，缓冲 960 帧（独占流无读回格式的接口，不以共享混音格式 GetMixFormat 代替）"),
            "{s}"
        );
        assert!(s.contains("回录证据：无（未回录）"), "{s}");
        assert!(s.contains("MMCSS “Pro Audio”：注册成功，已撤销"), "{s}");
        assert!(s.contains("欠载 0 次 / 0 帧，已排空"));
        assert!(s.contains("不证明设备端 bit-perfect"));
        assert_eq!(r.exit_code(), 0);

        let r = run(&[(16, 16)], &["pcm24_stereo_large.wav"]);
        let s = r.summary();
        assert!(s.contains("打开失败"), "{s}");
        assert!(s.contains("IsFormatSupported(独占) 0x88890008"), "{s}");
        assert!(s.contains("曲目 1 失败：Configure"), "{s}");
        assert!(s.contains("结论：播放未完整完成。"));
        assert!(!s.contains("无欠载、无错误"));
        assert_eq!(r.exit_code(), 1);
    }

    /// D1：DSD 源在 WASAPI 上只有 DoP：候选为 32/24 与 24/24 两种容器，只用 EXTENSIBLE，设备格式
    /// 为 DoP（字率不变，设备帧率为 2 倍）；没有 DoP 可用时的说明写明不转 PCM。
    #[test]
    fn dsd_uses_dop_only() {
        let Ok(src) = SourceFormat::dsd(2_822_400, qy_core::ChannelLayout::STEREO) else {
            panic!("dsd");
        };
        assert_eq!(candidates(&src), vec![(32, 24), (24, 24)]);
        let a = attempts(&src, None);
        assert!(a.iter().all(|(_, d)| *d == Descriptor::Extensible));
        let Ok(f) = device_format(&src, (32, 24)) else {
            panic!("dop");
        };
        assert_eq!(
            f.dsd_transport(),
            Some(qy_core::DsdTransport::Dop(qy_core::DopContainer::S32LeMsb))
        );
        assert_eq!((f.sample_rate(), f.device_rate()), (88_200, 176_400));
        assert!(f.accepts(&src));
        assert!(device_format(&src, (16, 16)).is_err());
        let n = Negotiation::new(src);
        assert!(failure_message(&n).contains("不把 DSD 转为 PCM"));
    }
}
