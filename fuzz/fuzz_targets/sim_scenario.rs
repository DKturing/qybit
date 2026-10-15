//! M2 T5：场景 fuzz。libFuzzer 的输入直接决定模拟场景：前 8 字节为种子（`种子 % 10` 选场景类型：
//! 正常、超预算停顿、输入故障、输出故障、停止重启），其余字节依次决定曲目组合（格式、时长、样本）、
//! 读取模型、生产端停顿、断开与重启时刻等每个选择（`Rng::from_tape`，用完后按种子继续）。
//!
//! 场景生成与核对与固定种子回归（`sim_seeds`）共用同一份代码，检查的不变式相同：不 panic、不超时、
//! 无协议错误与输出错误；正常场景设备字节与独立公式逐字节一致，故障场景为期望前缀、旧代次回报被拒。
//! 固定种子回归另外要求覆盖预定情形（如重启时有在途读取）；输入决定的场景不一定经过这些情形，
//! 因此这里只核对不变式（`verify_invariants`）。
#![no_main]

#[allow(dead_code)]
#[path = "../../crates/qy-host-sim/tests/seeds_common/mod.rs"]
mod seeds_common;

use std::cell::RefCell;

use libfuzzer_sys::fuzz_target;
use qy_host_sim::SimHost;
use qy_host_sim::sim::{Rng, run};
use seeds_common::{build_case_with, verify_invariants};

thread_local! {
    static HOST: RefCell<Option<SimHost>> = const { RefCell::new(None) };
}

fuzz_target!(|data: &[u8]| {
    let Some((seed, tape)) = data.split_first_chunk::<8>() else {
        return;
    };
    let seed = u64::from_le_bytes(*seed);
    let case = build_case_with(seed, Rng::from_tape(seed, tape));
    HOST.with(|h| {
        let mut h = h.borrow_mut();
        if h.is_none() {
            *h = SimHost::new().ok();
        }
        let Some(host) = h.as_mut() else {
            panic!("模拟宿主分配失败");
        };
        let report = run(host, &case.scenario);
        if let Err(e) = verify_invariants(&case, &report) {
            panic!("种子 {seed}（类型 {}）：{e}", seed % 10);
        }
    });
});
