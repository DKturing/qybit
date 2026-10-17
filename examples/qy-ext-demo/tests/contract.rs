//! 示例解码器与组合解码器满足扩展契约（`qy-testkit`），并与独立生成的样本逐个相同。

use qy_core::DecodeError;
use qy_ext_demo::qyt1::{HEADER, encode};
use qy_ext_demo::{ExtDecoder, Qyt1Decoder, leak};
use qy_testkit::{Ending, SeekSupport, check, check_seek};

/// 确定性的 N 位样本（覆盖正负极值）。
fn samples(frames: usize, channels: usize, bits: u32, salt: u32) -> Vec<i32> {
    let lo = -(1i64 << (bits - 1));
    let hi = (1i64 << (bits - 1)) - 1;
    (0..frames * channels)
        .map(|i| {
            let x = (i as u32).wrapping_mul(2_654_435_761).wrapping_add(salt) as i64;
            match i % 97 {
                0 => lo,
                1 => hi,
                _ => lo + x.rem_euclid(hi - lo + 1),
            }
        })
        .map(|v| v as i32)
        .collect()
}

fn qyt1() -> Qyt1Decoder {
    let Ok(d) = Qyt1Decoder::new(leak(512)) else {
        panic!("工作区");
    };
    d
}

fn qyt1_no_seek() -> Qyt1Decoder {
    let Ok(d) = Qyt1Decoder::without_seek(leak(512)) else {
        panic!("工作区");
    };
    d
}

#[test]
fn qyt1_satisfies_the_contract_and_matches_its_samples() {
    let mut cases = Vec::new();
    for (bits, ch, frames) in [
        (16u16, 2u16, 1000usize),
        (24, 2, 700),
        (32, 1, 300),
        (1, 1, 50),
        (12, 2, 333),
        (8, 2, 0),
    ] {
        let s = samples(frames, usize::from(ch), u32::from(bits), u32::from(bits));
        cases.push((encode(44_100, ch, bits, &s), Some(s)));
    }
    // 截断、魔数错、头部非法、样本超出位数。
    let good = encode(48_000, 2, 16, &samples(10, 2, 16, 3));
    cases.push((good[..HEADER + 37].to_vec(), None));
    let mut bad = good.clone();
    bad[0] = b'X';
    cases.push((bad, None));
    let mut bad = good.clone();
    bad[10] = 33;
    cases.push((bad, None));
    let mut wide = encode(48_000, 1, 12, &[0, 1, 2, 3]);
    wide[HEADER + 5] = 0x7F; // 第 2 个样本的高字节：超出 12 位。
    cases.push((wide, None));
    let srcs: Vec<&[u8]> = cases.iter().map(|(b, _)| b.as_slice()).collect();
    let r = match check(qyt1, &srcs) {
        Ok(r) => r,
        Err(v) => panic!("{v}"),
    };
    for ((_, want), got) in cases.iter().zip(&r) {
        match want {
            Some(s) => {
                assert_eq!(got.ending, Ending::End);
                assert_eq!(&got.words, s, "样本应与生成值逐个相同");
            }
            None => assert!(matches!(got.ending, Ending::Error(_)), "{got:?}"),
        }
    }
    assert_eq!(r[6].ending, Ending::Error(DecodeError::Truncated));
    assert_eq!(r[7].ending, Ending::Error(DecodeError::Malformed));
    assert_eq!(r[8].ending, Ending::Error(DecodeError::Unsupported));
    assert_eq!(r[9].ending, Ending::Error(DecodeError::Malformed));
    // 定位：支持时按字节直接定位；不支持时核心顺序解码。
    let src = &cases[0].0;
    let targets = [0, 1, 500, 999, 1000, 5000];
    assert_eq!(check_seek(qyt1, src, &targets), Ok(SeekSupport::Supported));
    assert_eq!(
        check_seek(qyt1_no_seek, src, &targets),
        Ok(SeekSupport::Unsupported)
    );
}

fn fixture(dir: &str, name: &str) -> Vec<u8> {
    let p = format!(
        "{}/../../crates/{dir}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&p).unwrap_or_else(|e| panic!("{p}：{e}"))
}

#[test]
fn composite_decoder_satisfies_the_contract() {
    let q = encode(44_100, 2, 16, &samples(800, 2, 16, 9));
    let wav = fixture("qy-codec-wav", "pcm16_stereo_44k.wav");
    let flac = fixture("qy-codec-flac", "cli_16_stereo_44k_default.flac");
    let aiff = fixture("qy-codec-aiff", "aiff_24_stereo_96k.aiff");
    let short = b"QY".to_vec();
    let srcs: Vec<&[u8]> = vec![&q, &wav, &flac, &aiff, &short, &q];
    let r = match check(|| ExtDecoder::desktop(true), &srcs) {
        Ok(r) => r,
        Err(v) => panic!("{v}"),
    };
    assert_eq!(r[0], r[5]);
    assert!(r[..4].iter().all(|d| d.ending == Ending::End));
    assert!(matches!(r[4].ending, Ending::Error(_)));
    for src in [&q, &wav, &flac] {
        assert_eq!(
            check_seek(|| ExtDecoder::desktop(true), src, &[0, 10, 299, 300]),
            Ok(SeekSupport::Supported)
        );
    }
}
