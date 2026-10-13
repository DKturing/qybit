//! pack：唯一的输出表示转换边界（AGENTS §3，M0 T3）。
//!
//! 输入为解码器输出的交错、右对齐 `i32`（有效位 N），输出为设备格式的完整帧字节。
//! N ≤ V 时按 `s × 2^(V−N)` 放入 V 位，保持精度与相对幅度（满幅仍为满幅），再按
//! 放置方式放入 C 位容器：
//! - [`Placement::MsbAligned`]（WASAPI 32/24 等）：V 位居容器高位，低 C−V 位为 0；
//! - [`Placement::LsbAligned`]（ALSA `S24_LE` 等）：V 位居容器低位，高位符号扩展。
//!
//! N > V 拒绝；样本超出 N 位有符号范围视为溢出并拒绝，不截断、不饱和。
//! pack 在生产端运行；渲染线程只搬运其结果，不做任何位移或字节序转换。

use core::marker::PhantomData;

use crate::{DeviceFormat, Endian, MAX_LAYOUT_CHANNELS, Placement, SourceFormat};

mod sealed {
    pub trait Sealed {}
}

/// 样本处理标记。只能是 [`Exact`] 或 [`Altered`]。
pub trait Fidelity: sealed::Sealed {}

/// 样本值与源逐位一致（解码器的直接输出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exact {}
/// 样本值已被改变（增益、重采样、抖动等）。M0 没有任何产生它的操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Altered {}

impl sealed::Sealed for Exact {}
impl sealed::Sealed for Altered {}
impl Fidelity for Exact {}
impl Fidelity for Altered {}

/// 交错 PCM 视图，带格式与处理标记。
///
/// 类型标记只约束软件操作，不证明设备端 bit-perfect（AGENTS §3）。
#[derive(Debug, Clone, Copy)]
pub struct Pcm<'a, M: Fidelity> {
    samples: &'a [i32],
    format: SourceFormat,
    _marker: PhantomData<M>,
}

/// pack 错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackError {
    /// 样本数不是声道数的整数倍。
    PartialFrame,
    /// 采样率或扬声器位置集合与设备格式不同（禁止隐式重采样、声道增减或混音）。
    FormatMismatch,
    /// N > V：设备有效位不足以无损容纳源精度。
    Precision { source_bits: u8, valid_bits: u8 },
    /// 样本超出 N 位有符号范围。
    SampleOutOfRange { index: usize },
}

impl<'a> Pcm<'a, Exact> {
    /// 由解码器输出构造精确 PCM。仅供直接承载解码结果的宿主/核心使用，
    /// 任何改变样本值的操作都不得返回 `Pcm<Exact>`。
    pub fn exact(samples: &'a [i32], format: SourceFormat) -> Result<Self, PackError> {
        if samples.len() % usize::from(format.channels()) != 0 {
            return Err(PackError::PartialFrame);
        }
        Ok(Self {
            samples,
            format,
            _marker: PhantomData,
        })
    }
}

impl<M: Fidelity> Pcm<'_, M> {
    pub fn samples(&self) -> &[i32] {
        self.samples
    }

    pub fn format(&self) -> SourceFormat {
        self.format
    }

    pub fn frames(&self) -> usize {
        self.samples.len() / usize::from(self.format.channels())
    }
}

/// 检查源与设备格式是否可无损 pack。
pub fn check(src: &SourceFormat, dev: &DeviceFormat) -> Result<(), PackError> {
    if src.sample_rate() != dev.sample_rate() || !src.layout().same_positions(&dev.layout()) {
        return Err(PackError::FormatMismatch);
    }
    if src.bits() > dev.valid_bits() {
        return Err(PackError::Precision {
            source_bits: src.bits(),
            valid_bits: dev.valid_bits(),
        });
    }
    Ok(())
}

/// 设备第 j 个声道取源的哪个声道：按扬声器位置对应（F1）。位置集合相同由 [`check`] 保证。
fn channel_map(
    src: &SourceFormat,
    dev: &DeviceFormat,
) -> Result<[u8; MAX_LAYOUT_CHANNELS], PackError> {
    let mut map = [0u8; MAX_LAYOUT_CHANNELS];
    let s = src.layout();
    for (slot, &pos) in map.iter_mut().zip(dev.layout().positions()) {
        let i = s.index_of(pos).ok_or(PackError::FormatMismatch)?;
        *slot = i as u8;
    }
    Ok(map)
}

/// 把 `src` 尽可能多的完整帧 pack 到 `out`，返回帧数（受 `out` 容量限制）。
///
/// 设备声道顺序与源不同（位置集合相同）时按位置纯置换：每个样本值原样放入对应扬声器的
/// 设备声道，不增减、不混合声道（tasks/F1.md §6）。
///
/// 出错时 `out` 内容未定义，调用方不得入队。
pub fn pack(src: &Pcm<'_, Exact>, dev: &DeviceFormat, out: &mut [u8]) -> Result<usize, PackError> {
    check(&src.format, dev)?;
    let map = channel_map(&src.format, dev)?;
    let n = u32::from(src.format.bits());
    let v = u32::from(dev.valid_bits());
    let c = u32::from(dev.container_bits());
    let bytes = (c / 8) as usize;
    let align = usize::from(dev.block_align());
    let channels = usize::from(dev.channels());
    let frames = src.frames().min(out.len() / align);
    let shift = match dev.placement() {
        Placement::MsbAligned => c - n,
        Placement::LsbAligned => v - n,
    };
    let lo = -(1i64 << (n - 1));
    let hi = (1i64 << (n - 1)) - 1;
    for (f, (frame, dst_frame)) in src
        .samples
        .chunks_exact(channels)
        .zip(out[..frames * align].chunks_exact_mut(align))
        .enumerate()
    {
        for (&from, dst) in map[..channels]
            .iter()
            .zip(dst_frame.chunks_exact_mut(bytes))
        {
            let from = usize::from(from);
            let s = i64::from(frame[from]);
            if s < lo || s > hi {
                return Err(PackError::SampleOutOfRange {
                    index: f * channels + from,
                });
            }
            // shift ≤ 31 且 s 在 N 位内，结果落在 C 位有符号范围内，不会溢出。
            let word = ((s << shift) as u32).to_le_bytes();
            match dev.endian() {
                Endian::Little => dst.copy_from_slice(&word[..bytes]),
                Endian::Big => {
                    for (k, b) in dst.iter_mut().enumerate() {
                        *b = word[bytes - 1 - k];
                    }
                }
            }
        }
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(rate: u32, ch: u16, bits: u8) -> SourceFormat {
        match SourceFormat::new(rate, ch, bits) {
            Ok(f) => f,
            Err(e) => panic!("{e:?}"),
        }
    }

    fn dev(ch: u16, c: u8, v: u8, e: Endian, p: Placement) -> DeviceFormat {
        match DeviceFormat::new(48_000, ch, c, v, e, p) {
            Ok(f) => f,
            Err(e) => panic!("{e:?}"),
        }
    }

    fn pack_vec(
        samples: &[i32],
        ch: u16,
        n: u8,
        d: &DeviceFormat,
    ) -> Result<([u8; 64], usize), PackError> {
        let pcm = Pcm::exact(samples, src(48_000, ch, n))?;
        let mut out = [0u8; 64];
        let frames = pack(&pcm, d, &mut out)?;
        Ok((out, frames * usize::from(d.block_align())))
    }

    /// (样本, 声道, N, 设备格式, 期望字节)
    type Case = (&'static [i32], u16, u8, DeviceFormat, &'static [u8]);

    /// 独立手写的期望字节。
    #[test]
    fn independent_expected_bytes() {
        use Endian::*;
        use Placement::*;
        let cases: &[Case] = &[
            // 16/16 立体声 LE
            (
                &[0x1234, -2],
                2,
                16,
                dev(2, 16, 16, Little, MsbAligned),
                &[0x34, 0x12, 0xFE, 0xFF],
            ),
            // N=16 → WASAPI 32/24：左移 16
            (
                &[0x1234, -2],
                2,
                16,
                dev(2, 32, 24, Little, MsbAligned),
                &[0, 0, 0x34, 0x12, 0, 0, 0xFE, 0xFF],
            ),
            // N=24 → 32/24
            (
                &[0x123456, -8_388_608],
                2,
                24,
                dev(2, 32, 24, Little, MsbAligned),
                &[0, 0x56, 0x34, 0x12, 0, 0, 0, 0x80],
            ),
            // N=24 → 24/24
            (
                &[0x123456, -1],
                2,
                24,
                dev(2, 24, 24, Little, MsbAligned),
                &[0x56, 0x34, 0x12, 0xFF, 0xFF, 0xFF],
            ),
            // N=16 → 24/24 单声道
            (
                &[0x1234, -32_768],
                1,
                16,
                dev(1, 24, 24, Little, MsbAligned),
                &[0, 0x34, 0x12, 0, 0, 0x80],
            ),
            // N=32 → 32/32
            (
                &[i32::MIN, i32::MAX],
                2,
                32,
                dev(2, 32, 32, Little, MsbAligned),
                &[0, 0, 0, 0x80, 0xFF, 0xFF, 0xFF, 0x7F],
            ),
            // N=20 → 32/24 单声道：左移 12
            (
                &[0x7FFFF],
                1,
                20,
                dev(1, 32, 24, Little, MsbAligned),
                &[0, 0xF0, 0xFF, 0x7F],
            ),
            // ALSA S24_LE（32 容器低 24 位，符号扩展）：N=16 左移 8；N=24 原值
            (
                &[0x1234],
                1,
                16,
                dev(1, 32, 24, Little, LsbAligned),
                &[0, 0x34, 0x12, 0],
            ),
            (
                &[-1],
                1,
                24,
                dev(1, 32, 24, Little, LsbAligned),
                &[0xFF, 0xFF, 0xFF, 0xFF],
            ),
            (
                &[-8_388_608],
                1,
                24,
                dev(1, 32, 24, Little, LsbAligned),
                &[0, 0, 0x80, 0xFF],
            ),
            (
                &[8_388_607],
                1,
                24,
                dev(1, 32, 24, Little, LsbAligned),
                &[0xFF, 0xFF, 0x7F, 0],
            ),
            // 大端
            (
                &[0x1234],
                1,
                16,
                dev(1, 16, 16, Big, MsbAligned),
                &[0x12, 0x34],
            ),
            (
                &[0x123456],
                1,
                24,
                dev(1, 32, 24, Big, MsbAligned),
                &[0x12, 0x34, 0x56, 0],
            ),
        ];
        for (i, (s, ch, n, d, want)) in cases.iter().enumerate() {
            let r = pack_vec(s, *ch, *n, d);
            let Ok((out, len)) = r else {
                panic!("case {i}: {r:?}")
            };
            assert_eq!(&out[..len], *want, "case {i}");
        }
    }

    #[test]
    fn rejects_precision_mismatch_and_overflow() {
        let d = dev(2, 32, 24, Endian::Little, Placement::MsbAligned);
        assert_eq!(
            pack_vec(&[0, 0], 2, 32, &d).map(|r| r.1),
            Err(PackError::Precision {
                source_bits: 32,
                valid_bits: 24
            })
        );
        let d16 = dev(2, 16, 16, Endian::Little, Placement::MsbAligned);
        assert_eq!(
            pack_vec(&[0, 0], 2, 24, &d16).map(|r| r.1),
            Err(PackError::Precision {
                source_bits: 24,
                valid_bits: 16
            })
        );
        assert_eq!(
            pack_vec(&[0], 1, 16, &d16).map(|r| r.1),
            Err(PackError::FormatMismatch)
        );
        assert_eq!(
            pack_vec(&[0, 0, 0], 2, 16, &d16).map(|r| r.1),
            Err(PackError::PartialFrame)
        );
        assert_eq!(
            pack_vec(&[0, 40_000], 2, 16, &d16).map(|r| r.1),
            Err(PackError::SampleOutOfRange { index: 1 })
        );
        assert_eq!(
            pack_vec(&[-32_769, 0], 2, 16, &d16).map(|r| r.1),
            Err(PackError::SampleOutOfRange { index: 0 })
        );
        let Ok(rate) = DeviceFormat::wasapi(44_100, 2, 16, 16) else {
            panic!()
        };
        assert_eq!(
            pack_vec(&[0, 0], 2, 16, &rate).map(|r| r.1),
            Err(PackError::FormatMismatch)
        );
    }

    #[test]
    fn packs_only_whole_frames_that_fit() {
        let d = dev(2, 24, 24, Endian::Little, Placement::MsbAligned);
        let s = [1, 2, 3, 4, 5, 6];
        let Ok(pcm) = Pcm::exact(&s, src(48_000, 2, 24)) else {
            panic!()
        };
        let mut out = [0u8; 13];
        assert_eq!(pack(&pcm, &d, &mut out), Ok(2));
        assert_eq!(&out[..12], &[1, 0, 0, 2, 0, 0, 3, 0, 0, 4, 0, 0]);
    }

    /// 测试专用逆变换（独立于 pack 的实现方式：逐字节组装再算术右移）。
    fn unpack(bytes: &[u8], n: u32, d: &DeviceFormat) -> i32 {
        let c = u32::from(d.container_bits());
        let v = u32::from(d.valid_bits());
        let mut word: u32 = 0;
        for k in 0..bytes.len() {
            let b = match d.endian() {
                Endian::Little => bytes[k],
                Endian::Big => bytes[bytes.len() - 1 - k],
            };
            word |= u32::from(b) << (8 * k);
        }
        // 先移到 32 位顶端再算术右移，恢复符号。
        let top = (word << (32 - c)) as i32;
        match d.placement() {
            Placement::MsbAligned => {
                assert_eq!(
                    word & ((1u64 << (c - v)) as u32).wrapping_sub(1),
                    0,
                    "低填充位必须为 0"
                );
                top >> (32 - n)
            }
            Placement::LsbAligned => {
                let val = top >> (32 - c);
                assert_eq!(val & ((1i64 << (v - n)) - 1) as i32, 0);
                val >> (v - n)
            }
        }
    }

    #[test]
    fn roundtrip_preserves_value_and_relative_amplitude() {
        let mut seed = 0x1234_5678_9ABC_DEF0u64;
        for &(c, v) in &[
            (16u8, 16u8),
            (24, 24),
            (32, 24),
            (32, 32),
            (32, 20),
            (24, 20),
        ] {
            for placement in [Placement::MsbAligned, Placement::LsbAligned] {
                for endian in [Endian::Little, Endian::Big] {
                    for ch in [1u16, 2] {
                        let d = dev(ch, c, v, endian, placement);
                        for n in 1..=v {
                            let lo = -(1i64 << (n - 1));
                            let hi = (1i64 << (n - 1)) - 1;
                            let mut s = [0i32; 16];
                            s[0] = lo as i32;
                            s[1] = hi as i32;
                            for x in s.iter_mut().skip(2) {
                                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                                *x = (lo + ((seed >> 20) as i64).rem_euclid(hi - lo + 1)) as i32;
                            }
                            let r = pack_vec(&s, ch, n, &d);
                            let Ok((out, len)) = r else { panic!("{r:?}") };
                            let b = usize::from(c / 8);
                            assert_eq!(len, 16 * b);
                            for (i, chunk) in out[..len].chunks_exact(b).enumerate() {
                                assert_eq!(
                                    unpack(chunk, u32::from(n), &d),
                                    s[i],
                                    "c={c} v={v} n={n} i={i}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// F1：5.1 源（WAVE 顺序 FL FR FC LFE BL BR）送到 HDA 顺序（FL FR BL BR FC LFE）的
    /// ALSA S24_LE 设备：每个样本值原样放入同一扬声器的设备声道（纯置换），期望字节独立写出。
    #[test]
    fn permutes_channels_by_speaker_position() {
        use crate::{ChannelLayout, Speaker::*};
        let (Some(wave51), Some(hda)) = (
            ChannelLayout::from_mask(0x3F),
            ChannelLayout::from_positions(&[
                FrontLeft,
                FrontRight,
                BackLeft,
                BackRight,
                FrontCenter,
                LowFrequency,
            ]),
        ) else {
            panic!("layouts");
        };
        let (Ok(src), Ok(dev)) = (
            SourceFormat::with_layout(48_000, wave51, 16),
            DeviceFormat::with_layout(48_000, hda, 32, 24, Endian::Little, Placement::LsbAligned),
        ) else {
            panic!("formats");
        };
        // 两帧：声道值即“帧号 × 16 + 声道编号”，负值覆盖符号扩展。
        let samples = [1, 2, 3, 4, 5, 6, -17, -18, -19, -20, -21, -22];
        let Ok(pcm) = Pcm::exact(&samples, src) else {
            panic!("pcm");
        };
        let mut out = [0u8; 48];
        assert_eq!(pack(&pcm, &dev, &mut out), Ok(2));
        // S24_LE（LsbAligned，C32/V24，N16）：s << 8，按设备顺序 FL FR BL BR FC LFE。
        let order = [0usize, 1, 4, 5, 2, 3];
        let mut expect = [0u8; 48];
        for f in 0..2 {
            for (j, &from) in order.iter().enumerate() {
                let v = (samples[f * 6 + from] << 8) as u32;
                expect[(f * 6 + j) * 4..(f * 6 + j) * 4 + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        assert_eq!(out, expect);
        // 位置集合不同（5.1 侧环绕）：拒绝。
        let Some(side) = ChannelLayout::from_mask(0x60F) else {
            panic!("side");
        };
        let Ok(dev_side) =
            DeviceFormat::with_layout(48_000, side, 32, 24, Endian::Little, Placement::LsbAligned)
        else {
            panic!("dev");
        };
        assert_eq!(
            pack(&pcm, &dev_side, &mut out),
            Err(PackError::FormatMismatch)
        );
        // 超范围样本报告的是源样本下标（第 2 帧的 FC = 下标 8）。
        let bad = [1, 2, 3, 4, 5, 6, 7, 8, 40_000, 10, 11, 12];
        let Ok(pcm) = Pcm::exact(&bad, src) else {
            panic!("pcm");
        };
        assert_eq!(
            pack(&pcm, &dev, &mut out),
            Err(PackError::SampleOutOfRange { index: 8 })
        );
    }
}
