//! 文件开头的 ID3v2 标签：跳过后与无标签文件逐样本一致；任意输入分块；标签头不合规则、
//! 标签超出文件与标签后不是支持的容器时明确拒绝。
//!
//! 标签按 ID3v2.4.0 结构规范 §3.1 在测试内独立构造（syncsafe 长度、v2.4 标签尾），期望样本为
//! FLAC 夹具的 libFLAC 参考 `.s32le` 与 WAV 夹具的 `.expect`。

#![cfg(all(feature = "wav", feature = "flac"))]

use std::path::PathBuf;

use qy_codec_auto::{AutoDecoder, Codec};
use qy_codec_flac::Capacity;
use qy_core::{DecodeError, Decoder, PCM_WORK_SAMPLES, Status};

fn leak<T: Copy + Default>(n: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); n].into_boxed_slice())
}

fn auto() -> AutoDecoder {
    let c = Capacity::DESKTOP;
    match AutoDecoder::new(
        leak(PCM_WORK_SAMPLES),
        c,
        leak(c.workspace_len()),
        leak(c.pcm_len()),
    ) {
        Ok(d) => d,
        Err(e) => panic!("{e:?}"),
    }
}

fn read(rel: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// ID3v2 标签：`major` 版本，`body` 字节的帧区（声明长度 = body 长度），`footer` 为 v2.4 标签尾。
fn id3(major: u8, body: usize, footer: bool) -> Vec<u8> {
    let n = u32::try_from(body).unwrap_or(0);
    assert!(n < 1 << 28);
    let flags = if footer { 0x10 } else { 0 };
    let size = [
        (n >> 21) as u8 & 0x7F,
        (n >> 14) as u8 & 0x7F,
        (n >> 7) as u8 & 0x7F,
        n as u8 & 0x7F,
    ];
    let mut t = vec![b'I', b'D', b'3', major, 0, flags];
    t.extend_from_slice(&size);
    // 帧区：一个 TIT2 帧后接填充；内容不影响跳过，只要求长度准确。其中故意含 "fLaC"/"RIFF"。
    let mut b = b"TIT2\0\0\0\x09\0\0\x03fLaC RIFF".to_vec();
    b.resize(body, 0);
    t.extend_from_slice(&b[..body]);
    if footer {
        t.extend_from_slice(&[b'3', b'D', b'I', major, 0, flags]);
        t.extend_from_slice(&size);
    }
    t
}

/// 按 `chunks` 循环给出的块长逐步喂入（模拟核心的输入窗口：未消耗的字节保留在窗口前部）。
fn decode_all(d: &mut AutoDecoder, data: &[u8], chunks: &[usize]) -> Result<Vec<i32>, DecodeError> {
    d.reset();
    let mut out = Vec::new();
    let (mut pos, mut avail, mut k) = (0usize, 0usize, 0usize);
    let window = d.required_window();
    for _ in 0..10_000_000 {
        let eof = pos + avail == data.len();
        let p = d.decode(&data[pos..pos + avail], eof)?;
        assert!(p.consumed <= avail);
        pos += p.consumed;
        avail -= p.consumed;
        match p.status {
            Status::Frames(n) => {
                let ch = d.format().map_or(0, |f| usize::from(f.channels()));
                out.extend_from_slice(&d.pcm()[..n * ch]);
                d.release(n);
            }
            Status::End => return Ok(out),
            Status::NeedInput => {
                assert!(!eof, "EOF 时不得返回 NeedInput");
                let step = chunks[k % chunks.len()];
                k += 1;
                let room = window - avail;
                assert!(room > 0 || p.consumed > 0, "窗口满仍缺输入");
                avail += step.min(room).min(data.len() - pos - avail);
            }
            Status::NeedOutput => {}
        }
    }
    panic!("没有结束");
}

const CHUNKS: [&[usize]; 5] = [&[1], &[3, 1, 7], &[9, 10, 11], &[4096], &[usize::MAX]];

#[test]
fn tagged_flac_and_wav_decode_like_untagged() {
    let flac = read("../qy-codec-flac/tests/fixtures/cli_16_stereo_44k_default.flac");
    let flac_ref: Vec<i32> =
        read("../qy-codec-flac/tests/fixtures/cli_16_stereo_44k_default.s32le")
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
    let wav = read("../qy-codec-wav/tests/fixtures/pcm24_mono_48k_odd_list.wav");
    // 按行解析（`lines` 同时去掉 CRLF：Windows 检出时 .expect 可能为 CRLF 行尾）。
    let wav_ref: Vec<i32> = String::from_utf8(read(
        "../qy-codec-wav/tests/fixtures/pcm24_mono_48k_odd_list.expect",
    ))
    .unwrap_or_default()
    .lines()
    .skip(6)
    .flat_map(|l| {
        l.split(' ')
            .filter(|x| !x.is_empty())
            .map(|x| x.parse::<i32>().unwrap_or(i32::MIN))
            .collect::<Vec<_>>()
    })
    .collect();
    assert!(!flac_ref.is_empty() && !wav_ref.is_empty());
    let tags: [(&str, Vec<u8>); 5] = [
        ("无标签", Vec::new()),
        ("v2.3", id3(3, 1000, false)),
        ("v2.4 + 标签尾", id3(4, 300, true)),
        ("v2.4 声明长度 0", id3(4, 0, false)),
        // 连续两个标签（libFLAC 同样逐个跳过）；第二个大于 WAV 的 8 KiB 最小窗口。
        (
            "两个标签",
            [id3(3, 17, false), id3(4, 200_000, false)].concat(),
        ),
    ];
    let mut d = auto();
    for (src, reference, codec) in [
        (&flac, &flac_ref, Codec::Flac),
        (&wav, &wav_ref, Codec::Wav),
    ] {
        for (what, tag) in &tags {
            let file = [tag.as_slice(), src].concat();
            for chunks in CHUNKS {
                let got = decode_all(&mut d, &file, chunks);
                assert!(
                    got.as_ref() == Ok(reference),
                    "{codec:?} {what} {chunks:?}: {:?}",
                    got.map(|g| g.len())
                );
                assert_eq!(d.codec(), codec);
                assert_eq!(d.skipped_tag_bytes(), tag.len() as u64, "{what}");
            }
        }
    }
}

#[test]
fn bad_or_truncated_tags_are_rejected() {
    let flac = read("../qy-codec-flac/tests/fixtures/cli_16_stereo_44k_default.flac");
    let mut d = auto();
    // syncsafe 长度字节最高位为 1：标签头不合规则，按无法识别拒绝并说明。
    let mut bad = id3(3, 10, false);
    bad[7] = 0x80;
    bad.extend_from_slice(&flac);
    assert_eq!(
        decode_all(&mut d, &bad, &[usize::MAX]),
        Err(DecodeError::Unsupported)
    );
    assert_eq!(d.codec(), Codec::Unrecognized);
    let hint = d.unrecognized().map(|u| u.to_string()).unwrap_or_default();
    assert!(
        hint.contains("49 44 33 03") && hint.contains("不合规则"),
        "{hint}"
    );
    // 版本字节为 FF 同样不合规则。
    let mut bad = id3(3, 10, false);
    bad[3] = 0xFF;
    assert_eq!(
        decode_all(&mut d, &bad, &[1]),
        Err(DecodeError::Unsupported)
    );
    // 声明长度超出文件：截断。
    let mut cut = id3(3, 5000, false);
    cut.truncate(3000);
    for chunks in CHUNKS {
        assert_eq!(
            decode_all(&mut d, &cut, chunks),
            Err(DecodeError::Truncated),
            "{chunks:?}"
        );
    }
    // 标签头本身残缺。
    assert_eq!(
        decode_all(&mut d, b"ID3\x03\0", &[1]),
        Err(DecodeError::Truncated)
    );
    assert_eq!(decode_all(&mut d, b"ID", &[1]), Err(DecodeError::Truncated));
    // 标签之后是 MPEG 音频帧（MP3）：说明中写明跳过的标签与其后的字节。
    let mp3 = [id3(4, 64, false), vec![0xFF, 0xFB, 0x90, 0x64, 0, 0, 0, 0]].concat();
    for chunks in CHUNKS {
        assert_eq!(
            decode_all(&mut d, &mp3, chunks),
            Err(DecodeError::Unsupported)
        );
        let hint = d.unrecognized().map(|u| u.to_string()).unwrap_or_default();
        assert!(
            hint.contains("跳过开头 74 字节的 ID3v2 标签")
                && hint.contains("FF FB 90 64")
                && hint.contains("MPEG"),
            "{hint}"
        );
    }
    // reset 清除标签状态。
    d.reset();
    assert_eq!(d.skipped_tag_bytes(), 0);
    assert_eq!(d.unrecognized(), None);
}
