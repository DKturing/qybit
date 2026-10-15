//! 按文件头选择解码器的 fuzz 目标（WAV、AIFF、FLAC，含文件开头的 ID3v2 标签）。
//!
//! 输入首字节为分块/取出种子，第二字节为标签参数（位 0：是否在文件前另加一个合规的 ID3v2 标签；
//! 位 1：v2.3/v2.4；位 2：标签尾（仅 v2.4）；位 3–7：声明长度 × 37），其余为文件内容。检查：不 panic；
//! 消耗量不越界；EOF 后不返回 NeedInput；分块输入与一次性输入的结果完全一致（容器、格式、样本、
//! 错误明细、修正、跳过的标签字节）；另加的标签只使跳过的字节数增加标签长度，其余结果不变。
#![no_main]

use libfuzzer_sys::fuzz_target;
use qy_codec_aiff::AiffDetail;
use qy_codec_auto::{AutoDecoder, Codec};
use qy_codec_flac::{Capacity, FlacError};
use qy_codec_wav::WavDetail;
use qy_core::engine::{Repair, Resources};
use qy_core::{DecodeError, Decoder, PCM_WORK_SAMPLES, SourceFormat, Status};
use std::sync::{Mutex, OnceLock};

#[derive(Debug, PartialEq)]
struct Outcome {
    result: Result<(), DecodeError>,
    codec: Codec,
    format: Option<SourceFormat>,
    samples: Vec<i32>,
    flac: Option<FlacError>,
    wav: Option<WavDetail>,
    aiff: Option<AiffDetail>,
    magic: Option<[u8; 4]>,
    repairs: Vec<Repair>,
    skipped: u64,
}

fn run(
    dec: &mut AutoDecoder,
    data: &[u8],
    mut chunk: impl FnMut() -> usize,
    mut take: impl FnMut(usize) -> usize,
) -> Outcome {
    dec.reset();
    let window_cap = dec.required_window();
    let mut window = Vec::new();
    let mut fed = 0;
    let mut samples = Vec::new();
    let result = loop {
        let eof = fed == data.len();
        let p = match dec.decode(&window, eof) {
            Ok(p) => p,
            Err(e) => break Err(e),
        };
        assert!(p.consumed <= window.len());
        window.drain(..p.consumed);
        match p.status {
            Status::Frames(_) | Status::NeedOutput => {
                let Some(f) = dec.format() else {
                    panic!("有 PCM 却无格式");
                };
                // 每帧的字数（F3：binary64 每个样本占两个字），`release` 按帧计。
                let ch = f.words_per_frame();
                let avail = dec.pcm().len() / ch;
                assert!(avail > 0 && dec.pcm().len() % ch == 0);
                let k = take(avail).clamp(1, avail);
                samples.extend_from_slice(&dec.pcm()[..k * ch]);
                dec.release(k);
            }
            Status::NeedInput => {
                assert!(!eof, "EOF 后不得返回 NeedInput");
                assert!(window.len() < window_cap, "窗口满仍缺输入");
                let n = chunk().clamp(1, (data.len() - fed).min(window_cap - window.len()));
                window.extend_from_slice(&data[fed..fed + n]);
                fed += n;
            }
            Status::End => break Ok(()),
        }
    };
    Outcome {
        result,
        codec: dec.codec(),
        format: dec.format(),
        samples,
        flac: dec.flac_error(),
        wav: dec.wav_detail(),
        aiff: dec.aiff_detail(),
        magic: dec.unrecognized().map(|u| u.magic),
        repairs: (0..dec.repair_count())
            .filter_map(|i| dec.repair(i))
            .collect(),
        skipped: dec.skipped_tag_bytes(),
    }
}

fn decoder() -> &'static Mutex<AutoDecoder> {
    static DEC: OnceLock<Mutex<AutoDecoder>> = OnceLock::new();
    DEC.get_or_init(|| {
        let cap = Capacity::DESKTOP_MULTICHANNEL;
        let leak_i32 = |n| Box::leak(vec![0i32; n].into_boxed_slice());
        let ws = Box::leak(vec![0i64; cap.workspace_len()].into_boxed_slice());
        match AutoDecoder::new(leak_i32(PCM_WORK_SAMPLES), cap, ws, leak_i32(cap.pcm_len())) {
            Ok(d) => Mutex::new(d),
            Err(e) => panic!("{e:?}"),
        }
    })
}

/// 合规的 ID3v2 标签（ID3v2.4.0 结构规范 §3.1）。
fn id3(major: u8, body: u32, footer: bool) -> Vec<u8> {
    let flags = if footer { 0x10 } else { 0 };
    let size = [21, 14, 7, 0].map(|s| (body >> s) as u8 & 0x7F);
    let mut t = vec![b'I', b'D', b'3', major, 0, flags];
    t.extend_from_slice(&size);
    t.resize(10 + body as usize, 0xFF);
    if footer {
        t.extend_from_slice(&[b'3', b'D', b'I', major, 0, flags]);
        t.extend_from_slice(&size);
    }
    t
}

fuzz_target!(|input: &[u8]| {
    let [seed, tag, data @ ..] = input else {
        return;
    };
    let (seed, tag) = (*seed, *tag);
    let Ok(mut dec) = decoder().lock() else {
        return;
    };
    let one = run(&mut dec, data, || usize::MAX, |n| n);
    let mut s = u32::from(seed) | 0x100;
    let mut next = move || {
        s = s.wrapping_mul(1_103_515_245).wrapping_add(12345);
        (s >> 16) as usize
    };
    let mut t = u32::from(seed).wrapping_mul(7919) | 1;
    let mut take = move |n: usize| {
        t = t.wrapping_mul(1_103_515_245).wrapping_add(12345);
        if seed & 0x80 == 0 {
            n
        } else {
            (t >> 16) as usize % n + 1
        }
    };
    let mut chunk = move || match seed % 4 {
        0 => 1,
        1 => next() % 97 + 1,
        _ => next() % 4096 + 1,
    };
    let chunked = run(&mut dec, data, &mut chunk, &mut take);
    assert_eq!(one, chunked);
    if tag & 1 != 0 {
        let major = if tag & 2 == 0 { 3 } else { 4 };
        // 标签尾只在 v2.4 定义。
        let t = id3(major, u32::from(tag >> 3) * 37, major == 4 && tag & 4 != 0);
        let tagged = [t.as_slice(), data].concat();
        let mut got = run(&mut dec, &tagged, &mut chunk, &mut take);
        assert_eq!(got.skipped, one.skipped + t.len() as u64);
        got.skipped = one.skipped;
        assert_eq!(got, one);
    }
});
