//! 参考工具核对：libFLAC 显式格式参考解码、metaflac 实际格式、逐夹具分支要求、
//! 种子与 SHA-256 摘要（M1 T3）。

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use super::{libflac_decode, run};
use crate::Result;

/// 各夹具使用的伪随机种子（固定常量，见 `flac.rs` 中对应生成调用）。
pub fn seeds(name: &str) -> &'static str {
    match name {
        "cli_16_stereo_44k_default" => "smooth 11",
        "cli_16_stereo_exhaustive_ms" => "smooth 12",
        "cli_24_mono_96k_lpc12" => "smooth 13",
        "cli_24_stereo_fixed_only" => "smooth 14",
        "cli_32_stereo_192k" => "smooth 15",
        "cli_32_noise_verbatim" => "noise 16",
        "cli_16_constant_channel" => "smooth 17",
        "cli_16_wasted_bits" => "smooth 18",
        "cli_24_rice2_noise" => "noise 19 + smooth 20",
        "cli_16_blocksize_1000" => "smooth 21",
        "cli_16_blocksize_16384" => "smooth 22",
        "max_block_verbatim_32_stereo" => "noise 101",
        "rice2_partitions" => "noise 202",
        "wasted_bits" => "smooth 303",
        "fixed_orders_and_lpc32" => "smooth 404",
        "variable_blocksize_uncommon" => "smooth 505",
        "rate_code12_7khz" => "smooth 7606",
        "rate_code13_11025" => "smooth 11631",
        "rate_code14_100k" => "smooth 100606",
        "rate_384k_stereo_24" => "smooth 384000",
        "metadata_skip" | "unknown_max_frame" | "lying_max_frame" => "smooth 707",
        "short_tail_frame" => "smooth 808",
        n if n.starts_with("err_") => "smooth 909 / 1001–1004（错误夹具）",
        _ => "无（确定性构造）",
    }
}

/// 每个有效夹具必须由 libFLAC 分析确认出现的分支：`a|b` 任选其一，`!x` 不得出现。
pub fn expected_branches(name: &str) -> &'static [&'static str] {
    match name {
        "cli_16_stereo_44k_default" => &["type=LPC"],
        "cli_16_stereo_exhaustive_ms" => &[
            "channel_assignment=MID_SIDE|channel_assignment=LEFT_SIDE|channel_assignment=RIGHT_SIDE",
        ],
        "cli_24_mono_96k_lpc12" => &["type=LPC", "!type=LPC order=32"],
        "cli_24_stereo_fixed_only" => &[
            "!type=LPC",
            "type=FIXED order=1|type=FIXED order=2|type=FIXED order=3|type=FIXED order=4",
        ],
        "cli_32_stereo_192k" => {
            &["type=LPC|type=FIXED order=2|type=FIXED order=3|type=FIXED order=4"]
        }
        "cli_32_noise_verbatim" => &["type=VERBATIM"],
        "cli_16_constant_channel" => &["type=CONSTANT"],
        "cli_16_wasted_bits" => &["wasted_bits>0"],
        "cli_24_rice2_noise" => &["residual_type=RICE2"],
        "cli_16_blocksize_1000" => &["blocksize=1000"],
        "cli_16_blocksize_16384" => &["blocksize=16384"],
        "max_block_verbatim_32_stereo" => &["blocksize=65535", "type=VERBATIM"],
        "side_33bit_extremes" => &[
            "channel_assignment=LEFT_SIDE",
            "channel_assignment=RIGHT_SIDE",
            "channel_assignment=MID_SIDE",
            "type=VERBATIM",
            "type=LPC",
            "residual_type=RICE2",
        ],
        "escape_partitions" => &["escape_partition", "type=FIXED order=2"],
        "rice2_partitions" => &[
            "residual_type=RICE2",
            "escape_partition",
            "type=FIXED order=0",
        ],
        "wasted_bits" => &[
            "wasted_bits>0",
            "type=CONSTANT",
            "type=LPC",
            "type=VERBATIM",
        ],
        "fixed_orders_and_lpc32" => &[
            "type=FIXED order=0",
            "type=FIXED order=1",
            "type=FIXED order=2",
            "type=FIXED order=3",
            "type=FIXED order=4",
            "type=LPC order=32",
        ],
        "variable_blocksize_uncommon" => &[
            "blocksize=16",
            "blocksize=1000",
            "blocksize=192",
            "blocksize=4608",
            "blocksize=300",
            "channel_assignment=MID_SIDE",
            "channel_assignment=RIGHT_SIDE",
        ],
        "short_tail_frame" => &["blocksize=17", "blocksize=512"],
        "rate_384k_stereo_24" => &["channel_assignment=MID_SIDE"],
        _ => &[],
    }
}

/// libFLAC `--analyze` 输出中出现的分支键集合。
pub fn analyze_keys(ana: &str) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for line in ana.lines() {
        let t = line.trim_start();
        let field = |key: &str| {
            t.split('\t')
                .find_map(|x| x.strip_prefix(key).map(str::to_owned))
        };
        if let Some(ty) = field("type=") {
            keys.insert(format!("type={ty}"));
            if let Some(o) = field("order=") {
                keys.insert(format!("type={ty} order={o}"));
            }
        }
        for key in ["channel_assignment=", "residual_type=", "blocksize="] {
            if let Some(v) = field(key) {
                keys.insert(format!("{key}{v}"));
            }
        }
        if field("wasted_bits=").is_some_and(|w| w != "0") {
            keys.insert("wasted_bits>0".into());
        }
        if t.starts_with("parameter[") && t.contains("=ESCAPE") {
            keys.insert("escape_partition".into());
        }
    }
    keys
}

pub fn check_branches(name: &str, keys: &BTreeSet<String>) -> Result<()> {
    for want in expected_branches(name) {
        let ok = match want.strip_prefix('!') {
            Some(neg) => !keys.contains(neg),
            None => want.split('|').any(|alt| keys.contains(alt)),
        };
        if !ok {
            return Err(format!("{name}: libFLAC 分析不满足分支要求 {want}").into());
        }
    }
    Ok(())
}

/// metaflac 读取的实际格式：(采样率, 声道, 位深, 最大块, 总样本数)。
pub fn metaflac_info(path: &Path) -> Result<(u32, u32, u32, u32, u64)> {
    let out = run(Command::new("metaflac")
        .args([
            "--show-sample-rate",
            "--show-channels",
            "--show-bps",
            "--show-max-blocksize",
            "--show-total-samples",
        ])
        .arg(path))?;
    let text = String::from_utf8_lossy(&out);
    let v: Vec<u64> = text
        .lines()
        .map(|l| l.trim().parse().unwrap_or(u64::MAX))
        .collect();
    if v.len() != 5 {
        return Err(format!("metaflac 输出异常: {text}").into());
    }
    Ok((v[0] as u32, v[1] as u32, v[2] as u32, v[3] as u32, v[4]))
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// libFLAC 参考解码。位深为 8/16/24/32 时输出显式格式的原始 PCM（有符号、小端、
/// 位深 = 源位深），返回 (样本, "raw")；其他位深（12/20）输出 WAV，再按容器与有效位
/// 还原，返回 (样本, "wav")。
pub fn libflac_reference(path: &Path, bits: u32, tmp: &Path) -> Result<(Vec<i64>, &'static str)> {
    if bits % 8 == 0 {
        let raw = tmp.join("ref.raw");
        run(Command::new("flac")
            .args([
                "--silent",
                "--force",
                "--decode",
                "--force-raw-format",
                "--endian=little",
                "--sign=signed",
                "-o",
            ])
            .arg(&raw)
            .arg(path))?;
        let bytes = std::fs::read(&raw)?;
        let bps = (bits / 8) as usize;
        let samples = bytes
            .chunks_exact(bps)
            .map(|c| {
                let mut v: i64 = 0;
                for (k, &b) in c.iter().enumerate() {
                    v |= i64::from(b) << (8 * k);
                }
                let sh = 64 - bits;
                (v << sh) >> sh
            })
            .collect();
        return Ok((samples, "raw"));
    }
    Ok((libflac_decode(path, tmp)?, "wav"))
}

/// 要求 Xiph flac ≥ 1.4.0（32 位支持）。
pub fn check_version(version: &str) -> Result<()> {
    let v: Vec<u32> = version
        .split_whitespace()
        .last()
        .unwrap_or("0")
        .split('.')
        .map(|x| x.parse().unwrap_or(0))
        .collect();
    if v.as_slice() < [1, 4, 0].as_slice() {
        return Err(format!("需要 Xiph flac ≥ 1.4.0，当前 {version}").into());
    }
    Ok(())
}
