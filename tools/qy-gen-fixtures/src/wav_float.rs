//! F3：浮点 WAV（格式码 3 与 EXTENSIBLE `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT`）的独立参考夹具。
//!
//! 期望值是生成器写入的位模式本身（`.expect`，十六进制），不经被测解码器得出。第二参考：
//! - ffmpeg 按 `f32le` / `f64le` 原样输出的字节（`.ffmpeg.raw`），必须与写入的位模式逐位一致；
//! - 只含 [−1, 1) 内有限值的夹具，再用 ffmpeg 转为 16 位与 32 位整数（`.ffmpeg16.s16le`、
//!   `.ffmpeg32.s32le`）。ffmpeg（libswresample）的约定是 `clip(lrint(x × 2^(V−1)))`，与 F3 §4 相同；
//!   生成器先用 `f64` 与 `i128` 按定义独立计算并断言 ffmpeg 的结果一致，再写出文件。不比较的部分：
//!   24 位（ffmpeg 经 32 位再截断，舍入约定不同）；|x| ≥ 1 的限幅（ffmpeg 的向量化转换在溢出时
//!   得到最小值，例如 2^20 转 16 位得 −32768）。限幅由 `qy-pcm-format` 的转换单元测试按独立参考
//!   覆盖。
//!
//! 输出目录：`crates/qy-codec-wav/tests/fixtures/float/`。

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use super::{Ext, Lcg, Result, SUBTYPE_FLOAT, chunk, ext, fmt_ext, riff, riff_with};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Enc {
    F32,
    F64,
}

impl Enc {
    fn bits(self) -> u16 {
        match self {
            Enc::F32 => 32,
            Enc::F64 => 64,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Enc::F32 => "f32",
            Enc::F64 => "f64",
        }
    }
}

struct Case {
    name: &'static str,
    data: Vec<u8>,
    result: &'static str,
    /// (采样率, 声道, 编码)
    fmt: Option<(u32, u16, Enc)>,
    mask: Option<u32>,
    detail: String,
    /// 期望的样本位模式（binary32 存低 32 位）。
    bits: Vec<u64>,
    ch: usize,
    /// ffmpeg 原样输出的比较。
    ffmpeg_raw: bool,
    /// ffmpeg 16 / 32 位转换参考。
    ffmpeg_convert: bool,
}

/// 格式码 3 的 fmt（18 字节，cbSize = 0）。
fn fmt_tag3(ch: u16, rate: u32, bits: u16) -> Vec<u8> {
    let align = ch * (bits / 8);
    let mut body = Vec::new();
    body.extend_from_slice(&3u16.to_le_bytes());
    body.extend_from_slice(&ch.to_le_bytes());
    body.extend_from_slice(&rate.to_le_bytes());
    body.extend_from_slice(&(rate * u32::from(align)).to_le_bytes());
    body.extend_from_slice(&align.to_le_bytes());
    body.extend_from_slice(&bits.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    chunk(b"fmt ", &body)
}

fn fmt_float_ext(ch: u16, rate: u32, enc: Enc, mask: u32) -> Vec<u8> {
    fmt_ext(Ext {
        sub: SUBTYPE_FLOAT,
        ..ext(ch, rate, enc.bits(), enc.bits(), mask)
    })
}

fn fact(frames: u32) -> Vec<u8> {
    chunk(b"fact", &frames.to_le_bytes())
}

fn encode(bits: &[u64], enc: Enc) -> Vec<u8> {
    bits.iter()
        .flat_map(|&b| match enc {
            Enc::F32 => (b as u32).to_le_bytes().to_vec(),
            Enc::F64 => b.to_le_bytes().to_vec(),
        })
        .collect()
}

/// 有限样本：特殊值、16 位半个 LSB 边界、由 16/24 位整数生成的值、[−1, 1) 内的随机值
/// （ffmpeg 的转换在 |x| ≥ 1 时与定义不同，见模块说明）。
fn finite(seed: u64, n: usize, enc: Enc) -> Vec<u64> {
    let mut r = Lcg(seed);
    let fixed: [f64; 12] = [
        0.0,
        -0.0,
        32_767.0 / 32_768.0,
        -1.0,
        0.5,
        -0.5,
        (100.5) / 32_768.0,
        (101.5) / 32_768.0,
        -(7.5) / 32_768.0,
        1.0 / 8_388_608.0,
        -1.0 / 8_388_608.0,
        1.0 / (1u64 << 40) as f64,
    ];
    (0..n)
        .map(|i| {
            let x = match fixed.get(i) {
                Some(&f) => f,
                None => match i % 3 {
                    // 由整数生成：s / 2^15 或 s / 2^23。
                    0 => ((r.next() % 65_536) as i64 - 32_768) as f64 / 32_768.0,
                    1 => ((r.next() % 16_777_216) as i64 - 8_388_608) as f64 / 8_388_608.0,
                    _ => (r.next() as f64 / (1u64 << 53) as f64) * 2.0 - 1.0,
                },
            };
            match enc {
                Enc::F32 => u64::from((x as f32).to_bits()),
                Enc::F64 => x.to_bits(),
            }
        })
        .collect()
}

/// 32 位特殊位模式（含次正规数、最大有限值、NaN、无穷，以及 1.0 之后的下一个值）。
const F32_SPECIAL: [u32; 14] = [
    0x0000_0000,
    0x8000_0000,
    0x0000_0001,
    0x807F_FFFF,
    0x0080_0000,
    0x7F7F_FFFF,
    0xFF7F_FFFF,
    0x3F80_0001,
    0xBF80_0001,
    0x7F80_0000,
    0xFF80_0000,
    0x7FC0_0000,
    0x7F80_0001,
    0xFFC1_2345,
];

const F64_SPECIAL: [u64; 10] = [
    0x0000_0000_0000_0001,
    0x8000_0000_0000_0000,
    0x7FEF_FFFF_FFFF_FFFF,
    0x0010_0000_0000_0000,
    0x3FF0_0000_0000_0001,
    0x7FF0_0000_0000_0000,
    0xFFF0_0000_0000_0000,
    0x7FF8_0000_0000_0000,
    0x7FF0_0000_0000_0001,
    0x3FE0_0000_0000_0000,
];

fn ok(
    cs: &mut Vec<Case>,
    name: &'static str,
    chunks: Vec<Vec<u8>>,
    fmt: (u32, u16, Enc),
    mask: Option<u32>,
    bits: Vec<u64>,
    convert: bool,
) {
    ok_raw(cs, name, riff(&chunks), fmt, mask, bits, convert);
}

/// 同 [`ok`]，文件字节由调用方给出（例如长度为占位值）。
fn ok_raw(
    cs: &mut Vec<Case>,
    name: &'static str,
    data: Vec<u8>,
    fmt: (u32, u16, Enc),
    mask: Option<u32>,
    bits: Vec<u64>,
    convert: bool,
) {
    cs.push(Case {
        name,
        data,
        result: "ok",
        fmt: Some(fmt),
        mask,
        detail: "none".into(),
        ch: usize::from(fmt.1),
        bits,
        ffmpeg_raw: true,
        ffmpeg_convert: convert,
    });
}

fn bad(cs: &mut Vec<Case>, name: &'static str, data: Vec<u8>, result: &'static str, detail: &str) {
    cs.push(Case {
        name,
        data,
        result,
        fmt: None,
        mask: None,
        detail: detail.into(),
        bits: Vec::new(),
        ch: 1,
        ffmpeg_raw: false,
        ffmpeg_convert: false,
    });
}

fn build() -> Vec<Case> {
    let mut cs = Vec::new();
    let c = &mut cs;
    // ---- 有效：格式码 3 与 EXTENSIBLE，32 / 64 位，单 / 双 / 5.1 声道 ----
    let b = finite(1, 300 * 2, Enc::F32);
    ok(
        c,
        "f32_stereo_44k_tag3",
        vec![
            fmt_tag3(2, 44_100, 32),
            fact(300),
            chunk(b"data", &encode(&b, Enc::F32)),
        ],
        (44_100, 2, Enc::F32),
        None,
        b,
        true,
    );
    let b = finite(2, 257, Enc::F32);
    ok(
        c,
        "f32_mono_48k_ext",
        vec![
            fmt_float_ext(1, 48_000, Enc::F32, 0x4),
            fact(257),
            chunk(b"data", &encode(&b, Enc::F32)),
        ],
        (48_000, 1, Enc::F32),
        Some(0x4),
        b,
        true,
    );
    let b = finite(3, 200 * 2, Enc::F64);
    ok(
        c,
        "f64_stereo_96k_tag3",
        vec![
            fmt_tag3(2, 96_000, 64),
            fact(200),
            chunk(b"data", &encode(&b, Enc::F64)),
        ],
        (96_000, 2, Enc::F64),
        None,
        b,
        true,
    );
    let b = finite(4, 64 * 6, Enc::F64);
    ok(
        c,
        "f64_51_48k_ext",
        vec![
            fmt_float_ext(6, 48_000, Enc::F64, 0x3F),
            fact(64),
            chunk(b"data", &encode(&b, Enc::F64)),
        ],
        (48_000, 6, Enc::F64),
        Some(0x3F),
        b,
        true,
    );
    // fact 在 fmt 之前、data 之前另有 LIST 块（块顺序由 RIFF 规范决定，只要 fact 在 data 之前）。
    let b = finite(5, 101 * 2, Enc::F32);
    let mut list = b"INFO".to_vec();
    list.extend_from_slice(&chunk(b"ISFT", b"qy-gen\0"));
    ok(
        c,
        "f32_fact_first_list",
        vec![
            fact(101),
            fmt_float_ext(2, 44_100, Enc::F32, 0x3),
            chunk(b"LIST", &list),
            chunk(b"data", &encode(&b, Enc::F32)),
        ],
        (44_100, 2, Enc::F32),
        Some(0x3),
        b,
        true,
    );
    // 特殊值（严格路径按位保留，含 NaN 与无穷）：不做转换比较。
    let b: Vec<u64> = F32_SPECIAL.iter().map(|&x| u64::from(x)).collect();
    ok(
        c,
        "f32_specials",
        vec![
            fmt_tag3(1, 44_100, 32),
            fact(b.len() as u32),
            chunk(b"data", &encode(&b, Enc::F32)),
        ],
        (44_100, 1, Enc::F32),
        None,
        b,
        false,
    );
    let b = F64_SPECIAL.to_vec();
    ok(
        c,
        "f64_specials",
        vec![
            fmt_tag3(2, 44_100, 64),
            fact(5),
            chunk(b"data", &encode(&b, Enc::F64)),
        ],
        (44_100, 2, Enc::F64),
        None,
        b,
        false,
    );

    // ---- fact 可有可无（用户 2026-10-16 决定：RIFF 1991 原文只要求压缩格式有 fact）----
    let fb = finite(6, 20, Enc::F32);
    let d = encode(&fb, Enc::F32);
    ok(
        c,
        "f32_no_fact",
        vec![fmt_tag3(2, 44_100, 32), chunk(b"data", &d)],
        (44_100, 2, Enc::F32),
        None,
        fb.clone(),
        false,
    );
    // data 之后的块不读取（同 PCM）。
    ok(
        c,
        "f32_fact_after_data",
        vec![fmt_tag3(2, 44_100, 32), chunk(b"data", &d), fact(10)],
        (44_100, 2, Enc::F32),
        None,
        fb.clone(),
        false,
    );
    // ffmpeg 输出到管道时的样子：RIFF 与 data 长度为占位值、没有 fact。data 按 W1 R2 延伸到源末尾。
    let pipe = |chunks: &[Vec<u8>]| {
        let mut b = riff(chunks);
        b[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        let at = b.len() - d.len() - 4;
        b[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        b
    };
    ok_raw(
        c,
        "f32_pipe_placeholder",
        pipe(&[fmt_float_ext(2, 44_100, Enc::F32, 0x3), chunk(b"data", &d)]),
        (44_100, 2, Enc::F32),
        Some(0x3),
        fb.clone(),
        false,
    );

    // ---- 不合规：fact 与 data 矛盾或无法核对 ----
    bad(
        c,
        "f32_fact_mismatch",
        riff(&[fmt_tag3(2, 44_100, 32), fact(11), chunk(b"data", &d)]),
        "Malformed",
        "fact:11:10:0",
    );
    bad(
        c,
        "f32_fact_duplicate",
        riff(&[
            fmt_tag3(2, 44_100, 32),
            fact(10),
            fact(10),
            chunk(b"data", &d),
        ]),
        "Malformed",
        "fact:10:10:1",
    );
    // 有 fact 而 data 长度为占位值：声明的帧数无法核对。
    bad(
        c,
        "f32_fact_placeholder",
        pipe(&[
            fmt_float_ext(2, 44_100, Enc::F32, 0x3),
            fact(10),
            chunk(b"data", &d),
        ]),
        "Malformed",
        "fact:10:none:0",
    );
    // EXTENSIBLE 浮点的有效位不等于容器。
    bad(
        c,
        "f32_ext_valid24",
        riff(&[
            fmt_ext(Ext {
                sub: SUBTYPE_FLOAT,
                ..ext(2, 44_100, 32, 24, 3)
            }),
            fact(10),
            chunk(b"data", &d),
        ]),
        "Malformed",
        "none",
    );

    // ---- 合法但不支持：16 / 24 位浮点 ----
    let d16 = vec![0u8; 8];
    bad(
        c,
        "f16_tag3",
        riff(&[fmt_tag3(2, 44_100, 16), fact(2), chunk(b"data", &d16)]),
        "Unsupported",
        "none",
    );
    bad(
        c,
        "f24_ext",
        riff(&[
            fmt_ext(Ext {
                sub: SUBTYPE_FLOAT,
                ..ext(2, 44_100, 24, 24, 3)
            }),
            fact(2),
            chunk(b"data", &[0u8; 12]),
        ]),
        "Unsupported",
        "none",
    );
    // ---- 截断：data 声明的帧比源多（W1 R5，已输出完整帧） ----
    let b = finite(7, 8 * 2, Enc::F32);
    let mut t = riff_with(
        &[
            fmt_tag3(2, 44_100, 32),
            fact(8),
            chunk(b"data", &encode(&b, Enc::F32)),
        ],
        0,
        b"RIFF",
        b"WAVE",
    );
    t.truncate(t.len() - 13);
    cs.push(Case {
        name: "f32_truncated",
        data: t,
        result: "Truncated",
        fmt: Some((44_100, 2, Enc::F32)),
        mask: None,
        detail: "data-short:64:13".into(),
        ch: 2,
        bits: b[..12].to_vec(),
        ffmpeg_raw: false,
        ffmpeg_convert: false,
    });
    let c = &mut cs;
    // ---- F4：RF64 浮点。ffmpeg `-rf64 always` 的结构：fact 为 0xFFFFFFFF，帧数见 ds64.sampleCount ----
    let rf = |body: &[u8], samples: u64, fact_value: Option<u32>, fmt: Vec<u8>| {
        let mut chunks = vec![fmt];
        if let Some(v) = fact_value {
            chunks.push(chunk(b"fact", &v.to_le_bytes()));
        }
        chunks.push(super::wav_rf64::data(u32::MAX, body));
        super::wav_rf64::rf64(
            b"RF64",
            super::wav_rf64::ds64(super::wav_rf64::AUTO, body.len() as u64, samples, &[], &[]),
            &chunks,
        )
    };
    let b = finite(31, 120 * 2, Enc::F32);
    let body = encode(&b, Enc::F32);
    ok_raw(
        c,
        "rf64_f32_fact_ds64",
        rf(
            &body,
            120,
            Some(u32::MAX),
            fmt_float_ext(2, 44_100, Enc::F32, 3),
        ),
        (44_100, 2, Enc::F32),
        Some(3),
        b,
        false,
    );
    bad(
        c,
        "rf64_f32_fact_mismatch",
        rf(
            &body,
            121,
            Some(u32::MAX),
            fmt_float_ext(2, 44_100, Enc::F32, 3),
        ),
        "Malformed",
        "fact:121:120:0",
    );
    let b = finite(32, 77, Enc::F64);
    ok_raw(
        c,
        "rf64_f64_mono_no_fact",
        rf(&encode(&b, Enc::F64), 0, None, fmt_tag3(1, 96_000, 64)),
        (96_000, 1, Enc::F64),
        None,
        b,
        false,
    );
    cs
}

fn expect_text(c: &Case) -> String {
    let mut s = format!("result {}\n", c.result);
    match c.fmt {
        Some((rate, ch, enc)) => {
            let _ = writeln!(s, "format {rate} {ch} {}", enc.name());
        }
        None => s.push_str("format none\n"),
    }
    match c.mask {
        Some(m) => {
            let _ = writeln!(s, "mask {m}");
        }
        None => s.push_str("mask none\n"),
    }
    let _ = writeln!(s, "detail {}", c.detail);
    let _ = writeln!(s, "samples {}", c.bits.len());
    let enc = c.fmt.map_or(Enc::F32, |f| f.2);
    for frame in c.bits.chunks(c.ch.max(1)) {
        let line: Vec<String> = frame
            .iter()
            .map(|&b| match enc {
                Enc::F32 => format!("{:08x}", b as u32),
                Enc::F64 => format!("{b:016x}"),
            })
            .collect();
        s.push_str(&line.join(" "));
        s.push('\n');
    }
    s
}

fn ffmpeg(path: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(args)
        .arg("-")
        .output()?;
    if !out.status.success() {
        return Err(format!("ffmpeg 失败: {}", String::from_utf8_lossy(&out.stderr)).into());
    }
    Ok(out.stdout)
}

/// F3 §4 的定义，`f64` 与 `i128` 独立计算（x 已是有限值，x × 2^(V−1) 在 f64 中精确）。
fn reference(x: f64, v: u32) -> i64 {
    let y = (x * 2f64.powi(v as i32 - 1)).round_ties_even();
    let max = (1i64 << (v - 1)) - 1;
    let min = -(1i64 << (v - 1));
    (y as i128).clamp(i128::from(min), i128::from(max)) as i64
}

pub fn generate(out: &Path) -> Result<()> {
    std::fs::create_dir_all(out)?;
    let cases = build();
    for c in &cases {
        let wav = out.join(format!("{}.wav", c.name));
        std::fs::write(&wav, &c.data)?;
        std::fs::write(out.join(format!("{}.expect", c.name)), expect_text(c))?;
        let Some((_, _, enc)) = c.fmt else { continue };
        if c.ffmpeg_raw {
            let fmt = match enc {
                Enc::F32 => ["-f", "f32le", "-c:a", "pcm_f32le"],
                Enc::F64 => ["-f", "f64le", "-c:a", "pcm_f64le"],
            };
            let raw = ffmpeg(&wav, &fmt)?;
            if raw != encode(&c.bits, enc) {
                return Err(format!("ffmpeg 原样输出与写入的位模式不一致: {}", c.name).into());
            }
            std::fs::write(out.join(format!("{}.ffmpeg.raw", c.name)), raw)?;
        }
        if c.ffmpeg_convert {
            let xs: Vec<f64> = c
                .bits
                .iter()
                .map(|&b| match enc {
                    Enc::F32 => f64::from(f32::from_bits(b as u32)),
                    Enc::F64 => f64::from_bits(b),
                })
                .collect();
            let s16 = ffmpeg(&wav, &["-f", "s16le", "-c:a", "pcm_s16le"])?;
            let want16: Vec<u8> = xs
                .iter()
                .flat_map(|&x| (reference(x, 16) as i16).to_le_bytes())
                .collect();
            if s16 != want16 {
                return Err(format!("ffmpeg 16 位转换与定义不一致: {}", c.name).into());
            }
            std::fs::write(out.join(format!("{}.ffmpeg16.s16le", c.name)), s16)?;
            let s32 = ffmpeg(&wav, &["-f", "s32le", "-c:a", "pcm_s32le"])?;
            let want32: Vec<u8> = xs
                .iter()
                .flat_map(|&x| (reference(x, 32) as i32).to_le_bytes())
                .collect();
            if s32 != want32 {
                return Err(format!("ffmpeg 32 位转换与定义不一致: {}", c.name).into());
            }
            std::fs::write(out.join(format!("{}.ffmpeg32.s32le", c.name)), s32)?;
        }
    }
    println!("{} float cases -> {}", cases.len(), out.display());
    Ok(())
}
