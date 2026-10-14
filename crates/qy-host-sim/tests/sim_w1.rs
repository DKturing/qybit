//! W1：真实世界 WAV 容错的模拟场景（真实核心、自有解码、pack 与队列；只模拟文件、时间与设备）。
//! 新夹具另建本场景，WAV 1000 种子的夹具池与摘要不变。
//!
//! 期望由 `qy-gen-fixtures` 按构造独立给出（`.expect` 的样本与修正），设备字节按独立公式
//! `s << (C − N)` 放置。每个夹具在种子 1–4 × 两种设备模型（WASAPI 整包 / 短写）下单独播放，
//! 读取含零长度读、短读与慢读；另有一个混排播放列表核对修正按曲目报告、截断曲目按策略跳过。

use std::path::PathBuf;

use qy_codec_auto::profile::DESKTOP;
use qy_core::engine::{EngineFault, ErrorPolicy, Repair};
use qy_core::{DecodeError, Fault};
use qy_host_sim::SimHost;
use qy_host_sim::sim::{DeviceModel, ReadModel, Report, Scenario, TrackSpec, run};

const SEEDS: [u64; 4] = [1, 2, 3, 4];
const DEVICES: [DeviceModel; 2] = [DeviceModel::Wasapi, DeviceModel::ShortWrite];
const SUPPORTED: [(u8, u8); 4] = [(16, 16), (32, 24), (24, 24), (32, 32)];

struct Fx {
    name: &'static str,
    data: Vec<u8>,
    bits: u32,
    samples: Vec<i32>,
    repairs: Vec<String>,
}

fn fixture(name: &'static str) -> Fx {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../qy-codec-wav/tests/fixtures");
    let read = |p: PathBuf| std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    let data = read(d.join(format!("{name}.wav")));
    let text = String::from_utf8(read(d.join(format!("{name}.expect")))).unwrap_or_default();
    let mut lines = text.lines();
    let mut field = |key: &str| -> String {
        let l = lines.next().unwrap_or_default();
        l.strip_prefix(key)
            .unwrap_or_else(|| panic!("{name}: 缺少 {key}"))
            .trim()
            .to_owned()
    };
    let _result = field("result");
    let bits = field("format")
        .split(' ')
        .nth(2)
        .and_then(|b| b.parse().ok())
        .unwrap_or_else(|| panic!("{name}: 格式"));
    let _mask = field("mask");
    let mut repairs: Vec<String> = match field("repairs").as_str() {
        "none" => Vec::new(),
        r => r.split(';').map(str::to_owned).collect(),
    };
    repairs.sort();
    let _detail = field("detail");
    let _count = field("samples");
    let samples = lines
        .flat_map(|l| l.split(' ').map(|x| x.parse::<i32>().unwrap_or(i32::MIN)))
        .collect();
    Fx {
        name,
        data,
        bits,
        samples,
        repairs,
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

/// 第 `track` 首报告的修正（文本，排序）。
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
    }
}

fn spec(fx: &Fx) -> TrackSpec {
    TrackSpec {
        data: fx.data.clone(),
        open_fails: false,
        read_error_at: None,
    }
}

const PLAYABLE: [&str; 17] = [
    "fix_ffmpeg_pipe",
    "fix_pipe_ext24_partial_tail",
    "fix_riff_placeholder",
    "fix_riff_fffffffe",
    "fix_riff_short",
    "fix_riff_is_file_size",
    "fix_riff_long",
    "fix_riff_long_by_2",
    "fix_riff_long_partial_tail",
    "fix_list_after_data_beyond_riff",
    "fix_trailing_chunk_cut",
    "fix_data_partial_frame",
    "fix_data_odd_partial_then_list",
    "fix_ext24_6ch_partial_frame",
    "fix_riff_short_partial_frame",
    "pcm8_stereo_44k",
    "fix_pcm8_partial_frame",
];

/// 每个可播放夹具单独播放：设备字节逐字节等于独立参考，修正与生成器的期望一致，保真完整。
#[test]
fn repaired_files_play_bit_exact_and_report_repairs() {
    let mut h = match SimHost::with_profile(&DESKTOP) {
        Ok(h) => h,
        Err(e) => panic!("{e:?}"),
    };
    for name in PLAYABLE {
        let fx = fixture(name);
        for seed in SEEDS {
            for device in DEVICES {
                let r = run(&mut h, &scenario(seed, device, vec![spec(&fx)]));
                let what = format!("{} 种子 {seed} {device:?}", fx.name);
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
                assert_eq!(r.repairs.len(), fx.repairs.len(), "{what}: 修正条数");
            }
        }
    }
}

/// 混排：修正按曲目编号报告；R5 截断的曲目播放完整帧后按策略跳过；规范文件没有修正。
#[test]
fn mixed_playlist_reports_repairs_per_track() {
    let tracks = [
        fixture("pcm16_stereo_44k"),
        fixture("fix_ffmpeg_pipe"),
        fixture("trunc_data_short_frame_boundary"),
        fixture("fix_riff_short"),
        fixture("fix_data_partial_frame"),
    ];
    let mut h = match SimHost::with_profile(&DESKTOP) {
        Ok(h) => h,
        Err(e) => panic!("{e:?}"),
    };
    for seed in SEEDS {
        for device in DEVICES {
            let r = run(
                &mut h,
                &scenario(seed, device, tracks.iter().map(spec).collect()),
            );
            let what = format!("种子 {seed} {device:?}");
            assert!(r.finished && r.fault.is_none(), "{what}: {:?}", r.fault);
            assert_eq!(
                r.skipped,
                vec![(2, EngineFault::Core(Fault::Decode(DecodeError::Truncated)))],
                "{what}"
            );
            for (i, fx) in tracks.iter().enumerate() {
                assert_eq!(repairs_of(&r, i as u32), fx.repairs, "{what}: 曲目 {i}");
            }
            // 设备字节：全部曲目（截断曲目只有完整帧）按顺序。曲目 1 为单声道，单独成段。
            let all: Vec<u8> = r.segments.iter().flat_map(|s| s.valid.clone()).collect();
            let want: Vec<u8> = tracks
                .iter()
                .flat_map(|fx| device_bytes(&fx.samples, fx.bits, 16))
                .collect();
            assert!(all == want, "{what}: 设备字节不一致");
        }
    }
}
