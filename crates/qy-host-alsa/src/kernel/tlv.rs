//! 声道映射能力的 TLV 解析（A1）。
//!
//! 出处：内核 `include/uapi/sound/tlv.h` 与 `sound/core/pcm_lib.c`（`pcm_chmap_ctl_tlv`）：
//! “Playback Channel Map” 控制元素的 TLV 是一个容器（`SNDRV_CTL_TLVT_CONTAINER`），其中每项为
//! `类型（CHMAP_FIXED / VAR / PAIRED）, 字节长度, 位置值 …`。与 alsa-lib
//! `snd_pcm_query_chmaps_from_hw` 的规则相同：顶层不是容器时视为不支持；长度不是 4 的倍数或
//! 越界时停止解析。位置值带标志位（相位反转、驱动自定义）或类型未知的映射不列出（F1：布局
//! 无法确认时拒绝）。

use qy_pcm_format::alsa::ChmapType;

use super::abi::{
    CHMAP_POSITION_MASK, TLVT_CHMAP_FIXED, TLVT_CHMAP_PAIRED, TLVT_CHMAP_VAR, TLVT_CONTAINER,
};

/// 解析 TLV 字（容器在前）；不支持或无法解析时为空。
pub fn parse_chmaps(words: &[u32]) -> Vec<(ChmapType, Vec<u32>)> {
    let mut out = Vec::new();
    let (Some(&kind), Some(&len)) = (words.first(), words.get(1)) else {
        return out;
    };
    if kind != TLVT_CONTAINER || len % 4 != 0 {
        return out;
    }
    let Some(body) = words.get(2..2 + (len / 4) as usize) else {
        return out;
    };
    let mut rest = body;
    while let [t, l, tail @ ..] = rest {
        if l % 4 != 0 {
            break;
        }
        let n = (*l / 4) as usize;
        let Some(positions) = tail.get(..n) else {
            break;
        };
        let kind = match *t {
            TLVT_CHMAP_FIXED => Some(ChmapType::Fixed),
            TLVT_CHMAP_VAR => Some(ChmapType::Var),
            TLVT_CHMAP_PAIRED => Some(ChmapType::Paired),
            _ => None,
        };
        let clean = positions.iter().all(|&p| p & !CHMAP_POSITION_MASK == 0);
        if let Some(k) = kind
            && clean
            && !positions.is_empty()
        {
            out.push((k, positions.to_vec()));
        }
        rest = &tail[n..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 驱动常见的形式（`snd_pcm_std_chmaps`）：单声道 FC、立体声 FL FR，均为固定映射；
    /// 一个带相位反转标志的映射与一个未知类型的项被跳过。
    #[test]
    fn container_with_fixed_maps() {
        let words = [
            TLVT_CONTAINER,
            4 * (3 + 4 + 4 + 3),
            TLVT_CHMAP_FIXED,
            4,
            2, // FC
            TLVT_CHMAP_FIXED,
            8,
            3,
            4, // FL FR
            TLVT_CHMAP_VAR,
            8,
            3,
            4 | 0x1_0000, // FR 相位反转：不列出
            0x999,
            4,
            7, // 未知类型
        ];
        assert_eq!(
            parse_chmaps(&words),
            vec![(ChmapType::Fixed, vec![2]), (ChmapType::Fixed, vec![3, 4])]
        );
    }

    #[test]
    fn malformed_input_is_not_listed() {
        assert!(parse_chmaps(&[]).is_empty());
        assert!(
            parse_chmaps(&[TLVT_CHMAP_FIXED, 4, 3]).is_empty(),
            "顶层不是容器"
        );
        assert!(
            parse_chmaps(&[TLVT_CONTAINER, 6, 0, 0]).is_empty(),
            "长度不是 4 的倍数"
        );
        assert!(parse_chmaps(&[TLVT_CONTAINER, 400, TLVT_CHMAP_FIXED, 4, 3]).is_empty());
        // 项的长度越界：停止，之前的项保留。
        let w = [
            TLVT_CONTAINER,
            24,
            TLVT_CHMAP_PAIRED,
            8,
            3,
            4,
            TLVT_CHMAP_FIXED,
            40,
        ];
        assert_eq!(parse_chmaps(&w), vec![(ChmapType::Paired, vec![3, 4])]);
    }
}
