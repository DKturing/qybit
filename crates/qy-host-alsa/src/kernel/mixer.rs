//! 混音器控制的纯逻辑（A2、A4）：按控制名分组、显示、音量百分比换算、播放时音量控制的选择、
//! 播放前的静音提示。
//!
//! 只描述设备自身的硬件控制（内核驱动提供的混音器元素），不涉及送出的数据。命名规则与
//! alsa-lib 简单混音器（`src/mixer/simple_none.c`）相同：元素名以 ` Playback Volume`、
//! ` Playback Switch`、` Capture Volume`、` Capture Switch`、` Volume`、` Switch` 结尾时，去掉后缀
//! 得到基名，同一基名、序号与方向的元素合为一组；其余元素单独成组。
//!
//! 百分比与 alsamixer 相同（alsa-utils `alsamixer/volume_mapping.c`）：有 dB 信息时，dB 范围不超过
//! 24 dB 按 dB 线性，否则按 10^(dB/60) 映射（接近听感，各设备的 40% 响度相近）；没有 dB 信息时按
//! 原始值线性。

use super::abi::{
    ELEM_ACCESS_INACTIVE, ELEM_ACCESS_READ, ELEM_ACCESS_WRITE, ElemId, TLVD_DB_GAIN_MUTE,
};
use super::tlv::{Db, DbInfo};

/// 控制的方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Playback,
    Capture,
    /// 名称中没有写明方向（如 snd-dummy 的 `Master Volume`）。
    Unspecified,
}

/// 元素在组中的作用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Volume,
    Switch,
    Other,
}

/// 拆分元素名：（基名, 方向, 作用）。
pub fn split_name(name: &str) -> (&str, Dir, Role) {
    // alsa-lib `base_len` 的特例：这两个名称本身就是录音方向的总控制。
    match name {
        "Capture Volume" => return ("Capture", Dir::Capture, Role::Volume),
        "Capture Switch" => return ("Capture", Dir::Capture, Role::Switch),
        _ => {}
    }
    const SUFFIXES: [(&str, Dir, Role); 6] = [
        (" Playback Volume", Dir::Playback, Role::Volume),
        (" Playback Switch", Dir::Playback, Role::Switch),
        (" Capture Volume", Dir::Capture, Role::Volume),
        (" Capture Switch", Dir::Capture, Role::Switch),
        (" Volume", Dir::Unspecified, Role::Volume),
        (" Switch", Dir::Unspecified, Role::Switch),
    ];
    for (suffix, dir, role) in SUFFIXES {
        // 基名以 “-” 结尾时不拆分（alsa-lib：如 “3D Control - Switch”）。
        if let Some(base) = name.strip_suffix(suffix)
            && !base.is_empty()
            && !base.ends_with('-')
        {
            return (base, dir, role);
        }
    }
    (name, Dir::Unspecified, Role::Other)
}

/// 元素的值类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Boolean,
    /// INTEGER 或 INTEGER64（`wide`）。
    Integer {
        min: i64,
        max: i64,
        step: i64,
        wide: bool,
    },
    Enumerated {
        items: Vec<String>,
    },
    /// 其他类型（BYTES、IEC958 等）：只列出，不显示也不设置值。
    Other(i32),
}

/// 一个混音器元素及其当前值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Control {
    pub id: ElemId,
    pub name: String,
    pub index: u32,
    pub kind: Kind,
    pub access: u32,
    /// 各声道的值：开关为 0/1，整数为原始值，枚举为项号。不可读时为空。
    pub values: Vec<i64>,
    pub db: Option<DbInfo>,
}

impl Control {
    pub fn writable(&self) -> bool {
        self.access & ELEM_ACCESS_WRITE != 0
    }

    pub fn readable(&self) -> bool {
        self.access & ELEM_ACCESS_READ != 0
    }

    pub fn inactive(&self) -> bool {
        self.access & ELEM_ACCESS_INACTIVE != 0
    }

    fn role(&self) -> Role {
        split_name(&self.name).2
    }

    /// 值的文字描述（不含名称）。
    pub fn describe(&self) -> String {
        let mut s = match &self.kind {
            Kind::Boolean => {
                let v: Vec<&str> = self
                    .values
                    .iter()
                    .map(|&v| if v != 0 { "开" } else { "关" })
                    .collect();
                format!("开关 {}", join_same(&v))
            }
            Kind::Integer { min, max, .. } => {
                let raws: Vec<String> = self.values.iter().map(|v| v.to_string()).collect();
                let pct: Vec<String> = self
                    .values
                    .iter()
                    .map(|&v| format!("{}%", volume_percent(*min, *max, self.db.as_ref(), v)))
                    .collect();
                let mut out = format!(
                    "音量 {}（范围 {min}..{max}），{}",
                    join_same(&raws),
                    join_same(&pct)
                );
                if let Some(db) = &self.db {
                    let dbs: Vec<String> = self
                        .values
                        .iter()
                        .map(|&v| match db.to_db(v, *min, *max) {
                            Some(d) => format_db(d),
                            None => "dB 未知".into(),
                        })
                        .collect();
                    out.push_str(&format!("，{}", join_same(&dbs)));
                }
                out
            }
            Kind::Enumerated { items } => {
                let cur: Vec<String> = self
                    .values
                    .iter()
                    .map(|&v| {
                        usize::try_from(v)
                            .ok()
                            .and_then(|i| items.get(i))
                            .cloned()
                            .unwrap_or_else(|| format!("#{v}"))
                    })
                    .collect();
                format!("{}（可选：{}）", join_same(&cur), items.join("、"))
            }
            Kind::Other(t) => format!("类型 {t}，不显示"),
        };
        if !self.readable() {
            s = "不可读".into();
        }
        if !self.writable() {
            s.push_str("（只读）");
        }
        if self.inactive() {
            s.push_str("（未启用）");
        }
        s
    }

    /// 处于关闭或最小音量（含 dB 静音）时的简短描述。
    fn silent(&self) -> Option<String> {
        if self.inactive() || self.values.is_empty() {
            return None;
        }
        match &self.kind {
            Kind::Boolean if self.role() == Role::Switch && self.values.iter().all(|&v| v == 0) => {
                Some("关".into())
            }
            Kind::Integer { min, max, .. } if self.role() == Role::Volume => {
                // 有 dB 信息时：静音，或最小档且增益低于 0 dB（最小档为 0 dB 的增益控制，如
                // 话筒增强，不使声音变小）；没有 dB 信息时：最小档。
                let mute = |v: i64| match self.db.as_ref().map(|d| d.to_db(v, *min, *max)) {
                    Some(Some(Db::Mute)) => true,
                    Some(Some(Db::Centi(c))) => v <= *min && c < 0,
                    _ => v <= *min,
                };
                self.values.iter().all(|&v| mute(v)).then(|| {
                    let v = self.values[0];
                    let mut t = format!("{}%", volume_percent(*min, *max, self.db.as_ref(), v));
                    if let Some(d) = self.db.as_ref().and_then(|d| d.to_db(v, *min, *max)) {
                        t.push_str(&format!("，{}", format_db(d)));
                    }
                    t
                })
            }
            _ => None,
        }
    }
}

/// 各声道相同时只写一次，否则依次列出。
fn join_same<T: AsRef<str> + PartialEq>(v: &[T]) -> String {
    match v.first() {
        None => "—".into(),
        Some(f) if v.iter().all(|x| x == f) => f.as_ref().to_string(),
        _ => v.iter().map(AsRef::as_ref).collect::<Vec<_>>().join(" / "),
    }
}

/// 增益的文字（`-65.25 dB`、`静音`）。
pub fn format_db(d: Db) -> String {
    match d {
        Db::Mute => "静音".into(),
        Db::Centi(c) => {
            let sign = if c < 0 { "-" } else { "" };
            let a = c.unsigned_abs();
            format!("{sign}{}.{:02} dB", a / 100, a % 100)
        }
    }
}

/// 原始值换算为百分比（四舍五入，与 amixer 相同的线性换算）。
pub fn raw_to_percent(min: i64, max: i64, raw: i64) -> u32 {
    if max <= min {
        return 0;
    }
    let span = i128::from(max) - i128::from(min);
    let off = (i128::from(raw) - i128::from(min)).clamp(0, span);
    u32::try_from((off * 200 + span) / (span * 2)).unwrap_or(100)
}

/// alsamixer 的线性/对数分界：dB 范围不超过 24 dB 时按 dB 线性。
const MAX_LINEAR_DB_SCALE: i64 = 2400;

/// 原始值范围两端的增益（0.01 dB）；静音按 alsa-lib 的 `SND_CTL_TLV_DB_GAIN_MUTE` 计。
fn db_range(info: &DbInfo, min: i64, max: i64) -> Option<(i64, i64)> {
    let c = |d: Db| match d {
        Db::Mute => i64::from(TLVD_DB_GAIN_MUTE),
        Db::Centi(c) => c,
    };
    let lo = c(info.to_db(min, min, max)?);
    let hi = c(info.to_db(max, min, max)?);
    (lo < hi).then_some((lo, hi))
}

/// 增益（0.01 dB）对应的归一化音量（0..=1），与 alsamixer `get_normalized_volume` 相同。
fn normalized_from_db(db: i64, lo: i64, hi: i64) -> f64 {
    if hi - lo <= MAX_LINEAR_DB_SCALE {
        return (db - lo) as f64 / (hi - lo) as f64;
    }
    let mut n = 10f64.powf((db - hi) as f64 / 6000.0);
    if lo != i64::from(TLVD_DB_GAIN_MUTE) {
        let min_norm = 10f64.powf((lo - hi) as f64 / 6000.0);
        n = (n - min_norm) / (1.0 - min_norm);
    }
    n
}

/// 原始值的音量百分比（四舍五入）：有 dB 信息时按 alsamixer 的映射，否则按原始值线性。
pub fn volume_percent(min: i64, max: i64, db: Option<&DbInfo>, raw: i64) -> u32 {
    let mapped = db.and_then(|d| {
        let (lo, hi) = db_range(d, min, max)?;
        let v = match d.to_db(raw, min, max)? {
            Db::Mute => i64::from(TLVD_DB_GAIN_MUTE),
            Db::Centi(c) => c,
        };
        Some(normalized_from_db(v.clamp(lo, hi), lo, hi))
    });
    match mapped {
        Some(n) => (n.clamp(0.0, 1.0) * 100.0).round() as u32,
        None => raw_to_percent(min, max, raw),
    }
}

/// 百分比对应的原始值，与 alsamixer `set_normalized_volume` 相同：先按映射求目标增益，再取增益
/// 最接近目标的档位（相同时取较小者）；没有 dB 信息时按原始值线性（四舍五入后按步长对齐）。
pub fn raw_for_percent(min: i64, max: i64, step: i64, db: Option<&DbInfo>, pct: u32) -> i64 {
    let v = f64::from(pct.min(100)) / 100.0;
    if let Some((d, (lo, hi))) = db.and_then(|d| Some((d, db_range(d, min, max)?))) {
        let target = if hi - lo <= MAX_LINEAR_DB_SCALE {
            (v * (hi - lo) as f64).round() as i64 + lo
        } else {
            let mut w = v;
            if lo != i64::from(TLVD_DB_GAIN_MUTE) {
                let min_norm = 10f64.powf((lo - hi) as f64 / 6000.0);
                w = w * (1.0 - min_norm) + min_norm;
            }
            if w <= 0.0 {
                return min;
            }
            (6000.0 * w.log10()).round() as i64 + hi
        };
        if let Some(r) = nearest_db(d, target, min, max, step) {
            return r;
        }
    }
    raw_linear(min, max, step, pct)
}

/// 原始值线性：四舍五入后按步长对齐。
fn raw_linear(min: i64, max: i64, step: i64, pct: u32) -> i64 {
    if max <= min {
        return min;
    }
    let span = i128::from(max) - i128::from(min);
    let pct = i128::from(pct.min(100));
    let mut off = (span * pct * 2 + 100) / 200;
    if step > 1 {
        let st = i128::from(step);
        off = (off + st / 2) / st * st;
        if off > span {
            off -= st;
        }
    }
    i64::try_from(i128::from(min) + off).unwrap_or(max)
}

/// 增益最接近 `target`（0.01 dB）的档位，相同时取较小者；档位太多时为 `None`。
fn nearest_db(info: &DbInfo, target: i64, min: i64, max: i64, step: i64) -> Option<i64> {
    let step = step.max(1);
    let n = (i128::from(max) - i128::from(min)) / i128::from(step);
    if !(0..=1 << 20).contains(&n) {
        return None;
    }
    let mut best: Option<(i64, i64)> = None;
    let mut raw = min;
    for _ in 0..=n {
        let db = match info.to_db(raw, min, max) {
            Some(Db::Mute) => i64::from(TLVD_DB_GAIN_MUTE),
            Some(Db::Centi(c)) => c,
            None => {
                raw = raw.saturating_add(step);
                continue;
            }
        };
        let dist = (db - target).abs();
        if best.is_none_or(|(_, d)| dist < d) {
            best = Some((raw, dist));
        }
        raw = raw.saturating_add(step);
    }
    best.map(|(r, _)| r)
}

/// 一组控制（同一基名、序号与方向）。
#[derive(Debug)]
pub struct Group<'a> {
    pub base: &'a str,
    pub index: u32,
    pub dir: Dir,
    pub members: Vec<&'a Control>,
}

impl Group<'_> {
    /// 一行描述，如 `Master（播放）：音量 0（范围 0..87），0%，-65.25 dB；开关 关`。
    pub fn line(&self) -> String {
        let mut head = self.base.to_string();
        if self.index != 0 {
            head.push_str(&format!(",{}", self.index));
        }
        match self.dir {
            Dir::Playback => head.push_str("（播放）"),
            Dir::Capture => head.push_str("（录音）"),
            Dir::Unspecified => {}
        }
        let parts: Vec<String> = self.members.iter().map(|c| c.describe()).collect();
        format!("{head}：{}", parts.join("；"))
    }
}

/// 按出现顺序分组。
pub fn groups(controls: &[Control]) -> Vec<Group<'_>> {
    let mut out: Vec<Group<'_>> = Vec::new();
    for c in controls {
        let (base, dir, role) = split_name(&c.name);
        let (base, dir) = if role == Role::Other {
            (c.name.as_str(), Dir::Unspecified)
        } else {
            (base, dir)
        };
        match out
            .iter_mut()
            .find(|g| g.base == base && g.index == c.index && g.dir == dir)
        {
            Some(g) => g.members.push(c),
            None => out.push(Group {
                base,
                index: c.index,
                dir,
                members: vec![c],
            }),
        }
    }
    out
}

/// 播放方向（含未写明方向）中处于关闭或最小音量的组，如 `Master（0%，-65.25 dB；关）`。
/// 最小音量：静音，或最小档且增益低于 0 dB；没有 dB 信息时为最小档。
pub fn silent_playback(controls: &[Control]) -> Vec<String> {
    groups(controls)
        .iter()
        .filter(|g| g.dir != Dir::Capture)
        .filter_map(|g| {
            let parts: Vec<String> = g.members.iter().filter_map(|c| c.silent()).collect();
            (!parts.is_empty()).then(|| {
                let mut name = g.base.to_string();
                if g.index != 0 {
                    name.push_str(&format!(",{}", g.index));
                }
                format!("{name}（{}）", parts.join("；"))
            })
        })
        .collect()
}

/// 按控制名找出元素：先按完整元素名，再按基名找播放方向的组，然后是未写明
/// 方向的组，最后是录音方向的组。名称不区分大小写。`index` 为 `名称,序号` 中的序号（缺省 0）。
pub fn resolve<'a>(controls: &'a [Control], name: &str, index: u32) -> Option<Vec<&'a Control>> {
    let exact: Vec<&Control> = controls
        .iter()
        .filter(|c| c.index == index && c.name.eq_ignore_ascii_case(name))
        .collect();
    if !exact.is_empty() {
        return Some(exact);
    }
    let gs = groups(controls);
    for dir in [Dir::Playback, Dir::Unspecified, Dir::Capture] {
        if let Some(g) = gs
            .iter()
            .find(|g| g.dir == dir && g.index == index && g.base.eq_ignore_ascii_case(name))
        {
            return Some(g.members.clone());
        }
    }
    None
}

/// 播放时调整音量所用的一组控制（A4）。`explicit` 为用户给出的控制名（`名称` 或 `名称,序号`）；
/// 否则按内核文档 *Standard ALSA Control Names*（`Documentation/sound/designs/control-names.rst`）
/// 的标准名称依次找 `Master`、`PCM`（播放方向或未写明方向、含音量的组），都没有时若播放方向
/// 只有一组含音量的控制就用它；仍不能确定时报错并列出候选。
pub fn playback_volume_group<'a>(
    controls: &'a [Control],
    explicit: Option<&str>,
) -> Result<Vec<&'a Control>, String> {
    let has_volume = |m: &[&Control]| {
        m.iter()
            .any(|c| c.role() == Role::Volume && matches!(c.kind, Kind::Integer { .. }))
    };
    let gs = groups(controls);
    let candidates: Vec<&Group<'_>> = gs
        .iter()
        .filter(|g| g.dir != Dir::Capture && has_volume(&g.members))
        .collect();
    let names = || {
        let n: Vec<String> = candidates.iter().map(|g| g.base.to_string()).collect();
        if n.is_empty() {
            "无".to_string()
        } else {
            n.join("、")
        }
    };
    if let Some(name) = explicit {
        let (name, index) = match name.rsplit_once(',') {
            Some((n, i)) if !i.is_empty() && i.bytes().all(|b| b.is_ascii_digit()) => {
                (n, i.parse().unwrap_or(0))
            }
            _ => (name, 0),
        };
        return match resolve(controls, name, index) {
            Some(m) if has_volume(&m) => Ok(m),
            Some(_) => Err(format!("控制 {name} 没有音量；可用：{}", names())),
            None => Err(format!("没有名为 {name} 的控制；可用：{}", names())),
        };
    }
    for std_name in ["Master", "PCM"] {
        if let Some(g) = candidates
            .iter()
            .find(|g| g.index == 0 && g.base == std_name)
        {
            return Ok(g.members.clone());
        }
    }
    let playback: Vec<&&Group<'_>> = candidates
        .iter()
        .filter(|g| g.dir == Dir::Playback)
        .collect();
    match playback.as_slice() {
        [g] => Ok(g.members.clone()),
        [] => Err("该声卡没有可用于播放的硬件音量控制".to_string()),
        _ => Err(format!(
            "该声卡有多个播放音量控制，不能确定用哪一个，请用 --volume-control 指定：{}",
            names()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::super::abi::{ELEM_ACCESS_TLV_READ, Zeroed};
    use super::super::tlv::DbInfo;
    use super::*;

    const RW: u32 = ELEM_ACCESS_READ | ELEM_ACCESS_WRITE;

    fn ctl(name: &str, kind: Kind, values: &[i64], db: Option<DbInfo>) -> Control {
        Control {
            id: ElemId::zeroed(),
            name: name.into(),
            index: 0,
            kind,
            access: RW | ELEM_ACCESS_TLV_READ,
            values: values.to_vec(),
            db,
        }
    }

    fn vol(min: i64, max: i64) -> Kind {
        Kind::Integer {
            min,
            max,
            step: 1,
            wide: false,
        }
    }

    /// 与用户机器上 HD-Audio 编解码器相同形式的一组控制。
    fn hda() -> Vec<Control> {
        let master_db = Some(DbInfo::Scale {
            min: -6525,
            step: 75,
            mute: false,
        });
        vec![
            ctl("Master Playback Volume", vol(0, 87), &[0, 0], master_db),
            ctl("Master Playback Switch", Kind::Boolean, &[0, 0], None),
            ctl("Mic Playback Volume", vol(0, 31), &[0, 0], None),
            ctl("Mic Playback Switch", Kind::Boolean, &[0, 0], None),
            ctl("Capture Volume", vol(0, 63), &[39, 39], None),
            ctl("Mic Capture Switch", Kind::Boolean, &[1, 1], None),
            ctl(
                "Auto-Mute Mode",
                Kind::Enumerated {
                    items: vec!["Disabled".into(), "Enabled".into()],
                },
                &[1],
                None,
            ),
        ]
    }

    #[test]
    fn names_split_like_simple_mixer() {
        assert_eq!(
            split_name("Master Playback Volume"),
            ("Master", Dir::Playback, Role::Volume)
        );
        assert_eq!(
            split_name("Mic Capture Switch"),
            ("Mic", Dir::Capture, Role::Switch)
        );
        assert_eq!(
            split_name("Master Volume"),
            ("Master", Dir::Unspecified, Role::Volume)
        );
        assert_eq!(
            split_name("Auto-Mute Mode"),
            ("Auto-Mute Mode", Dir::Unspecified, Role::Other)
        );
        // alsa-lib 的特例与 “-” 规则。
        assert_eq!(
            split_name("Capture Volume"),
            ("Capture", Dir::Capture, Role::Volume)
        );
        assert_eq!(
            split_name("Capture Switch"),
            ("Capture", Dir::Capture, Role::Switch)
        );
        assert_eq!(
            split_name("3D Control - Switch"),
            ("3D Control - Switch", Dir::Unspecified, Role::Other)
        );
        // 名称就是后缀本身时不拆成空基名。
        assert_eq!(
            split_name(" Volume"),
            (" Volume", Dir::Unspecified, Role::Other)
        );
    }

    #[test]
    fn groups_and_lines() {
        let c = hda();
        let g = groups(&c);
        let lines: Vec<String> = g.iter().map(Group::line).collect();
        assert_eq!(
            lines[0],
            "Master（播放）：音量 0（范围 0..87），0%，-65.25 dB；开关 关"
        );
        assert_eq!(lines[2], "Capture（录音）：音量 39（范围 0..63），62%");
        assert_eq!(lines[3], "Mic（录音）：开关 开");
        assert_eq!(
            lines[4],
            "Auto-Mute Mode：Enabled（可选：Disabled、Enabled）"
        );
        assert_eq!(g.len(), 5);
        // 声道不同时依次列出。
        let mut v = c[0].clone();
        v.values = vec![0, 87];
        assert_eq!(
            v.describe(),
            "音量 0 / 87（范围 0..87），0% / 100%，-65.25 dB / 0.00 dB"
        );
    }

    #[test]
    fn silent_playback_lists_off_and_minimum() {
        let c = hda();
        assert_eq!(
            silent_playback(&c),
            vec![
                "Master（0%，-65.25 dB；关）".to_string(),
                "Mic（0%；关）".to_string()
            ]
        );
        // 用户机器上的形式（2026-10-18）：最小档为 0 dB 的增益控制（话筒增强）不算最小音量；
        // 名为 Capture Volume / Capture Switch 的是录音方向，不列出。
        let mut more = c.clone();
        let boost_db = Some(DbInfo::Scale {
            min: 0,
            step: 1000,
            mute: false,
        });
        more.push(ctl("Mic Boost Volume", vol(0, 3), &[0, 0], boost_db));
        more.push(ctl("Capture Switch", Kind::Boolean, &[0, 0], None));
        more[4].values = vec![0, 0];
        assert_eq!(silent_playback(&more), silent_playback(&c));
        // 打开后不再列出；录音方向从不列出。
        let mut on = c.clone();
        on[0].values = vec![40, 40];
        on[1].values = vec![1, 1];
        on[2].values = vec![5, 5];
        on[3].values = vec![1, 1];
        assert!(silent_playback(&on).is_empty());
    }

    #[test]
    fn volume_percent_like_alsamixer() {
        let master = DbInfo::Scale {
            min: -6525,
            step: 75,
            mute: false,
        };
        // 65.25 dB > 24 dB：对数映射。min_norm = 10^(−65.25/60) = 0.08185。
        assert_eq!(volume_percent(0, 87, Some(&master), 0), 0);
        assert_eq!(volume_percent(0, 87, Some(&master), 87), 100);
        // 40%：0.4·(1 − 0.08185) + 0.08185 = 0.44911 → 60·log10 = −20.85 dB → 最接近的档 59（−21.00 dB）。
        assert_eq!(raw_for_percent(0, 87, 1, Some(&master), 40), 59);
        assert_eq!(volume_percent(0, 87, Some(&master), 59), 40);
        // 原来按原始值线性的 35 档（−39.00 dB）在映射下：(10^(−0.65) − 0.08185)/0.91815 = 0.155 → 15%。
        assert_eq!(volume_percent(0, 87, Some(&master), 35), 15);
        assert_eq!(raw_for_percent(0, 87, 1, Some(&master), 0), 0);
        assert_eq!(raw_for_percent(0, 87, 1, Some(&master), 100), 87);
        // dB 范围不超过 24 dB：按 dB 线性。0..=31，−34.5 dB 起每档 0.75 dB 是 23.25 dB。
        let small = DbInfo::Scale {
            min: -3450,
            step: 75,
            mute: false,
        };
        // 50%：−34.50 + 23.25/2 = −22.875 → 四舍五入 −22.87 dB；15 档 −23.25（差 0.38）、16 档 −22.50（差 0.37）→ 16。
        assert_eq!(raw_for_percent(0, 31, 1, Some(&small), 50), 16);
        // 没有 dB 信息：原始值线性。
        assert_eq!(volume_percent(0, 63, None, 39), 62);
        assert_eq!(raw_for_percent(0, 87, 1, None, 40), 35);
        assert_eq!(raw_for_percent(0, 100, 4, None, 37), 36);
        assert_eq!(raw_for_percent(0, 102, 4, None, 100), 100);
        // 最小档为静音：没有 min_norm 修正。
        let mute = DbInfo::Scale {
            min: -4500,
            step: 30,
            mute: true,
        };
        assert_eq!(volume_percent(-50, 100, Some(&mute), -50), 0);
        assert_eq!(raw_for_percent(-50, 100, 1, Some(&mute), 0), -50);
    }

    #[test]
    fn playback_volume_selection() {
        let c = hda();
        let m = playback_volume_group(&c, None).unwrap_or_default();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].name, "Master Playback Volume");
        // 指定控制名；录音方向与没有音量的控制不能用。
        let mic = playback_volume_group(&c, Some("Mic")).unwrap_or_default();
        assert_eq!(mic[0].name, "Mic Playback Volume");
        assert!(playback_volume_group(&c, Some("Auto-Mute Mode")).is_err());
        let e = playback_volume_group(&c, Some("Headphone"))
            .err()
            .unwrap_or_default();
        assert!(e.contains("可用：Master、Mic"), "{e}");
        // 只有 PCM：用 PCM。
        let usb = vec![
            ctl("PCM Playback Volume", vol(0, 127), &[100, 100], None),
            ctl("PCM Playback Switch", Kind::Boolean, &[1, 1], None),
        ];
        assert_eq!(playback_volume_group(&usb, None).map(|m| m.len()), Ok(2));
        // 没有标准名称：只有一组时用它，多组时报错。
        let one = vec![ctl("Speaker Playback Volume", vol(0, 31), &[1, 1], None)];
        assert!(playback_volume_group(&one, None).is_ok());
        let two = vec![
            ctl("Speaker Playback Volume", vol(0, 31), &[1, 1], None),
            ctl("Headphone Playback Volume", vol(0, 31), &[1, 1], None),
        ];
        let e = playback_volume_group(&two, None).err().unwrap_or_default();
        assert!(
            e.contains("--volume-control") && e.contains("Speaker、Headphone"),
            "{e}"
        );
        assert!(playback_volume_group(&[], None).is_err());
    }
}
