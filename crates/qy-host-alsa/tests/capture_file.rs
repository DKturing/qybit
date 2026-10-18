//! 外部播放程序的回环核对（仅 Linux，默认忽略）：生成一个测试曲目，由另一个程序（如图形界面
//! `qybit-gui`）向 snd-aloop 的 `hw:Loopback,0,0` 播放，本测试同时从 `hw:Loopback,1,0` 回录，
//! 按曲目开头的非静音段对齐后逐字节比对。用于在虚拟机中核对图形界面的播放路径（手动流程见
//! docs/gui-linux.md §4，尚未接入 CI）。
//!
//! - `make_wav`：把 44.1 kHz 16 位双声道的测试曲目写到 `QYBIT_CAPTURE_WAV`：前导静音、各声道
//!   极值、`QYBIT_CAPTURE_TRACK` 秒（缺省 3）随机段，末尾 0.5 秒静音（snd-aloop 按定时器分段
//!   复制，停止时最后不足一段的数据不转到回录端，静音垫片保证有效数据全部到达）；
//! - `capture_matches_wav`：回录 `QYBIT_CAPTURE_SECONDS` 秒（缺省 10）并逐字节比对。暂停时设备
//!   输出静音：比对中遇到整帧为零、至少 10 ms 的一段时跳过（记为一次间隙），之后必须从中断处
//!   继续；`QYBIT_CAPTURE_GAPS` 给出期望的间隙数（缺省 0）。
//!
//! 这是虚拟回环：经过 ALSA 内核路径，但不证明真实 DAC 输入样本逐位一致。
#![cfg(target_os = "linux")]

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{device_bytes, test_samples, write_wav};
use qy_host_alsa::linux::capture::start_capture;
use qy_host_alsa::loopback::find_sync;
use qy_pcm_format::alsa::AlsaFormat;

const RATE: u32 = 44_100;
const SEED: u64 = 0x5155_4942;

fn track_seconds() -> usize {
    std::env::var("QYBIT_CAPTURE_TRACK")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&s| (1..=600).contains(&s))
        .unwrap_or(3)
}

/// 有效数据（不含末尾静音）。
fn samples() -> Vec<i32> {
    test_samples(16, 2, RATE as usize * track_seconds(), SEED)
}

/// 末尾静音的帧数。
const TAIL: usize = RATE as usize / 2;

/// 逐字节比对 `got[start..]` 与 `want`，跳过整帧为零、至少 10 ms 的间隙（暂停）。返回间隙数。
fn compare_with_gaps(got: &[u8], start: usize, want: &[u8]) -> Result<usize, String> {
    const FRAME: usize = 4;
    let min_gap = RATE as usize / 100;
    let (mut i, mut j, mut gaps) = (start, 0, 0);
    while j < want.len() {
        let Some(&g) = got.get(i) else {
            return Err(format!("回录不足：比对到第 {j} / {} 字节", want.len()));
        };
        if g == want[j] {
            i += 1;
            j += 1;
            continue;
        }
        // 退回帧边界，数这里起整帧为零的帧数。
        let back = j % FRAME;
        i -= back;
        j -= back;
        let zero = got[i..]
            .chunks_exact(FRAME)
            .take_while(|f| f.iter().all(|&b| b == 0))
            .count();
        if zero < min_gap {
            return Err(format!(
                "第 {j} 字节不一致：期望 0x{:02X}，回录 0x{:02X}（零帧 {zero}）",
                want[j + back],
                got[i + back]
            ));
        }
        println!("间隙：数据第 {j} 字节前插入 {zero} 帧静音");
        i += zero * FRAME;
        gaps += 1;
    }
    Ok(gaps)
}

fn wav_path() -> PathBuf {
    match std::env::var("QYBIT_CAPTURE_WAV") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => panic!("未设置 QYBIT_CAPTURE_WAV"),
    }
}

#[test]
#[ignore = "只生成测试曲目，供回环核对使用"]
fn make_wav() {
    let mut v = samples();
    v.extend(std::iter::repeat_n(0, TAIL * 2));
    write_wav(&wav_path(), RATE, 2, 16, &v);
}

#[test]
#[ignore = "需要 snd-aloop 与另一个正在播放该曲目的程序"]
fn capture_matches_wav() {
    let seconds: u64 = std::env::var("QYBIT_CAPTURE_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let want = device_bytes(&samples(), 16, AlsaFormat::S16Le);
    // 对齐用开头第一个非零字节起的 4 KiB（各声道极值与随机段），其后的部分逐字节比对。
    let first = want.iter().position(|&b| b != 0).unwrap_or(0) & !3;
    let sync = &want[first..first + 4096];
    let rest = &want[first + 4096..];
    let (cap, applied) = match start_capture("hw:Loopback,1,0", AlsaFormat::S16Le, RATE, 2, 441) {
        Ok(c) => c,
        Err(e) => panic!("回录打开失败：{e}"),
    };
    println!("回录参数：{applied:?}");
    std::thread::sleep(Duration::from_secs(seconds));
    let got = match cap.finish() {
        Ok(b) => b,
        Err(e) => panic!("回录失败：{e}"),
    };
    let nonzero = got.iter().filter(|&&b| b != 0).count();
    println!(
        "回录 {} 字节（{:.2} 秒），非零字节 {nonzero}",
        got.len(),
        got.len() as f64 / f64::from(RATE * 4)
    );
    let Some(off) = find_sync(&got, sync) else {
        panic!("回录中找不到曲目开头：送到设备的数据与曲目不一致（或没有播放）");
    };
    let want_gaps: usize = std::env::var("QYBIT_CAPTURE_GAPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    match compare_with_gaps(&got, off + sync.len(), rest) {
        Ok(gaps) => {
            println!("对齐位置 {off}，全部 {} 字节一致，间隙 {gaps}", want.len());
            assert_eq!(gaps, want_gaps, "暂停造成的静音间隙数与操作不符");
        }
        Err(e) => panic!("{e}"),
    }
}
