//! 嵌入式场景（虚拟时间）：组合解码器经 `qy_host_embedded::Player` 接入真实引擎、pack、静态帧
//! 队列与喂数器，存储与 I²S 输出由示例实现。播出的字节与独立参考逐字节核对。

use qy_core::engine::ErrorPolicy;
use qy_ext_demo::embedded::{EmbeddedPlayer, Tracks, World, build, run};
use qy_ext_demo::qyt1::encode;
use qy_host_embedded::Notice;

/// 独立参考：N 位样本放进 C 位小端左对齐容器，取低 C/8 字节。
fn place(samples: &[i32], n: u8, c: u8) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|&s| {
            ((i64::from(s) << (c - n)) as u32).to_le_bytes()[..usize::from(c / 8)].to_vec()
        })
        .collect()
}

fn samples(frames: usize, channels: usize, bits: u32, salt: u32) -> Vec<i32> {
    let lo = -(1i64 << (bits - 1));
    let span = 1i64 << bits;
    (0..frames * channels)
        .map(|i| {
            let x = i64::from((i as u32).wrapping_mul(2_654_435_761).wrapping_add(salt));
            (lo + x.rem_euclid(span)) as i32
        })
        .collect()
}

fn fixture(dir: &str, name: &str) -> Vec<u8> {
    let p = format!(
        "{}/../../crates/{dir}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&p).unwrap_or_else(|e| panic!("{p}：{e}"))
}

/// WAV data 块原始字节（独立提取）。
fn data_chunk(wav: &[u8]) -> Vec<u8> {
    let mut i = 12;
    while i + 8 <= wav.len() {
        let size = u32::from_le_bytes([wav[i + 4], wav[i + 5], wav[i + 6], wav[i + 7]]) as usize;
        if &wav[i..i + 4] == b"data" {
            return wav[i + 8..(i + 8 + size).min(wav.len())].to_vec();
        }
        i += 8 + size + (size & 1);
    }
    Vec::new()
}

/// libFLAC 参考样本（交错右对齐 i32）。
fn flac_reference(stem: &str) -> Vec<i32> {
    fixture("qy-codec-flac", &format!("{stem}.s32le"))
        .chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn named(name: &str, bytes: Vec<u8>) -> (String, Vec<u8>) {
    (name.to_string(), bytes)
}

fn player(
    tracks: Vec<(String, Vec<u8>)>,
    storage_seeks: bool,
    decoder_seeks: bool,
    policy: ErrorPolicy,
) -> (EmbeddedPlayer, qy_ext_demo::embedded::Shared) {
    match build(Tracks::new(tracks, storage_seeks), decoder_seeks, policy) {
        Ok(x) => x,
        Err(e) => panic!("预算：{e:?}"),
    }
}

/// 全部输出段播出的有效字节依次相接。
fn played(w: &World) -> Vec<u8> {
    w.segments.iter().flat_map(|s| s.data.clone()).collect()
}

fn no_underrun(p: &EmbeddedPlayer) {
    let s = p.status();
    assert_eq!(s.output.underruns, 0, "{s:?}");
    assert_eq!(s.counters.protocol_errors, 0, "{s:?}");
}

/// QYT1 与 WAV、FLAC 混排，坏的 QYT1 按出错策略跳过：44.1 kHz / 16 位双声道的曲目共用一个
/// 16/16 设备流（坏曲目只播出坏样本之前的帧），48 kHz / 24 位的 QYT1 另开 32/24 设备流。
#[test]
fn mixed_playlist_with_skipped_error() {
    let q16 = samples(8_820, 2, 16, 1);
    let s12 = samples(4_410, 2, 12, 2);
    let mut bad = encode(44_100, 2, 12, &s12);
    bad[20 + 1_000 * 4 + 1] = 0x7F; // 第 1000 帧左声道超出 12 位。
    let wav = fixture("qy-codec-wav", "pcm16_stereo_44k.wav");
    let flac = fixture("qy-codec-flac", "cli_16_stereo_44k_default.flac");
    let q24 = samples(9_600, 2, 24, 3);
    let tracks = vec![
        named("a.qyt1", encode(44_100, 2, 16, &q16)),
        named("bad.qyt1", bad),
        named("b.wav", wav.clone()),
        named("c.flac", flac),
        named("d.qyt1", encode(48_000, 2, 24, &q24)),
    ];
    let (mut p, w) = player(tracks, true, true, ErrorPolicy::Skip);
    assert!(p.play(0));
    let notices = match run(&mut p, &w, 30_000_000, |_, _| {}) {
        Ok(n) => n,
        Err(n) => panic!("超时：{n:?}"),
    };
    assert!(
        notices
            .iter()
            .any(|n| matches!(n, Notice::Skipped { track: 1, .. })),
        "{notices:?}"
    );
    assert!(notices.contains(&Notice::Finished), "{notices:?}");
    assert_eq!(p.status().counters.decode_errors, 1);
    no_underrun(&p);
    let w = w.borrow();
    let segs: Vec<_> = w.segments.iter().filter(|s| !s.data.is_empty()).collect();
    assert_eq!(segs.len(), 2);
    let first = [
        place(&q16, 16, 16),
        place(&s12[..2_000], 12, 16),
        data_chunk(&wav),
        place(&flac_reference("cli_16_stereo_44k_default"), 16, 16),
    ]
    .concat();
    assert_eq!(
        (
            segs[0].format.sample_rate(),
            segs[0].format.container_bits()
        ),
        (44_100, 16)
    );
    assert!(segs[0].data == first, "第一个流应为四份参考依次相接");
    assert_eq!(
        (
            segs[1].format.sample_rate(),
            segs[1].format.container_bits(),
            segs[1].format.valid_bits()
        ),
        (48_000, 32, 24)
    );
    assert!(segs[1].data == place(&q24, 24, 32));
}

/// 暂停与恢复、切歌、停止：暂停期间只输出静音（单独计数），恢复后有效字节与参考连续；切到下一首后
/// 播出的是被切曲目的开头加后面两首的完整内容；停止立即生效，不发出“播放结束”。
#[test]
fn pause_next_and_stop() {
    let s: Vec<Vec<i32>> = (0..3).map(|k| samples(13_230, 2, 16, 10 + k)).collect();
    let tracks: Vec<(String, Vec<u8>)> = s
        .iter()
        .enumerate()
        .map(|(k, x)| named(&format!("{k}.qyt1"), encode(44_100, 2, 16, x)))
        .collect();
    let tenth = 4_410 * 4;

    let (mut p, w) = player(tracks.clone(), true, true, ErrorPolicy::Stop);
    assert!(p.play(0));
    let mut stage = 0;
    let mut paused_at = 0;
    let r = run(&mut p, &w, 30_000_000, |p, w| match stage {
        0 if w.played_bytes >= tenth => {
            p.pause();
            paused_at = w.now_us;
            stage = 1;
        }
        1 if w.now_us >= paused_at + 200_000 => {
            p.resume();
            stage = 2;
        }
        2 if w.played_bytes >= 2 * tenth => {
            p.next();
            stage = 3;
        }
        _ => {}
    });
    let notices = r.unwrap_or_else(|n| panic!("超时：{n:?}"));
    assert_eq!(stage, 3);
    assert!(notices.contains(&Notice::Finished), "{notices:?}");
    let st = p.status();
    assert!(st.output.paused_fills > 0, "{st:?}");
    no_underrun(&p);
    let out = played(&w.borrow());
    let tail = [place(&s[1], 16, 16), place(&s[2], 16, 16)].concat();
    assert!(out.len() >= 2 * tenth + tail.len() && out.ends_with(&tail));
    let head = &out[..out.len() - tail.len()];
    assert!(
        place(&s[0], 16, 16).starts_with(head),
        "被切走的曲目应只播出它的开头（暂停的静音不计入有效字节）"
    );

    // 停止。
    let (mut p, w) = player(tracks, true, true, ErrorPolicy::Stop);
    assert!(p.play(0));
    let r = run(&mut p, &w, 30_000_000, |p, w| {
        if w.played_bytes >= tenth && p.status().active {
            p.stop();
        }
    });
    let notices = r.unwrap_or_else(|n| panic!("超时：{n:?}"));
    assert!(!notices.contains(&Notice::Finished), "{notices:?}");
    let out = played(&w.borrow());
    assert!(out.len() >= tenth && out.len() < place(&s[0], 16, 16).len());
    assert!(place(&s[0], 16, 16).starts_with(&out));
}

/// 定位：解码器与存储都支持时直接定位；任一方不支持时引擎顺序解码到目标（发出 `SeekSequential`）。
/// 定位后播出的字节都从目标帧起与参考逐字节一致。
#[test]
fn seek_direct_and_sequential() {
    let s = samples(26_460, 2, 16, 77);
    // 300 ms × 44.1 kHz。
    let target = 13_230usize;
    let want = place(&s[target * 2..], 16, 16);
    let all = place(&s, 16, 16);
    for (decoder_seeks, storage_seeks) in [(true, true), (false, true), (true, false)] {
        let what = format!("解码器定位 {decoder_seeks}、存储定位 {storage_seeks}");
        let tracks = vec![named("s.qyt1", encode(44_100, 2, 16, &s))];
        let (mut p, w) = player(tracks, storage_seeks, decoder_seeks, ErrorPolicy::Stop);
        assert!(p.play(0));
        let mut sent = false;
        let mut reply = String::new();
        let r = run(&mut p, &w, 30_000_000, |p, w| {
            if !sent && w.played_bytes >= 4_410 * 4 {
                sent = true;
                let _ = p.command(qy_host_embedded::Command::Seek(300), &mut reply);
            }
        });
        let notices = r.unwrap_or_else(|n| panic!("{what}：超时 {n:?}"));
        assert!(reply.starts_with("OK seek"), "{what}：{reply}");
        assert_eq!(
            notices.contains(&Notice::SeekSequential { track: 0 }),
            !(decoder_seeks && storage_seeks),
            "{what}：{notices:?}"
        );
        no_underrun(&p);
        let out = played(&w.borrow());
        assert!(out.len() > want.len() && out.ends_with(&want), "{what}");
        assert!(
            all.starts_with(&out[..out.len() - want.len()]),
            "{what}：定位前播出的应是曲目开头"
        );
    }
}
