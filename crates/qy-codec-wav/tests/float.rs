//! F3：浮点 WAV 夹具（`tests/fixtures/float`，由 `qy-gen-fixtures wav` 生成）。
//!
//! 期望的位模式由生成器写入，不经本解码器得出；第二参考为 ffmpeg 原样输出的 `f32le` / `f64le`。
//! 每个夹具分别以一次性、逐字节与固定种子随机分块输入，并随机部分取走输出，结果必须逐位一致。
//! 只含 [−1, 1) 内有限值的夹具，再把解码结果经 `qy_pcm_format::convert` 转为 16 / 32 位整数，
//! 与 ffmpeg 的转换参考逐样本比较（舍入约定相同的部分，见生成器说明）。

use std::fs;
use std::path::{Path, PathBuf};

use qy_codec_wav::{WavDecoder, WavDetail};
use qy_core::{DecodeError, Decoder, PCM_WORK_SAMPLES, Pcm, SampleEncoding, SourceFormat, Status};
use qy_pcm_format::convert::{ConvertStats, float_to_int};

const CHUNK_SEEDS: u64 = 100;

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/float")
}

struct Expect {
    result: Result<(), DecodeError>,
    format: Option<(u32, u16, SampleEncoding)>,
    mask: Option<u32>,
    detail: String,
    /// 解码输出的字（binary64 拆成低、高两个字）。
    words: Vec<i32>,
}

fn parse(text: &str) -> Expect {
    let mut lines = text.lines();
    let mut field = |key: &str| -> String {
        let line = lines.next().unwrap_or_default();
        line.strip_prefix(key)
            .unwrap_or_else(|| panic!("缺少字段 {key}"))
            .trim()
            .to_owned()
    };
    let result = match field("result").as_str() {
        "ok" => Ok(()),
        "Malformed" => Err(DecodeError::Malformed),
        "Unsupported" => Err(DecodeError::Unsupported),
        "Truncated" => Err(DecodeError::Truncated),
        o => panic!("未知结果 {o}"),
    };
    let format = match field("format").as_str() {
        "none" => None,
        f => {
            let v: Vec<&str> = f.split(' ').collect();
            let enc = match v[2] {
                "f32" => SampleEncoding::Float32,
                "f64" => SampleEncoding::Float64,
                e => panic!("编码 {e}"),
            };
            Some((v[0].parse().unwrap_or(0), v[1].parse().unwrap_or(0), enc))
        }
    };
    let mask = match field("mask").as_str() {
        "none" => None,
        m => m.parse().ok(),
    };
    let detail = field("detail");
    let count: usize = field("samples").parse().unwrap_or(0);
    let hex: Vec<&str> = lines
        .flat_map(|l| l.split(' '))
        .filter(|x| !x.is_empty())
        .collect();
    assert_eq!(hex.len(), count);
    let words = hex
        .iter()
        .flat_map(|h| {
            let v = u64::from_str_radix(h, 16).unwrap_or(0);
            if h.len() == 16 {
                vec![v as u32 as i32, (v >> 32) as u32 as i32]
            } else {
                vec![v as u32 as i32]
            }
        })
        .collect();
    Expect {
        result,
        format,
        mask,
        detail,
        words,
    }
}

fn detail_text(d: Option<WavDetail>) -> String {
    let opt = |v: Option<String>| v.unwrap_or_else(|| "none".into());
    match d {
        None => "none".into(),
        Some(WavDetail::Fact {
            declared,
            frames,
            duplicate,
        }) => format!(
            "fact:{}:{}:{}",
            declared,
            opt(frames.map(|x| x.to_string())),
            u8::from(duplicate)
        ),
        Some(WavDetail::DataShort { declared, missing }) => {
            format!("data-short:{declared}:{missing}")
        }
        Some(other) => format!("{other:?}"),
    }
}

struct Lcg(u64);
impl Lcg {
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        lo + ((self.0 >> 33) as usize) % (hi - lo + 1)
    }
}

#[derive(Debug, PartialEq)]
struct Outcome {
    result: Result<(), DecodeError>,
    format: Option<SourceFormat>,
    mask: Option<u32>,
    detail: String,
    words: Vec<i32>,
}

fn run(
    data: &[u8],
    mut chunk: impl FnMut() -> usize,
    mut take: impl FnMut(usize) -> usize,
) -> Outcome {
    let pcm = Box::leak(vec![0i32; PCM_WORK_SAMPLES].into_boxed_slice());
    let Ok(mut dec) = WavDecoder::new(pcm) else {
        panic!("工作区")
    };
    let mut window: Vec<u8> = Vec::new();
    let mut fed = 0usize;
    let mut words = Vec::new();
    let mut steps = 0usize;
    let result = loop {
        steps += 1;
        assert!(steps < 10 * data.len() + 1000, "解码未推进");
        let eof = fed == data.len();
        match dec.decode(&window, eof) {
            Err(e) => break Err(e),
            Ok(p) => {
                window.drain(..p.consumed);
                match p.status {
                    Status::Frames(_) | Status::NeedOutput => {
                        let wpf = dec.format().map_or(1, |f| f.words_per_frame());
                        let avail = dec.pcm().len() / wpf;
                        assert!(avail > 0 && dec.pcm().len() % wpf == 0);
                        let k = take(avail).clamp(1, avail);
                        words.extend_from_slice(&dec.pcm()[..k * wpf]);
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
    Outcome {
        result,
        format: dec.format(),
        mask: dec.channel_mask(),
        detail: detail_text(dec.last_detail()),
        words,
    }
}

fn names() -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir())
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    e.file_name()
                        .into_string()
                        .ok()?
                        .strip_suffix(".wav")
                        .map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn read_le<const N: usize>(raw: &[u8]) -> Vec<i64> {
    raw.chunks_exact(N)
        .map(|c| match N {
            2 => i64::from(i16::from_le_bytes([c[0], c[1]])),
            _ => i64::from(i32::from_le_bytes([c[0], c[1], c[2], c[3]])),
        })
        .collect()
}

#[test]
fn float_fixtures_match_reference() {
    let names = names();
    assert!(names.len() >= 16, "夹具数 {}", names.len());
    let mut converted = 0;
    for name in &names {
        let data = fs::read(dir().join(format!("{name}.wav"))).unwrap_or_default();
        let exp =
            parse(&fs::read_to_string(dir().join(format!("{name}.expect"))).unwrap_or_default());
        let one = run(&data, || usize::MAX, |n| n);
        assert_eq!(one.result, exp.result, "{name}: 结果");
        assert_eq!(one.detail, exp.detail, "{name}: 明细");
        assert_eq!(one.words, exp.words, "{name}: 位模式");
        if let Some((rate, ch, enc)) = exp.format {
            let Some(f) = one.format else {
                panic!("{name}: 格式未知")
            };
            assert_eq!(
                (f.sample_rate(), f.channels(), f.encoding()),
                (rate, ch, enc),
                "{name}"
            );
            assert_eq!(
                u32::from(f.bits()),
                if enc == SampleEncoding::Float32 {
                    32
                } else {
                    64
                }
            );
        }
        if exp.result.is_ok() {
            assert_eq!(one.mask, exp.mask, "{name}: 掩码");
        }
        // ffmpeg 原样输出：逐位一致。
        if let Ok(raw) = fs::read(dir().join(format!("{name}.ffmpeg.raw"))) {
            let ours: Vec<u8> = one.words.iter().flat_map(|w| w.to_le_bytes()).collect();
            assert_eq!(ours, raw, "{name}: ffmpeg 原样输出");
        }
        // ffmpeg 转换参考（16 / 32 位）。
        for (v, file) in [(16u8, "ffmpeg16.s16le"), (32, "ffmpeg32.s32le")] {
            let Ok(raw) = fs::read(dir().join(format!("{name}.{file}"))) else {
                continue;
            };
            let reference = if v == 16 {
                read_le::<2>(&raw)
            } else {
                read_le::<4>(&raw)
            };
            let Some(f) = one.format else { panic!() };
            let Ok(pcm) = Pcm::exact(&one.words, f) else {
                panic!("{name}")
            };
            let mut out = vec![0i32; reference.len()];
            let mut stats = ConvertStats::default();
            let Ok(alt) = float_to_int(&pcm, v, &mut out, &mut stats) else {
                panic!("{name}: 转换")
            };
            let ours: Vec<i64> = alt.samples().iter().map(|&s| i64::from(s)).collect();
            assert_eq!(ours, reference, "{name}: ffmpeg {v} 位转换");
            assert_eq!(stats.clipped, 0, "{name}");
            converted += 1;
        }
        // 逐字节与随机分块。
        assert_eq!(run(&data, || 1, |n| n), one, "{name}: 逐字节");
        for seed in 0..CHUNK_SEEDS {
            let mut r = Lcg(seed ^ 0xF3F3);
            let mut r2 = Lcg(seed.wrapping_mul(7_919) + 1);
            let random = run(
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
            assert_eq!(random, one, "{name}: 随机分块 seed={seed}");
        }
    }
    assert!(converted >= 10, "转换参考 {converted}");
}
