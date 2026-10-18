//! AIFF / AIFF-C 夹具（F2，tasks/F2.md）。
//!
//! 不依赖任何 qy- crate：从已知整数样本直接构造 AIFF 字节（大端、左对齐、补码，AIFF 1.3
//! §Sample Points），期望值写入 `.expect`；ffmpeg 能读的有效夹具另解码为 s32le 写入
//! `.ffmpeg.s32le`，并断言其等于 `样本 << (32 − N)`，作为第二个独立参考。
//!
//! `.expect` 格式：
//! ```text
//! result ok|malformed|unsupported|truncated
//! format <采样率> <声道> <位数> | format none
//! detail <明细> | detail none
//! samples <样本数>
//! <每帧一行>
//! ```

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Lcg, Result, samples};

/// AIFF-C AIFCVersion1。
const AIFC_V1: u32 = 0xA280_5140;

/// 正整数采样率的 80 位扩展精度（规范化：尾数最高位为整数位）。
fn extended(rate: u64) -> [u8; 10] {
    assert!(rate > 0);
    let lz = rate.leading_zeros();
    let mantissa = rate << lz;
    let exponent = 16383 + 63 - lz as u16;
    let mut b = [0u8; 10];
    b[..2].copy_from_slice(&exponent.to_be_bytes());
    b[2..].copy_from_slice(&mantissa.to_be_bytes());
    b
}

/// 样本按 N 位左对齐写入 ceil(N/8) 字节（大端）；`dirty` 时低位填充随机非零位。
fn encode(vals: &[i64], n: u32, dirty: Option<u64>) -> Vec<u8> {
    let bytes = n.div_ceil(8);
    let pad = 8 * bytes - n;
    let mut r = dirty.map(Lcg);
    let mut out = Vec::with_capacity(vals.len() * bytes as usize);
    for &s in vals {
        let mut w = ((s as u64) << pad) & ((1u64 << (8 * bytes)) - 1);
        if let Some(r) = r.as_mut()
            && pad > 0
        {
            w |= r.next() & ((1 << pad) - 1);
        }
        for k in (0..bytes).rev() {
            out.push((w >> (8 * k)) as u8);
        }
    }
    out
}

/// chunk：ID + 大端长度 + 数据 + 奇数长度时的 0 填充。
fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut c = id.to_vec();
    c.extend_from_slice(&(body.len() as u32).to_be_bytes());
    c.extend_from_slice(body);
    if body.len() % 2 == 1 {
        c.push(0);
    }
    c
}

/// FORM：长度 = 4 + 各 chunk 字节数 + `adjust`。
fn form(kind: &[u8; 4], chunks: &[Vec<u8>], adjust: i64) -> Vec<u8> {
    let body: Vec<u8> = chunks.concat();
    let size = (4 + body.len() as i64 + adjust) as u32;
    let mut f = b"FORM".to_vec();
    f.extend_from_slice(&size.to_be_bytes());
    f.extend_from_slice(kind);
    f.extend_from_slice(&body);
    f
}

fn comm_fields(ch: i16, frames: u32, bits: i16, rate: [u8; 10]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&ch.to_be_bytes());
    b.extend_from_slice(&frames.to_be_bytes());
    b.extend_from_slice(&bits.to_be_bytes());
    b.extend_from_slice(&rate);
    b
}

/// AIFF 的 COMM（18 字节）。
fn comm(ch: i16, frames: u32, bits: i16, rate: [u8; 10]) -> Vec<u8> {
    chunk(b"COMM", &comm_fields(ch, frames, bits, rate))
}

/// pstring：计数 + 文本，总长为偶数。
fn pstring(s: &str) -> Vec<u8> {
    let mut p = vec![s.len() as u8];
    p.extend_from_slice(s.as_bytes());
    if p.len() % 2 == 1 {
        p.push(0);
    }
    p
}

/// AIFF-C 的 COMM（22 + pstring）。
fn comm_c(ch: i16, frames: u32, bits: i16, rate: [u8; 10], tag: &[u8; 4], name: &str) -> Vec<u8> {
    let mut b = comm_fields(ch, frames, bits, rate);
    b.extend_from_slice(tag);
    b.extend_from_slice(&pstring(name));
    chunk(b"COMM", &b)
}

fn fver(ts: u32) -> Vec<u8> {
    chunk(b"FVER", &ts.to_be_bytes())
}

/// SSND：offset、blockSize、offset 字节的前置填充、样本数据、`extra` 字节的尾部填充。
fn ssnd(offset: u32, data: &[u8], extra: usize) -> Vec<u8> {
    let mut b = offset.to_be_bytes().to_vec();
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend(std::iter::repeat_n(0xA5, offset as usize));
    b.extend_from_slice(data);
    b.extend(std::iter::repeat_n(0x5A, extra));
    chunk(b"SSND", &b)
}

struct Case {
    name: &'static str,
    data: Vec<u8>,
    result: &'static str,
    fmt: Option<(u32, u16, u32)>,
    detail: Option<String>,
    vals: Vec<i64>,
    ffmpeg: bool,
    /// 浮点（F5）的格式后缀（` f32` / ` f64`）；整数为空。浮点的 `vals` 是解码器输出的 `i32` 字
    /// （binary64 每样本两个字，先低后高）。
    enc: &'static str,
    /// 浮点：与 ffmpeg 按 f32le / f64le 原样输出的字节逐位比较。
    raw: bool,
}

/// 有效的 AIFF：`ch` 声道、`n` 位、`frames` 帧。
fn ok(name: &'static str, rate: u32, ch: u16, n: u32, frames: usize, seed: u64) -> Case {
    let vals = if n == 1 {
        // 1 位补码只有 −1 与 0（通用生成器的固定边界值含 1，超出范围）。
        let mut r = Lcg(seed);
        (0..frames * ch as usize)
            .map(|_| -((r.next() & 1) as i64))
            .collect()
    } else {
        samples(seed, frames, ch as usize, n)
    };
    let data = form(
        b"AIFF",
        &[
            comm(ch as i16, frames as u32, n as i16, extended(rate.into())),
            ssnd(0, &encode(&vals, n, None), 0),
        ],
        0,
    );
    Case {
        name,
        data,
        result: "ok",
        fmt: Some((rate, ch, n)),
        detail: None,
        vals,
        ffmpeg: true,
        enc: "",
        raw: false,
    }
}

fn err(name: &'static str, data: Vec<u8>, result: &'static str, detail: &str) -> Case {
    Case {
        name,
        data,
        result,
        fmt: None,
        detail: Some(detail.to_string()),
        vals: Vec::new(),
        ffmpeg: false,
        enc: "",
        raw: false,
    }
}

/// 在 COMM 之后才发现的错误：格式已知，出错前已输出的帧照常输出。
fn err_after(
    name: &'static str,
    data: Vec<u8>,
    result: &'static str,
    detail: &str,
    vals: Vec<i64>,
) -> Case {
    Case {
        fmt: Some((44_100, 2, 16)),
        vals,
        ..err(name, data, result, detail)
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

fn cases() -> Vec<Case> {
    let mut cs = vec![
        ok("aiff_16_stereo_44k", 44_100, 2, 16, 600, 1),
        ok("aiff_8_mono_8k", 8_000, 1, 8, 301, 2),
        ok("aiff_1_mono_44k", 44_100, 1, 1, 64, 3),
        ok("aiff_12_stereo_48k", 48_000, 2, 12, 300, 4),
        ok("aiff_20_mono_96k", 96_000, 1, 20, 257, 5),
        ok("aiff_24_stereo_96k", 96_000, 2, 24, 300, 6),
        ok("aiff_32_stereo_192k", 192_000, 2, 32, 300, 7),
        ok("aiff_16_3ch_48k", 48_000, 3, 16, 200, 8),
        ok("aiff_16_stereo_11025", 11_025, 2, 16, 100, 9),
        ok("aiff_24_mono_2822400", 2_822_400, 1, 24, 100, 10),
    ];

    // AIFF-C NONE：compressionName 长度为偶数（需要填充）与奇数（不需要）各一个。
    for (name, cname, seed) in [
        ("aifc_none_16_stereo", "not compressed", 11u64),
        ("aifc_none_24_mono_oddname", "uncompressed!", 12),
    ] {
        let (ch, n, frames) = if seed == 11 {
            (2u16, 16u32, 300usize)
        } else {
            (1, 24, 255)
        };
        let vals = samples(seed, frames, ch as usize, n);
        let data = form(
            b"AIFC",
            &[
                fver(AIFC_V1),
                comm_c(
                    ch as i16,
                    frames as u32,
                    n as i16,
                    extended(48_000),
                    b"NONE",
                    cname,
                ),
                ssnd(0, &encode(&vals, n, None), 0),
            ],
            0,
        );
        cs.push(Case {
            name,
            data,
            result: "ok",
            fmt: Some((48_000, ch, n)),
            detail: None,
            vals,
            ffmpeg: true,
            enc: "",
            raw: false,
        });
    }

    // 可选与未知 chunk、奇数长度文本 chunk 的填充、SSND 之后的 chunk、offset ≠ 0。
    {
        let vals = samples(13, 200, 2, 16);
        let data = form(
            b"AIFF",
            &[
                chunk(b"NAME", b"QyBit"),
                chunk(b"ANNO", b"odd"),
                chunk(b"zzzz", &[1, 2, 3, 4, 5, 6, 7]),
                comm(2, 200, 16, extended(44_100)),
                chunk(b"ID3 ", &[0x49, 0x44, 0x33, 3, 0, 0, 0, 0, 0, 1, 0]),
                ssnd(6, &encode(&vals, 16, None), 0),
                chunk(b"COMT", &[0, 0]),
                chunk(b"(c) ", b"2026"),
            ],
            0,
        );
        cs.push(Case {
            name: "aiff_chunks_offset",
            data,
            result: "ok",
            fmt: Some((44_100, 2, 16)),
            detail: None,
            vals,
            ffmpeg: true,
            enc: "",
            raw: false,
        });
    }

    // 帧数以 COMM 为准：SSND 中帧之后的剩余字节不是样本（ffmpeg 会读入，不作第二参考）。
    {
        let vals = samples(14, 100, 2, 16);
        let data = form(
            b"AIFF",
            &[
                comm(2, 100, 16, extended(44_100)),
                ssnd(0, &encode(&vals, 16, None), 10),
            ],
            0,
        );
        cs.push(Case {
            name: "aiff_ssnd_trailing_bytes",
            data,
            result: "ok",
            fmt: Some((44_100, 2, 16)),
            detail: None,
            vals,
            ffmpeg: false,
            enc: "",
            raw: false,
        });
    }

    // 8 位单声道奇数帧：SSND 长度为奇数，有填充字节。
    let mut odd = ok("aiff_8_mono_odd", 22_050, 1, 8, 99, 15);
    odd.ffmpeg = true;
    cs.push(odd);

    // 低位填充非零：不属于样本值，丢弃（ffmpeg 会保留，不作第二参考）。
    {
        let vals = samples(16, 200, 2, 12);
        let data = form(
            b"AIFF",
            &[
                comm(2, 200, 12, extended(48_000)),
                ssnd(0, &encode(&vals, 12, Some(99)), 0),
            ],
            0,
        );
        cs.push(Case {
            name: "aiff_12_dirty_pad",
            data,
            result: "ok",
            fmt: Some((48_000, 2, 12)),
            detail: None,
            vals,
            ffmpeg: false,
            enc: "",
            raw: false,
        });
    }

    // numSampleFrames = 0：没有 SSND 是合规的；有空 SSND 也合规。
    for (name, with_ssnd) in [("aiff_zero_frames", false), ("aiff_zero_frames_ssnd", true)] {
        let mut chunks = vec![comm(2, 0, 16, extended(44_100))];
        if with_ssnd {
            chunks.push(ssnd(0, &[], 0));
        }
        cs.push(Case {
            name,
            data: form(b"AIFF", &chunks, 0),
            result: "ok",
            fmt: Some((44_100, 2, 16)),
            detail: None,
            vals: Vec::new(),
            ffmpeg: false,
            enc: "",
            raw: false,
        });
    }

    // FORM 之后的字节不属于本文件，忽略。
    {
        let mut c = ok("aiff_trailing_after_form", 44_100, 2, 16, 100, 17);
        c.data.extend_from_slice(b"junk after form");
        c.ffmpeg = false;
        cs.push(c);
    }

    // ---- 不合规 ----
    let rate = extended(44_100);
    let good_ssnd = |frames: usize| ssnd(0, &vec![0u8; frames * 4], 0);
    cs.push(err(
        "err_no_comm",
        form(b"AIFF", &[chunk(b"NAME", b"x!")], 0),
        "malformed",
        "violation:missing:COMM",
    ));
    cs.push(err_after(
        "err_dup_comm",
        form(
            b"AIFF",
            &[comm(2, 1, 16, rate), comm(2, 1, 16, rate), good_ssnd(1)],
            0,
        ),
        "malformed",
        "violation:duplicate:COMM",
        Vec::new(),
    ));
    cs.push(err_after(
        "err_dup_ssnd",
        form(
            b"AIFF",
            &[comm(2, 1, 16, rate), good_ssnd(1), good_ssnd(1)],
            0,
        ),
        "malformed",
        "violation:duplicate:SSND",
        vec![0, 0],
    ));
    cs.push(err_after(
        "err_missing_ssnd",
        form(b"AIFF", &[comm(2, 5, 16, rate)], 0),
        "malformed",
        "violation:missing:SSND",
        Vec::new(),
    ));
    cs.push(err_after(
        "err_aifc_no_fver",
        form(
            b"AIFC",
            &[comm_c(2, 1, 16, rate, b"NONE", "x"), good_ssnd(1)],
            0,
        ),
        "malformed",
        "violation:missing:FVER",
        vec![0, 0],
    ));
    cs.push(err(
        "err_aifc_dup_fver",
        form(
            b"AIFC",
            &[
                fver(AIFC_V1),
                fver(AIFC_V1),
                comm_c(2, 1, 16, rate, b"NONE", "x"),
                good_ssnd(1),
            ],
            0,
        ),
        "malformed",
        "violation:duplicate:FVER",
    ));
    cs.push(err(
        "err_fver_size",
        form(
            b"AIFC",
            &[chunk(b"FVER", &[0xA2, 0x80, 0x51, 0x40, 0, 0, 0, 0])],
            0,
        ),
        "malformed",
        "violation:fver-size:8",
    ));
    {
        let mut body = comm_fields(2, 1, 16, rate);
        body.extend_from_slice(&[0, 0]);
        cs.push(err(
            "err_comm_size_aiff",
            form(b"AIFF", &[chunk(b"COMM", &body), good_ssnd(1)], 0),
            "malformed",
            "violation:comm-size:20:18",
        ));
    }
    {
        // pstring 计数为 3（应占 4 字节，共 26），实际只给 2 字节：长度 24。
        let mut body = comm_fields(2, 1, 16, rate);
        body.extend_from_slice(b"NONE");
        body.extend_from_slice(&[3, b'a']);
        cs.push(err(
            "err_comm_size_aifc",
            form(
                b"AIFC",
                &[fver(AIFC_V1), chunk(b"COMM", &body), good_ssnd(1)],
                0,
            ),
            "malformed",
            "violation:comm-size:24:26",
        ));
    }
    cs.push(err_after(
        "err_chunk_beyond_form",
        form(b"AIFF", &[comm(2, 1, 16, rate), good_ssnd(1)], -6),
        "malformed",
        "violation:chunk-beyond-form:SSND",
        Vec::new(),
    ));
    {
        let mut c = b"zzzz".to_vec();
        c.extend_from_slice(&0x8000_0000u32.to_be_bytes());
        cs.push(err_after(
            "err_chunk_size_negative",
            form(b"AIFF", &[comm(2, 1, 16, rate), c], 0),
            "malformed",
            "violation:chunk-size:zzzz:2147483648",
            Vec::new(),
        ));
    }
    cs.push(err_after(
        "err_stray_bytes",
        form(
            b"AIFF",
            &[comm(2, 1, 16, rate), good_ssnd(1), vec![1, 2, 3, 4]],
            0,
        ),
        "malformed",
        "violation:stray-bytes:4",
        vec![0, 0],
    ));
    cs.push(err(
        "err_channels_zero",
        form(b"AIFF", &[comm(0, 1, 16, rate)], 0),
        "malformed",
        "violation:channels:0",
    ));
    cs.push(err(
        "err_sample_size_33",
        form(b"AIFF", &[comm(2, 1, 33, rate)], 0),
        "malformed",
        "violation:sample-size:33",
    ));
    cs.push(err(
        "err_sample_size_0",
        form(b"AIFF", &[comm(2, 1, 0, rate)], 0),
        "malformed",
        "violation:sample-size:0",
    ));
    // COMM 声明 10 帧（40 字节），SSND 只有 8 + 20 字节。
    cs.push(err_after(
        "err_ssnd_too_short",
        form(b"AIFF", &[comm(2, 10, 16, rate), ssnd(0, &[0u8; 20], 0)], 0),
        "malformed",
        "violation:ssnd-too-short:28:48",
        Vec::new(),
    ));
    {
        let mut f = b"FORM".to_vec();
        f.extend_from_slice(&2u32.to_be_bytes());
        f.extend_from_slice(b"AIFF");
        cs.push(err(
            "err_form_size",
            f,
            "malformed",
            "violation:form-size:2",
        ));
    }

    // ---- 截断 ----
    {
        // 30 帧中只有 12 帧完整，外加 3 字节。
        let vals = samples(18, 30, 2, 16);
        let full = form(
            b"AIFF",
            &[comm(2, 30, 16, rate), ssnd(0, &encode(&vals, 16, None), 0)],
            0,
        );
        let cut = full.len() - (18 * 4 - 3);
        cs.push(Case {
            name: "trunc_data",
            data: full[..cut].to_vec(),
            result: "truncated",
            fmt: Some((44_100, 2, 16)),
            detail: Some(format!("data-short:30:{}", 18 * 4 - 3)),
            vals: vals[..24].to_vec(),
            ffmpeg: false,
            enc: "",
            raw: false,
        });
    }
    {
        // 样本完整，FORM 声明比文件多 100 字节。
        let vals = samples(19, 20, 2, 16);
        let data = form(
            b"AIFF",
            &[comm(2, 20, 16, rate), ssnd(0, &encode(&vals, 16, None), 0)],
            100,
        );
        cs.push(Case {
            name: "trunc_form",
            data,
            result: "truncated",
            fmt: Some((44_100, 2, 16)),
            detail: Some("form-short:100".to_string()),
            vals,
            ffmpeg: false,
            enc: "",
            raw: false,
        });
    }
    cs.push(Case {
        name: "trunc_header",
        data: b"FORM\0\0\0".to_vec(),
        result: "truncated",
        fmt: None,
        detail: None,
        vals: Vec::new(),
        ffmpeg: false,
        enc: "",
        raw: false,
    });

    // ---- 合规但尚不支持 ----
    cs.push(err(
        "uns_ssnd_before_comm",
        form(b"AIFF", &[good_ssnd(1), comm(2, 1, 16, rate)], 0),
        "unsupported",
        "ssnd-before-comm",
    ));
    {
        // 44100.5：88201 的指数减 1。
        let mut half = extended(88_201);
        half[1] -= 1;
        cs.push(err(
            "uns_rate_fraction",
            form(b"AIFF", &[comm(2, 1, 16, half), good_ssnd(1)], 0),
            "unsupported",
            &format!("sample-rate:{}", hex(&half)),
        ));
        let zero = [0u8; 10];
        cs.push(err(
            "uns_rate_zero",
            form(b"AIFF", &[comm(2, 1, 16, zero), good_ssnd(1)], 0),
            "unsupported",
            &format!("sample-rate:{}", hex(&zero)),
        ));
        let big = extended(1 << 32);
        cs.push(err(
            "uns_rate_too_big",
            form(b"AIFF", &[comm(2, 1, 16, big), good_ssnd(1)], 0),
            "unsupported",
            &format!("sample-rate:{}", hex(&big)),
        ));
    }
    cs.push(err(
        "uns_fver_version",
        form(
            b"AIFC",
            &[
                fver(0x1234_5678),
                comm_c(2, 1, 16, rate, b"NONE", "x"),
                good_ssnd(1),
            ],
            0,
        ),
        "unsupported",
        "format-version:12345678",
    ));
    for (name, tag) in [
        ("uns_compressed_mac3", b"MAC3"),
        ("uns_compressed_ace2", b"ACE2"),
    ] {
        cs.push(err(
            name,
            form(
                b"AIFC",
                &[
                    fver(AIFC_V1),
                    comm_c(2, 1, 16, rate, tag, "compressed"),
                    good_ssnd(1),
                ],
                0,
            ),
            "unsupported",
            &format!("compressed:{}", String::from_utf8_lossy(tag)),
        ));
    }
    for (ch, name, kind) in [
        (4i16, "uns_layout_4ch", "ambiguous-layout"),
        (6, "uns_layout_6ch", "ambiguous-layout"),
        (5, "uns_layout_5ch", "undefined-layout"),
        (8, "uns_layout_8ch", "undefined-layout"),
    ] {
        cs.push(err(
            name,
            form(
                b"AIFF",
                &[
                    comm(ch, 1, 16, rate),
                    ssnd(0, &vec![0u8; ch as usize * 2], 0),
                ],
                0,
            ),
            "unsupported",
            &format!("{kind}:{ch}"),
        ));
    }

    // ---- 规范之外的扩展 ----
    // F5 起 `twos`、`sowt`、`fl32`、`fl64` 可以播放（见 `f5_cases`）；其余仍为扩展。
    for (name, tag) in [
        ("ext_raw", b"raw "),
        ("ext_ulaw", b"ulaw"),
        ("ext_in24", b"in24"),
        ("ext_23ni", b"23ni"),
    ] {
        cs.push(err(
            name,
            form(
                b"AIFC",
                &[
                    fver(AIFC_V1),
                    comm_c(2, 1, 16, rate, tag, "ext"),
                    good_ssnd(1),
                ],
                0,
            ),
            "unsupported",
            &format!("non-standard:{}", String::from_utf8_lossy(tag)),
        ));
    }
    f5_cases(&mut cs);
    cs
}

/// 样本按 N 位左对齐写入 ceil(N/8) 字节，小端（`sowt`）：与 [`encode`] 相同，只是每个样本的字节
/// 顺序相反。
fn encode_le(vals: &[i64], n: u32, dirty: Option<u64>) -> Vec<u8> {
    let bytes = n.div_ceil(8) as usize;
    let be = encode(vals, n, dirty);
    be.chunks_exact(bytes)
        .flat_map(|c| c.iter().rev().copied().collect::<Vec<_>>())
        .collect()
}

/// binary32 的特殊值：±0、最小与最大次正规数、最小正规数、1、−1、最大有限值、±∞、静默 NaN、
/// 带载荷的信号 NaN。
const F32_SPECIAL: [u32; 12] = [
    0x0000_0000,
    0x8000_0000,
    0x0000_0001,
    0x007F_FFFF,
    0x0080_0000,
    0x3F80_0000,
    0xBF80_0000,
    0x7F7F_FFFF,
    0x7F80_0000,
    0xFF80_0000,
    0x7FC0_0000,
    0x7F80_1234,
];

const F64_SPECIAL: [u64; 10] = [
    0x0000_0000_0000_0000,
    0x8000_0000_0000_0000,
    0x0000_0000_0000_0001,
    0x0010_0000_0000_0000,
    0x3FF0_0000_0000_0000,
    0xBFE0_0000_0000_0000,
    0x7FEF_FFFF_FFFF_FFFF,
    0x7FF0_0000_0000_0000,
    0xFFF0_0000_0000_0000,
    0x7FF8_0000_0000_0001,
];

/// F5：`twos`、`sowt`、`fl32` / `FL32`、`fl64` / `FL64`（tasks/F5.md §2）。
fn f5_cases(cs: &mut Vec<Case>) {
    let aifc =
        |tag: &[u8; 4], ch: u16, frames: usize, bits: i16, rate: u32, data: &[u8], off: u32| {
            form(
                b"AIFC",
                &[
                    fver(AIFC_V1),
                    comm_c(
                        ch as i16,
                        frames as u32,
                        bits,
                        extended(rate.into()),
                        tag,
                        "f5",
                    ),
                    ssnd(off, data, 0),
                ],
                0,
            )
        };
    // ---- 整数：twos 与 sowt，1–32 位 ----
    // (名称, 类型码, 位数, 声道, 采样率, 种子, ffmpeg 能读)
    type IntCase = (&'static str, &'static [u8; 4], u32, u16, u32, u64, bool);
    let ints: [IntCase; 13] = [
        ("aifc_twos_8_stereo", b"twos", 8, 2, 44_100, 501, true),
        ("aifc_twos_12_stereo", b"twos", 12, 2, 48_000, 502, true),
        ("aifc_twos_16_stereo", b"twos", 16, 2, 44_100, 503, true),
        ("aifc_twos_24_mono", b"twos", 24, 1, 96_000, 504, true),
        ("aifc_twos_32_stereo", b"twos", 32, 2, 192_000, 505, true),
        ("aifc_twos_1_mono", b"twos", 1, 1, 44_100, 506, true),
        ("aifc_sowt_16_stereo", b"sowt", 16, 2, 44_100, 511, true),
        ("aifc_sowt_12_stereo", b"sowt", 12, 2, 48_000, 512, true),
        ("aifc_sowt_8_mono", b"sowt", 8, 1, 22_050, 513, false),
        ("aifc_sowt_20_3ch", b"sowt", 20, 3, 48_000, 514, false),
        ("aifc_sowt_24_stereo", b"sowt", 24, 2, 96_000, 515, false),
        ("aifc_sowt_32_mono", b"sowt", 32, 1, 192_000, 516, false),
        ("aifc_sowt_16_3ch", b"sowt", 16, 3, 48_000, 517, true),
    ];
    for (name, tag, n, ch, rate, seed, ffmpeg) in ints {
        let frames = 200;
        let vals = if n == 1 {
            let mut r = Lcg(seed);
            (0..frames * ch as usize)
                .map(|_| -((r.next() & 1) as i64))
                .collect()
        } else {
            samples(seed, frames, ch as usize, n)
        };
        let bytes = if tag == b"sowt" {
            encode_le(&vals, n, None)
        } else {
            encode(&vals, n, None)
        };
        cs.push(Case {
            name,
            data: aifc(tag, ch, frames, n as i16, rate, &bytes, 0),
            result: "ok",
            fmt: Some((rate, ch, n)),
            detail: None,
            vals,
            // ffmpeg 只把 sowt 当作 16 位小端；16 位以外的 sowt 只用本生成器的独立参考。
            ffmpeg,
            enc: "",
            raw: false,
        });
    }
    // 低位填充非零（不属于样本值，丢弃）；SSND offset ≠ 0。
    for (name, tag, n, seed) in [
        ("aifc_sowt_20_dirty_pad_offset", b"sowt", 20u32, 521u64),
        ("aifc_twos_12_dirty_pad_offset", b"twos", 12, 522),
    ] {
        let vals = samples(seed, 150, 2, n);
        let bytes = if tag == b"sowt" {
            encode_le(&vals, n, Some(seed))
        } else {
            encode(&vals, n, Some(seed))
        };
        cs.push(Case {
            name,
            data: aifc(tag, 2, 150, n as i16, 44_100, &bytes, 6),
            result: "ok",
            fmt: Some((44_100, 2, n)),
            detail: None,
            vals,
            ffmpeg: false,
            enc: "",
            raw: false,
        });
    }
    // ---- 浮点：特殊值 + 随机位模式（含 NaN 与 ∞，按位原样） ----
    let mut r = Lcg(531);
    let mut f32s: Vec<u32> = F32_SPECIAL.to_vec();
    f32s.extend((0..2 * 120 - F32_SPECIAL.len()).map(|_| r.next() as u32));
    let be32: Vec<u8> = f32s.iter().flat_map(|b| b.to_be_bytes()).collect();
    let words32: Vec<i64> = f32s.iter().map(|&b| i64::from(b as i32)).collect();
    for (name, tag) in [("aifc_fl32_stereo", b"fl32"), ("aifc_FL32_stereo", b"FL32")] {
        cs.push(Case {
            name,
            data: aifc(tag, 2, 120, 32, 44_100, &be32, 0),
            result: "ok",
            fmt: Some((44_100, 2, 32)),
            detail: None,
            vals: words32.clone(),
            ffmpeg: false,
            enc: " f32",
            // ffmpeg 只认小写 fl32；FL32 与它字节相同，只用独立参考。
            raw: tag == b"fl32",
        });
    }
    let mut f64s: Vec<u64> = F64_SPECIAL.to_vec();
    f64s.extend((0..77 - F64_SPECIAL.len()).map(|_| (r.next() << 11) ^ r.next()));
    let be64: Vec<u8> = f64s.iter().flat_map(|b| b.to_be_bytes()).collect();
    let words64: Vec<i64> = f64s
        .iter()
        .flat_map(|&b| {
            [
                i64::from(b as u32 as i32),
                i64::from((b >> 32) as u32 as i32),
            ]
        })
        .collect();
    for (name, tag) in [("aifc_fl64_mono", b"fl64"), ("aifc_FL64_mono", b"FL64")] {
        cs.push(Case {
            name,
            data: aifc(tag, 1, 77, 64, 96_000, &be64, 4),
            result: "ok",
            fmt: Some((96_000, 1, 64)),
            detail: None,
            vals: words64.clone(),
            ffmpeg: false,
            enc: " f64",
            raw: tag == b"fl64",
        });
    }
    // ---- 拒绝：浮点的 sampleSize 与类型不符 ----
    for (name, tag, bits, expected) in [
        ("bad_fl32_size16", b"fl32", 16i16, 32i16),
        ("bad_fl64_size32", b"fl64", 32, 64),
        ("bad_FL32_size64", b"FL32", 64, 32),
    ] {
        cs.push(err(
            name,
            aifc(tag, 2, 1, bits, 44_100, &[0u8; 16], 0),
            "malformed",
            &format!(
                "violation:float-sample-size:{}:{bits}:{expected}",
                String::from_utf8_lossy(tag)
            ),
        ));
    }
}

fn expect_text(c: &Case) -> String {
    let mut s = format!("result {}\n", c.result);
    match c.fmt {
        Some((rate, ch, bits)) => {
            let _ = writeln!(s, "format {rate} {ch} {bits}{}", c.enc);
        }
        None => s.push_str("format none\n"),
    }
    let _ = writeln!(s, "detail {}", c.detail.as_deref().unwrap_or("none"));
    let _ = writeln!(s, "samples {}", c.vals.len());
    let ch = c.fmt.map_or(1, |f| f.1 as usize);
    for frame in c.vals.chunks(ch) {
        let line: Vec<String> = frame.iter().map(i64::to_string).collect();
        s.push_str(&line.join(" "));
        s.push('\n');
    }
    s
}

fn ffmpeg_ref(path: &Path) -> Result<Vec<i32>> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-f", "s32le", "-c:a", "pcm_s32le", "-"])
        .output()?;
    if !out.status.success() {
        return Err(format!("ffmpeg 失败: {}", String::from_utf8_lossy(&out.stderr)).into());
    }
    Ok(out
        .stdout
        .chunks_exact(4)
        .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect())
}

pub fn generate(out: &PathBuf) -> Result<()> {
    std::fs::create_dir_all(out)?;
    let cases = cases();
    let mut names = std::collections::HashSet::new();
    for c in &cases {
        if !names.insert(c.name) {
            return Err(format!("重复的夹具名 {}", c.name).into());
        }
        let path = out.join(format!("{}.aiff", c.name));
        std::fs::write(&path, &c.data)?;
        std::fs::write(out.join(format!("{}.expect", c.name)), expect_text(c))?;
        if c.ffmpeg {
            let Some((_, _, n)) = c.fmt else {
                return Err(format!("{}: 有效夹具缺格式", c.name).into());
            };
            let reference = ffmpeg_ref(&path)?;
            let want: Vec<i32> = c.vals.iter().map(|&s| (s << (32 - n)) as i32).collect();
            if reference != want {
                return Err(format!("ffmpeg 参考与生成样本不一致: {}", c.name).into());
            }
            let bytes: Vec<u8> = reference.iter().flat_map(|x| x.to_le_bytes()).collect();
            std::fs::write(out.join(format!("{}.ffmpeg.s32le", c.name)), bytes)?;
        }
        if c.raw {
            // F5 浮点：ffmpeg 把 fl32 / fl64 原样转为小端浮点，必须与写入的位模式逐位一致（binary64
            // 的两个字先低后高，即小端字节序）。
            let f = if c.enc == " f64" { "f64le" } else { "f32le" };
            let o = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&path)
                .args(["-f", f, "-c:a", &format!("pcm_{f}"), "-"])
                .output()?;
            if !o.status.success() {
                return Err(format!("ffmpeg 失败: {}", String::from_utf8_lossy(&o.stderr)).into());
            }
            let want: Vec<u8> = c
                .vals
                .iter()
                .flat_map(|&w| (w as i32).to_le_bytes())
                .collect();
            if o.stdout != want {
                return Err(format!("ffmpeg 原样输出与写入的位模式不一致: {}", c.name).into());
            }
            std::fs::write(out.join(format!("{}.ffmpeg.raw", c.name)), o.stdout)?;
        }
    }
    println!("{} AIFF cases -> {}", cases.len(), out.display());
    Ok(())
}
