//! 混音器状态文件（A3）：序列化、解析与恢复计划，纯函数。
//!
//! 格式（UTF-8 文本，字段用制表符分隔，下例中写作 ⇥；元素名放在最后，可含空格）：
//!
//! ```text
//! # qybit mixer state v1
//! card ⇥ Generic_1 ⇥ HD-Audio Generic
//! ctl ⇥ 0 ⇥ integer ⇥ 35 35 ⇥ Master Playback Volume
//! ctl ⇥ 0 ⇥ boolean ⇥ 1 1 ⇥ Master Playback Switch
//! ctl ⇥ 0 ⇥ enum ⇥ 1 ⇥ Auto-Mute Mode
//! ```
//!
//! 值为各声道的原始值；开关为 0/1，枚举为项号。`#` 开头的行与空行忽略。文件是不可信输入：
//! 解析只返回错误，不 panic，并限制行数、行长与值的个数。

use super::mixer::{Control, Kind, Setting, apply};

/// 第一行（版本）。
pub const HEADER: &str = "# qybit mixer state v1";
const MAX_LINES: usize = 4096;
const MAX_LINE: usize = 4096;
/// 每个元素的值个数上限（与 `snd_ctl_elem_value` 的 `long value[128]` 相同）。
const MAX_VALUES: usize = 128;

/// 元素类型标记。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Boolean,
    Integer,
    Integer64,
    Enumerated,
}

impl Tag {
    fn text(self) -> &'static str {
        match self {
            Tag::Boolean => "boolean",
            Tag::Integer => "integer",
            Tag::Integer64 => "integer64",
            Tag::Enumerated => "enum",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "boolean" => Tag::Boolean,
            "integer" => Tag::Integer,
            "integer64" => Tag::Integer64,
            "enum" => Tag::Enumerated,
            _ => return None,
        })
    }

    /// 元素的类型标记；其他类型（BYTES 等）为 `None`，不保存。
    pub fn of(c: &Control) -> Option<Self> {
        match c.kind {
            Kind::Boolean => Some(Tag::Boolean),
            Kind::Integer { wide: false, .. } => Some(Tag::Integer),
            Kind::Integer { wide: true, .. } => Some(Tag::Integer64),
            Kind::Enumerated { .. } => Some(Tag::Enumerated),
            Kind::Other(_) => None,
        }
    }
}

/// 文件中的一个元素。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub index: u32,
    pub tag: Tag,
    pub values: Vec<i64>,
}

/// 解析后的状态文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub card_id: String,
    pub card_name: String,
    pub entries: Vec<Entry>,
}

/// 把元素名或卡名放进一个字段：不允许制表符与换行。
fn field_ok(s: &str) -> bool {
    !s.is_empty() && !s.contains(['\t', '\n', '\r'])
}

/// 序列化：可读且可写、类型受支持的元素。返回（文件内容, 未保存的元素及原因）。
pub fn serialize(card_id: &str, card_name: &str, controls: &[Control]) -> (String, Vec<String>) {
    let mut out = format!("{HEADER}\n");
    let name = card_name.replace(['\t', '\n', '\r'], " ");
    out.push_str(&format!("card\t{card_id}\t{name}\n"));
    let mut skipped = Vec::new();
    for c in controls {
        if !c.writable() {
            continue; // 只读元素（如电平表）不属于可恢复的状态
        }
        let Some(tag) = Tag::of(c) else {
            skipped.push(format!("{}（类型不支持）", c.name));
            continue;
        };
        if !c.readable() || c.values.is_empty() {
            skipped.push(format!("{}（读不到当前值）", c.name));
            continue;
        }
        if !field_ok(&c.name) {
            skipped.push(format!("{:?}（名称含制表符或换行）", c.name));
            continue;
        }
        let values: Vec<String> = c.values.iter().map(|v| v.to_string()).collect();
        out.push_str(&format!(
            "ctl\t{}\t{}\t{}\t{}\n",
            c.index,
            tag.text(),
            values.join(" "),
            c.name
        ));
    }
    (out, skipped)
}

/// 解析状态文件。
pub fn parse(text: &str) -> Result<State, String> {
    let mut lines = text.lines().enumerate();
    match lines.next() {
        Some((_, l)) if l.trim_end() == HEADER => {}
        _ => {
            return Err(format!(
                "不是 qybit 混音器状态文件（第一行应为 {HEADER:?}）"
            ));
        }
    }
    let mut card: Option<(String, String)> = None;
    let mut entries: Vec<Entry> = Vec::new();
    for (no, line) in lines {
        let n = no + 1;
        if n > MAX_LINES {
            return Err(format!("超过 {MAX_LINES} 行"));
        }
        if line.len() > MAX_LINE {
            return Err(format!("第 {n} 行超过 {MAX_LINE} 字节"));
        }
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = |what: &str| format!("第 {n} 行：{what}");
        let mut f = line.splitn(5, '\t');
        match f.next() {
            Some("card") => {
                let id = f
                    .next()
                    .filter(|s| field_ok(s))
                    .ok_or_else(|| bad("缺少声卡 id"))?;
                let rest: Vec<&str> = f.collect();
                if card.is_some() {
                    return Err(bad("重复的 card 行"));
                }
                card = Some((id.to_string(), rest.join("\t")));
            }
            Some("ctl") => {
                let index = f
                    .next()
                    .and_then(|s| s.parse::<u32>().ok())
                    .ok_or_else(|| bad("序号无效"))?;
                let tag = f
                    .next()
                    .and_then(Tag::parse)
                    .ok_or_else(|| bad("类型无效（应为 boolean、integer、integer64、enum）"))?;
                let vals = f.next().ok_or_else(|| bad("缺少值"))?;
                let values = vals
                    .split(' ')
                    .map(|v| v.parse::<i64>().ok())
                    .collect::<Option<Vec<_>>>()
                    .filter(|v| !v.is_empty() && v.len() <= MAX_VALUES)
                    .ok_or_else(|| {
                        bad(&format!("值无效（1..={MAX_VALUES} 个整数，用空格分隔）"))
                    })?;
                let name = f
                    .next()
                    .filter(|s| field_ok(s))
                    .ok_or_else(|| bad("缺少元素名"))?;
                if entries.iter().any(|e| e.name == name && e.index == index) {
                    return Err(bad(&format!("元素 {name},{index} 重复")));
                }
                entries.push(Entry {
                    name: name.to_string(),
                    index,
                    tag,
                    values,
                });
            }
            Some(other) => return Err(bad(&format!("未知的记录类型 {other:?}"))),
            None => {}
        }
    }
    let (card_id, card_name) = card.ok_or("缺少 card 行")?;
    Ok(State {
        card_id,
        card_name,
        entries,
    })
}

/// 恢复计划：要写入的元素与新值、跳过的元素及原因、值已相同的个数。
#[derive(Debug)]
pub struct Plan<'a> {
    pub writes: Vec<(&'a Control, Vec<i64>)>,
    pub skipped: Vec<String>,
    pub unchanged: usize,
}

/// 按“元素名 + 序号”把文件中的值对应到当前元素，逐项核对类型、声道数与取值范围。
pub fn plan_restore<'a>(state: &State, controls: &'a [Control]) -> Plan<'a> {
    let mut plan = Plan {
        writes: Vec::new(),
        skipped: Vec::new(),
        unchanged: 0,
    };
    for e in &state.entries {
        let label = if e.index == 0 {
            e.name.clone()
        } else {
            format!("{},{}", e.name, e.index)
        };
        let Some(c) = controls
            .iter()
            .find(|c| c.name == e.name && c.index == e.index)
        else {
            plan.skipped.push(format!("{label}（设备上没有这个控制）"));
            continue;
        };
        if Tag::of(c) != Some(e.tag) {
            plan.skipped.push(format!("{label}（类型不同）"));
            continue;
        }
        if !c.writable() {
            plan.skipped.push(format!("{label}（只读）"));
            continue;
        }
        if c.values.len() != e.values.len() {
            plan.skipped.push(format!(
                "{label}（声道数不同：文件 {}，设备 {}）",
                e.values.len(),
                c.values.len()
            ));
            continue;
        }
        if let Some(bad) = e
            .values
            .iter()
            .find(|&&v| !matches!(apply(c, &Setting::Raw(v)), Ok(Some(_))))
        {
            plan.skipped
                .push(format!("{label}（值 {bad} 超出设备的取值范围）"));
            continue;
        }
        if c.values == e.values {
            plan.unchanged += 1;
        } else {
            plan.writes.push((c, e.values.clone()));
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::super::abi::{ELEM_ACCESS_READ, ELEM_ACCESS_WRITE, ElemId, Zeroed};
    use super::*;

    const RW: u32 = ELEM_ACCESS_READ | ELEM_ACCESS_WRITE;

    fn ctl(name: &str, kind: Kind, values: &[i64], access: u32) -> Control {
        Control {
            id: ElemId::zeroed(),
            name: name.into(),
            index: 0,
            kind,
            access,
            values: values.to_vec(),
            db: None,
        }
    }

    fn vol() -> Kind {
        Kind::Integer {
            min: 0,
            max: 87,
            step: 1,
            wide: false,
        }
    }

    fn card() -> Vec<Control> {
        vec![
            ctl("Master Playback Volume", vol(), &[35, 35], RW),
            ctl("Master Playback Switch", Kind::Boolean, &[1, 1], RW),
            ctl(
                "Auto-Mute Mode",
                Kind::Enumerated {
                    items: vec!["Disabled".into(), "Enabled".into()],
                },
                &[1],
                RW,
            ),
            ctl("Peak Meter", vol(), &[3, 3], ELEM_ACCESS_READ),
            ctl("IEC958 Playback Default", Kind::Other(5), &[], RW),
        ]
    }

    #[test]
    fn serialize_and_parse_round_trip() {
        let (text, skipped) = serialize("Generic_1", "HD-Audio Generic", &card());
        assert_eq!(
            text,
            "# qybit mixer state v1\ncard\tGeneric_1\tHD-Audio Generic\n\
             ctl\t0\tinteger\t35 35\tMaster Playback Volume\n\
             ctl\t0\tboolean\t1 1\tMaster Playback Switch\n\
             ctl\t0\tenum\t1\tAuto-Mute Mode\n"
        );
        // 只读的电平表不保存也不列出；不支持的类型列出。
        assert_eq!(
            skipped,
            vec!["IEC958 Playback Default（类型不支持）".to_string()]
        );
        let s = parse(&text);
        let Ok(s) = s else { panic!("{s:?}") };
        assert_eq!(
            (s.card_id.as_str(), s.card_name.as_str()),
            ("Generic_1", "HD-Audio Generic")
        );
        assert_eq!(s.entries.len(), 3);
        assert_eq!(
            s.entries[0],
            Entry {
                name: "Master Playback Volume".into(),
                index: 0,
                tag: Tag::Integer,
                values: vec![35, 35]
            }
        );
        // 相同状态：没有要写的，全部为未改变。
        let c = card();
        let p = plan_restore(&s, &c);
        assert!(p.writes.is_empty() && p.skipped.is_empty());
        assert_eq!(p.unchanged, 3);
    }

    #[test]
    fn restore_plan_checks_each_entry() {
        let mut now = card();
        now[0].values = vec![0, 0];
        now[1].values = vec![0, 0];
        let text = "# qybit mixer state v1\r\ncard\tGeneric_1\tX\n# 注释\n\n\
                    ctl\t0\tinteger\t35 35\tMaster Playback Volume\n\
                    ctl\t0\tboolean\t1 1\tMaster Playback Switch\n\
                    ctl\t0\tinteger\t1\tAuto-Mute Mode\n\
                    ctl\t0\tinteger\t88 88\tPeak Meter\n\
                    ctl\t0\tboolean\t1\tMaster Playback Switch,dup\n\
                    ctl\t1\tinteger\t35 35\tMaster Playback Volume\n";
        let Ok(s) = parse(text) else { panic!("parse") };
        let p = plan_restore(&s, &now);
        let w: Vec<(&str, Vec<i64>)> = p
            .writes
            .iter()
            .map(|(c, v)| (c.name.as_str(), v.clone()))
            .collect();
        assert_eq!(
            w,
            vec![
                ("Master Playback Volume", vec![35, 35]),
                ("Master Playback Switch", vec![1, 1])
            ]
        );
        assert_eq!(
            p.skipped,
            vec![
                "Auto-Mute Mode（类型不同）".to_string(),
                "Peak Meter（只读）".to_string(),
                "Master Playback Switch,dup（设备上没有这个控制）".to_string(),
                "Master Playback Volume,1（设备上没有这个控制）".to_string(),
            ]
        );
        // 声道数与范围。
        let t = "# qybit mixer state v1\ncard\tG\t\nctl\t0\tinteger\t35\tMaster Playback Volume\n\
                 ctl\t0\tboolean\t2 2\tMaster Playback Switch\nctl\t0\tenum\t2\tAuto-Mute Mode\n";
        let Ok(s) = parse(t) else { panic!("parse") };
        let p = plan_restore(&s, &now);
        assert!(p.writes.is_empty());
        assert_eq!(
            p.skipped,
            vec![
                "Master Playback Volume（声道数不同：文件 1，设备 2）".to_string(),
                "Master Playback Switch（值 2 超出设备的取值范围）".to_string(),
                "Auto-Mute Mode（值 2 超出设备的取值范围）".to_string(),
            ]
        );
    }

    #[test]
    fn malformed_files_are_errors() {
        let h = "# qybit mixer state v1\n";
        for (text, what) in [
            ("", "空文件"),
            ("card\tX\n", "没有版本行"),
            ("# qybit mixer state v2\ncard\tX\n", "其他版本"),
            (h, "缺少 card 行"),
            (&format!("{h}card\tA\ncard\tB\n"), "重复 card"),
            (&format!("{h}card\tA\nctl\tx\tinteger\t1\tM\n"), "序号"),
            (&format!("{h}card\tA\nctl\t0\tfloat\t1\tM\n"), "类型"),
            (&format!("{h}card\tA\nctl\t0\tinteger\t1 a\tM\n"), "非数字"),
            (&format!("{h}card\tA\nctl\t0\tinteger\t\tM\n"), "没有值"),
            (&format!("{h}card\tA\nctl\t0\tinteger\t1\n"), "缺少名称"),
            (
                &format!("{h}card\tA\nctl\t0\tinteger\t1\tM\nctl\t0\tinteger\t2\tM\n"),
                "重复元素",
            ),
            (&format!("{h}card\tA\nmixer\t0\n"), "未知记录"),
            (
                &format!(
                    "{h}card\tA\nctl\t0\tinteger\t{}\tM\n",
                    vec!["1"; 129].join(" ")
                ),
                "值太多",
            ),
            (
                &format!("{h}card\tA\nctl\t0\tinteger\t99999999999999999999\tM\n"),
                "溢出",
            ),
            (&format!("{h}card\tA\n{}\n", "#".repeat(5000)), "行太长"),
        ] {
            assert!(parse(text).is_err(), "{what} 应当报错");
        }
        let many = format!("{h}card\tA\n{}", "#\n".repeat(5000));
        assert!(parse(&many).is_err(), "行太多");
    }

    #[test]
    fn names_with_tabs_are_not_saved() {
        let c = vec![ctl("Bad\tName", vol(), &[1, 1], RW)];
        let (text, skipped) = serialize("A", "B\tC", &c);
        assert_eq!(text, "# qybit mixer state v1\ncard\tA\tB C\n");
        assert_eq!(
            skipped,
            vec!["\"Bad\\tName\"（名称含制表符或换行）".to_string()]
        );
    }
}
