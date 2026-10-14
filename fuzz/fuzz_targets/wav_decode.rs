//! WAV 解码 fuzz 目标（M0 T2）。
//!
//! 输入首字节作为分块种子，其余为文件内容。检查：不 panic；消耗量不越界；
//! EOF 后不返回 NeedInput；分块输入与一次性输入的样本、结果、拒绝明细、格式（含声道布局，F1）
//! 完全一致；结构修正（W1）也完全一致，且每条修正的数值不超出输入长度。
#![no_main]

use libfuzzer_sys::fuzz_target;
use qy_codec_wav::{WavDecoder, WavDetail};
use qy_core::engine::Repair;
use qy_core::{DecodeError, Decoder, PCM_WORK_SAMPLES, SourceFormat, Status};
use std::sync::Mutex;
use std::sync::OnceLock;

type Outcome = (
    Result<(), DecodeError>,
    Option<WavDetail>,
    Option<SourceFormat>,
    Vec<i32>,
    Vec<Repair>,
);

/// 修正的数值来自源本身：读到的字节数、实际长度、丢弃的字节数都不超过输入长度。
fn check_repairs(dec: &WavDecoder, len: usize) -> Vec<Repair> {
    let r: Vec<Repair> = dec.repairs().collect();
    for x in &r {
        let n = match *x {
            Repair::LengthPlaceholder { read, .. } => read,
            Repair::LengthMismatch { actual, .. } => actual,
            Repair::PartialFrameDropped { bytes } => u64::from(bytes),
        };
        assert!(n <= len as u64, "{x:?} 超出输入长度 {len}");
    }
    r
}

fn run(dec: &mut WavDecoder, data: &[u8], mut chunk: impl FnMut() -> usize) -> Outcome {
    dec.reset();
    let mut window = Vec::new();
    let mut fed = 0;
    let mut out = Vec::new();
    loop {
        let eof = fed == data.len();
        match dec.decode(&window, eof) {
            Err(e) => {
                let r = check_repairs(dec, data.len());
                return (Err(e), dec.last_detail(), dec.format(), out, r);
            }
            Ok(p) => {
                assert!(p.consumed <= window.len());
                window.drain(..p.consumed);
                match p.status {
                    Status::Frames(_) | Status::NeedOutput => {
                        let n = dec.pcm().len();
                        assert!(n > 0);
                        let Some(f) = dec.format() else {
                            panic!("有 PCM 却无格式");
                        };
                        assert_eq!(n % usize::from(f.channels()), 0);
                        assert_eq!(
                            usize::from(f.layout().channels()),
                            usize::from(f.channels())
                        );
                        out.extend_from_slice(dec.pcm());
                        dec.release(usize::MAX);
                    }
                    Status::NeedInput => {
                        assert!(!eof);
                        let n = chunk().clamp(1, data.len() - fed);
                        window.extend_from_slice(&data[fed..fed + n]);
                        fed += n;
                    }
                    Status::End => {
                        let r = check_repairs(dec, data.len());
                        return (Ok(()), dec.last_detail(), dec.format(), out, r);
                    }
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
