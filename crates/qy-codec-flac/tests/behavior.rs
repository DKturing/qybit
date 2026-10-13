//! M1 T2：整帧门槛、EOF、容量与输出背压的专项行为。

use std::path::Path;

use qy_codec_flac::{Capacity, FlacDecoder, FlacError};
use qy_core::{DecodeError, Decoder, Status};

fn leak<T: Copy + Default>(n: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); n].into_boxed_slice())
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap_or_default()
}

fn dec(cap: Capacity) -> FlacDecoder {
    match FlacDecoder::new(cap, leak(cap.workspace_len()), leak(cap.pcm_len())) {
        Ok(d) => d,
        Err(e) => panic!("{e:?}"),
    }
}

fn meta_len(data: &[u8]) -> usize {
    let mut i = 4;
    loop {
        let last = data[i] & 0x80 != 0;
        let len = u32::from_be_bytes([0, data[i + 1], data[i + 2], data[i + 3]]) as usize;
        i += 4 + len;
        if last {
            return i;
        }
    }
}

/// 一次性喂入全部数据，直到结束或出错；返回结果与样本总数。
fn run_all(d: &mut FlacDecoder, data: &[u8]) -> (Result<(), DecodeError>, usize) {
    d.reset();
    let mut pos = 0;
    let mut n = 0;
    loop {
        match d.decode(&data[pos..], true) {
            Err(e) => return (Err(e), n),
            Ok(p) => {
                pos += p.consumed;
                match p.status {
                    Status::End => return (Ok(()), n),
                    Status::NeedInput => panic!("EOF 后 NeedInput"),
                    _ => {
                        n += d.pcm().len();
                        let ch = d.format().map_or(1, |f| usize::from(f.channels()));
                        d.release(d.pcm().len() / ch);
                    }
                }
            }
        }
    }
}

#[test]
fn window_just_enough_and_one_byte_short() {
    let data = fixture("max_block_verbatim_32_stereo.flac");
    let frame = data.len() - meta_len(&data);
    let exact = Capacity {
        input_window: frame,
        ..Capacity::DESKTOP
    };
    let mut d = dec(exact);
    assert_eq!(run_all(&mut d, &data), (Ok(()), 2 * 65_535));
    // 窗口少 1 字节：STREAMINFO 声明的最大帧长度先触发容量错误。
    let short = Capacity {
        input_window: frame - 1,
        ..Capacity::DESKTOP
    };
    let mut d = dec(short);
    assert_eq!(run_all(&mut d, &data).0, Err(DecodeError::CapacityExceeded));
    assert_eq!(d.last_error(), Some(FlacError::FrameTooLarge));
}

#[test]
fn unknown_or_lying_max_frame_still_bounded_by_window() {
    // 最大帧长度未知（0）或谎报为 100：元数据检查放行，实际帧超出窗口时返回容量错误。
    for name in ["unknown_max_frame.flac", "lying_max_frame.flac"] {
        let data = fixture(name);
        let mut ok = dec(Capacity::DESKTOP);
        assert_eq!(run_all(&mut ok, &data).0, Ok(()), "{name}");
        let small = Capacity {
            input_window: 1000,
            ..Capacity::DESKTOP
        };
        let mut d = dec(small);
        // 逐步喂入，模拟核心窗口：窗口满仍不完整 → 容量错误。
        let mut window = Vec::new();
        let mut fed = 0;
        let r = loop {
            match d.decode(&window, fed == data.len()) {
                Err(e) => break Err(e),
                Ok(p) => {
                    window.drain(..p.consumed);
                    match p.status {
                        Status::NeedInput => {
                            let n = (small.input_window - window.len())
                                .min(data.len() - fed)
                                .min(97);
                            window.extend_from_slice(&data[fed..fed + n]);
                            fed += n;
                        }
                        Status::End => break Ok(()),
                        _ => {
                            let ch = d.format().map_or(1, |f| usize::from(f.channels()));
                            d.release(d.pcm().len() / ch);
                        }
                    }
                }
            }
        };
        assert_eq!(r, Err(DecodeError::CapacityExceeded), "{name}");
        assert_eq!(d.last_error(), Some(FlacError::FrameTooLarge), "{name}");
    }
}

#[test]
fn streaminfo_block_larger_than_capacity_is_rejected() {
    let data = fixture("max_block_verbatim_32_stereo.flac");
    let mut d = dec(Capacity::MCU);
    assert_eq!(run_all(&mut d, &data).0, Err(DecodeError::CapacityExceeded));
    assert_eq!(d.last_error(), Some(FlacError::BlockTooLarge));
}

#[test]
fn eof_bypasses_threshold_for_complete_short_frame() {
    let data = fixture("short_tail_frame.flac");
    let mut d = dec(Capacity::DESKTOP);
    let ml = meta_len(&data);
    // 元数据与前两帧。
    let mut pos = 0;
    let mut frames = 0;
    while frames < 2 {
        let Ok(p) = d.decode(&data[pos..], false) else {
            panic!("decode")
        };
        pos += p.consumed;
        if let Status::Frames(_) = p.status {
            frames += 1;
            d.release(d.pcm().len());
        }
    }
    assert!(pos > ml);
    let tail = &data[pos..];
    assert!(tail.len() > 4);
    // 先给尾帧的大部分：不完整 → 门槛 = 2n。
    let part = tail.len() - 3;
    let Ok(p) = d.decode(&tail[..part], false) else {
        panic!("partial")
    };
    assert_eq!((p.consumed, p.status), (0, Status::NeedInput));
    assert_eq!(d.retry_at(), 2 * part);
    // 补齐后仍低于门槛：未 EOF 时不重新解析。
    let Ok(p) = d.decode(tail, false) else {
        panic!("below")
    };
    assert_eq!((p.consumed, p.status), (0, Status::NeedInput));
    // EOF：绕过门槛做最终解析，完整短帧成功。
    let Ok(p) = d.decode(tail, true) else {
        panic!("eof")
    };
    assert_eq!((p.consumed, p.status), (tail.len(), Status::Frames(17)));
    assert_eq!(d.retry_at(), 0, "成功后门槛归零");
    d.release(17);
    let Ok(p) = d.decode(&[], true) else {
        panic!("end")
    };
    assert_eq!(p.status, Status::End);
    // 截断的尾帧：EOF 时明确报错。
    let mut d = dec(Capacity::DESKTOP);
    let cut = &data[..data.len() - 2];
    assert_eq!(run_all(&mut d, cut).0, Err(DecodeError::Truncated));
}

#[test]
fn threshold_resets_between_consecutive_frames() {
    let data = fixture("cli_16_stereo_44k_default.flac");
    let mut d = dec(Capacity::DESKTOP);
    let ml = meta_len(&data);
    let Ok(p) = d.decode(&data[..ml], false) else {
        panic!("meta")
    };
    assert_eq!(p.consumed, ml);
    // 先给第一帧的 10 字节 → 门槛 20。
    let Ok(p) = d.decode(&data[ml..ml + 10], false) else {
        panic!()
    };
    assert_eq!(p.status, Status::NeedInput);
    assert_eq!(d.retry_at(), 20);
    // 一次给足：第一帧成功，门槛归零。
    let Ok(p) = d.decode(&data[ml..], false) else {
        panic!()
    };
    assert!(matches!(p.status, Status::Frames(_)));
    assert_eq!(d.retry_at(), 0);
    let f1 = p.consumed;
    d.release(d.pcm().len());
    // 第二帧只给 3 字节：门槛按新的 n 计算（6），不沿用旧值。
    let Ok(p) = d.decode(&data[ml + f1..ml + f1 + 3], false) else {
        panic!()
    };
    assert_eq!(p.status, Status::NeedInput);
    assert_eq!(d.retry_at(), 6);
}

#[test]
fn need_output_does_not_redecode_or_consume() {
    let data = fixture("cli_16_stereo_44k_default.flac");
    let mut d = dec(Capacity::DESKTOP);
    let ml = meta_len(&data);
    let Ok(p) = d.decode(&data[..ml], false) else {
        panic!()
    };
    assert_eq!(p.consumed, ml);
    let Ok(p) = d.decode(&data[ml..], false) else {
        panic!()
    };
    let Status::Frames(n) = p.status else {
        panic!("{p:?}")
    };
    let before = d.pcm().to_vec();
    for _ in 0..3 {
        let Ok(q) = d.decode(&data[ml + p.consumed..], false) else {
            panic!()
        };
        assert_eq!((q.consumed, q.status), (0, Status::NeedOutput));
    }
    assert_eq!(d.pcm(), &before[..], "待输出数据不得被重写");
    d.release(n / 2);
    assert_eq!(d.pcm(), &before[(n / 2) * 2..]);
    d.release(n);
    assert!(d.pcm().is_empty());
}

#[test]
fn incomplete_input_never_publishes_partial_audio() {
    let data = fixture("cli_24_mono_96k_lpc12.flac");
    let mut d = dec(Capacity::DESKTOP);
    let ml = meta_len(&data);
    let Ok(_) = d.decode(&data[..ml], false) else {
        panic!()
    };
    // 逐步增加输入但始终少于整帧：PCM 始终为空、格式已知、不 EOF。
    for n in [1usize, 5, 50, 500, 1500] {
        d.reset();
        let Ok(_) = d.decode(&data[..ml], false) else {
            panic!()
        };
        let Ok(p) = d.decode(&data[ml..ml + n], false) else {
            panic!()
        };
        if p.status == Status::NeedInput {
            assert!(d.pcm().is_empty(), "n={n}");
            assert_eq!(p.consumed, 0);
        }
    }
}
