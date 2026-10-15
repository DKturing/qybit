//! O1：单声道复制到左右两路（明确开启的非严格选项）的模拟场景。
//!
//! 真实核心、自有解码、pack 与队列；只模拟文件、时间与设备。期望样本取自夹具的独立 `.expect`，
//! 设备字节按独立公式 `s << (C − N)` 放置：开启时每帧左右两份相同的容器字，关闭时与严格路径相同。
//! 种子 1–4 × 两种设备模型（WASAPI 整包 / 短写），读取含零长度读、短读与慢读。

use std::path::PathBuf;

use qy_codec_auto::profile::DESKTOP;
use qy_core::engine::{ChannelPolicy, EngineFault, ErrorPolicy};
use qy_core::{ChannelLayout, Fault};
use qy_host_sim::SimHost;
use qy_host_sim::sim::{DeviceModel, ReadModel, Scenario, TrackSpec, run};

const SEEDS: [u64; 4] = [1, 2, 3, 4];
const DEVICES: [DeviceModel; 2] = [DeviceModel::Wasapi, DeviceModel::ShortWrite];
const SUPPORTED: [(u8, u8); 4] = [(16, 16), (32, 24), (24, 24), (32, 32)];

struct Fx {
    data: Vec<u8>,
    channels: usize,
    bits: u32,
    samples: Vec<i32>,
}

fn read(p: PathBuf) -> Vec<u8> {
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// 读取 `.expect`：第 2 行为 `format 率 声道 位数`，`skip` 行头部之后是样本。
fn load(path: PathBuf, skip: usize) -> Fx {
    let text = String::from_utf8(read(path.with_extension("expect"))).unwrap_or_default();
    let field = |k: usize| -> u32 {
        text.lines()
            .nth(1)
            .and_then(|l| l.split(' ').nth(k))
            .and_then(|b| b.parse().ok())
            .unwrap_or_else(|| panic!("{}: 格式", path.display()))
    };
    let samples = text
        .lines()
        .skip(skip)
        .flat_map(|l| {
            l.split(' ')
                .filter(|x| !x.is_empty())
                .map(|x| x.parse::<i32>().unwrap_or(i32::MIN))
                .collect::<Vec<_>>()
        })
        .collect();
    let (channels, bits) = (field(2) as usize, field(3));
    Fx {
        data: read(path),
        channels,
        bits,
        samples,
    }
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn wav(name: &str) -> Fx {
    load(
        root().join(format!("qy-codec-wav/tests/fixtures/{name}.wav")),
        6,
    )
}

fn aiff(name: &str) -> Fx {
    load(
        root().join(format!("qy-codec-aiff/tests/fixtures/{name}.aiff")),
        4,
    )
}

/// 独立公式：样本左对齐放入 C 位容器（小端）；`copies` 为每个样本写入的份数。
fn device_bytes(samples: &[i32], n: u32, c: u32, copies: usize) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|&s| {
            let w = (i64::from(s) << (c - n)) as u32;
            w.to_le_bytes()[..(c / 8) as usize].repeat(copies)
        })
        .collect()
}

fn scenario(
    seed: u64,
    device: DeviceModel,
    tracks: &[&Fx],
    layouts: Option<Vec<ChannelLayout>>,
) -> Scenario {
    Scenario {
        seed,
        tracks: tracks
            .iter()
            .map(|f| TrackSpec {
                data: f.data.clone(),
                open_fails: false,
                read_error_at: None,
                unseekable: false,
            })
            .collect(),
        reads: ReadModel {
            zero_per_mille: 30,
            tiny_per_mille: 80,
            slow_per_mille: 30,
            slow_max_ticks: 20,
        },
        device,
        supported: SUPPORTED.to_vec(),
        stalls: Vec::new(),
        disconnect_at: None,
        restart: None,
        max_ticks: 200_000,
        on_error: ErrorPolicy::Skip,
        controls: Vec::new(),
        device_layouts: layouts,
        float_formats: Vec::new(),
    }
}

fn host(policy: ChannelPolicy) -> SimHost {
    let mut h = match SimHost::with_profile(&DESKTOP) {
        Ok(h) => h,
        Err(e) => panic!("{e:?}"),
    };
    h.engine.set_channel_policy(policy);
    h
}

const MONO: [(&str, bool); 4] = [
    ("pcm16_mono_8k_fmt18", false),
    ("pcm24_mono_48k_odd_list", false),
    ("aiff_8_mono_8k", true),
    ("aiff_20_mono_96k", true),
];

fn mono(name: &str, is_aiff: bool) -> Fx {
    if is_aiff { aiff(name) } else { wav(name) }
}

/// 开启时：设备格式为同采样率、同有效位要求的 [FL FR]，每帧两份相同的容器字；报告记下该曲目，
/// 记录不再算严格路径完整；没有欠载与错误。只接受立体声的设备也能播放。
#[test]
fn duplicate_on_plays_each_sample_twice() {
    let stereo_only = Some(vec![ChannelLayout::STEREO]);
    let mut h = host(ChannelPolicy::DuplicateMono);
    for (name, is_aiff) in MONO {
        let fx = mono(name, is_aiff);
        assert_eq!(fx.channels, 1, "{name}");
        for layouts in [None, stereo_only.clone()] {
            for seed in SEEDS {
                for device in DEVICES {
                    let before = h.engine.stats().duplicated_tracks;
                    let r = run(&mut h, &scenario(seed, device, &[&fx], layouts.clone()));
                    let what = format!("{name} 种子 {seed} {device:?} {layouts:?}");
                    assert_eq!(h.engine.stats().duplicated_tracks - before, 1, "{what}");
                    assert!(
                        r.finished && r.fault.is_none() && r.skipped.is_empty() && !r.timed_out,
                        "{what}: {:?} {:?}",
                        r.fault,
                        r.skipped
                    );
                    assert!(r.underruns.is_empty(), "{what}: 欠载");
                    assert_eq!((r.protocol_errors, r.output_errors), (0, 0), "{what}");
                    assert_eq!(r.duplicated, vec![0], "{what}");
                    assert!(!r.fidelity_intact(), "{what}: 声道已改变，不是严格路径");
                    assert_eq!(r.segments.len(), 1, "{what}: 段数");
                    let seg = &r.segments[0];
                    assert_eq!(seg.format.layout(), ChannelLayout::STEREO, "{what}");
                    let c = u32::from(seg.format.container_bits());
                    assert!(
                        seg.valid == device_bytes(&fx.samples, fx.bits, c, 2),
                        "{what}: 设备字节不一致"
                    );
                }
            }
        }
    }
}

/// 关闭时（默认）：与严格路径逐字节相同，设备格式为 [FC]，报告不记“已改变”；只接受立体声的设备
/// 按“设备不支持”拒绝，不静默复制。
#[test]
fn duplicate_off_is_strict() {
    let mut h = host(ChannelPolicy::Strict);
    assert_eq!(h.engine.channel_policy(), ChannelPolicy::Strict);
    for (name, is_aiff) in MONO {
        let fx = mono(name, is_aiff);
        for seed in SEEDS {
            for device in DEVICES {
                let what = format!("{name} 种子 {seed} {device:?}");
                let r = run(&mut h, &scenario(seed, device, &[&fx], None));
                assert!(r.finished && r.fault.is_none(), "{what}: {:?}", r.fault);
                assert!(r.duplicated.is_empty() && r.fidelity_intact(), "{what}");
                let seg = &r.segments[0];
                assert_eq!(seg.format.layout(), ChannelLayout::MONO, "{what}");
                let c = u32::from(seg.format.container_bits());
                assert!(
                    seg.valid == device_bytes(&fx.samples, fx.bits, c, 1),
                    "{what}"
                );
                let r = run(
                    &mut h,
                    &scenario(seed, device, &[&fx], Some(vec![ChannelLayout::STEREO])),
                );
                // 唯一的曲目无法配置设备：按“设备不支持”失败（Configure），不静默复制。
                assert_eq!(
                    r.fault,
                    Some((0, EngineFault::Core(Fault::Configure))),
                    "{what}"
                );
                assert!(r.duplicated.is_empty(), "{what}");
                assert_eq!(h.engine.stats().duplicated_tracks, 0, "{what}");
                assert!(r.segments.iter().all(|s| s.valid.is_empty()), "{what}");
            }
        }
    }
}

/// 开启时只作用于 [FC]：立体声曲目、只占左声道位置的单声道 [FL] 照常按严格路径处理。
/// 混排 [FC] → 立体声 → [FC]（44.1 kHz / 16 位）时三首连续成一段，立体声曲目逐字节不变，
/// 只有两首单声道记为“已改变”。
#[test]
fn duplicate_only_affects_center_mono() {
    let mut h = host(ChannelPolicy::DuplicateMono);
    let center = wav("ext16_mono_mask0");
    let left = wav("ext16_mono_left_mask");
    let stereo = wav("pcm16_stereo_44k");
    for seed in SEEDS {
        for device in DEVICES {
            let what = format!("种子 {seed} {device:?}");
            let r = run(&mut h, &scenario(seed, device, &[&left], None));
            assert!(r.finished && r.fault.is_none(), "{what}: {:?}", r.fault);
            assert!(
                r.duplicated.is_empty() && r.fidelity_intact(),
                "{what}: [FL]"
            );
            assert!(
                r.segments[0].valid == device_bytes(&left.samples, 16, 16, 1),
                "{what}: [FL]"
            );

            let r = run(
                &mut h,
                &scenario(seed, device, &[&center, &stereo, &center], None),
            );
            assert!(r.finished && r.fault.is_none(), "{what}: {:?}", r.fault);
            assert!(r.underruns.is_empty(), "{what}: 欠载");
            assert_eq!(r.duplicated, vec![0, 2], "{what}");
            let mut want = device_bytes(&center.samples, 16, 16, 2);
            want.extend(device_bytes(&stereo.samples, 16, 16, 1));
            want.extend(device_bytes(&center.samples, 16, 16, 2));
            let all: Vec<u8> = r.segments.iter().flat_map(|s| s.valid.clone()).collect();
            assert_eq!(r.segments.len(), 1, "{what}: 同格式应续用设备");
            assert!(all == want, "{what}: 设备字节不一致");
        }
    }
}

/// 开启复制但设备只接受单声道：按立体声请求被拒，曲目配置失败；没有输出，不记“声道已改变”。
#[test]
fn duplicate_not_reported_when_device_rejects() {
    let mut h = host(ChannelPolicy::DuplicateMono);
    let fx = wav("pcm16_mono_8k_fmt18");
    for seed in SEEDS {
        for device in DEVICES {
            let what = format!("种子 {seed} {device:?}");
            let r = run(
                &mut h,
                &scenario(seed, device, &[&fx], Some(vec![ChannelLayout::MONO])),
            );
            assert_eq!(
                r.fault,
                Some((0, EngineFault::Core(Fault::Configure))),
                "{what}"
            );
            assert!(r.duplicated.is_empty(), "{what}");
            assert_eq!(h.engine.stats().duplicated_tracks, 0, "{what}");
            assert!(r.segments.iter().all(|s| s.valid.is_empty()), "{what}");
        }
    }
}
