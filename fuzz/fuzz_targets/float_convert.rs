//! 浮点转整数 fuzz 目标（F3 §7 第 4 项）。
//!
//! 输入首字节选择目标位数 V（1..=32）与编码（binary32 / binary64），其余为样本位模式（小端）。
//! 检查：不 panic；每个样本的结果与测试内按定义独立计算的参考（`f64` 与 `i64`，不调用被测实现）
//! 一致，结果在 V 位范围内，限幅与不精确标记一致；NaN / 无穷为 `None`；整块转换
//! （`float_to_int`）与逐样本结果一致，统计等于逐样本计数，遇到非有限值时报告第一个的下标。
#![no_main]

use libfuzzer_sys::fuzz_target;
use qy_pcm_format::convert::{
    ConvertError, ConvertStats, Converted, float_to_int, from_f32_bits, from_f64_bits,
};
use qy_pcm_format::pack::Pcm;
use qy_pcm_format::{ChannelLayout, SampleEncoding, SourceFormat};

/// 独立参考：x × 2^(V−1) 在 `f64` 中精确（乘 2 的幂只改指数；溢出为无穷，按限幅处理），
/// 就近取偶后限幅到 V 位。
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

fuzz_target!(|input: &[u8]| {
    let Some((&sel, data)) = input.split_first() else {
        return;
    };
    let v = u32::from(sel & 0x1f) + 1;
    let wide = sel & 0x20 != 0;
    let (encoding, words, bytes) = if wide {
        (SampleEncoding::Float64, 2, 8)
    } else {
        (SampleEncoding::Float32, 1, 4)
    };
    let mut src = Vec::new();
    let mut want = Vec::new();
    for c in data.chunks_exact(bytes) {
        let (got, x) = if wide {
            let mut b = [0u8; 8];
            b.copy_from_slice(c);
            let bits = u64::from_le_bytes(b);
            src.push(bits as u32 as i32);
            src.push((bits >> 32) as u32 as i32);
            (from_f64_bits(bits, v), f64::from_bits(bits))
        } else {
            let mut b = [0u8; 4];
            b.copy_from_slice(c);
            let bits = u32::from_le_bytes(b);
            src.push(bits as i32);
            (from_f32_bits(bits, v), f64::from(f32::from_bits(bits)))
        };
        let r = reference(x, v);
        assert_eq!(got, r, "{x:e} V={v}");
        if let Some((s, _, _)) = got {
            let s = i64::from(s);
            assert!(-(1i64 << (v - 1)) <= s && s < 1i64 << (v - 1), "{s} V={v}");
        }
        want.push(r);
    }
    if want.is_empty() {
        return;
    }
    let Ok(format) = SourceFormat::float(48_000, ChannelLayout::MONO, encoding) else {
        panic!("浮点格式");
    };
    let Ok(pcm) = Pcm::exact(&src, format) else {
        panic!("{} 字 {:?}", src.len(), format);
    };
    let mut out = vec![0i32; want.len()];
    let mut stats = ConvertStats::default();
    match float_to_int(&pcm, v as u8, &mut out, &mut stats) {
        Ok(p) => {
            assert_eq!(p.samples().len(), want.len());
            let mut expect = ConvertStats {
                samples: want.len() as u64,
                ..ConvertStats::default()
            };
            for (got, w) in p.samples().iter().zip(&want) {
                let Some((s, clipped, inexact)) = *w else {
                    panic!("非有限值未被拒绝");
                };
                assert_eq!(*got, s);
                expect.clipped += u64::from(clipped);
                expect.inexact += u64::from(inexact);
            }
            assert_eq!(stats, expect);
            assert_eq!(usize::from(p.format().bits()), v as usize);
            assert_eq!(p.format().encoding(), SampleEncoding::Int);
        }
        Err(ConvertError::NonFinite { index }) => {
            assert_eq!(Some(index), want.iter().position(Option::is_none));
            assert_eq!(src.len() % words, 0);
        }
        Err(e) => panic!("{e:?}"),
    }
});
