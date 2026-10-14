//! WASAPI 独占协商记录（M0 T5 起；M2 T1 起实现 `qy_player::AttemptRecord`）。
//!
//! 协商依据只记录独占 `IsFormatSupported`/`Initialize` 的 HRESULT，不使用共享混音格式。
//! WASAPI 独占流没有读回实际格式的接口，因此已应用参数只含 `GetBufferSize` 的端点缓冲
//! 帧数，格式标为不可查询——`GetMixFormat` 是共享混音格式，不得冒充独占流格式。

use qy_core::{DeviceFormat, SourceFormat};
use qy_player::{AppliedParams, AttemptRecord};

/// 设备格式候选 (C, V)：V ≥ N 中 V 最小者优先，同 V 先小容器。
/// 只列出不改变样本值的表示（pack 只做 N ≤ V 的放置）。
pub fn candidates(src: &SourceFormat) -> Vec<(u8, u8)> {
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
}
