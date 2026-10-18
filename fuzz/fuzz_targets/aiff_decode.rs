//! AIFF / AIFF-C 解码 fuzz 目标（F2）。
//!
//! 输入首字节作为分块与取出种子，其余为文件内容。检查：不 panic；消耗量不越界；EOF 后不返回
//! NeedInput；整数输出样本都在 N 位有符号范围内（F5 的浮点为位模式，不检查范围）；分块输入与一次性输入的结果、拒绝明细（含类别）、
//! 格式与样本完全一致；明细中的长度数值不超出输入长度能解释的范围。
#![no_main]

use libfuzzer_sys::fuzz_target;
use qy_codec_aiff::{AiffDecoder, AiffDetail};
use qy_core::{DecodeError, Decoder, PCM_WORK_SAMPLES, SourceFormat, Status};
use std::sync::{Mutex, OnceLock};

type Outcome = (
    Result<(), DecodeError>,
    Option<AiffDetail>,
    Option<SourceFormat>,
    Vec<i32>,
);

fn run(
    dec: &mut AiffDecoder,
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
            Err(e) => return (Err(e), dec.last_detail(), dec.format(), out),
            Ok(p) => {
                assert!(p.consumed <= window.len());
                window.drain(..p.consumed);
                match p.status {
                    Status::Frames(_) | Status::NeedOutput => {
                        let Some(f) = dec.format() else {
                            panic!("有 PCM 却无格式");
                        };
                        // 每帧的字数（F5：binary64 浮点每样本两个字）。
                        let ch = f.words_per_frame();
                        let avail = dec.pcm().len() / ch;
                        assert!(avail > 0 && dec.pcm().len() % ch == 0);
                        let k = take(avail).clamp(1, avail);
                        // 整数样本在 N 位有符号范围内；浮点是位模式，任何值都合法。
                        if !f.encoding().is_float() {
                            let lim = 1i64 << (f.bits() - 1);
                            for &s in &dec.pcm()[..k * ch] {
                                assert!((-lim..lim).contains(&i64::from(s)));
                            }
                        }
                        out.extend_from_slice(&dec.pcm()[..k * ch]);
                        dec.release(k);
                    }
                    Status::NeedInput => {
                        assert!(!eof, "EOF 后不得返回 NeedInput");
                        // 任一结构所需的最大连续输入（COMM 278 字节或一帧）远小于核心最小窗口。
                        assert!(
                            window.len() < 8192,
                            "缺输入时窗口已有 {} 字节",
                            window.len()
                        );
                        let n = chunk().clamp(1, data.len() - fed);
                        window.extend_from_slice(&data[fed..fed + n]);
                        fed += n;
                    }
                    Status::End => return (Ok(()), dec.last_detail(), dec.format(), out),
                }
            }
        }
    }
}

fn decoder() -> &'static Mutex<AiffDecoder> {
    static DEC: OnceLock<Mutex<AiffDecoder>> = OnceLock::new();
    DEC.get_or_init(|| {
        let pcm = Box::leak(vec![0i32; PCM_WORK_SAMPLES].into_boxed_slice());
        match AiffDecoder::new(pcm) {
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
    if let Some(AiffDetail::DataShort { missing, .. } | AiffDetail::FormShort { missing }) = one.1 {
        // 缺少的字节数来自文件头声明的长度（FORM 长度 ≤ 0x7FFFFFFF）。
        assert!(missing <= 0x7FFF_FFFF + 8, "{:?}", one.1);
    }
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
            1 => next() % 13 + 1,
            _ => next() % 300 + 1,
        },
        |n| {
            t = t.wrapping_mul(1_103_515_245).wrapping_add(12345);
            if seed & 0x80 == 0 {
                n
            } else {
                (t >> 16) as usize % n + 1
            }
        },
    );
    assert_eq!(one, chunked);
    if let Some(d) = one.1 {
        assert_eq!(
            d.category(),
            chunked.1.map_or(d.category(), |x| x.category())
        );
    }
});
