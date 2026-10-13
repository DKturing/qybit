//! M0 T4：1000 个固定种子的真实路径模拟。
//!
//! 每个种子生成一组曲目（测试内独立的 WAV 写入器，已知样本，含前导静音与非零尾帧）
//! 和一个场景，经真实核心、WAV 解码、pack 与 SPSC 队列播放，再与独立公式
//! `s << (C − N)` 计算的期望设备字节比较。
//!
//! 场景（种子 % 10）：
//! - 0–5 正常：短读/零长度读/慢读（≤ 2 tick，3%）、生产端停顿 ≤ 300 ms 且间隔 ≥ 1000 ms，
//!   预填 500 ms；WASAPI 整包或短写设备。要求逐字节匹配、无欠载、尾部填充 < 1 包。
//! - 6 超预算停顿：192 kHz C32 双声道（队列恰为 500 ms），设备启动后停顿 1000 ms；
//!   要求报告欠载（位置、缺失帧），有效音频仍逐字节完整、无重复，播放恢复并完成，
//!   且不得标记为完整保真。
//! - 7 输入故障：打开失败 / 读取错误 / 截断文件 / 非法 fmt（按种子轮换）。
//! - 8 输出故障：设备不支持格式 / 播放中设备断开（按种子轮换）。
//! - 9 停止重启：全部读取为慢读，播放中停止并从某曲目重新开始，在途旧读取必须被拒绝。
//!
//! 每个场景都有超时上限；失败时打印种子。单独复现：`QY_SIM_SEED=<种子> cargo test -p qy-host-sim --test sim_seeds`。

use qy_core::{DecodeError, DeviceFormat, Fault, SourceFormat};
use qy_host_sim::SimHost;
use qy_host_sim::sim::{
    DeviceModel, ReadModel, Report, Rng, Scenario, TrackSpec, choose_device_format, run,
};

const SEEDS: u64 = 1000;
const DEFAULT_SUPPORTED: [(u8, u8); 4] = [(16, 16), (32, 24), (24, 24), (32, 32)];
const RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Pcm(u8),
    /// EXTENSIBLE (容器, 有效位)
    Ext(u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct TrackFmt {
    rate: u32,
    channels: u16,
    kind: Kind,
}

impl TrackFmt {
    fn container(&self) -> u8 {
        match self.kind {
            Kind::Pcm(b) => b,
            Kind::Ext(c, _) => c,
        }
    }
    fn bits(&self) -> u8 {
        match self.kind {
            Kind::Pcm(b) => b,
            Kind::Ext(_, v) => v,
        }
    }
    fn block_align(&self) -> usize {
        usize::from(self.channels) * usize::from(self.container() / 8)
    }
    fn source(&self) -> SourceFormat {
        match SourceFormat::new(self.rate, self.channels, self.bits()) {
            Ok(f) => f,
            Err(e) => panic!("{e:?}"),
        }
    }
}

const KINDS: [Kind; 6] = [
    Kind::Pcm(16),
    Kind::Pcm(24),
    Kind::Pcm(32),
    Kind::Ext(24, 20),
    Kind::Ext(16, 12),
    Kind::Ext(32, 24),
];

struct Track {
    fmt: TrackFmt,
    /// 交错样本（右对齐 N 位）。
    samples: Vec<i32>,
    /// 解码器应输出的帧数（截断夹具小于总帧数）。
    playable_frames: usize,
    spec: TrackSpec,
}

/// 独立 WAV 写入器：有效位左对齐写入容器，低位补零。
fn write_wav(fmt: &TrackFmt, samples: &[i32], junk: bool, bad_align: bool) -> (Vec<u8>, usize) {
    let c = fmt.container();
    let n = fmt.bits();
    let ba = fmt.block_align() as u16 + u16::from(bad_align);
    let mut fmt_body = Vec::new();
    let tag: u16 = match fmt.kind {
        Kind::Pcm(_) => 1,
        Kind::Ext(..) => 0xFFFE,
    };
    fmt_body.extend_from_slice(&tag.to_le_bytes());
    fmt_body.extend_from_slice(&fmt.channels.to_le_bytes());
    fmt_body.extend_from_slice(&fmt.rate.to_le_bytes());
    fmt_body.extend_from_slice(&(fmt.rate * u32::from(ba)).to_le_bytes());
    fmt_body.extend_from_slice(&ba.to_le_bytes());
    fmt_body.extend_from_slice(&u16::from(c).to_le_bytes());
    if let Kind::Ext(_, v) = fmt.kind {
        fmt_body.extend_from_slice(&22u16.to_le_bytes());
        fmt_body.extend_from_slice(&u16::from(v).to_le_bytes());
        let mask: u32 = if fmt.channels == 1 { 0x4 } else { 0x3 };
        fmt_body.extend_from_slice(&mask.to_le_bytes());
        fmt_body.extend_from_slice(&[
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38,
            0x9B, 0x71,
        ]);
    }
    let mut data = Vec::with_capacity(samples.len() * usize::from(c / 8));
    for &s in samples {
        let word = ((i64::from(s)) << (c - n)) as u32;
        data.extend_from_slice(&word.to_le_bytes()[..usize::from(c / 8)]);
    }
    fn chunk(body: &mut Vec<u8>, id: &[u8], b: &[u8]) {
        body.extend_from_slice(id);
        body.extend_from_slice(&(b.len() as u32).to_le_bytes());
        body.extend_from_slice(b);
        if b.len() % 2 == 1 {
            body.push(0);
        }
    }
    let mut body = b"WAVE".to_vec();
    if junk {
        chunk(&mut body, b"JUNK", b"qyfi-sim");
    }
    chunk(&mut body, b"fmt ", &fmt_body);
    chunk(&mut body, b"LIST", b"INFOz");
    // data 体在最终文件中的偏移：RIFF 头 8 + 当前 body + data 头 8。
    let data_offset = 8 + body.len() + 8;
    chunk(&mut body, b"data", &data);
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    (out, data_offset)
}

fn gen_track(rng: &mut Rng, fmt: TrackFmt, max_ms: u64) -> Track {
    let frames = match rng.range(0, 19) {
        0 => 0,
        1..=5 => rng.range(1, u64::from(fmt.rate) / 20),
        _ => rng.range(
            u64::from(fmt.rate) / 20,
            u64::from(fmt.rate) * max_ms / 1000,
        ),
    } as usize;
    let ch = usize::from(fmt.channels);
    let n = u32::from(fmt.bits());
    let lo = -(1i64 << (n - 1));
    let hi = (1i64 << (n - 1)) - 1;
    let lead = if frames > 0 {
        rng.range(0, (frames as u64 / 4).min(u64::from(fmt.rate) / 20)) as usize
    } else {
        0
    };
    let mut samples = Vec::with_capacity(frames * ch);
    for f in 0..frames {
        for _ in 0..ch {
            let s = if f < lead {
                0 // 前导静音必须原样保留
            } else if f + 1 == frames {
                hi // 特征尾帧：最后一帧满幅，截掉即可发现
            } else {
                (lo + (rng.next_u64() % ((hi - lo + 1) as u64)) as i64) as i32 as i64
            };
            samples.push(s as i32);
        }
    }
    let junk = rng.chance(300);
    let (data, _) = write_wav(&fmt, &samples, junk, false);
    Track {
        fmt,
        samples,
        playable_frames: frames,
        spec: TrackSpec {
            data,
            open_fails: false,
            read_error_at: None,
        },
    }
}

fn pick_fmt(rng: &mut Rng, max_rate: u32) -> TrackFmt {
    let rates: Vec<u32> = RATES.iter().copied().filter(|&r| r <= max_rate).collect();
    TrackFmt {
        rate: rates[rng.range(0, rates.len() as u64 - 1) as usize],
        channels: rng.range(1, 2) as u16,
        kind: KINDS[rng.range(0, KINDS.len() as u64 - 1) as usize],
    }
}

fn normal_reads() -> ReadModel {
    ReadModel {
        zero_per_mille: 50,
        tiny_per_mille: 100,
        slow_per_mille: 30,
        slow_max_ticks: 2,
    }
}

fn duration_ms(tracks: &[Track]) -> u64 {
    tracks
        .iter()
        .map(|t| (t.playable_frames as u64 * 1000).div_ceil(u64::from(t.fmt.rate)))
        .sum()
}

#[derive(Debug, Clone, Copy)]
enum Expect {
    /// 全部曲目逐字节匹配、无欠载。
    Normal,
    /// 欠载必须报告；有效音频仍完整。
    Underrun,
    /// 曲目 `k` 失败；`partial` 为真时曲目 k 的输出只需是前缀。
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

fn build_case(seed: u64) -> Case {
    let mut rng = Rng::new(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) ^ 0x51F1);
    let kind = seed % 10;
    let mut supported = DEFAULT_SUPPORTED.to_vec();
    let mut reads = normal_reads();
    let mut stalls = Vec::new();
    let mut disconnect_at = None;
    let mut restart = None;
    let mut device = if rng.chance(250) {
        DeviceModel::ShortWrite
    } else {
        DeviceModel::Wasapi
    };
    let mut tracks: Vec<Track>;
    let expect;

    let gen_list = |rng: &mut Rng, max_rate: u32, max_ms: u64| -> Vec<Track> {
        let pool = [pick_fmt(rng, max_rate), pick_fmt(rng, max_rate)];
        let count = rng.range(1, 4) as usize;
        let mut v = Vec::new();
        for _ in 0..count {
            // 偏向同格式连续曲目，以覆盖无缝衔接。
            let fmt = if rng.chance(650) { pool[0] } else { pool[1] };
            v.push(gen_track(rng, fmt, max_ms));
        }
        v
    };

    match kind {
        0..=5 => {
            tracks = gen_list(&mut rng, 192_000, 400);
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
        6 => {
            device = DeviceModel::Wasapi;
            let fmt = TrackFmt {
                rate: 192_000,
                channels: 2,
                kind: if rng.chance(500) {
                    Kind::Pcm(32)
                } else {
                    Kind::Ext(32, 24)
                },
            };
            tracks = Vec::new();
            for _ in 0..2 {
                let mut t = gen_track(&mut rng, fmt, 800);
                // 保证总时长足以在停顿中耗尽 500 ms 队列。
                while t.playable_frames < 96_000 {
                    t = gen_track(&mut rng, fmt, 800);
                }
                tracks.push(t);
            }
            stalls.push((rng.range(20, 100), 1000));
            expect = Expect::Underrun;
        }
        7 => {
            tracks = gen_list(&mut rng, 192_000, 300);
            let k = rng.range(0, tracks.len() as u64 - 1) as usize;
            let k32 = k as u32;
            match (seed / 10) % 4 {
                0 => {
                    tracks[k].spec.open_fails = true;
                    tracks[k].playable_frames = 0;
                    expect = Expect::Fault {
                        track: k32,
                        fault: Fault::Open,
                        partial: false,
                    };
                }
                1 => {
                    let len = tracks[k].spec.data.len();
                    tracks[k].spec.read_error_at = Some(rng.range(0, len as u64 - 1) as usize);
                    expect = Expect::Fault {
                        track: k32,
                        fault: Fault::Read,
                        partial: true,
                    };
                }
                2 => {
                    // 截断：保留若干完整帧 + 半帧。
                    let t = &mut tracks[k];
                    let ba = t.fmt.block_align();
                    let (data, data_start) = write_wav(&t.fmt, &t.samples, false, false);
                    t.spec.data = data;
                    let keep = rng.range(0, (t.playable_frames as u64).saturating_sub(1)) as usize;
                    let partial = rng.range(1, ba as u64 - 1) as usize;
                    // 无音频时截在 data 头内部，否则截在第 keep 帧中间。
                    let cut = if t.playable_frames == 0 {
                        data_start - 1
                    } else {
                        data_start + keep * ba + partial
                    };
                    t.spec.data.truncate(cut);
                    t.playable_frames = keep;
                    expect = Expect::Fault {
                        track: k32,
                        fault: Fault::Decode(DecodeError::Truncated),
                        partial: false,
                    };
                }
                _ => {
                    let t = &mut tracks[k];
                    let (data, _) = write_wav(&t.fmt, &t.samples, false, true);
                    t.spec.data = data;
                    t.playable_frames = 0;
                    expect = Expect::Fault {
                        track: k32,
                        fault: Fault::Decode(DecodeError::Malformed),
                        partial: false,
                    };
                }
            }
        }
        8 => {
            tracks = gen_list(&mut rng, 192_000, 300);
            if (seed / 10) % 2 == 0 {
                // 设备不支持 32 位有效位：第一首 N=32 且有音频的曲目配置失败。
                supported = vec![(16, 16), (32, 24), (24, 24)];
                let k = rng.range(0, tracks.len() as u64 - 1) as usize;
                let mut fmt = tracks[k].fmt;
                fmt.kind = Kind::Pcm(32);
                let mut t = gen_track(&mut rng, fmt, 300);
                while t.playable_frames == 0 {
                    t = gen_track(&mut rng, fmt, 300);
                }
                tracks[k] = t;
                let first = tracks
                    .iter()
                    .position(|t| t.fmt.bits() == 32 && t.playable_frames > 0)
                    .unwrap_or(k);
                tracks[first].playable_frames = 0;
                expect = Expect::Fault {
                    track: first as u32,
                    fault: Fault::Configure,
                    partial: false,
                };
            } else {
                let dur = duration_ms(&tracks);
                disconnect_at = Some(rng.range(5, dur.max(10)));
                expect = Expect::DeviceLost;
            }
        }
        _ => {
            tracks = gen_list(&mut rng, 96_000, 400);
            reads = ReadModel {
                zero_per_mille: 50,
                tiny_per_mille: 50,
                slow_per_mille: 1000,
                slow_max_ticks: 2,
            };
            let dur = duration_ms(&tracks);
            let from = rng.range(0, tracks.len() as u64 - 1) as u32;
            restart = Some((rng.range(1, (dur / 3).max(2)), from));
            expect = Expect::Restart { from };
        }
    }

    let dur = duration_ms(&tracks);
    let stall_total: u64 = stalls.iter().map(|s| s.1).sum();
    let scenario = Scenario {
        seed,
        tracks: tracks.iter().map(|t| t.spec.clone()).collect(),
        reads,
        device,
        supported,
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

/// 期望的设备段：按设备格式分组相邻曲目，字节由独立公式计算。
fn expected_segments(
    tracks: &[Track],
    supported: &[(u8, u8)],
    frames_of: impl Fn(usize) -> usize,
) -> Vec<(DeviceFormat, Vec<u8>)> {
    let mut segs: Vec<(DeviceFormat, Vec<u8>)> = Vec::new();
    for (i, t) in tracks.iter().enumerate() {
        let frames = frames_of(i);
        if frames == 0 {
            continue;
        }
        let Some(dev) = choose_device_format(&t.fmt.source(), supported) else {
            break;
        };
        let c = u32::from(dev.container_bits());
        let n = u32::from(t.fmt.bits());
        let mut bytes = Vec::new();
        for &s in &t.samples[..frames * usize::from(t.fmt.channels)] {
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

fn actual_segments(report: &Report, epoch: u32) -> Vec<(DeviceFormat, Vec<u8>)> {
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
    let first_diff = actual
        .iter()
        .zip(expected)
        .position(|(a, e)| a != e)
        .map(|i| {
            let (a, e) = (&actual[i].1, &expected[i].1);
            let at = a
                .iter()
                .zip(e)
                .position(|(x, y)| x != y)
                .unwrap_or(a.len().min(e.len()));
            format!("段 {i} 字节 {at} 起不同")
        })
        .unwrap_or_default();
    Err(format!(
        "输出不一致 {first_diff}\n实际 [{}]\n期望 [{}]",
        describe(actual),
        describe(expected)
    ))
}

/// 实际输出是期望的前缀：除最后一段外完全相同，最后一段同格式且为前缀。
fn prefix(
    actual: &[(DeviceFormat, Vec<u8>)],
    expected: &[(DeviceFormat, Vec<u8>)],
) -> Result<(), String> {
    let err = || {
        Err(format!(
            "输出不是期望的前缀\n实际 [{}]\n期望 [{}]",
            describe(actual),
            describe(expected)
        ))
    };
    if actual.len() > expected.len() {
        return err();
    }
    for (i, (a, e)) in actual.iter().zip(expected).enumerate() {
        let last = i + 1 == actual.len();
        if a.0 != e.0 || (!last && a.1 != e.1) || (last && !e.1.starts_with(&a.1)) {
            return err();
        }
    }
    Ok(())
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
    let all = |upto: usize| {
        expected_segments(&case.tracks[..upto], &sc.supported, |i| {
            case.tracks[i].playable_frames
        })
    };
    let n = case.tracks.len();
    let actual = actual_segments(r, 0);
    for seg in &r.segments {
        let packet = u64::from(seg.format.sample_rate() / 100);
        if seg.tail_padding_frames >= packet && sc.device == DeviceModel::Wasapi {
            return Err(format!(
                "尾部填充 {} ≥ 一包 {packet}",
                seg.tail_padding_frames
            ));
        }
        if sc.device == DeviceModel::ShortWrite && seg.tail_padding_frames != 0 {
            return Err("短写后端不应补零".into());
        }
    }
    match case.expect {
        Expect::Normal => {
            if !r.underruns.is_empty() {
                return Err(format!("预算内出现欠载 {:?}", r.underruns));
            }
            if r.fault.is_some() || !r.finished || !r.fidelity_intact() {
                return Err(format!(
                    "未正常完成 fault={:?} finished={}",
                    r.fault, r.finished
                ));
            }
            exact(&actual, &all(n))
        }
        Expect::Underrun => {
            if r.underruns.is_empty() {
                return Err("超预算停顿未报告欠载".into());
            }
            if r.underruns.iter().any(|u| u.missing_frames == 0) {
                return Err("欠载记录缺失帧数为 0".into());
            }
            if r.fidelity_intact() {
                return Err("欠载后仍标记完整保真".into());
            }
            if r.fault.is_some() || !r.finished {
                return Err(format!("欠载后未恢复完成 fault={:?}", r.fault));
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
                let upto = expected_segments(&case.tracks[..=k], &sc.supported, |i| {
                    if i == k {
                        case.tracks[i].samples.len() / usize::from(case.tracks[i].fmt.channels)
                    } else {
                        case.tracks[i].playable_frames
                    }
                });
                if total(&actual) < total(&before) {
                    return Err("故障前曲目未完整排空".into());
                }
                prefix(&actual, &upto)
            } else {
                exact(&actual, &all(k + 1))
            }
        }
        Expect::DeviceLost => {
            if !r.device_lost && !r.finished {
                return Err("设备断开未报告".into());
            }
            if r.device_lost && r.fidelity_intact() {
                return Err("断开后仍标记完整保真".into());
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
                    "重启后未正常完成 finished={} fault={:?} underruns={}",
                    r.finished,
                    r.fault,
                    r.underruns.len()
                ));
            }
            if r.restarted_at.is_some() && r.reads_in_flight_at_restart == 0 {
                return Err("重启时没有在途读取，未覆盖旧回报拒绝".into());
            }
            if r.stale_rejected < r.reads_in_flight_at_restart {
                return Err(format!(
                    "在途旧读取 {} 未全部拒绝（拒绝 {}）",
                    r.reads_in_flight_at_restart, r.stale_rejected
                ));
            }
            if r.restarted_at.is_none() {
                // 重启点晚于播放结束：等价于正常场景。
                return exact(&actual, &all(n));
            }
            prefix(&actual, &all(n))?;
            let after = expected_segments(&case.tracks[from as usize..], &sc.supported, |i| {
                case.tracks[from as usize + i].playable_frames
            });
            exact(&actual_segments(r, 1), &after)
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

#[test]
fn thousand_seeds() {
    let Ok(mut host) = SimHost::new() else {
        panic!("host");
    };
    let seeds: Vec<u64> = match std::env::var("QY_SIM_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        Some(s) => vec![s],
        None => (0..SEEDS).collect(),
    };
    let mut failures = Vec::new();
    let mut stats = [0u32; 10];
    let mut underrun_events = 0usize;
    let mut stale = 0u32;
    let mut restarts = 0u32;
    let mut lost = 0u32;
    let mut faults = std::collections::BTreeMap::<String, u32>::new();
    let mut partial_nonempty = 0u32;
    for &seed in &seeds {
        let _guard = SeedGuard(seed);
        let case = build_case(seed);
        let report = run(&mut host, &case.scenario);
        stats[(seed % 10) as usize] += 1;
        underrun_events += report.underruns.len();
        stale += report.stale_rejected;
        restarts += u32::from(report.restarted_at.is_some());
        lost += u32::from(report.device_lost);
        if let Some((_, f)) = report.fault {
            *faults.entry(format!("{f:?}")).or_default() += 1;
            if matches!(f, Fault::Read) && report.segments.iter().any(|s| !s.valid.is_empty()) {
                partial_nonempty += 1;
            }
        }
        if let Err(e) = verify(&case, &report) {
            failures.push(format!("种子 {seed}（类型 {}）: {e}", seed % 10));
        }
    }
    eprintln!(
        "模拟种子 {}：各类型 {:?}，欠载事件 {underrun_events}，拒绝旧回报 {stale}，\
         实际重启 {restarts}，设备断开 {lost}，故障 {faults:?}，读错误前已有输出 {partial_nonempty}",
        seeds.len(),
        stats
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
