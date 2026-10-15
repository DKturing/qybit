//! F1：多声道模拟场景（真实核心、自有解码、pack 与队列；只模拟文件、时间与设备）。
//!
//! 每个场景用种子 1–4 × 两种设备模型（WASAPI 整包 / 短写），读取含零长度读、短读与慢读：
//! - 多声道与双声道混排：布局不同先排空再重开，每段格式、布局与设备字节逐字节一致；
//! - 设备只支持 ALSA 常见的固定顺序（如 HDA 5.1：FL FR BL BR FC LFE）：pack 按扬声器位置纯置换，
//!   位置集合不同（侧环绕）的源被设备拒绝并按策略跳过；
//! - 未定义布局（WAV 6 声道掩码为 0）按 Unsupported 跳过；
//! - Embedded Full 配置档（双声道）下多声道源按能力不足拒绝，不打开设备。
//!
//! 期望设备字节由独立公式计算：样本取 libFLAC 参考（`.s32le`）或 ffmpeg 参考
//! （`.ffmpeg.s32le` 右移 32 − N），按设备容器 `s << (C − N)` 放置；置换按扬声器位置在测试内
//! 独立展开（掩码位从低到高即源顺序）。

use std::path::PathBuf;

use qy_codec_auto::profile::{DESKTOP, EMBEDDED_FULL};
use qy_core::engine::{CapabilityError, EngineFault, ErrorPolicy};
use qy_core::{ChannelLayout, DecodeError, Fault, Speaker};
use qy_host_sim::SimHost;
use qy_host_sim::sim::{DeviceModel, ReadModel, Report, Scenario, TrackSpec, run};

const SEEDS: [u64; 4] = [1, 2, 3, 4];
const DEVICES: [DeviceModel; 2] = [DeviceModel::Wasapi, DeviceModel::ShortWrite];
const SUPPORTED: [(u8, u8); 4] = [(16, 16), (32, 24), (24, 24), (32, 32)];

/// 一个夹具：文件字节、独立参考样本、声道掩码、源位数。
struct Fx {
    data: Vec<u8>,
    samples: Vec<i32>,
    mask: u32,
    bits: u32,
}

fn fixture(name: &str, mask: u32, bits: u32) -> Fx {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let read = |p: PathBuf| std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    let words = |b: Vec<u8>| -> Vec<i32> {
        b.chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    };
    let (data, samples) = if let Some(stem) = name.strip_suffix(".flac") {
        let d = root.join("qy-codec-flac/tests/fixtures");
        (
            read(d.join(name)),
            words(read(d.join(format!("{stem}.s32le")))),
        )
    } else {
        let stem = name.trim_end_matches(".wav");
        let d = root.join("qy-codec-wav/tests/fixtures");
        let ff = words(read(d.join(format!("{stem}.ffmpeg.s32le"))));
        (
            read(d.join(name)),
            ff.into_iter().map(|s| s >> (32 - bits)).collect(),
        )
    };
    Fx {
        data,
        samples,
        mask,
        bits,
    }
}

/// 掩码按位从低到高展开的位置序号（源样本顺序）。
fn bits_of(mask: u32) -> Vec<u8> {
    (0..18u8).filter(|b| mask & (1 << b) != 0).collect()
}

/// 独立公式：源样本按设备位置顺序 `device` 置换后放入 C 位容器（左对齐、小端）。
fn device_bytes(fx: &Fx, device: &[u8], c: u32) -> Vec<u8> {
    let src = bits_of(fx.mask);
    let ch = src.len();
    let map: Vec<usize> = device
        .iter()
        .map(|p| match src.iter().position(|q| q == p) {
            Some(i) => i,
            None => panic!("设备位置 {p} 不在源中"),
        })
        .collect();
    let mut out = Vec::new();
    for frame in fx.samples.chunks_exact(ch) {
        for &i in &map {
            let w = ((i64::from(frame[i])) << (c - fx.bits)) as u32;
            out.extend_from_slice(&w.to_le_bytes()[..(c / 8) as usize]);
        }
    }
    out
}

/// WASAPI 式设备容器：N ≤ 16 → 16，否则 32（SUPPORTED 中 V = 24 的首项为 32/24，V = 32 为 32/32）。
fn container(bits: u32) -> u32 {
    if bits <= 16 { 16 } else { 32 }
}

fn spec(fx: &Fx) -> TrackSpec {
    TrackSpec {
        data: fx.data.clone(),
        open_fails: false,
        read_error_at: None,
        unseekable: false,
    }
}

fn scenario(
    seed: u64,
    device: DeviceModel,
    tracks: Vec<TrackSpec>,
    on_error: ErrorPolicy,
    device_layouts: Option<Vec<ChannelLayout>>,
) -> Scenario {
    Scenario {
        seed,
        tracks,
        reads: ReadModel {
            zero_per_mille: 30,
            tiny_per_mille: 50,
            slow_per_mille: 30,
            slow_max_ticks: 20,
        },
        device,
        supported: SUPPORTED.to_vec(),
        stalls: Vec::new(),
        disconnect_at: None,
        restart: None,
        max_ticks: 120_000,
        on_error,
        controls: Vec::new(),
        device_layouts,
        float_formats: Vec::new(),
    }
}

fn host(p: &qy_codec_auto::profile::Profile) -> SimHost {
    match SimHost::with_profile(p) {
        Ok(h) => h,
        Err(e) => panic!("{e:?}"),
    }
}

#[track_caller]
fn clean(r: &Report) {
    assert!(
        r.finished && r.fault.is_none() && r.underruns.is_empty() && !r.timed_out,
        "{:?} finished={} underruns={} timed_out={}",
        r.fault,
        r.finished,
        r.underruns.len(),
        r.timed_out
    );
    assert_eq!((r.protocol_errors, r.output_errors), (0, 0));
}

/// 每个设备段：(采样率, 设备位置顺序, C, 期望字节)。
fn check_segments(r: &Report, want: &[(u32, Vec<u8>, u32, Vec<u8>)], what: &str) {
    assert_eq!(r.segments.len(), want.len(), "{what}: 段数");
    for (i, (seg, (rate, order, c, bytes))) in r.segments.iter().zip(want).enumerate() {
        let f = seg.format;
        let got_order: Vec<u8> = f.layout().positions().iter().map(|p| *p as u8).collect();
        assert_eq!(
            (f.sample_rate(), &got_order, u32::from(f.container_bits())),
            (*rate, order, *c),
            "{what}: 段 {i} 格式"
        );
        assert!(seg.valid == *bytes, "{what}: 段 {i} 字节不一致");
    }
}

#[test]
fn mixed_layouts_play_bit_exact() {
    let stereo = fixture("pcm16_stereo_44k.wav", 0x3, 16);
    let c3 = fixture("crafted_3ch_no_tag.flac", 0x7, 16);
    let f51 = fixture("cli_24_6ch_96k_51.flac", 0x3F, 24);
    let w51 = fixture("ext24in32_6ch_51.wav", 0x3F, 24);
    let side = fixture("cli_16_6ch_48k_51_side.flac", 0x60F, 16);
    let f71 = fixture("cli_24_8ch_48k_71.flac", 0x63F, 24);
    let w71 = fixture("ext24_8ch_71.wav", 0x63F, 24);
    let sl_sr = fixture("crafted_2ch_tag_side.flac", 0x600, 16);
    let tracks = [&stereo, &c3, &f51, &w51, &side, &f71, &w71, &sl_sr];
    let seg = |fx: &[&Fx], rate: u32| {
        let c = container(fx[0].bits);
        let order = bits_of(fx[0].mask);
        let bytes = fx.iter().flat_map(|f| device_bytes(f, &order, c)).collect();
        (rate, order, c, bytes)
    };
    // 同布局、同采样率、同设备格式的相邻曲目共用一段（WAV 6 声道 24 位接在 FLAC 之后；
    // 8 声道 FLAC 与 WAV 同为 48 kHz / 32-24）。
    let want = [
        seg(&[&stereo], 44_100),
        seg(&[&c3], 44_100),
        seg(&[&f51, &w51], 96_000),
        seg(&[&side], 48_000),
        seg(&[&f71, &w71], 48_000),
        seg(&[&sl_sr], 44_100),
    ];
    let mut h = host(&DESKTOP);
    for seed in SEEDS {
        for device in DEVICES {
            let sc = scenario(
                seed,
                device,
                tracks.iter().map(|f| spec(f)).collect(),
                ErrorPolicy::Stop,
                None,
            );
            let r = run(&mut h, &sc);
            clean(&r);
            assert!(r.fidelity_intact());
            check_segments(&r, &want, &format!("种子 {seed} {device:?}"));
        }
    }
}

#[test]
fn fixed_device_order_permutes_and_rejects_other_positions() {
    use Speaker::*;
    let hda = [
        FrontLeft,
        FrontRight,
        BackLeft,
        BackRight,
        FrontCenter,
        LowFrequency,
    ];
    let (Some(hda51), Some(st)) = (
        ChannelLayout::from_positions(&hda),
        Some(ChannelLayout::STEREO),
    ) else {
        panic!("layouts");
    };
    let f51 = fixture("cli_24_6ch_96k_51.flac", 0x3F, 24);
    let side = fixture("cli_16_6ch_48k_51_side.flac", 0x60F, 16);
    let stereo = fixture("pcm16_stereo_44k.wav", 0x3, 16);
    // 设备顺序独立写出：FL=0 FR=1 BL=4 BR=5 FC=2 LFE=3。
    let order: Vec<u8> = vec![0, 1, 4, 5, 2, 3];
    let want = [
        (96_000, order.clone(), 32, device_bytes(&f51, &order, 32)),
        (44_100, vec![0, 1], 16, device_bytes(&stereo, &[0, 1], 16)),
    ];
    let mut h = host(&DESKTOP);
    for seed in SEEDS {
        for device in DEVICES {
            let sc = scenario(
                seed,
                device,
                vec![spec(&f51), spec(&side), spec(&stereo)],
                ErrorPolicy::Skip,
                Some(vec![hda51, st]),
            );
            let r = run(&mut h, &sc);
            assert!(r.finished && r.fault.is_none() && r.underruns.is_empty());
            assert_eq!(r.skipped, vec![(1, EngineFault::Core(Fault::Configure))]);
            check_segments(&r, &want, &format!("种子 {seed} {device:?}"));
        }
    }
}

#[test]
fn undefined_layout_and_capability_rejections() {
    let undefined = Fx {
        data: std::fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../qy-codec-wav/tests/fixtures/unsup_6ch_mask0.wav"),
        )
        .unwrap_or_default(),
        samples: Vec::new(),
        mask: 0,
        bits: 16,
    };
    let stereo = fixture("pcm16_stereo_44k.wav", 0x3, 16);
    let f51 = fixture("cli_24_6ch_96k_51.flac", 0x3F, 24);
    let w51 = fixture("ext24in32_6ch_51.wav", 0x3F, 24);
    let want_stereo = [(44_100, vec![0, 1], 16, device_bytes(&stereo, &[0, 1], 16))];
    // 桌面：未定义布局按 Unsupported 跳过，不打开设备。
    let mut desk = host(&DESKTOP);
    // Embedded Full（双声道）：6 声道 WAV 由引擎按能力不足拒绝；6 声道 FLAC 在 STREAMINFO 处
    // 即超出解码器容量（双声道工作区）。R1 起两者报告同一类原因（能力不足，6 声道 / 上限 2），
    // 原先 FLAC 只报 CapacityExceeded（tasks/R1.md §1）。都不打开设备。
    let mut emb = host(&EMBEDDED_FULL);
    for seed in SEEDS {
        for device in DEVICES {
            let r = run(
                &mut desk,
                &scenario(
                    seed,
                    device,
                    vec![spec(&undefined), spec(&stereo)],
                    ErrorPolicy::Skip,
                    None,
                ),
            );
            clean(&r);
            assert_eq!(
                r.skipped,
                vec![(
                    0,
                    EngineFault::Core(Fault::Decode(DecodeError::Unsupported))
                )]
            );
            check_segments(&r, &want_stereo, "未定义布局");

            let r = run(
                &mut emb,
                &scenario(
                    seed,
                    device,
                    vec![spec(&w51), spec(&f51), spec(&stereo)],
                    ErrorPolicy::Skip,
                    None,
                ),
            );
            clean(&r);
            assert_eq!(
                r.skipped,
                vec![
                    (
                        0,
                        EngineFault::Capability(CapabilityError::Channels {
                            channels: 6,
                            max: 2
                        })
                    ),
                    (
                        1,
                        EngineFault::Capability(CapabilityError::Channels {
                            channels: 6,
                            max: 2
                        })
                    ),
                ]
            );
            check_segments(&r, &want_stereo, "能力不足");
        }
    }
}
