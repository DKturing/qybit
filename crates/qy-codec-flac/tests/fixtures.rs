//! M1 T2：独立参考夹具（Xiph flac 1.4.3 编码与按 RFC 9639 手写，均经 libFLAC 解码核对）。
//!
//! 每个夹具：一次性、逐字节、50 个固定种子随机分块输入，并随机部分取走输出；
//! 结果、错误明细、格式与全部样本必须与 `.meta` / `.s32le` 一致。

use std::fs;
use std::path::{Path, PathBuf};

use qy_codec_flac::{Capacity, FlacDecoder};
use qy_core::{DecodeError, Decoder, SourceFormat, Status};

const SEEDS: u64 = 50;

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn leak<T: Copy + Default>(n: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); n].into_boxed_slice())
}

fn decoder() -> FlacDecoder {
    let c = Capacity::DESKTOP;
    match FlacDecoder::new(c, leak(c.workspace_len()), leak(c.pcm_len())) {
        Ok(d) => d,
        Err(e) => panic!("{e:?}"),
    }
}

pub struct Meta {
    pub result: String,
    pub detail: String,
    pub format: Option<(u32, u16, u8)>,
    pub frames: usize,
}

pub fn meta(name: &str) -> Meta {
    let text = fs::read_to_string(dir().join(format!("{name}.meta"))).unwrap_or_default();
    let mut m = Meta {
        result: String::new(),
        detail: String::new(),
        format: None,
        frames: 0,
    };
    for line in text.lines() {
        let (k, v) = line.split_once(' ').unwrap_or((line, ""));
        match k {
            "result" => m.result = v.into(),
            "detail" => m.detail = v.into(),
            "format" if v != "none" => {
                let p: Vec<u32> = v.split(' ').map(|x| x.parse().unwrap_or(0)).collect();
                m.format = Some((p[0], p[1] as u16, p[2] as u8));
            }
            "frames" => m.frames = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    m
}

fn samples(name: &str) -> Vec<i32> {
    fs::read(dir().join(format!("{name}.s32le")))
        .unwrap_or_default()
        .chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

struct Lcg(u64);
impl Lcg {
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        lo + ((self.0 >> 33) as usize) % (hi - lo + 1)
    }
}

#[derive(Debug, PartialEq)]
struct Outcome {
    result: Result<(), DecodeError>,
    detail: String,
    format: Option<SourceFormat>,
    samples: Vec<i32>,
}

fn run(
    dec: &mut FlacDecoder,
    data: &[u8],
    mut chunk: impl FnMut() -> usize,
    mut take: impl FnMut(usize) -> usize,
) -> Outcome {
    dec.reset();
    let mut window: Vec<u8> = Vec::new();
    let mut fed = 0usize;
    let mut out = Vec::new();
    let mut steps = 0usize;
    let result = loop {
        steps += 1;
        assert!(steps < 4 * data.len() + 10_000, "解码未推进");
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
                        assert!(avail > 0);
                        let k = take(avail).clamp(1, avail);
                        out.extend_from_slice(&dec.pcm()[..k * ch]);
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
        detail: dec.last_error().map_or("-".into(), |e| format!("{e:?}")),
        format: dec.format(),
        samples: out,
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
                        .strip_suffix(".flac")
                        .map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn expected_result(m: &Meta) -> Result<(), DecodeError> {
    match m.result.as_str() {
        "ok" => Ok(()),
        "Malformed" => Err(DecodeError::Malformed),
        "Unsupported" => Err(DecodeError::Unsupported),
        "Truncated" => Err(DecodeError::Truncated),
        "CapacityExceeded" => Err(DecodeError::CapacityExceeded),
        x => panic!("未知结果 {x}"),
    }
}

#[test]
fn all_fixtures_match_reference_under_any_chunking() {
    let names = names();
    assert!(names.len() >= 44, "{}", names.len());
    let mut dec = decoder();
    for name in &names {
        let data = fs::read(dir().join(format!("{name}.flac"))).unwrap_or_default();
        let m = meta(name);
        let want = samples(name);
        let one = run(&mut dec, &data, || usize::MAX, |n| n);
        assert_eq!(one.result, expected_result(&m), "{name}: 结果");
        assert_eq!(one.detail, m.detail, "{name}: 错误明细");
        assert!(
            one.samples == want,
            "{name}: 样本不一致（{} vs {}）",
            one.samples.len(),
            want.len()
        );
        if m.result == "ok" {
            let (r, c, b) = m.format.unwrap_or((0, 0, 0));
            assert_eq!(
                one.format
                    .map(|f| (f.sample_rate(), f.channels(), f.bits())),
                Some((r, c, b)),
                "{name}"
            );
        }
        // 逐字节（大夹具也跑，整帧门槛保证解析次数为 O(log F)）。
        let bytewise = run(&mut dec, &data, || 1, |n| n);
        assert_eq!(bytewise, one, "{name}: 逐字节");
        let seeds = if data.len() > 200_000 { 5 } else { SEEDS };
        for seed in 0..seeds {
            let mut r = Lcg(seed ^ 0xF1AC);
            let mut r2 = Lcg(seed.wrapping_mul(7919) + 1);
            let got = run(
                &mut dec,
                &data,
                || match r.range(0, 3) {
                    0 => 1,
                    1 => r.range(1, 64),
                    2 => r.range(1, 4096),
                    _ => r.range(1, 70_000),
                },
                |n| {
                    if r2.range(0, 1) == 0 {
                        n
                    } else {
                        r2.range(1, n)
                    }
                },
            );
            assert_eq!(got, one, "{name}: 随机分块 seed={seed}");
        }
    }
}
