//! R1 §6 端到端用例的缩短版（CI 运行）：只检查不 panic、逐字节一致、计数自洽与水位如实，
//! 不检查性能数值（CI 机器性能不稳定）。完整基准见 `qy-bench e2e` 与 docs/r1-reliability.md。

use std::time::Duration;

use qy_bench::{Codec, ReadDelay, Spec, end_to_end};
use qy_core::engine::{CapabilityError, EngineFault};
use qy_gen_fixtures::bench::Predictor;

const FLAC: Codec = Codec::Flac {
    block: 4096,
    predictor: Predictor::Lpc(32),
};

fn check(spec: Spec, seconds: f64, delay: ReadDelay) {
    let r = match end_to_end(spec, seconds, delay) {
        Ok(r) => r,
        Err(e) => panic!("{}：{e}", spec.label()),
    };
    assert!(r.report.finished, "{}：{:?}", spec.label(), r.report.fault);
    assert!(r.counters_ok, "{}：计数不自洽", spec.label());
    let Some(x) = r.render() else {
        panic!("{}：无渲染报告", spec.label());
    };
    // 逐字节：欠载明细超出保留条数时无法去掉补入的静音，如实标记未比对。
    match r.bytes_equal {
        Some(eq) => assert!(eq, "{}：设备收到的有效数据与参考不符", spec.label()),
        None => eprintln!(
            "{}：欠载 {} 次，超出明细条数，未逐字节比对",
            spec.label(),
            x.underrun_events
        ),
    }
    // 水位如实：取数被观测过；出现欠载时，此前必然观测到临界水位。
    assert!(x.watermark.observations > 0, "{}", spec.label());
    if x.underrun_events > 0 {
        assert!(
            x.watermark.critical > 0,
            "{}：欠载而无临界计数",
            spec.label()
        );
    }
    eprintln!(
        "{}：最低水位 {:?} 帧，低水位 {}，临界 {}，欠载 {}，墙钟 {:?}",
        spec.label(),
        x.watermark.min_frames,
        x.watermark.low,
        x.watermark.critical,
        x.underrun_events,
        r.wall
    );
}

#[test]
fn high_spec_end_to_end_short() {
    check(
        Spec {
            codec: Codec::Wav,
            rate: 768_000,
            channels: 2,
            bits: 32,
        },
        1.5,
        ReadDelay::default(),
    );
    check(
        Spec {
            codec: FLAC,
            rate: 192_000,
            channels: 8,
            bits: 24,
        },
        1.5,
        ReadDelay {
            per_read: Duration::from_millis(2),
            ..ReadDelay::default()
        },
    );
}

/// 停顿超过预填：如实报告（欠载时必有临界计数），其余检查不变。192 kHz / 8 声道 / C32 时桌面队列
/// 正好容纳 500 ms，700 ms 的停顿必然超出。
#[test]
fn stall_beyond_prefill_reported() {
    check(
        Spec {
            codec: Codec::Wav,
            rate: 192_000,
            channels: 8,
            bits: 24,
        },
        2.0,
        ReadDelay {
            stall_every_bytes: 2 << 20,
            stall: Duration::from_millis(700),
            ..ReadDelay::default()
        },
    );
}

/// 384 kHz / 8 声道 / 24 位预填 500 ms 至少需要 192 000 × 8 × 3 = 4 608 000 字节队列（按最小容器
/// 3 字节计），超出桌面配置的 3 072 000 字节：播放前按能力不足拒绝，并给出具体数值。
#[test]
fn queue_limit_rejected_before_device() {
    let spec = Spec {
        codec: FLAC,
        rate: 384_000,
        channels: 8,
        bits: 24,
    };
    let r = match end_to_end(spec, 0.2, ReadDelay::default()) {
        Ok(r) => r,
        Err(e) => panic!("{e}"),
    };
    assert!(!r.report.finished);
    assert!(r.report.streams.is_empty(), "不应打开设备");
    match r.report.fault {
        Some((
            0,
            EngineFault::Capability(CapabilityError::Queue {
                required,
                available,
            }),
        )) => {
            assert_eq!((required, available), (4_608_000, 3_072_000));
        }
        other => panic!("{other:?}"),
    }
}
