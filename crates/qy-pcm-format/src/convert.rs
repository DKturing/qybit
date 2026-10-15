//! 浮点转整数 PCM（F3 §4）：严格路径之外、用户明确开启的选项，是 [`Pcm<Altered>`] 唯一的产生者。
//!
//! 映射（与 ffmpeg / libsndfile 的常用约定一致）：`s = round_half_even(x × 2^(V−1))`，再限制到
//! `[−2^(V−1), 2^(V−1) − 1]`。即 −1.0 → 最小值，+1.0 及以上 → 最大值（限幅）。乘以 2^(V−1) 是浮点
//! 满幅 ±1.0 与整数满幅之间的表示换算，不是增益调整；不加抖动，同一输入总得到同一输出。
//!
//! 实现只用整数运算：按 IEEE 754 的符号、指数、尾数直接完成缩放与舍入（binary32：8 位指数、
//! 23 位尾数；binary64：11 位指数、52 位尾数；指数全 1 为无穷或 NaN，全 0 为零或次正规数），不依赖
//! 硬件浮点，桌面与没有双精度 FPU 的 MCU 结果逐位相同。NaN 与 ±∞ 不做猜测，按
//! [`ConvertError::NonFinite`] 拒绝（F3 §8 决定 2）。

use crate::pack::{Altered, Exact, Pcm};
use crate::{SampleEncoding, SourceFormat};

/// 转换统计（报告用）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConvertStats {
    /// 已转换的样本数。
    pub samples: u64,
    /// 超出 V 位范围而被限幅的样本数。
    pub clipped: u64,
    /// 不精确的样本数：转换结果按同一公式换回（`s / 2^(V−1)`，实数运算）不等于原值，即发生了
    /// 舍入或限幅。为 0 表示每个样本都能被 V 位整数精确表示（例如由整数 PCM 生成的浮点文件）。
    pub inexact: u64,
}

impl ConvertStats {
    pub fn add(&mut self, other: &ConvertStats) {
        self.samples += other.samples;
        self.clipped += other.clipped;
        self.inexact += other.inexact;
    }
}

/// 转换错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertError {
    /// 源不是浮点，或目标位数不在 1..=32。
    Format,
    /// 第 `index` 个样本（本次调用的源样本下标，按样本计，不是按字）是 NaN 或无穷。
    NonFinite { index: usize },
}

/// 一个样本的转换结果：(值, 是否限幅, 是否不精确)。
pub type Converted = (i32, bool, bool);

/// 有限值分解为 (符号, 尾数 M, 指数 E)，值 = (−1)^符号 × M × 2^E；非有限值为 `None`。
fn decompose(bits: u64, exp_bits: u32, frac_bits: u32) -> Option<(bool, u64, i32)> {
    let exp_mask = (1u64 << exp_bits) - 1;
    let bias = (1i32 << (exp_bits - 1)) - 1;
    let sign = (bits >> (exp_bits + frac_bits)) & 1 == 1;
    let e = (bits >> frac_bits) & exp_mask;
    let m = bits & ((1u64 << frac_bits) - 1);
    if e == exp_mask {
        return None;
    }
    let shift = bias + frac_bits as i32;
    if e == 0 {
        // 零与次正规数：没有隐含的 1，指数固定为 1 − bias。
        Some((sign, m, 1 - shift))
    } else {
        Some((sign, m | 1u64 << frac_bits, e as i32 - shift))
    }
}

/// 计算 round_half_even(±M × 2^(E + V − 1)) 并限幅到 V 位。
fn scale(sign: bool, m: u64, e: i32, v: u32) -> Converted {
    if m == 0 {
        return (0, false, false);
    }
    let max = (1i128 << (v - 1)) - 1;
    let min = -(1i128 << (v - 1));
    let k = e + (v as i32 - 1);
    let (mag, exact) = if k >= 0 {
        // M < 2^53：左移超过 64 位时数值远超 32 位范围，直接按超范围处理。
        if k > 64 {
            (u128::MAX >> 1, true)
        } else {
            (u128::from(m) << k, true)
        }
    } else {
        let r = k.unsigned_abs();
        if r > 64 {
            // 值小于 2^−11，舍入为 0。
            (0, false)
        } else {
            let m = u128::from(m);
            let q = m >> r;
            let rem = m & ((1u128 << r) - 1);
            let half = 1u128 << (r - 1);
            let up = rem > half || (rem == half && q & 1 == 1);
            (q + u128::from(up), rem == 0)
        }
    };
    let mag = mag.min(u128::MAX >> 1) as i128;
    let value = if sign { -mag } else { mag };
    let (value, clipped) = if value > max {
        (max, true)
    } else if value < min {
        (min, true)
    } else {
        (value, false)
    };
    (value as i32, clipped, clipped || !exact)
}

/// binary32 位模式转为 V 位整数（V 在 1..=32）；NaN 与无穷为 `None`。
pub fn from_f32_bits(bits: u32, v: u32) -> Option<Converted> {
    let (sign, m, e) = decompose(u64::from(bits), 8, 23)?;
    Some(scale(sign, m, e, v))
}

/// binary64 位模式转为 V 位整数（V 在 1..=32）；NaN 与无穷为 `None`。
pub fn from_f64_bits(bits: u64, v: u32) -> Option<Converted> {
    let (sign, m, e) = decompose(bits, 11, 52)?;
    Some(scale(sign, m, e, v))
}

/// 把浮点 `src` 尽可能多的完整帧（受 `out` 容量限制）转为 `bits` 位整数，写入 `out`，返回
/// 改变了样本的 PCM（同采样率、同布局、整数 N = `bits`），并累计 `stats`。
///
/// 遇到 NaN 或无穷时返回 [`ConvertError::NonFinite`]，`out` 内容未定义。
pub fn float_to_int<'a>(
    src: &Pcm<'_, Exact>,
    bits: u8,
    out: &'a mut [i32],
    stats: &mut ConvertStats,
) -> Result<Pcm<'a, Altered>, ConvertError> {
    let format = src.format();
    let encoding = format.encoding();
    if !encoding.is_float() || !(1..=32).contains(&bits) {
        return Err(ConvertError::Format);
    }
    let int = SourceFormat::with_layout(format.sample_rate(), format.layout(), bits)
        .map_err(|_| ConvertError::Format)?;
    let channels = usize::from(format.channels());
    let frames = src.frames().min(out.len() / channels.max(1));
    let n = frames * channels;
    let v = u32::from(bits);
    let words = encoding.words();
    let mut chunk = ConvertStats::default();
    for (i, (dst, w)) in out[..n]
        .iter_mut()
        .zip(src.samples().chunks_exact(words))
        .enumerate()
    {
        let r = match encoding {
            SampleEncoding::Float64 => {
                from_f64_bits(u64::from(w[1] as u32) << 32 | u64::from(w[0] as u32), v)
            }
            _ => from_f32_bits(w[0] as u32, v),
        };
        let Some((s, clipped, inexact)) = r else {
            return Err(ConvertError::NonFinite { index: i });
        };
        *dst = s;
        chunk.clipped += u64::from(clipped);
        chunk.inexact += u64::from(inexact);
    }
    chunk.samples = n as u64;
    stats.add(&chunk);
    Pcm::altered(&out[..n], int).map_err(|_| ConvertError::Format)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChannelLayout;
    use crate::pack::Fidelity;

    /// 独立参考（F3 §6）：用 `f64` 与 `i128` 按定义计算，不调用被测实现。x × 2^(V−1) 在 `f64` 中
    /// 精确（乘 2 的幂只改指数；溢出为无穷，按限幅处理）。
    fn reference(x: f64, v: u32) -> Option<Converted> {
        if !x.is_finite() {
            return None;
        }
        let y = x * 2f64.powi(v as i32 - 1);
        let r = y.round_ties_even();
        let max = ((1i64 << (v - 1)) - 1) as f64;
        let min = -((1i64 << (v - 1)) as f64);
        let (c, clipped) = if r > max {
            (max, true)
        } else if r < min {
            (min, true)
        } else {
            (r, false)
        };
        Some((c as i64 as i32, clipped, clipped || r != y))
    }

    const VS: [u32; 7] = [1, 2, 8, 16, 20, 24, 32];

    fn lcg(seed: &mut u64) -> u64 {
        *seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *seed
    }

    #[test]
    fn special_values() {
        for v in VS {
            let max = ((1i64 << (v - 1)) - 1) as i32;
            let min = -(1i64 << (v - 1)) as i32;
            assert_eq!(from_f32_bits(0, v), Some((0, false, false)));
            assert_eq!(from_f32_bits(0x8000_0000, v), Some((0, false, false)), "−0");
            assert_eq!(
                from_f32_bits((-1.0f32).to_bits(), v),
                Some((min, false, false))
            );
            assert_eq!(from_f32_bits(1.0f32.to_bits(), v), Some((max, true, true)));
            assert_eq!(
                from_f32_bits(f32::MAX.to_bits(), v),
                Some((max, true, true))
            );
            assert_eq!(
                from_f32_bits(f32::MIN.to_bits(), v),
                Some((min, true, true))
            );
            // 最小次正规数：远小于半个 LSB，舍入为 0，不精确。
            assert_eq!(from_f32_bits(1, v), Some((0, false, true)));
            assert_eq!(from_f64_bits(1, v), Some((0, false, true)));
            for nf in [
                f32::INFINITY.to_bits(),
                f32::NEG_INFINITY.to_bits(),
                0x7FC0_0000,
                0x7F80_0001,
                0xFFC0_0001,
            ] {
                assert_eq!(from_f32_bits(nf, v), None, "{nf:08x}");
            }
            for nf in [
                f64::INFINITY.to_bits(),
                f64::NEG_INFINITY.to_bits(),
                0x7FF8_0000_0000_0000,
                0x7FF0_0000_0000_0001,
            ] {
                assert_eq!(from_f64_bits(nf, v), None, "{nf:016x}");
            }
        }
        // 16 位：0.5 → 16384，−0.5 → −16384；1 − 2^−15 → 32767（整数来源，精确）。
        assert_eq!(
            from_f32_bits(0.5f32.to_bits(), 16),
            Some((16_384, false, false))
        );
        assert_eq!(
            from_f32_bits((-0.5f32).to_bits(), 16),
            Some((-16_384, false, false))
        );
        let near = 1.0f32 - 2f32.powi(-15);
        assert_eq!(
            from_f32_bits(near.to_bits(), 16),
            Some((32_767, false, false))
        );
    }

    /// 每个舍入边界：正好半个 LSB（偶数与奇数商）、半个 LSB ± 1 ulp，正负两侧（F3 §6）。
    #[test]
    fn rounding_boundaries_match_reference() {
        let mut seed = 0x5EED_F3A0_u64;
        for v in VS {
            let lsb = 2f64.powi(1 - v as i32);
            for _ in 0..2_000 {
                let k = (lcg(&mut seed) >> 33) as i64 % (1i64 << (v - 1));
                for sign in [1.0, -1.0] {
                    let x = sign * (k as f64 + 0.5) * lsb;
                    let b64 = x.to_bits();
                    for b in [b64.wrapping_sub(1), b64, b64.wrapping_add(1)] {
                        assert_eq!(
                            from_f64_bits(b, v),
                            reference(f64::from_bits(b), v),
                            "f64 v={v} {b:016x}"
                        );
                    }
                    let x32 = x as f32;
                    if f64::from(x32) == x {
                        let b32 = x32.to_bits();
                        for b in [b32.wrapping_sub(1), b32, b32.wrapping_add(1)] {
                            assert_eq!(
                                from_f32_bits(b, v),
                                reference(f64::from(f32::from_bits(b)), v),
                                "f32 v={v} {b:08x}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// 任意位模式（含次正规数、无穷、NaN、极大极小值）与独立参考一致。
    #[test]
    fn random_bit_patterns_match_reference() {
        let mut seed = 0xF3_u64;
        for _ in 0..200_000 {
            let r = lcg(&mut seed);
            let v = VS[(r % VS.len() as u64) as usize];
            let b32 = (r >> 32) as u32;
            assert_eq!(
                from_f32_bits(b32, v),
                reference(f64::from(f32::from_bits(b32)), v),
                "f32 v={v} {b32:08x}"
            );
            let b64 = lcg(&mut seed);
            assert_eq!(
                from_f64_bits(b64, v),
                reference(f64::from_bits(b64), v),
                "f64 v={v} {b64:016x}"
            );
            // 满幅附近：指数接近 0（|x| ≈ 1）的值最常见。
            let near = (0x3F00_0000 | (r as u32 & 0x00FF_FFFF)) ^ (r as u32 & 0x8000_0000);
            assert_eq!(
                from_f32_bits(near, v),
                reference(f64::from(f32::from_bits(near)), v)
            );
        }
    }

    /// 由 N 位整数生成的浮点（s / 2^(N−1)）转回 V ≥ N 位时精确（不精确计数为 0）。
    #[test]
    fn integer_sourced_floats_are_exact() {
        for n in [8u32, 16, 24] {
            for v in [n, 24, 32] {
                if v < n {
                    continue;
                }
                for s in [
                    -(1i64 << (n - 1)),
                    -1,
                    0,
                    1,
                    12_345 % (1 << (n - 1)),
                    (1 << (n - 1)) - 1,
                ] {
                    let x = s as f64 / 2f64.powi(n as i32 - 1);
                    let want = (s << (v - n)) as i32;
                    assert_eq!(
                        from_f32_bits((x as f32).to_bits(), v),
                        Some((want, false, false))
                    );
                    assert_eq!(from_f64_bits(x.to_bits(), v), Some((want, false, false)));
                }
            }
        }
    }

    fn marker<M: Fidelity>(_: &Pcm<'_, M>) {}

    #[test]
    fn float_to_int_converts_frames_and_counts() {
        let Ok(fmt) = SourceFormat::float(48_000, ChannelLayout::STEREO, SampleEncoding::Float32)
        else {
            panic!()
        };
        let x = [0.5f32, -1.0, 1.0, 0.25, 0.1, -0.1];
        let words: [i32; 6] = x.map(|v| v.to_bits() as i32);
        let Ok(pcm) = Pcm::exact(&words, fmt) else {
            panic!()
        };
        let mut out = [0i32; 4];
        let mut stats = ConvertStats::default();
        let Ok(alt) = float_to_int(&pcm, 16, &mut out, &mut stats) else {
            panic!()
        };
        marker(&alt);
        // 输出只够两帧：转换两帧。
        assert_eq!(alt.frames(), 2);
        assert_eq!(alt.samples(), &[16_384, -32_768, 32_767, 8_192]);
        assert_eq!(alt.format().bits(), 16);
        assert_eq!(alt.format().encoding(), SampleEncoding::Int);
        assert_eq!(
            stats,
            ConvertStats {
                samples: 4,
                clipped: 1,
                inexact: 1
            }
        );
        // binary64 每样本两个字（低、高）。
        let Ok(f64fmt) = SourceFormat::float(48_000, ChannelLayout::MONO, SampleEncoding::Float64)
        else {
            panic!()
        };
        let b = (-0.75f64).to_bits();
        let w = [b as u32 as i32, (b >> 32) as u32 as i32, 0, 0];
        let Ok(pcm) = Pcm::exact(&w, f64fmt) else {
            panic!()
        };
        let mut out = [0i32; 8];
        let mut stats = ConvertStats::default();
        let Ok(alt) = float_to_int(&pcm, 24, &mut out, &mut stats) else {
            panic!()
        };
        assert_eq!(alt.samples(), &[-6_291_456, 0]);
        // NaN：报告位置（样本下标）。
        let w = [0, 0x7FC0_0000u32 as i32];
        let Ok(pcm) = Pcm::exact(&w, fmt) else {
            panic!()
        };
        let mut out = [0i32; 2];
        assert_eq!(
            float_to_int(&pcm, 16, &mut out, &mut stats).map(|p| p.frames()),
            Err(ConvertError::NonFinite { index: 1 })
        );
        // 整数源、位数越界：拒绝。
        let Ok(int) = SourceFormat::new(48_000, 2, 16) else {
            panic!()
        };
        let Ok(pcm) = Pcm::exact(&[0, 0], int) else {
            panic!()
        };
        assert_eq!(
            float_to_int(&pcm, 16, &mut out, &mut stats).map(|p| p.frames()),
            Err(ConvertError::Format)
        );
        let Ok(pcm) = Pcm::exact(&words, fmt) else {
            panic!()
        };
        assert_eq!(
            float_to_int(&pcm, 33, &mut out, &mut stats).map(|p| p.frames()),
            Err(ConvertError::Format)
        );
    }
}
