//! L1 测量（手动运行，不在默认测试中）：界面模型 + 播放线程在按真实时间消费的假设备上，分别记录
//! 空闲、播放、暂停各 `QY_L1_SECS` 秒（默认 60）的进程 CPU 时间与生产端唤醒次数，以及暂停、恢复、
//! 下一首、停止从发出命令到状态快照反映的时间。数值只对运行的机器成立。
//!
//! ```text
//! QY_L1_SECS=60 cargo test --release -p qy-ui --test l1_measure -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use qy_player::Status;
use qy_player::signal::write_test_wav;
use qy_player::testing::{FakeBackend, FakeDevice};
use qy_ui::{Probe, Ui};

/// 生产端唤醒次数（本次播放累计）。
fn wakeups(s: &Status) -> u64 {
    s.producer_wakeups
}

/// 进程累计 CPU 时间（用户 + 内核，含已结束的线程）。
#[cfg(target_os = "linux")]
fn cpu() -> Duration {
    // /proc/self/stat 第 14、15 项：utime、stime，单位 USER_HZ（Linux 各主流架构为 100）。
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let rest = stat.rsplit_once(')').map_or("", |(_, r)| r);
    let f: Vec<u64> = rest
        .split_whitespace()
        .map(|x| x.parse().unwrap_or(0))
        .collect();
    let ticks = f.get(11).copied().unwrap_or(0) + f.get(12).copied().unwrap_or(0);
    Duration::from_millis(ticks * 10)
}

#[cfg(windows)]
fn cpu() -> Duration {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let (mut c, mut e, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: 四个输出参数都指向本函数内有效的 FILETIME；伪句柄 GetCurrentProcess 无需关闭。
    #[allow(unsafe_code)]
    let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
    if ok.is_err() {
        return Duration::ZERO;
    }
    let t = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    Duration::from_nanos((t(k) + t(u)) * 100)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn cpu() -> Duration {
    Duration::ZERO
}

fn status(ui: &Ui) -> Option<Status> {
    ui.model.playing.as_ref().and_then(|p| p.status)
}

/// 处理事件直到 `f` 成立（每 1 ms 查看），返回所用时间；10 s 内不成立时为 `None`。
fn until(ui: &mut Ui, f: impl Fn(&Ui) -> bool) -> Option<Duration> {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(10) {
        ui.wait(Duration::from_millis(1));
        if f(ui) {
            return Some(t.elapsed());
        }
    }
    None
}

/// 像界面线程一样只在有事件时处理（不轮询），持续 `d`。
fn idle_for(ui: &mut Ui, d: Duration) {
    let end = Instant::now() + d;
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        ui.wait(left.min(Duration::from_millis(500)));
    }
}

/// 一段状态的测量：CPU 毫秒与唤醒次数。
fn window(ui: &mut Ui, d: Duration) -> (u128, u64) {
    let (c0, w0) = (cpu(), status(ui).map_or(0, |s| wakeups(&s)));
    idle_for(ui, d);
    let (c1, w1) = (cpu(), status(ui).map_or(0, |s| wakeups(&s)));
    ((c1 - c0).as_millis(), w1.saturating_sub(w0))
}

fn track(dir: &Path, name: &str, rate: u32, bits: u8, secs: u64) -> PathBuf {
    let p = dir.join(name);
    if let Err(e) = write_test_wav(&p, rate, 2, bits, u64::from(rate) * secs) {
        panic!("{e}");
    }
    p
}

fn ms(d: Option<Duration>) -> String {
    d.map_or("超时".into(), |d| {
        format!("{:.1}", d.as_secs_f64() * 1000.0)
    })
}

#[test]
#[ignore = "手动测量：每个状态默认 60 s"]
fn measure() {
    let secs: u64 = std::env::var("QY_L1_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let d = Duration::from_secs(secs);
    let dir = std::env::temp_dir().join(format!("qybit-l1-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let cd = track(&dir, "cd.wav", 44_100, 16, secs + 15);
    let hi = track(&dir, "hi.wav", 192_000, 24, secs + 15);
    let mut b = FakeBackend::new(&[(16, 16), (24, 24), (32, 24), (32, 32)]);
    b.device = FakeDevice::Clock;
    let mut ui = Ui::new(move |_: Option<&str>| Ok(b.clone()), Arc::new(|| {}));
    let mut rows = Vec::new();

    // 空闲：没有播放会话。
    let c0 = cpu();
    idle_for(&mut ui, d);
    rows.push(("空闲（无会话）", (cpu() - c0).as_millis(), None));

    let mut latency: Vec<(&str, Option<Duration>)> = Vec::new();
    for (label, file) in [("44.1 kHz / 16 位", &cd), ("192 kHz / 24 位", &hi)] {
        ui.clear();
        ui.add_paths(vec![file.clone(), cd.clone()]);
        let probed = until(&mut ui, |u| {
            u.model.entries.iter().all(|e| e.probe != Probe::Pending)
        });
        assert!(probed.is_some());
        ui.play_from(0);
        let started = until(&mut ui, |u| {
            status(u).is_some_and(|s| s.device.is_some() && s.position_frames > 0)
        });
        assert!(started.is_some(), "未开始播放");
        let (cpu_ms, w) = window(&mut ui, d);
        rows.push((label, cpu_ms, Some(w)));
        ui.pause();
        latency.push((
            "暂停",
            until(&mut ui, |u| status(u).is_some_and(|s| s.paused)),
        ));
        let (cpu_ms, w) = window(&mut ui, d);
        rows.push(("  暂停中", cpu_ms, Some(w)));
        for _ in 0..3 {
            ui.resume();
            latency.push((
                "恢复",
                until(&mut ui, |u| status(u).is_some_and(|s| !s.paused)),
            ));
            idle_for(&mut ui, Duration::from_millis(300));
            ui.pause();
            latency.push((
                "暂停",
                until(&mut ui, |u| status(u).is_some_and(|s| s.paused)),
            ));
            idle_for(&mut ui, Duration::from_millis(300));
        }
        ui.resume();
        latency.push((
            "恢复",
            until(&mut ui, |u| status(u).is_some_and(|s| !s.paused)),
        ));
        idle_for(&mut ui, Duration::from_millis(300));
        ui.next();
        latency.push((
            "下一首",
            until(&mut ui, |u| status(u).is_some_and(|s| s.track == 1)),
        ));
        idle_for(&mut ui, Duration::from_millis(500));
        ui.stop();
        latency.push(("停止", until(&mut ui, |u| u.model.playing.is_none())));
        let Some(o) = ui.model.last.clone() else {
            panic!("无结果");
        };
        let wm: Vec<&str> = o.summary.lines().filter(|l| l.contains("水位")).collect();
        println!("{label}：欠载 {} 次；{}", o.underrun_events, wm.join(" / "));
        assert_eq!(o.underrun_events, 0, "{}", o.summary);
    }
    println!("\n| 状态 | {secs} s CPU（ms） | 生产端唤醒（次） | 每秒唤醒 |");
    println!("| --- | --- | --- | --- |");
    for (what, cpu_ms, w) in rows {
        match w {
            Some(w) => println!(
                "| {what} | {cpu_ms} | {w} | {:.1} |",
                w as f64 / secs as f64
            ),
            None => println!("| {what} | {cpu_ms} | 0（线程阻塞在请求通道） | 0 |"),
        }
    }
    println!("\n| 命令 | 次数 | 最长（ms） | 各次（ms） |");
    println!("| --- | --- | --- | --- |");
    for what in ["暂停", "恢复", "下一首", "停止"] {
        let all: Vec<Option<Duration>> = latency
            .iter()
            .filter(|(w, _)| *w == what)
            .map(|(_, d)| *d)
            .collect();
        let max = all.iter().copied().max().flatten();
        let list: Vec<String> = all.iter().map(|d| ms(*d)).collect();
        println!(
            "| {what} | {} | {} | {} |",
            all.len(),
            ms(max),
            list.join(", ")
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
