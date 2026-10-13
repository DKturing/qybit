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
    use crate::fake::{AUDCLNT_E_UNSUPPORTED_FORMAT, fake_backend};

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
        assert_eq!(r.fault, Some((1, Fault::Configure)));
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
