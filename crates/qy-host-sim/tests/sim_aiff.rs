//! F2：AIFF / AIFF-C 的模拟场景（真实核心、自有解码、pack 与队列；只模拟文件、时间与设备）。
//!
//! 期望样本由 `qy-gen-fixtures aiff` 独立给出（`.expect`），设备字节按独立公式 `s << (C − N)` 放置。
//! 每个可播放夹具在种子 1–4 × 两种设备模型（WASAPI 整包 / 短写）下单独播放，读取含零长度读、短读
//! 与慢读；另有 AIFF、WAV、FLAC 混排（中间夹一个被拒绝的 AIFF，按策略跳过）的播放列表。

use std::path::PathBuf;

use qy_codec_auto::profile::DESKTOP;
use qy_core::engine::{EngineFault, ErrorPolicy};
use qy_core::{DecodeError, Fault};
use qy_host_sim::SimHost;
use qy_host_sim::sim::{DeviceModel, ReadModel, Scenario, TrackSpec, run};

const SEEDS: [u64; 4] = [1, 2, 3, 4];
const DEVICES: [DeviceModel; 2] = [DeviceModel::Wasapi, DeviceModel::ShortWrite];
const SUPPORTED: [(u8, u8); 4] = [(16, 16), (32, 24), (24, 24), (32, 32)];

struct Fx {
    data: Vec<u8>,
    bits: u32,
    samples: Vec<i32>,
}

fn read(p: PathBuf) -> Vec<u8> {
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// 读取 `.expect`：`skip` 行头部之后是样本；格式行为第 2 行（`format 率 声道 位数`）。
fn load(path: PathBuf, skip: usize) -> Fx {
    let text = String::from_utf8(read(path.with_extension("expect"))).unwrap_or_default();
    let bits = text
        .lines()
        .nth(1)
        .and_then(|l| l.split(' ').nth(3))
        .and_then(|b| b.parse().ok())
        .unwrap_or_else(|| panic!("{}: 格式", path.display()));
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
    Fx {
        data: read(path),
        bits,
        samples,
    }
}

fn aiff(name: &str) -> Fx {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../qy-codec-aiff/tests/fixtures");
    load(d.join(format!("{name}.aiff")), 4)
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

fn scenario(seed: u64, device: DeviceModel, tracks: Vec<TrackSpec>) -> Scenario {
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
        supported: SUPPORTED.to_vec(),
        stalls: Vec::new(),
        disconnect_at: None,
        restart: None,
        max_ticks: 200_000,
        on_error: ErrorPolicy::Skip,
        controls: Vec::new(),
        device_layouts: None,
    }
}

fn spec(data: &[u8]) -> TrackSpec {
    TrackSpec {
        data: data.to_vec(),
        open_fails: false,
        read_error_at: None,
    }
}

const PLAYABLE: [&str; 12] = [
    "aiff_16_stereo_44k",
    "aiff_8_mono_8k",
    "aiff_1_mono_44k",
    "aiff_12_stereo_48k",
    "aiff_20_mono_96k",
    "aiff_24_stereo_96k",
    "aiff_32_stereo_192k",
    "aifc_none_16_stereo",
    "aifc_none_24_mono_oddname",
    "aiff_chunks_offset",
    "aiff_ssnd_trailing_bytes",
    "aiff_12_dirty_pad",
];

#[test]
fn aiff_tracks_play_bit_exact() {
    let mut h = match SimHost::with_profile(&DESKTOP) {
        Ok(h) => h,
        Err(e) => panic!("{e:?}"),
    };
    for name in PLAYABLE {
        let fx = aiff(name);
        for seed in SEEDS {
            for device in DEVICES {
                let r = run(&mut h, &scenario(seed, device, vec![spec(&fx.data)]));
                let what = format!("{name} 种子 {seed} {device:?}");
                assert!(
                    r.finished && r.fault.is_none() && r.skipped.is_empty() && !r.timed_out,
                    "{what}: {:?} {:?}",
                    r.fault,
                    r.skipped
                );
                assert!(r.underruns.is_empty(), "{what}: 欠载");
                assert_eq!((r.protocol_errors, r.output_errors), (0, 0), "{what}");
                assert!(r.fidelity_intact(), "{what}");
                assert_eq!(r.segments.len(), 1, "{what}: 段数");
                let seg = &r.segments[0];
                let c = u32::from(seg.format.container_bits());
                assert!(
                    seg.valid == device_bytes(&fx.samples, fx.bits, c),
                    "{what}: 设备字节不一致"
                );
            }
        }
    }
}

/// 混排：AIFF、WAV、FLAC 同为 44.1 kHz / 双声道 / 16 位，连续成一段；中间被拒绝的 AIFF 按策略跳过，
/// 跳过的原因为“不支持”。
#[test]
fn mixed_playlist_with_wav_and_flac() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let a = aiff("aiff_16_stereo_44k");
    let w = load(
        root.join("../qy-codec-wav/tests/fixtures/pcm16_stereo_44k.wav"),
        6,
    );
    let flac = read(root.join("../qy-codec-flac/tests/fixtures/cli_16_stereo_44k_default.flac"));
    let flac_ref: Vec<i32> =
        read(root.join("../qy-codec-flac/tests/fixtures/cli_16_stereo_44k_default.s32le"))
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
    let bad = read(root.join("../qy-codec-aiff/tests/fixtures/ext_sowt.aiff"));
    let tracks = [a.data.clone(), w.data.clone(), bad, flac, a.data.clone()];
    let want: Vec<i32> = [&a.samples, &w.samples, &flac_ref, &a.samples]
        .into_iter()
        .flatten()
        .copied()
        .collect();
    let mut h = match SimHost::with_profile(&DESKTOP) {
        Ok(h) => h,
        Err(e) => panic!("{e:?}"),
    };
    for seed in SEEDS {
        for device in DEVICES {
            let r = run(
                &mut h,
                &scenario(seed, device, tracks.iter().map(|t| spec(t)).collect()),
            );
            let what = format!("种子 {seed} {device:?}");
            assert!(r.finished && r.fault.is_none(), "{what}: {:?}", r.fault);
            assert_eq!(
                r.skipped,
                vec![(
                    2,
                    EngineFault::Core(Fault::Decode(DecodeError::Unsupported))
                )],
                "{what}"
            );
            assert!(r.underruns.is_empty(), "{what}: 欠载");
            let all: Vec<u8> = r.segments.iter().flat_map(|s| s.valid.clone()).collect();
            assert!(all == device_bytes(&want, 16, 16), "{what}: 设备字节不一致");
        }
    }
}

/// 软件不设人为上限，但资源有限：2.8224 MHz 单声道 24 位需要的队列超过 Desktop 配置档，
/// 在打开设备之前按“队列不足”明确拒绝，并给出所需与可用的字节数。
#[test]
fn very_high_rate_is_rejected_with_numbers() {
    let fx = aiff("aiff_24_mono_2822400");
    let mut h = match SimHost::with_profile(&DESKTOP) {
        Ok(h) => h,
        Err(e) => panic!("{e:?}"),
    };
    let r = run(
        &mut h,
        &scenario(1, DeviceModel::Wasapi, vec![spec(&fx.data)]),
    );
    let text = format!("{:?}", r.fault);
    assert!(
        text.contains("Queue")
            && text.contains("required: 4233600")
            && text.contains("available: 3072000"),
        "{text}"
    );
    assert!(r.segments.iter().all(|s| s.valid.is_empty()));
}
