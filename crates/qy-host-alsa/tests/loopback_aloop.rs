//! M2 T4 B 层：snd-aloop 内核虚拟声卡数字回环（仅 Linux，默认忽略）。
//!
//! 运行：`sudo modprobe snd-aloop`，确保 Loopback 声卡未被 PipeWire/PulseAudio 占用，然后
//! `scripts/aloop-loopback.sh`，或
//! `cargo test -p qy-host-alsa --test loopback_aloop -- --ignored --nocapture --test-threads=1`。
//! 可用 `QYBIT_ALOOP_CARD`（默认 `Loopback`）与 `QYBIT_ALOOP_SUBDEV`（默认 0）指定设备。
//!
//! 每个用例先以与播放端相同的格式打开 `hw:<卡>,1,<子设备>` 回录，再经真实播放路径
//! （核心 → 自有解码 → pack → 短写渲染 → alsa-lib → 内核）向 `hw:<卡>,0,<子设备>` 播放
//! 同步曲目 + 待测曲目，按同步序列对齐后逐字节比对。N > V 的用例须在设备选择阶段被拒绝。
//!
//! 这是虚拟回环：经过 ALSA 内核路径，但不证明真实 DAC 输入样本逐位一致。
#![cfg(target_os = "linux")]

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{cases, device_bytes, make_tracks, report_dir, sync_samples, test_samples, write_wav};
use qy_host_alsa::AlsaBackend;
use qy_host_alsa::linux::RealOpener;
use qy_host_alsa::linux::capture::start_capture;
use qy_host_alsa::linux::priority::RtPriority;
use qy_host_alsa::loopback::{Layer, LoopCase, LoopEntry, LoopReport, Verdict, align_and_compare};
use qy_pcm_format::alsa::AlsaFormat;
use qy_player::{HostResources, play};

struct Devices {
    playback: String,
    capture: String,
}

fn devices() -> Devices {
    let card = std::env::var("QYBIT_ALOOP_CARD").unwrap_or_else(|_| "Loopback".into());
    let sub = std::env::var("QYBIT_ALOOP_SUBDEV").unwrap_or_else(|_| "0".into());
    Devices {
        playback: format!("hw:{card},0,{sub}"),
        capture: format!("hw:{card},1,{sub}"),
    }
}

/// 回录 + 播放 + 比对。`files` 与 `want` 为同一格式；`sync` 为同步序列的期望字节。
fn loop_once(
    dev: &Devices,
    res: &mut HostResources,
    case: &LoopCase,
    forced: Option<AlsaFormat>,
    files: &[PathBuf],
    sync: &[u8],
    want: &[u8],
) -> LoopEntry {
    let mut entry = LoopEntry {
        case: *case,
        request: None,
        applied: None,
        captured_bytes: 0,
        sync_offset: None,
        compared_bytes: want.len(),
        verdict: Verdict::Failed(String::new()),
    };
    let period = case.rate / 100;
    let capture = match start_capture(&dev.capture, case.format, case.rate, case.channels, period) {
        Ok((c, _)) => c,
        Err(e) => {
            entry.verdict = Verdict::Failed(format!("回录打开失败：{e}"));
            return entry;
        }
    };
    let mut b = AlsaBackend::new(dev.playback.clone(), RealOpener, RtPriority::default());
    b.forced = forced;
    let r = play(&mut b, res, files);
    // 等回录取走最后的数据。
    std::thread::sleep(Duration::from_millis(150));
    let captured = capture.finish();
    if let Some(a) = r
        .streams
        .first()
        .and_then(|s| s.negotiation.chosen_attempt())
    {
        entry.request = a.request;
        entry.applied = a.applied;
    }
    let captured = match captured {
        Ok(c) => c,
        Err(e) => {
            entry.verdict = Verdict::Failed(format!("回录失败：{e}"));
            return entry;
        }
    };
    entry.captured_bytes = captured.len();
    if !r.software_path_intact() {
        entry.verdict = Verdict::Failed(r.summary());
        return entry;
    }
    let (off, v) = align_and_compare(&captured, sync, want);
    entry.sync_offset = off;
    entry.verdict = v;
    entry
}

#[test]
#[ignore = "需要已加载的 snd-aloop（sudo modprobe snd-aloop）"]
fn aloop_loopback_matrix() {
    let cards = std::fs::read_to_string("/proc/asound/cards").unwrap_or_default();
    let card = std::env::var("QYBIT_ALOOP_CARD").unwrap_or_else(|_| "Loopback".into());
    assert!(
        cards.contains(&card),
        "未运行：没有 {card} 声卡（未加载 snd-aloop）；/proc/asound/cards：{cards:?}"
    );
    let dev = devices();
    let dir = std::env::temp_dir().join(format!("qybit-loop-aloop-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let Ok(mut res) = HostResources::new() else {
        panic!("res");
    };
    let mut entries = Vec::new();
    for (i, case) in cases().into_iter().enumerate() {
        if case.source_bits > case.format.valid_bits() {
            // N > V：设备选择阶段即须拒绝，不打开回录。
            let tracks = make_tracks(&dir, &format!("c{i}"), &case, 256, 2000 + i as u64);
            let mut b = AlsaBackend::new(dev.playback.clone(), RealOpener, RtPriority::default());
            b.forced = Some(case.format);
            let r = play(&mut b, &mut res, &[tracks.test_path]);
            let verdict = if r.streams.is_empty() && !r.open_failures.is_empty() {
                Verdict::Rejected(r.open_failures[0].message.clone())
            } else {
                Verdict::Failed(format!("N > V 未被拒绝：{}", r.summary()))
            };
            entries.push(LoopEntry {
                case,
                request: None,
                applied: None,
                captured_bytes: 0,
                sync_offset: None,
                compared_bytes: 0,
                verdict,
            });
            continue;
        }
        let t = make_tracks(&dir, &format!("c{i}"), &case, 4096, 2000 + i as u64);
        let e = loop_once(
            &dev,
            &mut res,
            &case,
            Some(case.format),
            &[t.sync_path, t.test_path],
            &t.sync_bytes,
            &t.test_bytes,
        );
        eprintln!(
            "{} Hz / {} ch / {} / N={}: {:?}",
            case.rate, case.channels, case.format, case.source_bits, e.verdict
        );
        entries.push(e);
    }

    let mut extra = Vec::new();
    // 同格式多曲无缝衔接：同步 + 两首待测在一个流内连续。
    {
        let case = LoopCase {
            rate: 48_000,
            channels: 2,
            format: AlsaFormat::S24_3Le,
            source_bits: 24,
        };
        let t = make_tracks(&dir, "gapless", &case, 3000, 7);
        let second = test_samples(24, 2, 1500, 99);
        let second_path = dir.join("gapless-second.wav");
        write_wav(&second_path, 48_000, 2, 24, &second);
        let want = [t.test_bytes, device_bytes(&second, 24, AlsaFormat::S24_3Le)].concat();
        let e = loop_once(
            &dev,
            &mut res,
            &case,
            None,
            &[t.sync_path, t.test_path, second_path],
            &t.sync_bytes,
            &want,
        );
        extra.push((
            "同格式多曲无缝衔接（48 kHz/2ch/S24_3LE，三曲一个流）".to_string(),
            e.verdict == Verdict::Match,
            format!("{:?}", e.verdict),
        ));
    }
    // 不同格式依次重新打开设备：snd-aloop 的回录端须与播放端格式一致，因此每种格式各用
    // 一次独立的回录会话（同一 play 调用内的换格式重开由 A 层覆盖）。
    {
        let mut ok = true;
        let mut detail = Vec::new();
        for (k, (rate, ch, n, format)) in [
            (44_100u32, 2u16, 16u8, AlsaFormat::S16Le),
            (96_000, 1, 24, AlsaFormat::S24Le),
            (192_000, 2, 32, AlsaFormat::S32Le),
        ]
        .into_iter()
        .enumerate()
        {
            let case = LoopCase {
                rate,
                channels: ch,
                format,
                source_bits: n,
            };
            let s = sync_samples(n, ch, 50 + k as u64);
            let t = test_samples(n, ch, 2000, 60 + k as u64);
            let (sp, tp) = (
                dir.join(format!("fc{k}-s.wav")),
                dir.join(format!("fc{k}-t.wav")),
            );
            write_wav(&sp, rate, ch, n, &s);
            write_wav(&tp, rate, ch, n, &t);
            let e = loop_once(
                &dev,
                &mut res,
                &case,
                Some(format),
                &[sp, tp],
                &device_bytes(&s, n, format),
                &device_bytes(&t, n, format),
            );
            ok &= e.verdict == Verdict::Match;
            detail.push(format!("{rate}/{ch}/{format}: {:?}", e.verdict));
        }
        extra.push((
            "不同格式依次重新打开（S16_LE → S24_LE → S32_LE，独立回录会话）".to_string(),
            ok,
            detail.join("；"),
        ));
    }

    let report = LoopReport {
        layer: Layer::Aloop,
        device: format!("{} → {}", dev.playback, dev.capture),
        entries,
        extra,
    };
    match report.write(&report_dir()) {
        Ok(p) => eprintln!("B 层报告：{}", p.display()),
        Err(e) => panic!("写报告失败: {e}"),
    }
    let matched = report
        .entries
        .iter()
        .filter(|e| e.verdict == Verdict::Match)
        .count();
    eprintln!(
        "B 层：{} 项，逐字节一致 {matched}，附加检查 {:?}",
        report.entries.len(),
        report.extra.iter().map(|x| x.1).collect::<Vec<_>>()
    );
    let bad: Vec<_> = report
        .entries
        .iter()
        .filter(|e| !e.verdict.ok_for(&e.case))
        .collect();
    assert!(bad.is_empty(), "不通过的用例：{bad:#?}");
    assert!(report.extra.iter().all(|x| x.1), "{:#?}", report.extra);
    let _ = std::fs::remove_dir_all(&dir);
}
