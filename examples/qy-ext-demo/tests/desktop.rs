//! 桌面场景：组合解码器经 `HostResources::with_decoder` 接入真实引擎、pack、帧队列与整包渲染
//! 循环，输出到按真实时间消费的无硬件后端。每个场景的设备字节都与独立参考逐字节核对。

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use qy_codec_auto::profile::DESKTOP;
use qy_ext_demo::qyt1::encode;
use qy_ext_demo::{ExtDecoder, MemorySource, Output, Recorder};
use qy_player::{
    Command, CommandSender, HostResources, PlayControl, PlaybackReport, Status, command_channel,
    play_with,
};

const ALL: [(u8, u8); 4] = [(16, 16), (24, 24), (32, 24), (32, 32)];

/// 独立参考：N 位样本放进 C 位小端左对齐容器（有效位 V = C 或 N ≤ V），取低 C/8 字节。
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
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates")
        .join(dir)
        .join("tests/fixtures")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}：{e}", p.display()))
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

fn resources(seekable: bool) -> HostResources<ExtDecoder> {
    let decoder = ExtDecoder::desktop(seekable);
    let window = decoder.required_window();
    match HostResources::with_decoder(decoder, window, DESKTOP) {
        Ok(r) => r,
        Err(e) => panic!("{e:?}"),
    }
}

type Seen = Arc<Mutex<Vec<Status>>>;

/// 播放；`on` 在每张状态快照时调用（可发送命令）。返回报告、输出与日志文字。
fn run(
    res: &mut HostResources<ExtDecoder>,
    tracks: Vec<(String, Vec<u8>)>,
    seekable_source: bool,
    mut on: impl FnMut(&Status, &CommandSender),
) -> (PlaybackReport<qy_ext_demo::Attempt>, Vec<Output>, String) {
    let mut backend = Recorder::new(&ALL);
    let mut source = MemorySource {
        tracks,
        seekable: seekable_source,
    };
    let (tx, rx) = command_channel();
    let seen: Seen = Arc::default();
    let mut lines = Vec::new();
    let report = play_with(
        &mut backend,
        res,
        &mut source,
        PlayControl {
            on_line: &mut |l: &str| lines.push(l.to_string()),
            stop: None,
            commands: Some(&rx),
            first: 0,
            on_status: Some(&mut |s: &Status| {
                seen.lock().unwrap_or_else(PoisonError::into_inner).push(*s);
                on(s, &tx);
            }),
        },
    );
    assert!(res.consumer.is_some(), "消费端须归还");
    (report, backend.outputs(), lines.join("\n"))
}

fn named(name: &str, bytes: Vec<u8>) -> (String, Vec<u8>) {
    (name.to_string(), bytes)
}

/// QYT1 与 WAV、FLAC 混排：同格式（44.1 kHz / 16 位双声道）的三首共用一个设备流，逐字节等于
/// 三份参考依次相接；48 kHz / 24 位的 QYT1 另开一个 24/24 的流。
#[test]
fn qyt1_mixed_with_wav_and_flac() {
    let s1 = samples(8_820, 2, 16, 1);
    let s2 = samples(9_600, 2, 24, 2);
    let wav = fixture("qy-codec-wav", "pcm16_stereo_44k.wav");
    let flac = fixture("qy-codec-flac", "cli_16_stereo_44k_default.flac");
    let tracks = vec![
        named("a.qyt1", encode(44_100, 2, 16, &s1)),
        named("b.wav", wav.clone()),
        named("c.flac", flac),
        named("d.qyt1", encode(48_000, 2, 24, &s2)),
    ];
    let mut res = resources(true);
    let (r, outs, _) = run(&mut res, tracks, true, |_, _| {});
    assert!(r.finished && r.software_path_intact(), "{}", r.summary());
    assert_eq!(outs.len(), 2, "{}", r.summary());
    let first = [
        place(&s1, 16, 16),
        data_chunk(&wav),
        place(&flac_reference("cli_16_stereo_44k_default"), 16, 16),
    ]
    .concat();
    assert_eq!(
        (
            outs[0].format.sample_rate(),
            outs[0].format.container_bits()
        ),
        (44_100, 16)
    );
    assert!(outs[0].bytes == first, "第一个流应为三份参考依次相接");
    assert_eq!(
        (
            outs[1].format.sample_rate(),
            outs[1].format.container_bits()
        ),
        (48_000, 24)
    );
    assert!(outs[1].bytes == place(&s2, 24, 24));
    assert!(
        outs.iter()
            .all(|o| o.report.as_ref().is_some_and(|r| r.underrun_events == 0))
    );
}

/// 暂停与恢复、切歌、停止：暂停只插入静音（不计入有效字节），恢复后的有效字节与参考连续；
/// 跳曲后的新流从目标曲目开头逐字节一致；停止立即生效并标记中断。
#[test]
fn pause_skip_and_stop() {
    // 每首 1 s：状态快照至多每 100 ms 一次（暂停时更少），命令在快照回调中发出，曲目要足够长，
    // 切歌命令才能在第 1 首播完之前到达。
    let s: Vec<Vec<i32>> = (0..3).map(|k| samples(44_100, 2, 16, 10 + k)).collect();
    let tracks: Vec<(String, Vec<u8>)> = s
        .iter()
        .enumerate()
        .map(|(k, x)| named(&format!("{k}.qyt1"), encode(44_100, 2, 16, x)))
        .collect();
    let tenth = 4_410;

    // 暂停 0.2 s 再恢复；之后跳到第 3 首。
    let mut res = resources(true);
    let mut stage = 0;
    let mut paused_at = None;
    let (r, outs, _) = run(&mut res, tracks.clone(), true, |st, tx| match stage {
        0 if st.position_frames >= tenth => {
            stage += usize::from(tx.send(Command::Pause).is_ok());
            paused_at = Some(Instant::now());
        }
        1 if st.paused && paused_at.is_some_and(|t| t.elapsed().as_millis() >= 200) => {
            stage += usize::from(tx.send(Command::Resume).is_ok());
        }
        2 if !st.paused && st.position_frames >= 2 * tenth => {
            stage += usize::from(tx.send(Command::Goto(2)).is_ok());
        }
        _ => {}
    });
    assert_eq!(stage, 3, "{}", r.summary());
    assert!(r.finished && r.user_skips == 1, "{}", r.summary());
    assert_eq!(outs.len(), 2);
    let first = place(&s[0], 16, 16);
    assert!(
        outs[0].bytes.len() >= 2 * tenth as usize * 4 && first.starts_with(&outs[0].bytes),
        "被切走的流应为第 1 首的开头（暂停的静音不计入）：{} 字节",
        outs[0].bytes.len()
    );
    assert!(outs[0].report.as_ref().is_some_and(|r| r.paused_frames > 0));
    assert!(outs[1].bytes == place(&s[2], 16, 16));

    // 停止：立即停止、不排空，报告标记中断。
    let mut res = resources(true);
    let mut sent = false;
    let (r, outs, _) = run(&mut res, tracks, true, |st, tx| {
        if !sent && st.position_frames >= tenth {
            sent = tx.send(Command::Stop).is_ok();
        }
    });
    assert!(r.interrupted && !r.finished, "{}", r.summary());
    assert_eq!(outs.len(), 1);
    assert!(place(&s[0], 16, 16).starts_with(&outs[0].bytes));
}

/// 定位：解码器支持定位时按字节直接定位；解码器不支持或音源不可定位时，核心顺序解码到目标。
/// 三种情形下定位后的新流都从目标帧起与参考逐字节一致。
#[test]
fn seek_direct_and_sequential() {
    let s = samples(44_100, 2, 16, 77);
    let target = 13_000u64;
    let want = place(&s[target as usize * 2..], 16, 16);
    for (decoder_seeks, source_seeks) in [(true, true), (false, true), (true, false)] {
        let mut res = resources(decoder_seeks);
        let mut sent = false;
        let (r, outs, log) = run(
            &mut res,
            vec![named("s.qyt1", encode(44_100, 2, 16, &s))],
            source_seeks,
            |st, tx| {
                if !sent && st.position_frames >= 4_410 {
                    sent = tx.send(Command::Seek(target)).is_ok();
                }
            },
        );
        let what = format!("解码器定位 {decoder_seeks}、音源定位 {source_seeks}");
        assert!(r.finished && r.user_seeks == 1, "{what}：{}", r.summary());
        assert_eq!(outs.len(), 2, "{what}");
        assert!(
            outs[1].bytes == want,
            "{what}：定位后应从目标帧起逐字节一致"
        );
        let sequential = !(decoder_seeks && source_seeks);
        assert_eq!(r.sequential_seeks.is_empty(), !sequential, "{what}");
        assert_eq!(log.contains("按顺序解码"), sequential, "{what}：{log}");
    }
}

/// 解码错误：桌面配置出错即停止（`ErrorPolicy::Stop`），坏样本之前的有效音频照常排空播放，
/// 报告写明 QYT1 的错误明细（组合解码器经 `Diagnostics` 提供）。
#[test]
fn decode_error_reports_detail() {
    let good = samples(4_410, 2, 16, 5);
    // 12 位文件：每个样本占 2 字节，把第 1000 帧左声道的高字节改成 0x7F 即超出 12 位范围。
    let s12 = samples(4_410, 2, 12, 6);
    let mut bad = encode(44_100, 2, 12, &s12);
    let at = 20 + 1_000 * 4 + 1;
    bad[at] = 0x7F;
    let mut res = resources(true);
    let (r, outs, log) = run(
        &mut res,
        vec![
            named("good.qyt1", encode(44_100, 2, 16, &good)),
            named("bad.qyt1", bad),
        ],
        true,
        |_, _| {},
    );
    assert!(!r.finished && r.fault.is_some(), "{}", r.summary());
    assert!(
        log.contains("QYT1 错误明细：第 1000 帧的样本超出声明位数"),
        "{log}"
    );
    // 第 1 首完整；第 2 首（12 位 → 16/16）只到坏样本之前的 1000 帧。
    let played: Vec<u8> = outs.iter().flat_map(|o| o.bytes.clone()).collect();
    let want = [place(&good, 16, 16), place(&s12[..2_000], 12, 16)].concat();
    assert!(played == want, "出错前的有效音频应完整播放");
}
