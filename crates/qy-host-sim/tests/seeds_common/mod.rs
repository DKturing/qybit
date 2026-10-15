//! 固定种子回归（`sim_seeds`）与场景 fuzz（`fuzz/fuzz_targets/sim_scenario.rs`）共用的场景生成器与
//! 核对：独立 WAV 写入器、已知样本、按场景类型的期望（逐字节一致或期望前缀）。
//!
//! 场景说明见 `sim_seeds.rs`。
use qy_core::engine::{EngineFault, ErrorPolicy};
use qy_core::{DecodeError, DeviceFormat, Fault, SourceFormat};
use qy_host_sim::sim::{
    Control, DeviceModel, ReadModel, Report, Rng, Scenario, TrackSpec, choose_device_format,
};

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
        chunk(&mut body, b"JUNK", b"qybit-sim");
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
            unseekable: false,
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
    /// S1：播放中按 `Control::Seek` 定位。每个定位后的输出段从某首曲目的目标帧起与参考一致
    /// （之前的段为前缀，最后一段完整），无欠载。
    Seek,
}

pub struct Case {
    tracks: Vec<Track>,
    pub scenario: Scenario,
    expect: Expect,
}

/// 由种子生成场景（固定种子回归用）。
pub fn build_case(seed: u64) -> Case {
    build_case_with(
        seed,
        Rng::new(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) ^ 0x51F1),
    )
}

/// 由给定的随机源生成场景：固定种子回归用 SplitMix64，场景 fuzz 用 fuzzer 输入驱动的随机源
/// （[`Rng::from_tape`]）。`seed % 10` 决定场景类型，`seed` 也作为模拟器自身的随机种子。
pub fn build_case_with(seed: u64, mut rng: Rng) -> Case {
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
    let mut expect;
    let mut controls = Vec::new();

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
            // S1：类型 5 的一半种子在播放中随机定位 1–3 次（目标可超过曲目长度）。
            if kind == 5 && (seed / 10) % 2 == 1 {
                let longest = tracks.iter().map(|t| t.playable_frames).max().unwrap_or(0) as u64;
                for _ in 0..rng.range(1, 3) {
                    let at = rng.range(30, dur.max(40));
                    let frame = rng.range(0, longest + longest / 5 + 1);
                    controls.push((at, Control::Seek(frame)));
                }
                controls.sort_by_key(|c| c.0);
                expect = Expect::Seek;
            }
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
            if (seed / 10).is_multiple_of(2) {
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
        on_error: ErrorPolicy::Stop,
        controls,
        device_layouts: None,
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

/// 第 0 首从第 `start` 帧开始（S1 定位）、其后各首完整时的期望设备段。
fn expected_from(
    tracks: &[Track],
    supported: &[(u8, u8)],
    start: usize,
) -> Vec<(DeviceFormat, Vec<u8>)> {
    let mut segs: Vec<(DeviceFormat, Vec<u8>)> = Vec::new();
    for (i, t) in tracks.iter().enumerate() {
        let from = if i == 0 { start } else { 0 };
        if t.playable_frames <= from {
            continue;
        }
        let Some(dev) = choose_device_format(&t.fmt.source(), supported) else {
            break;
        };
        let (c, n) = (u32::from(dev.container_bits()), u32::from(t.fmt.bits()));
        let ch = usize::from(t.fmt.channels);
        let mut bytes = Vec::new();
        for &s in &t.samples[from * ch..t.playable_frames * ch] {
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

/// 核对不变式，并要求场景覆盖到预定情形（固定种子回归用）。
pub fn verify(case: &Case, r: &Report) -> Result<(), String> {
    check(case, r, true)
}

/// 只核对不变式，不要求场景覆盖到某种情形（场景 fuzz 用）：输入决定的场景可能恰好没有经过
/// 固定种子回归要求覆盖的状态（例如重启时刻没有在途读取），这不是错误。
pub fn verify_invariants(case: &Case, r: &Report) -> Result<(), String> {
    check(case, r, false)
}

/// `coverage` 为真时另外要求场景覆盖了预定的情形（固定种子回归用）。
fn check(case: &Case, r: &Report, coverage: bool) -> Result<(), String> {
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
            if r.fault != Some((track, EngineFault::Core(fault))) {
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
            if coverage && r.restarted_at.is_some() && r.reads_in_flight_at_restart == 0 {
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
        Expect::Seek => {
            if !r.finished || r.fault.is_some() || !r.underruns.is_empty() {
                return Err(format!(
                    "定位后未正常完成 finished={} fault={:?} underruns={}",
                    r.finished,
                    r.fault,
                    r.underruns.len()
                ));
            }
            let seeks: Vec<u64> = r
                .controls_done
                .iter()
                .filter_map(|c| match c.1 {
                    Control::Seek(f) => Some(f),
                    _ => None,
                })
                .collect();
            if seeks.is_empty() {
                return exact(&actual, &all(n));
            }
            prefix(&actual, &all(n))?;
            // 定位作用于当时正在播放的曲目 t（核对时不预设是哪一首）：输出须与某个 t 相符。
            for (k, &f) in seeks.iter().enumerate() {
                let got = actual_segments(r, k as u32 + 1);
                let last = k + 1 == seeks.len();
                let ok = (0..n).any(|t| {
                    let want = expected_from(
                        &case.tracks[t..],
                        &sc.supported,
                        usize::try_from(f).unwrap_or(usize::MAX),
                    );
                    if last {
                        exact(&got, &want).is_ok()
                    } else {
                        prefix(&got, &want).is_ok()
                    }
                });
                if !ok {
                    return Err(format!(
                        "第 {} 次定位（第 {f} 帧）后的输出与任何曲目从目标帧起的参考都不符：[{}]",
                        k + 1,
                        describe(&got)
                    ));
                }
            }
            Ok(())
        }
    }
}
