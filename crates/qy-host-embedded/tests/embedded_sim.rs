//! M3 T2：通用嵌入式宿主的宿主侧仿真场景（虚拟时间；不是实机）。
//!
//! 运行真实的 `qy-host-embedded` + 引擎 + 自有解码器 + pack + 静态队列 + `embedded-sdmmc`，
//! 只把 SD 卡与 I²S DMA 换成假设备（见 `common`）。场景：
//! 1. 夹具播放列表：各设备段与独立参考逐字节一致；能力不足、解码错误按策略跳过并计数；
//! 2. 慢读与周期性停顿在声明时序之内：无欠载；
//! 3. 实际读卡比声明慢（停顿超过预填）：欠载被计数并经状态报告，有效数据不丢不重；
//! 4. 时序不足时 `Player::new` 明确拒绝；
//! 5. 读错误、拔卡与重新插卡后 `scan`；
//! 6. 串口命令：ls、play、pause/resume、next、prev、status、stop、错误命令；
//! 7. 板级能力不足（缺采样率、只支持双声道）。

mod common;

use common::*;
use qy_core::engine::{CapabilityError, EngineFault, ErrorPolicy};
use qy_core::{DecodeError, Fault};
use qy_host_embedded::command::LineBuffer;
use qy_host_embedded::output::{OutputCaps, SelectError};
use qy_host_embedded::player::{Notice, write_notice, write_parse_error};
use qy_host_embedded::{BudgetError, Storage};

/// 与默认磁盘时序（每块 100 µs，每 64 次块读取停顿 5 ms；簇 4 KiB）配套的声明。读取任意连续 W 字节
/// 至多跨 ⌈W / 512⌉ + 1 个数据块，簇边界每 8 块一次 FAT 查找（再 + 1）：
/// - FLAC W = 28 244：57 + 9 = 66 次块读取、至多 2 次停顿：6 600 + 10 000 = 16 600 µs，取 17 ms；
/// - WAV W = 16 384：33 + 6 = 39 次块读取、至多 1 次停顿：3 900 + 5 000 = 8 900 µs，取 9 ms。
///
/// 解码按 FLAC 15 ms、WAV 1 ms 声明（仿真每步 20 µs，远小于声明值）。
const TIMING: Timing = Timing {
    wav_read_us: 9_000,
    wav_decode_us: 1_000,
    flac_read_us: 17_000,
    flac_decode_us: 15_000,
};

fn disk(image: Vec<u8>) -> Disk {
    Disk {
        image,
        block_us: 100,
        stall_every: 64,
        stall_us: 5_000,
        ..Disk::default()
    }
}

/// 夹具播放列表（Embedded Full：≤ 96 kHz、≤ 24 位、双声道）。
fn playlist() -> Vec<Track> {
    vec![
        fixture("T01.WAV", "pcm16_stereo_44k.wav", 44_100, 2, 16),
        fixture("T02.FLA", "cli_16_stereo_44k_default.flac", 44_100, 2, 16),
        // 第二帧 CRC 错误：先播放已校验的 512 帧，再按解码错误跳过。
        fixture("T03.FLA", "err_frame_crc.flac", 44_100, 2, 16),
        // 6 声道：本机配置最多 2 声道。
        fixture("T04.WAV", "ext24in32_6ch_51.wav", 96_000, 6, 24),
        // 192 kHz：超出本机配置的 96 kHz。
        fixture("T05.FLA", "cli_32_stereo_192k.flac", 192_000, 2, 32),
        fixture("T06.FLA", "cli_24_mono_96k_lpc12.flac", 96_000, 1, 24),
        fixture("T07.FLA", "fixed_orders_and_lpc32.flac", 48_000, 1, 20),
        // 长文件名：短名为 T08LON~1.WAV，按短名排序仍在 T07 与 T09 之间。
        fixture(
            "T08 long name.wav",
            "pcm24_mono_48k_odd_list.wav",
            48_000,
            1,
            24,
        ),
        fixture("T09.WAV", "pcm16_mono_8k_fmt18.wav", 8_000, 1, 16),
    ]
}

const PLAYABLE: [bool; 9] = [true, true, true, false, false, true, true, true, true];

fn expected_skips() -> Vec<Notice> {
    vec![
        Notice::Skipped {
            track: 2,
            fault: EngineFault::Core(Fault::Decode(DecodeError::Malformed)),
        },
        Notice::Skipped {
            track: 3,
            fault: EngineFault::Capability(CapabilityError::Channels {
                channels: 6,
                max: 2,
            }),
        },
        Notice::Skipped {
            track: 4,
            fault: EngineFault::Capability(CapabilityError::Rate {
                rate: 192_000,
                max: 96_000,
            }),
        },
    ]
}

fn skips(n: &[Notice]) -> Vec<Notice> {
    n.iter()
        .filter(|n| matches!(n, Notice::Skipped { .. } | Notice::Failed { .. }))
        .copied()
        .collect()
}

#[test]
fn fixture_playlist_matches_reference() {
    let tracks = playlist();
    let Ok((mut p, w, n)) = setup(disk(image(&tracks)), caps(), TIMING, ErrorPolicy::Skip) else {
        panic!("时序应满足");
    };
    assert_eq!(n, 9);
    // 播放列表按短文件名排序，与写入顺序一致。
    let mut names = String::new();
    for i in 0..n {
        let _ = p.storage().track_name(i, &mut names);
        names.push(' ');
    }
    assert_eq!(
        names,
        "T01.WAV T02.FLA T03.FLA T04.WAV T05.FLA T06.FLA T07.FLA T08LON~1.WAV T09.WAV "
    );
    let m = p.margins();
    eprintln!(
        "时序核对：负载 {}‰，需要预填 {} µs，配置档 {} µs",
        m.load_permille, m.required_prefill_us, m.prefill_us
    );
    assert!(p.play(0));
    let mut notices = Vec::new();
    run(&mut p, &w, 30_000_000, &mut notices, |_, _, _| {});
    let s = p.status();
    eprintln!("{s:?}");
    assert_eq!(skips(&notices), expected_skips());
    assert_eq!(notices.last(), Some(&Notice::Finished));
    assert_eq!(s.output.underruns, 0, "不应欠载");
    assert_eq!(s.output.errors, 0);
    assert_eq!(s.counters.decode_errors, 1);
    assert_eq!(s.counters.unsupported, 2);
    assert_eq!(s.counters.skipped, 3);
    assert_eq!((s.counters.read_errors, s.counters.open_errors), (0, 0));
    assert_eq!(s.counters.protocol_errors + s.engine.protocol_errors, 0);
    let limit = [None, None, None, None, None, None, None, None, None];
    check_segments(&w, &expected(&tracks, &PLAYABLE, &limit), "夹具播放列表");
    assert!(!s.active);
}

/// 合成的较长曲目：48 kHz/16 位/双声道 3 s、48 kHz/24 位/双声道 1.5 s（同采样率续用 32/24）、
/// 44.1 kHz/16 位/双声道 2 s。
fn long_playlist() -> Vec<Track> {
    vec![
        synth_wav("L1.WAV", 48_000, 2, 16, 144_000, 1),
        synth_wav("L2.WAV", 48_000, 2, 24, 72_000, 2),
        synth_wav("L3.WAV", 44_100, 2, 16, 88_200, 3),
    ]
}

#[test]
fn slow_reads_within_declared_timing_do_not_underrun() {
    let tracks = long_playlist();
    // 慢卡：每块 150 µs，每 40 次块读取停顿 8 ms。FLAC：66 × 150 + 2 × 8000 = 25 900 µs，
    // 声明 26 ms；WAV：39 × 150 + 1 × 8000 = 13 850 µs，声明 14 ms。
    let d = Disk {
        block_us: 150,
        stall_every: 40,
        stall_us: 8_000,
        ..disk(image(&tracks))
    };
    let timing = Timing {
        wav_read_us: 14_000,
        flac_read_us: 26_000,
        ..TIMING
    };
    let Ok((mut p, w, _)) = setup(d, caps(), timing, ErrorPolicy::Skip) else {
        panic!("时序应满足");
    };
    eprintln!("{:?}", p.margins());
    assert!(p.play(0));
    let mut notices = Vec::new();
    run(&mut p, &w, 60_000_000, &mut notices, |_, _, _| {});
    let s = p.status();
    assert_eq!(s.output.underruns, 0, "{s:?}");
    assert_eq!(notices.last(), Some(&Notice::Finished));
    // L1 16 位 → 16/16；L2 24 位不能由 16/16 承载 → 排空后开 32/24；L3 换采样率。
    check_segments(&w, &expected(&tracks, &[true; 3], &[None; 3]), "慢读");
    eprintln!(
        "慢读：块读取 {} 次，虚拟时长 {} µs，中断 {} 次",
        w.borrow().disk.block_reads,
        w.borrow().now,
        w.borrow().isr_calls
    );
}

#[test]
fn reads_slower_than_declared_underrun_and_are_reported() {
    let tracks = long_playlist();
    // 声明值照旧，实际每 300 次块读取停顿 400 ms，超过 250 ms 预填：必然欠载。
    let d = Disk {
        block_us: 100,
        stall_every: 300,
        stall_us: 400_000,
        ..disk(image(&tracks))
    };
    let Ok((mut p, w, _)) = setup(d, caps(), TIMING, ErrorPolicy::Skip) else {
        panic!("时序应满足（声明值）");
    };
    assert!(p.play(0));
    let mut notices = Vec::new();
    run(&mut p, &w, 120_000_000, &mut notices, |_, _, _| {});
    let s = p.status();
    assert!(s.output.underruns > 0, "应计数欠载：{s:?}");
    assert!(s.output.underrun_frames > 0);
    // 状态行报告欠载。
    let mut line = String::new();
    let _ = qy_host_embedded::player::write_status(&mut line, &s);
    assert!(
        line.contains(&format!("underruns={}", s.output.underruns)),
        "{line}"
    );
    // 欠载只插入静音，有效数据不丢、不重复。
    check_segments(&w, &expected(&tracks, &[true; 3], &[None; 3]), "欠载");
    eprintln!(
        "欠载 {} 次，缺 {} 帧",
        s.output.underruns, s.output.underrun_frames
    );
}

#[test]
fn insufficient_timing_is_rejected_at_startup() {
    let tracks = long_playlist();
    // 负载：FLAC 读最大帧 41 ms，仅读取就超过 100%（696000 × 41000 / 28244 / 1000 ≈ 1010‰）。
    let heavy = Timing {
        flac_read_us: 41_000,
        ..TIMING
    };
    match setup(disk(image(&tracks)), caps(), heavy, ErrorPolicy::Skip) {
        Err(e @ BudgetError::Load { load_permille }) if load_permille > 1000 => {
            eprintln!("拒绝：{e}")
        }
        Ok(_) => panic!("应拒绝"),
        Err(e) => panic!("{e:?}"),
    }
    // 负载与预填都在范围内：通过。
    let ok = Timing {
        flac_read_us: 16_000,
        flac_decode_us: 5_000,
        ..TIMING
    };
    assert!(setup(disk(image(&tracks)), caps(), ok, ErrorPolicy::Skip).is_ok());
    // 只有预填不足的组合（负载 ≤ 100% 而 U + D + T > P）在 Embedded Full 的需求下不会出现
    // （U ≤ W / R 才能满足负载，远小于 250 ms），组成核对见 budget 单元测试；这里确认报告文字。
    let e = qy_host_embedded::BudgetError::Prefill {
        required_us: 255_334,
        prefill_us: 250_000,
        read_us: 230_000,
        decode_us: 20_000,
        period_us: 5_334,
    };
    assert_eq!(
        e.to_string(),
        "预填 250000 µs 不足：最坏情况需要 255334 µs（读取最大输入单元 230000 + 解码 20000 + DMA 周期 5334）"
    );
}

#[test]
fn read_error_card_removal_and_rescan() {
    let tracks = long_playlist();
    let Ok((mut p, w, _)) = setup(disk(image(&tracks)), caps(), TIMING, ErrorPolicy::Skip) else {
        panic!("时序");
    };
    // 1) 第 2 首（L2）打开后第 40 次块读取失败一次：L2 被跳过，其余完整。
    assert!(p.play(0));
    let mut notices = Vec::new();
    let mut armed = false;
    run(&mut p, &w, 60_000_000, &mut notices, |_, w, n| {
        if !armed && n.contains(&Notice::Opened { track: 1 }) {
            let mut w = w.borrow_mut();
            w.disk.fail_at = Some(w.disk.block_reads + 40);
            armed = true;
        }
    });
    let s = p.status();
    assert_eq!(s.counters.read_errors, 1, "{s:?}");
    assert_eq!(
        skips(&notices),
        vec![Notice::Skipped {
            track: 1,
            fault: EngineFault::Core(Fault::Read)
        }]
    );
    assert_eq!(s.output.underruns, 0);
    // L2 只输出了读错误之前已解码的前缀：逐字节核对 L1、L2 前缀、L3。
    {
        let wb = w.borrow();
        let got: Vec<&common::Segment> =
            wb.segments.iter().filter(|s| !s.data.is_empty()).collect();
        let want = expected(&tracks, &[true; 3], &[None; 3]);
        assert_eq!(got.len(), 3);
        assert!(got[0].data == want[0].4, "L1");
        assert!(!got[1].data.is_empty() && got[1].data.len() < want[1].4.len());
        assert!(want[1].4.starts_with(&got[1].data), "L2 前缀");
        assert!(got[2].data == want[2].4, "L3");
    }

    // 2) 播放中拔卡：当前曲目读失败，其余曲目打开失败，全部跳过；最后一首出错时停止并报告失败。
    w.borrow_mut().segments.clear();
    let mut notices = Vec::new();
    assert!(p.play(0));
    let mut pulled = false;
    run(&mut p, &w, 60_000_000, &mut notices, |_, w, _| {
        let mut w = w.borrow_mut();
        if !pulled && w.now > 1_000_000 {
            w.disk.removed = true;
            pulled = true;
        }
    });
    let s = p.status();
    assert!(pulled);
    assert!(!s.active);
    assert!(
        s.counters.read_errors + s.counters.open_errors >= 1,
        "{s:?}"
    );
    assert!(
        matches!(notices.last(), Some(Notice::Failed { .. })),
        "{notices:?}"
    );
    let mut out = String::new();
    let _ = p.command(qy_host_embedded::Command::Play(0), &mut out);
    // 卡已拔出：play 照常接受（曲目表仍在），随后各曲打开失败。
    let mut notices = Vec::new();
    run(&mut p, &w, 120_000_000, &mut notices, |_, _, _| {});
    assert!(matches!(notices.last(), Some(Notice::Failed { .. })));

    // 3) 重新插卡、scan、再播放：完整且逐字节一致。
    w.borrow_mut().disk.removed = false;
    w.borrow_mut().segments.clear();
    let mut out = String::new();
    let _ = p.command(qy_host_embedded::Command::Scan, &mut out);
    assert_eq!(out, "OK scan tracks=3\n");
    assert!(p.play(0));
    let mut notices = Vec::new();
    run(&mut p, &w, 200_000_000, &mut notices, |_, _, _| {});
    assert_eq!(notices.last(), Some(&Notice::Finished));
    check_segments(&w, &expected(&tracks, &[true; 3], &[None; 3]), "重新插卡");
    eprintln!("{:?}", p.status());
}

/// 把一串字节送入行缓冲，执行命令，收集回应。
fn send(p: &mut SimPlayer, lb: &mut LineBuffer<32>, bytes: &[u8]) -> String {
    let mut out = String::new();
    for &b in bytes {
        match lb.push(b) {
            Some(Ok(c)) => {
                let _ = p.command(c, &mut out);
            }
            Some(Err(e)) => {
                let _ = write_parse_error(&mut out, e);
            }
            None => {}
        }
    }
    out
}

#[test]
fn serial_commands() {
    let tracks = long_playlist();
    let Ok((mut p, w, _)) = setup(disk(image(&tracks)), caps(), TIMING, ErrorPolicy::Skip) else {
        panic!("时序");
    };
    let mut lb = LineBuffer::<32>::new();
    assert_eq!(
        send(&mut p, &mut lb, b"ls\r\n"),
        "OK ls tracks=3\nTRACK 0 L1.WAV\nTRACK 1 L2.WAV\nTRACK 2 L3.WAV\n"
    );
    assert_eq!(send(&mut p, &mut lb, b"bogus\n"), "ERR unknown-command\n");
    assert_eq!(send(&mut p, &mut lb, b"play x\n"), "ERR bad-argument\n");
    assert_eq!(
        send(&mut p, &mut lb, b"play 7\n"),
        "ERR play no-track=7 tracks=3\n"
    );
    assert!(send(&mut p, &mut lb, b"status\n").starts_with("STATUS state=stopped "));
    assert_eq!(send(&mut p, &mut lb, b"play\n"), "OK play from=0\n");

    // 脚本：设备启动后 0.5 s 暂停，暂停 0.3 s 后恢复；L1 播到约 2 s 时 next（跳到 L2）；
    // L2 开始后 0.2 s prev（回到 L2 开头重播：prev 相对正在播放的曲目，第 2 首的上一首是第 1 首）。
    let mut log = String::new();
    let mut stage = 0;
    let mut t_mark = 0u64;
    let mut notices = Vec::new();
    run(&mut p, &w, 60_000_000, &mut notices, |p, w, n| {
        let now = w.borrow().now;
        let mut lb = LineBuffer::<32>::new();
        match stage {
            0 if n.iter().any(|n| matches!(n, Notice::Device { .. })) => {
                t_mark = now;
                stage = 1;
            }
            1 if now >= t_mark + 500_000 => {
                log += &send(p, &mut lb, b"pause\n");
                t_mark = now;
                stage = 2;
            }
            2 if now >= t_mark + 300_000 => {
                let st = send(p, &mut lb, b"status\n");
                assert!(st.starts_with("STATUS state=paused"), "{st}");
                log += &send(p, &mut lb, b"resume\n");
                t_mark = now;
                stage = 3;
            }
            3 if now >= t_mark + 1_200_000 => {
                log += &send(p, &mut lb, b"next\n");
                stage = 4;
            }
            4 if n
                .iter()
                .filter(|n| **n == Notice::Opened { track: 1 })
                .count()
                == 1 =>
            {
                t_mark = now;
                stage = 5;
            }
            5 if now >= t_mark + 600_000 => {
                // 此时 L2 已在播放（预填 250 ms 后启动）：prev 回到 L1。
                log += &send(p, &mut lb, b"prev\n");
                stage = 6;
            }
            6 if n
                .iter()
                .filter(|n| **n == Notice::Opened { track: 0 })
                .count()
                == 2 =>
            {
                t_mark = now;
                stage = 7;
            }
            7 if now >= t_mark + 400_000 => {
                log += &send(p, &mut lb, b"stop\n");
                stage = 8;
            }
            _ => {}
        }
    });
    assert_eq!(stage, 8, "脚本未走完：{notices:?}");
    assert_eq!(log, "OK pause\nOK resume\nOK next\nOK prev\nOK stop\n");
    let s = p.status();
    assert!(!s.active);
    assert_eq!(s.output.underruns, 0, "暂停与切歌不算欠载：{s:?}");
    assert!(s.output.paused_fills > 0);
    let st = send(&mut p, &mut lb, b"status\n");
    assert!(st.starts_with("STATUS state=stopped "), "{st}");
    // 输出：L1 前缀（含暂停，暂停只插静音）| L2 前缀 | L1 前缀（重播）。每段都是对应参考的前缀。
    let want = expected(&tracks, &[true; 3], &[None; 3]);
    let wb = w.borrow();
    let got: Vec<&common::Segment> = wb.segments.iter().filter(|s| !s.data.is_empty()).collect();
    assert_eq!(got.len(), 3, "{}", got.len());
    for (g, e) in got.iter().zip([&want[0].4, &want[1].4, &want[0].4]) {
        assert!(!g.data.is_empty() && g.data.len() < e.len());
        assert!(e.starts_with(&g.data), "应为参考的前缀");
    }
    let mut ev = String::new();
    for n in &notices {
        let _ = write_notice(&mut ev, n);
    }
    eprintln!("{ev}");
}

#[test]
fn board_capability_limits() {
    let tracks = long_playlist();
    // 时钟不能精确产生 44.1 kHz；只支持双声道。
    static RATES_48: [u32; 3] = [32_000, 48_000, 96_000];
    static STEREO: [u16; 1] = [2];
    let limited = OutputCaps {
        rates: &RATES_48,
        channels: &STEREO,
        ..caps()
    };
    let mut list = tracks.clone();
    list.push(synth_wav("L4.WAV", 48_000, 1, 16, 4_800, 4));
    let Ok((mut p, w, _)) = setup(disk(image(&list)), limited, TIMING, ErrorPolicy::Skip) else {
        panic!("时序");
    };
    assert!(p.play(0));
    let mut notices = Vec::new();
    run(&mut p, &w, 60_000_000, &mut notices, |_, _, _| {});
    let unsupported: Vec<_> = notices
        .iter()
        .filter_map(|n| match n {
            Notice::Unsupported { error } => Some(*error),
            _ => None,
        })
        .collect();
    assert_eq!(
        unsupported,
        vec![SelectError::Rate(44_100), SelectError::Channels(1)]
    );
    assert_eq!(
        skips(&notices),
        vec![
            Notice::Skipped {
                track: 2,
                fault: EngineFault::Core(Fault::Configure)
            },
            Notice::Failed {
                track: 3,
                fault: EngineFault::Core(Fault::Configure)
            },
        ]
    );
    check_segments(
        &w,
        &expected(&list, &[true, true, false, false], &[None; 4]),
        "板级能力",
    );
    assert_eq!(p.status().counters.unsupported, 2);
}
