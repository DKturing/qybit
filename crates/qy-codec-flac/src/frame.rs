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
    /// 帧头的编码号（§9.1.5）：可变块为首个样本号，固定块为帧号。
    pub number: u64,
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
    let number = utf8_number(b, variable)?;
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
        number,
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

/// M2 T5：cargo-mutants 发现的帧级测试缺口。帧由测试内的位写入器按 RFC 9639 §9 构造，CRC 用本
/// crate 的 crc8/crc16（其与按位定义等价已由 Kani 证明）。
#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;
    use std::vec::Vec;

    #[derive(Default)]
    struct W {
        bytes: Vec<u8>,
        bits: u32,
    }

    impl W {
        fn put(&mut self, v: u64, n: u32) {
            for i in (0..n).rev() {
                if self.bits.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                if (v >> i) & 1 == 1 {
                    let last = self.bytes.len() - 1;
                    self.bytes[last] |= 0x80 >> (self.bits % 8);
                }
                self.bits += 1;
            }
        }
        fn signed(&mut self, v: i64, n: u32) {
            self.put((v as u64) & ((1u64 << n) - 1), n);
        }
        fn align(&mut self) {
            while !self.bits.is_multiple_of(8) {
                self.put(0, 1);
            }
        }
        /// 一元码：q 个 0 后跟 1。
        fn unary(&mut self, q: u64) {
            for _ in 0..q {
                self.put(0, 1);
            }
            self.put(1, 1);
        }
    }

    /// 附加字段（值, 位数）。
    type Extra = Option<(u64, u32)>;

    struct Header {
        variable: bool,
        bs: u32,
        bs_extra: Extra,
        sr: u32,
        sr_extra: Extra,
        ch: u32,
        bps: u32,
        number: Vec<u8>,
    }

    impl Default for Header {
        fn default() -> Self {
            // 16 位立体声、块大小 192（码 1）、采样率取 STREAMINFO（码 0）、帧号 0。
            Self {
                variable: false,
                bs: 1,
                bs_extra: None,
                sr: 0,
                sr_extra: None,
                ch: 1,
                bps: 4,
                number: vec![0],
            }
        }
    }

    /// 帧头 + CRC-8，然后由 `body` 写子帧，零填充对齐后加 CRC-16。
    fn frame(h: &Header, body: impl FnOnce(&mut W)) -> Vec<u8> {
        let mut w = W::default();
        w.put(0x7FFC, 15);
        w.put(u64::from(h.variable), 1);
        w.put(u64::from(h.bs), 4);
        w.put(u64::from(h.sr), 4);
        w.put(u64::from(h.ch), 4);
        w.put(u64::from(h.bps), 3);
        w.put(0, 1);
        for &b in &h.number {
            w.put(u64::from(b), 8);
        }
        if let Some((v, n)) = h.bs_extra {
            w.put(v, n);
        }
        if let Some((v, n)) = h.sr_extra {
            w.put(v, n);
        }
        let c8 = crc8(&w.bytes);
        w.put(u64::from(c8), 8);
        body(&mut w);
        w.align();
        let c16 = crc16(&w.bytes);
        w.put(u64::from(c16), 16);
        w.bytes
    }

    /// CONSTANT 子帧（§9.2.4）。
    fn constant(w: &mut W, v: i64, bits: u32) {
        w.put(0, 1);
        w.put(0, 6);
        w.put(0, 1);
        w.signed(v, bits);
    }

    fn params(rate: u32, channels: u32, bits: u32) -> StreamParams {
        StreamParams {
            sample_rate: rate,
            channels,
            bits,
        }
    }

    fn run(
        input: &[u8],
        p: &StreamParams,
        max_block: usize,
    ) -> (Result<Decoded, FrameError>, Vec<i32>) {
        let n = p.channels as usize * max_block;
        let mut ws = vec![0i64; n];
        let mut pcm = vec![0i32; n];
        let r = decode(input, p, max_block, &mut ws, &mut pcm).result;
        (r, pcm)
    }

    fn bad_with(e: FlacError) -> Result<Decoded, FrameError> {
        Err(FrameError::Bad(e))
    }

    /// 类 UTF-8 编号（§9.1.5）：1–7 字节的全部长度、各长度的最小与最大值；非法首字节、非法后续
    /// 字节；固定块不允许 7 字节。读取位置为 8 × 字节数。
    #[test]
    fn utf8_numbers() {
        fn enc(v: u64, len: usize) -> Vec<u8> {
            if len == 1 {
                return vec![v as u8];
            }
            let prefix: u8 = match len {
                2 => 0xC0,
                3 => 0xE0,
                4 => 0xF0,
                5 => 0xF8,
                6 => 0xFC,
                _ => 0xFE,
            };
            let mut b = vec![prefix | (v >> (6 * (len - 1))) as u8];
            for i in (0..len - 1).rev() {
                b.push(0x80 | ((v >> (6 * i)) & 0x3F) as u8);
            }
            b
        }
        // (长度, 最小值, 最大值)
        let ranges = [
            (1, 0u64, 0x7F),
            (2, 0x80, 0x7FF),
            (3, 0x800, 0xFFFF),
            (4, 0x1_0000, 0x1F_FFFF),
            (5, 0x20_0000, 0x3FF_FFFF),
            (6, 0x400_0000, 0x7FFF_FFFF),
            (7, 0x8000_0000, 0xF_FFFF_FFFF),
        ];
        for (len, lo, hi) in ranges {
            for v in [lo, hi, (lo + hi) / 2] {
                let b = enc(v, len);
                let mut r = Bits::new(&b);
                assert_eq!(utf8_number(&mut r, true), Ok(v), "{len} 字节 {v:#x}");
                assert_eq!(r.bit_pos(), 8 * len);
                let mut r = Bits::new(&b);
                let fixed = utf8_number(&mut r, false);
                if len == 7 {
                    assert_eq!(fixed, bad(FlacError::BadFrameNumber));
                } else {
                    assert_eq!(fixed, Ok(v));
                }
            }
        }
        for first in [0x80u8, 0xBF, 0xFF] {
            let b = [first, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80];
            assert_eq!(
                utf8_number(&mut Bits::new(&b), true),
                bad(FlacError::BadFrameNumber),
                "{first:#x}"
            );
        }
        for cont in [0x00u8, 0x40, 0xC0] {
            assert_eq!(
                utf8_number(&mut Bits::new(&[0xC2, cont]), true),
                bad(FlacError::BadFrameNumber),
                "{cont:#x}"
            );
        }
        assert_eq!(
            utf8_number(&mut Bits::new(&[0xE0, 0x80]), true),
            Err(FrameError::Incomplete)
        );
    }

    #[test]
    fn fits_boundaries() {
        for bits in [4u32, 8, 16, 24, 32, 33] {
            let lo = -(1i64 << (bits - 1));
            let hi = (1i64 << (bits - 1)) - 1;
            assert!(fits(lo, bits) && fits(hi, bits), "{bits}");
            assert!(!fits(lo - 1, bits) && !fits(hi + 1, bits), "{bits}");
        }
    }

    /// 块大小码、采样率码与位深码（§9.1.1–9.1.4）的全部取值；与 STREAMINFO 只差一项也报不一致。
    #[test]
    fn header_codes() {
        let blocks: [(u32, Extra, usize); 9] = [
            (1, None, 192),
            (2, None, 576),
            (4, None, 2304),
            (5, None, 4608),
            (6, Some((1, 8)), 2),
            (7, Some((299, 16)), 300),
            (8, None, 256),
            (12, None, 4096),
            (15, None, 32768),
        ];
        for (bs, extra, want) in blocks {
            let h = Header {
                bs,
                bs_extra: extra,
                ..Header::default()
            };
            let f = frame(&h, |w| {
                constant(w, -5, 16);
                constant(w, 7, 16);
            });
            let (r, pcm) = run(&f, &params(44_100, 2, 16), 32768);
            assert_eq!(r.map(|d| d.block_size), Ok(want), "块大小码 {bs}");
            assert_eq!((pcm[0], pcm[1], pcm[2 * want - 1]), (-5, 7, 7));
        }
        let rates: [(u32, Extra, u32); 14] = [
            (1, None, 88_200),
            (2, None, 176_400),
            (3, None, 192_000),
            (4, None, 8_000),
            (5, None, 16_000),
            (6, None, 22_050),
            (7, None, 24_000),
            (8, None, 32_000),
            (9, None, 44_100),
            (10, None, 48_000),
            (11, None, 96_000),
            (12, Some((50, 8)), 50_000),
            (13, Some((12_345, 16)), 12_345),
            (14, Some((4_410, 16)), 44_100),
        ];
        for (sr, extra, want) in rates {
            let h = Header {
                sr,
                sr_extra: extra,
                ..Header::default()
            };
            let f = frame(&h, |w| {
                constant(w, 1, 16);
                constant(w, 1, 16);
            });
            assert!(
                run(&f, &params(want, 2, 16), 192).0.is_ok(),
                "采样率码 {sr}"
            );
            assert_eq!(
                run(&f, &params(want + 1, 2, 16), 192).0,
                bad_with(FlacError::StreamMismatch),
                "采样率码 {sr}"
            );
        }
        let depths = [(1u32, 8u32), (2, 12), (4, 16), (5, 20), (6, 24), (7, 32)];
        for (bps, bits) in depths {
            let h = Header {
                bps,
                ch: 0,
                ..Header::default()
            };
            let f = frame(&h, |w| constant(w, -1, bits));
            let (r, pcm) = run(&f, &params(44_100, 1, bits), 192);
            assert!(r.is_ok(), "位深码 {bps}");
            assert_eq!(pcm[0], -1);
            // 只有声道数或位深不一致。
            assert_eq!(
                run(&f, &params(44_100, 2, bits), 192).0,
                bad_with(FlacError::StreamMismatch)
            );
            let other = if bits == 8 { 12 } else { 8 };
            assert_eq!(
                run(&f, &params(44_100, 1, other), 192).0,
                bad_with(FlacError::StreamMismatch)
            );
        }
        let h = Header {
            bps: 3,
            ..Header::default()
        };
        let f = frame(&h, |_| {});
        assert_eq!(
            run(&f, &params(44_100, 2, 16), 192).0,
            bad_with(FlacError::ReservedCode)
        );
    }

    /// 可变块策略位（§9.1）：结果中的 `variable` 与帧头一致，可变块允许 7 字节编号。
    #[test]
    fn blocking_strategy_bit() {
        for variable in [false, true] {
            let h = Header {
                variable,
                number: if variable {
                    vec![0xFE, 0x80, 0x80, 0x80, 0x80, 0x80, 0x81]
                } else {
                    vec![0x05]
                },
                ..Header::default()
            };
            let f = frame(&h, |w| {
                constant(w, 0, 16);
                constant(w, 0, 16);
            });
            assert_eq!(
                run(&f, &params(44_100, 2, 16), 192).0.map(|d| d.variable),
                Ok(variable)
            );
        }
    }

    /// 工作区或 PCM 缓冲比声道数 × 块大小小（块大小未超过 max_block）：按容量不足拒绝。
    #[test]
    fn buffers_smaller_than_block() {
        let f = frame(&Header::default(), |w| {
            constant(w, 0, 16);
            constant(w, 0, 16);
        });
        let p = params(44_100, 2, 16);
        let mut ws = vec![0i64; 2 * 192 - 1];
        let mut pcm = vec![0i32; 2 * 192];
        assert_eq!(
            decode(&f, &p, 192, &mut ws, &mut pcm).result,
            bad_with(FlacError::BlockTooLarge)
        );
        let mut ws = vec![0i64; 2 * 192];
        let mut pcm = vec![0i32; 2 * 192 - 1];
        assert_eq!(
            decode(&f, &p, 192, &mut ws, &mut pcm).result,
            bad_with(FlacError::BlockTooLarge)
        );
    }

    /// 侧声道解相关（§9.2.3）后只有一个声道超出位深：也按样本溢出拒绝。
    #[test]
    fn side_channel_overflow_in_one_channel() {
        // left/side：left = 127，side = −1 → right = 128，超出 8 位。
        let h = Header {
            ch: 8,
            bps: 1,
            ..Header::default()
        };
        let f = frame(&h, |w| {
            constant(w, 127, 8);
            constant(w, -1, 9);
        });
        assert_eq!(
            run(&f, &params(44_100, 2, 8), 192).0,
            bad_with(FlacError::SampleOverflow)
        );
        // side/right：side = 1、right = 127 → left = 128。
        let h = Header {
            ch: 9,
            bps: 1,
            ..Header::default()
        };
        let f = frame(&h, |w| {
            constant(w, 1, 9);
            constant(w, 127, 8);
        });
        assert_eq!(
            run(&f, &params(44_100, 2, 8), 192).0,
            bad_with(FlacError::SampleOverflow)
        );
        // 两声道都在范围内时正常。
        let f = frame(&h, |w| {
            constant(w, 1, 9);
            constant(w, 126, 8);
        });
        let (r, pcm) = run(&f, &params(44_100, 2, 8), 192);
        assert!(r.is_ok());
        assert_eq!((pcm[0], pcm[1]), (127, 126));
    }

    /// 单声道、块大小 `block` 的帧，子帧由 `sub` 写出。
    fn mono(block: u64, bits_code: u32, sub: impl FnOnce(&mut W)) -> Vec<u8> {
        let h = Header {
            bs: 6,
            bs_extra: Some((block - 1, 8)),
            ch: 0,
            bps: bits_code,
            ..Header::default()
        };
        frame(&h, sub)
    }

    /// 预测阶数等于块大小时合法（全部是预热样本），大于块大小时拒绝（§9.2.5、§9.2.6）；
    /// LPC 右移量为 0 合法。
    #[test]
    fn predictor_order_versus_block() {
        let p = params(44_100, 1, 16);
        // 固定预测 2 阶，块大小 2：两个预热样本，残差分区 0 个样本。
        let f = mono(2, 4, |w| {
            w.put(0, 1);
            w.put(8 + 2, 6);
            w.put(0, 1);
            w.signed(-3, 16);
            w.signed(4, 16);
            w.put(0, 2); // Rice
            w.put(0, 4); // 分区阶数 0
            w.put(0, 4); // 参数
        });
        let (r, pcm) = run(&f, &p, 16);
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(&pcm[..2], &[-3, 4]);
        // 固定预测 4 阶，块大小 2。
        let f = mono(2, 4, |w| {
            w.put(0, 1);
            w.put(8 + 4, 6);
            w.put(0, 1);
        });
        assert_eq!(run(&f, &p, 16).0, bad_with(FlacError::BadSubframe));
        // LPC 2 阶（类型 33），块大小 2，精度 4 位，右移 0。
        let f = mono(2, 4, |w| {
            w.put(0, 1);
            w.put(32 + 1, 6);
            w.put(0, 1);
            w.signed(10, 16);
            w.signed(-10, 16);
            w.put(3, 4);
            w.signed(0, 5);
            w.signed(1, 4);
            w.signed(-1, 4);
            w.put(0, 2);
            w.put(0, 4);
            w.put(0, 4);
        });
        let (r, pcm) = run(&f, &p, 16);
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(&pcm[..2], &[10, -10]);
        // LPC 3 阶，块大小 2。
        let f = mono(2, 4, |w| {
            w.put(0, 1);
            w.put(32 + 2, 6);
            w.put(0, 1);
        });
        assert_eq!(run(&f, &p, 16).0, bad_with(FlacError::BadSubframe));
    }

    /// 残差分区（§9.2.7）：首分区样本数等于预测阶数合法（该分区为空），小于时拒绝。
    #[test]
    fn partition_size_versus_order() {
        let p = params(44_100, 1, 16);
        // 块大小 4、固定 2 阶、分区阶数 1：每区 2 个样本，首区 0 个残差，第二区 2 个（均为 0）。
        let f = mono(4, 4, |w| {
            w.put(0, 1);
            w.put(8 + 2, 6);
            w.put(0, 1);
            w.signed(1, 16);
            w.signed(2, 16);
            w.put(0, 2);
            w.put(1, 4);
            w.put(0, 4); // 首区参数
            w.put(0, 4); // 第二区参数 0：每个残差为一元码 0 → "1"
            w.unary(0);
            w.unary(0);
        });
        let (r, pcm) = run(&f, &p, 16);
        assert!(r.is_ok(), "{r:?}");
        // 2 阶固定预测：s[i] = 2 s[i−1] − s[i−2] + 残差。
        assert_eq!(&pcm[..4], &[1, 2, 3, 4]);
        // 固定 3 阶、分区阶数 1：每区 2 个样本 < 3。
        let f = mono(4, 4, |w| {
            w.put(0, 1);
            w.put(8 + 3, 6);
            w.put(0, 1);
            for v in [1, 2, 3] {
                w.signed(v, 16);
            }
            w.put(0, 2);
            w.put(1, 4);
        });
        assert_eq!(run(&f, &p, 16).0, bad_with(FlacError::BadPartitionOrder));
    }

    /// 残差 zigzag 值的上界（§9.2.7.1）：u = 2^32 − 1（即 −2^31）合法，2^32 溢出。
    #[test]
    fn residual_value_limit() {
        let p = params(44_100, 1, 32);
        let one = |q: u64, low: u64| {
            mono(1, 7, move |w| {
                w.put(0, 1);
                w.put(8, 6); // 固定 0 阶
                w.put(0, 1);
                w.put(1, 2); // Rice2：5 位参数
                w.put(0, 4);
                w.put(30, 5);
                w.unary(q);
                w.put(low, 30);
            })
        };
        let (r, pcm) = run(&one(3, (1 << 30) - 1), &p, 16);
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(pcm[0], i32::MIN);
        assert_eq!(
            run(&one(4, 0), &p, 16).0,
            bad_with(FlacError::ResidualOverflow)
        );
    }
}
