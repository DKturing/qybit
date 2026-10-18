//! TLV 解析：声道映射能力（A1）与音量控制的 dB 信息（A2）。
//!
//! 出处：内核 `include/uapi/sound/tlv.h` 与 `sound/core/pcm_lib.c`（`pcm_chmap_ctl_tlv`）：
//! “Playback Channel Map” 控制元素的 TLV 是一个容器（`SNDRV_CTL_TLVT_CONTAINER`），其中每项为
//! `类型（CHMAP_FIXED / VAR / PAIRED）, 字节长度, 位置值 …`。与 alsa-lib
//! `snd_pcm_query_chmaps_from_hw` 的规则相同：顶层不是容器时视为不支持；长度不是 4 的倍数或
//! 越界时停止解析。位置值带标志位（相位反转、驱动自定义）或类型未知的映射不列出（F1：布局
//! 无法确认时拒绝）。

use qy_pcm_format::alsa::ChmapType;

use super::abi::{
    CHMAP_POSITION_MASK, TLVD_DB_GAIN_MUTE, TLVD_DB_SCALE_MUTE, TLVT_CHMAP_FIXED,
    TLVT_CHMAP_PAIRED, TLVT_CHMAP_VAR, TLVT_CONTAINER, TLVT_DB_LINEAR, TLVT_DB_MINMAX,
    TLVT_DB_MINMAX_MUTE, TLVT_DB_RANGE, TLVT_DB_SCALE,
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

/// 音量控制的 dB 信息，单位 0.01 dB。
///
/// 出处：内核 `include/uapi/sound/tlv.h`；换算规则与 alsa-lib `src/control/tlv.c`
/// （`snd_tlv_convert_to_dB`）相同：DB_SCALE 线性步进、最小档可为静音；DB_MINMAX(_MUTE) 在最小与
/// 最大之间按原始值线性插值（整数除法）；DB_LINEAR 在幅度上线性；DB_RANGE 按原始值区间分段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbInfo {
    Scale {
        min: i32,
        step: i32,
        mute: bool,
    },
    MinMax {
        min: i32,
        max: i32,
        mute: bool,
    },
    Linear {
        min: i32,
        max: i32,
    },
    /// （原始值下限, 上限, 该段的 dB 信息）。
    Range(Vec<(i64, i64, DbInfo)>),
}

/// 一个档位的增益。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Db {
    Mute,
    /// 0.01 dB。
    Centi(i64),
}

impl Db {
    fn from_centi(c: i64) -> Self {
        if c <= i64::from(TLVD_DB_GAIN_MUTE) {
            Db::Mute
        } else {
            Db::Centi(c)
        }
    }
}

/// DB_RANGE 的嵌套深度上限（防止恶意或损坏的 TLV）。
const MAX_DEPTH: u32 = 4;

/// 解析元素 TLV 中的 dB 信息（容器中取第一项 dB 信息）；没有或无法解析为 `None`。
pub fn parse_db(words: &[u32]) -> Option<DbInfo> {
    parse_db_at(words, 0).map(|(d, _)| d)
}

/// 解析 `words` 开头的一项 TLV，返回 dB 信息与该项所占字数。
fn parse_db_at(words: &[u32], depth: u32) -> Option<(DbInfo, usize)> {
    if depth > MAX_DEPTH {
        return None;
    }
    let (&kind, &len) = (words.first()?, words.get(1)?);
    if len % 4 != 0 {
        return None;
    }
    let n = (len / 4) as usize;
    let body = words.get(2..2 + n)?;
    let item = 2 + n;
    let pair = || -> Option<(i32, i32)> { Some((*body.first()? as i32, *body.get(1)? as i32)) };
    let info = match kind {
        TLVT_DB_SCALE => {
            let (min, w) = pair()?;
            let w = w as u32;
            DbInfo::Scale {
                min,
                step: (w & 0xffff) as i32,
                mute: w & TLVD_DB_SCALE_MUTE != 0,
            }
        }
        TLVT_DB_MINMAX | TLVT_DB_MINMAX_MUTE => {
            let (min, max) = pair()?;
            DbInfo::MinMax {
                min,
                max,
                mute: kind == TLVT_DB_MINMAX_MUTE,
            }
        }
        TLVT_DB_LINEAR => {
            let (min, max) = pair()?;
            DbInfo::Linear { min, max }
        }
        TLVT_DB_RANGE => {
            let mut parts = Vec::new();
            let mut rest = body;
            while let [lo, hi, tail @ ..] = rest {
                let (sub, used) = parse_db_at(tail, depth + 1)?;
                parts.push((i64::from(*lo as i32), i64::from(*hi as i32), sub));
                rest = &tail[used..];
            }
            if parts.is_empty() {
                return None;
            }
            DbInfo::Range(parts)
        }
        TLVT_CONTAINER => {
            let mut rest = body;
            loop {
                let l = *rest.get(1)?;
                let used = 2 + (l / 4) as usize;
                // 嵌套深度在 parse_db_at 中限制。
                if let Some((d, _)) = parse_db_at(rest, depth + 1) {
                    return Some((d, item));
                }
                rest = rest.get(used..)?;
            }
        }
        _ => return None,
    };
    Some((info, item))
}

impl DbInfo {
    /// 原始值 `raw` 的增益；`min`、`max` 为元素的原始值范围。
    pub fn to_db(&self, raw: i64, min: i64, max: i64) -> Option<Db> {
        match *self {
            DbInfo::Scale {
                min: dmin,
                step,
                mute,
            } => {
                if mute && raw == min {
                    return Some(Db::Mute);
                }
                let c = i64::from(dmin) + i64::from(step) * raw.checked_sub(min)?;
                Some(Db::from_centi(c))
            }
            DbInfo::MinMax {
                min: dmin,
                max: dmax,
                mute,
            } => {
                let (dmin, dmax) = (i64::from(dmin), i64::from(dmax));
                if raw <= min || max <= min {
                    Some(if mute { Db::Mute } else { Db::from_centi(dmin) })
                } else if raw >= max {
                    Some(Db::from_centi(dmax))
                } else {
                    let c = (dmax - dmin) * (raw - min) / (max - min) + dmin;
                    Some(Db::from_centi(c))
                }
            }
            DbInfo::Linear {
                min: dmin,
                max: dmax,
            } => {
                if raw <= min || max <= min {
                    Some(Db::from_centi(i64::from(dmin)))
                } else if raw >= max {
                    Some(Db::from_centi(i64::from(dmax)))
                } else {
                    let v = (raw - min) as f64 / (max - min) as f64;
                    let c = if dmin <= TLVD_DB_GAIN_MUTE {
                        (2000.0 * v.log10()) as i64 + i64::from(dmax)
                    } else {
                        let lmin = 10f64.powf(f64::from(dmin) / 2000.0);
                        let lmax = 10f64.powf(f64::from(dmax) / 2000.0);
                        (2000.0 * ((lmax - lmin) * v + lmin).log10()) as i64
                    };
                    Some(Db::from_centi(c))
                }
            }
            DbInfo::Range(ref parts) => parts
                .iter()
                .find(|(lo, hi, _)| (*lo..=*hi).contains(&raw))
                .and_then(|(lo, hi, d)| d.to_db(raw, *lo, *hi)),
        }
    }
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

    /// 用户机器上一块 HD-Audio 编解码器的 Master：DB_SCALE −65.25 dB 起、每档 0.75 dB、原始值
    /// 0..=87（amixer 显示 0 档为 −65.25 dB，87 档为 0 dB）。
    #[test]
    fn db_scale() {
        let words = [TLVT_DB_SCALE, 8, (-6525i32) as u32, 75];
        let d = parse_db(&words);
        assert_eq!(
            d,
            Some(DbInfo::Scale {
                min: -6525,
                step: 75,
                mute: false
            })
        );
        let Some(d) = d else { return };
        assert_eq!(d.to_db(0, 0, 87), Some(Db::Centi(-6525)));
        assert_eq!(d.to_db(87, 0, 87), Some(Db::Centi(0)));
        // 静音标志：最小档为静音。
        let m = parse_db(&[TLVT_DB_SCALE, 8, (-4500i32) as u32, 30 | TLVD_DB_SCALE_MUTE]);
        assert_eq!(m.and_then(|m| m.to_db(-50, -50, 100)), Some(Db::Mute));
    }

    #[test]
    fn db_minmax_linear_and_range() {
        // MINMAX：−60 dB..0 dB，原始值 0..=100，中点按整数除法为 −30 dB。
        let mm = parse_db(&[TLVT_DB_MINMAX_MUTE, 8, (-6000i32) as u32, 0]);
        let Some(mm) = mm else { panic!("minmax") };
        assert_eq!(mm.to_db(0, 0, 100), Some(Db::Mute));
        assert_eq!(mm.to_db(50, 0, 100), Some(Db::Centi(-3000)));
        assert_eq!(mm.to_db(100, 0, 100), Some(Db::Centi(0)));
        // LINEAR：静音..0 dB，幅度线性：一半幅度为 20·log10(0.5) = −6.0206 dB → −602。
        let lin = parse_db(&[TLVT_DB_LINEAR, 8, TLVD_DB_GAIN_MUTE as u32, 0]);
        let Some(lin) = lin else { panic!("linear") };
        assert_eq!(lin.to_db(0, 0, 100), Some(Db::Mute));
        assert_eq!(lin.to_db(50, 0, 100), Some(Db::Centi(-602)));
        // RANGE：0..=1 静音/−50 dB 的 MINMAX_MUTE，2..=10 为 −40 dB 起每档 5 dB；外包一层容器。
        let range = [
            TLVT_CONTAINER,
            4 * 14,
            TLVT_DB_RANGE,
            4 * 12,
            0,
            1,
            TLVT_DB_MINMAX_MUTE,
            8,
            (-5000i32) as u32,
            (-5000i32) as u32,
            2,
            10,
            TLVT_DB_SCALE,
            8,
            (-4000i32) as u32,
            500,
        ];
        let Some(r) = parse_db(&range) else {
            panic!("range")
        };
        assert_eq!(r.to_db(0, 0, 10), Some(Db::Mute));
        assert_eq!(r.to_db(2, 0, 10), Some(Db::Centi(-4000)));
        assert_eq!(r.to_db(10, 0, 10), Some(Db::Centi(0)));
        assert_eq!(r.to_db(11, 0, 10), None);
    }

    #[test]
    fn malformed_db_is_none() {
        assert_eq!(parse_db(&[]), None);
        assert_eq!(parse_db(&[TLVT_DB_SCALE, 6, 0, 0]), None);
        assert_eq!(parse_db(&[TLVT_DB_SCALE, 40, 0, 0]), None);
        assert_eq!(parse_db(&[TLVT_CHMAP_FIXED, 4, 3]), None);
        assert_eq!(parse_db(&[TLVT_DB_RANGE, 8, 0, 1]), None);
        // RANGE 套 RANGE：两层可以解析，超过深度上限的拒绝。
        let nest = |levels: usize| {
            let mut t = vec![TLVT_DB_SCALE, 8, 0, 1];
            for _ in 0..levels {
                let mut outer = vec![TLVT_DB_RANGE, 4 * (2 + t.len() as u32), 0, 1];
                outer.extend(t);
                t = outer;
            }
            t
        };
        assert!(parse_db(&nest(2)).is_some());
        assert_eq!(parse_db(&nest(6)), None);
    }
}
