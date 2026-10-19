//! D1：DSD 格式层与 pack。参考字节由测试按各传输方式的规范**逐位**写出（时间顺序的位列表），
//! 不调用被测的 pack，也不用 32 位字的移位运算：
//! - 原生 `DSD_U8`：每设备帧每声道 8 位，最早的位在字节最高位；
//! - `DSD_U16_*` / `DSD_U32_*`：每设备帧每声道 2 / 4 个这样的字节，`_BE` 最早的字节在前，`_LE`
//!   最早的字节在后（ALSA `asound.h` 的字节序；与 MPD 的 DSD_U16 / DSD_U32 打包相同）；
//! - DoP v1.1：每设备帧每声道一个 24 位样本 = 标记字节 + 较早的 DSD 字节 + 较晚的 DSD 字节，
//!   标记逐帧交替 0x05 / 0xFA；按容器（`S24_3LE` 三字节小端、`S32_LE` 左移 8 位、`S24_LE` 低 24 位
//!   且高字节为符号扩展）写成小端字节。

use qy_pcm_format::pack::{Pcm, check, pack};
use qy_pcm_format::{
    ChannelLayout, DeviceFormat, DopContainer, DsdTransport, DsdWord, FormatError, SampleEncoding,
    SourceFormat, Speaker,
};

const ALL: [DsdTransport; 8] = [
    DsdTransport::Native(DsdWord::U8),
    DsdTransport::Native(DsdWord::U16Le),
    DsdTransport::Native(DsdWord::U16Be),
    DsdTransport::Native(DsdWord::U32Le),
    DsdTransport::Native(DsdWord::U32Be),
    DsdTransport::Dop(DopContainer::S24_3Le),
    DsdTransport::Dop(DopContainer::S32LeMsb),
    DsdTransport::Dop(DopContainer::S24LeLsb),
];

/// 固定种子的伪随机 32 位字。
fn words(n: usize, seed: u64) -> Vec<i32> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u32 as i32
        })
        .collect()
}

/// 一个声道的 DSD 位（时间顺序）：每个字最高位在前。
fn bits_of(w: &[i32], ch: usize, channels: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for frame in w.chunks_exact(channels) {
        let v = frame[ch] as u32;
        for i in (0..32).rev() {
            out.push(((v >> i) & 1) as u8);
        }
    }
    out
}

/// 从位列表取第 `i` 个 8 位字节（最早的位在最高位）。
fn byte(bits: &[u8], i: usize) -> u8 {
    bits[i * 8..i * 8 + 8]
        .iter()
        .fold(0u8, |a, &b| (a << 1) | b)
}

/// 独立参考：设备字节。`order[j]` 为设备第 j 声道取源的哪个声道。
fn reference(t: DsdTransport, w: &[i32], channels: usize, order: &[usize]) -> Vec<u8> {
    let bits: Vec<Vec<u8>> = (0..channels).map(|c| bits_of(w, c, channels)).collect();
    let bytes_per_channel = w.len() / channels * 4;
    let mut out = Vec::new();
    match t {
        DsdTransport::Native(word) => {
            let n = match word {
                DsdWord::U8 => 1,
                DsdWord::U16Le | DsdWord::U16Be => 2,
                DsdWord::U32Le | DsdWord::U32Be => 4,
            };
            let le = matches!(word, DsdWord::U16Le | DsdWord::U32Le);
            for f in 0..bytes_per_channel / n {
                for &src in order {
                    let mut s: Vec<u8> = (0..n).map(|k| byte(&bits[src], f * n + k)).collect();
                    if le {
                        s.reverse();
                    }
                    out.extend_from_slice(&s);
                }
            }
        }
        DsdTransport::Dop(c) => {
            for f in 0..bytes_per_channel / 2 {
                let marker = if f % 2 == 0 { 0x05u8 } else { 0xFA };
                for &src in order {
                    let early = byte(&bits[src], f * 2);
                    let late = byte(&bits[src], f * 2 + 1);
                    match c {
                        DopContainer::S24_3Le => out.extend_from_slice(&[late, early, marker]),
                        DopContainer::S32LeMsb => out.extend_from_slice(&[0, late, early, marker]),
                        DopContainer::S24LeLsb => {
                            let ext = if marker & 0x80 != 0 { 0xFF } else { 0 };
                            out.extend_from_slice(&[late, early, marker, ext]);
                        }
                    }
                }
            }
        }
    }
    out
}

fn layout(list: &[Speaker]) -> ChannelLayout {
    ChannelLayout::from_positions(list).unwrap_or_else(|| panic!("布局"))
}

#[test]
fn source_and_device_formats() {
    let st = ChannelLayout::STEREO;
    assert_eq!(SourceFormat::dsd(0, st), Err(FormatError::SampleRate));
    assert_eq!(
        SourceFormat::dsd(2_822_401, st),
        Err(FormatError::SampleRate)
    );
    let Ok(s) = SourceFormat::dsd(2_822_400, st) else {
        panic!("DSD64");
    };
    assert_eq!(
        (s.sample_rate(), s.bits(), s.encoding(), s.words_per_frame()),
        (88_200, 1, SampleEncoding::Dsd, 2)
    );
    assert_eq!(s.dsd_bit_rate(), Some(2_822_400));
    assert!(!s.encoding().is_float());
    // PCM 与 DSD 之间没有不改变样本的换法。
    assert_eq!(
        s.with_encoding(SampleEncoding::Int, 24),
        Err(FormatError::Encoding)
    );
    assert!(s.relayout(ChannelLayout::STEREO).is_ok_and(|x| x == s));
    // 设备侧数值：DSD64 双声道。
    let expect = [
        (ALL[0], 4, 352_800, 2, 8),
        (ALL[1], 2, 176_400, 4, 8),
        (ALL[2], 2, 176_400, 4, 8),
        (ALL[3], 1, 88_200, 8, 8),
        (ALL[4], 1, 88_200, 8, 8),
        (ALL[5], 2, 176_400, 6, 12),
        (ALL[6], 2, 176_400, 8, 16),
        (ALL[7], 2, 176_400, 8, 16),
    ];
    for (t, e, rate, frame, align) in expect {
        let Ok(d) = DeviceFormat::dsd(88_200, st, t) else {
            panic!("{t:?}");
        };
        assert_eq!(
            (
                d.expansion(),
                d.device_rate(),
                d.device_frame_bytes(),
                d.block_align()
            ),
            (e, rate, frame, align),
            "{t:?}"
        );
        assert_eq!(d.bytes_per_sec(), 88_200 * u32::from(align), "{t:?}");
        assert!(d.accepts(&s), "{t:?}");
        assert_eq!(check(&s, &d), Ok(()));
        // PCM 源不能放入 DSD 设备格式，DSD 源不能放入 PCM 设备格式。
        let Ok(pcm) = SourceFormat::new(88_200, 2, 16) else {
            panic!("pcm");
        };
        assert!(!d.accepts(&pcm) && check(&pcm, &d).is_err(), "{t:?}");
    }
    let Ok(pcm_dev) = DeviceFormat::wasapi(88_200, 2, 32, 24) else {
        panic!("pcm dev");
    };
    assert!(!pcm_dev.accepts(&s) && check(&s, &pcm_dev).is_err());
    // 8 声道 DoP 32 位：每字 2 设备帧 × 8 声道 × 4 字节。
    let Ok(d8) = DeviceFormat::dsd(
        88_200,
        ChannelLayout::default_for(2).unwrap_or(st),
        DsdTransport::Dop(DopContainer::S32LeMsb),
    ) else {
        panic!("dop");
    };
    assert_eq!(d8.block_align(), 16);
    assert!(DeviceFormat::dsd(0, st, ALL[0]).is_err());
    assert!(DeviceFormat::dsd(u32::MAX / 16, st, ALL[0]).is_err());
}

#[test]
fn pack_matches_bitwise_reference() {
    let st = ChannelLayout::STEREO;
    let swapped = layout(&[Speaker::FrontRight, Speaker::FrontLeft]);
    let five = layout(&[
        Speaker::FrontLeft,
        Speaker::FrontRight,
        Speaker::FrontCenter,
        Speaker::BackLeft,
        Speaker::BackRight,
    ]);
    let five_dev = layout(&[
        Speaker::FrontCenter,
        Speaker::FrontLeft,
        Speaker::FrontRight,
        Speaker::BackRight,
        Speaker::BackLeft,
    ]);
    let cases: [(ChannelLayout, ChannelLayout, Vec<usize>); 4] = [
        (ChannelLayout::MONO, ChannelLayout::MONO, vec![0]),
        (st, st, vec![0, 1]),
        (st, swapped, vec![1, 0]),
        (five, five_dev, vec![2, 0, 1, 4, 3]),
    ];
    for t in ALL {
        for (src_layout, dev_layout, order) in &cases {
            let ch = usize::from(src_layout.channels());
            for (seed, frames) in [(1u64, 1usize), (7, 37), (99, 300)] {
                let mut w = words(frames * ch, seed);
                // 全 0、全 1 与首尾位。
                w[0] = 0;
                if w.len() > 1 {
                    w[1] = -1;
                }
                if w.len() > 2 {
                    w[2] = i32::MIN;
                    let k = 3 % w.len();
                    w[k] = 1;
                }
                let Ok(s) = SourceFormat::dsd(88_200 * 32, *src_layout) else {
                    panic!("src");
                };
                let Ok(d) = DeviceFormat::dsd(88_200, *dev_layout, t) else {
                    panic!("dev");
                };
                let Ok(pcm) = Pcm::exact(&w, s) else {
                    panic!("pcm");
                };
                let mut out = vec![0xEEu8; frames * usize::from(d.block_align())];
                assert_eq!(pack(&pcm, &d, &mut out), Ok(frames), "{t:?}");
                assert!(
                    out == reference(t, &w, ch, order),
                    "{t:?} {src_layout} -> {dev_layout} 种子 {seed}"
                );
                // 容量不足一个字时只写完整的字。
                let mut short = vec![0u8; usize::from(d.block_align()) * 2 - 1];
                let n = pack(&pcm, &d, &mut short);
                assert_eq!(n, Ok(frames.min(1)), "{t:?}");
            }
        }
    }
}

#[test]
fn silence_frames() {
    let st = ChannelLayout::STEREO;
    for t in ALL {
        let Ok(d) = DeviceFormat::dsd(88_200, st, t) else {
            panic!("dev");
        };
        let s = d.silence_frame();
        assert_eq!(s.as_bytes().len(), usize::from(d.block_align()));
        // 静音帧就是 DSD 静音字 0x69696969 的 pack 结果（DoP 标记从 0x05 开始）。
        let w = [0x6969_6969u32 as i32; 2];
        let Ok(src) = SourceFormat::dsd(88_200 * 32, st) else {
            panic!("src");
        };
        let Ok(pcm) = Pcm::exact(&w, src) else {
            panic!("pcm");
        };
        let mut out = vec![0u8; usize::from(d.block_align())];
        assert_eq!(pack(&pcm, &d, &mut out), Ok(1));
        assert_eq!(s.as_bytes(), out.as_slice(), "{t:?}");
        assert!(!s.is_zero());
        let mut long = vec![0u8; out.len() * 3];
        s.fill(&mut long);
        assert!(long.chunks(out.len()).all(|c| c == out.as_slice()));
    }
    let Ok(pcm) = DeviceFormat::wasapi(44_100, 2, 32, 24) else {
        panic!("pcm");
    };
    assert!(pcm.silence_frame().is_zero() && pcm.silence_frame().as_bytes().len() == 8);
}

/// ALSA（D1）：DSD 格式码（内核 `asound.h`）、名称往返；候选“原生优先、DoP 其次”且只取设备支持的；
/// 设备格式按设备帧率构造；读回核对按设备帧率。
#[test]
fn alsa_mapping() {
    use qy_pcm_format::alsa::{Access, AlsaFormat, HwApplied, HwRequest, candidates, confirm};
    let codes = [
        (AlsaFormat::DsdU8, 48),
        (AlsaFormat::DsdU16Le, 49),
        (AlsaFormat::DsdU32Le, 50),
        (AlsaFormat::DsdU16Be, 51),
        (AlsaFormat::DsdU32Be, 52),
    ];
    for (f, c) in codes {
        assert_eq!(f.code(), c);
        assert_eq!(AlsaFormat::from_code(c), Ok(f));
        assert_eq!(AlsaFormat::from_name(f.name()), Ok(f));
        assert_eq!(f.encoding(), SampleEncoding::Dsd);
        assert_eq!(f.wire(), f);
    }
    assert_eq!(AlsaFormat::DopS32Le.code(), AlsaFormat::S32Le.code());
    assert_eq!(AlsaFormat::DopS24_3Le.wire(), AlsaFormat::S24_3Le);
    assert_eq!(AlsaFormat::DopS24Le.wire(), AlsaFormat::S24Le);
    let Ok(s) = SourceFormat::dsd(2_822_400, ChannelLayout::STEREO) else {
        panic!("src");
    };
    // 设备支持 DSD_U32_BE、S32_LE、S16_LE：先原生，再 DoP（S16 承载不了 DoP）。
    let supported = [AlsaFormat::S16Le, AlsaFormat::S32Le, AlsaFormat::DsdU32Be];
    let c: Vec<AlsaFormat> = candidates(&s, &supported).as_slice().collect();
    assert_eq!(c, [AlsaFormat::DsdU32Be, AlsaFormat::DopS32Le]);
    // 只支持 PCM 16 位：没有候选（明确拒绝，不转 PCM）。
    assert!(candidates(&s, &[AlsaFormat::S16Le]).is_empty());
    // PCM 源不会选到 DSD 或 DoP。
    let Ok(p) = SourceFormat::new(44_100, 2, 16) else {
        panic!("pcm");
    };
    let all = [
        AlsaFormat::DsdU32Be,
        AlsaFormat::DopS32Le,
        AlsaFormat::S16Le,
        AlsaFormat::S32Le,
    ];
    let c: Vec<AlsaFormat> = candidates(&p, &all).as_slice().collect();
    assert_eq!(c, [AlsaFormat::S16Le, AlsaFormat::S32Le]);
    // 设备格式：按设备帧率构造，字率 = 帧率 ÷ e；帧率不是 e 的倍数时拒绝。
    let Ok(d) = AlsaFormat::DopS32Le.device_format_layout(176_400, ChannelLayout::STEREO) else {
        panic!("dop");
    };
    assert_eq!((d.sample_rate(), d.device_rate()), (88_200, 176_400));
    assert_eq!(
        AlsaFormat::DsdU8.device_format_layout(352_801, ChannelLayout::STEREO),
        Err(FormatError::SampleRate)
    );
    assert_eq!(AlsaFormat::from_device(&d), Ok(AlsaFormat::DopS32Le));
    // 读回核对：设备帧率与格式码一致时得到同一个设备格式。
    for f in [
        AlsaFormat::DsdU32Be,
        AlsaFormat::DsdU8,
        AlsaFormat::DopS24_3Le,
    ] {
        let Some(t) = f.dsd_transport() else {
            panic!("transport");
        };
        let rate = 88_200 * t.expansion();
        let req = HwRequest {
            format: f,
            sample_rate: rate,
            channels: 2,
            layout: ChannelLayout::STEREO,
            period_frames: 4_096,
            buffer_frames: 16_384,
        };
        let applied = HwApplied {
            access: Access::RwInterleaved,
            format_code: f.code(),
            rate_num: rate,
            rate_den: 1,
            channels: 2,
            period_frames: 4_096,
            buffer_frames: 16_384,
            resample: false,
            significant_bits: None,
        };
        let Ok(got) = confirm(&s, &req, &applied) else {
            panic!("{f:?}");
        };
        assert_eq!(got.dsd_transport(), Some(t));
        assert!(got.accepts(&s));
        let bad = HwApplied {
            rate_num: rate / 2,
            ..applied
        };
        assert!(confirm(&s, &req, &bad).is_err(), "{f:?}");
    }
}

/// WASAPI（D1）：DoP 以承载它的整数 PCM 描述——设备帧率（DSD64 为 176.4 kHz）、容器 32 位或 24 位、
/// 有效 24 位、`KSDATAFORMAT_SUBTYPE_PCM`；原生 DSD 与 `WAVEFORMATEX` 没有描述，明确拒绝。
#[test]
fn wave_descriptor_for_dop() {
    use qy_pcm_format::wave::{WaveFormatEx, WaveFormatExtensible};
    let st = ChannelLayout::STEREO;
    for (c, bits, align) in [
        (DopContainer::S32LeMsb, 32u16, 8u16),
        (DopContainer::S24_3Le, 24, 6),
    ] {
        let Ok(d) = DeviceFormat::dsd(88_200, st, DsdTransport::Dop(c)) else {
            panic!("dop");
        };
        let Ok(w) = WaveFormatExtensible::from_device(&d) else {
            panic!("{c:?}");
        };
        let Ok(pcm) = WaveFormatExtensible::pcm(176_400, 2, bits, 24) else {
            panic!("pcm");
        };
        assert_eq!(w, pcm, "{c:?}");
        assert_eq!(w.format.block_align, align);
        assert_eq!(d.dop_wire().map(|p| p.block_align()), Some(align));
        assert!(WaveFormatEx::from_device(&d).is_err());
    }
    let Ok(n) = DeviceFormat::dsd(88_200, st, DsdTransport::Native(DsdWord::U32Be)) else {
        panic!("native");
    };
    assert!(WaveFormatExtensible::from_device(&n).is_err());
    assert_eq!(n.dop_wire(), None);
}
