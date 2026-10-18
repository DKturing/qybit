//! 混音器控制的纯逻辑（A2）：按控制名分组、显示、设置值的解析与换算、播放前的静音提示。
//!
//! 只描述设备自身的硬件控制（内核驱动提供的混音器元素），不涉及送出的数据。命名规则与
//! alsa-lib 简单混音器（`src/mixer/simple_none.c`）相同：元素名以 ` Playback Volume`、
//! ` Playback Switch`、` Capture Volume`、` Capture Switch`、` Volume`、` Switch` 结尾时，去掉后缀
//! 得到基名，同一基名、序号与方向的元素合为一组；其余元素单独成组。百分比按原始值范围线性换算，
//! 与 amixer 相同。

use super::abi::{ELEM_ACCESS_INACTIVE, ELEM_ACCESS_READ, ELEM_ACCESS_WRITE, ElemId};
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
    const SUFFIXES: [(&str, Dir, Role); 6] = [
        (" Playback Volume", Dir::Playback, Role::Volume),
        (" Playback Switch", Dir::Playback, Role::Switch),
        (" Capture Volume", Dir::Capture, Role::Volume),
        (" Capture Switch", Dir::Capture, Role::Switch),
        (" Volume", Dir::Unspecified, Role::Volume),
        (" Switch", Dir::Unspecified, Role::Switch),
    ];
    for (suffix, dir, role) in SUFFIXES {
        if let Some(base) = name.strip_suffix(suffix)
            && !base.is_empty()
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
                    .map(|&v| format!("{}%", raw_to_percent(*min, *max, v)))
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
                let mute = |v: i64| {
                    v <= *min
                        || self
                            .db
                            .as_ref()
                            .is_some_and(|d| d.to_db(v, *min, *max) == Some(Db::Mute))
                };
                self.values.iter().all(|&v| mute(v)).then(|| {
                    let v = self.values[0];
                    let mut t = format!("{}%", raw_to_percent(*min, *max, v));
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

/// 百分比换算为原始值（四舍五入后按步长对齐）。
pub fn percent_to_raw(min: i64, max: i64, step: i64, pct: u32) -> i64 {
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

/// 增益不超过 `target`（0.01 dB）的最大档位；没有这样的档位或档位太多时为 `None`。
pub fn db_to_raw(info: &DbInfo, target: i64, min: i64, max: i64, step: i64) -> Option<i64> {
    let step = step.max(1);
    let n = (i128::from(max) - i128::from(min)) / i128::from(step);
    if !(0..=1 << 20).contains(&n) {
        return None;
    }
    let mut best = None;
    let mut raw = min;
    for _ in 0..=n {
        let ok = match info.to_db(raw, min, max) {
            Some(Db::Mute) => true,
            Some(Db::Centi(c)) => c <= target,
            None => false,
        };
        if ok {
            best = Some(raw);
        }
        raw = raw.saturating_add(step);
    }
    best
}

/// 命令行给出的一个设置值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Setting {
    On,
    Off,
    Percent(u32),
    /// 0.01 dB。
    Db(i64),
    Raw(i64),
    /// 枚举项名称。
    Item(String),
}

/// 解析设置值：`on`/`off`（`unmute`/`mute`）、`40%`、`-20dB`/`-20.5dB`、整数、枚举项名称。
pub fn parse_setting(s: &str) -> Result<Setting, String> {
    match s.to_ascii_lowercase().as_str() {
        "on" | "unmute" => return Ok(Setting::On),
        "off" | "mute" => return Ok(Setting::Off),
        _ => {}
    }
    if let Some(p) = s.strip_suffix('%') {
        return match p.parse::<u32>() {
            Ok(v) if v <= 100 => Ok(Setting::Percent(v)),
            _ => Err(format!("百分比 {s} 无效（应为 0%..100% 的整数）")),
        };
    }
    let lower = s.to_ascii_lowercase();
    if let Some(d) = lower.strip_suffix("db") {
        return parse_centi(d.trim())
            .map(Setting::Db)
            .ok_or_else(|| format!("dB 值 {s} 无效（如 -20dB、-20.5dB，至多两位小数）"));
    }
    if let Ok(v) = s.parse::<i64>() {
        return Ok(Setting::Raw(v));
    }
    Ok(Setting::Item(s.to_string()))
}

/// `-20.5` → −2050（0.01 dB）。
fn parse_centi(s: &str) -> Option<i64> {
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if int.is_empty() || frac.len() > 2 || !int.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let i: i64 = int.parse().ok()?;
    let f: i64 = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<2}").parse().ok()?
    };
    let v = i.checked_mul(100)?.checked_add(f)?;
    Some(if neg { -v } else { v })
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

/// 播放方向（含未写明方向）中处于关闭或最小音量的组，如 `Master（关；0%，-65.25 dB）`。
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

/// 按命令行的控制名找出要设置的元素：先按完整元素名，再按基名找播放方向的组，然后是未写明
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

/// 把一个设置值用到元素上，返回各声道的新值；值不适用于该元素时为 `Ok(None)`，适用但无效时为错误。
pub fn apply(c: &Control, s: &Setting) -> Result<Option<Vec<i64>>, String> {
    let n = c.values.len().max(1);
    let all = |v: i64| Some(vec![v; n]);
    match (&c.kind, s) {
        (Kind::Boolean, Setting::On) => Ok(all(1)),
        (Kind::Boolean, Setting::Off) => Ok(all(0)),
        (Kind::Boolean, Setting::Raw(v @ (0 | 1))) => Ok(all(*v)),
        (Kind::Integer { min, max, step, .. }, Setting::Percent(p)) => {
            Ok(all(percent_to_raw(*min, *max, *step, *p)))
        }
        (Kind::Integer { min, max, step, .. }, Setting::Raw(v)) => {
            if !(*min..=*max).contains(v) || (*step > 1 && (v - min) % step != 0) {
                return Err(format!(
                    "{}：原始值 {v} 不在范围 {min}..{max}{} 内",
                    c.name,
                    if *step > 1 {
                        format!("（步长 {step}）")
                    } else {
                        String::new()
                    }
                ));
            }
            Ok(all(*v))
        }
        (Kind::Integer { min, max, step, .. }, Setting::Db(t)) => match &c.db {
            Some(d) => db_to_raw(d, *t, *min, *max, *step).map(all).ok_or_else(|| {
                format!("{}：没有不超过 {} 的档位", c.name, format_db(Db::Centi(*t)))
            }),
            None => Err(format!(
                "{}：设备没有提供 dB 信息，请用百分比或原始值",
                c.name
            )),
        },
        (Kind::Enumerated { items }, Setting::Item(name)) => items
            .iter()
            .position(|i| i.eq_ignore_ascii_case(name))
            .map(|i| all(i as i64))
            .ok_or_else(|| format!("{}：没有选项 {name}（可选：{}）", c.name, items.join("、"))),
        (Kind::Enumerated { items }, Setting::Raw(v)) => {
            if usize::try_from(*v).is_ok_and(|i| i < items.len()) {
                Ok(all(*v))
            } else {
                Err(format!(
                    "{}：选项号 {v} 超出范围 0..{}",
                    c.name,
                    items.len()
                ))
            }
        }
        _ => Ok(None),
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
        assert_eq!(lines[2], "Capture：音量 39（范围 0..63），62%");
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
        // 打开后不再列出；录音方向从不列出。
        let mut on = c.clone();
        on[0].values = vec![40, 40];
        on[1].values = vec![1, 1];
        on[2].values = vec![5, 5];
        on[3].values = vec![1, 1];
        assert!(silent_playback(&on).is_empty());
    }

    #[test]
    fn percent_and_raw() {
        assert_eq!(raw_to_percent(0, 87, 0), 0);
        assert_eq!(raw_to_percent(0, 87, 87), 100);
        assert_eq!(raw_to_percent(0, 87, 35), 40); // 40.2%
        assert_eq!(percent_to_raw(0, 87, 1, 40), 35); // 34.8 → 35
        assert_eq!(percent_to_raw(-50, 100, 1, 0), -50);
        assert_eq!(percent_to_raw(-50, 100, 1, 100), 100);
        // 步长 4：0..=100，37% → 37 → 36。
        assert_eq!(percent_to_raw(0, 100, 4, 37), 36);
        assert_eq!(percent_to_raw(0, 102, 4, 100), 100);
        assert_eq!(raw_to_percent(5, 5, 5), 0);
    }

    #[test]
    fn db_target_picks_largest_not_above() {
        let d = DbInfo::Scale {
            min: -6525,
            step: 75,
            mute: false,
        };
        // −20 dB：−65.25 + 0.75·k ≤ −20 → k ≤ 60.33 → 60（−20.25 dB）。
        assert_eq!(db_to_raw(&d, -2000, 0, 87, 1), Some(60));
        assert_eq!(db_to_raw(&d, 0, 0, 87, 1), Some(87));
        assert_eq!(db_to_raw(&d, 600, 0, 87, 1), Some(87));
        assert_eq!(db_to_raw(&d, -7000, 0, 87, 1), None);
        let m = DbInfo::Scale {
            min: -4500,
            step: 30,
            mute: true,
        };
        assert_eq!(db_to_raw(&m, -9000, -50, 100, 1), Some(-50));
    }

    #[test]
    fn settings_parse() {
        assert_eq!(parse_setting("on"), Ok(Setting::On));
        assert_eq!(parse_setting("MUTE"), Ok(Setting::Off));
        assert_eq!(parse_setting("40%"), Ok(Setting::Percent(40)));
        assert!(parse_setting("140%").is_err());
        assert_eq!(parse_setting("-20dB"), Ok(Setting::Db(-2000)));
        assert_eq!(parse_setting("-20.5dB"), Ok(Setting::Db(-2050)));
        assert_eq!(parse_setting("+1.25dB"), Ok(Setting::Db(125)));
        assert_eq!(parse_setting("0dB"), Ok(Setting::Db(0)));
        assert!(parse_setting("-1.234dB").is_err());
        assert!(parse_setting("xdB").is_err());
        assert_eq!(parse_setting("35"), Ok(Setting::Raw(35)));
        assert_eq!(
            parse_setting("Enabled"),
            Ok(Setting::Item("Enabled".into()))
        );
    }

    #[test]
    fn resolve_and_apply() {
        let c = hda();
        let m = resolve(&c, "master", 0).unwrap_or_default();
        assert_eq!(m.len(), 2);
        assert_eq!(apply(m[0], &Setting::Percent(40)), Ok(Some(vec![35, 35])));
        assert_eq!(apply(m[0], &Setting::On), Ok(None));
        assert_eq!(apply(m[1], &Setting::On), Ok(Some(vec![1, 1])));
        assert_eq!(apply(m[0], &Setting::Db(-2000)), Ok(Some(vec![60, 60])));
        assert!(apply(m[0], &Setting::Raw(88)).is_err());
        // 没有 dB 信息时 dB 设置报错。
        let mic = resolve(&c, "Mic", 0).unwrap_or_default();
        assert_eq!(mic[0].name, "Mic Playback Volume");
        assert!(apply(mic[0], &Setting::Db(-100)).is_err());
        // 录音方向在播放方向之后匹配；完整元素名优先。
        let cap = resolve(&c, "Mic Capture Switch", 0).unwrap_or_default();
        assert_eq!(cap.len(), 1);
        let am = resolve(&c, "auto-mute mode", 0).unwrap_or_default();
        assert_eq!(
            apply(am[0], &Setting::Item("disabled".into())),
            Ok(Some(vec![0]))
        );
        assert!(apply(am[0], &Setting::Item("Maybe".into())).is_err());
        assert!(resolve(&c, "Headphone", 0).is_none());
        assert!(resolve(&c, "Master", 1).is_none());
    }
}
