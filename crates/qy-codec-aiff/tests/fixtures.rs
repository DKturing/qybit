//! F2：AIFF / AIFF-C 夹具（`qy-gen-fixtures aiff` 生成，期望由生成器独立给出，ffmpeg 为第二参考）。
//!
//! 每个夹具按整块、1 字节、随机分块（固定种子）三种方式输入，结果（类别、格式、明细、样本）都必须
//! 与 `.expect` 相同；有 `.ffmpeg.s32le` 的另与 ffmpeg 逐样本比较。

use std::path::PathBuf;

use qy_codec_aiff::{AiffDecoder, AiffDetail, Violation};
use qy_core::{DecodeError, Decoder, PCM_WORK_SAMPLES, SampleEncoding, Status};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn id(t: [u8; 4]) -> String {
    String::from_utf8_lossy(&t).into_owned()
}

/// 明细的规范文本（与生成器的写法相同）。
fn detail_text(d: &AiffDetail) -> String {
    match *d {
        AiffDetail::NotCompliant(v) => format!(
            "violation:{}",
            match v {
                Violation::FormSize { size } => format!("form-size:{size}"),
                Violation::ChunkSize { id: i, size } => format!("chunk-size:{}:{size}", id(i)),
                Violation::ChunkBeyondForm { id: i } => format!("chunk-beyond-form:{}", id(i)),
                Violation::StrayBytes { bytes } => format!("stray-bytes:{bytes}"),
                Violation::CommSize { size, expected } => format!("comm-size:{size}:{expected}"),
                Violation::FverSize { size } => format!("fver-size:{size}"),
                Violation::DuplicateChunk { id: i } => format!("duplicate:{}", id(i)),
                Violation::MissingChunk { id: i } => format!("missing:{}", id(i)),
                Violation::Channels { value } => format!("channels:{value}"),
                Violation::SampleSize { value } => format!("sample-size:{value}"),
                Violation::FloatSampleSize {
                    tag,
                    value,
                    expected,
                } => format!("float-sample-size:{}:{value}:{expected}", id(tag)),
                Violation::SsndTooShort { size, needed } => {
                    format!("ssnd-too-short:{size}:{needed}")
                }
            }
        ),
        AiffDetail::DataShort { frames, missing } => format!("data-short:{frames}:{missing}"),
        AiffDetail::FormShort { missing } => format!("form-short:{missing}"),
        AiffDetail::SsndBeforeComm => "ssnd-before-comm".into(),
        AiffDetail::SampleRate { raw } => format!(
            "sample-rate:{}",
            raw.iter().map(|x| format!("{x:02X}")).collect::<String>()
        ),
        AiffDetail::FormatVersion { timestamp } => format!("format-version:{timestamp:08X}"),
        AiffDetail::Compressed { tag } => format!("compressed:{}", id(tag)),
        AiffDetail::NonStandardType { tag } => format!("non-standard:{}", id(tag)),
        AiffDetail::AmbiguousLayout { channels } => format!("ambiguous-layout:{channels}"),
        AiffDetail::UndefinedLayout { channels } => format!("undefined-layout:{channels}"),
    }
}

#[derive(Debug, PartialEq)]
struct Outcome {
    result: String,
    format: String,
    detail: String,
    samples: Vec<i32>,
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        (self.0 >> 33) as usize
    }
}

fn run(d: &mut AiffDecoder, data: &[u8], mut chunk: impl FnMut() -> usize) -> Outcome {
    d.reset();
    let mut window: Vec<u8> = Vec::new();
    let mut fed = 0;
    let mut samples = Vec::new();
    let result = loop {
        let eof = fed == data.len();
        match d.decode(&window, eof) {
            Err(e) => break e,
            Ok(p) => {
                assert!(p.consumed <= window.len());
                window.drain(..p.consumed);
                match p.status {
                    Status::Frames(n) => {
                        let words = d.format().map_or(1, |f| f.words_per_frame());
                        assert_eq!(d.pcm().len(), n * words);
                        samples.extend_from_slice(d.pcm());
                        d.release(n);
                    }
                    Status::NeedOutput => panic!("工作区已取空仍要求输出"),
                    Status::NeedInput => {
                        assert!(!eof, "EOF 时不得返回 NeedInput");
                        let n = chunk().clamp(1, data.len() - fed);
                        window.extend_from_slice(&data[fed..fed + n]);
                        fed += n;
                    }
                    Status::End => {
                        return Outcome {
                            result: "ok".into(),
                            format: fmt(d),
                            detail: d.last_detail().map_or("none".into(), |x| detail_text(&x)),
                            samples,
                        };
                    }
                }
            }
        }
    };
    let result = match result {
        DecodeError::Malformed => "malformed",
        DecodeError::Unsupported => "unsupported",
        DecodeError::Truncated => "truncated",
        DecodeError::CapacityExceeded => "capacity",
    };
    Outcome {
        result: result.into(),
        format: fmt(d),
        detail: d.last_detail().map_or("none".into(), |x| detail_text(&x)),
        samples,
    }
}

/// 格式文本：整数为“采样率 声道 位数”，浮点（F5）另加 ` f32` / ` f64`。
fn fmt(d: &AiffDecoder) -> String {
    d.format().map_or("none".into(), |f| {
        let enc = match f.encoding() {
            SampleEncoding::Int => "",
            SampleEncoding::Float32 => " f32",
            SampleEncoding::Float64 => " f64",
        };
        format!("{} {} {}{enc}", f.sample_rate(), f.channels(), f.bits())
    })
}

fn expected(text: &str) -> Outcome {
    let mut lines = text.lines();
    let mut field = |k: &str| {
        lines
            .next()
            // 只去掉“字段名 + 空格”：明细中的类型码可能以空格结尾（如 `raw `）。
            .and_then(|l| l.strip_prefix(k)?.strip_prefix(' '))
            .map(str::to_string)
            .unwrap_or_else(|| panic!("缺少 {k}"))
    };
    let result = field("result");
    let format = field("format");
    let detail = field("detail");
    let _count = field("samples");
    let samples = lines
        .flat_map(|l| {
            l.split(' ')
                .filter(|x| !x.is_empty())
                .map(|x| x.parse::<i32>().unwrap_or(i32::MIN))
                .collect::<Vec<_>>()
        })
        .collect();
    Outcome {
        result,
        format,
        detail,
        samples,
    }
}

#[test]
fn fixtures_match_independent_reference_under_any_chunking() {
    let pcm = Box::leak(vec![0i32; PCM_WORK_SAMPLES].into_boxed_slice());
    let Ok(mut d) = AiffDecoder::new(pcm) else {
        panic!("decoder");
    };
    let mut names: Vec<_> = std::fs::read_dir(dir())
        .map(|r| {
            r.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "aiff"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    assert!(names.len() >= 50, "夹具数 {}", names.len());
    let mut ffmpeg = 0;
    for path in &names {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let data = std::fs::read(path).unwrap_or_default();
        let text = std::fs::read_to_string(path.with_extension("expect")).unwrap_or_default();
        let want = expected(&text);
        let whole = run(&mut d, &data, || usize::MAX);
        assert_eq!(whole, want, "{name}");
        assert_eq!(run(&mut d, &data, || 1), want, "{name} 1 字节分块");
        let mut r = Lcg(name.len() as u64 * 7919);
        assert_eq!(
            run(&mut d, &data, || r.next() % 97 + 1),
            want,
            "{name} 随机分块"
        );
        if let Ok(bytes) = std::fs::read(dir().join(format!("{name}.ffmpeg.s32le"))) {
            let n: u32 = want
                .format
                .split(' ')
                .nth(2)
                .and_then(|b| b.parse().ok())
                .unwrap_or(0);
            let reference: Vec<i32> = bytes
                .chunks_exact(4)
                .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) >> (32 - n))
                .collect();
            assert_eq!(whole.samples, reference, "{name} ffmpeg");
            ffmpeg += 1;
        }
        // F5 浮点：ffmpeg 按 f32le / f64le 原样输出的字节与解码出的位模式（binary64 先低后高）逐位一致。
        if let Ok(bytes) = std::fs::read(dir().join(format!("{name}.ffmpeg.raw"))) {
            let ours: Vec<u8> = whole.samples.iter().flat_map(|w| w.to_le_bytes()).collect();
            assert!(ours == bytes, "{name} ffmpeg 原样输出");
            ffmpeg += 1;
        }
    }
    assert!(ffmpeg >= 14, "ffmpeg 参考数 {ffmpeg}");
}

/// 明细的类别与报告文字。
#[test]
fn detail_categories_and_text() {
    use qy_codec_aiff::Category;
    let cases = [
        (
            AiffDetail::NotCompliant(Violation::MissingChunk { id: *b"COMM" }),
            Category::NotCompliant,
            "不符合 AIFF 规范：缺少必需的 chunk 'COMM'",
        ),
        (
            AiffDetail::SsndBeforeComm,
            Category::NotYetSupported,
            "符合规范但本程序尚不支持：SSND",
        ),
        (
            AiffDetail::NonStandardType { tag: *b"ulaw" },
            Category::Extension,
            "AIFF-C 规范之外的扩展，本程序不支持：compressionType 'ulaw'",
        ),
        (
            AiffDetail::AmbiguousLayout { channels: 4 },
            Category::NotYetSupported,
            "4 声道",
        ),
        (
            AiffDetail::DataShort {
                frames: 30,
                missing: 69,
            },
            Category::NotCompliant,
            "缺少 69 字节",
        ),
    ];
    for (d, cat, text) in cases {
        assert_eq!(d.category(), cat, "{d:?}");
        let s = d.to_string();
        assert!(s.contains(text), "{s}");
    }
}
