//! F5：AIFF-C `twos`、`sowt`、`fl32` / `fl64` 的模拟场景（真实核心、自有解码、pack、转换与队列；
//! 只模拟文件、时间与设备）。
//!
//! - 整数：每个 F5 整数夹具单独播放，设备字节等于独立公式 `s << (C − N)`；`sowt`、`twos`、AIFF
//!   `NONE` 与 WAV 混排连续成一段；
//! - 浮点严格路径：设备接受同一浮点编码时按位输出（含 NaN 与无穷）；不接受时按设备不支持拒绝；
//! - 明确开启转换（`FloatPolicy::ToInt`）：测试内独立写出的 `fl32` / `fl64` 文件（[−1, 1) 内的有限值）
//!   转为设备有效位 V 的整数，与按 F3 §4 定义独立计算的结果逐字节相同。
//!
//! 夹具与期望取自 `qy-codec-aiff/tests/fixtures`（`qy-gen-fixtures aiff` 生成）。种子 1–4 × 两种设备
//! 模型（WASAPI 整包 / 短写），读取含零长度读、短读与慢读。

use std::path::PathBuf;

use qy_codec_auto::profile::DESKTOP;
use qy_core::engine::{EngineFault, ErrorPolicy, FloatPolicy};
use qy_core::{Fault, SampleEncoding};
use qy_host_sim::SimHost;
use qy_host_sim::sim::{DeviceModel, ReadModel, Report, Scenario, TrackSpec, run};

const SEEDS: [u64; 4] = [1, 2, 3, 4];
const DEVICES: [DeviceModel; 2] = [DeviceModel::Wasapi, DeviceModel::ShortWrite];
const SUPPORTED: [(u8, u8); 4] = [(16, 16), (32, 24), (24, 24), (32, 32)];
const FLOATS: [SampleEncoding; 2] = [SampleEncoding::Float32, SampleEncoding::Float64];

struct Fx {
    data: Vec<u8>,
    bits: u32,
    encoding: SampleEncoding,
    /// 解码器输出的字（整数为样本值；binary64 每样本两个字，先低后高）。
    words: Vec<i32>,
}

fn crates() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn read(p: PathBuf) -> Vec<u8> {
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn aiff(name: &str) -> Fx {
    let d = crates().join("qy-codec-aiff/tests/fixtures");
    let text = String::from_utf8(read(d.join(format!("{name}.expect")))).unwrap_or_default();
    let fmt: Vec<&str> = text
        .lines()
        .nth(1)
        .and_then(|l| l.strip_prefix("format "))
        .unwrap_or_else(|| panic!("{name}: 格式"))
        .split(' ')
        .collect();
    let encoding = match fmt.get(3) {
        None => SampleEncoding::Int,
        Some(&"f32") => SampleEncoding::Float32,
        Some(&"f64") => SampleEncoding::Float64,
        e => panic!("{name}: 编码 {e:?}"),
    };
    let words = text
        .lines()
        .skip(4)
        .flat_map(|l| {
            l.split(' ')
                .filter(|x| !x.is_empty())
                .map(|x| x.parse::<i32>().unwrap_or(i32::MIN))
                .collect::<Vec<_>>()
        })
        .collect();
    Fx {
        data: read(d.join(format!("{name}.aiff"))),
        bits: fmt[2].parse().unwrap_or(0),
        encoding,
        words,
    }
}

fn wav(name: &str) -> Fx {
    let d = crates().join("qy-codec-wav/tests/fixtures");
    let text = String::from_utf8(read(d.join(format!("{name}.expect")))).unwrap_or_default();
    let bits = text
        .lines()
        .nth(1)
        .and_then(|l| l.split(' ').nth(3))
        .and_then(|b| b.parse().ok())
        .unwrap_or(0);
    let words = text
        .lines()
        .skip(6)
        .flat_map(|l| {
            l.split(' ')
                .map(|x| x.parse::<i32>().unwrap_or(i32::MIN))
                .collect::<Vec<_>>()
        })
        .collect();
    Fx {
        data: read(d.join(format!("{name}.wav"))),
        bits,
        encoding: SampleEncoding::Int,
        words,
    }
}

/// 独立公式：样本左对齐放入 C 位容器（小端）。
fn device_bytes(samples: &[i32], n: u32, c: u32) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|&s| {
            let w = (i64::from(s) << (c - n)) as u32;
            w.to_le_bytes()[..(c / 8) as usize].to_vec()
        })
        .collect()
}

/// 浮点设备字节：位模式原样、小端（binary64 的两个字先低后高，即小端字节序）。
fn float_bytes(words: &[i32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

fn scenario(
    seed: u64,
    device: DeviceModel,
    tracks: Vec<TrackSpec>,
    supported: &[(u8, u8)],
    floats: &[SampleEncoding],
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
        on_error: ErrorPolicy::Stop,
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
    let mut h = SimHost::with_profile(&DESKTOP).unwrap_or_else(|e| panic!("{e:?}"));
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

const INTS: [&str; 15] = [
    "aifc_twos_8_stereo",
    "aifc_twos_12_stereo",
    "aifc_twos_16_stereo",
    "aifc_twos_24_mono",
    "aifc_twos_32_stereo",
    "aifc_twos_1_mono",
    "aifc_sowt_16_stereo",
    "aifc_sowt_12_stereo",
    "aifc_sowt_8_mono",
    "aifc_sowt_20_3ch",
    "aifc_sowt_24_stereo",
    "aifc_sowt_32_mono",
    "aifc_sowt_16_3ch",
    "aifc_sowt_20_dirty_pad_offset",
    "aifc_twos_12_dirty_pad_offset",
];

/// 整数类型单独播放：设备字节逐字节等于独立参考，保真完整。
#[test]
fn twos_and_sowt_play_bit_exact() {
    let mut h = host(FloatPolicy::Strict);
    for name in INTS {
        let f = aiff(name);
        for seed in SEEDS {
            for device in DEVICES {
                let what = format!("{name} 种子 {seed} {device:?}");
                let r = run(
                    &mut h,
                    &scenario(seed, device, vec![spec(&f.data)], &SUPPORTED, &[]),
                );
                clean(&r, &what);
                assert!(r.fidelity_intact(), "{what}");
                assert_eq!(r.segments.len(), 1, "{what}");
                let c = u32::from(r.segments[0].format.container_bits());
                assert!(
                    r.segments[0].valid == device_bytes(&f.words, f.bits, c),
                    "{what}: 设备字节"
                );
            }
        }
    }
}

/// `sowt`、`twos`、AIFF `NONE` 与 WAV 混排（44.1 kHz / 16 位 / 双声道）：连续成一段，设备字节为
/// 各曲目依次相接。
#[test]
fn sowt_twos_mixed_with_aiff_and_wav() {
    let tracks = [
        aiff("aifc_sowt_16_stereo"),
        aiff("aiff_16_stereo_44k"),
        aiff("aifc_twos_16_stereo"),
        wav("pcm16_stereo_44k"),
    ];
    let mut h = host(FloatPolicy::Strict);
    for seed in SEEDS {
        for device in DEVICES {
            let what = format!("种子 {seed} {device:?}");
            let r = run(
                &mut h,
                &scenario(
                    seed,
                    device,
                    tracks.iter().map(|f| spec(&f.data)).collect(),
                    &SUPPORTED,
                    &[],
                ),
            );
            clean(&r, &what);
            assert_eq!(r.segments.len(), 1, "{what}: 段数");
            let want: Vec<u8> = tracks
                .iter()
                .flat_map(|f| device_bytes(&f.words, f.bits, 16))
                .collect();
            assert!(r.segments[0].valid == want, "{what}: 设备字节");
        }
    }
}

/// 浮点严格路径：设备接受同一浮点编码时按位输出（含 NaN 与无穷）；不接受时拒绝、不转换。
#[test]
fn aifc_float_strict_bit_exact_or_rejected() {
    let mut h = host(FloatPolicy::Strict);
    for name in [
        "aifc_fl32_stereo",
        "aifc_FL32_stereo",
        "aifc_fl64_mono",
        "aifc_FL64_mono",
    ] {
        let f = aiff(name);
        for seed in SEEDS {
            for device in DEVICES {
                let what = format!("{name} 种子 {seed} {device:?}");
                let r = run(
                    &mut h,
                    &scenario(seed, device, vec![spec(&f.data)], &[(16, 16)], &FLOATS),
                );
                clean(&r, &what);
                assert_eq!(r.segments.len(), 1, "{what}");
                assert_eq!(r.segments[0].format.encoding(), f.encoding, "{what}");
                assert!(all_bytes(&r) == float_bytes(&f.words), "{what}: 设备字节");
                assert!(r.float_converted.is_empty(), "{what}");
                let other: Vec<SampleEncoding> =
                    FLOATS.into_iter().filter(|e| *e != f.encoding).collect();
                let r = run(
                    &mut h,
                    &scenario(seed, device, vec![spec(&f.data)], &[(16, 16)], &other),
                );
                assert_eq!(
                    r.fault,
                    Some((0, EngineFault::Core(Fault::Configure))),
                    "{what}"
                );
                assert!(all_bytes(&r).is_empty(), "{what}");
            }
        }
    }
}

/// 测试内独立写出的 AIFF-C 浮点文件（大端）。
fn aifc_float(tag: &[u8; 4], ch: u16, rate: u32, values: &[f64]) -> Vec<u8> {
    let f64 = tag == b"fl64";
    let mut data = Vec::new();
    for &x in values {
        if f64 {
            data.extend_from_slice(&x.to_be_bytes());
        } else {
            data.extend_from_slice(&(x as f32).to_be_bytes());
        }
    }
    let frames = values.len() / usize::from(ch);
    let lz = u64::from(rate).leading_zeros();
    let mut ext = [0u8; 10];
    ext[..2].copy_from_slice(&(16383 + 63 - lz as u16).to_be_bytes());
    ext[2..].copy_from_slice(&(u64::from(rate) << lz).to_be_bytes());
    let mut comm = Vec::new();
    comm.extend_from_slice(&(ch as i16).to_be_bytes());
    comm.extend_from_slice(&(frames as u32).to_be_bytes());
    comm.extend_from_slice(&(if f64 { 64i16 } else { 32 }).to_be_bytes());
    comm.extend_from_slice(&ext);
    comm.extend_from_slice(tag);
    comm.extend_from_slice(&[0, 0]); // 空 pstring + 填充
    let chunk = |id: &[u8; 4], body: &[u8]| {
        let mut c = id.to_vec();
        c.extend_from_slice(&(body.len() as u32).to_be_bytes());
        c.extend_from_slice(body);
        c
    };
    let mut ssnd = vec![0u8; 8];
    ssnd.extend_from_slice(&data);
    let mut body = b"AIFC".to_vec();
    body.extend(chunk(b"FVER", &0xA280_5140u32.to_be_bytes()));
    body.extend(chunk(b"COMM", &comm));
    body.extend(chunk(b"SSND", &ssnd));
    let mut f = b"FORM".to_vec();
    f.extend_from_slice(&(body.len() as u32).to_be_bytes());
    f.extend(body);
    f
}

/// F3 §4 的定义（与 sim_float.rs 相同，独立计算）：乘以 2^(V−1)，round-half-even，限幅。
fn reference(x: f64, v: u32) -> i64 {
    let r = (x * 2f64.powi(v as i32 - 1)).round_ties_even();
    let max = ((1i64 << (v - 1)) - 1) as f64;
    let min = -((1i64 << (v - 1)) as f64);
    r.clamp(min, max) as i64
}

/// 明确开启转换：AIFF-C 浮点转为设备有效位最大的整数格式，与独立定义逐字节一致。
#[test]
fn aifc_float_converts_when_enabled() {
    let mut h = host(FloatPolicy::ToInt);
    // [−1, 1) 内的有限值：边界、±0、半格点（检验 round-half-even）与伪随机值。
    let mut vals = vec![-1.0, 0.0, -0.0, 0.5, -0.5, 1.5 / 32768.0, 2.5 / 32768.0];
    let mut s = 0x5155_4942u64;
    while vals.len() < 2 * 300 {
        s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        vals.push((s >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0);
    }
    for tag in [b"fl32", b"fl64"] {
        let file = aifc_float(tag, 2, 48_000, &vals);
        // binary32 先舍入到单精度（与写入的文件相同），再按定义转换。
        let xs: Vec<f64> = if tag == b"fl32" {
            vals.iter().map(|&x| f64::from(x as f32)).collect()
        } else {
            vals.clone()
        };
        for (supported, (c, v)) in [
            (&[(16u8, 16u8), (24, 24), (32, 32)][..], (32u32, 32u32)),
            (&[(16, 16), (32, 24)][..], (32, 24)),
            (&[(16, 16)][..], (16, 16)),
        ] {
            let want: Vec<u8> = xs
                .iter()
                .flat_map(|&x| {
                    let w = ((reference(x, v) << (c - v)) as u64).to_le_bytes();
                    w[..(c / 8) as usize].to_vec()
                })
                .collect();
            for seed in SEEDS {
                for device in DEVICES {
                    let what = format!(
                        "{} V{v} 种子 {seed} {device:?}",
                        String::from_utf8_lossy(tag)
                    );
                    let r = run(
                        &mut h,
                        &scenario(seed, device, vec![spec(&file)], supported, &[]),
                    );
                    clean(&r, &what);
                    assert_eq!(r.float_converted, vec![(0, v as u8)], "{what}");
                    assert!(all_bytes(&r) == want, "{what}: 设备字节");
                }
            }
        }
    }
}
