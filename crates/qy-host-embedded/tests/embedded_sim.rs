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
//! 7. 板级能力不足（缺采样率、只支持双声道）；
//! 8. 无文件系统的存储（曲目表 + `embedded_storage::ReadStorage` 闪存）：同一夹具播放列表与读错误；
//! 9. O1：串口命令 `mono-to-stereo` 开启单声道复制（只支持双声道的板），播放中关闭只影响之后的曲目。
//! 10. S1：串口命令 `seek`（FAT 与无文件系统存储）：定位后的输出从目标帧起与参考逐字节一致。

mod common;

use common::*;
use qy_core::engine::{CapabilityError, ChannelPolicy, EngineFault, ErrorPolicy};
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
fn send<S: Storage>(p: &mut SimPlayerWith<S>, lb: &mut LineBuffer<32>, bytes: &[u8]) -> String {
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

/// S1：串口命令 `seek <毫秒>`。空闲时报错；L1（48 kHz）播放约 0.5 s 后定位到 1500 ms（第
/// 72 000 帧）：此前的输出是 L1 的前缀，之后的段依次为 L1 从目标帧起的部分、L2、L3，与参考逐字节
/// 一致，不计欠载。`storage` 为 FAT 或无文件系统两种存储。
fn seek_script<S: Storage>(p: &mut SimPlayerWith<S>, w: &Shared, tracks: &[Track], what: &str) {
    let mut lb = LineBuffer::<32>::new();
    assert_eq!(send(p, &mut lb, b"seek 100\n"), "ERR seek idle\n", "{what}");
    assert_eq!(send(p, &mut lb, b"seek\n"), "ERR bad-argument\n", "{what}");
    assert!(p.play(0));
    let (mut stage, mut t_mark, mut log) = (0, 0u64, String::new());
    let mut notices = Vec::new();
    run(p, w, 60_000_000, &mut notices, |p, w, n| {
        let now = w.borrow().now;
        let mut lb = LineBuffer::<32>::new();
        match stage {
            0 if n.iter().any(|n| matches!(n, Notice::Device { .. })) => {
                t_mark = now;
                stage = 1;
            }
            1 if now >= t_mark + 500_000 => {
                log += &send(p, &mut lb, b"seek 1500\n");
                stage = 2;
            }
            _ => {}
        }
    });
    assert_eq!(log, "OK seek track=0 frame=72000\n", "{what}");
    assert_eq!(notices.last(), Some(&Notice::Finished), "{what}");
    let s = p.status();
    assert_eq!(s.output.underruns, 0, "{what}：{s:?}");
    let want = expected(tracks, &[true; 3], &[None; 3]);
    let fb = want[0].4.len() / 144_000;
    let wb = w.borrow();
    let got: Vec<&common::Segment> = wb.segments.iter().filter(|s| !s.data.is_empty()).collect();
    assert_eq!(got.len(), 4, "{what}：段数");
    assert!(
        got[0].data.len() < 72_000 * fb && want[0].4.starts_with(&got[0].data),
        "{what}：定位前应为 L1 的前缀"
    );
    assert!(got[1].data == want[0].4[72_000 * fb..], "{what}：定位后");
    assert!(
        got[2].data == want[1].4 && got[3].data == want[2].4,
        "{what}：L2、L3"
    );
}

#[test]
fn seek_command_restarts_at_target() {
    let tracks = long_playlist();
    let Ok((mut p, w, _)) = setup(disk(image(&tracks)), caps(), TIMING, ErrorPolicy::Skip) else {
        panic!("时序");
    };
    seek_script(&mut p, &w, &tracks, "FAT");
    let Ok((mut p, w)) = setup_flat(&tracks, disk(Vec::new()), TIMING, ErrorPolicy::Skip) else {
        panic!("时序");
    };
    seek_script(&mut p, &w, &tracks, "无文件系统");
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

/// 无文件系统：同一夹具播放列表放在“闪存”里（曲目表 + 按偏移读取），慢读与停顿照常注入。
#[test]
fn flat_storage_fixture_playlist_matches_reference() {
    let tracks = playlist();
    let Ok((mut p, w)) = setup_flat(&tracks, disk(Vec::new()), TIMING, ErrorPolicy::Skip) else {
        panic!("时序应满足");
    };
    let mut names = String::new();
    for i in 0..p.storage().track_count() {
        let _ = p.storage().track_name(i, &mut names);
        names.push(' ');
    }
    assert_eq!(
        names,
        "T01.WAV T02.FLA T03.FLA T04.WAV T05.FLA T06.FLA T07.FLA T08 long name.wav T09.WAV "
    );
    assert!(p.play(0));
    let mut notices = Vec::new();
    run(&mut p, &w, 30_000_000, &mut notices, |_, _, _| {});
    let s = p.status();
    assert_eq!(skips(&notices), expected_skips());
    assert_eq!(notices.last(), Some(&Notice::Finished));
    assert_eq!(s.output.underruns, 0, "{s:?}");
    assert_eq!(s.counters.read_errors + s.counters.open_errors, 0);
    check_segments(&w, &expected(&tracks, &PLAYABLE, &[None; 9]), "无文件系统");
    eprintln!(
        "闪存块读取 {} 次，虚拟时长 {} µs",
        w.borrow().disk.block_reads,
        w.borrow().now
    );
}

/// 无文件系统：读取第 2 首时闪存报错一次：该曲按读取错误跳过，只输出出错前的前缀，其余完整。
#[test]
fn flat_storage_read_error_skips_track() {
    let tracks = long_playlist();
    let Ok((mut p, w)) = setup_flat(&tracks, disk(Vec::new()), TIMING, ErrorPolicy::Skip) else {
        panic!("时序");
    };
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
    let wb = w.borrow();
    let got: Vec<&common::Segment> = wb.segments.iter().filter(|s| !s.data.is_empty()).collect();
    let want = expected(&tracks, &[true; 3], &[None; 3]);
    assert_eq!(got.len(), 3);
    assert!(got[0].data == want[0].4, "L1");
    assert!(!got[1].data.is_empty() && got[1].data.len() < want[1].4.len());
    assert!(want[1].4.starts_with(&got[1].data), "L2 前缀");
    assert!(got[2].data == want[2].4, "L3");
}

/// 改写 FLAC 的 STREAMINFO（`fLaC` 后第一个元数据块）：最小/最大块与最大帧。只改声明，不改音频。
fn patch_streaminfo(bytes: &[u8], block: Option<u16>, max_frame: Option<u32>) -> Vec<u8> {
    let mut b = bytes.to_vec();
    let si = 8; // "fLaC" + 4 字节块头
    if let Some(v) = block {
        b[si..si + 2].copy_from_slice(&v.to_be_bytes());
        b[si + 2..si + 4].copy_from_slice(&v.to_be_bytes());
    }
    if let Some(v) = max_frame {
        b[si + 7..si + 10].copy_from_slice(&v.to_be_bytes()[1..]);
    }
    b
}

/// R1 资源计划：文件头阶段的容量拒绝按能力不足报告具体数值；WAV 与 FLAC 的多声道拒绝同一类原因；
/// STREAMINFO 不实（声称小块、实际帧更大）仍在帧级按解码错误拒绝；可播放的曲目报告播放计划。
#[test]
fn resource_plan_reasons() {
    let big = fixture("R1.FLA", "cli_16_blocksize_16384.flac", 96_000, 2, 16);
    let mut frame = fixture("R2.FLA", "cli_16_stereo_44k_default.flac", 44_100, 2, 16);
    frame.bytes = patch_streaminfo(&frame.bytes, None, Some(30_000));
    let mut lie = big.clone();
    lie.name = "R5.FLA".into();
    lie.bytes = patch_streaminfo(&big.bytes, Some(4_096), Some(0));
    let tracks = vec![
        big,
        frame,
        fixture("R3.FLA", "cli_16_6ch_48k_51_side.flac", 48_000, 6, 16),
        fixture("R4.WAV", "ext16_6ch_51_side.wav", 48_000, 6, 16),
        lie,
        fixture("R6.WAV", "pcm16_stereo_44k.wav", 44_100, 2, 16),
    ];
    let Ok((mut p, w, _)) = setup(disk(image(&tracks)), caps(), TIMING, ErrorPolicy::Skip) else {
        panic!("时序");
    };
    assert!(p.play(0));
    let mut notices = Vec::new();
    run(&mut p, &w, 30_000_000, &mut notices, |_, _, _| {});
    let got = skips(&notices);
    let ch6 = EngineFault::Capability(CapabilityError::Channels {
        channels: 6,
        max: 2,
    });
    assert_eq!(
        got[..4],
        [
            Notice::Skipped {
                track: 0,
                fault: EngineFault::Capability(CapabilityError::FlacBlock {
                    block: 16_384,
                    max: 4_608
                })
            },
            Notice::Skipped {
                track: 1,
                fault: EngineFault::Capability(CapabilityError::FlacFrame {
                    bytes: 30_000,
                    window: 28_244
                })
            },
            Notice::Skipped {
                track: 2,
                fault: ch6
            },
            Notice::Skipped {
                track: 3,
                fault: ch6
            },
        ]
    );
    // STREAMINFO 不实：帧级拒绝，仍是解码错误（不归入能力不足，因为文件头并未声明超限）。
    match got.get(4) {
        Some(Notice::Skipped {
            track: 4,
            fault: EngineFault::Core(Fault::Decode(_)),
        }) => {}
        other => panic!("STREAMINFO 不实应按解码错误拒绝：{other:?}"),
    }
    assert_eq!(got.len(), 5);
    assert_eq!(notices.last(), Some(&Notice::Finished));
    // 原因文字带具体数值。
    assert_eq!(
        CapabilityError::FlacBlock {
            block: 16_384,
            max: 4_608
        }
        .to_string(),
        "FLAC 最大块 16384 帧超出本机配置的 4608 帧"
    );
    assert_eq!(
        CapabilityError::FlacFrame {
            bytes: 30_000,
            window: 28_244
        }
        .to_string(),
        "FLAC 声明的最大帧 30000 字节超出本机输入窗口 28244 字节"
    );
    // 播放计划：WAV 16 位双声道 44.1 kHz，16/16 设备，预填 250 ms = 11025 帧 × 4 字节。
    let plans: Vec<String> = notices
        .iter()
        .filter(|n| matches!(n, Notice::Plan { .. }))
        .map(|n| {
            let mut s = String::new();
            let _ = write_notice(&mut s, n);
            s
        })
        .collect();
    assert_eq!(
        plans,
        vec![
            "EVENT plan rate=44100 ch=2 bits=16 device=16-16 prefill_frames=11025 prefill_ms=250 \
             queue=44100/192000\n"
                .to_string()
        ]
    );
    check_segments(
        &w,
        &expected(
            &tracks,
            &[false, false, false, false, false, true],
            &[None; 6],
        ),
        "资源计划",
    );
}

/// 播放计划含 FLAC 的声明需求与本机容量（文件头声明，只用于报告）。
#[test]
fn plan_reports_flac_demand() {
    let tracks = vec![fixture(
        "P1.FLA",
        "cli_16_stereo_44k_default.flac",
        44_100,
        2,
        16,
    )];
    let Ok((mut p, w, _)) = setup(disk(image(&tracks)), caps(), TIMING, ErrorPolicy::Skip) else {
        panic!("时序");
    };
    assert!(p.play(0));
    let mut notices = Vec::new();
    run(&mut p, &w, 30_000_000, &mut notices, |_, _, _| {});
    let Some(Notice::Plan { plan }) = notices.iter().find(|n| matches!(n, Notice::Plan { .. }))
    else {
        panic!("缺少播放计划");
    };
    let Some(d) = plan.decoder else {
        panic!("FLAC 应报告需求");
    };
    // 独立读取夹具的 STREAMINFO：最大块与最大帧。
    let b = &tracks[0].bytes;
    let max_block = u32::from(u16::from_be_bytes([b[10], b[11]]));
    let max_frame = u32::from_be_bytes([0, b[15], b[16], b[17]]);
    assert_eq!((d.max_block, d.block_capacity), (max_block, 4_608));
    assert_eq!((d.max_frame, d.window), (max_frame, 28_244));
    let text = plan.to_string();
    assert!(
        text.contains(&format!("FLAC 最大块 {max_block}/4608 帧")),
        "{text}"
    );
    check_segments(&w, &expected(&tracks, &[true], &[None]), "FLAC 计划");
}

/// R1 水位：时序在声明以内时最低水位高于临界线（临界 0 次）；停顿超过预填时，先出现临界再出现欠载。
/// 数值是确定的（虚拟时间、固定磁盘时序），打印出来写入记录。
#[test]
fn watermark_low_critical_and_underrun_order() {
    let tracks = long_playlist();
    // 声明以内（与 slow_reads 相同的慢卡）。
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
        panic!("时序");
    };
    assert!(p.play(0));
    let mut notices = Vec::new();
    let mut lowest = u64::MAX;
    run(&mut p, &w, 60_000_000, &mut notices, |p, _, _| {
        let s = p.status();
        if let Some(m) = s.output.watermark.min_frames {
            lowest = lowest.min(m);
        }
    });
    let s = p.status();
    let wm = s.output.watermark;
    assert_eq!(s.output.underruns, 0);
    assert_eq!(wm.critical, 0, "{wm:?}");
    assert!(
        wm.min_frames.is_some_and(|m| m >= wm.critical_frames()),
        "{wm:?}"
    );
    eprintln!("声明以内：最后一段水位 {wm:?}，全程最低 {lowest} 帧");

    // 超出预填：每 300 次块读取停顿 400 ms。第一次欠载时临界计数必须已大于 0。
    let d = Disk {
        block_us: 100,
        stall_every: 300,
        stall_us: 400_000,
        ..disk(image(&tracks))
    };
    let Ok((mut p, w, _)) = setup(d, caps(), TIMING, ErrorPolicy::Skip) else {
        panic!("时序");
    };
    assert!(p.play(0));
    let mut notices = Vec::new();
    let mut at_first_underrun = None;
    run(&mut p, &w, 120_000_000, &mut notices, |p, _, _| {
        let s = p.status();
        if at_first_underrun.is_none() && s.output.underruns > 0 {
            at_first_underrun = Some(s.output.watermark);
        }
    });
    let Some(first) = at_first_underrun else {
        panic!("应发生欠载");
    };
    assert!(first.critical > 0, "第一次欠载前应已出现临界：{first:?}");
    let mut line = String::new();
    let _ = qy_host_embedded::player::write_status(&mut line, &p.status());
    assert!(
        line.contains(" wm_low=") && line.contains(" wm_critical="),
        "{line}"
    );
    eprintln!("超出预填：第一次欠载时 {first:?}；{line}");
}

/// R1 模型随机测试发现的回归（种子 11）：播放结束后“上一首”会重新开始播放，此时状态必须是
/// playing，`scan` 必须被拒绝（之前宿主自己维护的标志仍为“已停止”，`scan` 在播放中重建存储，
/// 导致读取失败）。
#[test]
fn prev_after_finish_resumes_and_blocks_scan() {
    let tracks = long_playlist();
    let Ok((mut p, w, _)) = setup(disk(image(&tracks)), caps(), TIMING, ErrorPolicy::Skip) else {
        panic!("时序");
    };
    assert!(p.play(2));
    let mut notices = Vec::new();
    run(&mut p, &w, 60_000_000, &mut notices, |_, _, _| {});
    assert_eq!(notices.last(), Some(&Notice::Finished));
    assert!(!p.status().active);
    let mut out = String::new();
    let _ = p.command(qy_host_embedded::Command::Prev, &mut out);
    assert!(p.status().active, "上一首应重新开始播放");
    out.clear();
    let _ = p.command(qy_host_embedded::Command::Scan, &mut out);
    assert_eq!(out, "ERR scan busy\n");
    let mut notices = Vec::new();
    run(&mut p, &w, 120_000_000, &mut notices, |_, _, _| {});
    let s = p.status();
    assert_eq!(
        (s.counters.read_errors, s.counters.open_errors),
        (0, 0),
        "{s:?}"
    );
    assert_eq!(notices.last(), Some(&Notice::Finished));
}

/// W1：不规范的 WAV 头（ffmpeg 管道占位长度、不足一帧的尾部）与 8 位 PCM 在 MCU 宿主上照常播放，
/// 设备字节等于 ffmpeg 第二参考；每条修正有 `EVENT repaired` 通知，`status` 给出修正计数；
/// R5 截断的曲目播放完整帧后跳过（解码错误计数）。
#[test]
fn w1_repairs_counted_and_bit_exact() {
    let tracks = vec![
        fixture("A.WAV", "fix_ffmpeg_pipe.wav", 44_100, 1, 16),
        fixture("B.WAV", "fix_data_partial_frame.wav", 44_100, 2, 16),
        fixture(
            "C.WAV",
            "trunc_data_short_frame_boundary.wav",
            44_100,
            2,
            16,
        ),
        fixture("D.WAV", "pcm8_stereo_44k.wav", 44_100, 2, 8),
    ];
    let Ok((mut p, w, n)) = setup(disk(image(&tracks)), caps(), TIMING, ErrorPolicy::Skip) else {
        panic!("时序应满足");
    };
    assert_eq!(n, 4);
    assert!(p.play(0));
    let mut notices = Vec::new();
    run(&mut p, &w, 30_000_000, &mut notices, |_, _, _| {});
    let s = p.status();
    assert_eq!(notices.last(), Some(&Notice::Finished));
    assert_eq!(s.output.underruns, 0);
    // 曲目 0：RIFF 与 data 占位（2 条）；曲目 1：丢弃 2 字节（1 条）。
    let repaired: Vec<u32> = notices
        .iter()
        .filter_map(|n| match n {
            Notice::Repaired { track, .. } => Some(*track),
            _ => None,
        })
        .collect();
    assert_eq!(repaired, vec![0, 0, 1]);
    assert_eq!(s.counters.repairs, 3);
    assert_eq!((s.counters.decode_errors, s.counters.skipped), (1, 1));
    let mut line = String::new();
    let _ = qy_host_embedded::player::write_status(&mut line, &s);
    assert!(line.contains(" repairs=3"), "{line}");
    let mut ev = String::new();
    for n in &notices {
        let _ = qy_host_embedded::player::write_notice(&mut ev, n);
    }
    assert!(
        ev.contains("EVENT repaired track=1 丢弃数据末尾不足一帧的 2 字节"),
        "{ev}"
    );
    // 截断曲目 C 只有前 30 帧完整（声明 100 帧）。
    let limit = [None, None, Some(30), None];
    check_segments(
        &w,
        &expected(&tracks, &[true; 4], &limit),
        "W1 修正播放列表",
    );
}

/// 独立公式：每个样本左对齐放入 C 位容器后写两份（单声道复制到左右两路）。
fn place_twice(s: &[i32], n: u32, c: u32) -> Vec<u8> {
    s.iter()
        .flat_map(|&x| place(&[x], n, c).repeat(2))
        .collect()
}

/// O1：板只支持双声道。默认（严格）时单声道按“声道不支持”拒绝；串口 `mono-to-stereo on` 后单声道
/// WAV 与 AIFF 都按 [FL FR] 播放，每个样本原样写两份，与独立参考逐字节一致；立体声曲目不受影响并
/// 续用同一设备。通知、计数与状态行写明“已改变”。播放中 `mono-to-stereo off` 不影响正在播放的曲目，
/// 之后的单声道曲目照严格规则拒绝。
#[test]
fn mono_to_stereo_command_on_stereo_only_board() {
    static STEREO: [u16; 1] = [2];
    let stereo_only = OutputCaps {
        channels: &STEREO,
        ..caps()
    };
    let tracks = vec![
        synth_wav("A.WAV", 48_000, 1, 16, 4_800, 11),
        synth_wav("B.WAV", 48_000, 2, 16, 2_400, 12),
        fixture("C.AIF", "aiff_20_mono_96k.aiff", 96_000, 1, 20),
    ];
    let a = place_twice(&tracks[0].samples, 16, 16);
    let b = place(&tracks[1].samples, 16, 16);
    let c = place_twice(&tracks[2].samples, 20, 32);

    // 默认严格：两首单声道被拒绝，只有立体声曲目播放。
    let Ok((mut p, w, n)) = setup(disk(image(&tracks)), stereo_only, TIMING, ErrorPolicy::Skip)
    else {
        panic!("时序");
    };
    assert_eq!(n, 3);
    let mut names = String::new();
    let _ = p.storage().track_name(2, &mut names);
    assert_eq!(names, "C.AIF");
    assert_eq!(p.engine().channel_policy(), ChannelPolicy::Strict);
    assert!(p.play(0));
    let mut notices = Vec::new();
    run(&mut p, &w, 30_000_000, &mut notices, |_, _, _| {});
    assert_eq!(p.status().counters.unsupported, 2);
    assert_eq!(p.status().counters.duplicated, 0);
    assert!(
        !notices
            .iter()
            .any(|n| matches!(n, Notice::ChannelsDuplicated { .. }))
    );
    check_segments(&w, &[(48_000, 2, 16, 16, b.clone())], "严格");

    // 串口开启：三首都播放。
    let Ok((mut p, w, _)) = setup(disk(image(&tracks)), stereo_only, TIMING, ErrorPolicy::Skip)
    else {
        panic!("时序");
    };
    let mut lb = LineBuffer::<32>::new();
    let out = send(&mut p, &mut lb, b"mono-to-stereo on\r\n");
    assert!(
        out.starts_with("OK mono-to-stereo=on altered mono[FC]->stereo[FL FR]"),
        "{out}"
    );
    assert!(p.play(0));
    let mut notices = Vec::new();
    run(&mut p, &w, 30_000_000, &mut notices, |_, _, _| {});
    let s = p.status();
    assert_eq!(notices.last(), Some(&Notice::Finished));
    assert_eq!(s.output.underruns, 0);
    assert_eq!((s.counters.unsupported, s.counters.skipped), (0, 0));
    assert_eq!(s.counters.duplicated, 2);
    let dup: Vec<u32> = notices
        .iter()
        .filter_map(|n| match n {
            Notice::ChannelsDuplicated { track } => Some(*track),
            _ => None,
        })
        .collect();
    assert_eq!(dup, vec![0, 2]);
    let mut line = String::new();
    let _ = qy_host_embedded::player::write_status(&mut line, &s);
    assert!(line.contains(" duplicated=2 mono_to_stereo=on"), "{line}");
    let mut ev = String::new();
    let _ = write_notice(&mut ev, &Notice::ChannelsDuplicated { track: 0 });
    assert_eq!(
        ev,
        "EVENT channels-altered track=0 mono[FC]->stereo[FL FR] samples-unchanged\n"
    );
    check_segments(
        &w,
        &[
            (48_000, 2, 16, 16, [a.clone(), b.clone()].concat()),
            (96_000, 2, 32, 24, c),
        ],
        "单声道复制",
    );

    // 播放中关闭：正在播放的 A 仍按复制播放完；之后的 C 按严格规则拒绝。
    let Ok((mut p, w, _)) = setup(disk(image(&tracks)), stereo_only, TIMING, ErrorPolicy::Skip)
    else {
        panic!("时序");
    };
    p.set_channel_policy(ChannelPolicy::DuplicateMono);
    assert!(p.play(0));
    let mut notices = Vec::new();
    let mut sent = false;
    run(&mut p, &w, 30_000_000, &mut notices, |p, _, n| {
        // A 已按复制配置设备（通知已发出）、尚未播完时关闭。
        if !sent && n.contains(&Notice::ChannelsDuplicated { track: 0 }) {
            sent = true;
            let mut lb = LineBuffer::<32>::new();
            let out = send(p, &mut lb, b"mono-to-stereo off\n");
            assert_eq!(out, "OK mono-to-stereo=off strict applies-to=next-tracks\n");
        }
    });
    assert!(sent);
    let s = p.status();

    assert!(!s.mono_to_stereo);
    assert_eq!((s.counters.duplicated, s.counters.unsupported), (1, 1));
    assert_eq!(s.output.errors, 0);
    check_segments(&w, &[(48_000, 2, 16, 16, [a, b].concat())], "播放中关闭");
}

/// F3：浮点 WAV 在嵌入式宿主上。I²S 槽位只承载整数：默认（严格）按“不接受浮点”拒绝；串口
/// `float-to-int on` 后按 32 → 24 → 16 位依次协商（Embedded Full 只到 24 位，32 位一档按能力跳过），
/// 转为 24 位放入 32/24 槽位，与按定义独立计算的字节一致；通知、计数与状态行写明“样本已改变”。
#[test]
fn float_wav_strict_rejected_or_converted() {
    let name = "f32_stereo_44k_tag3";
    let dir = fixtures("qy-codec-wav").join("float");
    let bytes = std::fs::read(dir.join(format!("{name}.wav"))).unwrap_or_default();
    let text = std::fs::read_to_string(dir.join(format!("{name}.expect"))).unwrap_or_default();
    let bits: Vec<u32> = text
        .lines()
        .skip(5)
        .flat_map(|l| {
            l.split(' ')
                .filter(|x| !x.is_empty())
                .map(|h| u32::from_str_radix(h, 16).unwrap_or(0))
                .collect::<Vec<_>>()
        })
        .collect();
    // 独立定义：round_half_even(x × 2^23)，限幅到 24 位，左对齐放入 32 位容器。
    let want: Vec<u8> = bits
        .iter()
        .flat_map(|&b| {
            let y = (f64::from(f32::from_bits(b)) * 8_388_608.0).round_ties_even();
            let s = y.clamp(-8_388_608.0, 8_388_607.0) as i64;
            (((s << 8) as u32).to_le_bytes()).to_vec()
        })
        .collect();
    let track = Track {
        name: "F.WAV".into(),
        bytes,
        rate: 44_100,
        channels: 2,
        bits: 24,
        samples: Vec::new(),
    };
    let tracks = vec![track];

    // 严格：拒绝，原因是浮点。
    let Ok((mut p, w, _)) = setup(disk(image(&tracks)), caps(), TIMING, ErrorPolicy::Skip) else {
        panic!("时序");
    };
    assert!(p.play(0));
    let mut notices = Vec::new();
    run(&mut p, &w, 30_000_000, &mut notices, |_, _, _| {});
    assert!(
        notices.contains(&Notice::Unsupported {
            error: SelectError::Float
        }),
        "{notices:?}"
    );
    assert_eq!(p.status().counters.converted, 0);
    check_segments(&w, &[], "严格");

    // 串口开启：转为 24 位。
    let Ok((mut p, w, _)) = setup(disk(image(&tracks)), caps(), TIMING, ErrorPolicy::Skip) else {
        panic!("时序");
    };
    let mut lb = LineBuffer::<32>::new();
    let out = send(&mut p, &mut lb, b"float-to-int on\r\n");
    assert!(
        out.starts_with("OK float-to-int=on altered samples-changed"),
        "{out}"
    );
    assert!(p.play(0));
    let mut notices = Vec::new();
    run(&mut p, &w, 30_000_000, &mut notices, |_, _, _| {});
    let s = p.status();
    assert_eq!(notices.last(), Some(&Notice::Finished), "{notices:?}");
    assert_eq!((s.output.underruns, s.counters.converted), (0, 1));
    assert!(notices.contains(&Notice::FloatConverted { track: 0, bits: 24 }));
    let stats = notices.iter().find_map(|n| match n {
        Notice::FloatStats { track: 0, report } => Some(*report),
        _ => None,
    });
    let Some(stats) = stats else {
        panic!("{notices:?}")
    };
    assert_eq!(
        (
            stats.converted_to,
            stats.convert.samples,
            stats.convert.clipped
        ),
        (Some(24), bits.len() as u64, 0)
    );
    let mut line = String::new();
    let _ = qy_host_embedded::player::write_status(&mut line, &s);
    assert!(line.contains(" converted=1 float_to_int=on"), "{line}");
    let mut ev = String::new();
    let _ = write_notice(&mut ev, &Notice::FloatConverted { track: 0, bits: 24 });
    assert_eq!(ev, "EVENT samples-altered track=0 float->int24 explicit\n");
    check_segments(&w, &[(44_100, 2, 32, 24, want)], "浮点转 24 位");
}
