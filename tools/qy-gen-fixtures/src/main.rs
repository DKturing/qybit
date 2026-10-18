//! 生成 qy-codec-wav 的独立参考夹具（M0 T2）。
//!
//! 本工具不依赖任何 qy- crate：从已知整数样本直接构造 WAV 字节，样本值写入
//! `.expect`；对有效夹具再调用 ffmpeg 解码为 s32le 写入 `.ffmpeg.s32le`，并断言其
//! 等于 `样本 << (32 − V)`，作为第二个独立参考。
//!
//! 用法：`cargo run -p qy-gen-fixtures -- [输出目录]`（默认 `crates/qy-codec-wav/tests/fixtures`）。
//! 固定种子，重复运行产生逐字节相同的输出（需要 PATH 中有 ffmpeg，版本记录在文档）。

mod aiff;
mod flac;
mod wav_float;
mod wav_rf64;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const SUBTYPE_PCM: [u8; 16] = [
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];
const SUBTYPE_FLOAT: [u8; 16] = [
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

/// 64 位线性同余发生器，输出高 53 位。
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 11
    }
}

/// 交错样本：前 7 个为极值与边界值，其余为 V 位范围内的均匀随机数。
fn samples(seed: u64, frames: usize, ch: usize, v: u32) -> Vec<i64> {
    let lo = -(1i64 << (v - 1));
    let hi = (1i64 << (v - 1)) - 1;
    let fixed = [hi, lo, 0, -1, 1, hi - 1, lo + 1];
    let mut r = Lcg(seed);
    let span = (hi - lo + 1) as u64;
    (0..frames * ch)
        .map(|i| match fixed.get(i) {
            Some(&f) => f,
            None => lo + (r.next() % span) as i64,
        })
        .collect()
}

/// 有效位左对齐写入 C 位容器（小端）；`dirty` 时低位填充随机非零位。8 位容器按 WAVE 规范为
/// 无符号、偏移 128（W1）。
fn encode(vals: &[i64], c: u32, v: u32, dirty_seed: Option<u64>) -> Vec<u8> {
    if c == 8 {
        let mut r = dirty_seed.map(Lcg);
        return vals
            .iter()
            .map(|&s| {
                let mut x = s << (8 - v);
                if let Some(r) = r.as_mut()
                    && v < 8
                {
                    x |= (1 + r.next() % ((1u64 << (8 - v)) - 1)) as i64;
                }
                (x + 128) as u8
            })
            .collect();
    }
    let mut r = dirty_seed.map(Lcg);
    let mask = if c == 64 { u64::MAX } else { (1u64 << c) - 1 };
    let mut out = Vec::with_capacity(vals.len() * (c / 8) as usize);
    for &s in vals {
        let mut x = s << (c - v);
        if let Some(r) = r.as_mut()
            && c > v
        {
            x |= (1 + r.next() % ((1u64 << (c - v)) - 1)) as i64;
        }
        let bytes = ((x as u64) & mask).to_le_bytes();
        out.extend_from_slice(&bytes[..(c / 8) as usize]);
    }
    out
}

fn chunk_opt(id: &[u8; 4], body: &[u8], pad: bool) -> Vec<u8> {
    let mut b = id.to_vec();
    b.extend_from_slice(&(body.len() as u32).to_le_bytes());
    b.extend_from_slice(body);
    if pad && body.len() % 2 == 1 {
        b.push(0);
    }
    b
}

fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    chunk_opt(id, body, true)
}

#[derive(Clone, Copy)]
struct Pcm {
    ch: u16,
    rate: u32,
    bits: u16,
    align: Option<u16>,
    tag: u16,
    size18: bool,
}

fn pcm(ch: u16, rate: u32, bits: u16) -> Pcm {
    Pcm {
        ch,
        rate,
        bits,
        align: None,
        tag: 1,
        size18: false,
    }
}

fn fmt_pcm(p: Pcm) -> Vec<u8> {
    let align = p.align.unwrap_or(p.ch * (p.bits / 8));
    let mut body = Vec::new();
    body.extend_from_slice(&p.tag.to_le_bytes());
    body.extend_from_slice(&p.ch.to_le_bytes());
    body.extend_from_slice(&p.rate.to_le_bytes());
    body.extend_from_slice(&(p.rate * u32::from(align)).to_le_bytes());
    body.extend_from_slice(&align.to_le_bytes());
    body.extend_from_slice(&p.bits.to_le_bytes());
    if p.size18 {
        body.extend_from_slice(&0u16.to_le_bytes());
    }
    chunk(b"fmt ", &body)
}

#[derive(Clone, Copy)]
struct Ext {
    ch: u16,
    rate: u32,
    c: u16,
    v: u16,
    mask: u32,
    sub: [u8; 16],
    cb: u16,
    extra: &'static [u8],
    align: Option<u16>,
}

fn ext(ch: u16, rate: u32, c: u16, v: u16, mask: u32) -> Ext {
    Ext {
        ch,
        rate,
        c,
        v,
        mask,
        sub: SUBTYPE_PCM,
        cb: 22,
        extra: b"",
        align: None,
    }
}

fn fmt_ext(e: Ext) -> Vec<u8> {
    let align = e.align.unwrap_or(e.ch * (e.c / 8));
    let mut body = Vec::new();
    body.extend_from_slice(&0xFFFEu16.to_le_bytes());
    body.extend_from_slice(&e.ch.to_le_bytes());
    body.extend_from_slice(&e.rate.to_le_bytes());
    body.extend_from_slice(&(e.rate * u32::from(align)).to_le_bytes());
    body.extend_from_slice(&align.to_le_bytes());
    body.extend_from_slice(&e.c.to_le_bytes());
    body.extend_from_slice(&e.cb.to_le_bytes());
    body.extend_from_slice(&e.v.to_le_bytes());
    body.extend_from_slice(&e.mask.to_le_bytes());
    body.extend_from_slice(&e.sub);
    body.extend_from_slice(e.extra);
    chunk(b"fmt ", &body)
}

fn riff_with(chunks: &[Vec<u8>], size_adjust: i64, magic: &[u8; 4], form: &[u8; 4]) -> Vec<u8> {
    let mut body = form.to_vec();
    for c in chunks {
        body.extend_from_slice(c);
    }
    let size = (body.len() as i64 + size_adjust) as u32;
    let mut out = magic.to_vec();
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&body);
    out
}

fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
    riff_with(chunks, 0, b"RIFF", b"WAVE")
}

struct Case {
    name: &'static str,
    data: Vec<u8>,
    result: &'static str,
    fmt: Option<(u32, u16, u32)>,
    vals: Vec<i64>,
    ch: usize,
    v: u32,
    c: u32,
    mask: Option<u32>,
    ffmpeg: bool,
    /// W1：期望的结构修正（文本形式，见 `expect_text`），由生成器按构造独立给出。
    repairs: Vec<String>,
    /// W1 R5：期望的 `DataShort` 明细（声明长度，缺少的字节数）。
    short: Option<(u64, u64)>,
    /// 期望的压缩码流明细（类型，在 data 中的偏移）。
    bitstream: Option<(&'static str, u32)>,
    /// 其他明细的文本（F4 的 `ds64:…`），优先于上面两项。
    detail: Option<String>,
}

fn err_case(name: &'static str, data: Vec<u8>, result: &'static str) -> Case {
    Case {
        name,
        data,
        result,
        fmt: None,
        vals: Vec::new(),
        ch: 1,
        v: 0,
        c: 0,
        mask: None,
        ffmpeg: false,
        repairs: Vec::new(),
        short: None,
        bitstream: None,
        detail: None,
    }
}

fn ok_pcm(
    cases: &mut Vec<Case>,
    name: &'static str,
    p: Pcm,
    frames: usize,
    seed: u64,
    before: &[Vec<u8>],
    after: &[Vec<u8>],
) {
    let bits = u32::from(p.bits);
    let vals = samples(seed, frames, usize::from(p.ch), bits);
    let mut chunks = before.to_vec();
    chunks.push(fmt_pcm(p));
    chunks.push(chunk(b"data", &encode(&vals, bits, bits, None)));
    chunks.extend_from_slice(after);
    cases.push(Case {
        name,
        data: riff(&chunks),
        result: "ok",
        fmt: Some((p.rate, p.ch, bits)),
        vals,
        ch: usize::from(p.ch),
        v: bits,
        c: bits,
        mask: None,
        ffmpeg: true,
        repairs: Vec::new(),
        short: None,
        bitstream: None,
        detail: None,
    });
}

fn ok_ext(
    cases: &mut Vec<Case>,
    name: &'static str,
    e: Ext,
    frames: usize,
    seed: u64,
    dirty: Option<u64>,
) {
    let (c, v) = (u32::from(e.c), u32::from(e.v));
    let vals = samples(seed, frames, usize::from(e.ch), v);
    let data = riff(&[fmt_ext(e), chunk(b"data", &encode(&vals, c, v, dirty))]);
    cases.push(Case {
        name,
        data,
        result: "ok",
        fmt: Some((e.rate, e.ch, v)),
        vals,
        ch: usize::from(e.ch),
        v,
        c,
        mask: Some(e.mask),
        ffmpeg: dirty.is_none(),
        repairs: Vec::new(),
        short: None,
        bitstream: None,
        detail: None,
    });
}

#[allow(clippy::too_many_arguments)]
fn manual(
    cases: &mut Vec<Case>,
    name: &'static str,
    data: Vec<u8>,
    result: &'static str,
    fmt: (u32, u16, u32),
    vals: Vec<i64>,
    ffmpeg: bool,
) {
    let (_, ch, v) = fmt;
    cases.push(Case {
        name,
        data,
        result,
        fmt: Some(fmt),
        vals,
        ch: usize::from(ch),
        v,
        c: v,
        mask: None,
        ffmpeg,
        repairs: Vec::new(),
        short: None,
        bitstream: None,
        detail: None,
    });
}

fn build_cases() -> Result<Vec<Case>> {
    let mut cases = Vec::new();
    let cs = &mut cases;

    // ---- 有效夹具 ----
    ok_pcm(cs, "pcm16_stereo_44k", pcm(2, 44_100, 16), 300, 1, &[], &[]);
    ok_pcm(
        cs,
        "pcm16_mono_8k_fmt18",
        Pcm {
            size18: true,
            ..pcm(1, 8_000, 16)
        },
        257,
        2,
        &[],
        &[],
    );
    ok_pcm(
        cs,
        "pcm24_mono_48k_odd_list",
        pcm(1, 48_000, 24),
        301,
        3,
        &[],
        &[chunk(b"LIST", b"INFOx")],
    );
    ok_pcm(cs, "pcm32_stereo_96k", pcm(2, 96_000, 32), 200, 4, &[], &[]);
    // M3 T1 去掉 192 kHz 软件上限：384 kHz 为有效样本（原 unsup_rate_384k）。
    ok_pcm(
        cs,
        "pcm16_stereo_384k",
        pcm(2, 384_000, 16),
        400,
        384,
        &[],
        &[],
    );
    ok_pcm(
        cs,
        "pcm24_stereo_large",
        pcm(2, 88_200, 24),
        10_000,
        5,
        &[],
        &[],
    );
    ok_pcm(
        cs,
        "unknown_chunks_odd",
        pcm(2, 44_100, 16),
        123,
        6,
        &[chunk(b"JUNK", b"1234567")],
        &[],
    );
    // fmt 与 data 之间的未知奇数长度 chunk
    let v = samples(7, 64, 2, 16);
    let data = riff(&[
        fmt_pcm(pcm(2, 48_000, 16)),
        chunk(b"qyX1", b"abcdefghi"),
        chunk(b"data", &encode(&v, 16, 16, None)),
    ]);
    manual(
        cs,
        "chunk_between_fmt_data",
        data,
        "ok",
        (48_000, 2, 16),
        v,
        true,
    );
    ok_ext(
        cs,
        "ext16_mono_mask0",
        ext(1, 44_100, 16, 16, 0),
        100,
        8,
        None,
    );
    ok_ext(
        cs,
        "ext12in16_stereo",
        ext(2, 48_000, 16, 12, 3),
        150,
        9,
        None,
    );
    ok_ext(
        cs,
        "ext20in24_mono_center",
        ext(1, 96_000, 24, 20, 4),
        151,
        10,
        None,
    );
    ok_ext(
        cs,
        "ext24in32_stereo_192k",
        ext(2, 192_000, 32, 24, 3),
        500,
        11,
        None,
    );
    ok_ext(cs, "ext32_stereo", ext(2, 176_400, 32, 32, 3), 99, 12, None);
    // F1：掩码完整声明布局即有效，含非标准的单/双声道位置（原 unsup_stereo_side_mask /
    // unsup_mono_left_mask：Microsoft WAVEFORMATEXTENSIBLE 中样本按掩码位从低到高对应扬声器）。
    ok_ext(
        cs,
        "ext16_stereo_side_mask",
        ext(2, 44_100, 16, 16, 0x600),
        64,
        30,
        None,
    );
    ok_ext(
        cs,
        "ext16_mono_left_mask",
        ext(1, 44_100, 16, 16, 0x1),
        64,
        31,
        None,
    );
    ok_ext(
        cs,
        "ext16_3ch_30",
        ext(3, 48_000, 16, 16, 0x7),
        120,
        32,
        None,
    );
    ok_ext(
        cs,
        "ext24_4ch_quad",
        ext(4, 44_100, 24, 24, 0x33),
        110,
        33,
        None,
    );
    ok_ext(
        cs,
        "ext24in32_6ch_51",
        ext(6, 96_000, 32, 24, 0x3F),
        100,
        34,
        None,
    );
    ok_ext(
        cs,
        "ext16_6ch_51_side",
        ext(6, 48_000, 16, 16, 0x60F),
        100,
        35,
        None,
    );
    ok_ext(
        cs,
        "ext24_8ch_71",
        ext(8, 48_000, 24, 24, 0x63F),
        90,
        36,
        None,
    );
    ok_ext(
        cs,
        "ext24_stereo_cb24",
        Ext {
            cb: 24,
            extra: b"\x00\x00",
            ..ext(2, 48_000, 24, 24, 3)
        },
        77,
        13,
        None,
    );
    ok_ext(
        cs,
        "ext20in24_dirty_pad_bits",
        ext(2, 48_000, 24, 20, 3),
        80,
        14,
        Some(99),
    );
    let data = riff(&[fmt_pcm(pcm(2, 44_100, 16)), chunk(b"data", b"")]);
    manual(
        cs,
        "empty_data",
        data,
        "ok",
        (44_100, 2, 16),
        Vec::new(),
        false,
    );
    // 最后一个奇数长度 chunk 缺少填充字节，RIFF 大小也未计入填充：容忍。
    let v = samples(15, 33, 1, 24);
    let data = riff(&[
        fmt_pcm(pcm(1, 44_100, 24)),
        chunk_opt(b"data", &encode(&v, 24, 24, None), false),
    ]);
    manual(
        cs,
        "final_pad_missing",
        data,
        "ok",
        (44_100, 1, 24),
        v,
        true,
    );
    let v = samples(16, 20, 2, 16);
    let mut data = riff(&[
        fmt_pcm(pcm(2, 44_100, 16)),
        chunk(b"data", &encode(&v, 16, 16, None)),
    ]);
    data.extend_from_slice(b"garbage!");
    manual(
        cs,
        "trailing_bytes_after_riff",
        data,
        "ok",
        (44_100, 2, 16),
        v,
        false,
    );

    // ---- 截断 ----
    let v = samples(20, 100, 2, 16);
    let full = riff(&[
        fmt_pcm(pcm(2, 44_100, 16)),
        chunk(b"data", &encode(&v, 16, 16, None)),
    ]);
    let cut = 44 + 50 * 4 + 3; // 第 51 帧中间
    manual(
        cs,
        "trunc_mid_frame",
        full[..cut].to_vec(),
        "Truncated",
        (44_100, 2, 16),
        v[..100].to_vec(),
        false,
    );
    // W1 R5：data 声明 400 字节，源中只有 203 字节。
    set_short(cs, 400, 400 - (50 * 4 + 3));
    cs.push(err_case("trunc_in_fmt", full[..20].to_vec(), "Truncated"));
    cs.push(err_case(
        "trunc_riff_header",
        full[..7].to_vec(),
        "Truncated",
    ));
    cs.push(err_case("trunc_empty_file", Vec::new(), "Truncated"));

    // ---- 合法但不支持 ----
    let d = chunk(b"data", &[0u8; 8]);
    let un = "Unsupported";
    // 浮点（F3 起支持）的夹具在 wav_float 中单独生成。
    cs.push(err_case(
        "unsup_3ch",
        riff(&[fmt_pcm(pcm(3, 44_100, 16)), chunk(b"data", &[0u8; 6])]),
        un,
    ));
    // F1：多于 2 声道而布局未完整声明（tasks/F1.md §1）。
    cs.push(err_case(
        "unsup_6ch_pcm_tag1",
        riff(&[fmt_pcm(pcm(6, 48_000, 16)), chunk(b"data", &[0u8; 12])]),
        un,
    ));
    cs.push(err_case(
        "unsup_6ch_mask0",
        riff(&[
            fmt_ext(ext(6, 48_000, 16, 16, 0)),
            chunk(b"data", &[0u8; 12]),
        ]),
        un,
    ));
    cs.push(err_case(
        "unsup_6ch_mask_fewer_bits",
        riff(&[
            fmt_ext(ext(6, 48_000, 16, 16, 0x7)),
            chunk(b"data", &[0u8; 12]),
        ]),
        un,
    ));
    cs.push(err_case(
        "unsup_6ch_mask_reserved_bit",
        riff(&[
            fmt_ext(ext(6, 48_000, 16, 16, 0x1F | (1 << 20))),
            chunk(b"data", &[0u8; 12]),
        ]),
        un,
    ));
    cs.push(err_case(
        "unsup_rifx",
        riff_with(
            &[fmt_pcm(pcm(2, 44_100, 16)), d.clone()],
            0,
            b"RIFX",
            b"WAVE",
        ),
        un,
    ));
    cs.push(err_case(
        "unsup_bits64",
        riff(&[fmt_pcm(pcm(1, 44_100, 64)), d.clone()]),
        un,
    ));
    cs.push(err_case(
        "unsup_pcm_bits12",
        riff(&[
            fmt_pcm(Pcm {
                align: Some(2),
                ..pcm(1, 44_100, 12)
            }),
            d.clone(),
        ]),
        un,
    ));
    cs.push(err_case(
        "unsup_adpcm_tag2",
        riff(&[
            fmt_pcm(Pcm {
                tag: 2,
                ..pcm(1, 44_100, 16)
            }),
            d.clone(),
        ]),
        un,
    ));

    // ---- 非法 ----
    let ma = "Malformed";
    cs.push(err_case(
        "mal_block_align",
        riff(&[
            fmt_pcm(Pcm {
                align: Some(6),
                ..pcm(2, 44_100, 16)
            }),
            chunk(b"data", &[0u8; 12]),
        ]),
        ma,
    ));
    cs.push(err_case(
        "mal_valid_gt_container",
        riff(&[fmt_ext(ext(2, 44_100, 16, 20, 3)), d.clone()]),
        ma,
    ));
    cs.push(err_case(
        "mal_valid_zero",
        riff(&[fmt_ext(ext(2, 44_100, 16, 0, 3)), d.clone()]),
        ma,
    ));
    cs.push(err_case(
        "mal_data_before_fmt",
        riff(&[d.clone(), fmt_pcm(pcm(2, 44_100, 16))]),
        ma,
    ));
    cs.push(err_case(
        "mal_cbsize_small",
        riff(&[
            fmt_ext(Ext {
                cb: 20,
                ..ext(2, 44_100, 16, 16, 3)
            }),
            d.clone(),
        ]),
        ma,
    ));
    cs.push(err_case(
        "mal_channels0",
        riff(&[
            fmt_pcm(Pcm {
                align: Some(0),
                ..pcm(0, 44_100, 16)
            }),
            d.clone(),
        ]),
        ma,
    ));
    cs.push(err_case(
        "mal_rate0",
        riff(&[fmt_pcm(pcm(2, 0, 16)), d.clone()]),
        ma,
    ));
    cs.push(err_case(
        "mal_not_wave",
        riff_with(
            &[fmt_pcm(pcm(2, 44_100, 16)), d.clone()],
            0,
            b"RIFF",
            b"AVI ",
        ),
        ma,
    ));
    cs.push(err_case(
        "mal_not_riff",
        riff_with(
            &[fmt_pcm(pcm(2, 44_100, 16)), d.clone()],
            0,
            b"FORM",
            b"WAVE",
        ),
        ma,
    ));
    cs.push(err_case(
        "mal_dup_fmt",
        riff(&[
            fmt_pcm(pcm(2, 44_100, 16)),
            fmt_pcm(pcm(2, 44_100, 16)),
            d.clone(),
        ]),
        ma,
    ));
    let data = riff(&[fmt_pcm(pcm(2, 44_100, 16)), d.clone(), d.clone()]);
    manual(
        cs,
        "mal_dup_data",
        data,
        ma,
        (44_100, 2, 16),
        vec![0; 4],
        false,
    );
    cs.push(err_case(
        "mal_no_data",
        riff(&[fmt_pcm(pcm(2, 44_100, 16))]),
        ma,
    ));
    cs.push(err_case(
        "mal_fmt_short",
        riff(&[chunk(b"fmt ", &[1, 0].repeat(7)), d.clone()]),
        ma,
    ));
    let ext_full = fmt_ext(ext(2, 44_100, 16, 16, 3));
    cs.push(err_case(
        "mal_ext_fmt_short",
        riff(&[chunk(b"fmt ", &ext_full[8..38]), d.clone()]),
        ma,
    ));
    cs.push(err_case(
        "mal_mask_too_many_bits",
        riff(&[fmt_ext(ext(1, 44_100, 16, 16, 0x3)), d.clone()]),
        ma,
    ));
    cs.push(err_case(
        "mal_ext_container_not_byte",
        riff(&[
            fmt_ext(Ext {
                align: Some(3),
                ..ext(1, 44_100, 20, 20, 4)
            }),
            d.clone(),
        ]),
        ma,
    ));
    cs.push(err_case(
        "mal_riff_size_lt4",
        b"RIFF\x02\x00\x00\x00WAVE".to_vec(),
        ma,
    ));
    let data = riff(&[fmt_pcm(pcm(2, 44_100, 16)), d.clone(), vec![0, 0, 0]]);
    manual(
        cs,
        "mal_tail_lt8_in_riff",
        data,
        ma,
        (44_100, 2, 16),
        vec![0; 4],
        false,
    );
    w1_cases(cs);
    bitstream_cases(cs);
    wav_rf64::cases(cs)?;
    Ok(cases)
}

/// 可复现的伪随机字节。
fn noise_bytes(seed: u64, n: usize) -> Vec<u8> {
    let mut r = Lcg(seed);
    (0..n).map(|_| (r.next() >> 13) as u8).collect()
}

/// 16 位字序列按字节序写出。
fn words_bytes(words: &[u16], be: bool) -> Vec<u8> {
    words
        .iter()
        .flat_map(|w| if be { w.to_be_bytes() } else { w.to_le_bytes() })
        .collect()
}

/// 16 位 DTS 帧（ETSI TS 102 114）：同步字 0x7FFE8001，FTYPE = 1、SHORT = 31、CPF = 0、
/// NBLKS = 15、FSIZE = `fsize`、AMODE = 9、SFREQ = 8（44.1 kHz），其余为随机内容。帧长 FSIZE + 1。
fn dts16_frame(seed: u64, fsize: u16, be: bool) -> Vec<u8> {
    let fields: u64 =
        (1 << 47) | (31 << 42) | (15 << 34) | (u64::from(fsize) << 20) | (9 << 14) | (8 << 10);
    let mut words = vec![0x7FFE, 0x8001];
    words.extend([(fields >> 32) as u16, (fields >> 16) as u16, fields as u16]);
    let mut out = words_bytes(&words, be);
    out.extend(noise_bytes(seed, usize::from(fsize) + 1 - out.len()));
    out
}

/// 伪装成 16 位双声道 PCM 的压缩码流（DTS-WAV 等）：按 `Unsupported` 拒绝，不输出任何样本。
/// 码流内容是合成的（同步字 + 帧头字段 + 随机字节），只用于识别测试。
fn bitstream_cases(cs: &mut Vec<Case>) {
    let wav = |data: &[u8]| riff(&[fmt_pcm(pcm(2, 44_100, 16)), chunk(b"data", data)]);
    let mut push = |name: &'static str, data: Vec<u8>, kind: &'static str, offset: u32| {
        cs.push(Case {
            bitstream: Some((kind, offset)),
            ..err_case(name, wav(&data), "Unsupported")
        });
    };
    // 14 位 DTS（“DTS CD”）：与用户提供的真实文件开头同构（FF 1F 00 E8 F1 07 …）。
    let mut d = Vec::new();
    for k in 0..4 {
        d.extend_from_slice(&[0xFF, 0x1F, 0x00, 0xE8, 0xF1, 0x07]);
        d.extend(noise_bytes(200 + k, 2010));
    }
    push("unsup_dts14_le_cd", d, "dts14", 0);
    // 14 位 DTS 大端，前面有 1000 字节静音。
    let mut d = vec![0u8; 1000];
    d.extend_from_slice(&[0x1F, 0xFF, 0xE8, 0x00, 0x07, 0xF3]);
    d.extend(noise_bytes(210, 5000));
    push("unsup_dts14_be_after_silence", d, "dts14", 1000);
    // 16 位 DTS 小端与大端：连续两帧，帧长 1006 字节。
    for (name, be) in [("unsup_dts16_le", false), ("unsup_dts16_be", true)] {
        let mut d = dts16_frame(220, 1005, be);
        d.extend(dts16_frame(221, 1005, be));
        d.extend(dts16_frame(222, 1005, be));
        push(name, d, "dts16", 0);
    }
    // IEC 61937（S/PDIF 封装的 AC-3）：Pa Pb Pc = 1（AC-3）Pd = 0x3800。
    let mut d = words_bytes(&[0xF872, 0x4E1F, 0x0001, 0x3800], false);
    d.extend(noise_bytes(230, 6144 - 8));
    push("unsup_iec61937_ac3", d, "iec61937", 0);

    // 刚好不成立：同步字形状相同但固定位不符，按 PCM 播放（样本即这些字节）。
    for (name, head) in [
        // 14 位：第 5 字节高半字节不是 F。
        (
            "pcm16_dts14_near_miss",
            vec![0xFFu8, 0x1F, 0x00, 0xE8, 0x01, 0x07],
        ),
        // 16 位：0x7FFE 0x8001 之后 FTYPE = 0（削波音频中自然出现的极值对）。
        (
            "pcm16_dts16_sync_clipped",
            vec![0xFE, 0x7F, 0x01, 0x80, 0x00, 0x7C],
        ),
    ] {
        let mut d = head;
        d.extend(noise_bytes(240, 4096 - 6));
        let vals: Vec<i64> = d
            .chunks_exact(2)
            .map(|c| i64::from(i16::from_le_bytes([c[0], c[1]])))
            .collect();
        manual(cs, name, wav(&d), "ok", (44_100, 2, 16), vals, true);
    }
}

/// 给最后加入的夹具设置期望的修正与明细。
fn set_repairs(cs: &mut [Case], repairs: &[String]) {
    if let Some(c) = cs.last_mut() {
        c.repairs = repairs.to_vec();
    }
}

fn set_short(cs: &mut [Case], declared: u64, missing: u64) {
    if let Some(c) = cs.last_mut() {
        c.short = Some((declared, missing));
    }
}

/// 写一个 chunk 头（不写体）：用于声明长度与实际内容不同的情形。
fn chunk_head(id: &[u8; 4], size: u32) -> Vec<u8> {
    let mut b = id.to_vec();
    b.extend_from_slice(&size.to_le_bytes());
    b
}

/// 用给定的 RIFF 长度字段拼出整个文件（`body` 从 “WAVE” 之后开始）。
fn riff_raw(size: u32, body: &[u8]) -> Vec<u8> {
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(body);
    out
}

fn riff_placeholder(file: &[u8]) -> String {
    format!("RIFF:placeholder:{}:{}", u32::MAX, file.len() - 8)
}

fn riff_mismatch(declared: u32, file: &[u8]) -> String {
    format!("RIFF:mismatch:{declared}:{}", file.len() - 8)
}

/// W1：真实世界文件头的容错（tasks/W1.md §1）与 8 位 PCM（§2）。每个夹具的期望（样本、修正、
/// 明细）由这里按构造独立给出。真实来源的头部结构核对见 docs/w1-wav-tolerance.md。
fn w1_cases(cs: &mut Vec<Case>) {
    let list = chunk(b"LIST", b"INFOISFT\x0e\x00\x00\x00Lavf60.16.100\x00");

    // ---- R1 + R2：ffmpeg 输出到管道（RIFF 与 data 都是 0xFFFFFFFF，data 前有 LIST）----
    let v = samples(101, 200, 1, 16);
    let mut body = fmt_pcm(pcm(1, 44_100, 16));
    body.extend_from_slice(&list);
    body.extend_from_slice(&chunk_head(b"data", u32::MAX));
    body.extend_from_slice(&encode(&v, 16, 16, None));
    let file = riff_raw(u32::MAX, &body);
    let r = vec![
        riff_placeholder(&file),
        format!("data:placeholder:{}:{}", u32::MAX, 400),
    ];
    manual(cs, "fix_ffmpeg_pipe", file, "ok", (44_100, 1, 16), v, true);
    set_repairs(cs, &r);

    // 同上，EXTENSIBLE 24 位双声道，源末尾多出不足一帧的 4 字节（R2 + R4）。
    let v = samples(102, 150, 2, 24);
    let mut body = fmt_ext(ext(2, 48_000, 24, 24, 3));
    body.extend_from_slice(&chunk_head(b"data", u32::MAX));
    body.extend_from_slice(&encode(&v, 24, 24, None));
    body.extend_from_slice(&[1, 2, 3, 4]);
    let file = riff_raw(u32::MAX, &body);
    let r = vec![
        "partial:4".to_string(),
        riff_placeholder(&file),
        format!("data:placeholder:{}:{}", u32::MAX, 150 * 6),
    ];
    cs.push(Case {
        name: "fix_pipe_ext24_partial_tail",
        data: file,
        result: "ok",
        fmt: Some((48_000, 2, 24)),
        vals: v,
        ch: 2,
        v: 24,
        c: 24,
        mask: Some(3),
        ffmpeg: true,
        repairs: r,
        short: None,
        bitstream: None,
        detail: None,
    });

    // RIFF 占位、data 长度正确（只有 R1）：data 之后的 LIST 照常跳过。
    let v = samples(103, 64, 2, 16);
    let mut body = fmt_pcm(pcm(2, 44_100, 16));
    body.extend_from_slice(&chunk(b"data", &encode(&v, 16, 16, None)));
    body.extend_from_slice(&list);
    let file = riff_raw(u32::MAX, &body);
    let r = vec![riff_placeholder(&file)];
    manual(
        cs,
        "fix_riff_placeholder",
        file,
        "ok",
        (44_100, 2, 16),
        v,
        true,
    );
    set_repairs(cs, &r);

    // R1 刚好不成立：RIFF = 0xFFFFFFFE 不是占位值，按 R3（RIFF 偏长）处理。
    let v = samples(104, 32, 1, 16);
    let body = [
        fmt_pcm(pcm(1, 8_000, 16)),
        chunk(b"data", &encode(&v, 16, 16, None)),
    ]
    .concat();
    let file = riff_raw(0xFFFF_FFFE, &body);
    let r = vec![riff_mismatch(0xFFFF_FFFE, &file)];
    manual(cs, "fix_riff_fffffffe", file, "ok", (8_000, 1, 16), v, true);
    set_repairs(cs, &r);

    // R2 刚好不成立：data = 0xFFFFFFFE 不是占位值，源比声明短（R5）。
    let v = samples(105, 40, 1, 16);
    let mut body = fmt_pcm(pcm(1, 8_000, 16));
    body.extend_from_slice(&chunk_head(b"data", 0xFFFF_FFFE));
    body.extend_from_slice(&encode(&v, 16, 16, None));
    let file = riff_raw(u32::MAX, &body);
    manual(
        cs,
        "trunc_data_fffffffe",
        file,
        "Truncated",
        (8_000, 1, 16),
        v,
        true,
    );
    set_short(cs, 0xFFFF_FFFE, u64::from(0xFFFF_FFFEu32) - 80);

    // RIFF 占位而 data 长度为 0：意图不明，拒绝。
    let mut body = fmt_pcm(pcm(1, 8_000, 16));
    body.extend_from_slice(&chunk_head(b"data", 0));
    body.extend_from_slice(&encode(&samples(106, 8, 1, 16), 16, 16, None));
    cs.push(err_case(
        "mal_riff_placeholder_data_zero",
        riff_raw(u32::MAX, &body),
        "Malformed",
    ));

    // ---- R3：RIFF 长度与源不符，data 完整 ----
    // RIFF 偏短 4 字节：data 越出 RIFF（原 mal_chunk_beyond_riff，体为全零；现改用已知样本）。
    let v = samples(107, 50, 2, 16);
    let body = [
        fmt_pcm(pcm(2, 44_100, 16)),
        chunk(b"data", &encode(&v, 16, 16, None)),
    ]
    .concat();
    let declared = (body.len() + 4 - 4) as u32;
    let file = riff_raw(declared, &body);
    let r = vec![riff_mismatch(declared, &file)];
    manual(cs, "fix_riff_short", file, "ok", (44_100, 2, 16), v, true);
    set_repairs(cs, &r);

    // RIFF 偏长 8 字节（写入程序把文件总长写进 RIFF 长度）。
    let v = samples(108, 50, 1, 24);
    let body = [
        fmt_pcm(pcm(1, 48_000, 24)),
        chunk(b"data", &encode(&v, 24, 24, None)),
    ]
    .concat();
    let declared = (body.len() + 4 + 8) as u32;
    let file = riff_raw(declared, &body);
    let r = vec![riff_mismatch(declared, &file)];
    manual(
        cs,
        "fix_riff_is_file_size",
        file,
        "ok",
        (48_000, 1, 24),
        v,
        true,
    );
    set_repairs(cs, &r);

    // RIFF 偏长 10 字节（原 trunc_riff_claims_more：data 完整，源在 RIFF 声明结束之前结束）。
    let v = samples(21, 10, 1, 16);
    let body = [
        fmt_pcm(pcm(1, 44_100, 16)),
        chunk(b"data", &encode(&v, 16, 16, None)),
    ]
    .concat();
    let declared = (body.len() + 4 + 10) as u32;
    let file = riff_raw(declared, &body);
    let r = vec![riff_mismatch(declared, &file)];
    manual(cs, "fix_riff_long", file, "ok", (44_100, 1, 16), v, true);
    set_repairs(cs, &r);

    // fuzz 发现（分块与一次性输入结果不同）：RIFF 偏长不足一个块头（2 字节），data 之后源结束；
    // 以及 RIFF 偏长 5 字节、源中只有 3 字节残余。两者都要等源结束才能判断，结果与分块无关。
    let v = samples(119, 12, 2, 16);
    let body = [
        fmt_pcm(pcm(2, 44_100, 16)),
        chunk(b"data", &encode(&v, 16, 16, None)),
    ]
    .concat();
    let declared = (body.len() + 4 + 2) as u32;
    let file = riff_raw(declared, &body);
    let r = vec![riff_mismatch(declared, &file)];
    manual(
        cs,
        "fix_riff_long_by_2",
        file,
        "ok",
        (44_100, 2, 16),
        v,
        true,
    );
    set_repairs(cs, &r);
    let v = samples(120, 12, 1, 24);
    let mut body = [
        fmt_pcm(pcm(1, 48_000, 24)),
        chunk(b"data", &encode(&v, 24, 24, None)),
    ]
    .concat();
    body.extend_from_slice(&[0xEE, 0xEE, 0xEE]);
    let declared = (body.len() + 4 + 2) as u32;
    let file = riff_raw(declared, &body);
    let r = vec![riff_mismatch(declared, &file)];
    manual(
        cs,
        "fix_riff_long_partial_tail",
        file,
        "ok",
        (48_000, 1, 24),
        v,
        true,
    );
    set_repairs(cs, &r);

    // data 之后的 LIST 越出 RIFF（RIFF 偏短 8 字节）：样本完整。
    let v = samples(109, 30, 2, 16);
    let body = [
        fmt_pcm(pcm(2, 44_100, 16)),
        chunk(b"data", &encode(&v, 16, 16, None)),
        list.clone(),
    ]
    .concat();
    let declared = (body.len() + 4 - 8) as u32;
    let file = riff_raw(declared, &body);
    let r = vec![riff_mismatch(declared, &file)];
    manual(
        cs,
        "fix_list_after_data_beyond_riff",
        file,
        "ok",
        (44_100, 2, 16),
        v,
        true,
    );
    set_repairs(cs, &r);

    // data 之后的块被源截断（RIFF 声明完整的 LIST，源在 LIST 体中间结束）。
    let v = samples(110, 30, 1, 16);
    let full = riff(&[
        fmt_pcm(pcm(1, 44_100, 16)),
        chunk(b"data", &encode(&v, 16, 16, None)),
        list.clone(),
    ]);
    let file = full[..full.len() - 10].to_vec();
    let declared = (full.len() - 8) as u32;
    let r = vec![riff_mismatch(declared, &file)];
    manual(
        cs,
        "fix_trailing_chunk_cut",
        file,
        "ok",
        (44_100, 1, 16),
        v,
        true,
    );
    set_repairs(cs, &r);

    // R3 不成立：data 之前的块越出 RIFF（无法判断是 RIFF 还是该块长度写错），拒绝。
    let v = samples(111, 8, 1, 16);
    let body = [
        fmt_pcm(pcm(1, 44_100, 16)),
        list.clone(),
        chunk(b"data", &encode(&v, 16, 16, None)),
    ]
    .concat();
    let declared = (4 + 24 + 8) as u32; // RIFF 结束在 LIST 体中间
    cs.push(err_case(
        "mal_chunk_before_data_beyond_riff",
        riff_raw(declared, &body),
        "Malformed",
    ));

    // ---- R4：data 长度不是整帧 ----
    // 16 位双声道（4 字节一帧），data 长 4k + 2：丢弃 2 字节（原 mal_data_not_frame_multiple）。
    let v = samples(112, 40, 2, 16);
    let mut d = encode(&v, 16, 16, None);
    d.extend_from_slice(&[0x55, 0xAA]);
    let file = riff(&[fmt_pcm(pcm(2, 44_100, 16)), chunk(b"data", &d)]);
    manual(
        cs,
        "fix_data_partial_frame",
        file,
        "ok",
        (44_100, 2, 16),
        v,
        true,
    );
    set_repairs(cs, &["partial:2".to_string()]);

    // data 长 4k + 1（奇数，含填充字节），其后还有 LIST：丢弃 1 字节，LIST 照常跳过。
    let v = samples(113, 33, 2, 16);
    let mut d = encode(&v, 16, 16, None);
    d.push(0x7F);
    let file = riff(&[
        fmt_pcm(pcm(2, 44_100, 16)),
        chunk(b"data", &d),
        list.clone(),
    ]);
    manual(
        cs,
        "fix_data_odd_partial_then_list",
        file,
        "ok",
        (44_100, 2, 16),
        v,
        true,
    );
    set_repairs(cs, &["partial:1".to_string()]);

    // 24 位 6 声道 5.1（18 字节一帧），data 长 18k + 17。
    let v = samples(114, 20, 6, 24);
    let mut d = encode(&v, 24, 24, None);
    d.extend_from_slice(&[0x11; 17]);
    let file = riff(&[fmt_ext(ext(6, 48_000, 24, 24, 0x3F)), chunk(b"data", &d)]);
    cs.push(Case {
        name: "fix_ext24_6ch_partial_frame",
        data: file,
        result: "ok",
        fmt: Some((48_000, 6, 24)),
        vals: v,
        ch: 6,
        v: 24,
        c: 24,
        mask: Some(0x3F),
        ffmpeg: true,
        repairs: vec!["partial:17".to_string()],
        short: None,
        bitstream: None,
        detail: None,
    });

    // R3 + R4：RIFF 偏短，data 不是整帧。
    let v = samples(115, 25, 1, 24);
    let mut d = encode(&v, 24, 24, None);
    d.extend_from_slice(&[9, 9]);
    let body = [fmt_pcm(pcm(1, 48_000, 24)), chunk(b"data", &d)].concat();
    let declared = (body.len() + 4 - 6) as u32;
    let file = riff_raw(declared, &body);
    let r = vec!["partial:2".to_string(), riff_mismatch(declared, &file)];
    manual(
        cs,
        "fix_riff_short_partial_frame",
        file,
        "ok",
        (48_000, 1, 24),
        v,
        true,
    );
    set_repairs(cs, &r);

    // ---- R5：data 声明长度超过源 ----
    // 截在帧边界：声明 400 字节，源中只有 30 帧（120 字节）。
    let v = samples(116, 100, 2, 16);
    let full = riff(&[
        fmt_pcm(pcm(2, 44_100, 16)),
        chunk(b"data", &encode(&v, 16, 16, None)),
    ]);
    manual(
        cs,
        "trunc_data_short_frame_boundary",
        full[..44 + 120].to_vec(),
        "Truncated",
        (44_100, 2, 16),
        v[..60].to_vec(),
        true,
    );
    set_short(cs, 400, 280);

    // arecord 流式输出 / 录音被中止（实测 alsa-utils 1.2.9）：RIFF = 0x80000024，
    // data = 0x80000000（不是整帧，S24_3LE 双声道 6 字节一帧），源只有若干帧加半帧。
    let v = samples(117, 120, 2, 24);
    let mut body = fmt_pcm(pcm(2, 48_000, 24));
    body.extend_from_slice(&chunk_head(b"data", 0x8000_0000));
    body.extend_from_slice(&encode(&v, 24, 24, None));
    body.extend_from_slice(&[1, 2, 3]);
    let file = riff_raw(0x8000_0024, &body);
    manual(
        cs,
        "trunc_arecord_placeholder",
        file,
        "Truncated",
        (48_000, 2, 24),
        v,
        true,
    );
    set_short(cs, 0x8000_0000, 0x8000_0000 - (120 * 6 + 3));

    // 修正与已有拒绝情形的组合：RIFF 为占位值不改变 fmt 矛盾与重复 data 的拒绝。
    let mut body = fmt_pcm(Pcm {
        align: Some(6),
        ..pcm(2, 44_100, 16)
    });
    body.extend_from_slice(&chunk_head(b"data", u32::MAX));
    body.extend_from_slice(&[0u8; 12]);
    cs.push(err_case(
        "mal_pipe_block_align",
        riff_raw(u32::MAX, &body),
        "Malformed",
    ));
    let v = samples(118, 4, 1, 16);
    let d = chunk(b"data", &encode(&v, 16, 16, None));
    let body = [fmt_pcm(pcm(1, 44_100, 16)), d.clone(), d].concat();
    manual(
        cs,
        "mal_riff_placeholder_dup_data",
        riff_raw(u32::MAX, &body),
        "Malformed",
        (44_100, 1, 16),
        v,
        false,
    );

    // ---- 8 位 PCM（W1 §2）：无符号、偏移 128；含 0、128、255 等极值 ----
    // 单声道 301 帧：data 为奇数长度，含填充字节。
    ok_pcm(cs, "pcm8_mono_8k_odd", pcm(1, 8_000, 8), 301, 120, &[], &[]);
    ok_pcm(cs, "pcm8_stereo_44k", pcm(2, 44_100, 8), 101, 121, &[], &[]);
    ok_ext(cs, "ext8_stereo", ext(2, 22_050, 8, 8, 3), 90, 122, None);
    ok_ext(cs, "ext6in8_mono", ext(1, 11_025, 8, 6, 4), 70, 123, None);
    ok_ext(
        cs,
        "ext6in8_dirty_pad_bits",
        ext(1, 11_025, 8, 6, 4),
        70,
        124,
        Some(125),
    );
    // 8 位 + R4：双声道 2 字节一帧，data 长 2k + 1（奇数，含填充）。
    let v = samples(126, 50, 2, 8);
    let mut d = encode(&v, 8, 8, None);
    d.push(0x80);
    let file = riff(&[fmt_pcm(pcm(2, 8_000, 8)), chunk(b"data", &d)]);
    manual(
        cs,
        "fix_pcm8_partial_frame",
        file,
        "ok",
        (8_000, 2, 8),
        v,
        true,
    );
    set_repairs(cs, &["partial:1".to_string()]);
}

fn ffmpeg_ref(path: &Path, _ch: usize, c: u32) -> Result<Vec<i32>> {
    let input_codec = if c == 8 {
        "pcm_u8".to_string()
    } else {
        format!("pcm_s{c}le")
    };
    // 显式指定输入解码器：ffmpeg 6.1 会把 C32/V24 的 EXTENSIBLE PCM 误识别为 pcm_f24le。
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-c:a", &input_codec, "-i"])
        .arg(path)
        // 不加 -ac：保持源声道布局原样输出，不让 ffmpeg 插入重混（F1：侧环绕等布局无法“转换”）。
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

fn expect_text(c: &Case) -> String {
    let mut s = format!("result {}\n", c.result);
    match c.fmt {
        Some((rate, ch, bits)) => {
            let _ = writeln!(s, "format {rate} {ch} {bits}");
        }
        None => s.push_str("format none\n"),
    }
    match c.mask {
        Some(m) => {
            let _ = writeln!(s, "mask {m}");
        }
        None => s.push_str("mask none\n"),
    }
    if c.repairs.is_empty() {
        s.push_str("repairs none\n");
    } else {
        let _ = writeln!(s, "repairs {}", c.repairs.join(";"));
    }
    match (c.short, c.bitstream) {
        _ if c.detail.is_some() => {
            let _ = writeln!(s, "detail {}", c.detail.as_deref().unwrap_or_default());
        }
        (Some((declared, missing)), _) => {
            let _ = writeln!(s, "detail data-short:{declared}:{missing}");
        }
        (None, Some((kind, offset))) => {
            let _ = writeln!(s, "detail bitstream:{kind}:{offset}");
        }
        (None, None) => s.push_str("detail none\n"),
    }
    let _ = writeln!(s, "samples {}", c.vals.len());
    for frame in c.vals.chunks(c.ch.max(1)) {
        let line: Vec<String> = frame.iter().map(i64::to_string).collect();
        s.push_str(&line.join(" "));
        s.push('\n');
    }
    s
}

fn main() -> Result<()> {
    // 用法：qy-gen-fixtures [wav|flac|aiff|all] [输出目录]
    let args: Vec<String> = std::env::args().skip(1).collect();
    let what = args.first().map_or("all", String::as_str);
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates");
    let dir = |default: &str| {
        args.get(1)
            .map_or_else(|| root.join(default), PathBuf::from)
    };
    match what {
        "wav" => wav_main(dir("qy-codec-wav/tests/fixtures")),
        "flac" => flac::generate(&dir("qy-codec-flac/tests/fixtures")),
        "aiff" => aiff::generate(&dir("qy-codec-aiff/tests/fixtures")),
        "all" => {
            wav_main(root.join("qy-codec-wav/tests/fixtures"))?;
            flac::generate(&root.join("qy-codec-flac/tests/fixtures"))?;
            aiff::generate(&root.join("qy-codec-aiff/tests/fixtures"))
        }
        x => Err(
            format!("未知参数 {x}；用法：qy-gen-fixtures [wav|flac|aiff|all] [输出目录]").into(),
        ),
    }
}

fn wav_main(out: PathBuf) -> Result<()> {
    std::fs::create_dir_all(&out)?;
    let cases = build_cases()?;
    let mut names = std::collections::HashSet::new();
    for c in &cases {
        if !names.insert(c.name) {
            return Err(format!("重复的夹具名 {}", c.name).into());
        }
        let wav = out.join(format!("{}.wav", c.name));
        std::fs::write(&wav, &c.data)?;
        std::fs::write(out.join(format!("{}.expect", c.name)), expect_text(c))?;
        if c.ffmpeg {
            let reference = ffmpeg_ref(&wav, c.ch, c.c)?;
            let want: Vec<i32> = c.vals.iter().map(|&s| (s << (32 - c.v)) as i32).collect();
            if reference != want {
                return Err(format!("ffmpeg 参考与生成样本不一致: {}", c.name).into());
            }
            let bytes: Vec<u8> = reference.iter().flat_map(|x| x.to_le_bytes()).collect();
            std::fs::write(out.join(format!("{}.ffmpeg.s32le", c.name)), bytes)?;
        }
    }
    println!("{} cases -> {}", cases.len(), out.display());
    wav_float::generate(&out.join("float"))
}
