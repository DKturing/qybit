//! F2：按文件头识别 AIFF / AIFF-C；WAV 与 AIFF 共用一块流式工作区，交替播放时结果与单独解码相同。
#![cfg(all(feature = "wav", feature = "flac", feature = "aiff"))]

use std::path::PathBuf;

use qy_codec_auto::{AutoDecoder, AutoInitError, Codec};
use qy_codec_flac::Capacity;
use qy_core::{DecodeError, Decoder, PCM_WORK_SAMPLES, Status};

fn leak<T: Copy + Default>(n: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); n].into_boxed_slice())
}

fn read(rel: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// `.expect` 中的样本（第 5 行之后；WAV 夹具的头部多两行，由 `skip` 给出）。
fn expect_samples(rel: &str, skip: usize) -> Vec<i32> {
    String::from_utf8(read(rel))
        .unwrap_or_default()
        .lines()
        .skip(skip)
        .flat_map(|l| {
            l.split(' ')
                .filter(|x| !x.is_empty())
                .map(|x| x.parse::<i32>().unwrap_or(i32::MIN))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn decode_all(d: &mut AutoDecoder, data: &[u8]) -> Result<Vec<i32>, DecodeError> {
    d.reset();
    let mut out = Vec::new();
    let mut pos = 0;
    let mut avail = 0;
    loop {
        let eof = pos + avail == data.len();
        let p = d.decode(&data[pos..pos + avail], eof)?;
        pos += p.consumed;
        avail -= p.consumed;
        match p.status {
            Status::Frames(n) => {
                let ch = d.format().map_or(1, |f| usize::from(f.channels()));
                out.extend_from_slice(&d.pcm()[..n * ch]);
                d.release(n);
            }
            Status::End => return Ok(out),
            // 7 字节一块：覆盖 FORM 头需要 12 字节才能识别的情形。
            Status::NeedInput => avail = (avail + 7).min(data.len() - pos),
            Status::NeedOutput => {}
        }
    }
}

#[test]
fn wav_and_aiff_share_one_stream_buffer() {
    let c = Capacity::MCU;
    let Ok(mut d) = AutoDecoder::new(
        leak(PCM_WORK_SAMPLES),
        c,
        leak(c.workspace_len()),
        leak(c.pcm_len()),
    ) else {
        panic!("decoder");
    };
    assert_eq!(d.stream_capacity(), PCM_WORK_SAMPLES);
    let aiff = read("../qy-codec-aiff/tests/fixtures/aiff_24_stereo_96k.aiff");
    let aiff_ref = expect_samples(
        "../qy-codec-aiff/tests/fixtures/aiff_24_stereo_96k.expect",
        4,
    );
    let aifc = read("../qy-codec-aiff/tests/fixtures/aifc_none_16_stereo.aiff");
    let aifc_ref = expect_samples(
        "../qy-codec-aiff/tests/fixtures/aifc_none_16_stereo.expect",
        4,
    );
    let wav = read("../qy-codec-wav/tests/fixtures/pcm16_stereo_44k.wav");
    let wav_ref = expect_samples("../qy-codec-wav/tests/fixtures/pcm16_stereo_44k.expect", 6);
    assert!(!aiff_ref.is_empty() && !wav_ref.is_empty() && !aifc_ref.is_empty());
    for (data, want, codec) in [
        (&wav, &wav_ref, Codec::Wav),
        (&aiff, &aiff_ref, Codec::Aiff),
        (&wav, &wav_ref, Codec::Wav),
        (&aifc, &aifc_ref, Codec::Aiff),
        (&aiff, &aiff_ref, Codec::Aiff),
    ] {
        assert_eq!(decode_all(&mut d, data).as_ref(), Ok(want), "{codec:?}");
        assert_eq!(d.codec(), codec);
        // 工作区只有一块，在两个解码器之间移交，容量不变。
        assert_eq!(d.stream_capacity(), PCM_WORK_SAMPLES);
        assert_eq!(d.wav().is_some(), codec == Codec::Wav);
        assert_eq!(d.aiff().is_some(), codec == Codec::Aiff);
    }
    // AIFF 拒绝明细经自动识别给出。
    let ext = read("../qy-codec-aiff/tests/fixtures/ext_ulaw.aiff");
    assert_eq!(decode_all(&mut d, &ext), Err(DecodeError::Unsupported));
    let detail = d.aiff_detail().map(|x| x.to_string()).unwrap_or_default();
    assert!(
        detail.contains("规范之外的扩展") && detail.contains("ulaw"),
        "{detail}"
    );
    // 其他 IFF 形式类型：无法识别，写明形式类型。
    let mut svx = b"FORM\0\0\0\x048SVX".to_vec();
    svx.extend_from_slice(&[0; 8]);
    assert_eq!(decode_all(&mut d, &svx), Err(DecodeError::Unsupported));
    assert_eq!(d.codec(), Codec::Unrecognized);
    let hint = d.unrecognized().map(|u| u.to_string()).unwrap_or_default();
    assert!(
        hint.contains("38 53 56 58") && hint.contains("不是 AIFF"),
        "{hint}"
    );
    // FORM 头残缺。
    assert_eq!(decode_all(&mut d, b"FORM\0\0"), Err(DecodeError::Truncated));
}

#[test]
fn aiff_needs_stream_buffer_and_can_be_left_disabled() {
    assert_eq!(
        AutoDecoder::empty().with_aiff().err(),
        Some(AutoInitError::NoStreamBuffer)
    );
    assert_eq!(
        AutoDecoder::empty().with_stream_pcm(leak(10)).err(),
        Some(AutoInitError::Stream(DecodeError::CapacityExceeded))
    );
    let aiff = read("../qy-codec-aiff/tests/fixtures/aiff_16_stereo_44k.aiff");
    let want = expect_samples(
        "../qy-codec-aiff/tests/fixtures/aiff_16_stereo_44k.expect",
        4,
    );
    // 只启用 WAV：AIFF 按“已识别但未启用”拒绝。
    let Ok(mut w) = AutoDecoder::empty().with_wav(leak(PCM_WORK_SAMPLES)) else {
        panic!("wav");
    };
    assert_eq!(decode_all(&mut w, &aiff), Err(DecodeError::Unsupported));
    assert_eq!(w.codec(), Codec::Disabled);
    // 只启用 AIFF（工作区由 with_stream_pcm 提供）。
    let Ok(mut a) = AutoDecoder::empty()
        .with_stream_pcm(leak(PCM_WORK_SAMPLES))
        .and_then(AutoDecoder::with_aiff)
    else {
        panic!("aiff");
    };
    assert_eq!(decode_all(&mut a, &aiff), Ok(want));
    let wav = read("../qy-codec-wav/tests/fixtures/pcm16_stereo_44k.wav");
    assert_eq!(decode_all(&mut a, &wav), Err(DecodeError::Unsupported));
    assert_eq!(a.codec(), Codec::Disabled);
}
