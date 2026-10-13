//! 生成 qy-codec-wav 的独立参考夹具（M0 T2）。
//!
//! 本工具不依赖任何 qy- crate：从已知整数样本直接构造 WAV 字节，样本值写入
//! `.expect`；对有效夹具再调用 ffmpeg 解码为 s32le 写入 `.ffmpeg.s32le`，并断言其
//! 等于 `样本 << (32 − V)`，作为第二个独立参考。
//!
//! 用法：`cargo run -p qy-gen-fixtures -- [输出目录]`（默认 `crates/qy-codec-wav/tests/fixtures`）。
//! 固定种子，重复运行产生逐字节相同的输出（需要 PATH 中有 ffmpeg，版本记录在文档）。

mod flac;

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

/// 有效位左对齐写入 C 位容器（小端）；`dirty` 时低位填充随机非零位。
fn encode(vals: &[i64], c: u32, v: u32, dirty_seed: Option<u64>) -> Vec<u8> {
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
    });
}

fn build_cases() -> Vec<Case> {
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
    cs.push(err_case("trunc_in_fmt", full[..20].to_vec(), "Truncated"));
    cs.push(err_case(
        "trunc_riff_header",
        full[..7].to_vec(),
        "Truncated",
    ));
    let v = samples(21, 10, 1, 16);
    let data = riff_with(
        &[
            fmt_pcm(pcm(1, 44_100, 16)),
            chunk(b"data", &encode(&v, 16, 16, None)),
        ],
        10,
        b"RIFF",
        b"WAVE",
    );
    manual(
        cs,
        "trunc_riff_claims_more",
        data,
        "Truncated",
        (44_100, 1, 16),
        v,
        false,
    );
    cs.push(err_case("trunc_empty_file", Vec::new(), "Truncated"));

    // ---- 合法但不支持 ----
    let d = chunk(b"data", &[0u8; 8]);
    let un = "Unsupported";
    cs.push(err_case(
        "unsup_8bit",
        riff(&[fmt_pcm(pcm(2, 44_100, 8)), d.clone()]),
        un,
    ));
    cs.push(err_case(
        "unsup_float_tag3",
        riff(&[
            fmt_pcm(Pcm {
                tag: 3,
                ..pcm(2, 44_100, 32)
            }),
            d.clone(),
        ]),
        un,
    ));
    cs.push(err_case(
        "unsup_ext_float",
        riff(&[
            fmt_ext(Ext {
                sub: SUBTYPE_FLOAT,
                ..ext(2, 44_100, 32, 32, 3)
            }),
            d.clone(),
        ]),
        un,
    ));
    cs.push(err_case(
        "unsup_3ch",
        riff(&[fmt_pcm(pcm(3, 44_100, 16)), chunk(b"data", &[0u8; 6])]),
        un,
    ));
    cs.push(err_case(
        "unsup_stereo_side_mask",
        riff(&[fmt_ext(ext(2, 44_100, 16, 16, 0x600)), d.clone()]),
        un,
    ));
    cs.push(err_case(
        "unsup_mono_left_mask",
        riff(&[fmt_ext(ext(1, 44_100, 16, 16, 0x1)), d.clone()]),
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
        "unsup_rf64",
        riff_with(
            &[fmt_pcm(pcm(2, 44_100, 16)), d.clone()],
            0,
            b"RF64",
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
        "unsup_rate_384k",
        riff(&[fmt_pcm(pcm(2, 384_000, 16)), d.clone()]),
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
        "mal_chunk_beyond_riff",
        riff_with(
            &[fmt_pcm(pcm(2, 44_100, 16)), d.clone()],
            -4,
            b"RIFF",
            b"WAVE",
        ),
        ma,
    ));
    cs.push(err_case(
        "mal_data_not_frame_multiple",
        riff(&[fmt_pcm(pcm(2, 44_100, 16)), chunk(b"data", &[0u8; 6])]),
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
    cases
}

fn ffmpeg_ref(path: &Path, ch: usize, c: u32) -> Result<Vec<i32>> {
    // 显式指定输入解码器：ffmpeg 6.1 会把 C32/V24 的 EXTENSIBLE PCM 误识别为 pcm_f24le。
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-c:a", &format!("pcm_s{c}le"), "-i"])
        .arg(path)
        .args([
            "-f",
            "s32le",
            "-c:a",
            "pcm_s32le",
            "-ac",
            &ch.to_string(),
            "-",
        ])
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
    let _ = writeln!(s, "samples {}", c.vals.len());
    for frame in c.vals.chunks(c.ch.max(1)) {
        let line: Vec<String> = frame.iter().map(i64::to_string).collect();
        s.push_str(&line.join(" "));
        s.push('\n');
    }
    s
}

fn main() -> Result<()> {
    // 用法：qy-gen-fixtures [wav|flac|all] [输出目录]
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
        "all" => {
            wav_main(root.join("qy-codec-wav/tests/fixtures"))?;
            flac::generate(&root.join("qy-codec-flac/tests/fixtures"))
        }
        x => Err(format!("未知参数 {x}；用法：qy-gen-fixtures [wav|flac|all] [输出目录]").into()),
    }
}

fn wav_main(out: PathBuf) -> Result<()> {
    std::fs::create_dir_all(&out)?;
    let cases = build_cases();
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
    Ok(())
}
