//! FLAC 独立参考夹具（M1 T2）。
//!
//! 两类来源，均不依赖本项目的解码器：
//! 1. Xiph `flac` 命令行（≥ 1.4.0，版本写入清单）编码已知样本的 WAV；
//! 2. 本文件按 RFC 9639 编写的帧编码器手写码流，覆盖命令行难以稳定产生的分支
//!    （escape 分区、Rice2、32 位侧声道极值、最大块 verbatim、可变块与非常用码等）。
//!
//! 每个有效夹具都用 `flac -d`（libFLAC）解码，要求与源样本逐样本相等，作为第二参考；
//! 生成器最后用 `flac --analyze` 汇总编码分支覆盖，缺分支即失败。
//!
//! 输出：`<名>.flac`、`<名>.meta`（结果、错误明细、格式、帧数），有效夹具另有
//! `<名>.s32le`（交错 i32 小端源样本）。`MANIFEST.txt` 记录工具版本、命令与覆盖。

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use crate::Result;

mod reference;

// ---------------------------------------------------------------- 工具

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 11
    }

    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % ((hi - lo + 1) as u64)) as i64
    }
}

fn lim(bits: u32) -> (i64, i64) {
    (-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1)
}

/// 可压缩信号：随机游走 + 正弦，限制在位深范围内。
fn smooth(seed: u64, frames: usize, ch: usize, bits: u32, amp_frac: f64) -> Vec<i64> {
    let (lo, hi) = lim(bits);
    let mut r = Lcg(seed);
    let amp = hi as f64 * amp_frac;
    let mut walk = vec![0f64; ch];
    let mut out = Vec::with_capacity(frames * ch);
    for i in 0..frames {
        for (c, w) in walk.iter_mut().enumerate() {
            *w = (*w + (r.range(-1000, 1000) as f64) / 1000.0 * amp * 0.01)
                .clamp(-amp * 0.3, amp * 0.3);
            let s = (i as f64 * (0.013 + c as f64 * 0.007)).sin() * amp * 0.6 + *w;
            out.push((s.round() as i64).clamp(lo, hi));
        }
    }
    out
}

fn noise(seed: u64, frames: usize, ch: usize, bits: u32) -> Vec<i64> {
    let (lo, hi) = lim(bits);
    let mut r = Lcg(seed);
    (0..frames * ch).map(|_| r.range(lo, hi)).collect()
}

fn s32le(vals: &[i64]) -> Vec<u8> {
    vals.iter()
        .flat_map(|&v| (v as i32).to_le_bytes())
        .collect()
}

/// 最小 PCM WAV（格式码 1）。
fn wav(vals: &[i64], rate: u32, ch: u16, bits: u16) -> Vec<u8> {
    let bpsamp = usize::from(bits / 8);
    let mut data = Vec::with_capacity(vals.len() * bpsamp);
    for &v in vals {
        data.extend_from_slice(&(v as u32).to_le_bytes()[..bpsamp]);
    }
    let align = ch * (bits / 8);
    let mut w = b"RIFF".to_vec();
    w.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&ch.to_le_bytes());
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * u32::from(align)).to_le_bytes());
    w.extend_from_slice(&align.to_le_bytes());
    w.extend_from_slice(&bits.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(data.len() as u32).to_le_bytes());
    w.extend_from_slice(&data);
    w
}

// ---------------------------------------------------------------- 位写入器与 CRC

#[derive(Default)]
struct BitW {
    buf: Vec<u8>,
    acc: u8,
    n: u32,
}

impl BitW {
    fn bit(&mut self, b: bool) {
        self.acc = (self.acc << 1) | u8::from(b);
        self.n += 1;
        if self.n == 8 {
            self.buf.push(self.acc);
            self.acc = 0;
            self.n = 0;
        }
    }

    fn put(&mut self, v: u64, bits: u32) {
        for i in (0..bits).rev() {
            self.bit((v >> i) & 1 == 1);
        }
    }

    fn put_signed(&mut self, v: i64, bits: u32) {
        let (lo, hi) = lim(bits);
        assert!(v >= lo && v <= hi, "样本 {v} 超出 {bits} 位");
        let mask = if bits == 64 {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        };
        self.put((v as u64) & mask, bits);
    }

    fn unary(&mut self, q: u64) {
        for _ in 0..q {
            self.bit(false);
        }
        self.bit(true);
    }

    fn align(&mut self) {
        while self.n != 0 {
            self.bit(false);
        }
    }

    fn len(&self) -> usize {
        self.buf.len()
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

// ---------------------------------------------------------------- 帧编码器

#[derive(Clone)]
enum Param {
    Rice(u32),
    /// escape：原始位宽（0 表示全零残差）。
    Escape(u32),
}

#[derive(Clone)]
struct Res {
    /// 0 = Rice（4 位参数），1 = Rice2（5 位参数）。
    method: u32,
    porder: u32,
    params: Vec<Param>,
}

/// 单分区 Rice；参数 ≥ 15 时使用 Rice2（5 位参数）。
fn rice(param: u32) -> Res {
    Res {
        method: u32::from(param >= 15),
        porder: 0,
        params: vec![Param::Rice(param)],
    }
}

#[derive(Clone)]
enum Kind {
    Constant,
    Verbatim,
    Fixed(usize, Res),
    Lpc {
        precision: u32,
        shift: i64,
        coefs: Vec<i64>,
        res: Res,
    },
}

#[derive(Clone)]
struct Sub {
    kind: Kind,
    wasted: u32,
}

fn sub(kind: Kind) -> Sub {
    Sub { kind, wasted: 0 }
}

#[derive(Clone, Copy, PartialEq)]
enum Assign {
    Indep,
    LeftSide,
    SideRight,
    MidSide,
}

#[derive(Clone)]
struct FrameSpec {
    variable: bool,
    number: u64,
    rate: u32,
    bits: u32,
    assign: Assign,
    /// 帧头采样率/位深是否写 0（取自 STREAMINFO）。
    rate_from_si: bool,
    bits_from_si: bool,
    /// 强制使用非常用块大小码（6 或 7）。
    force_bs_code: Option<u32>,
    subs: Vec<Sub>,
}

fn utf8(w: &mut BitW, v: u64) {
    if v < 0x80 {
        w.put(v, 8);
        return;
    }
    let n = (1..=6).find(|&k| v < (1u64 << (5 * k + 6))).unwrap_or(6) as u32 + 1;
    // n 字节：首字节 n 个 1、0，再接 7−n−1 位；其余每字节 6 位。
    let first_bits = 7 - n;
    let lead = (0xFFu64 << (8 - n)) & 0xFF;
    w.put(lead | (v >> (6 * (n - 1))), 8);
    let _ = first_bits;
    for i in (0..n - 1).rev() {
        w.put(0x80 | ((v >> (6 * i)) & 0x3F), 8);
    }
}

fn rate_code(rate: u32) -> (u32, Option<(u64, u32)>) {
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
        r => (14, Some((u64::from(r / 10), 16))),
    }
}

fn bits_code(bits: u32) -> u32 {
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

fn predict_fixed(s: &[i64], i: usize, order: usize) -> i64 {
    match order {
        0 => 0,
        1 => s[i - 1],
        2 => 2 * s[i - 1] - s[i - 2],
        3 => 3 * s[i - 1] - 3 * s[i - 2] + s[i - 3],
        _ => 4 * s[i - 1] - 6 * s[i - 2] + 4 * s[i - 3] - s[i - 4],
    }
}

fn write_residual(w: &mut BitW, res: &Res, r: &[i64], block: usize, order: usize) {
    w.put(u64::from(res.method), 2);
    w.put(u64::from(res.porder), 4);
    let parts = 1usize << res.porder;
    assert_eq!(res.params.len(), parts, "分区参数个数");
    let per = block >> res.porder;
    let pbits = if res.method == 0 { 4 } else { 5 };
    let esc = (1u64 << pbits) - 1;
    let mut idx = 0;
    for (p, param) in res.params.iter().enumerate() {
        let n = if p == 0 { per - order } else { per };
        let part = &r[idx..idx + n];
        match *param {
            Param::Rice(k) => {
                assert!(u64::from(k) < esc);
                w.put(u64::from(k), pbits);
                for &v in part {
                    assert!(
                        v >= i64::from(i32::MIN) && v <= i64::from(i32::MAX),
                        "残差超出 32 位"
                    );
                    let u = ((v << 1) ^ (v >> 63)) as u64;
                    w.unary(u >> k);
                    w.put(u & ((1u64 << k) - 1), k);
                }
            }
            Param::Escape(b) => {
                w.put(esc, pbits);
                w.put(u64::from(b), 5);
                for &v in part {
                    if b == 0 {
                        assert_eq!(v, 0);
                    } else {
                        w.put_signed(v, b);
                    }
                }
            }
        }
        idx += n;
    }
}

fn write_subframe(w: &mut BitW, s: &[i64], sub_bits: u32, sp: &Sub) {
    let block = s.len();
    let k = sp.wasted;
    let eff = sub_bits - k;
    let v: Vec<i64> = s
        .iter()
        .map(|&x| {
            assert_eq!(x & ((1i64 << k) - 1), 0, "wasted bits 位须为 0");
            x >> k
        })
        .collect();
    w.bit(false);
    let ty = match &sp.kind {
        Kind::Constant => 0,
        Kind::Verbatim => 1,
        Kind::Fixed(o, _) => 8 + *o as u64,
        Kind::Lpc { coefs, .. } => 32 + coefs.len() as u64 - 1,
    };
    w.put(ty, 6);
    if k > 0 {
        w.bit(true);
        w.unary(u64::from(k - 1));
    } else {
        w.bit(false);
    }
    match &sp.kind {
        Kind::Constant => {
            assert!(v.iter().all(|&x| x == v[0]));
            w.put_signed(v[0], eff);
        }
        Kind::Verbatim => v.iter().for_each(|&x| w.put_signed(x, eff)),
        Kind::Fixed(o, res) => {
            v[..*o].iter().for_each(|&x| w.put_signed(x, eff));
            let r: Vec<i64> = (*o..block)
                .map(|i| v[i] - predict_fixed(&v, i, *o))
                .collect();
            write_residual(w, res, &r, block, *o);
        }
        Kind::Lpc {
            precision,
            shift,
            coefs,
            res,
        } => {
            let o = coefs.len();
            v[..o].iter().for_each(|&x| w.put_signed(x, eff));
            w.put(u64::from(precision - 1), 4);
            w.put_signed(*shift, 5);
            coefs.iter().for_each(|&c| w.put_signed(c, *precision));
            let r: Vec<i64> = (o..block)
                .map(|i| {
                    let sum: i64 = coefs
                        .iter()
                        .enumerate()
                        .map(|(j, &c)| c * v[i - 1 - j])
                        .sum();
                    v[i] - (sum >> shift)
                })
                .collect();
            write_residual(w, res, &r, block, o);
        }
    }
}

/// 编码一帧；`chans` 为各声道原始样本（未解相关）。返回帧字节。
fn write_frame(f: &FrameSpec, chans: &[Vec<i64>]) -> Vec<u8> {
    let block = chans[0].len();
    let mut w = BitW::default();
    w.put(0x7FFC, 15);
    w.bit(f.variable);
    let (bs_code, bs_extra) = match (f.force_bs_code, block) {
        (Some(6), b) => (6, Some(((b - 1) as u64, 8))),
        (Some(_), b) => (7, Some(((b - 1) as u64, 16))),
        (None, 192) => (1, None),
        (None, b) if [576, 1152, 2304, 4608].contains(&b) => (2 + (b / 576).trailing_zeros(), None),
        (None, b) if b >= 256 && b.is_power_of_two() && b <= 32768 => {
            (8 + (b / 256).trailing_zeros(), None)
        }
        (None, b) if b <= 256 => (6, Some(((b - 1) as u64, 8))),
        (None, b) => (7, Some(((b - 1) as u64, 16))),
    };
    w.put(u64::from(bs_code), 4);
    let (rc, rextra) = if f.rate_from_si {
        (0, None)
    } else {
        rate_code(f.rate)
    };
    w.put(u64::from(rc), 4);
    let ch_code = match f.assign {
        Assign::Indep => chans.len() as u64 - 1,
        Assign::LeftSide => 8,
        Assign::SideRight => 9,
        Assign::MidSide => 10,
    };
    w.put(ch_code, 4);
    w.put(
        u64::from(if f.bits_from_si { 0 } else { bits_code(f.bits) }),
        3,
    );
    w.bit(false);
    utf8(&mut w, f.number);
    if let Some((v, b)) = bs_extra {
        w.put(v, b);
    }
    if let Some((v, b)) = rextra {
        w.put(v, b);
    }
    let c8 = crc8(&w.buf);
    w.put(u64::from(c8), 8);
    // 解相关后的子帧样本。
    let encoded: Vec<(Vec<i64>, u32)> = match f.assign {
        Assign::Indep => chans.iter().map(|c| (c.clone(), f.bits)).collect(),
        Assign::LeftSide => vec![
            (chans[0].clone(), f.bits),
            (
                chans[0].iter().zip(&chans[1]).map(|(l, r)| l - r).collect(),
                f.bits + 1,
            ),
        ],
        Assign::SideRight => vec![
            (
                chans[0].iter().zip(&chans[1]).map(|(l, r)| l - r).collect(),
                f.bits + 1,
            ),
            (chans[1].clone(), f.bits),
        ],
        Assign::MidSide => vec![
            (
                chans[0]
                    .iter()
                    .zip(&chans[1])
                    .map(|(l, r)| (l + r) >> 1)
                    .collect(),
                f.bits,
            ),
            (
                chans[0].iter().zip(&chans[1]).map(|(l, r)| l - r).collect(),
                f.bits + 1,
            ),
        ],
    };
    for ((s, b), sp) in encoded.iter().zip(&f.subs) {
        write_subframe(&mut w, s, *b, sp);
    }
    w.align();
    let c16 = crc16(&w.buf);
    w.put(u64::from(c16), 16);
    w.buf
}

struct Stream {
    rate: u32,
    channels: u32,
    bits: u32,
    /// 帧字节与每帧块大小。
    frames: Vec<(Vec<u8>, usize)>,
    /// 额外元数据块 (类型, 内容)，写在 STREAMINFO 之后。
    meta: Vec<(u8, Vec<u8>)>,
    /// 覆盖 STREAMINFO 的最大帧长度（0 表示未知）。
    max_frame_override: Option<u32>,
    min_frame_override: Option<u32>,
    total_override: Option<u64>,
    max_block_override: Option<u32>,
}

fn stream_bytes(s: &Stream) -> Vec<u8> {
    let blocks: Vec<usize> = s.frames.iter().map(|f| f.1).collect();
    let lens: Vec<usize> = s.frames.iter().map(|f| f.0.len()).collect();
    let min_block = blocks[..blocks.len().saturating_sub(1).max(1)]
        .iter()
        .copied()
        .min()
        .unwrap_or(16)
        .max(16);
    let max_block = s
        .max_block_override
        .unwrap_or(blocks.iter().copied().max().unwrap_or(16).max(16) as u32);
    let mut si = BitW::default();
    si.put(min_block.min(max_block as usize) as u64, 16);
    si.put(u64::from(max_block), 16);
    si.put(
        u64::from(
            s.min_frame_override
                .unwrap_or(lens.iter().copied().min().unwrap_or(0) as u32),
        ),
        24,
    );
    si.put(
        u64::from(
            s.max_frame_override
                .unwrap_or(lens.iter().copied().max().unwrap_or(0) as u32),
        ),
        24,
    );
    si.put(u64::from(s.rate), 20);
    si.put(u64::from(s.channels - 1), 3);
    si.put(u64::from(s.bits - 1), 5);
    si.put(
        s.total_override
            .unwrap_or(blocks.iter().sum::<usize>() as u64),
        36,
    );
    si.put(0, 64);
    si.put(0, 64); // MD5 全零：未知
    let mut out = b"fLaC".to_vec();
    let mut blocks_meta: Vec<(u8, Vec<u8>)> = vec![(0, si.buf)];
    blocks_meta.extend(s.meta.iter().cloned());
    let n = blocks_meta.len();
    for (i, (ty, body)) in blocks_meta.iter().enumerate() {
        out.push(if i + 1 == n { 0x80 | ty } else { *ty });
        out.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        out.extend_from_slice(body);
    }
    for (f, _) in &s.frames {
        out.extend_from_slice(f);
    }
    out
}

fn frame_spec(rate: u32, bits: u32, assign: Assign, number: u64, subs: Vec<Sub>) -> FrameSpec {
    FrameSpec {
        variable: false,
        number,
        rate,
        bits,
        assign,
        rate_from_si: false,
        bits_from_si: false,
        force_bs_code: None,
        subs,
    }
}

/// 交错样本 → 各声道。
fn split(vals: &[i64], ch: usize) -> Vec<Vec<i64>> {
    (0..ch)
        .map(|c| vals.iter().skip(c).step_by(ch).copied().collect())
        .collect()
}

fn interleave(chans: &[Vec<i64>]) -> Vec<i64> {
    let n = chans[0].len();
    (0..n)
        .flat_map(|i| chans.iter().map(move |c| c[i]))
        .collect()
}

// ---------------------------------------------------------------- 夹具

struct Fixture {
    name: String,
    data: Vec<u8>,
    /// 期望结果："ok" 或 DecodeError 名。
    result: &'static str,
    /// 期望错误明细（FlacError 名）。
    detail: &'static str,
    format: Option<(u32, u32, u32)>,
    /// 期望输出的交错样本（错误夹具为出错前已完整输出的帧）。
    samples: Vec<i64>,
    /// 由 libFLAC 校验（错误夹具与本项目不支持的格式不校验）。
    libflac_check: bool,
    origin: String,
}

fn crafted(name: &str, s: &Stream, samples: Vec<i64>, origin: &str) -> Fixture {
    Fixture {
        name: name.into(),
        data: stream_bytes(s),
        result: "ok",
        detail: "-",
        format: Some((s.rate, s.channels, s.bits)),
        samples,
        libflac_check: true,
        origin: format!("手写：{origin}"),
    }
}

fn stream(rate: u32, channels: u32, bits: u32, frames: Vec<(Vec<u8>, usize)>) -> Stream {
    Stream {
        rate,
        channels,
        bits,
        frames,
        meta: Vec::new(),
        max_frame_override: None,
        min_frame_override: None,
        total_override: None,
        max_block_override: None,
    }
}

/// 把交错样本按块切分并用同一组子帧规格逐帧编码。
fn encode_blocks(
    vals: &[i64],
    ch: usize,
    rate: u32,
    bits: u32,
    block: usize,
    assign: Assign,
    subs: &dyn Fn(usize) -> Vec<Sub>,
) -> Vec<(Vec<u8>, usize)> {
    let chans = split(vals, ch);
    let total = chans[0].len();
    let mut out = Vec::new();
    let mut start = 0;
    let mut n = 0u64;
    while start < total {
        let end = (start + block).min(total);
        let part: Vec<Vec<i64>> = chans.iter().map(|c| c[start..end].to_vec()).collect();
        let f = frame_spec(rate, bits, assign, n, subs(n as usize));
        out.push((write_frame(&f, &part), end - start));
        start = end;
        n += 1;
    }
    out
}

fn crafted_cases() -> Vec<Fixture> {
    let mut v = Vec::new();

    // 最大块 verbatim：65535 帧 × 双声道 × 32 位，含极值（约 512 KiB 帧）。
    let mut vals = noise(101, 65_535, 2, 32);
    vals[0] = i64::from(i32::MAX);
    vals[1] = i64::from(i32::MIN);
    vals[2] = i64::from(i32::MIN);
    vals[3] = i64::from(i32::MAX);
    let frames = encode_blocks(&vals, 2, 48_000, 32, 65_535, Assign::Indep, &|_| {
        vec![sub(Kind::Verbatim), sub(Kind::Verbatim)]
    });
    v.push(crafted(
        "max_block_verbatim_32_stereo",
        &stream(48_000, 2, 32, frames),
        vals,
        "65535 块 verbatim，32 位极值",
    ));

    // 32 位侧声道 33 位极值：left/side、side/right、mid/side 各一帧，侧声道 verbatim 33 位。
    let pairs: Vec<(i64, i64)> = vec![
        (i64::from(i32::MAX), i64::from(i32::MIN)),
        (i64::from(i32::MIN), i64::from(i32::MAX)),
        (i64::from(i32::MAX), i64::from(i32::MAX)),
        (i64::from(i32::MIN), i64::from(i32::MIN)),
        (0, i64::from(i32::MIN)),
        (-1, i64::from(i32::MAX)),
        (12345, -67890),
        (i64::from(i32::MAX), 0),
    ];
    let mut all = Vec::new();
    let mut frames = Vec::new();
    for (n, assign) in [
        Assign::LeftSide,
        Assign::SideRight,
        Assign::MidSide,
        Assign::Indep,
    ]
    .into_iter()
    .enumerate()
    {
        let l: Vec<i64> = pairs.iter().map(|p| p.0).cycle().take(32).collect();
        let r: Vec<i64> = pairs.iter().map(|p| p.1).cycle().take(32).collect();
        let subs = vec![sub(Kind::Verbatim), sub(Kind::Verbatim)];
        let f = frame_spec(44_100, 32, assign, n as u64, subs);
        frames.push((write_frame(&f, &[l.clone(), r.clone()]), 32));
        all.extend(interleave(&[l, r]));
    }
    // 33 位侧声道上的固定预测与 LPC（残差在 32 位内）。
    let l: Vec<i64> = (0..64).map(|i| i64::from(i32::MAX) - i * 1000).collect();
    let r: Vec<i64> = (0..64).map(|i| i64::from(i32::MIN) + i * 777).collect();
    let f = frame_spec(
        44_100,
        32,
        Assign::LeftSide,
        4,
        vec![sub(Kind::Fixed(1, rice(12))), sub(Kind::Fixed(2, rice(3)))],
    );
    frames.push((write_frame(&f, &[l.clone(), r.clone()]), 64));
    all.extend(interleave(&[l, r]));
    let l: Vec<i64> = (0..64).map(|i| -(i * 999_999)).collect();
    let r: Vec<i64> = (0..64).map(|i| i64::from(i32::MAX) - i * 5).collect();
    let lpc = Kind::Lpc {
        precision: 15,
        shift: 13,
        coefs: vec![8192, 0],
        res: rice(25),
    };
    let f = frame_spec(
        44_100,
        32,
        Assign::MidSide,
        5,
        vec![sub(Kind::Fixed(1, rice(20))), sub(lpc)],
    );
    frames.push((write_frame(&f, &[l.clone(), r.clone()]), 64));
    all.extend(interleave(&[l, r]));
    v.push(crafted(
        "side_33bit_extremes",
        &stream(44_100, 2, 32, frames),
        all,
        "32 位 left/side、side/right、mid/side 极值，33 位侧声道 verbatim/固定/LPC",
    ));

    // escape 分区与 Rice 参数边界：固定 2 阶，分区阶数 2：escape 12 位、escape 0 位、Rice 0、Rice 14。
    let mut s: Vec<i64> = Vec::new();
    for i in 0..256i64 {
        // 样本 62..128 为线性段：其二阶残差（分区 1，样本 64..128）全为 0。
        s.push(match i {
            0..=61 => (i * 37 % 200) - 100,
            62..=127 => i * 3 - 200,
            _ => match i / 64 {
                2 => 50 + (i % 3) - 1,
                _ => (i * 10_007) % 30_000,
            },
        });
    }
    // 第 2 分区（64..128）为二阶线性：二阶残差为 0，可用 0 位 escape。
    let res = Res {
        method: 0,
        porder: 2,
        params: vec![
            Param::Escape(12),
            Param::Escape(0),
            Param::Rice(0),
            Param::Rice(14),
        ],
    };
    let check: Vec<i64> = (2..256).map(|i| s[i] - (2 * s[i - 1] - s[i - 2])).collect();
    let part1 = &check[62..126];
    assert!(part1.iter().all(|&x| x == 0), "escape 0 位分区残差须全零");
    let f = frame_spec(16_000, 16, Assign::Indep, 0, vec![sub(Kind::Fixed(2, res))]);
    let frames = vec![(write_frame(&f, &[s.clone()]), 256)];
    v.push(crafted(
        "escape_partitions",
        &stream(16_000, 1, 16, frames),
        s,
        "固定 2 阶，escape 12 位/0 位，Rice 参数 0 与 14",
    ));

    // Rice2：24 位噪声，固定 0 阶，参数 23 与 escape 31（Rice2 的 escape 码 0b11111）。
    let s = noise(202, 512, 1, 24);
    let res = Res {
        method: 1,
        porder: 1,
        params: vec![Param::Rice(23), Param::Escape(24)],
    };
    let f = frame_spec(96_000, 24, Assign::Indep, 0, vec![sub(Kind::Fixed(0, res))]);
    let frames = vec![(write_frame(&f, std::slice::from_ref(&s)), 512)];
    v.push(crafted(
        "rice2_partitions",
        &stream(96_000, 1, 24, frames),
        s,
        "Rice2 参数 23、Rice2 escape 24 位",
    ));

    // wasted bits：常量、verbatim、固定、LPC 各带 wasted bits。
    let base = smooth(303, 300, 2, 12, 0.8);
    let vals: Vec<i64> = base.iter().map(|x| x << 4).collect(); // 16 位，低 4 位为 0
    let frames = encode_blocks(&vals, 2, 44_100, 16, 100, Assign::Indep, &|n| match n {
        0 => vec![
            Sub {
                kind: Kind::Verbatim,
                wasted: 4,
            },
            Sub {
                kind: Kind::Fixed(3, rice(5)),
                wasted: 4,
            },
        ],
        1 => vec![
            Sub {
                kind: Kind::Lpc {
                    precision: 12,
                    shift: 10,
                    coefs: vec![2000, -980],
                    res: rice(6),
                },
                wasted: 3,
            },
            Sub {
                kind: Kind::Fixed(4, rice(7)),
                wasted: 1,
            },
        ],
        _ => vec![
            Sub {
                kind: Kind::Fixed(1, rice(4)),
                wasted: 2,
            },
            Sub {
                kind: Kind::Verbatim,
                wasted: 4,
            },
        ],
    });
    let mut s = stream(44_100, 2, 16, frames);
    let c = vec![1i64 << 5; 40];
    let f = frame_spec(
        44_100,
        16,
        Assign::Indep,
        3,
        vec![
            Sub {
                kind: Kind::Constant,
                wasted: 5,
            },
            Sub {
                kind: Kind::Constant,
                wasted: 0,
            },
        ],
    );
    s.frames
        .push((write_frame(&f, &[c.clone(), vec![-7; 40]]), 40));
    let mut all = vals;
    all.extend(interleave(&[c, vec![-7; 40]]));
    v.push(crafted(
        "wasted_bits",
        &s,
        all,
        "wasted bits 1–5，常量/verbatim/固定/LPC",
    ));

    // 固定 0–4 阶与 LPC 1–32 阶（最高精度 15、位移 15）。
    let vals = smooth(404, 640 * 6, 1, 20, 0.7);
    let frames = encode_blocks(&vals, 1, 48_000, 20, 640, Assign::Indep, &|n| {
        let k = match n % 6 {
            5 => Kind::Lpc {
                precision: 15,
                shift: 15,
                coefs: (0..32)
                    .map(|j| {
                        if j == 0 {
                            16_000
                        } else if j == 1 {
                            -7_500
                        } else {
                            (j as i64 % 5) - 2
                        }
                    })
                    .collect(),
                res: Res {
                    method: 0,
                    porder: 4,
                    params: vec![Param::Rice(14); 16],
                },
            },
            o => Kind::Fixed(o.min(4), rice(14)),
        };
        vec![sub(k)]
    });
    v.push(crafted(
        "fixed_orders_and_lpc32",
        &stream(48_000, 1, 20, frames),
        vals,
        "固定 0–4 阶，LPC 32 阶精度 15 位移 15，分区阶数 4",
    ));

    // 可变块策略 + 非常用块大小码与 STREAMINFO 继承的采样率/位深。
    let vals = smooth(505, 16 + 1000 + 192 + 4608 + 300, 2, 16, 0.5);
    let chans = split(&vals, 2);
    let mut frames = Vec::new();
    let mut start = 0usize;
    for (i, (bs, force, from_si)) in [
        (16usize, Some(6), false),
        (1000, Some(7), true),
        (192, None, false),
        (4608, None, true),
        (300, None, false),
    ]
    .into_iter()
    .enumerate()
    {
        let part: Vec<Vec<i64>> = chans
            .iter()
            .map(|c| c[start..start + bs].to_vec())
            .collect();
        let f = FrameSpec {
            variable: true,
            number: start as u64,
            force_bs_code: force,
            rate_from_si: from_si,
            bits_from_si: from_si,
            ..frame_spec(
                44_100,
                16,
                if i % 2 == 0 {
                    Assign::MidSide
                } else {
                    Assign::SideRight
                },
                0,
                vec![sub(Kind::Fixed(2, rice(8))), sub(Kind::Fixed(1, rice(8)))],
            )
        };
        frames.push((write_frame(&f, &part), bs));
        start += bs;
    }
    v.push(crafted(
        "variable_blocksize_uncommon",
        &stream(44_100, 2, 16, frames),
        vals,
        "可变块，块大小码 6/7/1/5 与 8/16 位非常用值，采样率/位深取自 STREAMINFO",
    ));

    // 非常用采样率码 12/13/14。
    for (name, rate) in [
        ("rate_code12_7khz", 7_000u32),
        ("rate_code13_11025", 11_025),
        ("rate_code14_100k", 100_000),
    ] {
        let vals = smooth(606 + u64::from(rate), 500, 1, 16, 0.5);
        let frames = encode_blocks(&vals, 1, rate, 16, 250, Assign::Indep, &|_| {
            vec![sub(Kind::Fixed(2, rice(6)))]
        });
        v.push(crafted(
            name,
            &stream(rate, 1, 16, frames),
            vals,
            &format!("非常用采样率 {rate} Hz"),
        ));
    }

    // 元数据：PADDING、VORBIS_COMMENT、PICTURE（100 KB，不缓存）、保留类型 10，均跳过。
    let vals = smooth(707, 1000, 2, 24, 0.5);
    let frames = encode_blocks(&vals, 2, 48_000, 24, 512, Assign::LeftSide, &|_| {
        vec![sub(Kind::Fixed(2, rice(10))), sub(Kind::Fixed(2, rice(10)))]
    });
    let mut s = stream(48_000, 2, 24, frames);
    s.meta = vec![
        (1, vec![0; 333]),
        (4, vorbis_comment()),
        (6, picture(100_000)),
        (10, vec![1, 2, 3]),
    ];
    v.push(crafted(
        "metadata_skip",
        &s,
        vals.clone(),
        "填充、注释、100 KB 图片与保留类型元数据",
    ));

    // STREAMINFO 最大帧长度未知（0）。
    let frames = encode_blocks(&vals, 2, 48_000, 24, 512, Assign::LeftSide, &|_| {
        vec![sub(Kind::Fixed(2, rice(10))), sub(Kind::Fixed(2, rice(10)))]
    });
    let mut s = stream(48_000, 2, 24, frames);
    s.max_frame_override = Some(0);
    s.min_frame_override = Some(0);
    v.push(crafted(
        "unknown_max_frame",
        &s,
        vals.clone(),
        "STREAMINFO 最小/最大帧长度为 0（未知）",
    ));

    // STREAMINFO 谎报最大帧长度（远小于实际）：不可信，按实际帧在窗口内解码。
    let frames = encode_blocks(&vals, 2, 48_000, 24, 512, Assign::LeftSide, &|_| {
        vec![sub(Kind::Fixed(2, rice(10))), sub(Kind::Fixed(2, rice(10)))]
    });
    let mut s = stream(48_000, 2, 24, frames);
    s.max_frame_override = Some(100);
    v.push(crafted(
        "lying_max_frame",
        &s,
        vals.clone(),
        "STREAMINFO 最大帧长度谎报为 100",
    ));

    // 完整短尾帧：固定块 512，最后一帧 17。
    let vals = smooth(808, 512 * 2 + 17, 1, 16, 0.6);
    let frames = encode_blocks(&vals, 1, 22_050, 16, 512, Assign::Indep, &|_| {
        vec![sub(Kind::Fixed(2, rice(7)))]
    });
    v.push(crafted(
        "short_tail_frame",
        &stream(22_050, 1, 16, frames),
        vals,
        "固定块 512，尾帧 17 帧",
    ));

    v
}

/// 合法的 VORBIS_COMMENT（§8.6）：vendor 字符串与 0 条注释（字段为小端）。
fn vorbis_comment() -> Vec<u8> {
    let vendor = b"qybit-gen";
    let mut b = (vendor.len() as u32).to_le_bytes().to_vec();
    b.extend_from_slice(vendor);
    b.extend_from_slice(&0u32.to_le_bytes());
    b
}

/// 合法的 PICTURE（§8.8）：类型 3、MIME、描述、尺寸与 `n` 字节图像数据（大端字段）。
fn picture(n: usize) -> Vec<u8> {
    let mut b = Vec::new();
    let mime = b"image/png";
    b.extend_from_slice(&3u32.to_be_bytes());
    b.extend_from_slice(&(mime.len() as u32).to_be_bytes());
    b.extend_from_slice(mime);
    b.extend_from_slice(&0u32.to_be_bytes());
    for v in [16u32, 16, 24, 0] {
        b.extend_from_slice(&v.to_be_bytes());
    }
    b.extend_from_slice(&(n as u32).to_be_bytes());
    b.extend((0..n).map(|i| (i * 31 % 251) as u8));
    b
}

fn err_fixture(
    name: &str,
    data: Vec<u8>,
    result: &'static str,
    detail: &'static str,
    format: Option<(u32, u32, u32)>,
    samples: Vec<i64>,
    origin: &str,
) -> Fixture {
    Fixture {
        name: name.into(),
        data,
        result,
        detail,
        format,
        samples,
        libflac_check: false,
        origin: format!("手写错误：{origin}"),
    }
}

fn error_cases() -> Vec<Fixture> {
    let mut v = Vec::new();
    let vals = smooth(909, 1024, 2, 16, 0.5);
    let base_frames = || {
        encode_blocks(&vals, 2, 44_100, 16, 512, Assign::Indep, &|_| {
            vec![sub(Kind::Verbatim), sub(Kind::Fixed(2, rice(9)))]
        })
    };
    let base = stream_bytes(&stream(44_100, 2, 16, base_frames()));
    let fmt = Some((44_100, 2, 16));
    let meta_len = 4 + 4 + 34;
    let f0_len = base_frames()[0].0.len();
    let first = vals[..512 * 2].to_vec();

    let mut d = base.clone();
    d[meta_len + 4] ^= 0x01; // 帧号字节（第 5 字节）0→1：编码仍合法，CRC-8 不符
    v.push(err_fixture(
        "err_header_crc",
        d,
        "Malformed",
        "HeaderCrc",
        fmt,
        vec![],
        "首帧帧头 1 位翻转",
    ));
    let mut d = base.clone();
    d[meta_len + f0_len + 20] ^= 0x10; // 第二帧 verbatim 样本区
    v.push(err_fixture(
        "err_frame_crc",
        d,
        "Malformed",
        "FrameCrc",
        fmt,
        first.clone(),
        "第二帧 verbatim 样本 1 位翻转",
    ));
    v.push(err_fixture(
        "err_truncated_mid_frame",
        base[..base.len() - 7].to_vec(),
        "Truncated",
        "Truncated",
        fmt,
        first.clone(),
        "尾帧缺最后 7 字节",
    ));
    v.push(err_fixture(
        "err_truncated_at_frame_boundary",
        base[..meta_len + f0_len].to_vec(),
        "Truncated",
        "Truncated",
        fmt,
        first.clone(),
        "在帧边界截断，STREAMINFO 总样本数不足",
    ));
    let mut d = base.clone();
    d[0] = b'f' ^ 0x20;
    v.push(err_fixture(
        "err_no_marker",
        d,
        "Malformed",
        "BadMarker",
        None,
        vec![],
        "标记不是 fLaC",
    ));
    let mut d = base.clone();
    d[4] = 0x81; // 第一个块类型改为 1
    v.push(err_fixture(
        "err_first_block_not_streaminfo",
        d,
        "Malformed",
        "BadStreamInfo",
        None,
        vec![],
        "首个元数据块不是 STREAMINFO",
    ));
    let mut s = stream(44_100, 2, 16, base_frames());
    s.meta = vec![(127, vec![0; 4])];
    v.push(err_fixture(
        "err_metadata_type_127",
        stream_bytes(&s),
        "Malformed",
        "BadMetadataType",
        fmt,
        vec![],
        "元数据块类型 127",
    ));

    // 语法错误：在 CRC 正确的前提下写入非法字段。
    let one = |subs: Vec<Sub>, s: &[i64]| -> Vec<u8> {
        let f = frame_spec(44_100, 16, Assign::Indep, 0, subs);
        stream_bytes(&stream(
            44_100,
            1,
            16,
            vec![(write_frame(&f, &[s.to_vec()]), s.len())],
        ))
    };
    let mono = smooth(1001, 64, 1, 16, 0.3);
    // 负位移：先按位移 0 编码再改写位移字段并重算 CRC。
    let mut d = one(
        vec![sub(Kind::Lpc {
            precision: 8,
            shift: 0,
            coefs: vec![1],
            res: rice(10),
        })],
        &mono,
    );
    patch_lpc_field(&mut d, &mono, true);
    v.push(err_fixture(
        "err_negative_lpc_shift",
        d,
        "Malformed",
        "NegativeShift",
        Some((44_100, 1, 16)),
        vec![],
        "LPC 右移量 −1（CRC 正确）",
    ));
    let mut d = one(
        vec![sub(Kind::Lpc {
            precision: 8,
            shift: 0,
            coefs: vec![1],
            res: rice(10),
        })],
        &mono,
    );
    patch_lpc_field(&mut d, &mono, false);
    v.push(err_fixture(
        "err_lpc_precision_15",
        d,
        "Malformed",
        "BadLpcPrecision",
        Some((44_100, 1, 16)),
        vec![],
        "LPC 精度字段 0b1111（CRC 正确）",
    ));
    // 分区阶数不能整除块大小：块 100、分区阶数 3。
    let m100 = smooth(1002, 100, 1, 16, 0.3);
    let d = raw_frame_stream(44_100, 1, 16, 100, |w| {
        w.bit(false);
        w.put(8, 6); // 固定 0 阶
        w.bit(false);
        w.put(0, 2);
        w.put(3, 4);
    });
    v.push(err_fixture(
        "err_partition_order",
        d,
        "Malformed",
        "BadPartitionOrder",
        Some((44_100, 1, 16)),
        vec![],
        "块 100、分区阶数 3",
    ));
    let _ = m100;
    // 残差超出 32 位：Rice2 参数 30、商 4 → 4·2^30 = 2^32 > u32::MAX（只需几个字节）。
    let d = raw_frame_stream(44_100, 1, 16, 16, |w| {
        w.bit(false);
        w.put(8, 6);
        w.bit(false);
        w.put(1, 2);
        w.put(0, 4);
        w.put(30, 5);
        w.unary(4);
        w.put(0, 30);
    });
    v.push(err_fixture(
        "err_residual_overflow",
        d,
        "Malformed",
        "ResidualOverflow",
        Some((44_100, 1, 16)),
        vec![],
        "Rice 商 2^32",
    ));
    // 预测结果溢出位深：16 位固定 1 阶，预热 32767，残差 +1。
    let d = raw_frame_stream(44_100, 1, 16, 16, |w| {
        w.bit(false);
        w.put(9, 6);
        w.bit(false);
        w.put_signed(32_767, 16);
        w.put(0, 2);
        w.put(0, 4);
        w.put(0, 4);
        for _ in 0..15 {
            w.unary(2); // zigzag 2 → +1
        }
    });
    v.push(err_fixture(
        "err_sample_overflow",
        d,
        "Malformed",
        "SampleOverflow",
        Some((44_100, 1, 16)),
        vec![],
        "16 位预测结果 32768",
    ));
    // 帧尾填充位非零（CRC 正确）。
    let d = raw_frame_stream_pad(44_100, 1, 16, 16, true);
    v.push(err_fixture(
        "err_nonzero_padding",
        d,
        "Malformed",
        "NonZeroPadding",
        Some((44_100, 1, 16)),
        vec![],
        "帧尾填充位为 1",
    ));
    // 帧位深与 STREAMINFO 不一致。
    let f = frame_spec(44_100, 24, Assign::Indep, 0, vec![sub(Kind::Verbatim)]);
    let st = Stream {
        bits: 16,
        ..stream(
            44_100,
            1,
            16,
            vec![(
                write_frame(
                    &f,
                    &[vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]],
                ),
                16,
            )],
        )
    };
    v.push(err_fixture(
        "err_stream_mismatch",
        stream_bytes(&st),
        "Malformed",
        "StreamMismatch",
        Some((44_100, 1, 16)),
        vec![],
        "帧位深 24、STREAMINFO 16",
    ));
    // 阻塞策略中途改变：先固定块后可变块。
    let s2 = smooth(1003, 64, 1, 16, 0.3);
    let f1 = frame_spec(44_100, 16, Assign::Indep, 0, vec![sub(Kind::Verbatim)]);
    let f2 = FrameSpec {
        variable: true,
        number: 32,
        ..frame_spec(44_100, 16, Assign::Indep, 0, vec![sub(Kind::Verbatim)])
    };
    let st = stream(
        44_100,
        1,
        16,
        vec![
            (write_frame(&f1, &[s2[..32].to_vec()]), 32),
            (write_frame(&f2, &[s2[32..].to_vec()]), 32),
        ],
    );
    v.push(err_fixture(
        "err_blocking_change",
        stream_bytes(&st),
        "Malformed",
        "StreamMismatch",
        Some((44_100, 1, 16)),
        s2[..32].to_vec(),
        "第二帧改为可变块",
    ));
    // 保留码：块大小码 0、声道码 11。
    let mut d = one(vec![sub(Kind::Verbatim)], &mono);
    patch_header(&mut d, |h| h[2] &= 0x0F);
    v.push(err_fixture(
        "err_reserved_blocksize",
        d,
        "Malformed",
        "ReservedCode",
        Some((44_100, 1, 16)),
        vec![],
        "块大小码 0（CRC-8 重算）",
    ));
    let mut d = one(vec![sub(Kind::Verbatim)], &mono);
    patch_header(&mut d, |h| h[3] = (h[3] & 0x0F) | 0xB0);
    v.push(err_fixture(
        "err_reserved_channels",
        d,
        "Malformed",
        "ReservedCode",
        Some((44_100, 1, 16)),
        vec![],
        "声道码 11（CRC-8 重算）",
    ));
    // 不支持：3 声道、384 kHz、3 位位深（RFC 下限 4 位为非法）。
    let tri = smooth(1004, 64, 3, 16, 0.3);
    let frames = encode_blocks(&tri, 3, 44_100, 16, 64, Assign::Indep, &|_| {
        vec![sub(Kind::Verbatim); 3]
    });
    let mut f3 = err_fixture(
        "err_unsupported_3ch",
        stream_bytes(&stream(44_100, 3, 16, frames)),
        "Unsupported",
        "Unsupported",
        None,
        vec![],
        "3 声道（libFLAC 可解）",
    );
    f3.libflac_check = false;
    v.push(f3);
    let frames = encode_blocks(&mono, 1, 384_000, 16, 64, Assign::Indep, &|_| {
        vec![sub(Kind::Verbatim)]
    });
    v.push(err_fixture(
        "err_unsupported_384k",
        stream_bytes(&stream(384_000, 1, 16, frames)),
        "Unsupported",
        "Unsupported",
        None,
        vec![],
        "384 kHz",
    ));
    v
}

/// 构造只含一帧的流，帧体由闭包写出子帧（帧头与 CRC 自动生成）。
fn raw_frame_stream(
    rate: u32,
    ch: u32,
    bits: u32,
    block: usize,
    body: impl Fn(&mut BitW),
) -> Vec<u8> {
    raw_frame_stream_inner(rate, ch, bits, block, body, false)
}

fn raw_frame_stream_pad(rate: u32, ch: u32, bits: u32, block: usize, bad_pad: bool) -> Vec<u8> {
    raw_frame_stream_inner(
        rate,
        ch,
        bits,
        block,
        |w| {
            // 固定 0 阶、Rice 参数 0、16 个零残差：1+6+1 + 2+4+4 + 16 = 34 位，帧尾需 6 位填充。
            w.bit(false);
            w.put(8, 6);
            w.bit(false);
            w.put(0, 2);
            w.put(0, 4);
            w.put(0, 4);
            for _ in 0..16 {
                w.unary(0);
            }
        },
        bad_pad,
    )
}

fn raw_frame_stream_inner(
    rate: u32,
    ch: u32,
    bits: u32,
    block: usize,
    body: impl Fn(&mut BitW),
    bad_pad: bool,
) -> Vec<u8> {
    let mut w = BitW::default();
    w.put(0x7FFC, 15);
    w.bit(false);
    w.put(7, 4);
    w.put(u64::from(rate_code(rate).0), 4);
    w.put(u64::from(ch - 1), 4);
    w.put(u64::from(bits_code(bits)), 3);
    w.bit(false);
    w.put(0, 8);
    w.put((block - 1) as u64, 16);
    let c = crc8(&w.buf);
    w.put(u64::from(c), 8);
    body(&mut w);
    if bad_pad {
        // 帧尾填充位全部写 1（CRC-16 仍按实际字节计算，因此只有填充非零这一处错误）。
        while w.n != 0 {
            w.bit(true);
        }
    } else {
        w.align();
    }
    let c = crc16(&w.buf);
    w.put(u64::from(c), 16);
    let n = w.len();
    let s = Stream {
        max_block_override: Some(block.max(16) as u32),
        ..stream(rate, ch, bits, vec![(w.buf, block)])
    };
    let _ = n;
    stream_bytes(&s)
}

/// 修改唯一一帧的帧头（闭包得到帧头字节，不含 CRC-8），并重算 CRC-8 与 CRC-16。
fn patch_header(stream: &mut [u8], f: impl Fn(&mut [u8])) {
    let start = 4 + 4 + 34;
    // 本工具生成的单帧流：固定块、帧号 1 字节、块大小码由块大小决定；
    // 定位 CRC-8：扫描到第一个满足 crc8(头) == 下一字节 的位置。
    let hlen = (5..16)
        .find(|&l| crc8(&stream[start..start + l]) == stream[start + l])
        .unwrap_or(4);
    f(&mut stream[start..start + hlen]);
    stream[start + hlen] = crc8(&stream[start..start + hlen]);
    let end = stream.len() - 2;
    let c = crc16(&stream[start..end]);
    stream[end..].copy_from_slice(&c.to_be_bytes());
}

/// 在单帧 LPC 1 阶流中改写位移（−1）或精度（0b1111），并重算 CRC-16。
fn patch_lpc_field(stream: &mut Vec<u8>, mono: &[i64], negative_shift: bool) {
    // 重新编码帧体，按位定位字段：帧头长度由 CRC-8 定位。
    let start = 4 + 4 + 34;
    let hlen = (5..16)
        .find(|&l| crc8(&stream[start..start + l]) == stream[start + l])
        .unwrap_or(4)
        + 1;
    // 子帧：1 + 6 + 1 位，预热 16 位，精度 4 位，位移 5 位。
    let base_bit = (start + hlen) * 8 + 8 + 16;
    let set_bits = |s: &mut Vec<u8>, bit: usize, v: u64, n: usize| {
        for i in 0..n {
            let b = bit + i;
            let mask = 0x80u8 >> (b % 8);
            if (v >> (n - 1 - i)) & 1 == 1 {
                s[b / 8] |= mask;
            } else {
                s[b / 8] &= !mask;
            }
        }
    };
    if negative_shift {
        set_bits(stream, base_bit + 4, 0b11111, 5);
    } else {
        set_bits(stream, base_bit, 0b1111, 4);
    }
    let end = stream.len() - 2;
    let c = crc16(&stream[start..end]);
    stream[end..].copy_from_slice(&c.to_be_bytes());
    let _ = mono;
}

// ---------------------------------------------------------------- flac 命令行

fn run(cmd: &mut Command) -> Result<Vec<u8>> {
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(format!("{:?} 失败: {}", cmd, String::from_utf8_lossy(&out.stderr)).into());
    }
    Ok(out.stdout)
}

struct CliCase {
    name: &'static str,
    rate: u32,
    ch: u16,
    bits: u16,
    vals: Vec<i64>,
    args: &'static [&'static str],
}

// 用例逐个追加，部分用例需先计算样本，保持统一写法。
#[allow(clippy::vec_init_then_push)]
fn cli_cases() -> Vec<CliCase> {
    let mut v = Vec::new();
    v.push(CliCase {
        name: "cli_16_stereo_44k_default",
        rate: 44_100,
        ch: 2,
        bits: 16,
        vals: smooth(11, 20_000, 2, 16, 0.5),
        args: &[],
    });
    v.push(CliCase {
        name: "cli_16_stereo_exhaustive_ms",
        rate: 48_000,
        ch: 2,
        bits: 16,
        vals: smooth(12, 12_000, 2, 16, 0.4),
        args: &["-8", "-e", "-m"],
    });
    v.push(CliCase {
        name: "cli_24_mono_96k_lpc12",
        rate: 96_000,
        ch: 1,
        bits: 24,
        vals: smooth(13, 15_000, 1, 24, 0.7),
        args: &["-l", "12", "-r", "6"],
    });
    v.push(CliCase {
        name: "cli_24_stereo_fixed_only",
        rate: 88_200,
        ch: 2,
        bits: 24,
        vals: smooth(14, 9_000, 2, 24, 0.6),
        args: &["-l", "0"],
    });
    v.push(CliCase {
        name: "cli_32_stereo_192k",
        rate: 192_000,
        ch: 2,
        bits: 32,
        vals: smooth(15, 20_000, 2, 32, 0.9),
        args: &["--lax", "-8"],
    });
    v.push(CliCase {
        name: "cli_32_noise_verbatim",
        rate: 176_400,
        ch: 2,
        bits: 32,
        vals: noise(16, 9_000, 2, 32),
        args: &["--lax"],
    });
    let silent: Vec<i64> = smooth(17, 8_000, 2, 16, 0.5)
        .chunks(2)
        .flat_map(|f| [f[0], 0])
        .collect();
    v.push(CliCase {
        name: "cli_16_constant_channel",
        rate: 44_100,
        ch: 2,
        bits: 16,
        vals: silent,
        args: &[],
    });
    let wasted: Vec<i64> = smooth(18, 8_000, 1, 12, 0.6)
        .iter()
        .map(|x| x << 4)
        .collect();
    v.push(CliCase {
        name: "cli_16_wasted_bits",
        rate: 32_000,
        ch: 1,
        bits: 16,
        vals: wasted,
        args: &[],
    });
    v.push(CliCase {
        name: "cli_24_rice2_noise",
        rate: 48_000,
        ch: 1,
        bits: 24,
        vals: noise(19, 8_000, 1, 24)
            .iter()
            .zip(smooth(20, 8_000, 1, 24, 0.5))
            .map(|(n, s)| ((n >> 3) + s).clamp(-(1 << 23), (1 << 23) - 1))
            .collect(),
        args: &["-8"],
    });
    v.push(CliCase {
        name: "cli_16_blocksize_1000",
        rate: 22_050,
        ch: 1,
        bits: 16,
        vals: smooth(21, 5_500, 1, 16, 0.5),
        args: &["-b", "1000"],
    });
    v.push(CliCase {
        name: "cli_16_blocksize_16384",
        rate: 96_000,
        ch: 2,
        bits: 16,
        vals: smooth(22, 40_000, 2, 16, 0.5),
        args: &["--lax", "-b", "16384"],
    });
    v
}

fn flac_version() -> Result<String> {
    Ok(
        String::from_utf8_lossy(&run(Command::new("flac").arg("--version"))?)
            .trim()
            .to_string(),
    )
}

/// libFLAC 解码为 WAV（支持 20 位等非整字节位深），再按容器位宽与有效位还原样本。
fn libflac_decode(path: &Path, tmp: &Path) -> Result<Vec<i64>> {
    let out = tmp.join("ref.wav");
    run(Command::new("flac")
        .args(["--silent", "--force", "--decode", "-o"])
        .arg(&out)
        .arg(path))?;
    let w = std::fs::read(&out)?;
    let mut i = 12;
    let (mut container, mut valid) = (0u32, 0u32);
    while i + 8 <= w.len() {
        let id = &w[i..i + 4];
        let size = u32::from_le_bytes([w[i + 4], w[i + 5], w[i + 6], w[i + 7]]) as usize;
        let body = &w[i + 8..(i + 8 + size).min(w.len())];
        if id == b"fmt " {
            container = u32::from(u16::from_le_bytes([body[14], body[15]]));
            valid = container;
            if u16::from_le_bytes([body[0], body[1]]) == 0xFFFE {
                valid = u32::from(u16::from_le_bytes([body[18], body[19]]));
            }
        } else if id == b"data" {
            let bps = (container / 8) as usize;
            return Ok(body
                .chunks_exact(bps)
                .map(|c| {
                    let mut v: i64 = 0;
                    for (k, &b) in c.iter().enumerate() {
                        v |= i64::from(b) << (8 * k);
                    }
                    let sh = 64 - container;
                    ((v << sh) >> sh) >> (container - valid)
                })
                .collect());
        }
        i += 8 + size + (size & 1);
    }
    Err("libFLAC 输出的 WAV 缺少 data chunk".into())
}

fn analyze(path: &Path, tmp: &Path) -> Result<String> {
    let ana = tmp.join("x.ana");
    run(Command::new("flac")
        .args(["--silent", "--force", "--analyze", "-o"])
        .arg(&ana)
        .arg(path))?;
    Ok(std::fs::read_to_string(&ana)?)
}

// ---------------------------------------------------------------- 主流程

fn meta_text(f: &Fixture) -> String {
    let mut s = format!("result {}\ndetail {}\n", f.result, f.detail);
    match f.format {
        Some((r, c, b)) => {
            let _ = writeln!(s, "format {r} {c} {b}");
        }
        None => s.push_str("format none\n"),
    }
    let ch = f.format.map_or(1, |x| x.1 as usize);
    let _ = writeln!(s, "frames {}", f.samples.len() / ch.max(1));
    let _ = writeln!(s, "origin {}", f.origin);
    s
}

pub fn generate(out: &Path) -> Result<()> {
    std::fs::create_dir_all(out)?;
    let tmp = std::env::temp_dir().join(format!("qy-gen-flac-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let version = flac_version()?;
    reference::check_version(&version)?;
    let mut manifest = format!(
        "# qy-codec-flac 夹具清单（由 cargo run -p qy-gen-fixtures -- flac 生成）\n参考工具：{version}\n\n"
    );
    let mut fixtures: Vec<Fixture> = Vec::new();

    for c in cli_cases() {
        let w = tmp.join(format!("{}.wav", c.name));
        std::fs::write(&w, wav(&c.vals, c.rate, c.ch, c.bits))?;
        let f = tmp.join(format!("{}.flac", c.name));
        let mut cmd = Command::new("flac");
        cmd.args(["--silent", "--force", "--no-padding"])
            .args(c.args)
            .arg("-o")
            .arg(&f)
            .arg(&w);
        let _ = writeln!(
            manifest,
            "{}: flac --silent --force --no-padding {} -o {}.flac {}.wav",
            c.name,
            c.args.join(" "),
            c.name,
            c.name
        );
        run(&mut cmd)?;
        fixtures.push(Fixture {
            name: c.name.into(),
            data: std::fs::read(&f)?,
            result: "ok",
            detail: "-",
            format: Some((c.rate, u32::from(c.ch), u32::from(c.bits))),
            samples: c.vals,
            libflac_check: true,
            origin: format!("flac 命令行：{}", c.args.join(" ")),
        });
    }
    fixtures.extend(crafted_cases());
    fixtures.extend(error_cases());

    let mut coverage = std::collections::BTreeMap::<String, u32>::new();
    for f in &fixtures {
        let path = out.join(format!("{}.flac", f.name));
        std::fs::write(&path, &f.data)?;
        std::fs::write(out.join(format!("{}.meta", f.name)), meta_text(f))?;
        if !f.samples.is_empty() {
            std::fs::write(out.join(format!("{}.s32le", f.name)), s32le(&f.samples))?;
        }
        let mut check = String::from("未做参考解码（错误或不支持的夹具）");
        if f.libflac_check {
            let (rate, ch, bits) = f.format.ok_or("缺格式")?;
            let (reference, how) = reference::libflac_reference(&path, bits, &tmp)?;
            if reference != f.samples {
                let at = reference.iter().zip(&f.samples).position(|(a, b)| a != b);
                return Err(format!(
                    "libFLAC 解码与源样本不一致: {}（长度 {} vs {}，首个差异 {:?}）",
                    f.name,
                    reference.len(),
                    f.samples.len(),
                    at
                )
                .into());
            }
            // metaflac 读出的实际格式必须与预期一致（不假设编码选项生效）。
            let (m_rate, m_ch, m_bits, m_maxbs, m_total) = reference::metaflac_info(&path)?;
            let frames = (f.samples.len() / ch as usize) as u64;
            if (m_rate, m_ch, m_bits) != (rate, ch, bits) || (m_total != 0 && m_total != frames) {
                return Err(format!(
                    "{}: metaflac 实际格式 {m_rate}/{m_ch}/{m_bits} 总样本 {m_total} 与预期 {rate}/{ch}/{bits} {frames} 不符",
                    f.name
                )
                .into());
            }
            let keys = reference::analyze_keys(&analyze(&path, &tmp)?);
            reference::check_branches(&f.name, &keys)?;
            for k in &keys {
                *coverage.entry(k.clone()).or_default() += 1;
            }
            let blocks: Vec<&str> = keys
                .iter()
                .filter_map(|k| k.strip_prefix("blocksize="))
                .collect();
            check = format!(
                "libFLAC 参考解码（{how}，有符号小端 {bits} 位）逐样本一致；metaflac {m_rate} Hz/{m_ch} 声道/{m_bits} 位/最大块 {m_maxbs}/总样本 {m_total}；块大小 {blocks:?}；分支要求 {:?} 满足",
                reference::expected_branches(&f.name)
            );
        }
        let _ = writeln!(
            manifest,
            "{} -> {} {}\n  sha256 {}\n  {} 字节；来源：{}；种子：{}\n  核对：{}",
            f.name,
            f.result,
            f.detail,
            reference::sha256_hex(&f.data),
            f.data.len(),
            f.origin,
            reference::seeds(&f.name),
            check
        );
    }
    // 块大小键只在覆盖汇总里保留 65535。
    coverage.retain(|k, _| !k.starts_with("blocksize=") || k == "blocksize=65535");
    manifest.push_str("\n# libFLAC --analyze 分支覆盖（含该分支的有效夹具数）\n");
    for (k, n) in &coverage {
        let _ = writeln!(manifest, "{k}: {n}");
    }
    let required = [
        "type=CONSTANT",
        "type=VERBATIM",
        "type=FIXED order=0",
        "type=FIXED order=1",
        "type=FIXED order=2",
        "type=FIXED order=3",
        "type=FIXED order=4",
        "type=LPC",
        "type=LPC order=32",
        "channel_assignment=INDEPENDENT",
        "channel_assignment=LEFT_SIDE",
        "channel_assignment=RIGHT_SIDE",
        "channel_assignment=MID_SIDE",
        "residual_type=RICE",
        "residual_type=RICE2",
        "wasted_bits>0",
        "escape_partition",
        "blocksize=65535",
    ];
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|k| !coverage.contains_key(*k))
        .collect();
    std::fs::write(out.join("MANIFEST.txt"), manifest)?;
    let _ = std::fs::remove_dir_all(&tmp);
    if !missing.is_empty() {
        return Err(format!("libFLAC 分析未覆盖分支: {missing:?}").into());
    }
    println!("{} FLAC 夹具 -> {}", fixtures.len(), out.display());
    Ok(())
}
