//! M0 T4：1000 个固定种子的真实路径模拟。
//!
//! 每个种子生成一组曲目（测试内独立的 WAV 写入器，已知样本，含前导静音与非零尾帧）
//! 和一个场景，经真实核心、WAV 解码、pack 与 SPSC 队列播放，再与独立公式
//! `s << (C − N)` 计算的期望设备字节比较。
//!
//! 场景（种子 % 10）：
//! - 0–5 正常：短读/零长度读/慢读（≤ 2 tick，3%）、生产端停顿 ≤ 300 ms 且间隔 ≥ 1000 ms，
//!   预填 500 ms；WASAPI 整包或短写设备。要求逐字节匹配、无欠载、尾部填充 < 1 包。
//! - 6 超预算停顿：192 kHz C32 双声道（队列恰为 500 ms），设备启动后停顿 1000 ms；
//!   要求报告欠载（位置、缺失帧），有效音频仍逐字节完整、无重复，播放恢复并完成，
//!   且不得标记为完整保真。
//! - 7 输入故障：打开失败 / 读取错误 / 截断文件 / 非法 fmt（按种子轮换）。
//! - 8 输出故障：设备不支持格式 / 播放中设备断开（按种子轮换）。
//! - 9 停止重启：全部读取为慢读，播放中停止并从某曲目重新开始，在途旧读取必须被拒绝。
//!
//! 每个场景都有超时上限；失败时打印种子。单独复现：`QY_SIM_SEED=<种子> cargo test -p qy-host-sim --test sim_seeds`。

mod seeds_common;

use qy_core::Fault;
use qy_core::engine::EngineFault;
use qy_host_sim::SimHost;
use qy_host_sim::sim::run;
use seeds_common::{build_case, verify};

const SEEDS: u64 = 1000;

struct SeedGuard(u64);
impl Drop for SeedGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("panic 于种子 {}（QY_SIM_SEED={} 复现）", self.0, self.0);
        }
    }
}

#[test]
fn thousand_seeds() {
    let Ok(mut host) = SimHost::new() else {
        panic!("host");
    };
    let seeds: Vec<u64> = match std::env::var("QY_SIM_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        Some(s) => vec![s],
        None => (0..SEEDS).collect(),
    };
    let mut failures = Vec::new();
    let mut stats = [0u32; 10];
    let mut underrun_events = 0usize;
    let mut stale = 0u32;
    let mut restarts = 0u32;
    let mut lost = 0u32;
    let mut faults = std::collections::BTreeMap::<String, u32>::new();
    let mut partial_nonempty = 0u32;
    for &seed in &seeds {
        let _guard = SeedGuard(seed);
        let case = build_case(seed);
        let report = run(&mut host, &case.scenario);
        if std::env::var_os("QY_DIGEST").is_some() {
            println!("DIGEST {seed} {}", report.digest());
        }
        stats[(seed % 10) as usize] += 1;
        underrun_events += report.underruns.len();
        stale += report.stale_rejected;
        restarts += u32::from(report.restarted_at.is_some());
        lost += u32::from(report.device_lost);
        if let Some((_, ef)) = report.fault {
            let EngineFault::Core(f) = ef else {
                panic!("种子 {seed} 出现非核心故障 {ef:?}");
            };
            *faults.entry(format!("{f:?}")).or_default() += 1;
            if matches!(f, Fault::Read) && report.segments.iter().any(|s| !s.valid.is_empty()) {
                partial_nonempty += 1;
            }
        }
        if let Err(e) = verify(&case, &report) {
            failures.push(format!("种子 {seed}（类型 {}）: {e}", seed % 10));
        }
    }
    eprintln!(
        "模拟种子 {}：各类型 {:?}，欠载事件 {underrun_events}，拒绝旧回报 {stale}，\
         实际重启 {restarts}，设备断开 {lost}，故障 {faults:?}，读错误前已有输出 {partial_nonempty}",
        seeds.len(),
        stats
    );
    assert!(
        failures.is_empty(),
        "{} 个种子失败：\n{}",
        failures.len(),
        failures
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
