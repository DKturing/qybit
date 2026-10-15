//! R1 §3：基于模型的随机命令与故障测试（虚拟时间仿真，不是实机）。
//!
//! 每个种子生成：播放列表（合成的随机样本曲目、截断曲目、能力不足与格式错误的曲目）、存储类型
//! （FAT 或无文件系统交替）、在随机虚拟时刻经串口行缓冲送入的命令序列（`play n`、`pause`、
//! `resume`、`stop`、`next`、`prev`、`status`、`ls`、`scan`、错误命令、超长行），以及故障
//! （块读取失败、超出预填的停顿、拔卡后重新插卡再 `scan`）。
//!
//! 每个种子检查的不变量：
//! 1. 不 panic，主循环在时限内结束；
//! 2. 每个输出段的有效数据，按曲目打开的顺序分解后，每一块都是对应曲目参考字节（按该段设备格式
//!    放置）的整帧前缀；段内不得剩下无法归属的数据（不丢、不重、不串曲）；
//!    以“播放结束”收尾时，最后一首有效曲目必须完整播出（排空不得丢掉在途缓冲）；
//! 3. 未注入超出预填的停顿时欠载为 0；
//! 4. 暂停期间不取数（有效帧计数不增加）；
//! 5. 计数自洽：跳过数 = `skip` 通知数；读取错误 + 打开错误 + 扫描失败 = 实际命中的注入故障数
//!    （扫描失败后存储未挂载，之后的打开错误不对应新故障，此时要求不少于故障数）；协议错误、
//!    喂数错误、输出错误、丢弃的通知都为 0；
//! 6. 结束后 `status` 为 `stopped`。
//!
//! 种子数默认 64（常规测试与 CI）；设 `QY_MODEL_SEEDS=500` 运行完整规模（记录见 docs/r1-reliability.md）。

mod common;

use std::collections::HashMap;

use common::*;
use qy_core::engine::ErrorPolicy;
use qy_host_embedded::Storage;
use qy_host_embedded::command::LineBuffer;
use qy_host_embedded::player::{Notice, Step, write_parse_error};

/// 一个随机生成的操作。
#[derive(Debug, Clone)]
enum Op {
    /// 经串口送入的原始字节（命令行，含错误命令与超长行）。
    Line(Vec<u8>),
    /// 第 k 次之后的块读取失败一次。
    FailRead(u64),
    /// 之后的读取出现超出预填的停顿（之后允许欠载）。
    BigStall,
    /// 拔卡；`ms` 毫秒后重新插卡。
    Pull(u64),
}

/// 曲目的参考：（采样率, 声道, 位, 样本）；被拒绝的曲目为 `None`。
type Ref = Option<(u32, u16, u32, Vec<i32>)>;

fn playlist(rng: &mut Prng) -> (Vec<Track>, Vec<Ref>) {
    let n = 3 + (rng.next() % 4) as usize;
    let mut tracks = Vec::new();
    let mut refs = Vec::new();
    for i in 0..n {
        let name = format!("M{i:02}.WAV");
        match rng.next() % 10 {
            // 能力不足（6 声道）。
            0 => {
                let mut t = fixture(&name, "ext24in32_6ch_51.wav", 96_000, 6, 24);
                t.samples.clear();
                tracks.push(t);
                refs.push(None);
            }
            // 能力不足（192 kHz FLAC）。
            1 => {
                let mut t = fixture(
                    &format!("M{i:02}.FLA"),
                    "cli_32_stereo_192k.flac",
                    192_000,
                    2,
                    32,
                );
                t.samples.clear();
                tracks.push(t);
                refs.push(None);
            }
            // 截断：播放完整帧后报截断（参考为截断前的完整帧）。
            2 => {
                let frames = 4_000 + (rng.next() % 8_000) as usize;
                let mut t = synth_wav(&name, 48_000, 2, 16, frames, rng.next());
                let cut = t.bytes.len() - 1 - (rng.next() % 3_000) as usize;
                t.bytes.truncate(cut);
                let whole = (cut - 44) / 4;
                let s = t.samples[..whole * 2].to_vec();
                refs.push(Some((48_000, 2, 16, s)));
                tracks.push(t);
            }
            // 合成曲目：44.1/48 kHz、16/24 位、单/双声道。
            _ => {
                let rate = if rng.next().is_multiple_of(2) {
                    44_100
                } else {
                    48_000
                };
                let bits = if rng.next().is_multiple_of(2) { 16 } else { 24 };
                let ch = if rng.next().is_multiple_of(4) { 1 } else { 2 };
                let frames = 6_000 + (rng.next() % 30_000) as usize;
                let t = synth_wav(&name, rate, ch, bits, frames, rng.next());
                refs.push(Some((rate, ch, bits, t.samples.clone())));
                tracks.push(t);
            }
        }
    }
    (tracks, refs)
}

fn ops(rng: &mut Prng, tracks: usize) -> Vec<(u64, Op)> {
    let mut t = 0u64;
    let mut v = vec![(0, Op::Line(b"play\n".to_vec()))];
    let n = 20 + (rng.next() % 15) as usize;
    for _ in 0..n {
        t += 30_000 + rng.next() % 600_000;
        let op = match rng.next() % 20 {
            0..=2 => Op::Line(b"pause\n".to_vec()),
            3..=5 => Op::Line(b"resume\n".to_vec()),
            6..=7 => Op::Line(b"next\n".to_vec()),
            8..=9 => Op::Line(b"prev\n".to_vec()),
            10 => Op::Line(b"stop\n".to_vec()),
            11 => Op::Line(format!("play {}\n", rng.next() % (tracks as u64 + 1)).into_bytes()),
            12 => Op::Line(b"status\n".to_vec()),
            13 => Op::Line(b"ls\n".to_vec()),
            14 => Op::Line(b"scan\n".to_vec()),
            15 => Op::Line(b"bogus 1 2\n".to_vec()),
            16 => Op::Line([b'x'; 40].iter().copied().chain(*b"\n").collect()),
            17 => Op::FailRead(1 + rng.next() % 60),
            18 => {
                if rng.next().is_multiple_of(4) {
                    Op::BigStall
                } else {
                    Op::Line(b"status\n".to_vec())
                }
            }
            _ => {
                if rng.next().is_multiple_of(3) {
                    Op::Pull(100_000 + rng.next() % 900_000)
                } else {
                    Op::Line(b"play\n".to_vec())
                }
            }
        };
        v.push((t, op));
    }
    // 收尾：恢复（若仍暂停），让播放自然结束。
    v.push((t + 50_000, Op::Line(b"resume\n".to_vec())));
    v
}

const TIMING: Timing = Timing {
    wav_read_us: 9_000,
    wav_decode_us: 1_000,
    flac_read_us: 17_000,
    flac_decode_us: 15_000,
};

/// 按曲目打开顺序分解各输出段（见模块说明第 2 条）。
/// 返回各块（曲目、匹配字节数、参考字节数），按输出顺序。
fn check_output(
    segs: &[Segment],
    opened: &[u32],
    refs: &[Ref],
    seed: u64,
) -> Result<Vec<(u32, usize, usize)>, String> {
    let mut pieces = Vec::new();
    let mut cache: HashMap<(u32, u32), Vec<u8>> = HashMap::new();
    let mut k = 0usize;
    for (si, seg) in segs.iter().enumerate() {
        let f = seg.format;
        let fb = usize::from(f.block_align());
        let c = u32::from(f.container_bits());
        let mut rest: &[u8] = &seg.data;
        while !rest.is_empty() {
            let Some(&t) = opened.get(k) else {
                return Err(format!(
                    "种子 {seed}：段 {si} 剩 {} 字节无法归属（已无打开的曲目）",
                    rest.len()
                ));
            };
            let Some(Some((rate, ch, bits, samples))) = refs.get(t as usize) else {
                k += 1;
                continue;
            };
            let compatible = *rate == f.sample_rate()
                && *ch == f.channels()
                && *bits <= u32::from(f.valid_bits());
            if !compatible {
                k += 1;
                continue;
            }
            let r = cache
                .entry((t, c))
                .or_insert_with(|| place(samples, *bits, c));
            let lcp = rest
                .iter()
                .zip(r.iter())
                .take_while(|(a, b)| a == b)
                .count();
            let lcp = lcp / fb * fb;
            rest = &rest[lcp..];
            if lcp > 0 {
                pieces.push((t, lcp, r.len()));
            }
            k += 1;
        }
    }
    Ok(pieces)
}

fn run_seed(seed: u64) -> String {
    let mut rng = Prng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x51ED);
    let (tracks, refs) = playlist(&mut rng);
    let script = ops(&mut rng, tracks.len());
    let flat = seed % 2 == 1;
    let base = Disk {
        block_us: 100,
        stall_every: 64,
        stall_us: 5_000,
        ..Disk::default()
    };
    if flat {
        let Ok((p, w)) = setup_flat(&tracks, base, TIMING, ErrorPolicy::Skip) else {
            panic!("种子 {seed}：时序");
        };
        drive(seed, p, w, &script, &refs)
    } else {
        let d = Disk {
            image: image(&tracks),
            ..base
        };
        let Ok((p, w, _)) = setup(d, caps(), TIMING, ErrorPolicy::Skip) else {
            panic!("种子 {seed}：时序");
        };
        drive(seed, p, w, &script, &refs)
    }
}

fn drive<S: Storage>(
    seed: u64,
    mut p: SimPlayerWith<S>,
    w: Shared,
    script: &[(u64, Op)],
    refs: &[Ref],
) -> String {
    let mut lb = LineBuffer::<32>::new();
    let mut next = 0usize;
    let mut notices = Vec::new();
    let mut opened = Vec::new();
    let mut scan_errors = 0u64;
    let mut big_stall = false;
    let mut reinsert_at: Option<u64> = None;
    let mut paused_frames: Option<u64> = None;
    let limit = 200_000_000u64;
    let mut out = String::new();
    // `QY_MODEL_TRACE=1`：打印命令与通知（排查单个种子时用）。
    let trace = std::env::var_os("QY_MODEL_TRACE").is_some();
    loop {
        let now = w.borrow().now;
        if now > limit {
            panic!(
                "种子 {seed}：仿真超时；状态 {:?}；脚本进度 {next}/{}；最后通知 {:?}",
                p.status(),
                script.len(),
                notices.iter().rev().take(6).collect::<Vec<_>>()
            );
        }
        if reinsert_at.is_some_and(|t| now >= t) {
            w.borrow_mut().disk.removed = false;
            reinsert_at = None;
            for &b in b"stop\nscan\nplay\n" {
                if let Some(Ok(c)) = lb.push(b) {
                    let before = out.len();
                    let _ = p.command(c, &mut out);
                    if out[before..].starts_with("ERR scan storage") {
                        scan_errors += 1;
                    }
                }
            }
        }
        while let Some((at, op)) = script.get(next) {
            if *at > now {
                break;
            }
            next += 1;
            if trace {
                let st = p.status();
                eprintln!(
                    "t={now} op={op:?} active={} paused={} data={}",
                    st.active, st.paused, st.output.data_frames
                );
            }
            match op {
                Op::Line(bytes) => {
                    // 命令可能改变暂停状态（同一时刻先恢复再暂停）：重新开始计暂停区间。
                    paused_frames = None;
                    for &b in bytes {
                        match lb.push(b) {
                            Some(Ok(c)) => {
                                let before = out.len();
                                let _ = p.command(c, &mut out);
                                if out[before..].starts_with("ERR scan storage") {
                                    scan_errors += 1;
                                }
                            }
                            Some(Err(e)) => {
                                let _ = write_parse_error(&mut out, e);
                            }
                            None => {}
                        }
                    }
                }
                Op::FailRead(k) => {
                    let mut wb = w.borrow_mut();
                    wb.disk.fail_at = Some(wb.disk.block_reads + k);
                }
                Op::BigStall => {
                    big_stall = true;
                    let mut wb = w.borrow_mut();
                    wb.disk.stall_every = 50;
                    wb.disk.stall_us = 300_000;
                }
                Op::Pull(ms) => {
                    if reinsert_at.is_none() {
                        w.borrow_mut().disk.removed = true;
                        reinsert_at = Some(now + ms);
                    }
                }
            }
        }
        // 不变量 4：暂停期间有效帧不增加。
        let s = p.status();
        if s.paused {
            match paused_frames {
                Some(f) => assert_eq!(s.output.data_frames, f, "种子 {seed}：暂停期间仍在取数"),
                None => paused_frames = Some(s.output.data_frames),
            }
        } else {
            paused_frames = None;
        }
        let step = p.step();
        while let Some(n) = p.pop_notice() {
            if trace {
                eprintln!(
                    "t={now} notice={n:?} data={}",
                    p.status().output.data_frames
                );
            }
            if let Notice::Opened { track } = n {
                opened.push(track);
            }
            notices.push(n);
        }
        match step {
            Step::Busy => w.borrow_mut().advance(STEP_US),
            Step::Idle => {
                let nd = w.borrow().next_dma();
                let done = next >= script.len() && reinsert_at.is_none();
                match nd {
                    Some(t) => w.borrow_mut().advance_to(t),
                    None if done && !p.status().active => break,
                    None => w.borrow_mut().advance(1_000),
                }
            }
        }
    }
    let s = p.status();
    // 不变量 6。
    let mut st = String::new();
    let _ = qy_host_embedded::player::write_status(&mut st, &s);
    assert!(st.starts_with("STATUS state=stopped"), "种子 {seed}：{st}");
    // 不变量 2。
    let wb = w.borrow();
    let pieces = match check_output(&wb.segments, &opened, refs, seed) {
        Ok(p) => p,
        Err(e) => panic!("{e}\n打开顺序：{opened:?}\n通知：{notices:?}"),
    };
    // 不变量 2'：以“播放结束”收尾时，最后一首有效曲目必须完整播出（排空不得丢掉在途缓冲）。
    if notices.last() == Some(&Notice::Finished)
        && let (Some(&last_opened), Some(&(t, got, full))) = (opened.last(), pieces.last())
        && t == last_opened
    {
        assert_eq!(got, full, "种子 {seed}：最后一首曲目 {t} 未完整播出");
    }
    // 不变量 3。
    if !big_stall {
        assert_eq!(s.output.underruns, 0, "种子 {seed}：未注入超预填停顿却欠载");
    }
    // 不变量 5。
    let skips = notices
        .iter()
        .filter(|n| matches!(n, Notice::Skipped { .. }))
        .count() as u32;
    assert_eq!(s.counters.skipped, skips, "种子 {seed}");
    // 每次注入的故障都恰好以一次错误报告；扫描失败后存储处于未挂载状态，之后按旧列表打开曲目
    // 会如实报告打开错误（不对应新的块读取故障），此时只要求不少于故障数。
    let reported =
        u64::from(s.counters.read_errors) + u64::from(s.counters.open_errors) + scan_errors;
    if scan_errors == 0 {
        assert_eq!(
            reported, wb.disk.failures,
            "种子 {seed}：错误计数与注入不符（{s:?}）"
        );
    } else {
        assert!(
            reported >= wb.disk.failures,
            "种子 {seed}：有注入故障未被报告（{s:?}）"
        );
    }
    assert_eq!(
        s.counters.protocol_errors + s.engine.protocol_errors,
        0,
        "种子 {seed}"
    );
    assert_eq!(s.output.errors, 0, "种子 {seed}");
    assert_eq!(s.counters.output_errors, 0, "种子 {seed}");
    assert_eq!(s.counters.notices_dropped, 0, "种子 {seed}");
    format!(
        "seed={seed} storage={} tracks={} ops={} opened={} segments={} data_frames={} skips={} failures={} underruns={} big_stall={} virtual_us={}",
        if seed % 2 == 1 { "flat" } else { "fat" },
        refs.len(),
        script.len(),
        opened.len(),
        wb.segments.iter().filter(|g| !g.data.is_empty()).count(),
        s.output.data_frames,
        skips,
        wb.disk.failures,
        s.output.underruns,
        big_stall,
        wb.now
    )
}

#[test]
fn model_random_commands_and_faults() {
    let seeds: u64 = std::env::var("QY_MODEL_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64);
    let only: Option<u64> = std::env::var("QY_MODEL_ONLY")
        .ok()
        .and_then(|v| v.parse().ok());
    let (mut runs, mut total_ops, mut min_ops, mut virtual_us) = (0u64, 0u64, u64::MAX, 0u64);
    let (mut failures, mut stalls, mut underruns, mut skips) = (0u64, 0u64, 0u64, 0u64);
    for seed in 1..=seeds {
        if only.is_some_and(|o| o != seed) {
            continue;
        }
        let line = run_seed(seed);
        if std::env::var_os("QY_MODEL_VERBOSE").is_some() {
            eprintln!("{line}");
        }
        let ops = field(&line, "ops");
        runs += 1;
        total_ops += ops;
        min_ops = min_ops.min(ops);
        virtual_us += field(&line, "virtual_us");
        failures += field(&line, "failures");
        underruns += field(&line, "underruns");
        skips += field(&line, "skips");
        stalls += u64::from(line.contains("big_stall=true"));
    }
    if only.is_none() {
        // R1 §3：每个种子至少 20 次操作。
        assert!(min_ops >= 20, "最少的种子只有 {min_ops} 次操作");
    }
    eprintln!(
        "模型随机测试：{runs} 个种子，共 {total_ops} 个操作（每种子至少 {min_ops}），虚拟时长共 {:.1} 秒；\
         注入并命中的故障 {failures} 次，超预填停顿 {stalls} 个种子（欠载共 {underruns} 次），\
         跳过 {skips} 首；全部通过",
        virtual_us as f64 / 1e6
    );
}

/// 从摘要行取 `key=数值` 字段。
fn field(line: &str, key: &str) -> u64 {
    line.split(' ')
        .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("摘要缺少 {key}：{line}"))
}
