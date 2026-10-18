//! F4：超过 4 GiB 的 RF64 源（tasks/F4.md §3、§4.3）。
//!
//! 源字节按需生成，不落盘：8 位单声道，data 长 2^32 + 1_000_003 字节（帧数超过 `u32::MAX`），
//! 第 `i` 帧的字节为 `i mod 251`（解码后为该值 − 128）。先解码文件头与开头若干帧，再定位到 4 GiB
//! 边界之前，一直解码到源末尾：逐帧核对样本、总帧数与正常结束；定位到末尾之后返回 `PastEnd`。

use qy_codec_wav::WavDecoder;
use qy_core::{Decoder, PCM_WORK_SAMPLES, SeekError, Status};

const FRAMES: u64 = (1 << 32) + 1_000_003;

fn header() -> Vec<u8> {
    let mut h = b"RF64\xff\xff\xff\xffWAVE".to_vec();
    // ds64：riffSize、dataSize、sampleCount、tableLength = 0。
    let fmt_len = 8 + 16;
    let riff = 4 + (8 + 28) + fmt_len + 8 + FRAMES + (FRAMES & 1);
    h.extend_from_slice(b"ds64");
    h.extend_from_slice(&28u32.to_le_bytes());
    h.extend_from_slice(&riff.to_le_bytes());
    h.extend_from_slice(&FRAMES.to_le_bytes());
    h.extend_from_slice(&FRAMES.to_le_bytes());
    h.extend_from_slice(&0u32.to_le_bytes());
    // fmt：PCM 8 位单声道 8 kHz。
    h.extend_from_slice(b"fmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes());
    h.extend_from_slice(&8_000u32.to_le_bytes());
    h.extend_from_slice(&8_000u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes());
    h.extend_from_slice(&8u16.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&u32::MAX.to_le_bytes());
    h
}

/// 源中 `at` 起的 `n` 个字节。
fn source(head: &[u8], at: u64, n: usize, out: &mut Vec<u8>) {
    let total = head.len() as u64 + FRAMES + (FRAMES & 1);
    let end = (at + n as u64).min(total);
    for off in at..end {
        out.push(match usize::try_from(off).ok().and_then(|i| head.get(i)) {
            Some(&b) => b,
            None => {
                let frame = off - head.len() as u64;
                if frame < FRAMES {
                    (frame % 251) as u8
                } else {
                    0
                }
            }
        });
    }
}

#[test]
fn rf64_beyond_4gib_seek_and_decode_to_end() {
    let pcm = Box::leak(vec![0i32; PCM_WORK_SAMPLES].into_boxed_slice());
    let mut d = WavDecoder::new(pcm).unwrap_or_else(|e| panic!("{e:?}"));
    let head = header();
    let total = head.len() as u64 + FRAMES + (FRAMES & 1);

    // 1. 文件头与开头的帧。
    let mut window = Vec::new();
    source(&head, 0, 4096, &mut window);

    let p = d.decode(&window, false).unwrap_or_else(|e| panic!("{e:?}"));
    assert!(matches!(p.status, Status::Frames(_)), "{:?}", p.status);
    assert_eq!(d.declared_frames(), Some(FRAMES));
    assert_eq!(d.pcm()[0], -128);
    assert_eq!(d.pcm()[300], (300 % 251) - 128);
    let n = d.pcm().len();
    d.release(n);

    // 2. 定位到末尾之后：PastEnd。
    assert_eq!(d.seek(FRAMES), Err(SeekError::PastEnd));

    // 3. 定位到 4 GiB 边界之前 300_000 帧，解码到源末尾。
    let start = (1u64 << 32) - 300_000;
    let at = d.seek(start).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(at, head.len() as u64 + start);
    window.clear();
    let mut fed = at;
    let mut frame = start;
    let mut ended = false;
    for _ in 0..1_000_000 {
        let eof = fed == total;
        let p = d
            .decode(&window, eof)
            .unwrap_or_else(|e| panic!("帧 {frame}: {e:?}"));
        window.drain(..p.consumed);
        match p.status {
            Status::Frames(_) | Status::NeedOutput => {
                for &s in d.pcm() {
                    assert_eq!(s, (frame % 251) as i32 - 128, "帧 {frame}");
                    frame += 1;
                }
                let n = d.pcm().len();
                d.release(n);
            }
            Status::NeedInput => {
                let before = window.len();
                source(&head, fed, 1 << 20, &mut window);
                fed += (window.len() - before) as u64;
            }
            Status::End => {
                ended = true;
                break;
            }
        }
    }
    assert!(ended, "没有正常结束");
    assert_eq!(frame, FRAMES, "总帧数");
}
