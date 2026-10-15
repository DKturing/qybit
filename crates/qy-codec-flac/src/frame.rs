//! 整帧解析与重建（RFC 9639 §9）。
//!
//! 每次调用都从帧头重新解析整帧，不保存位级暂停状态。子帧先解码进 `i64` 工作区；
//! 读完帧尾 CRC-16 并校验通过后，才做声道解相关并写入交错 `i32` PCM。
//! 任何一步失败，调用方都不会发布本次写入的数据。
//!
//! 关键规则与出处（RFC 9639；逐项与 Xiph libFLAC 1.4 `stream_decoder.c` 对照）：
//! - 帧头 §9.1：同步码 0b111111111111100，阻塞策略位；块大小码 0b0000 保留；
//!   采样率码 0b1111 禁用；声道码 0b1011–0b1111 保留；位深码 0b011 保留；保留位须为 0；
//!   编码号为类 UTF-8 变长数（固定块 ≤ 6 字节 / 31 位，可变块 ≤ 7 字节 / 36 位）；
//!   非常用块大小 8/16 位（值 +1），非常用采样率 8 位 kHz / 16 位 Hz / 16 位 10 Hz；
//!   帧头 CRC-8 §9.1.8。
//! - 子帧 §9.2：1 位零填充、6 位类型（常量、verbatim、固定 0–4 阶、LPC 1–32 阶），
//!   wasted bits 标志与一元码 k−1（§9.2.2），侧声道多 1 位（§9.2.3，32 位源的侧声道为 33 位）。
//! - LPC §9.2.6：精度字段 0b1111 无效，右移量为负无效。
//! - 残差 §9.2.7：编码方法 00 Rice（4 位参数）、01 Rice2（5 位参数），1x 保留；
//!   分区阶数须整除块大小，且首分区样本数不少于预测阶数；参数全 1 为 escape：
//!   5 位原始位宽，0 表示全零；残差须能以 32 位有符号数表示。
//! - 帧尾 §9.3：零填充到字节边界（须为 0），CRC-16。
//! - 解相关 §9.2.3 / §4.2：left/side、side/right、mid/side。

use crate::FlacError;
use crate::bits::{Bits, Incomplete};
use crate::crc::{crc8, crc16};

/// 流参数（来自 STREAMINFO）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamParams {
    pub sample_rate: u32,
    pub channels: u32,
    pub bits: u32,
}

/// 一次整帧尝试的失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// 输入在帧结束前耗尽。
    Incomplete,
    Bad(FlacError),
}

impl From<Incomplete> for FrameError {
    fn from(_: Incomplete) -> Self {
        FrameError::Incomplete
    }
}

/// 成功解码的帧。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decoded {
    /// 帧字节长度（含 CRC-16）。
    pub len: usize,
    pub block_size: usize,
    /// 可变块大小策略。
    pub variable: bool,
}

/// 一次尝试的结果与统计。
pub struct Attempt {
    pub result: Result<Decoded, FrameError>,
    /// 本次尝试读取的位数。
    pub bits: u64,
    /// 本次尝试的样本重建运算次数。
    pub sample_ops: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Assign {
    Independent,
    LeftSide,
    SideRight,
    MidSide,
}

fn bad<T>(e: FlacError) -> Result<T, FrameError> {
    Err(FrameError::Bad(e))
}

/// `bits` 位有符号数的范围检查。
fn fits(v: i64, bits: u32) -> bool {
    let lo = -(1i64 << (bits - 1));
    let hi = (1i64 << (bits - 1)) - 1;
    v >= lo && v <= hi
}

/// 解码一帧。`max_block` 为宿主容量；`ws` 至少 `channels × max_block`，
/// `pcm` 至少同样长度。
pub fn decode(
    input: &[u8],
    params: &StreamParams,
    max_block: usize,
    ws: &mut [i64],
    pcm: &mut [i32],
) -> Attempt {
    let mut b = Bits::new(input);
    let mut ops = 0u64;
    let result = decode_inner(&mut b, input, params, max_block, ws, pcm, &mut ops);
    Attempt {
        result,
        bits: b.bit_pos() as u64,
        sample_ops: ops,
    }
}

fn utf8_number(b: &mut Bits<'_>, variable: bool) -> Result<u64, FrameError> {
    let first = b.read(8)?;
    let (len, mut v) = match first {
        x if x & 0x80 == 0 => (1, x),
        x if x & 0xE0 == 0xC0 => (2, x & 0x1F),
        x if x & 0xF0 == 0xE0 => (3, x & 0x0F),
        x if x & 0xF8 == 0xF0 => (4, x & 0x07),
        x if x & 0xFC == 0xF8 => (5, x & 0x03),
        x if x & 0xFE == 0xFC => (6, x & 0x01),
        0xFE => (7, 0),
        _ => return bad(FlacError::BadFrameNumber),
    };
    if !variable && len > 6 {
        return bad(FlacError::BadFrameNumber);
    }
    let mut n = u64::from(v);
    for _ in 1..len {
        let c = b.read(8)?;
        if c & 0xC0 != 0x80 {
            return bad(FlacError::BadFrameNumber);
        }
        v = c & 0x3F;
        n = (n << 6) | u64::from(v);
    }
    Ok(n)
}

#[allow(clippy::too_many_arguments)]
fn decode_inner(
    b: &mut Bits<'_>,
    input: &[u8],
    params: &StreamParams,
    max_block: usize,
    ws: &mut [i64],
    pcm: &mut [i32],
    ops: &mut u64,
) -> Result<Decoded, FrameError> {
    // ---- 帧头 §9.1 ----
    if b.read(15)? != 0x7FFC {
        return bad(FlacError::LostSync);
    }
    let variable = b.read(1)? == 1;
    let bs_code = b.read(4)?;
    let sr_code = b.read(4)?;
    let ch_code = b.read(4)?;
    let bps_code = b.read(3)?;
    if b.read(1)? != 0 {
        return bad(FlacError::ReservedCode);
    }
    let _number = utf8_number(b, variable)?;
    let block_size = match bs_code {
        0 => return bad(FlacError::ReservedCode),
        1 => 192,
        2..=5 => 576usize << (bs_code - 2),
        6 => b.read(8)? as usize + 1,
        7 => b.read(16)? as usize + 1,
        _ => 256usize << (bs_code - 8),
    };
    let sample_rate = match sr_code {
        0 => params.sample_rate,
        1 => 88_200,
        2 => 176_400,
        3 => 192_000,
        4 => 8_000,
        5 => 16_000,
        6 => 22_050,
        7 => 24_000,
        8 => 32_000,
        9 => 44_100,
        10 => 48_000,
        11 => 96_000,
        12 => b.read(8)? * 1000,
        13 => b.read(16)?,
        14 => b.read(16)? * 10,
        _ => return bad(FlacError::ReservedCode),
    };
    let (channels, assign) = match ch_code {
        0..=7 => (ch_code + 1, Assign::Independent),
        8 => (2, Assign::LeftSide),
        9 => (2, Assign::SideRight),
        10 => (2, Assign::MidSide),
        _ => return bad(FlacError::ReservedCode),
    };
    let bits = match bps_code {
        0 => params.bits,
        1 => 8,
        2 => 12,
        4 => 16,
        5 => 20,
        6 => 24,
        7 => 32,
        _ => return bad(FlacError::ReservedCode),
    };
    // 帧头到此字节对齐；CRC-8 覆盖帧头全部字节。
    let header_len = b.byte_pos();
    let crc = b.read(8)?;
    if crc8(&input[..header_len]) as u32 != crc {
        return bad(FlacError::HeaderCrc);
    }
    if sample_rate != params.sample_rate || channels != params.channels || bits != params.bits {
        return bad(FlacError::StreamMismatch);
    }
    if block_size > max_block {
        return bad(FlacError::BlockTooLarge);
    }
    let ch = channels as usize;
    if ws.len() < ch * block_size || pcm.len() < ch * block_size {
        return bad(FlacError::BlockTooLarge);
    }

    // ---- 子帧 §9.2 ----
    for c in 0..ch {
        let side = matches!(
            (assign, c),
            (Assign::LeftSide, 1) | (Assign::SideRight, 0) | (Assign::MidSide, 1)
        );
        let sub_bits = bits + u32::from(side);
        let out = &mut ws[c * block_size..(c + 1) * block_size];
        subframe(b, sub_bits, out, ops)?;
    }

    // ---- 帧尾 §9.3 ----
    if b.align()? != 0 {
        return bad(FlacError::NonZeroPadding);
    }
    let body_len = b.byte_pos();
    let crc = b.read(16)?;
    if u32::from(crc16(&input[..body_len])) != crc {
        return bad(FlacError::FrameCrc);
    }

    // ---- CRC 通过后才解相关并写出交错 PCM ----
    let (a, rest) = ws.split_at(block_size);
    let second = rest.get(..block_size);
    for i in 0..block_size {
        let (l, r) = match (assign, second) {
            (Assign::Independent, _) => {
                for c in 0..ch {
                    pcm[i * ch + c] = ws[c * block_size + i] as i32;
                }
                continue;
            }
            (Assign::LeftSide, Some(s)) => (a[i], a[i] - s[i]),
            (Assign::SideRight, Some(r)) => (a[i] + r[i], r[i]),
            (Assign::MidSide, Some(s)) => {
                let side = s[i];
                let mid = (a[i] << 1) | (side & 1);
                ((mid + side) >> 1, (mid - side) >> 1)
            }
            _ => return bad(FlacError::StreamMismatch),
        };
        if !fits(l, bits) || !fits(r, bits) {
            return bad(FlacError::SampleOverflow);
        }
        pcm[i * 2] = l as i32;
        pcm[i * 2 + 1] = r as i32;
    }
    *ops += block_size as u64;
    Ok(Decoded {
        len: body_len + 2,
        block_size,
        variable,
    })
}

/// 解码一个子帧到 `out`（长度为块大小）。
fn subframe(
    b: &mut Bits<'_>,
    sub_bits: u32,
    out: &mut [i64],
    ops: &mut u64,
) -> Result<(), FrameError> {
    let block = out.len();
    if b.read(1)? != 0 {
        return bad(FlacError::BadSubframe);
    }
    let ty = b.read(6)?;
    let wasted = if b.read(1)? == 1 {
        b.unary()?.saturating_add(1)
    } else {
        0
    };
    if wasted >= u64::from(sub_bits) {
        return bad(FlacError::BadWastedBits);
    }
    let k = wasted as u32;
    let eff = sub_bits - k;
    match ty {
        0 => {
            let v = b.read_signed(eff)?;
            out.fill(v);
        }
        1 => {
            for s in out.iter_mut() {
                *s = b.read_signed(eff)?;
            }
        }
        8..=12 => {
            let order = (ty - 8) as usize;
            if order > block {
                return bad(FlacError::BadSubframe);
            }
            for s in out[..order].iter_mut() {
                *s = b.read_signed(eff)?;
            }
            residual(b, order, out)?;
            fixed_predict(out, order, eff)?;
        }
        32..=63 => {
            let order = ((ty & 31) + 1) as usize;
            if order > block {
                return bad(FlacError::BadSubframe);
            }
            for s in out[..order].iter_mut() {
                *s = b.read_signed(eff)?;
            }
            let precision = b.read(4)?;
            if precision == 0b1111 {
                return bad(FlacError::BadLpcPrecision);
            }
            let precision = precision + 1;
            let shift = b.read_signed(5)?;
            if shift < 0 {
                return bad(FlacError::NegativeShift);
            }
            let mut coefs = [0i64; 32];
            for c in coefs[..order].iter_mut() {
                *c = b.read_signed(precision)?;
            }
            residual(b, order, out)?;
            lpc_predict(out, &coefs[..order], shift as u32, eff)?;
        }
        _ => return bad(FlacError::BadSubframe),
    }
    if k > 0 {
        for s in out.iter_mut() {
            *s <<= k;
        }
    }
    *ops += block as u64;
    Ok(())
}

/// 残差解码（§9.2.7），写入 `out[order..]`。
fn residual(b: &mut Bits<'_>, order: usize, out: &mut [i64]) -> Result<(), FrameError> {
    let block = out.len();
    let param_bits = match b.read(2)? {
        0 => 4,
        1 => 5,
        _ => return bad(FlacError::BadResidualCoding),
    };
    let porder = b.read(4)?;
    let parts = 1usize << porder;
    let per = block >> porder;
    if !block.is_multiple_of(parts) || per < order {
        return bad(FlacError::BadPartitionOrder);
    }
    let escape = (1u32 << param_bits) - 1;
    let mut idx = order;
    for p in 0..parts {
        let n = if p == 0 { per - order } else { per };
        let param = b.read(param_bits)?;
        let dst = &mut out[idx..idx + n];
        if param == escape {
            let raw = b.read(5)?;
            for s in dst.iter_mut() {
                *s = if raw == 0 { 0 } else { b.read_signed(raw)? };
            }
        } else {
            for s in dst.iter_mut() {
                let q = b.unary()?;
                if q > u64::from(u32::MAX) {
                    return bad(FlacError::ResidualOverflow);
                }
                let low = b.read(param)?;
                let u = (q << param) | u64::from(low);
                if u > u64::from(u32::MAX) {
                    return bad(FlacError::ResidualOverflow);
                }
                // zigzag：0, -1, 1, -2, …；u ≤ 2^32−1 时结果在 i32 范围内。
                *s = ((u >> 1) as i64) ^ -((u & 1) as i64);
            }
        }
        idx += n;
    }
    Ok(())
}

fn fixed_predict(s: &mut [i64], order: usize, bits: u32) -> Result<(), FrameError> {
    if !s[..order].iter().all(|&v| fits(v, bits)) {
        return bad(FlacError::SampleOverflow);
    }
    for i in order..s.len() {
        let p = match order {
            0 => 0,
            1 => s[i - 1],
            2 => 2 * s[i - 1] - s[i - 2],
            3 => 3 * s[i - 1] - 3 * s[i - 2] + s[i - 3],
            _ => 4 * s[i - 1] - 6 * s[i - 2] + 4 * s[i - 3] - s[i - 4],
        };
        let v = s[i] + p;
        if !fits(v, bits) {
            return bad(FlacError::SampleOverflow);
        }
        s[i] = v;
    }
    Ok(())
}

fn lpc_predict(s: &mut [i64], coefs: &[i64], shift: u32, bits: u32) -> Result<(), FrameError> {
    let order = coefs.len();
    if !s[..order].iter().all(|&v| fits(v, bits)) {
        return bad(FlacError::SampleOverflow);
    }
    for i in order..s.len() {
        // |系数| < 2^15、|样本| ≤ 2^32、阶数 ≤ 32：和 < 2^52，i64 不溢出。
        let mut sum = 0i64;
        for (j, &c) in coefs.iter().enumerate() {
            sum += c * s[i - 1 - j];
        }
        let v = s[i] + (sum >> shift);
        if !fits(v, bits) {
            return bad(FlacError::SampleOverflow);
        }
        s[i] = v;
    }
    Ok(())
}
