//! 独立参考夹具测试（M0 T2）。
//!
//! 夹具由 `tools/qy-gen-fixtures`（`cargo run -p qy-gen-fixtures`） 从已知整数样本构造，期望值不经本解码器得出；
//! 有效夹具另附 ffmpeg 解码的 s32le 参考。每个夹具分别以一次性、逐字节、
//! 固定种子随机分块输入，并以随机部分取走输出驱动，结果必须与期望逐样本一致。
//! W1：结构修正（`repairs`）与 R5 的 `DataShort` 明细也由生成器按构造独立给出，必须完全一致。

use std::fs;
use std::path::{Path, PathBuf};

use qy_codec_wav::{Bitstream, WavDecoder, WavDetail};
use qy_core::engine::{Repair, Resources};
use qy_core::{DecodeError, Decoder, PCM_WORK_SAMPLES, SourceFormat, Status};

/// 随机分块的固定种子数。
const CHUNK_SEEDS: u64 = 200;

struct Expect {
    result: Result<(), DecodeError>,
    format: Option<(u32, u16, u8)>,
    mask: Option<u32>,
    repairs: Vec<String>,
    detail: Option<String>,
    samples: Vec<i32>,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn parse_expect(text: &str) -> Expect {
    let mut lines = text.lines();
    let mut field = |key: &str| -> String {
        let line = lines.next().unwrap_or_default();
        let rest = line
            .strip_prefix(key)
            .unwrap_or_else(|| panic!("缺少字段 {key}"));
        rest.trim().to_owned()
    };
    let result = match field("result").as_str() {
        "ok" => Ok(()),
        "Malformed" => Err(DecodeError::Malformed),
        "Unsupported" => Err(DecodeError::Unsupported),
        "Truncated" => Err(DecodeError::Truncated),
        "CapacityExceeded" => Err(DecodeError::CapacityExceeded),
        other => panic!("未知结果 {other}"),
    };
    let format = match field("format").as_str() {
        "none" => None,
        f => {
            let v: Vec<u32> = f.split(' ').map(|x| x.parse().unwrap_or(0)).collect();
            Some((v[0], v[1] as u16, v[2] as u8))
        }
    };
    let mask = match field("mask").as_str() {
        "none" => None,
        m => m.parse().ok(),
    };
    let mut repairs: Vec<String> = match field("repairs").as_str() {
        "none" => Vec::new(),
        r => r.split(';').map(str::to_owned).collect(),
    };
    repairs.sort();
    let detail = match field("detail").as_str() {
        "none" => None,
        d => Some(d.to_owned()),
    };
    let count: usize = field("samples").parse().unwrap_or(0);
    let samples: Vec<i32> = lines
        .flat_map(|l| l.split(' ').map(|x| x.parse::<i32>().unwrap_or(i32::MIN)))
        .collect();
    assert_eq!(samples.len(), count);
    Expect {
        result,
        format,
        mask,
        repairs,
        detail,
        samples,
    }
}

/// 修正的文本形式（与生成器的 `.expect` 一致）。
fn repair_text(r: Repair) -> String {
    let id = |c: [u8; 4]| String::from_utf8_lossy(&c).into_owned();
    match r {
        Repair::LengthPlaceholder {
            chunk,
            declared,
            read,
        } => format!("{}:placeholder:{declared}:{read}", id(chunk)),
        Repair::LengthMismatch {
            chunk,
            declared,
            actual,
        } => format!("{}:mismatch:{declared}:{actual}", id(chunk)),
        Repair::PartialFrameDropped { bytes } => format!("partial:{bytes}"),
    }
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + (self.next() as usize) % (hi - lo + 1)
    }
}

#[derive(Debug, PartialEq)]
struct Outcome {
    result: Result<(), DecodeError>,
    format: Option<SourceFormat>,
    mask: Option<u32>,
    repairs: Vec<String>,
    detail: Option<String>,
    samples: Vec<i32>,
}

fn new_decoder() -> WavDecoder {
    let pcm = Box::leak(vec![0i32; PCM_WORK_SAMPLES].into_boxed_slice());
    match WavDecoder::new(pcm) {
        Ok(d) => d,
        Err(e) => panic!("{e:?}"),
    }
}

/// 模拟宿主：按 `chunk` 给出的块大小喂入，按 `take` 给出的帧数取走输出。
fn run(
    dec: &mut WavDecoder,
    data: &[u8],
    mut chunk: impl FnMut() -> usize,
    mut take: impl FnMut(usize) -> usize,
) -> Outcome {
    dec.reset();
    let mut window: Vec<u8> = Vec::new();
    let mut fed = 0usize;
    let mut samples = Vec::new();
    let mut steps = 0usize;
    let result = loop {
        steps += 1;
        assert!(steps < 10 * data.len() + 1000, "解码未推进");
        let eof = fed == data.len();
        match dec.decode(&window, eof) {
            Err(e) => break Err(e),
            Ok(p) => {
                assert!(p.consumed <= window.len());
                window.drain(..p.consumed);
                match p.status {
                    Status::Frames(_) | Status::NeedOutput => {
                        let ch = dec.format().map_or(1, |f| usize::from(f.channels()));
                        let avail = dec.pcm().len() / ch;
                        assert!(avail > 0, "无输出时不应返回 Frames/NeedOutput");
                        let k = take(avail).clamp(1, avail);
                        samples.extend_from_slice(&dec.pcm()[..k * ch]);
                        dec.release(k);
                    }
                    Status::NeedInput => {
                        assert!(!eof, "EOF 后不得返回 NeedInput");
                        let n = chunk().clamp(1, data.len() - fed);
                        window.extend_from_slice(&data[fed..fed + n]);
                        fed += n;
                    }
                    Status::End => break Ok(()),
                }
            }
        }
    };
    // 引擎经 `Resources` 取修正，与解码器自己的列表一致。
    let via_trait: Vec<Repair> = (0..dec.repair_count())
        .filter_map(|i| dec.repair(i))
        .collect();
    assert_eq!(via_trait, dec.repairs().collect::<Vec<_>>());
    assert_eq!(dec.repair(dec.repair_count()), None);
    let mut repairs: Vec<String> = via_trait.into_iter().map(repair_text).collect();
    repairs.sort();
    let detail = match dec.last_detail() {
        Some(WavDetail::DataShort { declared, missing }) => {
            Some(format!("data-short:{declared}:{missing}"))
        }
        Some(WavDetail::Ds64(p)) => Some(format!("ds64:{p:?}")),
        Some(WavDetail::Bitstream { kind, offset }) => {
            let k = match kind {
                Bitstream::Dts14 => "dts14",
                Bitstream::Dts16 => "dts16",
                Bitstream::Iec61937 => "iec61937",
            };
            Some(format!("bitstream:{k}:{offset}"))
        }
        _ => None,
    };
    Outcome {
        result,
        format: dec.format(),
        mask: dec.channel_mask(),
        repairs,
        detail,
        samples,
    }
}

fn fixture_names() -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(fixtures_dir())
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    let n = e.file_name().into_string().ok()?;
                    n.strip_suffix(".wav").map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn check_against_expect(name: &str, out: &Outcome, exp: &Expect) {
    assert_eq!(out.result, exp.result, "{name}: 结果");
    assert_eq!(out.samples, exp.samples, "{name}: 样本");
    assert_eq!(out.repairs, exp.repairs, "{name}: 修正");
    assert_eq!(out.detail, exp.detail, "{name}: 明细");
    if let Some((rate, ch, bits)) = exp.format {
        let f = out.format.unwrap_or_else(|| panic!("{name}: 格式未知"));
        assert_eq!(
            (f.sample_rate(), f.channels(), f.bits()),
            (rate, ch, bits),
            "{name}"
        );
    } else if exp.result.is_ok() {
        panic!("{name}: 有效夹具缺少格式");
    }
    if exp.result.is_ok() {
        assert_eq!(out.mask, exp.mask, "{name}: 声道掩码");
        // F1：布局独立推出——掩码为空或 0 时按单/双声道默认，否则样本按掩码位从低到高。
        let (ch, layout) = match out.format {
            Some(f) => (f.channels(), f.layout()),
            None => panic!("{name}: 格式未知"),
        };
        let expect_mask = match exp.mask {
            None | Some(0) => match ch {
                1 => 0x4,
                2 => 0x3,
                _ => panic!("{name}: 多声道必须声明掩码"),
            },
            Some(m) => m,
        };
        let bits: Vec<u8> = (0..18u8).filter(|b| expect_mask & (1 << b) != 0).collect();
        let got: Vec<u8> = layout.positions().iter().map(|p| *p as u8).collect();
        assert_eq!(got, bits, "{name}: 声道布局");
    }
}

#[test]
fn fixtures_match_independent_reference() {
    let names = fixture_names();
    assert!(names.len() >= 80, "夹具数量 {}", names.len());
    let mut dec = new_decoder();
    for name in &names {
        let dir = fixtures_dir();
        let data = fs::read(dir.join(format!("{name}.wav"))).unwrap_or_default();
        let text = fs::read_to_string(dir.join(format!("{name}.expect"))).unwrap_or_default();
        let exp = parse_expect(&text);

        let one_shot = run(&mut dec, &data, || usize::MAX, |n| n);
        check_against_expect(name, &one_shot, &exp);

        // ffmpeg 独立参考：容器左对齐到 32 位的值 = 样本 << (32 - N)。
        let ff = dir.join(format!("{name}.ffmpeg.s32le"));
        if let Ok(raw) = fs::read(&ff) {
            let bits = one_shot.format.map_or(32, |f| u32::from(f.bits()));
            // 8 位 EXTENSIBLE（V < 8）：ffmpeg 不看有效位，输出整个 8 位容器；填充位为 0 时相同。
            let reference: Vec<i32> = raw
                .chunks_exact(4)
                .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            let ours: Vec<i32> = one_shot
                .samples
                .iter()
                .map(|&s| ((s as i64) << (32 - bits)) as i32)
                .collect();
            assert_eq!(ours, reference, "{name}: ffmpeg 参考");
        }

        let byte_wise = run(&mut dec, &data, || 1, |n| n);
        assert_eq!(byte_wise, one_shot, "{name}: 逐字节");

        for seed in 0..CHUNK_SEEDS {
            let mut r = Lcg(seed ^ 0x5157_4649);
            let mut r2 = Lcg(seed.wrapping_mul(7919));
            let random = run(
                &mut dec,
                &data,
                || match r.range(0, 3) {
                    0 => 1,
                    1 => r.range(1, 16),
                    2 => r.range(1, 300),
                    _ => r.range(1, 8192),
                },
                |n| {
                    if r2.range(0, 1) == 0 {
                        n
                    } else {
                        r2.range(1, n)
                    }
                },
            );
            assert_eq!(random, one_shot, "{name}: 随机分块 seed={seed}");
        }
    }
}

#[test]
fn format_unknown_until_header_parsed() {
    let data = fs::read(fixtures_dir().join("pcm16_stereo_44k.wav")).unwrap_or_default();
    let mut dec = new_decoder();
    for cut in 0..36 {
        dec.reset();
        let p = dec.decode(&data[..cut], false);
        assert!(
            matches!(p, Ok(p) if p.status == Status::NeedInput),
            "cut={cut}"
        );
        assert_eq!(dec.format(), None, "cut={cut}");
    }
    dec.reset();
    let p = dec.decode(&data[..44], false);
    assert!(matches!(p, Ok(p) if p.status == Status::NeedInput && p.consumed == 44));
    assert!(dec.format().is_some());
}

#[test]
fn need_output_does_not_consume_and_error_is_sticky() {
    let data = fs::read(fixtures_dir().join("pcm24_stereo_large.wav")).unwrap_or_default();
    let mut dec = new_decoder();
    let Ok(p) = dec.decode(&data, true) else {
        panic!("decode");
    };
    assert_eq!(p.status, Status::Frames(4096));
    let again = dec.decode(&data[p.consumed..], true);
    assert_eq!(
        again.ok().map(|p| (p.consumed, p.status)),
        Some((0, Status::NeedOutput))
    );

    let bad = fs::read(fixtures_dir().join("mal_block_align.wav")).unwrap_or_default();
    dec.reset();
    assert_eq!(dec.decode(&bad, true), Err(DecodeError::Malformed));
    assert_eq!(dec.decode(&data, true), Err(DecodeError::Malformed));
    dec.reset();
    assert!(dec.decode(&data, true).is_ok());
}

#[test]
fn eof_before_end_is_truncated_not_need_input() {
    let data = fs::read(fixtures_dir().join("pcm16_stereo_44k.wav")).unwrap_or_default();
    let mut dec = new_decoder();
    for cut in [0usize, 5, 12, 30, 44, 46] {
        let out = run(&mut dec, &data[..cut], || usize::MAX, |n| n);
        assert_eq!(out.result, Err(DecodeError::Truncated), "cut={cut}");
    }
}

/// F1：多于 2 声道而布局未完整声明时按 Unsupported 拒绝，并给出“未定义布局”的明细。
#[test]
fn undefined_layout_reports_detail() {
    use qy_codec_wav::WavDetail;
    let cases = [
        ("unsup_6ch_pcm_tag1", None),
        ("unsup_6ch_mask0", Some(0)),
        ("unsup_6ch_mask_fewer_bits", Some(0x7)),
        ("unsup_6ch_mask_reserved_bit", Some(0x1F | (1 << 20))),
        ("unsup_3ch", None),
    ];
    let mut dec = new_decoder();
    for (name, mask) in cases {
        let data = fs::read(fixtures_dir().join(format!("{name}.wav"))).unwrap_or_default();
        let out = run(&mut dec, &data, || usize::MAX, |n| n);
        assert_eq!(out.result, Err(DecodeError::Unsupported), "{name}");
        let channels = if name == "unsup_3ch" { 3 } else { 6 };
        assert_eq!(
            dec.last_detail(),
            Some(WavDetail::UndefinedLayout { channels, mask }),
            "{name}"
        );
        assert!(out.format.is_none(), "{name}: 拒绝时不得报告格式");
    }
    // reset 清除明细。
    dec.reset();
    assert_eq!(dec.last_detail(), None);
}
