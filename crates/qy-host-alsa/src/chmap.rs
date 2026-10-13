//! ALSA 声道映射的文本解析（F1，纯逻辑，跨平台可测）。
//!
//! alsa crate 0.12.1 把映射转换为 `Vec<ChmapPosition>` 时对未知值 `unwrap`（反相标志、驱动
//! 自定义位置会 panic），其原始位置数组又不公开。为不引入 `unsafe` 也不 panic，本模块读取
//! alsa-lib 自己的文本形式（`snd_pcm_chmap_print`，即 `Chmap` 的 `Display`）并严格解析为
//! 原始位置值，再交给 `qy_pcm_format::alsa::layout_from_chmap` 判定。
//!
//! 文本格式（alsa-lib `src/pcm/pcm.c` 的 `snd_pcm_chmap_print`，已核对源码）：声道之间一个空格；
//! 标准位置打印 `chmap_names` 表中的名字（`UNKNOWN NA MONO FL FR RL RR FC LFE SL SR RC FLC FRC
//! RLC RRC FLW FRW FLH FCH FRH TC TFL TFR TFC TRL TRR TRC TFLC TFRC TSL TSR LLFE RLFE BC BLC
//! BRC`，序号即位置值）；驱动自定义位置打印十进制数字；表中无名字时打印 `Ch<值>`；反相声道
//! 追加 `[INV]`。

use qy_pcm_format::alsa::{CHMAP_DRIVER_SPEC, CHMAP_PHASE_INVERSE};

/// alsa-lib `chmap_names`，下标为 `SND_CHMAP_*` 位置值。
const NAMES: [&str; 37] = [
    "UNKNOWN", "NA", "MONO", "FL", "FR", "RL", "RR", "FC", "LFE", "SL", "SR", "RC", "FLC", "FRC",
    "RLC", "RRC", "FLW", "FRW", "FLH", "FCH", "FRH", "TC", "TFL", "TFR", "TFC", "TRL", "TRR",
    "TRC", "TFLC", "TFRC", "TSL", "TSR", "LLFE", "RLFE", "BC", "BLC", "BRC",
];

/// 把 `snd_pcm_chmap_print` 的文本解析为原始位置值（含反相、驱动自定义标志）。
/// 不认识的记号返回 `None`（调用方按“映射不可确认”处理）。
pub fn parse_chmap_text(text: &str) -> Option<Vec<u32>> {
    let mut out = Vec::new();
    for tok in text.split(' ').filter(|t| !t.is_empty()) {
        let (name, inv) = match tok.strip_suffix("[INV]") {
            Some(n) => (n, CHMAP_PHASE_INVERSE),
            None => (tok, 0),
        };
        let v = if let Some(i) = NAMES.iter().position(|&n| n == name) {
            i as u32
        } else if let Some(d) = name.strip_prefix("Ch") {
            d.parse::<u32>().ok()?
        } else if name.bytes().all(|b| b.is_ascii_digit()) && !name.is_empty() {
            name.parse::<u32>().ok()? | CHMAP_DRIVER_SPEC
        } else {
            return None;
        };
        out.push(v | inv);
    }
    (!out.is_empty()).then_some(out)
}

/// 位置值的 alsa-lib 名字（报告用）。
pub fn chmap_text(raw: &[u32]) -> String {
    raw.iter()
        .map(|&p| {
            let pos = (p & 0xffff) as usize;
            let name = if p & CHMAP_DRIVER_SPEC != 0 {
                pos.to_string()
            } else {
                NAMES
                    .get(pos)
                    .map_or(format!("Ch{pos}"), |n| (*n).to_string())
            };
            if p & CHMAP_PHASE_INVERSE != 0 {
                format!("{name}[INV]")
            } else {
                name
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_alsa_lib_text() {
        // 独立写出：FL=3 FR=4 RL=5 RR=6 FC=7 LFE=8。
        assert_eq!(
            parse_chmap_text("FL FR RL RR FC LFE"),
            Some(vec![3, 4, 5, 6, 7, 8])
        );
        assert_eq!(parse_chmap_text("MONO"), Some(vec![2]));
        assert_eq!(
            parse_chmap_text("FL FR[INV]"),
            Some(vec![3, 4 | CHMAP_PHASE_INVERSE])
        );
        assert_eq!(
            parse_chmap_text("FL 5"),
            Some(vec![3, 5 | CHMAP_DRIVER_SPEC])
        );
        assert_eq!(parse_chmap_text("FL Ch40"), Some(vec![3, 40]));
        assert_eq!(parse_chmap_text("BRC TSR"), Some(vec![36, 31]));
        assert_eq!(parse_chmap_text("FL XYZ"), None);
        assert_eq!(parse_chmap_text(""), None);
        // 往返。
        let all: Vec<u32> = (0..37).collect();
        assert_eq!(parse_chmap_text(&chmap_text(&all)), Some(all));
        let odd = vec![3 | CHMAP_PHASE_INVERSE, 9 | CHMAP_DRIVER_SPEC, 40];
        assert_eq!(parse_chmap_text(&chmap_text(&odd)), Some(odd));
    }
}
