//! 内核 ALSA 控制接口（`/dev/snd/controlC{卡}`，A1）：声卡信息、PCM 设备枚举、子设备偏好与
//! 声道映射元素。
//!
//! 出处：内核 `include/uapi/sound/asound.h`（控制协议 2.0.9）、`sound/core/control.c`、
//! `sound/core/pcm.c`（`snd_pcm_control_ioctl`）与 `sound/core/pcm_lib.c`
//! （`snd_pcm_add_chmap_ctls`：元素名 “Playback Channel Map” / “Capture Channel Map”，
//! 接口 PCM，`device` 为 PCM 设备号，`index` 为子设备号）。与 alsa-lib `pcm_hw.c`
//! （`fill_chmap_ctl_id`、`snd_pcm_hw_query_chmaps` / `get_chmap` / `set_chmap`）用同一个元素。

use std::fs::File;
use std::path::PathBuf;

use qy_pcm_format::alsa::{CHMAP_POSITION_MASK, Card, ChmapType};

use super::sys;
use crate::kernel::abi::{
    CTL_IOCTL_CARD_INFO, CTL_IOCTL_ELEM_INFO, CTL_IOCTL_ELEM_READ, CTL_IOCTL_ELEM_WRITE,
    CTL_IOCTL_PCM_INFO, CTL_IOCTL_PCM_NEXT_DEVICE, CTL_IOCTL_PCM_PREFER_SUBDEVICE,
    CTL_IOCTL_PVERSION, CTL_IOCTL_TLV_READ, CardInfo, ELEM_IFACE_PCM, ElemId, ElemInfo, ElemValue,
    PcmInfo, STREAM_PLAYBACK, Zeroed, c_text, protocol_major, set_c_text,
};
use crate::kernel::tlv::parse_chmaps;
use crate::pcm::{PcmError, errno};

/// 设备节点目录。
pub const SND_DIR: &str = "/dev/snd";

/// 声道映射 TLV 的读取缓冲（字数，含 2 字头）；驱动的映射表远小于此（alsa-lib 用 256 字）。
const TLV_WORDS: usize = 1024;

/// 已有控制设备的声卡号（`/dev/snd/controlC*`），从小到大。
pub fn cards() -> Vec<u32> {
    let mut out: Vec<u32> = std::fs::read_dir(SND_DIR)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let n = name.to_str()?.strip_prefix("controlC")?;
            if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            n.parse().ok()
        })
        .collect();
    out.sort_unstable();
    out
}

/// 一个打开的控制设备。
pub struct Ctl {
    file: File,
}

impl Ctl {
    /// 打开 `/dev/snd/controlC{card}` 并核对控制接口主版本。
    pub fn open(card: u32) -> Result<Self, PcmError> {
        let path = PathBuf::from(format!("{SND_DIR}/controlC{card}"));
        let file = sys::open_rw(&path, true).map_err(|e| PcmError {
            call: "open /dev/snd/controlC*",
            errno: e,
        })?;
        let mut v = 0i32;
        sys::ioctl(&file, CTL_IOCTL_PVERSION, &mut v).map_err(|e| PcmError {
            call: "SNDRV_CTL_IOCTL_PVERSION",
            errno: e,
        })?;
        if protocol_major(v as u32) != 2 {
            return Err(PcmError {
                call: "SNDRV_CTL_IOCTL_PVERSION（控制接口主版本不是 2）",
                errno: errno::EINVAL,
            });
        }
        Ok(Self { file })
    }

    pub fn card_info(&self) -> Result<CardInfo, PcmError> {
        let mut info = CardInfo::zeroed();
        sys::ioctl(&self.file, CTL_IOCTL_CARD_INFO, &mut info).map_err(|e| PcmError {
            call: "SNDRV_CTL_IOCTL_CARD_INFO",
            errno: e,
        })?;
        Ok(info)
    }

    /// 号码大于 `after` 的下一个 PCM 设备（`after = -1` 从头开始）；没有为 `None`。
    pub fn next_pcm_device(&self, after: i32) -> Result<Option<u32>, PcmError> {
        let mut d = after;
        sys::ioctl(&self.file, CTL_IOCTL_PCM_NEXT_DEVICE, &mut d).map_err(|e| PcmError {
            call: "SNDRV_CTL_IOCTL_PCM_NEXT_DEVICE",
            errno: e,
        })?;
        Ok(u32::try_from(d).ok())
    }

    /// 某设备、子设备、方向的 PCM 信息；该方向不存在时为错误（内核返回 `ENOENT` 等）。
    pub fn pcm_info(&self, device: u32, subdevice: u32, stream: i32) -> Result<PcmInfo, i32> {
        let mut info = PcmInfo::zeroed();
        info.device = device;
        info.subdevice = subdevice;
        info.stream = stream;
        sys::ioctl(&self.file, CTL_IOCTL_PCM_INFO, &mut info)?;
        Ok(info)
    }

    /// 本进程下一次打开该卡 PCM 时优先选用的子设备（`-1` 为不指定）。只在本控制设备打开期间
    /// 有效（内核按打开它的进程记录）。
    pub fn prefer_subdevice(&self, subdevice: i32) -> Result<(), PcmError> {
        let mut s = subdevice;
        sys::ioctl(&self.file, CTL_IOCTL_PCM_PREFER_SUBDEVICE, &mut s)
            .map(|_| ())
            .map_err(|e| PcmError {
                call: "SNDRV_CTL_IOCTL_PCM_PREFER_SUBDEVICE",
                errno: e,
            })
    }

    /// 声卡 id（`hw:CARD=<id>` 中的名称）。
    pub fn card_id(&self) -> Result<String, PcmError> {
        Ok(c_text(&self.card_info()?.id))
    }

    /// 设备支持的声道映射（TLV）；元素不存在、读取失败或无法解析时为空（与 alsa-lib 相同：
    /// 不支持即不列出）。
    pub fn query_chmaps(
        &self,
        device: u32,
        subdevice: u32,
        stream: i32,
    ) -> Vec<(ChmapType, Vec<u32>)> {
        let Some(id) = chmap_id(device, subdevice, stream) else {
            return Vec::new();
        };
        let mut info = ElemInfo::zeroed();
        info.id = id;
        if sys::ioctl(&self.file, CTL_IOCTL_ELEM_INFO, &mut info).is_err() || info.id.numid == 0 {
            return Vec::new();
        }
        let mut buf = vec![0u32; TLV_WORDS];
        buf[0] = info.id.numid;
        if sys::ioctl_tlv(&self.file, CTL_IOCTL_TLV_READ, &mut buf).is_err() {
            return Vec::new();
        }
        // 头之后的字：内核写入的 TLV（长度由 TLV 自身的第二个字给出，解析时核对）。
        parse_chmaps(&buf[2..])
    }

    /// 读当前声道映射的前 `channels` 个位置值；读不到或值不在位置范围内为 `None`。
    pub fn read_chmap(
        &self,
        device: u32,
        subdevice: u32,
        stream: i32,
        channels: u32,
    ) -> Option<Vec<u32>> {
        let mut v = ElemValue::zeroed();
        v.id = chmap_id(device, subdevice, stream)?;
        sys::ioctl(&self.file, CTL_IOCTL_ELEM_READ, &mut v).ok()?;
        (0..channels as usize)
            .map(|i| {
                v.integer(i)
                    .and_then(|x| u32::try_from(x).ok())
                    .filter(|&p| p & !CHMAP_POSITION_MASK == 0)
            })
            .collect()
    }

    /// 写声道映射（在 hw_params 之后）。
    pub fn write_chmap(
        &self,
        device: u32,
        subdevice: u32,
        stream: i32,
        positions: &[u32],
    ) -> Result<(), PcmError> {
        let bad = PcmError {
            call: "SNDRV_CTL_IOCTL_ELEM_WRITE（Channel Map）",
            errno: errno::EINVAL,
        };
        let mut v = ElemValue::zeroed();
        v.id = chmap_id(device, subdevice, stream).ok_or(bad)?;
        for (i, &p) in positions.iter().enumerate() {
            if p & !CHMAP_POSITION_MASK != 0 || !v.set_integer(i, i64::from(p)) {
                return Err(bad);
            }
        }
        sys::ioctl(&self.file, CTL_IOCTL_ELEM_WRITE, &mut v)
            .map(|_| ())
            .map_err(|e| PcmError {
                call: "SNDRV_CTL_IOCTL_ELEM_WRITE（Channel Map）",
                errno: e,
            })
    }
}

/// 声道映射元素的标识。
fn chmap_id(device: u32, subdevice: u32, stream: i32) -> Option<ElemId> {
    let mut id = ElemId::zeroed();
    id.iface = ELEM_IFACE_PCM;
    id.device = device;
    id.index = subdevice;
    let name = if stream == STREAM_PLAYBACK {
        "Playback Channel Map"
    } else {
        "Capture Channel Map"
    };
    set_c_text(&mut id.name, name).then_some(id)
}

/// 把设备名中的声卡解析为卡号：数字直接用；名称按各卡的 id 查找，找不到为 `ENODEV`
/// （与 alsa-lib `snd_card_get_index` 相同）。
pub fn resolve_card(card: Card<'_>) -> Result<u32, PcmError> {
    match card {
        Card::Index(i) => Ok(i),
        Card::Name(name) => {
            // 打不开的卡跳过，继续找；都找不到时，若有卡打不开（如无权限）报告该错误，
            // 因为要找的卡可能就是它。
            let mut unopened = None;
            for c in cards() {
                match Ctl::open(c) {
                    Ok(ctl) if ctl.card_id().is_ok_and(|id| id == name) => return Ok(c),
                    Ok(_) => {}
                    Err(e) => unopened = Some(e),
                }
            }
            Err(unopened.unwrap_or(PcmError {
                call: "查找声卡 id",
                errno: errno::ENODEV,
            }))
        }
    }
}
