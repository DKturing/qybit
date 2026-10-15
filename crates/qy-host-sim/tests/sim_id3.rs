//! 文件开头的 ID3v2 标签：真实核心、自有解码、pack 与队列（只模拟文件、时间与设备）下，带标签的
//! FLAC/WAV 与无标签文件输出相同的设备字节；种子 1–4 × 两种设备模型，读取含零长度读、短读与慢读。
//! 标签在测试内按 ID3v2.4.0 结构规范 §3.1 独立构造；期望样本为 libFLAC 参考 `.s32le` 与 WAV `.expect`。

use std::path::PathBuf;

use qy_codec_auto::profile::DESKTOP;
use qy_core::engine::ErrorPolicy;
use qy_host_sim::SimHost;
use qy_host_sim::sim::{DeviceModel, ReadModel, Scenario, TrackSpec, run};

const SEEDS: [u64; 4] = [1, 2, 3, 4];
const DEVICES: [DeviceModel; 2] = [DeviceModel::Wasapi, DeviceModel::ShortWrite];

fn read(rel: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// ID3v2 标签：声明长度 `body`（syncsafe），`footer` 时带 v2.4 标签尾。
fn id3(major: u8, body: u32, footer: bool) -> Vec<u8> {
    let flags = if footer { 0x10 } else { 0 };
    let size = [21, 14, 7, 0].map(|s| (body >> s) as u8 & 0x7F);
    let mut t = vec![b'I', b'D', b'3', major, 0, flags];
    t.extend_from_slice(&size);
    t.resize(10 + body as usize, 0x55);
    if footer {
        t.extend_from_slice(&[b'3', b'D', b'I', major, 0, flags]);
        t.extend_from_slice(&size);
    }
    t
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

#[test]
fn tagged_tracks_play_bit_exact() {
    let flac = read("../qy-codec-flac/tests/fixtures/cli_16_stereo_44k_default.flac");
    let flac_ref: Vec<i32> =
        read("../qy-codec-flac/tests/fixtures/cli_16_stereo_44k_default.s32le")
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
    let wav = read("../qy-codec-wav/tests/fixtures/pcm16_stereo_44k.wav");
    let wav_ref: Vec<i32> = String::from_utf8(read(
        "../qy-codec-wav/tests/fixtures/pcm16_stereo_44k.expect",
    ))
    .unwrap_or_default()
    .lines()
    .skip(6)
    .flat_map(|l| {
        l.split(' ')
            .map(|x| x.parse::<i32>().unwrap_or(i32::MIN))
            .collect::<Vec<_>>()
    })
    .collect();
    assert!(!flac_ref.is_empty() && !wav_ref.is_empty());
    // 播放列表：标签 FLAC、无标签 WAV、标签 WAV（v2.4 + 标签尾）、连续两个标签的 FLAC。
    let tracks = [
        [id3(3, 4000, false), flac.clone()].concat(),
        wav.clone(),
        [id3(4, 700, true), wav.clone()].concat(),
        [id3(3, 5, false), id3(4, 20_000, false), flac.clone()].concat(),
    ];
    let want: Vec<i32> = [&flac_ref, &wav_ref, &wav_ref, &flac_ref]
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
            let s = Scenario {
                seed,
                tracks: tracks
                    .iter()
                    .map(|d| TrackSpec {
                        data: d.clone(),
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
                supported: vec![(16, 16), (32, 24), (24, 24), (32, 32)],
                stalls: Vec::new(),
                disconnect_at: None,
                restart: None,
                max_ticks: 200_000,
                on_error: ErrorPolicy::Stop,
                controls: Vec::new(),
                device_layouts: None,
            };
            let r = run(&mut h, &s);
            let what = format!("种子 {seed} {device:?}");
            assert!(
                r.finished && r.fault.is_none() && r.skipped.is_empty() && !r.timed_out,
                "{what}: {:?} {:?}",
                r.fault,
                r.skipped
            );
            assert!(r.underruns.is_empty(), "{what}: 欠载");
            assert_eq!((r.protocol_errors, r.output_errors), (0, 0), "{what}");
            assert!(r.fidelity_intact() && r.repairs.is_empty(), "{what}");
            // 四首同为 44.1 kHz / 双声道 / 16 位：同一段连续播放。
            assert_eq!(r.segments.len(), 1, "{what}: 段数");
            let seg = &r.segments[0];
            let c = u32::from(seg.format.container_bits());
            assert!(
                seg.valid == device_bytes(&want, 16, c),
                "{what}: 设备字节不一致"
            );
        }
    }
}
