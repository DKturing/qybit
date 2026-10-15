//! M3 T1：引擎新增行为的模拟场景（真实核心、自有解码、pack 与队列；只模拟文件、时间与设备）。
//!
//! 每个场景在若干固定种子（读取分块/延迟与短写设备的随机性）和两种设备模型下运行：
//! - 暂停/恢复：播放中暂停 500 ms 再恢复，以及在排空阶段暂停；设备字节与源逐字节一致，
//!   暂停期间的静音单独计数、不算欠载；
//! - 下一首 / 上一首：切歌丢弃已入队音频，新输出段从目标曲目开头逐字节一致；
//! - 出错跳过：打开失败、解码失败、设备不支持、能力超限的曲目按策略跳过并报告，其余曲目完整；
//!   最后一首出错时停止；
//! - 能力超限拒绝：Embedded Full 配置档下 192 kHz 与 32 位源在打开设备前被拒绝，原因具体；
//! - 高采样率：桌面配置档播放 384 kHz（WAV 16 位、FLAC 24 位），默认模拟队列预算下 384 kHz
//!   24 位被明确拒绝（队列不足）。
//!
//! 期望设备字节由独立公式计算：本文件的 WAV 写入器按容器 = 有效位写入，设备选同宽格式，
//! 设备字节即 data 块；FLAC 期望来自 libFLAC 参考样本 `.s32le`。

use std::path::PathBuf;

use qy_codec_auto::profile::{DESKTOP, EMBEDDED_FULL};
use qy_core::engine::{CapabilityError, EngineFault, ErrorPolicy};
use qy_core::{DecodeError, Fault};
use qy_host_sim::SimHost;
use qy_host_sim::sim::{Control, DeviceModel, ReadModel, Report, Scenario, TrackSpec, run};

const SEEDS: [u64; 4] = [1, 2, 3, 4];
const DEVICES: [DeviceModel; 2] = [DeviceModel::Wasapi, DeviceModel::ShortWrite];
const SUPPORTED: [(u8, u8); 4] = [(16, 16), (32, 24), (24, 24), (32, 32)];

/// 一首测试曲目：文件字节与按同宽设备格式播放时的期望设备字节。
#[derive(Clone)]
struct Track {
    file: Vec<u8>,
    device_bytes: Vec<u8>,
    rate: u32,
}

/// 整数 PCM WAV（容器 = 有效位），样本为各帧不同的可辨识序列；期望设备字节按独立公式给出。
fn wav(rate: u32, channels: u16, bits: u16, frames: usize, salt: u32) -> Track {
    let bytes = usize::from(bits / 8);
    let mut data = Vec::with_capacity(frames * usize::from(channels) * bytes);
    for i in 0..frames * usize::from(channels) {
        let x = (i as u32).wrapping_mul(2_654_435_761).wrapping_add(salt);
        data.extend_from_slice(&x.to_le_bytes()[..bytes]);
    }
    let ba = channels * bits / 8;
    let mut f = Vec::new();
    f.extend_from_slice(b"RIFF");
    f.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    f.extend_from_slice(b"WAVEfmt ");
    f.extend_from_slice(&16u32.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&channels.to_le_bytes());
    f.extend_from_slice(&rate.to_le_bytes());
    f.extend_from_slice(&(rate * u32::from(ba)).to_le_bytes());
    f.extend_from_slice(&ba.to_le_bytes());
    f.extend_from_slice(&bits.to_le_bytes());
    f.extend_from_slice(b"data");
    f.extend_from_slice(&(data.len() as u32).to_le_bytes());
    f.extend_from_slice(&data);
    // 设备格式按 SUPPORTED 的偏好选 V ≥ N 的最小 V：16 → 16/16、32 → 32/32 字节不变；
    // 24 → 32/24（左对齐），每个样本前补一个零字节（小端 s << 8）。
    let device_bytes = if bits == 24 {
        data.chunks_exact(3)
            .flat_map(|c| [0, c[0], c[1], c[2]])
            .collect()
    } else {
        data
    };
    Track {
        file: f,
        device_bytes,
        rate,
    }
}

fn spec(t: &Track) -> TrackSpec {
    TrackSpec {
        data: t.file.clone(),
        open_fails: false,
        read_error_at: None,
        unseekable: false,
    }
}

fn bad(data: &[u8], open_fails: bool) -> TrackSpec {
    TrackSpec {
        data: data.to_vec(),
        open_fails,
        read_error_at: None,
        unseekable: false,
    }
}

fn reads() -> ReadModel {
    ReadModel {
        zero_per_mille: 30,
        tiny_per_mille: 50,
        slow_per_mille: 30,
        slow_max_ticks: 20,
    }
}

fn scenario(
    seed: u64,
    device: DeviceModel,
    tracks: Vec<TrackSpec>,
    on_error: ErrorPolicy,
    controls: Vec<(u64, Control)>,
) -> Scenario {
    Scenario {
        seed,
        tracks,
        reads: reads(),
        device,
        supported: SUPPORTED.to_vec(),
        stalls: Vec::new(),
        disconnect_at: None,
        restart: None,
        max_ticks: 60_000,
        on_error,
        controls,
        device_layouts: None,
    }
}

/// 某输出段 epoch 的全部有效字节。
fn epoch_bytes(r: &Report, epoch: u32) -> Vec<u8> {
    r.segments
        .iter()
        .filter(|s| s.epoch == epoch)
        .flat_map(|s| s.valid.iter().copied())
        .collect()
}

fn concat(ts: &[&Track]) -> Vec<u8> {
    ts.iter()
        .flat_map(|t| t.device_bytes.iter().copied())
        .collect()
}

fn host() -> SimHost {
    match SimHost::new() {
        Ok(h) => h,
        Err(e) => panic!("{e:?}"),
    }
}

#[track_caller]
fn clean(r: &Report) {
    assert!(
        r.finished && r.fault.is_none(),
        "{:?} finished={} timed_out={} ticks={} controls={:?} segs={}",
        r.fault,
        r.finished,
        r.timed_out,
        r.ticks,
        r.controls_done,
        r.segments.len()
    );
    assert!(r.underruns.is_empty(), "欠载 {:?}", r.underruns);
    assert_eq!((r.protocol_errors, r.output_errors), (0, 0));
    assert!(!r.timed_out);
}

#[test]
fn pause_resume_keeps_bytes_and_counts_silence_separately() {
    let mut h = host();
    // 2 首 1.5 s 的 48 kHz 16 位双声道：第一次暂停在播放中，第二次在最后一首已解码完、排空中。
    let a = wav(48_000, 2, 16, 72_000, 1);
    let b = wav(48_000, 2, 16, 72_000, 2);
    let controls = vec![
        (600, Control::Pause),
        (1_100, Control::Resume),
        (2_900, Control::Pause),
        (3_400, Control::Resume),
    ];
    for seed in SEEDS {
        for device in DEVICES {
            let sc = scenario(
                seed,
                device,
                vec![spec(&a), spec(&b)],
                ErrorPolicy::Stop,
                controls.clone(),
            );
            let r = run(&mut h, &sc);
            clean(&r);
            assert!(r.fidelity_intact());
            assert_eq!(r.controls_done.len(), 4, "种子 {seed}");
            assert_eq!(
                epoch_bytes(&r, 0),
                concat(&[&a, &b]),
                "种子 {seed} {device:?}"
            );
            assert_eq!(r.segments.len(), 1, "同格式只开一次设备");
            match device {
                // 两次 500 ms 暂停，每 10 ms 一个整包静音。
                DeviceModel::Wasapi => assert_eq!(r.pause_silence_frames, 2 * 50 * 480),
                DeviceModel::ShortWrite => assert_eq!(r.pause_silence_frames, 0),
            }
        }
    }
}

#[test]
fn pause_before_start_defers_device_start() {
    let mut h = host();
    let a = wav(44_100, 2, 16, 44_100, 3);
    for seed in SEEDS {
        let sc = scenario(
            seed,
            DeviceModel::Wasapi,
            vec![spec(&a)],
            ErrorPolicy::Stop,
            vec![(0, Control::Pause), (700, Control::Resume)],
        );
        let r = run(&mut h, &sc);
        clean(&r);
        assert_eq!(epoch_bytes(&r, 0), a.device_bytes);
        // 设备在恢复之前没有启动，因此没有暂停静音。
        assert_eq!(r.pause_silence_frames, 0);
        // 启动时立即处理第一个周期，因此结束不早于 700 + 1000 − 10 tick。
        assert!(
            r.ticks >= 700 + 1_000 - 10,
            "恢复后才开始播放：{} tick",
            r.ticks
        );
    }
}

#[test]
fn next_and_prev_restart_from_target_track() {
    let mut h = host();
    let t: Vec<Track> = (0..3).map(|i| wav(48_000, 2, 16, 48_000, 10 + i)).collect();
    let mono = wav(96_000, 1, 24, 96_000, 20);
    for seed in SEEDS {
        for device in DEVICES {
            // 下一首：在第 0 首播放中切到第 1 首。
            let tracks = t.iter().map(spec).collect();
            let r = run(
                &mut h,
                &scenario(
                    seed,
                    device,
                    tracks,
                    ErrorPolicy::Stop,
                    vec![(400, Control::Next)],
                ),
            );
            clean(&r);
            let first = epoch_bytes(&r, 0);
            assert!(
                !first.is_empty() && t[0].device_bytes.starts_with(&first),
                "切歌前为第 0 首前缀"
            );
            assert!(first.len() < t[0].device_bytes.len());
            assert_eq!(
                epoch_bytes(&r, 1),
                concat(&[&t[1], &t[2]]),
                "种子 {seed} {device:?}"
            );

            // 上一首：第 1 首（单声道 96 kHz 24 位，换格式）播放中回到第 0 首。
            let tracks = vec![spec(&t[0]), spec(&mono), spec(&t[2])];
            let r = run(
                &mut h,
                &scenario(
                    seed,
                    device,
                    tracks,
                    ErrorPolicy::Stop,
                    vec![(1_800, Control::Prev)],
                ),
            );
            clean(&r);
            assert_eq!(
                epoch_bytes(&r, 0)[..t[0].device_bytes.len()],
                t[0].device_bytes[..]
            );
            let after = epoch_bytes(&r, 1);
            assert_eq!(
                after,
                concat(&[&t[0], &mono, &t[2]]),
                "种子 {seed} {device:?}"
            );
            assert!(
                r.segments.iter().filter(|s| s.epoch == 1).count() == 3,
                "换格式各开一次"
            );

            // 最后一首时“下一首”即停止。
            let r = run(
                &mut h,
                &scenario(
                    seed,
                    device,
                    vec![spec(&t[0])],
                    ErrorPolicy::Stop,
                    vec![(300, Control::Next)],
                ),
            );
            assert!(!r.finished && r.fault.is_none() && !r.timed_out);
            assert_eq!(r.protocol_errors, 0);
        }
    }
}

/// 12 首 120 ms 的短曲目（队列一次可容纳全部）：曲目边界记录达到上限时引擎暂停解码，
/// 输出仍逐字节完整；“下一首”跳到正在播放的曲目之后，而不是正在解码的曲目之后。
#[test]
fn many_short_tracks_and_next_follows_playing_track() {
    let mut h = host();
    let t: Vec<Track> = (0..12).map(|i| wav(48_000, 2, 16, 5_760, 60 + i)).collect();
    let all: Vec<&Track> = t.iter().collect();
    for seed in SEEDS {
        for device in DEVICES {
            let tracks: Vec<TrackSpec> = t.iter().map(spec).collect();
            let r = run(
                &mut h,
                &scenario(seed, device, tracks.clone(), ErrorPolicy::Stop, Vec::new()),
            );
            clean(&r);
            assert_eq!(epoch_bytes(&r, 0), concat(&all));

            let r = run(
                &mut h,
                &scenario(
                    seed,
                    device,
                    tracks,
                    ErrorPolicy::Stop,
                    vec![(650, Control::Next)],
                ),
            );
            clean(&r);
            let before = epoch_bytes(&r, 0);
            assert!(concat(&all).starts_with(&before));
            // 切歌时正在播放的曲目 k：已播字节落在第 k 首内（含其末尾）。
            let per = t[0].device_bytes.len();
            let k = before.len().saturating_sub(1) / per;
            assert_eq!(
                epoch_bytes(&r, 1),
                concat(&all[k + 1..]),
                "种子 {seed} {device:?}：已播 {} 字节",
                before.len()
            );
            assert!(k >= 3, "650 ms 时应在第 4 首之后：k = {k}");
        }
    }
}

/// S1：播放中定位。第 1 首播到约 300 ms 时定位到第 30 000 帧：此前已播的字节是第 1 首的前缀，
/// 定位后的输出段从目标帧起与第 1 首其余部分和第 2 首逐字节一致（无欠载）。音源不可定位时改为
/// 顺序解码，结果相同并报告；暂停中定位保持暂停，恢复后从目标帧播放；目标超过长度时进入下一首；
/// 每 10 ms 一次、连续 100 次定位后只播放最后一次的目标。
#[test]
fn seek_restarts_track_at_target_frame() {
    let mut h = host();
    let a = wav(44_100, 2, 16, 66_150, 40);
    let b = wav(44_100, 2, 24, 22_050, 41);
    let fb = 4;
    let tail = |frame: usize| [&a.device_bytes[frame * fb..], &b.device_bytes[..]].concat();
    for seed in SEEDS {
        for device in DEVICES {
            for unseekable in [false, true] {
                let mut tracks = vec![spec(&a), spec(&b)];
                tracks[0].unseekable = unseekable;
                let sc = scenario(
                    seed,
                    device,
                    tracks.clone(),
                    ErrorPolicy::Stop,
                    vec![(300, Control::Seek(30_000))],
                );
                let r = run(&mut h, &sc);
                clean(&r);
                let ctx = format!("种子 {seed} {device:?} 不可定位 {unseekable}");
                assert!(a.device_bytes.starts_with(&epoch_bytes(&r, 0)), "{ctx}");
                assert_eq!(epoch_bytes(&r, 1), tail(30_000), "{ctx}");
                assert_eq!(
                    r.sequential_seeks,
                    if unseekable { vec![0] } else { vec![] }
                );
                assert_eq!(r.seeks.len(), usize::from(!unseekable), "{ctx}");

                // 暂停中定位：保持暂停（静音单独计数），恢复后从目标帧播放。
                let sc = scenario(
                    seed,
                    device,
                    tracks.clone(),
                    ErrorPolicy::Stop,
                    vec![
                        (250, Control::Pause),
                        (300, Control::Seek(40_000)),
                        (900, Control::Resume),
                    ],
                );
                let r = run(&mut h, &sc);
                clean(&r);
                assert_eq!(epoch_bytes(&r, 1), tail(40_000), "{ctx}");
                // 整包设备暂停时送静音包；短写设备不补零（同上面的暂停测试）。
                assert_eq!(
                    r.pause_silence_frames > 0,
                    device == DeviceModel::Wasapi,
                    "{ctx}"
                );

                // 目标超过长度：按播完该曲处理，进入下一首。
                let sc = scenario(
                    seed,
                    device,
                    tracks.clone(),
                    ErrorPolicy::Stop,
                    vec![(300, Control::Seek(70_000))],
                );
                let r = run(&mut h, &sc);
                clean(&r);
                assert_eq!(epoch_bytes(&r, 1), b.device_bytes, "{ctx}");

                // 连续快速定位：只播放最后一次的目标。
                let controls: Vec<_> = (0..100u64)
                    .map(|k| (300 + 10 * k, Control::Seek(500 * k + 7)))
                    .collect();
                let r = run(
                    &mut h,
                    &scenario(seed, device, tracks, ErrorPolicy::Stop, controls),
                );
                clean(&r);
                assert_eq!(r.controls_done.len(), 100, "{ctx}");
                assert_eq!(epoch_bytes(&r, 100), tail(500 * 99 + 7), "{ctx}");
            }
        }
    }
}

#[test]
fn skip_policy_reports_and_continues() {
    let mut h = host();
    let a = wav(44_100, 2, 16, 22_050, 30);
    let b = wav(44_100, 2, 24, 22_050, 31);
    let c = wav(48_000, 1, 16, 24_000, 32);
    let malformed = b"RIFF\x04\x00\x00\x00WAVE";
    for seed in SEEDS {
        for device in DEVICES {
            // 打开失败、文件头损坏、设备不支持（只支持 16 位时的 24 位源）。
            let tracks = vec![
                spec(&a),
                bad(b"", true),
                bad(malformed, false),
                spec(&b),
                spec(&c),
            ];
            let mut sc = scenario(seed, device, tracks, ErrorPolicy::Skip, Vec::new());
            sc.supported = vec![(16, 16)];
            let skipped_before = h.engine.stats().skipped;
            let r = run(&mut h, &sc);
            clean(&r);
            assert_eq!(
                h.engine.stats().skipped - skipped_before,
                3,
                "引擎统计的跳过数"
            );
            assert_eq!(
                r.skipped,
                vec![
                    (1, EngineFault::Core(Fault::Open)),
                    (2, EngineFault::Core(Fault::Decode(DecodeError::Malformed))),
                    (3, EngineFault::Core(Fault::Configure)),
                ],
                "种子 {seed} {device:?}"
            );
            // 跳过不重开设备：a 与 c 格式不同，各一段。
            assert_eq!(epoch_bytes(&r, 0), concat(&[&a, &c]));
            assert!(!r.fidelity_intact(), "有曲目被跳过，不能记为完整");

            // 最后一首出错：排空已入队音频后停止并报告。
            let tracks = vec![spec(&a), bad(malformed, false)];
            let r = run(
                &mut h,
                &scenario(seed, device, tracks, ErrorPolicy::Skip, Vec::new()),
            );
            assert_eq!(
                r.fault,
                Some((1, EngineFault::Core(Fault::Decode(DecodeError::Malformed))))
            );
            assert!(!r.finished && r.skipped.is_empty());
            assert_eq!(epoch_bytes(&r, 0), a.device_bytes);

            // 停止策略：同样的列表在第一处错误处停止。
            let tracks = vec![spec(&a), bad(b"", true), spec(&c)];
            let r = run(
                &mut h,
                &scenario(seed, device, tracks, ErrorPolicy::Stop, Vec::new()),
            );
            assert_eq!(r.fault, Some((1, EngineFault::Core(Fault::Open))));
            assert_eq!(epoch_bytes(&r, 0), a.device_bytes);
        }
    }
}

#[test]
fn embedded_profile_rejects_out_of_capability_sources_before_opening() {
    let mut h = match SimHost::with_profile(&EMBEDDED_FULL) {
        Ok(h) => h,
        Err(e) => panic!("{e:?}"),
    };
    let ok = wav(96_000, 2, 24, 48_000, 40);
    let hi_rate = wav(192_000, 2, 16, 19_200, 41);
    let deep = wav(48_000, 2, 32, 4_800, 42);
    let tail = wav(44_100, 2, 16, 22_050, 43);
    for seed in SEEDS {
        for device in DEVICES {
            let tracks = vec![spec(&ok), spec(&hi_rate), spec(&deep), spec(&tail)];
            let r = run(
                &mut h,
                &scenario(seed, device, tracks.clone(), ErrorPolicy::Skip, Vec::new()),
            );
            clean(&r);
            assert_eq!(
                r.skipped,
                vec![
                    (
                        1,
                        EngineFault::Capability(CapabilityError::Rate {
                            rate: 192_000,
                            max: 96_000
                        })
                    ),
                    (
                        2,
                        EngineFault::Capability(CapabilityError::Bits { bits: 32, max: 24 })
                    ),
                ]
            );
            // 超限的源没有打开设备：只有 ok 与 tail 两段。
            assert_eq!(r.segments.len(), 2);
            assert_eq!(epoch_bytes(&r, 0), concat(&[&ok, &tail]));

            let r = run(
                &mut h,
                &scenario(seed, device, tracks, ErrorPolicy::Stop, Vec::new()),
            );
            let Some((1, EngineFault::Capability(e))) = r.fault else {
                panic!("期望能力不足 {:?}", r.fault);
            };
            assert_eq!(
                e.to_string(),
                "采样率 192000 Hz 超出本机配置的上限 96000 Hz"
            );
            assert_eq!(epoch_bytes(&r, 0), ok.device_bytes);
        }
    }
}

fn flac_fixture(name: &str) -> (Vec<u8>, Vec<i32>) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../qy-codec-flac/tests/fixtures");
    let data = std::fs::read(dir.join(format!("{name}.flac"))).unwrap_or_default();
    let reference = std::fs::read(dir.join(format!("{name}.s32le")))
        .unwrap_or_default()
        .chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    (data, reference)
}

#[test]
fn rate_384k_plays_on_desktop_profile_and_is_rejected_by_small_queue() {
    let (flac, reference) = flac_fixture("rate_384k_stereo_24");
    assert_eq!(reference.len(), 2_048 * 2);
    // 24 位源：设备按偏好选 V = 24 中的第一项 32/24（左对齐），设备字节为 s << 8 的 4 字节小端。
    let flac_bytes: Vec<u8> = reference
        .iter()
        .flat_map(|s| (s << 8).to_le_bytes())
        .collect();
    let w = wav(384_000, 2, 16, 76_800, 50);
    assert_eq!(w.rate, 384_000);

    let mut desktop = match SimHost::with_profile(&DESKTOP) {
        Ok(h) => h,
        Err(e) => panic!("{e:?}"),
    };
    for seed in SEEDS {
        for device in DEVICES {
            let tracks = vec![spec(&w), bad(&flac, false)];
            let r = run(
                &mut desktop,
                &scenario(seed, device, tracks, ErrorPolicy::Stop, Vec::new()),
            );
            clean(&r);
            assert!(r.fidelity_intact());
            let fmts: Vec<_> = r
                .segments
                .iter()
                .map(|s| (s.format.sample_rate(), s.format.container_bits()))
                .collect();
            assert_eq!(fmts, vec![(384_000, 16), (384_000, 32)]);
            assert_eq!(
                r.segments[0].valid, w.device_bytes,
                "种子 {seed} {device:?}"
            );
            assert_eq!(r.segments[1].valid, flac_bytes, "种子 {seed} {device:?}");
        }
    }

    // 默认模拟队列（192 kHz 双声道 C32 的 500 ms = 768 000 字节）：384 kHz 16 位双声道恰好放得下，
    // 24 位需要 1 152 000 字节，在打开设备前明确拒绝。
    let mut h = host();
    let r = run(
        &mut h,
        &scenario(
            1,
            DeviceModel::Wasapi,
            vec![spec(&w), bad(&flac, false)],
            ErrorPolicy::Stop,
            Vec::new(),
        ),
    );
    assert_eq!(epoch_bytes(&r, 0), w.device_bytes);
    let Some((1, EngineFault::Capability(e))) = r.fault else {
        panic!("期望队列不足 {:?}", r.fault);
    };
    assert_eq!(
        e,
        CapabilityError::Queue {
            required: 1_152_000,
            available: 768_000
        }
    );
    assert!(e.to_string().contains("队列不足以承载该格式"));
}
