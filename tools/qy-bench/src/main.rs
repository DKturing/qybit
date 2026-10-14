//! `qy-bench`：桌面吞吐基准（R1 §6）。用 release 构建运行：
//!
//! ```text
//! cargo run --release -p qy-bench -- decode [--seconds 4] [--reps 5]
//! cargo run --release -p qy-bench -- e2e [--seconds 10]
//! ```
//!
//! 输出 Markdown 表格（记录见 docs/r1-reliability.md）。

use std::time::Duration;

use qy_bench::{Codec, DecodeRig, ReadDelay, Spec, bench_decode, decode_specs, end_to_end};
use qy_gen_fixtures::bench::Predictor;

fn arg(args: &[String], name: &str, default: f64) -> f64 {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("decode") => decode(
            arg(&args, "--seconds", 4.0),
            arg(&args, "--reps", 5.0) as usize,
        ),
        Some("e2e") => e2e(arg(&args, "--seconds", 10.0)),
        _ => {
            eprintln!("用法：qy-bench decode [--seconds S] [--reps N] | e2e [--seconds S]");
            2
        }
    };
    std::process::exit(code);
}

fn decode(seconds: f64, reps: usize) -> i32 {
    let mut rig = match DecodeRig::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    println!(
        "解码 + pack 实时倍数：每项 {seconds} 秒音频，计时 {reps} 次（先逐样本比对一次，不计时）"
    );
    println!();
    println!("| 规格 | 文件字节 | 最慢 | 中位数 |");
    println!("| --- | ---: | ---: | ---: |");
    let mut failed = 0;
    for spec in decode_specs() {
        match bench_decode(&mut rig, spec, seconds, reps) {
            Ok(r) => println!(
                "| {} | {} | {:.1}× | {:.1}× |",
                spec.label(),
                r.file_bytes,
                r.worst(),
                r.median()
            ),
            Err(e) => {
                failed += 1;
                println!("| {} | – | 失败：{e} | – |", spec.label());
            }
        }
    }
    i32::from(failed > 0)
}

/// 端到端规格：R1 §6 指定的两项，另加队列上限处的 8 声道（384 kHz / 8 声道 / C32 需要的 500 ms
/// 队列超出桌面配置，按能力不足拒绝）。
pub fn e2e_specs() -> Vec<Spec> {
    let flac = Codec::Flac {
        block: 4096,
        predictor: Predictor::Lpc(32),
    };
    vec![
        Spec {
            codec: Codec::Wav,
            rate: 768_000,
            channels: 2,
            bits: 32,
        },
        Spec {
            codec: flac,
            rate: 384_000,
            channels: 8,
            bits: 24,
        },
        Spec {
            codec: flac,
            rate: 192_000,
            channels: 8,
            bits: 24,
        },
        Spec {
            codec: flac,
            rate: 384_000,
            channels: 2,
            bits: 24,
        },
    ]
}

fn e2e(seconds: f64) -> i32 {
    let delays = [
        ("无", ReadDelay::default()),
        (
            "每次读 5 ms",
            ReadDelay {
                per_read: Duration::from_millis(5),
                ..ReadDelay::default()
            },
        ),
        (
            "每 4 MiB 停顿 200 ms",
            ReadDelay {
                stall_every_bytes: 4 << 20,
                stall: Duration::from_millis(200),
                ..ReadDelay::default()
            },
        ),
        (
            "每 4 MiB 停顿 600 ms",
            ReadDelay {
                stall_every_bytes: 4 << 20,
                stall: Duration::from_millis(600),
                ..ReadDelay::default()
            },
        ),
    ];
    println!(
        "端到端（qy-player → 引擎 → 解码 → pack → 队列 → 整包渲染循环 → 按真实时间消费的假设备，每 10 ms 一包），每项 {seconds} 秒"
    );
    println!();
    println!(
        "| 规格 | 设备格式（队列容量） | 读取延迟 | 结果 | 最低水位 | 低水位 / 临界 | 欠载（次 / 帧） | 逐字节 | 计数 |"
    );
    println!("| --- | --- | --- | --- | ---: | ---: | ---: | --- | --- |");
    let mut bad = 0;
    for spec in e2e_specs() {
        for (name, delay) in delays {
            match end_to_end(spec, seconds, delay) {
                Ok(r) => {
                    let result = if r.report.finished {
                        "完成".to_string()
                    } else if let Some((_, f)) = &r.report.fault {
                        format!("拒绝：{}", qy_player::fault_text(f))
                    } else {
                        "未完成".to_string()
                    };
                    let (wm, lc, ur) = match r.render() {
                        Some(x) => {
                            let w = &x.watermark;
                            let min = match (w.min_frames, w.min_ms(spec.rate)) {
                                (Some(f), Some(ms)) => format!("{f} 帧（{ms} ms）"),
                                _ => "–".into(),
                            };
                            (
                                min,
                                format!("{} / {}", w.low, w.critical),
                                format!("{} / {}", x.underrun_events, x.underrun_frames),
                            )
                        }
                        None => ("–".into(), "–".into(), "–".into()),
                    };
                    let eq = match r.bytes_equal {
                        Some(true) => "一致",
                        Some(false) => {
                            bad += 1;
                            "不一致"
                        }
                        None => "未比对",
                    };
                    let counters = if !r.report.finished {
                        "–"
                    } else if r.counters_ok {
                        "自洽"
                    } else {
                        bad += 1;
                        "不自洽"
                    };
                    let device = match r.report.streams.first() {
                        Some(st) => {
                            let f = st.format;
                            let queue = qy_codec_auto::profile::DESKTOP.caps.queue_bytes;
                            let ms = queue as u64 * 1000
                                / (u64::from(f.block_align()) * u64::from(f.sample_rate()));
                            format!("C{}/V{}（{ms} ms）", f.container_bits(), f.valid_bits())
                        }
                        None => "–".into(),
                    };
                    println!(
                        "| {} | {device} | {name} | {result} | {wm} | {lc} | {ur} | {eq} | {counters} |",
                        spec.label()
                    );
                }
                Err(e) => {
                    bad += 1;
                    println!("| {} | – | {name} | 失败：{e} | | | | | |", spec.label());
                }
            }
        }
    }
    i32::from(bad > 0)
}
