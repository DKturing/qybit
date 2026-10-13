//! M1 T3：FLAC 接入模拟宿主的固定种子场景。
//!
//! 经真实核心、自动识别解码器（自有 FLAC/WAV）、pack 与 SPSC 队列播放 qy-codec-flac 的
//! 夹具（期望样本来自 libFLAC 参考 `.s32le`，见夹具 MANIFEST），与测试内独立 WAV 写入器
//! 生成的曲目混排；期望设备字节由独立公式 `s << (C − N)` 计算。
//!
//! 场景（种子 % 6）：
//! - 0–1 正常：FLAC/WAV 混排 1–4 首，短读/零长度读/慢读与生产端停顿 ≤ 300 ms；
//!   WASAPI 整包或短写设备。逐字节匹配、无欠载。
//! - 2 解码故障：某首替换为错误夹具；故障前曲目完整，故障曲目只输出参考中 CRC 已校验的
//!   帧（`.s32le`），故障类型与 `.meta` 一致。
//! - 3 读取错误：某首 FLAC 在随机偏移读取失败；输出为参考前缀。
//! - 4 停止重启：全部慢读，播放中停止并从某曲目重放，在途旧读取必须被拒绝。
//! - 5 最大块：65535 帧 32 位 verbatim 夹具（约 512 KiB 一帧）与其他曲目混排，
//!   读取多为 1–16 字节短读；按种子轮换设备断开。
//!
//! 整个测试只创建一个 [`SimHost`]（一套泄漏一次的工作区）跨全部种子复用。
//! 单独复现：`QY_SIM_SEED=<种子> cargo test -p qy-host-sim --test sim_flac`。

use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use qy_core::{DecodeError, DeviceFormat, Fault, SourceFormat};
use qy_host_sim::SimHost;
use qy_host_sim::sim::{
    DeviceModel, ReadModel, Report, Rng, Scenario, TrackSpec, choose_device_format, run,
};

const SEEDS: u64 = 1200;
const SUPPORTED: [(u8, u8); 4] = [(16, 16), (32, 24), (24, 24), (32, 32)];
const MAX_BLOCK: &str = "max_block_verbatim_32_stereo";

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../qy-codec-flac/tests/fixtures")
}

/// 一个 FLAC 夹具及其独立参考。
#[derive(Clone)]
struct Fixture {
    name: String,
    data: Vec<u8>,
    result: Result<(), DecodeError>,
    format: Option<SourceFormat>,
    /// libFLAC 参考样本（错误夹具为出错前已校验帧的样本）。
    samples: Vec<i32>,
}

fn load_fixtures() -> Vec<Fixture> {
    let mut names: Vec<String> = fs::read_dir(dir())
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    e.file_name()
                        .into_string()
                        .ok()?
                        .strip_suffix(".flac")
                        .map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let data = fs::read(dir().join(format!("{name}.flac"))).unwrap_or_default();
            let meta = fs::read_to_string(dir().join(format!("{name}.meta"))).unwrap_or_default();
            let mut result = Ok(());
            let mut format = None;
            for line in meta.lines() {
                let (k, v) = line.split_once(' ').unwrap_or((line, ""));
                match k {
                    "result" => {
                        result = match v {
                            "ok" => Ok(()),
                            "Malformed" => Err(DecodeError::Malformed),
                            "Unsupported" => Err(DecodeError::Unsupported),
                            "Truncated" => Err(DecodeError::Truncated),
                            "CapacityExceeded" => Err(DecodeError::CapacityExceeded),
                            x => panic!("{name}: 未知结果 {x}"),
                        }
                    }
                    "format" if v != "none" => {
                        let p: Vec<u32> = v.split(' ').filter_map(|x| x.parse().ok()).collect();
                        assert_eq!(p.len(), 3, "{name}: format 行");
                        format = SourceFormat::new(p[0], p[1] as u16, p[2] as u8).ok();
                        assert!(format.is_some(), "{name}: 非法格式");
                    }
                    _ => {}
                }
            }
            let samples = fs::read(dir().join(format!("{name}.s32le")))
                .unwrap_or_default()
                .chunks_exact(4)
                .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            Fixture {
                name,
                data,
                result,
                format,
                samples,
            }
        })
        .collect()
}

struct Track {
    label: String,
    format: Option<SourceFormat>,
    samples: Vec<i32>,
    /// 应输出的帧数。
    playable_frames: usize,
    spec: TrackSpec,
}

impl Track {
    fn channels(&self) -> usize {
        self.format.map_or(1, |f| usize::from(f.channels()))
    }
    fn total_frames(&self) -> usize {
        self.samples.len() / self.channels()
    }
}

fn spec(data: Vec<u8>) -> TrackSpec {
    TrackSpec {
        data,
        open_fails: false,
        read_error_at: None,
    }
}

fn flac_track(f: &Fixture) -> Track {
    let mut t = Track {
        label: f.name.clone(),
        format: f.format,
        samples: f.samples.clone(),
        playable_frames: 0,
        spec: spec(f.data.clone()),
    };
    t.playable_frames = t.total_frames();
    t
}

/// 独立 WAV 写入器（PCM 16/24 位，小端）。
fn wav_track(rng: &mut Rng) -> Track {
    let rate = [44_100u32, 48_000, 96_000][rng.range(0, 2) as usize];
    let ch = rng.range(1, 2) as u16;
    let bits: u8 = if rng.chance(500) { 16 } else { 24 };
    let frames = rng.range(1, u64::from(rate) / 5) as usize;
    let lo = -(1i64 << (bits - 1));
    let hi = (1i64 << (bits - 1)) - 1;
    let samples: Vec<i32> = (0..frames * usize::from(ch))
        .map(|_| (lo + (rng.next_u64() % ((hi - lo + 1) as u64)) as i64) as i32)
        .collect();
    let bytes = usize::from(bits / 8);
    let ba = ch * u16::from(bits / 8);
    let mut data = Vec::new();
    for &s in &samples {
        data.extend_from_slice(&s.to_le_bytes()[..bytes]);
    }
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&((4 + 24 + 8 + data.len()) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&ch.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * u32::from(ba)).to_le_bytes());
    out.extend_from_slice(&ba.to_le_bytes());
    out.extend_from_slice(&u16::from(bits).to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    let format = SourceFormat::new(rate, ch, bits).ok();
    Track {
        label: format!("wav{bits}/{rate}/{ch}"),
        format,
        samples,
        playable_frames: frames,
        spec: spec(out),
    }
}

#[derive(Debug, Clone, Copy)]
enum Expect {
    Normal,
    /// 曲目 `track` 以 `fault` 失败；`partial` 为真时该曲目输出只需为前缀。
    Fault {
        track: u32,
        fault: Fault,
        partial: bool,
    },
    DeviceLost,
    Restart {
        from: u32,
    },
}

struct Case {
    tracks: Vec<Track>,
    scenario: Scenario,
    expect: Expect,
}

fn reads(slow: u64) -> ReadModel {
    ReadModel {
        zero_per_mille: 50,
        tiny_per_mille: 100,
        slow_per_mille: slow,
        slow_max_ticks: 2,
    }
}

fn duration_ms(tracks: &[Track]) -> u64 {
    tracks
        .iter()
        .filter_map(|t| {
            let rate = u64::from(t.format?.sample_rate());
            Some((t.total_frames() as u64 * 1000).div_ceil(rate))
        })
        .sum()
}

fn build_case(seed: u64, fixtures: &[Fixture]) -> Case {
    let mut rng = Rng::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xF1AC);
    let ok: Vec<&Fixture> = fixtures
        .iter()
        .filter(|f| f.result.is_ok() && f.name != MAX_BLOCK)
        .collect();
    let errs: Vec<&Fixture> = fixtures.iter().filter(|f| f.result.is_err()).collect();
    let max_block = fixtures.iter().find(|f| f.name == MAX_BLOCK);
    let pick_ok = |rng: &mut Rng| ok[rng.range(0, ok.len() as u64 - 1) as usize];
    let list = |rng: &mut Rng, count: usize| -> Vec<Track> {
        (0..count)
            .map(|_| {
                if rng.chance(250) {
                    wav_track(rng)
                } else {
                    flac_track(pick_ok(rng))
                }
            })
            .collect()
    };
    let mut device = if rng.chance(300) {
        DeviceModel::ShortWrite
    } else {
        DeviceModel::Wasapi
    };
    let mut read_model = reads(30);
    let mut stalls = Vec::new();
    let mut disconnect_at = None;
    let mut restart = None;
    let mut tracks;
    let expect;
    match seed % 6 {
        0 | 1 => {
            let n = rng.range(1, 4) as usize;
            tracks = list(&mut rng, n);
            let dur = duration_ms(&tracks);
            let mut t = rng.range(0, 200);
            while t < dur + 500 && stalls.len() < 3 {
                if rng.chance(600) {
                    stalls.push((t, rng.range(1, 300)));
                }
                t += 1000 + rng.range(0, 500);
            }
            expect = Expect::Normal;
        }
        2 => {
            let n = rng.range(1, 3) as usize;
            tracks = list(&mut rng, n);
            let k = rng.range(0, n as u64) as usize;
            let e = errs[((seed / 6) % errs.len() as u64) as usize];
            tracks.insert(k, flac_track(e));
            // 无 `fLaC` 标记的文件在自动识别层即判为不支持的容器（不进入 FLAC 解码器），
            // 其余错误与 qy-codec-flac 单独解码的 `.meta` 结果一致。
            let err = match e.result {
                Err(_) if !e.data.starts_with(b"fLaC") => DecodeError::Unsupported,
                Err(err) => err,
                Ok(()) => unreachable!(),
            };
            expect = Expect::Fault {
                track: k as u32,
                fault: Fault::Decode(err),
                partial: false,
            };
        }
        3 => {
            let n = rng.range(1, 3) as usize;
            tracks = list(&mut rng, n);
            let k = rng.range(0, n as u64) as usize;
            let mut t = flac_track(pick_ok(&mut rng));
            t.spec.read_error_at = Some(rng.range(0, t.spec.data.len() as u64 - 1) as usize);
            tracks.insert(k, t);
            expect = Expect::Fault {
                track: k as u32,
                fault: Fault::Read,
                partial: true,
            };
        }
        4 => {
            let n = rng.range(1, 3) as usize;
            tracks = list(&mut rng, n);
            read_model = ReadModel {
                zero_per_mille: 50,
                tiny_per_mille: 50,
                slow_per_mille: 1000,
                slow_max_ticks: 2,
            };
            let dur = duration_ms(&tracks);
            let from = rng.range(0, n as u64 - 1) as u32;
            restart = Some((rng.range(1, (dur / 3).max(2)), from));
            expect = Expect::Restart { from };
        }
        _ => {
            let Some(mb) = max_block else {
                panic!("缺少 {MAX_BLOCK}");
            };
            let n = rng.range(0, 2) as usize;
            tracks = list(&mut rng, n);
            let k = rng.range(0, n as u64) as usize;
            tracks.insert(k, flac_track(mb));
            read_model = ReadModel {
                zero_per_mille: 50,
                tiny_per_mille: 700,
                slow_per_mille: 10,
                slow_max_ticks: 2,
            };
            device = if (seed / 6) % 2 == 0 {
                DeviceModel::Wasapi
            } else {
                DeviceModel::ShortWrite
            };
            if (seed / 6) % 3 == 2 {
                let dur = duration_ms(&tracks);
                disconnect_at = Some(rng.range(5, dur.max(10)));
                expect = Expect::DeviceLost;
            } else {
                expect = Expect::Normal;
            }
        }
    }
    let dur = duration_ms(&tracks);
    let stall_total: u64 = stalls.iter().map(|s| s.1).sum();
    let scenario = Scenario {
        seed,
        tracks: tracks.iter().map(|t| t.spec.clone()).collect(),
        reads: read_model,
        device,
        supported: SUPPORTED.to_vec(),
        stalls,
        disconnect_at,
        restart,
        max_ticks: dur * 3 + stall_total + 100 * tracks.len() as u64 + 5_000,
    };
    Case {
        tracks,
        scenario,
        expect,
    }
}

type Segs = Vec<(DeviceFormat, Vec<u8>)>;

/// 期望的设备段：按设备格式分组相邻曲目，字节由独立公式计算。
fn expected_segments(tracks: &[Track], frames_of: impl Fn(usize) -> usize) -> Segs {
    let mut segs: Segs = Vec::new();
    for (i, t) in tracks.iter().enumerate() {
        let frames = frames_of(i);
        let Some(src) = t.format else { continue };
        if frames == 0 {
            continue;
        }
        let Some(dev) = choose_device_format(&src, &SUPPORTED) else {
            break;
        };
        let c = u32::from(dev.container_bits());
        let n = u32::from(src.bits());
        let mut bytes = Vec::new();
        for &s in &t.samples[..frames * t.channels()] {
            let word = (i64::from(s) << (c - n)) as u32;
            bytes.extend_from_slice(&word.to_le_bytes()[..(c / 8) as usize]);
        }
        match segs.last_mut() {
            Some((f, b)) if *f == dev => b.extend_from_slice(&bytes),
            _ => segs.push((dev, bytes)),
        }
    }
    segs
}

fn actual_segments(report: &Report, epoch: u32) -> Segs {
    report
        .segments
        .iter()
        .filter(|s| s.epoch == epoch && !s.valid.is_empty())
        .map(|s| (s.format, s.valid.clone()))
        .collect()
}

fn describe(segs: &[(DeviceFormat, Vec<u8>)]) -> String {
    segs.iter()
        .map(|(f, b)| {
            format!(
                "{}Hz/{}ch/C{}V{}:{}B",
                f.sample_rate(),
                f.channels(),
                f.container_bits(),
                f.valid_bits(),
                b.len()
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn exact(
    actual: &[(DeviceFormat, Vec<u8>)],
    expected: &[(DeviceFormat, Vec<u8>)],
) -> Result<(), String> {
    if actual == expected {
        return Ok(());
    }
    Err(format!(
        "输出不一致\n实际 [{}]\n期望 [{}]",
        describe(actual),
        describe(expected)
    ))
}

fn prefix(
    actual: &[(DeviceFormat, Vec<u8>)],
    expected: &[(DeviceFormat, Vec<u8>)],
) -> Result<(), String> {
    let ok = actual.len() <= expected.len()
        && actual.iter().zip(expected).enumerate().all(|(i, (a, e))| {
            let last = i + 1 == actual.len();
            a.0 == e.0
                && if last {
                    e.1.starts_with(&a.1)
                } else {
                    a.1 == e.1
                }
        });
    if ok {
        Ok(())
    } else {
        Err(format!(
            "输出不是期望的前缀\n实际 [{}]\n期望 [{}]",
            describe(actual),
            describe(expected)
        ))
    }
}

fn total(segs: &[(DeviceFormat, Vec<u8>)]) -> usize {
    segs.iter().map(|s| s.1.len()).sum()
}

fn verify(case: &Case, r: &Report) -> Result<(), String> {
    let sc = &case.scenario;
    if r.timed_out {
        return Err(format!("超时（{} tick）", r.ticks));
    }
    if r.protocol_errors != 0 || r.output_errors != 0 {
        return Err(format!(
            "协议错误 {} / 输出错误 {}",
            r.protocol_errors, r.output_errors
        ));
    }
    for seg in &r.segments {
        let packet = u64::from(seg.format.sample_rate() / 100);
        if sc.device == DeviceModel::Wasapi && seg.tail_padding_frames >= packet {
            return Err(format!(
                "尾部填充 {} ≥ 一包 {packet}",
                seg.tail_padding_frames
            ));
        }
        if sc.device == DeviceModel::ShortWrite && seg.tail_padding_frames != 0 {
            return Err("短写后端不应补零".into());
        }
    }
    let tracks = &case.tracks;
    let all = |upto: usize| expected_segments(&tracks[..upto], |i| tracks[i].playable_frames);
    let n = tracks.len();
    let actual = actual_segments(r, 0);
    match case.expect {
        Expect::Normal => {
            if !r.finished || r.fault.is_some() || !r.fidelity_intact() {
                return Err(format!(
                    "未正常完成 fault={:?} finished={} underruns={}",
                    r.fault,
                    r.finished,
                    r.underruns.len()
                ));
            }
            exact(&actual, &all(n))
        }
        Expect::Fault {
            track,
            fault,
            partial,
        } => {
            if r.fault != Some((track, fault)) {
                return Err(format!("期望故障 {:?}，实际 {:?}", (track, fault), r.fault));
            }
            if r.finished || r.fidelity_intact() {
                return Err("故障场景不应完成或标记完整".into());
            }
            let k = track as usize;
            if partial {
                let before = all(k);
                if total(&actual) < total(&before) {
                    return Err("故障前曲目未完整排空".into());
                }
                prefix(&actual, &all(k + 1))
            } else {
                exact(&actual, &all(k + 1))
            }
        }
        Expect::DeviceLost => {
            if r.device_lost && r.fidelity_intact() {
                return Err("断开后仍标记完整保真".into());
            }
            if !r.device_lost && !r.finished {
                return Err("设备断开未报告".into());
            }
            if r.device_lost {
                prefix(&actual, &all(n))
            } else {
                exact(&actual, &all(n))
            }
        }
        Expect::Restart { from } => {
            if !r.finished || r.fault.is_some() || !r.underruns.is_empty() {
                return Err(format!(
                    "重启后未正常完成 finished={} fault={:?}",
                    r.finished, r.fault
                ));
            }
            if r.restarted_at.is_none() {
                return exact(&actual, &all(n));
            }
            if r.reads_in_flight_at_restart == 0 {
                return Err("重启时没有在途读取".into());
            }
            if r.stale_rejected < r.reads_in_flight_at_restart {
                return Err(format!(
                    "在途旧读取 {} 未全部拒绝（拒绝 {}）",
                    r.reads_in_flight_at_restart, r.stale_rejected
                ));
            }
            prefix(&actual, &all(n))?;
            let rest = &tracks[from as usize..];
            exact(
                &actual_segments(r, 1),
                &expected_segments(rest, |i| rest[i].playable_frames),
            )
        }
    }
}

struct SeedGuard(u64);
impl Drop for SeedGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("panic 于种子 {}（QY_SIM_SEED={} 复现）", self.0, self.0);
        }
    }
}

fn seeds() -> Vec<u64> {
    match std::env::var("QY_SIM_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        Some(s) => vec![s],
        None => (0..SEEDS).collect(),
    }
}

#[test]
fn flac_seeded_scenarios() {
    let fixtures = load_fixtures();
    assert!(fixtures.len() >= 44, "夹具数 {}", fixtures.len());
    let Ok(mut host) = SimHost::new() else {
        panic!("host");
    };
    let started = Instant::now();
    let mut failures = Vec::new();
    let mut kinds = [0u32; 6];
    let mut faults = std::collections::BTreeMap::<String, u32>::new();
    let mut flac_tracks = 0usize;
    let mut stale = 0u32;
    let mut lost = 0u32;
    for seed in seeds() {
        let _guard = SeedGuard(seed);
        let case = build_case(seed, &fixtures);
        let report = run(&mut host, &case.scenario);
        kinds[(seed % 6) as usize] += 1;
        flac_tracks += case
            .tracks
            .iter()
            .filter(|t| !t.label.starts_with("wav"))
            .count();
        stale += report.stale_rejected;
        lost += u32::from(report.device_lost);
        if let Some((_, f)) = report.fault {
            *faults.entry(format!("{f:?}")).or_default() += 1;
        }
        if let Err(e) = verify(&case, &report) {
            let labels: Vec<&str> = case.tracks.iter().map(|t| t.label.as_str()).collect();
            failures.push(format!("种子 {seed}（类型 {}，{labels:?}）: {e}", seed % 6));
        }
    }
    eprintln!(
        "FLAC 模拟种子 {}：各类型 {kinds:?}，FLAC 曲目 {flac_tracks}，故障 {faults:?}，\
         拒绝旧回报 {stale}，设备断开 {lost}，耗时 {:.1} s",
        kinds.iter().sum::<u32>(),
        started.elapsed().as_secs_f64()
    );
    assert!(
        failures.is_empty(),
        "{} 个种子失败：\n{}",
        failures.len(),
        failures
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// 并发实例：每个线程拥有独立的 SimHost（独立、不重叠的工作区），
/// 各自跑一组种子，结果与单实例逐字节一致。
#[test]
fn concurrent_hosts_have_independent_workspaces() {
    const THREADS: u64 = 4;
    const PER_THREAD: u64 = 12;
    let fixtures = load_fixtures();
    let mut hosts = Vec::new();
    for _ in 0..THREADS {
        let Ok(h) = SimHost::new() else {
            panic!("host");
        };
        hosts.push(h);
    }
    let mut regions: Vec<(usize, usize)> = hosts.iter().flat_map(|h| h.buffer_regions()).collect();
    regions.sort_unstable();
    for w in regions.windows(2) {
        assert!(w[0].1 <= w[1].0, "工作区重叠：{:x?} 与 {:x?}", w[0], w[1]);
    }
    let handles: Vec<_> = hosts
        .into_iter()
        .enumerate()
        .map(|(i, mut host)| {
            let fixtures = fixtures.clone();
            std::thread::spawn(move || {
                let mut outs = Vec::new();
                for j in 0..PER_THREAD {
                    // 线程间错开种子类型；同一实例跨种子复用同一套缓冲。
                    let seed = 10_000 + i as u64 * PER_THREAD + j;
                    let case = build_case(seed, &fixtures);
                    let r = run(&mut host, &case.scenario);
                    if let Err(e) = verify(&case, &r) {
                        return Err(format!("线程 {i} 种子 {seed}: {e}"));
                    }
                    outs.push((seed, actual_segments(&r, 0)));
                }
                Ok(outs)
            })
        })
        .collect();
    let mut all = Vec::new();
    for h in handles {
        match h.join() {
            Ok(Ok(v)) => all.extend(v),
            Ok(Err(e)) => panic!("{e}"),
            Err(p) => std::panic::resume_unwind(p),
        }
    }
    // 单实例顺序重放，结果必须相同。
    let Ok(mut single) = SimHost::new() else {
        panic!("host");
    };
    for (seed, segs) in all {
        let case = build_case(seed, &fixtures);
        let r = run(&mut single, &case.scenario);
        assert_eq!(
            actual_segments(&r, 0),
            segs,
            "种子 {seed} 并发结果与单实例不同"
        );
    }
}
