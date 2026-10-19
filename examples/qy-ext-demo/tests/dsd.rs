//! D1：DSD 输出通道。示例 DSD 格式 QYD1（代表 Full 插件的 DSF / DFF 解码器）经真实引擎、pack、
//! 帧队列与整包渲染循环送到无硬件后端：
//! - 设备接受原生 `DSD_U32_BE` / DoP 时，送到设备的有效字节与独立参考逐字节一致（参考按传输规范
//!   逐位写出，不调用被测 pack）；
//! - 设备只接受 PCM 时明确拒绝，不转换；
//! - 报告写明 DSD 倍率与传输方式；定位按字直接定位；
//! - QYD1 满足解码器契约与识别一致性。

use std::sync::{Arc, Mutex, PoisonError};

use qy_core::{DopContainer, DsdTransport, DsdWord};
use qy_ext_demo::qyd1::{Qyd1Decoder, encode};
use qy_ext_demo::{MemorySource, Output, Recorder, leak};
use qy_player::{
    Command, CommandSender, HostResources, PlayControl, PlaybackReport, Status, command_channel,
    play_with,
};
use qy_testkit::{Ending, SeekSupport, check, check_identify, check_seek};

const DSD64: u32 = 2_822_400;

fn words(n: usize, seed: u64) -> Vec<i32> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u32 as i32
        })
        .collect()
}

/// 独立参考：按位列表写出设备字节（双声道，同顺序）。
fn reference(t: DsdTransport, w: &[i32], channels: usize) -> Vec<u8> {
    let bit = |c: usize, i: usize| -> u8 {
        let v = w[(i / 32) * channels + c] as u32;
        ((v >> (31 - (i % 32))) & 1) as u8
    };
    let byte = |c: usize, k: usize| -> u8 { (0..8).fold(0u8, |a, j| (a << 1) | bit(c, k * 8 + j)) };
    let total_bytes = w.len() / channels * 4;
    let mut out = Vec::new();
    match t {
        DsdTransport::Native(DsdWord::U32Be) => {
            for f in 0..total_bytes / 4 {
                for c in 0..channels {
                    out.extend((0..4).map(|k| byte(c, f * 4 + k)));
                }
            }
        }
        DsdTransport::Dop(DopContainer::S32LeMsb) => {
            for f in 0..total_bytes / 2 {
                let marker = if f % 2 == 0 { 0x05 } else { 0xFA };
                for c in 0..channels {
                    out.extend_from_slice(&[0, byte(c, f * 2 + 1), byte(c, f * 2), marker]);
                }
            }
        }
        other => panic!("测试未覆盖 {other:?}"),
    }
    out
}

fn resources() -> HostResources<Qyd1Decoder> {
    let Ok(d) = Qyd1Decoder::new(leak(qy_core::PCM_WORK_SAMPLES)) else {
        panic!("工作区");
    };
    match HostResources::with_decoder(
        d,
        qy_core::INPUT_WINDOW_BYTES,
        qy_codec_auto::profile::DESKTOP,
    ) {
        Ok(r) => r,
        Err(e) => panic!("{e:?}"),
    }
}

fn run(
    res: &mut HostResources<Qyd1Decoder>,
    backend: &mut Recorder,
    tracks: Vec<(String, Vec<u8>)>,
    mut on: impl FnMut(&Status, &CommandSender),
) -> (PlaybackReport<qy_ext_demo::Attempt>, Vec<Output>, String) {
    let mut source = MemorySource {
        tracks,
        seekable: true,
    };
    let (tx, rx) = command_channel();
    let seen: Arc<Mutex<Vec<Status>>> = Arc::default();
    let mut lines = Vec::new();
    let report = play_with(
        backend,
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
    (report, backend.outputs(), lines.join("\n"))
}

#[test]
fn native_and_dop_are_bit_exact() {
    let w = words(44_100 * 2, 3); // 0.5 秒 DSD64 双声道（字率 88.2 kHz）
    for (t, name) in [
        (DsdTransport::Native(DsdWord::U32Be), "原生 DSD_U32_BE"),
        (DsdTransport::Dop(DopContainer::S32LeMsb), "DoP"),
    ] {
        let mut backend = Recorder::new(&[(16, 16), (24, 24), (32, 24), (32, 32)]);
        backend.dsd = vec![t];
        let mut res = resources();
        let (r, outs, log) = run(
            &mut res,
            &mut backend,
            vec![("a.qyd1".into(), encode(DSD64, 2, &w))],
            |_, _| {},
        );
        assert!(r.finished && r.software_path_intact(), "{}", r.summary());
        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0].format.dsd_transport(), Some(t));
        assert!(outs[0].bytes == reference(t, &w, 2), "{name}：逐字节");
        let s = r.summary();
        assert!(s.contains("DSD64（2.8224 MHz）"), "{s}");
        assert!(s.contains(name), "{s}");
        assert!(log.contains("DSD64"), "{log}");
        if matches!(t, DsdTransport::Dop(_)) {
            assert!(
                s.contains("176400 Hz"),
                "DoP 写明设备以 PCM 176.4 kHz 打开：{s}"
            );
        }
    }
}

/// 设备只接受 PCM：明确拒绝，不把 DSD 转成 PCM，也不送出任何数据。
#[test]
fn pcm_only_device_rejects_dsd() {
    let w = words(4_410 * 2, 5);
    let mut backend = Recorder::new(&[(16, 16), (24, 24), (32, 24), (32, 32)]);
    let mut res = resources();
    let (r, outs, _) = run(
        &mut res,
        &mut backend,
        vec![("a.qyd1".into(), encode(DSD64, 2, &w))],
        |_, _| {},
    );
    assert!(outs.is_empty() || outs.iter().all(|o| o.bytes.is_empty()));
    assert!(!r.software_path_intact());
    assert!(r.summary().contains("不转为 PCM"), "{}", r.summary());
}

/// 定位：按字直接定位，定位后的新流从目标字起与参考逐字节一致。
#[test]
fn seek_is_word_exact() {
    let w = words(88_200 * 2, 9); // 1 秒
    let t = DsdTransport::Dop(DopContainer::S32LeMsb);
    let target = 40_000u64;
    let mut backend = Recorder::new(&[]);
    backend.dsd = vec![t];
    let mut res = resources();
    let mut sent = false;
    let (r, outs, _) = run(
        &mut res,
        &mut backend,
        vec![("a.qyd1".into(), encode(DSD64, 2, &w))],
        |st, tx| {
            if !sent && st.position_frames >= 8_820 {
                sent = tx.send(Command::Seek(target)).is_ok();
            }
        },
    );
    assert!(r.finished && r.user_seeks == 1, "{}", r.summary());
    assert_eq!(outs.len(), 2);
    assert!(outs[1].bytes == reference(t, &w[target as usize * 2..], 2));
}

#[test]
fn qyd1_contract_and_identify() {
    let good = encode(DSD64, 2, &words(500 * 2, 1));
    let mono = encode(DSD64 * 2, 1, &words(37, 2));
    let mut bad_rate = good.clone();
    bad_rate[4] = 1; // 位率不是 32 的倍数
    let srcs: Vec<&[u8]> = vec![&good, &mono, &good[..30], &bad_rate];
    let make = || {
        let Ok(d) = Qyd1Decoder::new(leak(64)) else {
            panic!("工作区");
        };
        d
    };
    let r = match check(make, &srcs) {
        Ok(r) => r,
        Err(v) => panic!("{v}"),
    };
    assert_eq!(r[0].ending, Ending::End);
    assert_eq!(r[0].frames(), 500);
    assert!(matches!(r[2].ending, Ending::Error(_)));
    assert_eq!(
        check_seek(make, &good, &[0, 1, 250, 499, 500]),
        Ok(SeekSupport::Supported)
    );
    let mut d = make();
    for src in &srcs {
        if let Err(v) = check_identify(
            qy_ext_demo::qyd1::identify,
            &mut d,
            Qyd1Decoder::last_detail,
            src,
        ) {
            panic!("{v}");
        }
    }
}
