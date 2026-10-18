//! F4：RF64 / BW64 的模拟场景（真实核心、自有解码、pack 与队列；只模拟文件、时间与设备）。
//!
//! 期望由 `qy-gen-fixtures` 按构造独立给出（`.expect` 的样本与修正）；FLAC 曲目用 libFLAC 参考
//! （`.s32le`）。设备字节按独立公式 `s << (C − N)` 放置。每个 RF64 / BW64 夹具在种子 1–4 × 两种设备
//! 模型（WASAPI 整包 / 短写）下单独播放，读取含零长度读、短读与慢读；另有一个与 AIFF、FLAC 混排的
//! 播放列表，核对连续成一段、设备字节逐字节一致、修正按曲目报告。

use std::path::PathBuf;

use qy_codec_auto::profile::DESKTOP;
use qy_core::engine::{ErrorPolicy, Repair};
use qy_host_sim::SimHost;
use qy_host_sim::sim::{DeviceModel, ReadModel, Report, Scenario, TrackSpec, run};

const SEEDS: [u64; 4] = [1, 2, 3, 4];
const DEVICES: [DeviceModel; 2] = [DeviceModel::Wasapi, DeviceModel::ShortWrite];
const SUPPORTED: [(u8, u8); 4] = [(16, 16), (32, 24), (24, 24), (32, 32)];

struct Fx {
    data: Vec<u8>,
    bits: u32,
    samples: Vec<i32>,
    repairs: Vec<String>,
}

fn crates() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn read(p: PathBuf) -> Vec<u8> {
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// `.expect`（WAV：result/format/mask/repairs/detail/samples；AIFF：result/format/detail/samples）。
fn parse(text: &str, wav: bool) -> (u32, Vec<String>, Vec<i32>) {
    let mut lines = text.lines();
    let mut field = |key: &str| -> String {
        let l = lines.next().unwrap_or_default();
        l.strip_prefix(key)
            .unwrap_or_else(|| panic!("缺少 {key}"))
            .trim()
            .to_owned()
    };
    let _ = field("result");
    let bits = field("format")
        .split(' ')
        .nth(2)
        .and_then(|b| b.parse().ok())
        .unwrap_or_else(|| panic!("格式"));
    let mut repairs = Vec::new();
    if wav {
        let _ = field("mask");
        repairs = match field("repairs").as_str() {
            "none" => Vec::new(),
            r => r.split(';').map(str::to_owned).collect(),
        };
        repairs.sort();
    }
    let _ = field("detail");
    let _ = field("samples");
    let samples = lines
        .flat_map(|l| l.split(' ').map(|x| x.parse::<i32>().unwrap_or(i32::MIN)))
        .collect();
    (bits, repairs, samples)
}

fn wav(name: &str) -> Fx {
    let d = crates().join("qy-codec-wav/tests/fixtures");
    let data = read(d.join(format!("{name}.wav")));
    let text = String::from_utf8(read(d.join(format!("{name}.expect")))).unwrap_or_default();
    let (bits, repairs, samples) = parse(&text, true);
    Fx {
        data,
        bits,
        samples,
        repairs,
    }
}

fn aiff(name: &str) -> Fx {
    let d = crates().join("qy-codec-aiff/tests/fixtures");
    let data = read(d.join(format!("{name}.aiff")));
    let text = String::from_utf8(read(d.join(format!("{name}.expect")))).unwrap_or_default();
    let (bits, _, samples) = parse(&text, false);
    Fx {
        data,
        bits,
        samples,
        repairs: Vec::new(),
    }
}

/// FLAC 夹具与 libFLAC 参考（`.s32le` 为右对齐的样本值，见该目录的 MANIFEST）。
fn flac16(name: &str) -> Fx {
    let d = crates().join("qy-codec-flac/tests/fixtures");
    let data = read(d.join(format!("{name}.flac")));
    let samples = read(d.join(format!("{name}.s32le")))
        .chunks_exact(4)
        .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    Fx {
        data,
        bits: 16,
        samples,
        repairs: Vec::new(),
    }
}

fn repair_text(r: &Repair) -> String {
    let id = |c: [u8; 4]| String::from_utf8_lossy(&c).into_owned();
    match *r {
        Repair::LengthPlaceholder {
            chunk,
            declared,
            read,
        } => format!("{}:placeholder:{declared}:{read}", id(chunk)),
        Repair::LengthMismatch {
            chunk,
            declared,
            actual,
        } => format!("{}:mismatch:{declared}:{actual}", id(chunk)),
        Repair::PartialFrameDropped { bytes } => format!("partial:{bytes}"),
    }
}

fn repairs_of(r: &Report, track: u32) -> Vec<String> {
    let mut v: Vec<String> = r
        .repairs
        .iter()
        .filter(|(t, _)| *t == track)
        .map(|(_, x)| repair_text(x))
        .collect();
    v.sort();
    v
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
        max_ticks: 120_000,
        on_error: ErrorPolicy::Skip,
        controls: Vec::new(),
        device_layouts: None,
        float_formats: Vec::new(),
    }
}

fn spec(fx: &Fx) -> TrackSpec {
    TrackSpec {
        data: fx.data.clone(),
        open_fails: false,
        read_error_at: None,
        unseekable: false,
    }
}

const PLAYABLE: [&str; 9] = [
    "rf64_pcm16_stereo",
    "bw64_pcm24_mono_adm",
    "rf64_table_junk",
    "rf64_ds64_in_riff_skipped",
    "rf64_pipe_placeholder",
    "rf64_riff_longer",
    "rf64_partial_tail",
    "rf64_ffmpeg_seekable",
    "rf64_ffmpeg_pipe",
];

/// 每个可播放的 RF64 / BW64 夹具单独播放：设备字节逐字节等于独立参考，修正与期望一致，保真完整。
#[test]
fn rf64_files_play_bit_exact() {
    let mut h = SimHost::with_profile(&DESKTOP).unwrap_or_else(|e| panic!("{e:?}"));
    for name in PLAYABLE {
        let fx = wav(name);
        for seed in SEEDS {
            for device in DEVICES {
                let r = run(&mut h, &scenario(seed, device, vec![spec(&fx)]));
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
                assert_eq!(repairs_of(&r, 0), fx.repairs, "{what}: 修正");
            }
        }
    }
}

/// 与 AIFF、FLAC、普通 WAV 混排（都是 44.1 kHz / 16 位 / 双声道）：连续成一段，设备字节为各曲目
/// 依次相接，修正只出现在 ffmpeg 管道输出的那一首。
#[test]
fn rf64_mixed_with_aiff_flac_and_wav() {
    let tracks = [
        wav("rf64_pcm16_stereo"),
        aiff("aiff_16_stereo_44k"),
        flac16("cli_16_stereo_44k_default"),
        wav("rf64_ffmpeg_pipe"),
        wav("pcm16_stereo_44k"),
    ];
    let mut h = SimHost::with_profile(&DESKTOP).unwrap_or_else(|e| panic!("{e:?}"));
    for seed in SEEDS {
        for device in DEVICES {
            let r = run(
                &mut h,
                &scenario(seed, device, tracks.iter().map(spec).collect()),
            );
            let what = format!("种子 {seed} {device:?}");
            assert!(
                r.finished && r.fault.is_none() && r.skipped.is_empty() && !r.timed_out,
                "{what}: {:?} {:?}",
                r.fault,
                r.skipped
            );
            assert!(r.fidelity_intact(), "{what}");
            assert_eq!(r.segments.len(), 1, "{what}: 段数");
            for (i, fx) in tracks.iter().enumerate() {
                assert_eq!(repairs_of(&r, i as u32), fx.repairs, "{what}: 曲目 {i}");
            }
            let want: Vec<u8> = tracks
                .iter()
                .flat_map(|fx| device_bytes(&fx.samples, fx.bits, 16))
                .collect();
            assert!(r.segments[0].valid == want, "{what}: 设备字节不一致");
        }
    }
}
