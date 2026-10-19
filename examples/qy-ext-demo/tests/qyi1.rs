//! E6：解码需要的读取跳转。示例格式 QYI1（索引在文件末尾，代表 M4A 这类 `moov` 在后的容器）：
//!
//! - 解码器契约、定位与识别一致性（`qy-testkit`），样本与生成时的样本逐个相同；
//! - 桌面真实引擎（可定位音源）：与 WAV、QYT1 混排、带 ID3v2 标签，送到设备的字节与参考逐字节一致，
//!   报告写明文件头与数据阶段的跳转次数；用户定位后从目标帧起一致；
//! - 不可定位音源：明确失败（`Fault::Jump(Unseekable)`），不重新打开，按策略继续下一首；
//! - 坏文件（索引截断、非法偏移、索引位置在文件末尾之后、包越界或重叠、包数超出容量）：明确错误与明细；
//! - 嵌入式虚拟时间：逐字节一致、跳转计数，不可定位存储明确拒绝，超出跳转预算时给出通知；
//! - 命令行 `qybit-ext decode` 与文件信息读取（`read_header`）：经同一套跳转规则读出相同样本与格式。

use std::cell::Cell;
use std::sync::{Arc, Mutex, PoisonError};

use qy_core::engine::{EngineFault, ErrorPolicy};
use qy_core::{DecodeError, Fault, Identify, JumpFault, SourceFormat};
use qy_ext_demo::embedded::{self, Tracks, build_with_loads, loads};
use qy_ext_demo::qyi1::{Packet, Qyi1Decoder, Qyi1Detail, encode, identify};
use qy_ext_demo::{ExtDecoder, ExtFormats, MemoryReader, MemorySource, Output, Recorder, leak};
use qy_host_embedded::Notice;
use qy_player::{
    Command, CommandSender, Formats, HostResources, PlayControl, PlaybackReport, Status,
    TrackSource, command_channel, play_with,
};
use qy_testkit::{Ending, SeekSupport, check, check_identify, check_seek};

fn samples(frames: usize, channels: usize, salt: i32) -> Vec<i16> {
    (0..frames * channels)
        .map(|i| ((i as i32).wrapping_mul(7_919).wrapping_add(salt) % 32_768 - 16_384) as i16)
        .collect()
}

fn le16(s: &[i16]) -> Vec<u8> {
    s.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn words(s: &[i16]) -> Vec<i32> {
    s.iter().map(|&v| i32::from(v)).collect()
}

/// 44.1 kHz 双声道，四个包：第 2、4 包之前有填充（数据阶段跳转），第 3 包紧接第 2 包。
fn tail_file(salt: i32) -> (Vec<u8>, Vec<i16>) {
    let s = samples(9_000, 2, salt);
    let (a, rest) = s.split_at(2_000 * 2);
    let (b, rest) = rest.split_at(3_000 * 2);
    let (c, d) = rest.split_at(1_500 * 2);
    let f = encode(44_100, 2, &[(5, a), (37, b), (0, c), (1_001, d)]);
    (f, s)
}

/// ID3v2.4 标签（10 字节标签头 + `n` 字节；长度为 syncsafe 整数，`n` < 128）。
fn id3(n: u8) -> Vec<u8> {
    assert!(n < 0x80);
    let mut v = b"ID3\x04\x00\x00\x00\x00\x00".to_vec();
    v.push(n);
    v.resize(10 + usize::from(n), 0);
    v
}

fn qyi1(table: usize) -> Qyi1Decoder {
    match Qyi1Decoder::new(leak(1024), leak::<Packet>(table)) {
        Ok(d) => d,
        Err(e) => panic!("{e:?}"),
    }
}

// ---- 契约与识别 ----

#[test]
fn contract_identify_and_reference() {
    let (good, ref_good) = tail_file(1);
    let mono_s = samples(777, 1, 2);
    let mono = encode(48_000, 1, &[(0, &mono_s)]);
    let mut truncated = good.clone();
    truncated.truncate(good.len() - 5); // 索引截断
    let mut past_end = mono.clone();
    past_end[4..12].copy_from_slice(&(mono.len() as u64 + 100).to_le_bytes()); // 索引位置在末尾之后
    let mut bad_offset = mono.clone();
    bad_offset[4..12].copy_from_slice(&5u64.to_le_bytes()); // 索引位置落在文件头内
    let mut overlap = good.clone();
    let index_at = u64::from_le_bytes(good[4..12].try_into().unwrap_or([0; 8])) as usize;
    let second = index_at + 16 + 12; // 第二个包的位置字段
    overlap[second..second + 8].copy_from_slice(&20u64.to_le_bytes()); // 与第一包重叠
    let mut bad_magic = good.clone();
    bad_magic[index_at] = b'X';
    let srcs: Vec<&[u8]> = vec![
        &good,
        &mono,
        &truncated,
        &past_end,
        &bad_offset,
        &overlap,
        &bad_magic,
    ];
    let r = match check(|| qyi1(64), &srcs) {
        Ok(r) => r,
        Err(v) => panic!("{v}"),
    };
    assert_eq!(r[0].ending, Ending::End);
    assert_eq!(r[0].words, words(&ref_good));
    assert_eq!(r[0].format, SourceFormat::new(44_100, 2, 16).ok());
    assert_eq!(r[1].words, words(&mono_s));
    for (i, e) in [
        (2, DecodeError::Truncated),
        (3, DecodeError::Truncated),
        (4, DecodeError::Malformed),
        (5, DecodeError::Malformed),
        (6, DecodeError::Malformed),
    ] {
        assert_eq!(r[i].ending, Ending::Error(e), "源 {i}");
        assert!(r[i].words.is_empty(), "源 {i}：格式未确定前不得产出样本");
    }
    // 明细与识别一致。
    let mut d = qyi1(64);
    let want = [
        None,
        None,
        Some(Qyi1Detail::IndexTruncated),
        Some(Qyi1Detail::IndexPastEnd {
            at: mono.len() as u64 + 100,
        }),
        Some(Qyi1Detail::IndexOffset { at: 5 }),
        Some(Qyi1Detail::PacketRange { index: 1 }),
        Some(Qyi1Detail::IndexMagic),
    ];
    for (src, want) in srcs.iter().zip(want) {
        match check_identify(identify, &mut d, Qyi1Decoder::last_detail, src) {
            Ok((Identify::Rejected { detail, .. }, _)) => assert_eq!(detail, want),
            Ok((Identify::Supported { .. }, _)) => assert_eq!(want, None),
            Ok(other) => panic!("{other:?}"),
            Err(v) => panic!("{v}"),
        }
    }
    // 前缀不含索引：已认领容器，`total` 远大于输入窗口。
    assert_eq!(
        identify(&good[..64], false),
        Identify::NeedMore {
            total: index_at as u64 + 16,
            container: true
        }
    );
    // 包数超过索引表容量：明确的容量错误。
    let r = match check(|| qyi1(2), &[&good]) {
        Ok(r) => r,
        Err(v) => panic!("{v}"),
    };
    assert_eq!(r[0].ending, Ending::Error(DecodeError::CapacityExceeded));
    // 定位：包内、包边界、最后一帧、长度。
    assert_eq!(
        check_seek(
            || qyi1(64),
            &good,
            &[0, 1, 1_999, 2_000, 4_999, 5_000, 6_500, 8_999, 9_000]
        ),
        Ok(SeekSupport::Supported)
    );
}

/// 组合解码器：路由见到 `container: true` 即转交；ID3v2 标签之后的跳转位置加回标签长度。
#[test]
fn ext_decoder_routes_and_offsets_jumps_by_tag() {
    let (f, s) = tail_file(3);
    let tagged = [id3(120), f.clone()].concat();
    let r = match check(|| ExtDecoder::desktop(true), &[&f, &tagged]) {
        Ok(r) => r,
        Err(v) => panic!("{v}"),
    };
    assert_eq!(r[0].words, words(&s));
    assert_eq!(r[1].words, words(&s));
    assert_eq!(
        check_seek(|| ExtDecoder::desktop(true), &tagged, &[0, 2_000, 7_777]),
        Ok(SeekSupport::Supported)
    );
}

// ---- 桌面真实引擎 ----

fn resources() -> HostResources<ExtDecoder> {
    let d = ExtDecoder::desktop(true);
    let window = d.required_window();
    match HostResources::with_decoder(d, window, qy_codec_auto::profile::DESKTOP) {
        Ok(r) => r,
        Err(e) => panic!("{e:?}"),
    }
}

/// 记录打开次数的音源（不重新打开的证明）。
struct Counted {
    inner: MemorySource,
    opens: Vec<Cell<u32>>,
}

impl TrackSource for Counted {
    type Reader = MemoryReader;
    fn count(&self) -> u32 {
        self.inner.count()
    }
    fn open(&mut self, track: u32) -> std::io::Result<MemoryReader> {
        if let Some(c) = self.opens.get(track as usize) {
            c.set(c.get() + 1);
        }
        self.inner.open(track)
    }
    fn name(&self, track: u32) -> String {
        self.inner.name(track)
    }
}

fn play(
    tracks: Vec<(String, Vec<u8>)>,
    seekable: bool,
    mut on: impl FnMut(&Status, &CommandSender),
) -> (PlaybackReport<qy_ext_demo::Attempt>, Vec<Output>, Vec<u32>) {
    let n = tracks.len();
    let mut source = Counted {
        inner: MemorySource { tracks, seekable },
        opens: (0..n).map(|_| Cell::new(0)).collect(),
    };
    let mut backend = Recorder::new(&[(16, 16), (24, 24), (32, 24), (32, 32)]);
    let mut res = resources();
    let (tx, rx) = command_channel();
    let seen: Arc<Mutex<Vec<Status>>> = Arc::default();
    let report = play_with(
        &mut backend,
        &mut res,
        &mut source,
        PlayControl {
            on_line: &mut |_: &str| {},
            stop: None,
            commands: Some(&rx),
            first: 0,
            on_status: Some(&mut |s: &Status| {
                seen.lock().unwrap_or_else(PoisonError::into_inner).push(*s);
                on(s, &tx);
            }),
        },
    );
    let opens = source.opens.iter().map(Cell::get).collect();
    (report, backend.outputs(), opens)
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

#[test]
fn desktop_engine_is_bit_exact_with_jumps_and_reuse() {
    let (a, sa) = tail_file(5);
    let (b, sb) = tail_file(6);
    let wav = fixture("qy-codec-wav", "pcm16_stereo_44k.wav");
    let tracks = vec![
        ("a.qyi1".into(), a),
        ("b.wav".into(), wav.clone()),
        ("c.qyi1".into(), [id3(50), b].concat()),
    ];
    let (r, outs, opens) = play(tracks, true, |_, _| {});
    assert!(r.finished && r.software_path_intact(), "{}", r.summary());
    assert_eq!(outs.len(), 1, "同格式三首共用一个设备流");
    let want = [le16(&sa), data_chunk(&wav), le16(&sb)].concat();
    assert!(outs[0].bytes == want, "逐字节");
    assert_eq!(r.parse_jumps, vec![(0, 2, 2), (2, 2, 2)]);
    let s = r.summary();
    assert!(
        s.contains("曲目 1 按解码器的要求跳转读取：文件头 2 次、数据 2 次"),
        "{s}"
    );
    assert_eq!(opens, vec![1, 1, 1]);
}

/// 桌面播放器的出错策略是停止（`qy-player`）：曲目明确失败，不重新打开，不送出任何数据。
/// 按策略跳到下一首见 [`embedded_virtual_time`]。
#[test]
fn desktop_unseekable_source_fails_without_reopening() {
    let (a, _) = tail_file(7);
    let wav = fixture("qy-codec-wav", "pcm16_stereo_44k.wav");
    let tracks = vec![("a.qyi1".into(), a), ("b.wav".into(), wav)];
    let (r, outs, opens) = play(tracks, false, |_, _| {});
    assert_eq!(
        r.fault,
        Some((0, EngineFault::Core(Fault::Jump(JumpFault::Unseekable)))),
        "{}",
        r.summary()
    );
    assert_eq!(opens, vec![1, 0], "不重新打开");
    assert!(r.summary().contains("需要随机读取"), "{}", r.summary());
    assert!(outs.iter().all(|o| o.bytes.is_empty()));
}

#[test]
fn desktop_bad_files_are_reported() {
    let s = samples(500, 2, 9);
    let ok = encode(44_100, 2, &[(0, &s)]);
    let mut past = ok.clone();
    past[4..12].copy_from_slice(&1_000_000u64.to_le_bytes());
    let mut trunc = ok.clone();
    trunc.truncate(ok.len() - 1);
    for (bytes, fault, text) in [
        (
            past,
            DecodeError::Truncated,
            "索引位置 1000000 在文件末尾或之后",
        ),
        (trunc, DecodeError::Truncated, "索引在文件末尾被截断"),
    ] {
        let (r, outs, _) = play(vec![("x.qyi1".into(), bytes)], true, |_, _| {});
        assert_eq!(
            r.fault,
            Some((0, EngineFault::Core(Fault::Decode(fault)))),
            "{}",
            r.summary()
        );
        assert!(outs.iter().all(|o| o.bytes.is_empty()));
        assert!(r.summary().contains(text), "{}", r.summary());
    }
}

/// 用户定位：文件头跳转之后交给解码器，定位后的新流从目标帧起与参考逐字节一致。
#[test]
fn desktop_user_seek_after_header_jumps() {
    let (a, s) = tail_file(11);
    let target = 6_000u64;
    let mut sent = false;
    let (r, outs, _) = play(vec![("a.qyi1".into(), a)], true, |st, tx| {
        if !sent && st.position_frames >= 1_000 {
            sent = tx.send(Command::Seek(target)).is_ok();
        }
    });
    assert!(r.finished && r.user_seeks == 1, "{}", r.summary());
    assert_eq!(outs.len(), 2);
    assert!(outs[1].bytes == le16(&s[target as usize * 2..]));
    // 两次打开（开始与定位）各有两次文件头跳转；定位之后只剩第 4 包之前的一次数据跳转。
    assert_eq!(r.parse_jumps.len(), 1);
    assert_eq!(r.parse_jumps[0].1, 4);
}

// ---- 嵌入式虚拟时间 ----

fn played(w: &embedded::World) -> Vec<u8> {
    w.segments.iter().flat_map(|s| s.data.clone()).collect()
}

#[test]
fn embedded_virtual_time() {
    let (a, s) = tail_file(13);
    let wav = fixture("qy-codec-wav", "pcm16_stereo_44k.wav");
    let tracks = || {
        vec![
            ("a.qyi1".to_string(), a.clone()),
            ("b.wav".to_string(), wav.clone()),
        ]
    };
    // 可定位存储：逐字节一致，跳转计数，不超预算。
    let (mut p, w) = match build_with_loads(
        Tracks::new(tracks(), true),
        embedded::decoder(true),
        ErrorPolicy::Skip,
        &loads(),
    ) {
        Ok(x) => x,
        Err(e) => panic!("{e:?}"),
    };
    assert_eq!(p.margins().header_jumps, embedded::QYI1_HEADER_JUMPS);
    assert!(p.play(0));
    let notices =
        embedded::run(&mut p, &w, 30_000_000, |_, _| {}).unwrap_or_else(|n| panic!("{n:?}"));
    assert!(notices.contains(&Notice::Finished), "{notices:?}");
    let st = p.status();
    assert_eq!(st.output.underruns, 0, "{st:?}");
    assert_eq!(st.counters.parse_jumps, 4);
    assert_eq!(st.counters.jump_budget_exceeded, 0);
    assert!(played(&w.borrow()) == [le16(&s), data_chunk(&wav)].concat());

    // 不可定位存储：明确拒绝（能力不足），不重开，下一首照常。
    let (mut p, w) = match build_with_loads(
        Tracks::new(tracks(), false),
        embedded::decoder(true),
        ErrorPolicy::Skip,
        &loads(),
    ) {
        Ok(x) => x,
        Err(e) => panic!("{e:?}"),
    };
    assert!(p.play(0));
    let notices =
        embedded::run(&mut p, &w, 30_000_000, |_, _| {}).unwrap_or_else(|n| panic!("{n:?}"));
    assert!(
        notices.contains(&Notice::Skipped {
            track: 0,
            fault: EngineFault::Core(Fault::Jump(JumpFault::Unseekable))
        }),
        "{notices:?}"
    );
    assert_eq!(
        notices
            .iter()
            .filter(|n| matches!(n, Notice::Opened { track: 0 }))
            .count(),
        1
    );
    assert_eq!(p.status().counters.unsupported, 1);
    assert!(played(&w.borrow()) == data_chunk(&wav));

    // 声明的文件头跳转预算只有 1 次：给出通知（并计数），播放照常。
    let mut tight = loads();
    tight[3].header_jumps = 1;
    let (mut p, w) = match build_with_loads(
        Tracks::new(tracks(), true),
        embedded::decoder(true),
        ErrorPolicy::Skip,
        &tight,
    ) {
        Ok(x) => x,
        Err(e) => panic!("{e:?}"),
    };
    assert!(p.play(0));
    let notices =
        embedded::run(&mut p, &w, 30_000_000, |_, _| {}).unwrap_or_else(|n| panic!("{n:?}"));
    assert!(
        notices.contains(&Notice::JumpBudgetExceeded {
            track: 0,
            budget: 1
        }),
        "{notices:?}"
    );
    assert_eq!(p.status().counters.jump_budget_exceeded, 1);
    let mut line = String::new();
    if let Err(e) = qy_host_embedded::player::write_notice(
        &mut line,
        &Notice::JumpBudgetExceeded {
            track: 0,
            budget: 1,
        },
    ) {
        panic!("{e}");
    }
    assert_eq!(
        line,
        "EVENT jump-budget-exceeded track=0 header_jumps>1 underrun-possible\n"
    );
}

// ---- 命令行与文件信息读取 ----

#[test]
fn cli_decode_and_probe_follow_jumps() {
    let (f, s) = tail_file(17);
    let dir = std::env::temp_dir().join(format!("qy-ext-e6-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        panic!("{e}");
    }
    let input = dir.join("a.qyi1");
    let out = dir.join("a.wav");
    if let Err(e) = std::fs::write(&input, [id3(30), f].concat()) {
        panic!("{e}");
    }
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_qybit-ext"))
        .args([
            "decode",
            input.to_str().unwrap_or_default(),
            out.to_str().unwrap_or_default(),
        ])
        .output()
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let wav = std::fs::read(&out).unwrap_or_else(|e| panic!("{e}"));
    assert!(data_chunk(&wav) == le16(&s));
    let mut probe = match ExtFormats.prober() {
        Ok(p) => p,
        Err(e) => panic!("{e}"),
    };
    let info = match probe.probe(&input) {
        Ok(i) => i,
        Err(e) => panic!("{e}"),
    };
    assert_eq!(info.label, "QYI1");
    assert_eq!(info.frames, Some(9_000));
    assert_eq!(
        info.format,
        SourceFormat::new(44_100, 2, 16).unwrap_or_else(|e| panic!("{e:?}"))
    );
    let _ = std::fs::remove_dir_all(&dir);
}
