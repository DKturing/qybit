//! 声卡硬件混音器（A2 查看、A4 播放期间的音量）：经控制设备列出与读取混音器元素；播放时按
//! 用户给出的百分比临时设置设备自身的硬件音量并打开其静音开关，播放结束时恢复原值。
//!
//! 只调设备自身的硬件控制（编解码器放大器、静音开关、USB 功能单元等），送到设备的数据不变，
//! 软件增益仍恒为 1。出处：内核 `sound/core/control.c`（`snd_ctl_elem_list` / `info` / `read` /
//! `write`、`snd_ctl_tlv_ioctl`）。分组、换算、选择与提示规则见 [`crate::kernel::mixer`]。

use qy_pcm_format::alsa::check_device_name;

use super::ctl::{Ctl, resolve_card};
use crate::kernel::abi::{
    ELEM_ACCESS_READ, ELEM_ACCESS_TLV_READ, ELEM_IFACE_MIXER, ELEM_TYPE_BOOLEAN,
    ELEM_TYPE_ENUMERATED, ELEM_TYPE_INTEGER, ELEM_TYPE_INTEGER64, ElemValue, Zeroed, c_text,
};
use crate::kernel::mixer::{
    Control, Kind, Role, groups, playback_volume_group, raw_for_percent, silent_playback,
    split_name,
};
use crate::kernel::tlv::parse_db;

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

/// 播放前的提示：该卡播放方向中处于关闭或最小音量的控制。没有或无法读取时为 `None`。
pub fn playback_hint(device: &str) -> Option<String> {
    let m = open(device).ok()?;
    let silent = silent_playback(&m.controls().ok()?);
    (!silent.is_empty()).then(|| {
        format!(
            "提示：{}的混音器中，以下播放控制处于关闭或最小音量：{}。设备可能因此不出声\
             （其中有的本来就该关闭，如话筒监听）。用 qybit mixer --device {device} 查看；\
             播放音量用 --volume 与 --volume-control 指定。",
            m.header(),
            silent.join("、")
        )
    })
}

/// 播放期间的设备硬件音量（A4）：创建时设置音量并打开同组的静音开关，[`restore`](Self::restore)
/// 或析构时恢复原值。进程被强制结束（如第二次 Ctrl+C、SIGKILL）时来不及恢复。
pub struct PlaybackVolume {
    mixer: CardMixer,
    /// 改动过的元素与原值。
    saved: Vec<(Control, Vec<i64>)>,
}

impl PlaybackVolume {
    /// 设置 `device` 所在声卡的播放音量为 `percent`（与 alsamixer 相同的百分比），`control` 为
    /// 用户指定的控制名（缺省按标准名称选择，见 [`playback_volume_group`]）。返回守卫与一行说明。
    pub fn apply(
        device: &str,
        percent: u32,
        control: Option<&str>,
    ) -> Result<(Self, String), String> {
        let mixer = open(device)?;
        let controls = mixer.controls()?;
        let group = playback_volume_group(&controls, control)
            .map_err(|e| format!("{}：{e}", mixer.header()))?;
        let mut plan: Vec<(&Control, Vec<i64>)> = Vec::new();
        for c in &group {
            if !c.writable() || c.values.is_empty() {
                continue;
            }
            match (&c.kind, split_name(&c.name).2) {
                (Kind::Integer { min, max, step, .. }, Role::Volume) => {
                    let raw = raw_for_percent(*min, *max, *step, c.db.as_ref(), percent);
                    plan.push((c, vec![raw; c.values.len()]));
                }
                (Kind::Boolean, Role::Switch) => plan.push((c, vec![1; c.values.len()])),
                _ => {}
            }
        }
        if !plan
            .iter()
            .any(|(c, _)| matches!(c.kind, Kind::Integer { .. }))
        {
            return Err(format!("{}：所选控制没有可写的音量", mixer.header()));
        }
        let before: Vec<String> = plan.iter().map(|(c, _)| c.describe()).collect();
        let mut guard = PlaybackVolume {
            mixer,
            saved: Vec::new(),
        };
        for (c, v) in &plan {
            if c.values != *v {
                guard.mixer.write(c, v)?;
                guard.saved.push(((*c).clone(), c.values.clone()));
            }
        }
        let after = guard.mixer.controls()?;
        let name = split_name(&group[0].name).0.to_string();
        let now: Vec<String> = plan
            .iter()
            .filter_map(|(c, _)| after.iter().find(|a| a.id == c.id).map(Control::describe))
            .collect();
        let line = format!(
            "设备硬件音量：{} {name}：{}（原为 {}；播放结束后恢复）。只调设备自身的硬件音量，送出的数据不变",
            guard.mixer.header(),
            now.join("；"),
            before.join("；")
        );
        Ok((guard, line))
    }

    /// 恢复原值，返回一行说明。
    pub fn restore(mut self) -> Result<String, String> {
        self.restore_inner()
    }

    fn restore_inner(&mut self) -> Result<String, String> {
        let saved = std::mem::take(&mut self.saved);
        if saved.is_empty() {
            return Ok("设备硬件音量：播放前已是该值，无需恢复".into());
        }
        let mut errors = Vec::new();
        for (c, v) in saved.iter().rev() {
            if let Err(e) = self.mixer.write(c, v) {
                errors.push(e);
            }
        }
        let after = self.mixer.controls()?;
        for (c, v) in &saved {
            if !after.iter().any(|a| a.id == c.id && a.values == *v) {
                errors.push(format!("{} 读回与原值不符", c.name));
            }
        }
        if errors.is_empty() {
            let names: Vec<&str> = saved.iter().map(|(c, _)| c.name.as_str()).collect();
            Ok(format!("设备硬件音量已恢复：{}", names.join("、")))
        } else {
            Err(format!("设备硬件音量恢复不完整：{}", errors.join("；")))
        }
    }
}

impl Drop for PlaybackVolume {
    fn drop(&mut self) {
        let _ = self.restore_inner();
    }
}
