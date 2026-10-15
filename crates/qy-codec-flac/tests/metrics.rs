//! M1 T2：整帧重试的复杂度（需显式启用 `--features test-metrics`）。
//!
//! 逐字节喂入一个 F 字节的完整帧：解析尝试次数应为 O(log F)，累计输入扫描量为 O(F)。
//! 明确界限：尝试次数 ≤ ⌈log2 F⌉ + 2；累计扫描字节 ≤ 4F（几何级数）+ F（最终成功）；
//! 样本运算量按样本数单独记录，不计入输入扫描。

use qy_codec_flac::{Capacity, FlacDecoder};
use qy_core::{Decoder, Status};

fn leak<T: Copy + Default>(n: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); n].into_boxed_slice())
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap_or_default()
}

/// 跳过元数据：返回第一帧起始偏移与第一帧长度（依据 STREAMINFO 只含一帧的夹具：帧到文件尾）。
fn frames_start(data: &[u8]) -> usize {
    let mut i = 4;
    loop {
        let last = data[i] & 0x80 != 0;
        let len = u32::from_be_bytes([0, data[i + 1], data[i + 2], data[i + 3]]) as usize;
        i += 4 + len;
        if last {
            return i;
        }
    }
}

struct Run {
    attempts: u64,
    scanned_bytes: u64,
    sample_ops: u64,
    calls: u64,
}

/// 逐字节喂入，统计只覆盖帧部分。
fn bytewise(name: &str) -> (usize, Run) {
    let data = fixture(name);
    let start = frames_start(&data);
    let c = Capacity::DESKTOP;
    let Ok(mut d) = FlacDecoder::new(c, leak(c.workspace_len()), leak(c.pcm_len())) else {
        panic!("decoder");
    };
    // 元数据一次性喂入。
    let mut window = data[..start].to_vec();
    let Ok(p) = d.decode(&window, false) else {
        panic!("meta")
    };
    assert_eq!((p.consumed, p.status), (start, Status::NeedInput));
    window.clear();
    d.reset_metrics();
    let mut calls = 0u64;
    let mut fed = start;
    loop {
        let eof = fed == data.len();
        calls += 1;
        match d.decode(&window, eof) {
            Ok(p) => {
                window.drain(..p.consumed);
                match p.status {
                    Status::NeedInput => {
                        window.push(data[fed]);
                        fed += 1;
                    }
                    Status::Frames(_) | Status::NeedOutput => {
                        let ch = d.format().map_or(1, |f| usize::from(f.channels()));
                        let n = d.pcm().len() / ch;
                        d.release(n);
                    }
                    Status::End => break,
                }
            }
            Err(e) => panic!("{name}: {e:?} {:?}", d.last_error()),
        }
    }
    let m = d.metrics();
    (
        data.len() - start,
        Run {
            attempts: m.parse_attempts,
            scanned_bytes: m.scanned_bits.div_ceil(8),
            sample_ops: m.sample_ops,
            calls,
        },
    )
}

#[test]
fn max_block_frame_bytewise_is_log_attempts_linear_scan() {
    let (f, r) = bytewise("max_block_verbatim_32_stereo.flac");
    let f = f as u64;
    assert!(f > 500_000, "最大块夹具约 512 KiB，实际 {f}");
    let log2 = 64 - (f - 1).leading_zeros() as u64; // ⌈log2 F⌉
    println!(
        "F={f} 字节：解析尝试 {}（界 {}），累计扫描 {} 字节（界 {}），样本运算 {}，decode 调用 {}",
        r.attempts,
        log2 + 2,
        r.scanned_bytes,
        5 * f,
        r.sample_ops,
        r.calls
    );
    assert!(
        r.attempts <= log2 + 2,
        "解析尝试 {} > {}",
        r.attempts,
        log2 + 2
    );
    assert!(r.scanned_bytes <= 5 * f, "扫描 {} > 5F", r.scanned_bytes);
    // 成功一次的样本运算为 2 声道 × 65535 + 解相关 65535；失败尝试只在完整子帧后计数。
    assert!(
        r.sample_ops >= 3 * 65_535 && r.sample_ops <= 3 * 3 * 65_535,
        "样本运算 {}",
        r.sample_ops
    );
}

#[test]
fn compressed_frames_count_samples_separately_from_input_scan() {
    // 高压缩帧：输入扫描量小，但样本运算按样本数计，不与扫描混算。
    let (f, r) = bytewise("cli_16_constant_channel.flac");
    let f = f as u64;
    assert!(
        r.scanned_bytes <= 5 * f,
        "扫描 {} > 5F={}",
        r.scanned_bytes,
        5 * f
    );
    assert!(r.sample_ops >= 8_000 * 2, "样本运算 {}", r.sample_ops);
}

/// 计数在解码中累计，`reset_metrics` 清零后重新开始。
#[test]
fn metrics_reset_clears_counters() {
    let data = fixture("cli_16_stereo_44k_default.flac");
    let cap = Capacity::DESKTOP;
    let Ok(mut d) = FlacDecoder::new(cap, leak(cap.workspace_len()), leak(cap.pcm_len())) else {
        panic!()
    };
    let mut pos = 0;
    loop {
        let Ok(p) = d.decode(&data[pos..], true) else {
            panic!()
        };
        pos += p.consumed;
        match p.status {
            Status::End => break,
            Status::NeedInput => panic!("EOF 后 NeedInput"),
            _ => {
                let ch = d.format().map_or(1, |f| usize::from(f.channels()));
                d.release(d.pcm().len() / ch);
            }
        }
    }
    let m = d.metrics();
    assert!(m.frames > 0 && m.parse_attempts >= m.frames && m.scanned_bits > 0);
    d.reset_metrics();
    let z = d.metrics();
    assert_eq!(
        (
            z.frames,
            z.parse_attempts,
            z.incomplete_attempts,
            z.scanned_bits,
            z.sample_ops
        ),
        (0, 0, 0, 0, 0)
    );
}
