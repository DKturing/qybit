//! 吞吐基准的测试文件（R1 §6）：按固定种子生成确定的样本，写成 WAV 或 FLAC。
//!
//! 与解码器相互独立：样本由整数随机游走生成（不用浮点三角函数，跨平台逐位相同），FLAC 由本文件按
//! RFC 9639 编码（§9 帧、§9.2 子帧、§9.2.7 残差）。基准程序解码后与这里的样本逐个比对，
//! 因此编码器与解码器任何一方出错都会被发现。
//!
//! FLAC 编码的取舍（只为基准，不追求压缩率）：
//! - 各声道独立编码（不做立体声去相关），帧头写固定块大小、采样率与位深代码；
//! - 预测器为固定阶 2，或 LPC 32 阶（精度 15 位、移位 12；系数前两项近似二阶差分，其余为
//!   小的非零值，使解码器真正执行 32 阶运算）；
//! - 残差用分区 Rice（每分区不少于 256 个样本、至多 2⁸ 个分区），每分区按平均值选参数，
//!   参数 ≥ 15 时用 Rice2（5 位参数）。

/// 可复现的伪随机数（SplitMix64）。
struct Prng(u64);

impl Prng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// 交错样本：每声道一条带惯性的随机游走（速度随机扰动、在 ±半满幅处反弹），再加几个最低位的
/// 噪声。频谱以低频为主、有宽带噪声底，近似真实录音，预测残差的大小也接近真实音乐。
pub fn samples(seed: u64, frames: usize, channels: u16, bits: u32) -> Vec<i32> {
    assert!((4..=32).contains(&bits), "位深 {bits}");
    let ch = usize::from(channels);
    let mut rng = Prng(seed ^ 0x5142_4954);
    let half = (1i64 << (bits - 1)) / 2;
    // 速度上限与噪声幅度按位深缩放（16 位时约 ±600 与 ±4 LSB）。
    let vmax = (half >> 5).max(1);
    let noise = (1i64 << bits.saturating_sub(14)).max(1);
    let mut pos = vec![0i64; ch];
    let mut vel = vec![0i64; ch];
    let step = (vmax >> 3).max(1);
    let mut out = Vec::with_capacity(frames * ch);
    for _ in 0..frames {
        for c in 0..ch {
            let r = rng.next();
            let dv = ((r & 0xFFFF_FFFF) % (2 * step as u64 + 1)) as i64 - step;
            vel[c] = (vel[c] + dv).clamp(-vmax, vmax);
            pos[c] += vel[c];
            if pos[c] > half || pos[c] < -half {
                vel[c] = -vel[c];
                pos[c] = pos[c].clamp(-half, half);
            }
            let n = ((r >> 32) % (2 * noise as u64 + 1)) as i64 - noise;
            out.push((pos[c] + n) as i32);
        }
    }
    out
}

/// WAV（`WAVE_FORMAT_EXTENSIBLE`，整数 PCM，容器位宽 = 有效位数向上取整到字节，默认声道掩码）。
pub fn wav(samples: &[i32], rate: u32, channels: u16, bits: u32) -> Vec<u8> {
    let bytes = bits.div_ceil(8) as usize;
    let block = bytes * usize::from(channels);
    let data_len = samples.len() * bytes;
    let mut out = Vec::with_capacity(data_len + 68);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((60 + data_len) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&0xFFFEu16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * block as u32).to_le_bytes());
    out.extend_from_slice(&(block as u16).to_le_bytes());
    out.extend_from_slice(&((bytes * 8) as u16).to_le_bytes());
    out.extend_from_slice(&22u16.to_le_bytes());
    out.extend_from_slice(&(bits as u16).to_le_bytes());
    let mask: u32 = match channels {
        1 => 0x4,
        2 => 0x3,
        6 => 0x3F,
        8 => 0x63F,
        n => (1u32 << n.min(18)) - 1,
    };
    out.extend_from_slice(&mask.to_le_bytes());
    // KSDATAFORMAT_SUBTYPE_PCM
    out.extend_from_slice(&[
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B,
        0x71,
    ]);
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    // 有效位数不是字节整数倍时左对齐（低位补零），与 WAV EXTENSIBLE 的约定一致。
    let shift = bytes as u32 * 8 - bits;
    for &s in samples {
        let v = (i64::from(s) << shift) as u64;
        out.extend_from_slice(&v.to_le_bytes()[..bytes]);
    }
    out
}

/// FLAC 预测器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Predictor {
    /// 固定预测，阶数 0–4。
    Fixed(usize),
    /// LPC，阶数 1–32。
    Lpc(usize),
}

/// 按位写入（大端位序）。
#[derive(Default)]
struct BitW {
    buf: Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitW {
    fn put(&mut self, v: u64, bits: u32) {
        debug_assert!(bits <= 57);
        if bits == 0 {
            return;
        }
        self.acc = (self.acc << bits) | (v & ((1u64 << bits) - 1));
        self.n += bits;
        while self.n >= 8 {
            self.n -= 8;
            self.buf.push((self.acc >> self.n) as u8);
        }
    }

    fn put_long(&mut self, v: u64, bits: u32) {
        if bits > 32 {
            self.put(v >> 32, bits - 32);
            self.put(v & 0xFFFF_FFFF, 32);
        } else {
            self.put(v, bits);
        }
    }

    fn put_signed(&mut self, v: i64, bits: u32) {
        assert!(
            bits == 64 || (v >= -(1i64 << (bits - 1)) && v < (1i64 << (bits - 1))),
            "{v} 超出 {bits} 位"
        );
        self.put_long(v as u64, bits);
    }

    fn unary(&mut self, q: u64) {
        let mut q = q;
        while q >= 32 {
            self.put(0, 32);
            q -= 32;
        }
        self.put(1, q as u32 + 1);
    }

    fn align(&mut self) {
        let pad = (8 - self.n % 8) % 8;
        self.put(0, pad);
    }
}

fn crc8(d: &[u8]) -> u8 {
    let mut c = 0u8;
    for &b in d {
        c ^= b;
        for _ in 0..8 {
            c = if c & 0x80 != 0 {
                (c << 1) ^ 0x07
            } else {
                c << 1
            };
        }
    }
    c
}

fn crc16(d: &[u8]) -> u16 {
    let mut c = 0u16;
    for &b in d {
        c ^= u16::from(b) << 8;
        for _ in 0..8 {
            c = if c & 0x8000 != 0 {
                (c << 1) ^ 0x8005
            } else {
                c << 1
            };
        }
    }
    c
}

/// UTF-8 式编码的帧号（RFC 9639 §9.1.5）。
fn coded_number(w: &mut BitW, v: u64) {
    if v < 0x80 {
        w.put(v, 8);
        return;
    }
    let n = (2..=7u32).find(|&k| v < (1u64 << (5 * k + 1))).unwrap_or(7);
    let lead = (0xFFu64 << (8 - n)) & 0xFF;
    w.put(lead | (v >> (6 * (n - 1))), 8);
    for i in (0..n - 1).rev() {
        w.put(0x80 | ((v >> (6 * i)) & 0x3F), 8);
    }
}

/// 采样率代码与帧头末尾的附加字段（RFC 9639 §9.1.2）。
fn rate_code(rate: u32) -> (u64, Option<(u64, u32)>) {
    match rate {
        88_200 => (1, None),
        176_400 => (2, None),
        192_000 => (3, None),
        8_000 => (4, None),
        16_000 => (5, None),
        22_050 => (6, None),
        24_000 => (7, None),
        32_000 => (8, None),
        44_100 => (9, None),
        48_000 => (10, None),
        96_000 => (11, None),
        r if r % 1000 == 0 && r / 1000 < 256 => (12, Some((u64::from(r / 1000), 8))),
        r if r < 65_536 => (13, Some((u64::from(r), 16))),
        r if r % 10 == 0 && r / 10 < 65_536 => (14, Some((u64::from(r / 10), 16))),
        // 帧头无法表示：写 0，取 STREAMINFO 的采样率。
        _ => (0, None),
    }
}

fn bits_code(bits: u32) -> u64 {
    match bits {
        8 => 1,
        12 => 2,
        16 => 4,
        20 => 5,
        24 => 6,
        32 => 7,
        _ => 0,
    }
}

/// LPC 32 阶等的系数：前两项近似二阶差分（2, −1），其余为交替的小值。精度 15 位、移位 12。
fn lpc_coefs(order: usize) -> Vec<i64> {
    (0..order)
        .map(|i| match i {
            0 => 2 << 12,
            1 => -(1 << 12),
            i if i % 2 == 0 => 1 + (i as i64 % 3),
            i => -(1 + (i as i64 % 3)),
        })
        .collect()
}

const LPC_PRECISION: u32 = 15;
const LPC_SHIFT: u32 = 12;

fn residual(s: &[i64], p: Predictor) -> (usize, Vec<i64>) {
    match p {
        Predictor::Fixed(o) => {
            let r = (o..s.len())
                .map(|i| {
                    let pred = match o {
                        0 => 0,
                        1 => s[i - 1],
                        2 => 2 * s[i - 1] - s[i - 2],
                        3 => 3 * s[i - 1] - 3 * s[i - 2] + s[i - 3],
                        _ => 4 * s[i - 1] - 6 * s[i - 2] + 4 * s[i - 3] - s[i - 4],
                    };
                    s[i] - pred
                })
                .collect();
            (o, r)
        }
        Predictor::Lpc(o) => {
            let c = lpc_coefs(o);
            let r = (o..s.len())
                .map(|i| {
                    let sum: i64 = c.iter().enumerate().map(|(j, &k)| k * s[i - 1 - j]).sum();
                    s[i] - (sum >> LPC_SHIFT)
                })
                .collect();
            (o, r)
        }
    }
}

/// 分区 Rice 残差（RFC 9639 §9.2.7）。
fn write_residual(w: &mut BitW, r: &[i64], block: usize, order: usize) {
    // 分区数 2^p：每分区至少 256 个样本、块长能整除、首分区长于阶数。
    let mut porder = 0u32;
    while porder < 8
        && block % (1 << (porder + 1)) == 0
        && block >> (porder + 1) >= 256
        && block >> (porder + 1) > order
    {
        porder += 1;
    }
    let per = block >> porder;
    let zig = |v: i64| -> u64 {
        assert!(
            v >= i64::from(i32::MIN) && v <= i64::from(i32::MAX),
            "残差 {v} 超出 32 位（RFC 9639 §9.2.6）"
        );
        ((v << 1) ^ (v >> 63)) as u64
    };
    let parts: Vec<&[i64]> = {
        let mut out = Vec::new();
        let mut idx = 0;
        for p in 0..(1usize << porder) {
            let n = if p == 0 { per - order } else { per };
            out.push(&r[idx..idx + n]);
            idx += n;
        }
        out
    };
    let params: Vec<u32> = parts
        .iter()
        .map(|part| {
            let sum: u64 = part.iter().map(|&v| zig(v)).sum();
            let mean = sum / part.len().max(1) as u64;
            (64 - mean.leading_zeros()).saturating_sub(1).min(30)
        })
        .collect();
    let rice2 = params.iter().any(|&k| k >= 15);
    w.put(u64::from(rice2), 2);
    w.put(u64::from(porder), 4);
    let pbits = if rice2 { 5 } else { 4 };
    for (part, &k) in parts.iter().zip(&params) {
        w.put(u64::from(k), pbits);
        for &v in *part {
            let u = zig(v);
            w.unary(u >> k);
            w.put(u & ((1u64 << k) - 1), k);
        }
    }
}

fn write_subframe(w: &mut BitW, s: &[i64], bits: u32, p: Predictor) {
    let (order, r) = residual(s, p);
    w.put(0, 1);
    let ty = match p {
        Predictor::Fixed(o) => 8 + o as u64,
        Predictor::Lpc(o) => 32 + o as u64 - 1,
    };
    w.put(ty, 6);
    w.put(0, 1); // 无 wasted bits
    for &x in &s[..order] {
        w.put_signed(x, bits);
    }
    if let Predictor::Lpc(o) = p {
        w.put(u64::from(LPC_PRECISION - 1), 4);
        w.put_signed(i64::from(LPC_SHIFT), 5);
        for c in lpc_coefs(o) {
            w.put_signed(c, LPC_PRECISION);
        }
    }
    write_residual(w, &r, s.len(), order);
}

/// 编码一帧（各声道独立）。
fn frame(chans: &[&[i64]], rate: u32, bits: u32, number: u64, p: Predictor) -> Vec<u8> {
    let block = chans[0].len();
    let mut w = BitW::default();
    w.put(0x7FFC, 15);
    w.put(0, 1); // 固定块大小
    // 块大小代码（RFC 9639 §9.1.1）：4096 为 12，16384 为 14，其余写 16 位“块长 − 1”。
    let (code, extra) = match block {
        4096 => (12, None),
        16384 => (14, None),
        _ => (7, Some((block as u64 - 1, 16))),
    };
    let (rc, rate_extra) = rate_code(rate);
    w.put(code, 4);
    w.put(rc, 4);
    w.put(chans.len() as u64 - 1, 4);
    w.put(bits_code(bits), 3);
    w.put(0, 1);
    coded_number(&mut w, number);
    if let Some((v, n)) = extra {
        w.put(v, n);
    }
    if let Some((v, n)) = rate_extra {
        w.put(v, n);
    }
    let crc = crc8(&w.buf);
    w.put(u64::from(crc), 8);
    for c in chans {
        write_subframe(&mut w, c, bits, p);
    }
    w.align();
    let crc = crc16(&w.buf);
    w.put(u64::from(crc), 16);
    w.buf
}

/// FLAC：`block` 为 4096 或 16384（末帧可更短）。
pub fn flac(
    samples: &[i32],
    rate: u32,
    channels: u16,
    bits: u32,
    block: usize,
    p: Predictor,
) -> Vec<u8> {
    assert!(
        block == 4096 || block == 16384,
        "块大小 {block} 只支持 4096 或 16384"
    );
    let ch = usize::from(channels);
    let chans: Vec<Vec<i64>> = (0..ch)
        .map(|c| {
            samples
                .iter()
                .skip(c)
                .step_by(ch)
                .map(|&v| i64::from(v))
                .collect()
        })
        .collect();
    let total = chans[0].len();
    let mut frames = Vec::new();
    let mut start = 0;
    while start < total {
        let end = (start + block).min(total);
        let part: Vec<&[i64]> = chans.iter().map(|c| &c[start..end]).collect();
        frames.push(frame(&part, rate, bits, frames.len() as u64, p));
        start = end;
    }
    let min_frame = frames.iter().map(Vec::len).min().unwrap_or(0);
    let max_frame = frames.iter().map(Vec::len).max().unwrap_or(0);
    let mut si = BitW::default();
    si.put(block as u64, 16);
    si.put(block as u64, 16);
    si.put(min_frame as u64, 24);
    si.put(max_frame as u64, 24);
    si.put(u64::from(rate), 20);
    si.put(u64::from(channels) - 1, 3);
    si.put(u64::from(bits) - 1, 5);
    si.put_long(total as u64, 36);
    si.put_long(0, 64);
    si.put_long(0, 64); // MD5 全零：未知
    let mut out = b"fLaC".to_vec();
    out.push(0x80);
    out.extend_from_slice(&(si.buf.len() as u32).to_be_bytes()[1..]);
    out.extend_from_slice(&si.buf);
    for f in frames {
        out.extend_from_slice(&f);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 确定性：同一种子两次生成逐字节相同；不同种子不同。
    #[test]
    fn deterministic() {
        let a = samples(7, 1000, 2, 24);
        assert_eq!(a, samples(7, 1000, 2, 24));
        assert_ne!(a, samples(8, 1000, 2, 24));
        let f = flac(&a, 96_000, 2, 24, 4096, Predictor::Lpc(32));
        assert_eq!(f, flac(&a, 96_000, 2, 24, 4096, Predictor::Lpc(32)));
        assert_eq!(&f[..4], b"fLaC");
    }

    /// 样本在位深范围内，WAV 长度正确。
    #[test]
    fn ranges_and_wav_length() {
        for bits in [16, 24, 32] {
            let s = samples(1, 5000, 8, bits);
            let lim = 1i64 << (bits - 1);
            assert!(
                s.iter()
                    .all(|&v| i64::from(v) >= -lim && i64::from(v) < lim)
            );
            let w = wav(&s, 48_000, 8, bits);
            assert_eq!(w.len(), 68 + s.len() * (bits as usize / 8));
        }
    }
}
