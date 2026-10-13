//! M2 T3：ALSA 后端在假 PCM 上的协商、播放与矩阵测试。期望设备字节由独立公式计算。

use std::path::PathBuf;

use qy_core::{DecodeError, Fault, SourceFormat};
use qy_pcm_format::alsa::AlsaFormat;
use qy_player::render::{ApiError, ApiOp, ErrorCode, RenderEnd};
use qy_player::testing::{
    FakePriority, data_chunk, fixture, fixture_path, flac_fixture_path, flac_reference, place,
};
use qy_player::{Backend, HostResources, play};

use crate::attempt::PlaybackReport;
use crate::backend::AlsaBackend;
use crate::matrix::{Playback, matrix_items, run_matrix, run_sequential};
use crate::pcm::errno;
use crate::testing::{FakeAlsaConfig, FakeAlsaOutput, FakeOpener, PCall};

type Fake = AlsaBackend<FakeOpener, FakePriority>;

fn backend(cfg: FakeAlsaConfig) -> Fake {
    AlsaBackend::new(
        "hw:CARD=Fake,DEV=0",
        FakeOpener::new(cfg),
        FakePriority::default(),
    )
}

fn src(rate: u32, ch: u16, bits: u8) -> SourceFormat {
    match SourceFormat::new(rate, ch, bits) {
        Ok(s) => s,
        Err(e) => panic!("{e:?}"),
    }
}

fn run(b: &mut Fake, names: &[&str]) -> (PlaybackReport, Vec<FakeAlsaOutput>) {
    let Ok(mut res) = HostResources::new() else {
        panic!("res");
    };
    let files: Vec<PathBuf> = names
        .iter()
        .map(|n| {
            if n.ends_with(".flac") {
                flac_fixture_path(n)
            } else {
                fixture_path(n)
            }
        })
        .collect();
    let r = play(b, &mut res, &files);
    assert!(res.consumer.is_some(), "消费端须归还");
    // 只保留真正播放过的 PCM（协商失败而关闭的不计）。
    let outs = b
        .opener
        .outputs()
        .into_iter()
        .filter(|o| o.calls.contains(&PCall::Start))
        .collect();
    (r, outs)
}

/// 独立公式：N 位样本放在 C32 容器的低 V 位（ALSA `S24_LE`），高字节符号扩展。
fn place_lsb(samples: &[i32], n: u8, v: u8) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|&s| ((i64::from(s) << (v - n)) as u32).to_le_bytes())
        .collect()
}

/// 独立读取 24 位 PCM WAV 的 data chunk 为右对齐样本。
fn wav24_samples(name: &str) -> Vec<i32> {
    data_chunk(&fixture(name))
        .chunks_exact(3)
        .map(|b| i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8)
        .collect()
}

fn check_calls(o: &FakeAlsaOutput) {
    let start = o.calls.iter().position(|c| *c == PCall::Start);
    let first_write = o
        .calls
        .iter()
        .position(|c| matches!(c, PCall::Write(_, n) if *n > 0));
    assert!(
        start.is_some() && first_write < start,
        "Start 前须已写入数据"
    );
    assert!(
        !o.calls
            .iter()
            .any(|c| matches!(c, PCall::Write(_, n) if *n == -i64::from(errno::EBADFD))),
        "不得在错误状态下写入"
    );
}

#[test]
fn negotiation_picks_smallest_valid_bits_in_device_order() {
    // 设备只接受 S24_LE 与 S32_LE：16 位源选 V = 24 的 S24_LE（低位对齐）。
    let mut b = backend(FakeAlsaConfig {
        supported: vec![AlsaFormat::S32Le, AlsaFormat::S24Le],
        ..FakeAlsaConfig::default()
    });
    let p = match b.open(&src(44_100, 2, 16)) {
        Ok(p) => p,
        Err(f) => panic!("{f:?}"),
    };
    use qy_player::PendingStream;
    let f = p.format();
    assert_eq!((f.container_bits(), f.valid_bits()), (32, 24));
    assert_eq!(f.placement(), qy_pcm_format::Placement::LsbAligned);
    let n = p.negotiation();
    assert_eq!(n.attempts.len(), 1);
    assert_eq!(n.attempts[0].alsa_format, AlsaFormat::S24Le);
    assert!(n.attempts[0].accepted());
    // 32 位源只有 S32_LE；设备不接受时打开失败，不降精度。
    let mut b = backend(FakeAlsaConfig {
        supported: vec![AlsaFormat::S16Le, AlsaFormat::S24Le, AlsaFormat::S24_3Le],
        ..FakeAlsaConfig::default()
    });
    match b.open(&src(48_000, 2, 32)) {
        Ok(_) => panic!("不应打开"),
        Err(f) => {
            assert!(f.negotiation.attempts.is_empty());
            assert!(
                f.message.contains("不能以不改变样本的格式打开"),
                "{}",
                f.message
            );
            assert!(
                f.message.contains("S16_LE, S24_3LE, S24_LE"),
                "{}",
                f.message
            );
        }
    }
}

#[test]
fn readback_mismatch_and_setting_errors_try_next_candidate() {
    // S24_3LE 设置采样率失败 → 记录后试 S24_LE。
    let mut b = backend(FakeAlsaConfig {
        reject_rate_for: Some(AlsaFormat::S24_3Le),
        ..FakeAlsaConfig::default()
    });
    let p = match b.open(&src(96_000, 1, 20)) {
        Ok(p) => p,
        Err(f) => panic!("{f:?}"),
    };
    use qy_player::PendingStream;
    let n = p.negotiation();
    assert_eq!(n.attempts.len(), 2);
    assert_eq!(
        n.attempts[0].error.map(|e| (e.call, e.errno)),
        Some(("snd_pcm_hw_params_set_rate", errno::EINVAL))
    );
    assert_eq!(n.chosen, Some(1));
    assert_eq!(n.attempts[1].alsa_format, AlsaFormat::S24Le);
    drop(p);
    let calls = b
        .opener
        .outputs()
        .pop()
        .map(|o| o.calls)
        .unwrap_or_default();
    assert_eq!(
        &calls[..3],
        &[
            PCall::Configure(AlsaFormat::S24_3Le),
            PCall::HwFree,
            PCall::Configure(AlsaFormat::S24Le)
        ],
        "失败后须释放参数再试"
    );

    // 读回格式被改写、近似采样率、有效位不足：每个候选都不一致 → 打开失败并列出原因。
    let cases: [(FakeAlsaConfig, &str); 3] = [
        (
            FakeAlsaConfig {
                rewrite_format: Some(AlsaFormat::S16Le),
                ..FakeAlsaConfig::default()
            },
            "格式被改为 S16_LE",
        ),
        (
            FakeAlsaConfig {
                rate_numden: Some((44_099, 1)),
                ..FakeAlsaConfig::default()
            },
            "采样率为 44099/1 Hz",
        ),
        (
            FakeAlsaConfig {
                significant_bits: Some(20),
                ..FakeAlsaConfig::default()
            },
            "设备有效位 20 少于源有效位 24",
        ),
    ];
    for (cfg, why) in cases {
        let mut b = backend(cfg);
        match b.open(&src(44_100, 2, 24)) {
            Ok(_) => panic!("{why}: 不应打开"),
            Err(f) => {
                let n = &f.negotiation;
                assert!(n.chosen.is_none());
                assert!(!n.attempts.is_empty());
                for a in &n.attempts {
                    assert!(!a.accepted());
                    assert!(
                        a.mismatch.as_deref().is_some_and(|m| m.contains(why)),
                        "{why}: {:?}",
                        a.mismatch
                    );
                }
            }
        }
    }
}

#[test]
fn device_name_and_open_errors() {
    for name in ["plughw:0,0", "default", "pipewire", "dmix", "front:CARD=X"] {
        let mut b = backend(FakeAlsaConfig::default());
        b.device = name.into();
        match b.open(&src(48_000, 2, 16)) {
            Ok(_) => panic!("{name} 不应打开"),
            Err(f) => assert!(f.message.contains("被拒绝"), "{name}: {}", f.message),
        }
        assert!(
            b.opener
                .opened
                .lock()
                .map(|o| o.is_empty())
                .unwrap_or(false),
            "{name}: 被拒绝的设备名不得调用 snd_pcm_open"
        );
    }
    let mut b = AlsaBackend::new(
        "hw:1,0",
        FakeOpener::new(FakeAlsaConfig {
            open_errno: Some(errno::EBUSY),
            occupants: vec!["4242 pipewire".into()],
            ..FakeAlsaConfig::default()
        }),
        FakePriority::default(),
    );
    match b.open(&src(48_000, 2, 16)) {
        Ok(_) => panic!("不应打开"),
        Err(f) => {
            let m = &f.message;
            assert!(
                m.contains("EBUSY") && m.contains("不会改用共享或混音路径"),
                "{m}"
            );
            assert!(m.contains("/dev/snd/pcmC1D0p 的进程：4242 pipewire"), "{m}");
        }
    }
    let mut b = backend(FakeAlsaConfig {
        open_errno: Some(errno::EACCES),
        ..FakeAlsaConfig::default()
    });
    match b.open(&src(48_000, 2, 16)) {
        Ok(_) => panic!("不应打开"),
        Err(f) => assert!(f.message.contains("audio 组"), "{}", f.message),
    }
}

/// FLAC/WAV 混排、随机短写：设备字节与独立公式一致，无填充；S24_LE 低位对齐。
#[test]
fn playback_bytes_exact_with_short_writes() {
    for seed in 1..=3 {
        let mut b = backend(FakeAlsaConfig {
            short_writes: true,
            seed,
            ..FakeAlsaConfig::default()
        });
        let names = [
            "pcm16_stereo_44k.wav",
            "cli_16_stereo_44k_default.flac",
            "fixed_orders_and_lpc32.flac",
            "cli_32_stereo_192k.flac",
            "max_block_verbatim_32_stereo.flac",
        ];
        let (r, outs) = run(&mut b, &names);
        assert!(
            r.finished && r.software_path_intact(),
            "seed {seed}: {}",
            r.summary()
        );
        assert_eq!(outs.len(), 4, "同格式 WAV → FLAC 共用一个流");
        let expect = [
            [
                data_chunk(&fixture(names[0])),
                place(&flac_reference("cli_16_stereo_44k_default"), 16, 16),
            ]
            .concat(),
            // 20 位源：设备接受全部格式，V = 24 中按设备顺序先 S24_3LE（3 字节、左移 4 位）。
            place(&flac_reference("fixed_orders_and_lpc32"), 20, 24),
            place(&flac_reference("cli_32_stereo_192k"), 32, 32),
            place(&flac_reference("max_block_verbatim_32_stereo"), 32, 32),
        ];
        for (o, e) in outs.iter().zip(&expect) {
            assert!(o.rendered == *e, "seed {seed}: {} 字节不一致", o.name);
            check_calls(o);
            assert_eq!(o.calls.last(), Some(&PCall::Drain));
        }
        let short = outs
            .iter()
            .flat_map(|o| &o.calls)
            .filter(|c| matches!(c, PCall::Write(g, n) if *n >= 0 && (*n as u32) < *g))
            .count();
        assert!(short > 0, "短写未被覆盖");
        for s in &r.streams {
            assert!(matches!(
                s.submission,
                qy_player::Submission::ShortWrite { .. }
            ));
            assert_eq!(
                s.render.map(|x| x.tail_fill_frames + x.drain_frames),
                Some(0)
            );
        }
    }

    // 24 位源、设备只接受 S16_LE/S24_LE/S32_LE：选 S24_LE，低 24 位 + 符号扩展。
    let mut b = backend(FakeAlsaConfig {
        supported: vec![AlsaFormat::S16Le, AlsaFormat::S24Le, AlsaFormat::S32Le],
        short_writes: true,
        ..FakeAlsaConfig::default()
    });
    let (r, outs) = run(&mut b, &["pcm24_stereo_large.wav"]);
    assert!(r.software_path_intact(), "{}", r.summary());
    let samples = wav24_samples("pcm24_stereo_large.wav");
    assert!(
        samples.iter().any(|&s| s < 0),
        "夹具须含负样本以检查符号扩展"
    );
    assert!(outs[0].rendered == place_lsb(&samples, 24, 24));
}

#[test]
fn xrun_is_underrun_and_device_loss_is_error() {
    // 同一文件连播 3 遍（同格式一个流）；运行中第 3、9 次写入时设备已取空：记为欠载（缺失帧），恢复后继续，有效字节完整无重复。
    let mut b = backend(FakeAlsaConfig {
        xrun_at_writes: vec![3, 9],
        ..FakeAlsaConfig::default()
    });
    let three = ["pcm24_stereo_large.wav"; 3];
    let (r, outs) = run(&mut b, &three);
    let writes = outs[0]
        .calls
        .iter()
        .filter(|c| matches!(c, PCall::Write(..)))
        .count();
    assert!(r.finished, "{}", r.summary());
    assert!(
        !r.software_path_intact(),
        "写入 {writes} 次：{}",
        r.summary()
    );
    assert_eq!(r.exit_code(), 2);
    let rr = r.streams[0].render.unwrap_or_else(|| panic!("render"));
    assert_eq!(rr.underrun_events, 2, "{:?}", outs[0].calls);
    assert_eq!(rr.underrun_frames, 2 * 37);
    assert!(rr.underrun_records().all(|u| u.missing_frames == 37));
    assert!(outs[0].rendered == data_chunk(&fixture("pcm24_stereo_large.wav")).repeat(3));
    assert_eq!(
        outs[0]
            .calls
            .iter()
            .filter(|c| **c == PCall::Prepare)
            .count(),
        2,
        "每次 xrun 用 prepare 恢复"
    );
    assert!(r.summary().contains("本次播放不是完整保真"));

    // 设备消失（ENODEV）与挂起（ESTRPIPE）：设备错误，停止。
    for e in [errno::ENODEV, errno::ESTRPIPE] {
        let mut b = backend(FakeAlsaConfig {
            fail_write: Some((5, e)),
            ..FakeAlsaConfig::default()
        });
        let (r, _) = run(
            &mut b,
            &["pcm24_stereo_large.wav", "pcm24_stereo_large.wav"],
        );
        assert!(r.device_error && !r.finished, "errno {e}: {}", r.summary());
        assert_eq!(r.exit_code(), 1);
        assert_eq!(
            r.streams[0].render.map(|x| x.end),
            Some(RenderEnd::Error(ApiError {
                op: ApiOp::Write,
                code: ErrorCode::Errno(e)
            }))
        );
    }
}

#[test]
fn decode_fault_drains_and_summary_reports_readback() {
    let mut b = backend(FakeAlsaConfig {
        short_writes: true,
        ..FakeAlsaConfig::default()
    });
    let (r, outs) = run(&mut b, &["err_frame_crc.flac"]);
    assert_eq!(
        r.fault,
        Some((
            0,
            qy_core::engine::EngineFault::Core(Fault::Decode(DecodeError::Malformed))
        ))
    );
    assert!(outs[0].rendered == place(&flac_reference("err_frame_crc"), 16, 16));

    let mut b = backend(FakeAlsaConfig::default());
    let (r, _) = run(&mut b, &["pcm16_stereo_44k.wav"]);
    let s = r.summary();
    assert!(
        // F1：报告写明声道映射的确认情况（tasks/F1.md §3）。
        s.contains("请求 #0：44100 Hz / 2 声道 / 容器 16 位 / 有效 16 位（S16_LE）hw_params 成功，读回一致：精确采样率 44100/1；声道映射：设备不报告，按默认布局 [FL FR] ← 已协商"),
        "{s}"
    );
    assert!(
        s.contains("已应用参数：格式 44100 Hz / 2 声道 / 容器 16 位 / 有效 16 位，缓冲 1764 帧，周期 441 帧（由 snd_pcm_hw_params_current 读回）"),
        "{s}"
    );
    assert!(s.contains("提交方式：短写"), "{s}");
    assert!(s.contains("回录证据：无（未回录）"), "{s}");
    assert!(s.contains("不证明设备端 bit-perfect"), "{s}");
    assert_eq!(r.exit_code(), 0);
}

#[test]
fn fake_matrix_records_capabilities_and_playback() {
    let dir = std::env::temp_dir().join(format!("qybit-alsa-matrix-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let Ok(mut res) = HostResources::new() else {
        panic!("res");
    };
    let cfg = FakeAlsaConfig {
        supported: vec![AlsaFormat::S16Le, AlsaFormat::S32Le],
        max_rate: Some(96_000),
        short_writes: true,
        ..FakeAlsaConfig::default()
    };
    let items = matrix_items();
    assert_eq!(items.len(), 48);
    let entries = match run_matrix(&items, &mut res, &dir, |item| {
        let mut b = backend(cfg.clone());
        b.forced = Some(item.format);
        b
    }) {
        Ok(e) => e,
        Err(e) => panic!("{e}"),
    };
    for e in &entries {
        let ok =
            e.item.rate <= 96_000 && matches!(e.item.format, AlsaFormat::S16Le | AlsaFormat::S32Le);
        assert_eq!(e.accepted, ok, "{e:?}");
        if ok {
            assert!(e.played_ok(), "{e:?}");
            assert!(e.applied.is_some());
        } else {
            assert_eq!(e.playback, Playback::NotRun);
            assert!(e.detail.is_some() && !e.errors.is_empty(), "{e:?}");
        }
    }
    let mut b = backend(cfg);
    let seq = match run_sequential(&entries, &mut res, &dir, &mut b) {
        Ok(Some(s)) => s,
        other => panic!("{other:?}"),
    };
    assert!(seq.ok, "{seq:?}");
    assert_eq!(seq.rates, vec![44_100, 48_000]);
    let report = crate::matrix::MatrixReport {
        device: "hw:CARD=Fake,DEV=0".into(),
        backend: crate::matrix::backend_version(),
        entries,
        sequential: Some(seq),
    };
    let json = report.to_json();
    assert_eq!(json, report.to_json(), "报告须确定");
    assert!(json.contains("\"report\": \"qybit-alsa-device-matrix\""));
    assert!(json.contains("\"format\": \"S24_LE\", \"accepted\": false"));
    assert!(json.contains("不证明 DAC 输入样本逐位一致"));
    let csv = report.to_csv();
    assert_eq!(csv.lines().count(), 49);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- F1：声道映射（chmap）----

/// 独立写出的 ALSA 位置值：FL=3 FR=4 RL=5 RR=6 FC=7 LFE=8。
const HDA_51: [u32; 6] = [3, 4, 5, 6, 7, 8];
const WAVE_51: [u32; 6] = [3, 4, 7, 8, 5, 6];

/// 5.1（WAVE 顺序 FL FR FC LFE BL BR）参考样本按 `order`（源声道下标）放入 S32_LE（s << 8）。
fn s32_51(order: &[usize; 6]) -> Vec<u8> {
    flac_reference("cli_24_6ch_96k_51")
        .chunks_exact(6)
        .flat_map(|f| order.map(|i| ((f[i] << 8) as u32).to_le_bytes()))
        .flatten()
        .collect()
}

fn cfg_51(chmaps: Vec<(qy_pcm_format::alsa::ChmapType, Vec<u32>)>) -> FakeAlsaConfig {
    FakeAlsaConfig {
        supported: vec![AlsaFormat::S32Le],
        chmaps,
        ..FakeAlsaConfig::default()
    }
}

#[test]
fn chmap_var_fixed_none_readback_and_inverse() {
    use qy_pcm_format::alsa::{CHMAP_PHASE_INVERSE, ChmapType};
    let name = ["cli_24_6ch_96k_51.flac"];

    // 可变映射：设置为源布局并读回一致，不置换。
    let mut b = backend(cfg_51(vec![(ChmapType::Var, HDA_51.to_vec())]));
    let (r, outs) = run(&mut b, &name);
    assert!(r.finished && r.software_path_intact(), "{r:?}");
    assert!(outs[0].rendered == s32_51(&[0, 1, 2, 3, 4, 5]));
    assert!(
        r.summary()
            .contains("声道映射：可变，已设置并读回 FL FR FC LFE RL RR")
    );

    // 固定映射（HDA 顺序 FL FR RL RR FC LFE）：按设备顺序纯置换。
    let mut b = backend(cfg_51(vec![(ChmapType::Fixed, HDA_51.to_vec())]));
    let (r, outs) = run(&mut b, &name);
    assert!(r.finished && r.software_path_intact(), "{r:?}");
    assert!(
        outs[0].rendered == s32_51(&[0, 1, 4, 5, 2, 3]),
        "置换后的设备字节"
    );
    let s = r.summary();
    assert!(
        s.contains("声道映射：固定，读回 FL FR RL RR FC LFE，已按设备映射重排声道"),
        "{s}"
    );

    // 设备不报告映射：多于 2 声道无法确认布局，打开失败、不做任何回退。
    let mut b = backend(cfg_51(Vec::new()));
    let (r, outs) = run(&mut b, &name);
    assert_eq!(
        r.fault,
        Some((0, qy_core::engine::EngineFault::Core(Fault::Configure)))
    );
    assert!(outs.is_empty());
    assert!(
        r.summary().contains("设备不报告声道映射"),
        "{}",
        r.summary()
    );

    // 读回与设置不符：该格式失败。
    let mut c = cfg_51(vec![(ChmapType::Var, HDA_51.to_vec())]);
    c.chmap_readback = Some(HDA_51.to_vec());
    let mut b = backend(c);
    let (r, outs) = run(&mut b, &name);
    assert_eq!(
        r.fault,
        Some((0, qy_core::engine::EngineFault::Core(Fault::Configure)))
    );
    assert!(outs.is_empty());
    assert!(
        r.summary().contains("读回的声道映射与请求不符"),
        "{}",
        r.summary()
    );

    // 反相声道的映射不被选用；侧环绕映射与源（后环绕）位置集合不同：都拒绝。
    let mut inv = WAVE_51;
    inv[5] |= CHMAP_PHASE_INVERSE;
    let side = vec![3, 4, 7, 8, 9, 10];
    let mut b = backend(cfg_51(vec![
        (ChmapType::Fixed, inv.to_vec()),
        (ChmapType::Fixed, side),
    ]));
    let (r, _) = run(&mut b, &name);
    assert_eq!(
        r.fault,
        Some((0, qy_core::engine::EngineFault::Core(Fault::Configure)))
    );
    assert!(
        r.summary().contains("没有与源扬声器位置相同的声道映射"),
        "{}",
        r.summary()
    );

    // 双声道 + 可变映射：设置为 FL FR 并读回。
    let mut b = backend(FakeAlsaConfig {
        chmaps: vec![(ChmapType::Var, vec![4, 3])],
        ..FakeAlsaConfig::default()
    });
    let (r, outs) = run(&mut b, &["pcm16_stereo_44k.wav"]);
    assert!(r.finished && r.software_path_intact(), "{r:?}");
    assert!(outs[0].rendered == data_chunk(&fixture("pcm16_stereo_44k.wav")));
    assert!(r.summary().contains("声道映射：可变，已设置并读回 FL FR"));
}
