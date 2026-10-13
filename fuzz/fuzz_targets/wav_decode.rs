//! WAV 解码 fuzz 目标（M0 T2）。
//!
//! 输入首字节作为分块种子，其余为文件内容。检查：不 panic；消耗量不越界；
//! EOF 后不返回 NeedInput；分块输入与一次性输入的样本、结果、格式完全一致。
#![no_main]

use libfuzzer_sys::fuzz_target;
use qy_codec_wav::WavDecoder;
use qy_core::{DecodeError, Decoder, PCM_WORK_SAMPLES, SourceFormat, Status};
use std::sync::Mutex;
use std::sync::OnceLock;

type Outcome = (Result<(), DecodeError>, Option<SourceFormat>, Vec<i32>);

fn run(dec: &mut WavDecoder, data: &[u8], mut chunk: impl FnMut() -> usize) -> Outcome {
    dec.reset();
    let mut window = Vec::new();
    let mut fed = 0;
    let mut out = Vec::new();
    loop {
        let eof = fed == data.len();
        match dec.decode(&window, eof) {
            Err(e) => return (Err(e), dec.format(), out),
            Ok(p) => {
                assert!(p.consumed <= window.len());
                window.drain(..p.consumed);
                match p.status {
                    Status::Frames(_) | Status::NeedOutput => {
                        let n = dec.pcm().len();
                        assert!(n > 0);
                        out.extend_from_slice(dec.pcm());
                        dec.release(usize::MAX);
                    }
                    Status::NeedInput => {
                        assert!(!eof);
                        let n = chunk().clamp(1, data.len() - fed);
                        window.extend_from_slice(&data[fed..fed + n]);
                        fed += n;
                    }
                    Status::End => return (Ok(()), dec.format(), out),
                }
            }
        }
    }
}

fn decoder() -> &'static Mutex<WavDecoder> {
    static DEC: OnceLock<Mutex<WavDecoder>> = OnceLock::new();
    DEC.get_or_init(|| {
        let pcm = Box::leak(vec![0i32; PCM_WORK_SAMPLES].into_boxed_slice());
        match WavDecoder::new(pcm) {
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
    let one = run(&mut dec, data, || usize::MAX);
    let mut s = u32::from(seed) | 0x100;
    let chunked = run(&mut dec, data, || {
        s = s.wrapping_mul(1_103_515_245).wrapping_add(12345);
        match seed % 4 {
            0 => 1,
            _ => ((s >> 16) % 97 + 1) as usize,
        }
    });
    assert_eq!(one, chunked);
});
