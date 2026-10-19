//! 示例解码器与组合解码器满足扩展契约（`qy-testkit`），并与独立生成的样本逐个相同。

use qy_core::{DecodeError, Identify};
use qy_ext_demo::qyt1::{HEADER, encode};
use qy_ext_demo::{ExtDecoder, Qyt1Decoder, leak};
use qy_testkit::{Ending, SeekSupport, check, check_identify, check_seek};

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

/// E1：有损变体 QYL1 满足同样的契约；格式标为有损（同一个源内不变，`qy-testkit` 检查格式不变），
/// 其余字段与 QYT1 相同，样本与生成值逐个相同。
#[test]
fn qyl1_is_marked_lossy_and_satisfies_the_contract() {
    let s = samples(500, 2, 16, 7);
    let lossy = qy_ext_demo::qyt1::encode_lossy(44_100, 2, 16, &s);
    let plain = encode(44_100, 2, 16, &s);
    let r = match check(qyt1, &[lossy.as_slice(), plain.as_slice()]) {
        Ok(r) => r,
        Err(v) => panic!("{v}"),
    };
    let (Some(fl), Some(fp)) = (r[0].format, r[1].format) else {
        panic!("格式");
    };
    assert!(fl.is_lossy() && !fp.is_lossy());
    assert_eq!(fl, fp.lossy(), "除有损标记外格式相同");
    assert_eq!(r[0].words, s);
    assert_eq!(r[0].words, r[1].words);
    assert_eq!(
        check_seek(qyt1, &lossy, &[0, 1, 250, 499]),
        Ok(SeekSupport::Supported)
    );
}

/// E3：QYT1 自己的识别函数与完整解码一致（`qy_testkit::check_identify`，含每个前缀）。
#[test]
fn qyt1_identify_matches_full_decode() {
    let good = encode(48_000, 2, 16, &samples(40, 2, 16, 5));
    let mut magic = good.clone();
    magic[3] = b'X';
    let mut header = good.clone();
    header[10] = 40;
    let lossy = qy_ext_demo::qyt1::encode_lossy(8_000, 1, 8, &samples(9, 1, 8, 1));
    let cases: [(&[u8], &str); 6] = [
        (&good, "Supported"),
        (&lossy, "Supported"),
        (&good[..HEADER - 1], "Rejected"),
        (&header, "Rejected"),
        (&magic, "Foreign"),
        (b"RIFF", "Foreign"),
    ];
    let mut d = qyt1();
    for (bytes, want) in cases {
        let r = check_identify(
            qy_ext_demo::qyt1::identify,
            &mut d,
            Qyt1Decoder::last_detail,
            bytes,
        );
        let Ok((got, 0)) = r else {
            panic!("{want}：{r:?}");
        };
        let name = match got {
            Identify::Supported { .. } => "Supported",
            Identify::Rejected { .. } => "Rejected",
            Identify::Foreign => "Foreign",
            _ => "其他",
        };
        assert_eq!(name, want);
    }
}

/// 合规的 ID3v2.4 标签（ID3v2.4.0 结构规范 §3.1），`body` 字节的 0 填充。
fn id3(body: u32) -> Vec<u8> {
    let size = [21, 14, 7, 0].map(|s| (body >> s) as u8 & 0x7F);
    let mut t = vec![b'I', b'D', b'3', 4, 0, 0];
    t.extend_from_slice(&size);
    t.resize(10 + body as usize, 0);
    t
}

/// E3：组合解码器按识别结果路由，不缓存文件头。文件开头的 ID3v2 标签由路由层跳过（QYT1 与 WAV
/// 都可以带标签），样本与不带标签时相同，报告的跳过字节数为标签长度；定位位置加回标签长度。
#[test]
fn router_skips_id3_tags_without_caching_headers() {
    let s = samples(300, 2, 16, 11);
    let q = encode(44_100, 2, 16, &s);
    let wav = fixture("qy-codec-wav", "pcm16_stereo_44k.wav");
    let tag = id3(5_000);
    let tagged_q = [tag.as_slice(), &q].concat();
    let tagged_wav = [tag.as_slice(), &wav].concat();
    let srcs: Vec<&[u8]> = vec![&q, &tagged_q, &wav, &tagged_wav];
    let r = match check(|| ExtDecoder::desktop(true), &srcs) {
        Ok(r) => r,
        Err(v) => panic!("{v}"),
    };
    assert_eq!(r[0], r[1], "带标签的 QYT1 与不带时相同");
    assert_eq!(r[2], r[3], "带标签的 WAV 与不带时相同");
    assert_eq!(r[1].words, s);
    for (src, is_qyt1) in [(&tagged_q, true), (&tagged_wav, false)] {
        let mut d = ExtDecoder::desktop(true);
        if let Err(v) = qy_testkit::decode(&mut d, src, qy_testkit::Chunking::Bytewise) {
            panic!("{v}");
        }
        assert_eq!(d.is_qyt1(), is_qyt1);
        assert_eq!(
            qy_player::Diagnostics::skipped_tag_bytes(&d),
            tag.len() as u64
        );
        assert_eq!(
            check_seek(|| ExtDecoder::desktop(true), src, &[0, 7, 299]),
            Ok(SeekSupport::Supported)
        );
    }
}

/// E5：包模式（定位需要预滚）满足定位契约：全部目标（含包边界两侧、第一包内、最后一帧、越界）
/// 定位后的样本与从头解码的对应部分逐个相同；组合解码器转交预滚。
#[test]
fn packet_mode_seek_with_preroll() {
    let s = samples(5_000, 2, 16, 13);
    let src = encode(44_100, 2, 16, &s);
    let mut targets: Vec<u64> = vec![0, 1, 63, 64, 65, 127, 128, 129, 4_999, 5_000, 6_000];
    // 伪随机目标（固定种子）。
    let mut x = 0x9E37_79B9u64;
    for _ in 0..40 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        targets.push(x % 5_000);
    }
    let packets = || {
        let Ok(d) = Qyt1Decoder::with_packets(leak(512), 64) else {
            panic!("工作区");
        };
        d
    };
    assert_eq!(
        check_seek(packets, &src, &targets),
        Ok(SeekSupport::Supported)
    );
    assert_eq!(
        check_seek(|| ExtDecoder::desktop_packets(64), &src, &targets),
        Ok(SeekSupport::Supported)
    );
    let srcs: Vec<&[u8]> = vec![&src];
    if let Err(v) = check(packets, &srcs) {
        panic!("{v}");
    }
}
