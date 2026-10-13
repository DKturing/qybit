//! FLAC 解码 fuzz 目标（M1 T3）。
//!
//! 输入首字节作为分块/取出种子，其余为文件内容。检查：不 panic；消耗量不越界；
//! EOF 后不返回 NeedInput；输出样本均在 N 位有符号范围内；分块输入与一次性输入的结果、
//! 错误明细、格式与样本完全一致。工作区为桌面容量（泄漏一次，跨输入复用）。
#![no_main]

use libfuzzer_sys::fuzz_target;
use qy_codec_flac::{Capacity, FlacDecoder, FlacError};
use qy_core::{DecodeError, Decoder, SourceFormat, Status};
use std::sync::{Mutex, OnceLock};

type Outcome = (
    Result<(), DecodeError>,
    Option<FlacError>,
    Option<SourceFormat>,
    Vec<i32>,
);

fn run(
    dec: &mut FlacDecoder,
    data: &[u8],
    mut chunk: impl FnMut() -> usize,
    mut take: impl FnMut(usize) -> usize,
) -> Outcome {
    dec.reset();
    let mut window = Vec::new();
    let mut fed = 0;
    let mut out = Vec::new();
    loop {
        let eof = fed == data.len();
        match dec.decode(&window, eof) {
            Err(e) => return (Err(e), dec.last_error(), dec.format(), out),
            Ok(p) => {
                assert!(p.consumed <= window.len());
                window.drain(..p.consumed);
                match p.status {
                    Status::Frames(_) | Status::NeedOutput => {
                        let Some(f) = dec.format() else {
                            panic!("有 PCM 却无格式");
                        };
                        let ch = usize::from(f.channels());
                        let avail = dec.pcm().len() / ch;
                        assert!(avail > 0 && dec.pcm().len() % ch == 0);
                        let lim = 1i64 << (f.bits() - 1);
                        let k = take(avail).clamp(1, avail);
                        for &s in &dec.pcm()[..k * ch] {
                            assert!((-lim..lim).contains(&i64::from(s)));
                        }
                        out.extend_from_slice(&dec.pcm()[..k * ch]);
                        dec.release(k);
                    }
                    Status::NeedInput => {
                        assert!(!eof, "EOF 后不得返回 NeedInput");
                        assert!(window.len() < Capacity::DESKTOP.input_window);
                        let n = chunk().clamp(1, data.len() - fed);
                        window.extend_from_slice(&data[fed..fed + n]);
                        fed += n;
                    }
                    Status::End => return (Ok(()), dec.last_error(), dec.format(), out),
                }
            }
        }
    }
}

fn decoder() -> &'static Mutex<FlacDecoder> {
    static DEC: OnceLock<Mutex<FlacDecoder>> = OnceLock::new();
    DEC.get_or_init(|| {
        let cap = Capacity::DESKTOP;
        let ws = Box::leak(vec![0i64; cap.workspace_len()].into_boxed_slice());
        let pcm = Box::leak(vec![0i32; cap.pcm_len()].into_boxed_slice());
        match FlacDecoder::new(cap, ws, pcm) {
            Ok(d) => Mutex::new(d),
            Err(e) => panic!("{e:?}"),
        }
    })
}

fuzz_target!(|input: &[u8]| {
    let Some((&seed, data)) = input.split_first() else {
        return;
    };
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
    let chunked = run(
        &mut dec,
        data,
        || match seed % 4 {
            0 => 1,
            1 => next() % 97 + 1,
            _ => next() % 4096 + 1,
        },
        |n| {
            t = t.wrapping_mul(1_103_515_245).wrapping_add(12345);
            if seed & 0x80 == 0 { n } else { (t >> 16) as usize % n + 1 }
        },
    );
    assert_eq!(one, chunked);
});
