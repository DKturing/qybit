//! F3：浮点 WAV 的模拟场景（真实核心、自有解码、pack、转换与队列；只模拟文件、时间与设备）。
//!
//! 夹具与期望位模式取自 `qy-codec-wav/tests/fixtures/float`（`qy-gen-fixtures wav` 生成）。
//! - 严格路径、设备接受同一浮点编码：设备字节等于写入的位模式（小端），NaN 与无穷原样保留并计数；
//! - 严格路径、设备只接受整数：按设备不支持拒绝；
//! - 明确开启转换（`FloatPolicy::ToInt`）：按 32 → 24 → 16 位依次协商，转为设备有效位 V 的整数。
//!   V = 32 / 16 时设备字节与 ffmpeg 的转换参考逐字节相同，V = 24 时与测试内按定义独立计算的结果
//!   相同；报告“样本已改变”及限幅、不精确计数；
//! - 转换遇到 NaN：该曲目在第一个非有限样本所在帧停止，此前的帧照常播放；
//! - 配置档不提供转换工作区时按资源不足拒绝；
//! - 浮点与整数曲目混排。
//!
//! 种子 1–4 × 两种设备模型（WASAPI 整包 / 短写），读取含零长度读、短读与慢读。

use std::path::PathBuf;

use qy_codec_auto::profile::{DESKTOP, EMBEDDED_LITE};
use qy_core::engine::{CapabilityError, EngineFault, ErrorPolicy, FloatPolicy};
use qy_core::{Fault, SampleEncoding};
use qy_host_sim::SimHost;
use qy_host_sim::sim::{DeviceModel, ReadModel, Report, Scenario, TrackSpec, run};

const SEEDS: [u64; 4] = [1, 2, 3, 4];
const DEVICES: [DeviceModel; 2] = [DeviceModel::Wasapi, DeviceModel::ShortWrite];
const FLOATS: [SampleEncoding; 2] = [SampleEncoding::Float32, SampleEncoding::Float64];

struct Fx {
    data: Vec<u8>,
    encoding: SampleEncoding,
    /// 样本位模式（binary32 存低 32 位）。
    bits: Vec<u64>,
}

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../qy-codec-wav/tests/fixtures/float")
}

fn read(p: PathBuf) -> Vec<u8> {
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn fx(name: &'static str) -> Fx {
    let text = String::from_utf8(read(dir().join(format!("{name}.expect")))).unwrap_or_default();
    let encoding = match text.lines().nth(1).and_then(|l| l.split(' ').nth(3)) {
        Some("f32") => SampleEncoding::Float32,
        Some("f64") => SampleEncoding::Float64,
        e => panic!("{name}: 编码 {e:?}"),
    };
    let bits = text
        .lines()
        .skip(5)
        .flat_map(|l| {
            l.split(' ')
                .filter(|x| !x.is_empty())
                .map(|h| u64::from_str_radix(h, 16).unwrap_or(0))
                .collect::<Vec<_>>()
        })
        .collect();
    Fx {
        data: read(dir().join(format!("{name}.wav"))),
        encoding,
        bits,
    }
}

fn value(b: u64, enc: SampleEncoding) -> f64 {
    match enc {
        SampleEncoding::Float32 => f64::from(f32::from_bits(b as u32)),
        _ => f64::from_bits(b),
    }
}

/// F3 §4 的定义，`f64` 与 `i128` 独立计算：(值, 限幅, 不精确)。
fn reference(x: f64, v: u32) -> (i64, bool, bool) {
    let y = x * 2f64.powi(v as i32 - 1);
    let r = y.round_ties_even();
    let max = ((1i64 << (v - 1)) - 1) as f64;
    let min = -((1i64 << (v - 1)) as f64);
    let (c, clipped) = if r > max {
        (max, true)
    } else if r < min {
        (min, true)
    } else {
        (r, false)
    };
    (c as i64, clipped, clipped || r != y)
}

fn float_bytes(f: &Fx) -> Vec<u8> {
    f.bits
        .iter()
        .flat_map(|&b| match f.encoding {
            SampleEncoding::Float32 => (b as u32).to_le_bytes().to_vec(),
            _ => b.to_le_bytes().to_vec(),
        })
        .collect()
}

/// 转为 V 位整数后放入 C 位容器（WASAPI：小端、高位对齐）的独立参考字节。
fn int_bytes(f: &Fx, c: u32, v: u32) -> Vec<u8> {
    f.bits
        .iter()
        .flat_map(|&b| {
            let (s, _, _) = reference(value(b, f.encoding), v);
            let w = ((s << (c - v)) as u64).to_le_bytes();
            w[..(c / 8) as usize].to_vec()
        })
        .collect()
}

fn scenario(
    seed: u64,
    device: DeviceModel,
    tracks: Vec<TrackSpec>,
    supported: &[(u8, u8)],
    floats: &[SampleEncoding],
    on_error: ErrorPolicy,
) -> Scenario {
    Scenario {
        seed,
        tracks,
        reads: ReadModel {
            zero_per_mille: 30,
            tiny_per_mille: 80,
            slow_per_mille: 30,
            slow_max_ticks: 20,
        },
        device,
        supported: supported.to_vec(),
        stalls: Vec::new(),
        disconnect_at: None,
        restart: None,
        max_ticks: 200_000,
        on_error,
        controls: Vec::new(),
        device_layouts: None,
        float_formats: floats.to_vec(),
    }
}

fn spec(data: &[u8]) -> TrackSpec {
    TrackSpec {
        data: data.to_vec(),
        open_fails: false,
        read_error_at: None,
        unseekable: false,
    }
}

fn host(policy: FloatPolicy) -> SimHost {
    let mut h = match SimHost::with_profile(&DESKTOP) {
        Ok(h) => h,
        Err(e) => panic!("{e:?}"),
    };
    h.engine.set_float_policy(policy);
    h
}

fn clean(r: &Report, what: &str) {
    assert!(
        r.finished && r.fault.is_none() && r.skipped.is_empty() && !r.timed_out,
        "{what}: {:?} {:?}",
        r.fault,
        r.skipped
    );
    assert!(r.underruns.is_empty(), "{what}: 欠载");
    assert_eq!((r.protocol_errors, r.output_errors), (0, 0), "{what}");
}

fn all_bytes(r: &Report) -> Vec<u8> {
    r.segments.iter().flat_map(|s| s.valid.clone()).collect()
}

const OK: [&str; 7] = [
    "f32_stereo_44k_tag3",
    "f32_mono_48k_ext",
    "f64_stereo_96k_tag3",
    "f64_51_48k_ext",
    "f32_fact_first_list",
    "f32_specials",
    "f64_specials",
];

/// 严格路径：设备接受同一浮点编码时按位输出（含 NaN 与无穷，计数）；设备只接受整数时拒绝。
#[test]
fn strict_float_bit_exact_or_rejected() {
    let mut h = host(FloatPolicy::Strict);
    for name in OK {
        let f = fx(name);
        let nonfinite = f
            .bits
            .iter()
            .filter(|&&b| !value(b, f.encoding).is_finite())
            .count() as u64;
        for seed in SEEDS {
            for device in DEVICES {
                let what = format!("{name} 种子 {seed} {device:?}");
                let r = run(
                    &mut h,
                    &scenario(
                        seed,
                        device,
                        vec![spec(&f.data)],
                        &[(16, 16), (32, 32)],
                        &FLOATS,
                        ErrorPolicy::Stop,
                    ),
                );
                clean(&r, &what);
                assert_eq!(r.segments.len(), 1, "{what}");
                assert_eq!(r.segments[0].format.encoding(), f.encoding, "{what}");
                assert!(all_bytes(&r) == float_bytes(&f), "{what}: 设备字节");
                assert!(r.float_converted.is_empty(), "{what}");
                assert_eq!(r.float_stats.len(), 1, "{what}");
                let s = r.float_stats[0].1;
                assert_eq!(
                    (s.converted_to, s.nonfinite, s.convert.samples),
                    (None, nonfinite, 0),
                    "{what}"
                );
                // 设备不接受该浮点编码：按设备不支持拒绝，不转换。
                let other: Vec<SampleEncoding> =
                    FLOATS.into_iter().filter(|e| *e != f.encoding).collect();
                let r = run(
                    &mut h,
                    &scenario(
                        seed,
                        device,
                        vec![spec(&f.data)],
                        &[(16, 16), (32, 32)],
                        &other,
                        ErrorPolicy::Stop,
                    ),
                );
                assert_eq!(
                    r.fault,
                    Some((0, EngineFault::Core(Fault::Configure))),
                    "{what}"
                );
                assert!(
                    all_bytes(&r).is_empty() && r.float_converted.is_empty(),
                    "{what}"
                );
            }
        }
    }
}

/// 明确开启转换：转为设备支持的有效位最大的整数格式，与 ffmpeg（V = 16 / 32）或独立定义（V = 24）
/// 逐字节一致；报告转换位数与统计。
#[test]
fn convert_to_best_integer_format() {
    let mut h = host(FloatPolicy::ToInt);
    // (设备支持的整数格式, 期望 (C, V), ffmpeg 参考文件)
    type Case<'a> = (&'a [(u8, u8)], (u32, u32), Option<&'a str>);
    let devices: [Case; 3] = [
        (
            &[(16, 16), (24, 24), (32, 32)],
            (32, 32),
            Some("ffmpeg32.s32le"),
        ),
        (&[(16, 16), (32, 24)], (32, 24), None),
        (&[(16, 16)], (16, 16), Some("ffmpeg16.s16le")),
    ];
    for name in [
        "f32_stereo_44k_tag3",
        "f32_mono_48k_ext",
        "f64_stereo_96k_tag3",
        "f64_51_48k_ext",
        "f32_fact_first_list",
    ] {
        let f = fx(name);
        for (supported, (c, v), ffmpeg) in devices {
            let want = int_bytes(&f, c, v);
            if let Some(file) = ffmpeg {
                assert!(
                    want == read(dir().join(format!("{name}.{file}"))),
                    "{name}: 独立定义与 ffmpeg 不一致"
                );
            }
            let (clipped, inexact) = f.bits.iter().fold((0u64, 0u64), |(c0, i0), &b| {
                let (_, cl, ie) = reference(value(b, f.encoding), v);
                (c0 + u64::from(cl), i0 + u64::from(ie))
            });
            for seed in SEEDS {
                for device in DEVICES {
                    let what = format!("{name} V{v} 种子 {seed} {device:?}");
                    let r = run(
                        &mut h,
                        &scenario(
                            seed,
                            device,
                            vec![spec(&f.data)],
                            supported,
                            &[],
                            ErrorPolicy::Stop,
                        ),
                    );
                    clean(&r, &what);
                    let seg = &r.segments[0];
                    assert_eq!(
                        (
                            u32::from(seg.format.container_bits()),
                            u32::from(seg.format.valid_bits())
                        ),
                        (c, v),
                        "{what}"
                    );
                    assert!(all_bytes(&r) == want, "{what}: 设备字节");
                    assert_eq!(r.float_converted, vec![(0, v as u8)], "{what}");
                    let s = r.float_stats[0].1;
                    assert_eq!(s.converted_to, Some(v as u8), "{what}");
                    assert_eq!(
                        (s.convert.samples, s.convert.clipped, s.convert.inexact),
                        (f.bits.len() as u64, clipped, inexact),
                        "{what}"
                    );
                }
            }
        }
    }
    // 设备接受浮点时不转换（转换只在设备不接受时发生）。
    let f = fx("f32_mono_48k_ext");
    let r = run(
        &mut h,
        &scenario(
            1,
            DeviceModel::Wasapi,
            vec![spec(&f.data)],
            &[(16, 16)],
            &FLOATS,
            ErrorPolicy::Stop,
        ),
    );
    clean(&r, "接受浮点");
    assert!(all_bytes(&r) == float_bytes(&f) && r.float_converted.is_empty());
}

/// 转换遇到 NaN / 无穷：在第一个非有限样本所在帧停止该曲目（此前的帧已播放），报告位置；
/// 跳过策略下继续下一首。
#[test]
fn nonfinite_stops_track_at_position() {
    let mut h = host(FloatPolicy::ToInt);
    let f = fx("f32_specials");
    let first = f
        .bits
        .iter()
        .position(|&b| !value(b, f.encoding).is_finite())
        .unwrap_or_else(|| panic!("夹具应含非有限值"));
    let int = fx("f32_mono_48k_ext");
    for seed in SEEDS {
        for device in DEVICES {
            let what = format!("种子 {seed} {device:?}");
            let r = run(
                &mut h,
                &scenario(
                    seed,
                    device,
                    vec![spec(&f.data)],
                    &[(16, 16)],
                    &[],
                    ErrorPolicy::Stop,
                ),
            );
            assert_eq!(
                r.fault,
                Some((
                    0,
                    EngineFault::NonFinite {
                        frame: first as u64
                    }
                )),
                "{what}"
            );
            let mut head = int_bytes(&f, 16, 16);
            head.truncate(first * 2);
            assert!(all_bytes(&r) == head, "{what}: 停止前的帧");
            // 跳过：下一首照常转换播放。
            let r = run(
                &mut h,
                &scenario(
                    seed,
                    device,
                    vec![spec(&f.data), spec(&int.data)],
                    &[(16, 16)],
                    &[],
                    ErrorPolicy::Skip,
                ),
            );
            assert!(r.finished && r.fault.is_none(), "{what}: {:?}", r.fault);
            assert_eq!(
                r.skipped,
                vec![(
                    0,
                    EngineFault::NonFinite {
                        frame: first as u64
                    }
                )],
                "{what}"
            );
            let tail: Vec<u8> = r
                .segments
                .iter()
                .filter(|s| s.format.sample_rate() == 48_000)
                .flat_map(|s| s.valid.clone())
                .collect();
            assert!(tail == int_bytes(&int, 16, 16), "{what}: 下一首");
        }
    }
}

/// 配置档不提供转换工作区（Embedded Lite）：开启转换也按资源不足拒绝，原因写明。
#[test]
fn no_workspace_rejects_with_reason() {
    let mut h = match SimHost::with_profile(&EMBEDDED_LITE) {
        Ok(h) => h,
        Err(e) => panic!("{e:?}"),
    };
    h.engine.set_float_policy(FloatPolicy::ToInt);
    let f = fx("f32_mono_48k_ext");
    let r = run(
        &mut h,
        &scenario(
            1,
            DeviceModel::Wasapi,
            vec![spec(&f.data)],
            &[(16, 16)],
            &[],
            ErrorPolicy::Stop,
        ),
    );
    assert_eq!(
        r.fault,
        Some((
            0,
            EngineFault::Capability(CapabilityError::ConvertWorkspace)
        )),
        "{:?}",
        r.fault
    );
}

/// 混排：浮点（转换）、16 位整数、浮点（转换）同为 44.1 kHz 双声道，设备只接受 16 位：三首都送到
/// 16 位设备，字节依次为各自的参考；只有两首浮点曲目报告转换。
#[test]
fn mixed_float_and_integer_playlist() {
    let mut h = host(FloatPolicy::ToInt);
    let fl = fx("f32_stereo_44k_tag3");
    let int_wav = read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../qy-codec-wav/tests/fixtures/pcm16_stereo_44k.wav"),
    );
    let int_expect = String::from_utf8(read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../qy-codec-wav/tests/fixtures/pcm16_stereo_44k.expect"),
    ))
    .unwrap_or_default();
    let int_samples: Vec<i16> = int_expect
        .lines()
        .skip(6)
        .flat_map(|l| {
            l.split(' ')
                .filter(|x| !x.is_empty())
                .map(|x| x.parse::<i16>().unwrap_or(0))
                .collect::<Vec<_>>()
        })
        .collect();
    let int_bytes16: Vec<u8> = int_samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let fl16 = int_bytes(&fl, 16, 16);
    let want = [fl16.clone(), int_bytes16, fl16].concat();
    for seed in SEEDS {
        for device in DEVICES {
            let what = format!("种子 {seed} {device:?}");
            let r = run(
                &mut h,
                &scenario(
                    seed,
                    device,
                    vec![spec(&fl.data), spec(&int_wav), spec(&fl.data)],
                    &[(16, 16)],
                    &[],
                    ErrorPolicy::Stop,
                ),
            );
            clean(&r, &what);
            assert!(all_bytes(&r) == want, "{what}: 设备字节");
            assert_eq!(r.float_converted, vec![(0, 16), (2, 16)], "{what}");
        }
    }
}
