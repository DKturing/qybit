//! 声卡硬件混音器（A2，`qybit mixer`）：经控制设备列出、读取与设置混音器元素；A3 起可把
//! 状态保存到文件并恢复（格式见 [`crate::kernel::mixer_state`]）。
//!
//! 只调设备自身的硬件控制（编解码器放大器、静音开关、USB 功能单元等），送到设备的数据不变，
//! 软件增益仍恒为 1；只在用户明确运行命令时改动。出处：内核 `sound/core/control.c`
//! （`snd_ctl_elem_list` / `info` / `read` / `write`、`snd_ctl_tlv_ioctl`）。分组、换算与提示规则
//! 见 [`crate::kernel::mixer`]。

use qy_pcm_format::alsa::check_device_name;

use super::ctl::{Ctl, resolve_card};
use crate::kernel::abi::{
    ELEM_ACCESS_READ, ELEM_ACCESS_TLV_READ, ELEM_IFACE_MIXER, ELEM_TYPE_BOOLEAN,
    ELEM_TYPE_ENUMERATED, ELEM_TYPE_INTEGER, ELEM_TYPE_INTEGER64, ElemValue, Zeroed, c_text,
};
use crate::kernel::mixer::{Control, Kind, apply, groups, parse_setting, resolve, silent_playback};
use crate::kernel::mixer_state::{parse as parse_state, plan_restore, serialize};
use crate::kernel::tlv::parse_db;
use std::path::{Path, PathBuf};

/// 值数组的声道上限（`long value[128]`；`long long` 为 64）。
const MAX_VALUES: usize = 128;
const MAX_VALUES_64: usize = 64;
/// 枚举项数上限（只用于显示，超出部分不列出）。
const MAX_ITEMS: u32 = 128;

/// 一块声卡的混音器。
pub struct CardMixer {
    ctl: Ctl,
    /// 声卡 id（`hw:CARD=<id>`）与名称。
    pub id: String,
    pub name: String,
}

/// 按 `hw:` 设备名打开该卡的控制设备（设备号与子设备号不用）。
pub fn open(device: &str) -> Result<CardMixer, String> {
    let hw = check_device_name(device).map_err(|e| format!("设备名 {device:?} 被拒绝：{e}"))?;
    let card = resolve_card(hw.card).map_err(|e| format!("找不到声卡：{e}"))?;
    let ctl = Ctl::open(card).map_err(|e| format!("打开声卡 {card} 的控制设备失败：{e}"))?;
    let info = ctl.card_info().map_err(|e| e.to_string())?;
    Ok(CardMixer {
        ctl,
        id: c_text(&info.id),
        name: c_text(&info.name),
    })
}

impl CardMixer {
    /// 全部混音器元素（接口 MIXER）及其当前值。读不到的单个元素跳过。
    pub fn controls(&self) -> Result<Vec<Control>, String> {
        let ids = self.ctl.elem_ids().map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for id in ids.iter().filter(|id| id.iface == ELEM_IFACE_MIXER) {
            let Ok(info) = self.ctl.elem_info(id) else {
                continue;
            };
            let kind = match info.elem_type {
                ELEM_TYPE_BOOLEAN => Kind::Boolean,
                ELEM_TYPE_INTEGER => match info.integer_range() {
                    Some((min, max, step)) => Kind::Integer {
                        min,
                        max,
                        step,
                        wide: false,
                    },
                    None => Kind::Other(info.elem_type),
                },
                ELEM_TYPE_INTEGER64 => {
                    let (min, max, step) = info.integer64_range();
                    Kind::Integer {
                        min,
                        max,
                        step,
                        wide: true,
                    }
                }
                ELEM_TYPE_ENUMERATED => Kind::Enumerated {
                    items: (0..info.enum_items().min(MAX_ITEMS))
                        .map(|i| {
                            self.ctl
                                .enum_item_name(&info.id, i)
                                .unwrap_or_else(|_| format!("#{i}"))
                        })
                        .collect(),
                },
                t => Kind::Other(t),
            };
            let values = if info.access & ELEM_ACCESS_READ != 0 {
                self.ctl
                    .elem_read(&info.id)
                    .map(|v| read_values(&v, &kind, info.count as usize))
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let db = if info.access & ELEM_ACCESS_TLV_READ != 0
                && matches!(kind, Kind::Integer { .. })
            {
                self.ctl.elem_tlv(info.id.numid).and_then(|t| parse_db(&t))
            } else {
                None
            };
            out.push(Control {
                id: info.id,
                name: c_text(&info.id.name),
                index: info.id.index,
                kind,
                access: info.access,
                values,
                db,
            });
        }
        Ok(out)
    }

    /// 写一个元素的各声道值。
    pub fn write(&self, c: &Control, values: &[i64]) -> Result<(), String> {
        if !c.writable() {
            return Err(format!("{} 是只读控制", c.name));
        }
        let mut v = ElemValue::zeroed();
        v.id = c.id;
        for (i, &x) in values.iter().enumerate() {
            let ok = match c.kind {
                Kind::Boolean => v.set_integer(i, x),
                Kind::Integer { wide: false, .. } => v.set_integer(i, x),
                Kind::Integer { wide: true, .. } => v.set_integer64(i, x),
                Kind::Enumerated { .. } => u32::try_from(x).is_ok_and(|x| v.set_enumerated(i, x)),
                Kind::Other(_) => false,
            };
            if !ok {
                return Err(format!("{}：值 {x} 无法写入", c.name));
            }
        }
        self.ctl
            .elem_write(&mut v)
            .map_err(|e| format!("设置 {} 失败：{e}", c.name))
    }

    fn header(&self) -> String {
        format!("声卡 {}（{}）", self.id, self.name)
    }
}

fn read_values(v: &ElemValue, kind: &Kind, count: usize) -> Vec<i64> {
    let n = match kind {
        Kind::Integer { wide: true, .. } => count.min(MAX_VALUES_64),
        _ => count.min(MAX_VALUES),
    };
    (0..n)
        .filter_map(|i| match kind {
            Kind::Boolean | Kind::Integer { wide: false, .. } => v.integer(i),
            Kind::Integer { wide: true, .. } => v.integer64(i),
            Kind::Enumerated { .. } => v.enumerated(i).map(i64::from),
            Kind::Other(_) => None,
        })
        .collect()
}

/// `qybit mixer --device <设备>`：列出混音器控制，每组一行。
pub fn show(device: &str) -> Result<Vec<String>, String> {
    let m = open(device)?;
    let controls = m.controls()?;
    let mut out = vec![format!("{}的混音器控制：", m.header())];
    if controls.is_empty() {
        out.push("  （该声卡没有混音器控制）".into());
    }
    out.extend(groups(&controls).iter().map(|g| format!("  {}", g.line())));
    Ok(out)
}

/// `名称` 或 `名称,序号`。
fn split_index(arg: &str) -> (&str, u32) {
    match arg.rsplit_once(',') {
        Some((name, idx)) if !idx.is_empty() && idx.bytes().all(|b| b.is_ascii_digit()) => {
            (name, idx.parse().unwrap_or(0))
        }
        _ => (arg, 0),
    }
}

/// `qybit mixer --device <设备> <控制> <值>...`：设置后读回，返回该控制的新状态。
pub fn set(device: &str, control: &str, values: &[String]) -> Result<Vec<String>, String> {
    if values.is_empty() {
        return Err("需要至少一个值：on、off、40%、-20dB、原始值或枚举项名称".into());
    }
    let settings = values
        .iter()
        .map(|v| parse_setting(v))
        .collect::<Result<Vec<_>, _>>()?;
    let m = open(device)?;
    let controls = m.controls()?;
    let (name, index) = split_index(control);
    let Some(targets) = resolve(&controls, name, index) else {
        let mut names: Vec<String> = Vec::new();
        for g in groups(&controls) {
            if !names.iter().any(|n| n == g.base) {
                names.push(g.base.to_string());
            }
        }
        return Err(format!(
            "{}没有名为 {control} 的控制；可用：{}",
            m.header(),
            if names.is_empty() {
                "无".into()
            } else {
                names.join("、")
            }
        ));
    };
    // 每个值用到第一个接受它的元素上；后面的值覆盖前面的。
    let mut plan: Vec<(&Control, Vec<i64>)> = Vec::new();
    for (s, raw) in settings.iter().zip(values) {
        let mut used = false;
        for c in &targets {
            if let Some(v) = apply(c, s)? {
                match plan.iter_mut().find(|(p, _)| p.id == c.id) {
                    Some(p) => p.1 = v,
                    None => plan.push((c, v)),
                }
                used = true;
                break;
            }
        }
        if !used {
            let kinds: Vec<&str> = targets.iter().map(|c| c.name.as_str()).collect();
            return Err(format!(
                "值 {raw} 不适用于 {control}（该控制包含：{}）",
                kinds.join("、")
            ));
        }
    }
    for (c, v) in &plan {
        m.write(c, v)?;
    }
    let after = m.controls()?;
    let changed: Vec<String> = groups(&after)
        .iter()
        .filter(|g| {
            g.members
                .iter()
                .any(|c| plan.iter().any(|(p, _)| p.id == c.id))
        })
        .map(|g| format!("  {}", g.line()))
        .collect();
    let mut out = vec![format!("{}已设置（读回）：", m.header())];
    out.extend(changed);
    Ok(out)
}

/// 播放前的提示：该卡播放方向中处于关闭或最小音量的控制。没有或无法读取时为 `None`。
pub fn playback_hint(device: &str) -> Option<String> {
    let m = open(device).ok()?;
    let silent = silent_playback(&m.controls().ok()?);
    (!silent.is_empty()).then(|| {
        format!(
            "提示：{}的混音器中，以下播放控制处于关闭或最小音量：{}。设备可能因此不出声\
             （其中有的本来就该关闭，如话筒监听）。用 qybit mixer --device {device} 查看，\
             用 qybit mixer --device {device} <控制> 40% on 打开（没有静音开关的控制只给音量）；这只调设备自身的硬件音量，\
             送出的数据不变。",
            m.header(),
            silent.join("、")
        )
    })
}

/// 状态文件大小上限（解析另有行数与行长限制）。
const MAX_STATE_BYTES: u64 = 1 << 20;

/// 缺省的状态文件：`$XDG_CONFIG_HOME/qybit/mixer-<声卡 id>.txt`，没有时为
/// `$HOME/.config/qybit/…`；都没有为 `None`（须用 `--file`）。
pub fn default_state_path(card_id: &str) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("qybit").join(format!("mixer-{card_id}.txt")))
}

fn state_path(file: Option<&Path>, card_id: &str) -> Result<PathBuf, String> {
    match file {
        Some(f) => Ok(f.to_path_buf()),
        None => default_state_path(card_id)
            .ok_or_else(|| "没有 HOME 或 XDG_CONFIG_HOME，请用 --file 指定状态文件".to_string()),
    }
}

/// `qybit mixer --device <设备> --save [--file <路径>]`。
pub fn save(device: &str, file: Option<&Path>) -> Result<Vec<String>, String> {
    let m = open(device)?;
    let controls = m.controls()?;
    let path = state_path(file, &m.id)?;
    let (text, skipped) = serialize(&m.id, &m.name, &controls);
    let saved = text.lines().filter(|l| l.starts_with("ctl\t")).count();
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("创建目录 {} 失败：{e}", dir.display()))?;
    }
    // 先写临时文件再改名：不会留下写了一半的状态文件。
    let mut tmp = path.clone().into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text).map_err(|e| format!("写入 {} 失败：{e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("改名为 {} 失败：{e}", path.display()))?;
    let mut out = vec![format!(
        "{}：已保存 {saved} 个控制到 {}",
        m.header(),
        path.display()
    )];
    if !skipped.is_empty() {
        out.push(format!("未保存：{}", skipped.join("、")));
    }
    Ok(out)
}

/// `qybit mixer --device <设备> --restore [--file <路径>]`：写回后读回核对。
pub fn restore(device: &str, file: Option<&Path>) -> Result<Vec<String>, String> {
    let m = open(device)?;
    let path = state_path(file, &m.id)?;
    let meta =
        std::fs::metadata(&path).map_err(|e| format!("读取 {} 失败：{e}", path.display()))?;
    if meta.len() > MAX_STATE_BYTES {
        return Err(format!(
            "{} 超过 {MAX_STATE_BYTES} 字节，不是混音器状态文件",
            path.display()
        ));
    }
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("读取 {} 失败：{e}", path.display()))?;
    let state = parse_state(&text).map_err(|e| format!("{}：{e}", path.display()))?;
    if state.card_id != m.id {
        return Err(format!(
            "{} 保存的是声卡 {}（{}）的状态，与设备 {device} 的声卡 {} 不同，没有恢复",
            path.display(),
            state.card_id,
            state.card_name,
            m.id
        ));
    }
    let controls = m.controls()?;
    let plan = plan_restore(&state, &controls);
    let mut errors = Vec::new();
    for (c, v) in &plan.writes {
        if let Err(e) = m.write(c, v) {
            errors.push(e);
        }
    }
    // 读回核对：驱动可能拒绝或调整写入的值。
    let after = m.controls()?;
    for (c, v) in &plan.writes {
        match after.iter().find(|a| a.id == c.id) {
            Some(a) if a.values == *v => {}
            Some(a) => errors.push(format!("{}：写入 {v:?}，读回 {:?}", c.name, a.values)),
            None => errors.push(format!("{}：恢复后读不到", c.name)),
        }
    }
    let mut out = vec![format!(
        "{}：从 {} 恢复 {} 个控制（另有 {} 个已相同）",
        m.header(),
        path.display(),
        plan.writes.len(),
        plan.unchanged
    )];
    for (c, _) in &plan.writes {
        if let Some(a) = after.iter().find(|a| a.id == c.id) {
            out.push(format!("  {}：{}", a.name, a.describe()));
        }
    }
    if !plan.skipped.is_empty() {
        out.push(format!("跳过：{}", plan.skipped.join("、")));
    }
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(format!(
            "{}\n恢复不完整：{}",
            out.join("\n"),
            errors.join("；")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_index_suffix() {
        assert_eq!(split_index("Master"), ("Master", 0));
        assert_eq!(split_index("Headphone,1"), ("Headphone", 1));
        assert_eq!(split_index("A,B"), ("A,B", 0));
        assert_eq!(split_index("X,"), ("X,", 0));
    }
}
