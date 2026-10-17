//! Linux 内核 ALSA 用户空间接口的结构体与 ioctl 编号（A1）。
//!
//! 出处：Linux `include/uapi/sound/asound.h`（PCM 协议 2.0.17、控制协议 2.0.9）与
//! `include/uapi/sound/tlv.h`。结构体按 `#[repr(C)]` 逐字段转写，`unsigned long` 写作
//! `usize`、`long` 写作 `isize`（Linux 上二者与指针同宽）。大小与偏移在编译期按指针宽度核对，
//! 测试另按从头文件编译得到的参考值逐项核对（64 位与 armv7）。
//!
//! 时间结构：32 位架构上 `struct snd_pcm_status` 含 `struct timespec`，内核 5.6 起另有 64 位
//! 时间的版本。这里按 32 位时间的旧布局定义（`long` 秒与纳秒），对应的 ioctl 编号所有内核都
//! 接受；QyBit 不读这些时间戳。
//!
//! 本模块不含 `unsafe`，与平台无关，可在任何系统上编译与测试；真正的系统调用在
//! `crate::linux::sys`。

use core::mem::size_of;

// ioctl 编号按 asm-generic 编码（x86、ARM、AArch64、LoongArch、RISC-V 都用它；MIPS、PowerPC、
// SPARC 等不同，未核对，不编译）。所有已核对的架构都是小端。
#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "arm",
    target_arch = "loongarch64",
    target_arch = "riscv64"
)))]
compile_error!("内核 ALSA 接口只在 x86_64、aarch64、arm、loongarch64、riscv64 上核对过");

#[cfg(not(target_endian = "little"))]
compile_error!("内核 ALSA 接口的字段转换只为小端架构核对过");

/// `_IOC(dir, type, nr, size)`（asm-generic/ioctl.h：nr 8 位、type 8 位、size 14 位、dir 2 位）。
pub const fn ioc(dir: u32, ty: u8, nr: u8, size: usize) -> u32 {
    (dir << 30) | ((size as u32) << 16) | ((ty as u32) << 8) | nr as u32
}
const IOC_NONE: u32 = 0;
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const fn io(ty: u8, nr: u8) -> u32 {
    ioc(IOC_NONE, ty, nr, 0)
}
const fn ior(ty: u8, nr: u8, size: usize) -> u32 {
    ioc(IOC_READ, ty, nr, size)
}
const fn iow(ty: u8, nr: u8, size: usize) -> u32 {
    ioc(IOC_WRITE, ty, nr, size)
}
const fn iowr(ty: u8, nr: u8, size: usize) -> u32 {
    ioc(IOC_READ | IOC_WRITE, ty, nr, size)
}

/// `SNDRV_PROTOCOL_VERSION(major, minor, subminor)`。
pub const fn protocol(major: u32, minor: u32, micro: u32) -> u32 {
    (major << 16) | (minor << 8) | micro
}
/// 本模块转写的 PCM 接口版本。
pub const PCM_VERSION: u32 = protocol(2, 0, 17);
/// 本模块转写的控制接口版本。
pub const CTL_VERSION: u32 = protocol(2, 0, 9);
/// 主版本号（不兼容时不同）。
pub const fn protocol_major(v: u32) -> u32 {
    (v >> 16) & 0xffff
}

// ---- PCM 参数 ----

/// `struct snd_mask`（`SNDRV_MASK_MAX` = 256 位）。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mask {
    pub bits: [u32; 8],
}

/// `struct snd_interval`：位域 `openmin:1, openmax:1, integer:1, empty:1` 在小端上依次为第 0–3 位。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interval {
    pub min: u32,
    pub max: u32,
    pub flags: u32,
}

pub const INTERVAL_OPENMIN: u32 = 1 << 0;
pub const INTERVAL_OPENMAX: u32 = 1 << 1;
pub const INTERVAL_INTEGER: u32 = 1 << 2;
pub const INTERVAL_EMPTY: u32 = 1 << 3;

/// 掩码参数（`SNDRV_PCM_HW_PARAM_ACCESS` … `SUBFORMAT`）。
pub const MASK_ACCESS: usize = 0;
pub const MASK_FORMAT: usize = 1;
pub const MASK_SUBFORMAT: usize = 2;
/// 区间参数在 `intervals` 中的位置（`SNDRV_PCM_HW_PARAM_SAMPLE_BITS` = 8 起）。
pub const SAMPLE_BITS: usize = 0;
pub const FRAME_BITS: usize = 1;
pub const CHANNELS: usize = 2;
pub const RATE: usize = 3;
pub const PERIOD_TIME: usize = 4;
pub const PERIOD_SIZE: usize = 5;
pub const PERIOD_BYTES: usize = 6;
pub const PERIODS: usize = 7;
pub const BUFFER_TIME: usize = 8;
pub const BUFFER_SIZE: usize = 9;
pub const BUFFER_BYTES: usize = 10;
pub const TICK_TIME: usize = 11;

/// `SNDRV_PCM_ACCESS_*`。
pub const ACCESS_MMAP_INTERLEAVED: u32 = 0;
pub const ACCESS_RW_INTERLEAVED: u32 = 3;
/// `SNDRV_PCM_HW_PARAMS_NORESAMPLE`：不重采样（hw 设备本来就不重采样；与 alsa-lib 的
/// `set_rate_resample(0)` 相同的标志）。
pub const HW_PARAMS_NORESAMPLE: u32 = 1 << 0;

/// `struct snd_pcm_hw_params`。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HwParams {
    pub flags: u32,
    pub masks: [Mask; 3],
    pub mres: [Mask; 5],
    pub intervals: [Interval; 12],
    pub ires: [Interval; 9],
    pub rmask: u32,
    pub cmask: u32,
    pub info: u32,
    pub msbits: u32,
    pub rate_num: u32,
    pub rate_den: u32,
    pub fifo_size: usize,
    pub reserved: [u8; 64],
}

/// `struct snd_pcm_sw_params`。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwParams {
    pub tstamp_mode: i32,
    pub period_step: u32,
    pub sleep_min: u32,
    pub avail_min: usize,
    pub xfer_align: usize,
    pub start_threshold: usize,
    pub stop_threshold: usize,
    pub silence_threshold: usize,
    pub silence_size: usize,
    pub boundary: usize,
    pub proto: u32,
    pub tstamp_type: u32,
    pub reserved: [u8; 56],
}

/// 32 位时间的 `struct timespec`（`long` 秒与纳秒），见模块说明。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Timespec {
    pub sec: isize,
    pub nsec: isize,
}

/// `struct snd_pcm_status`（旧时间布局）。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub state: i32,
    pub pad1: [u8; size_of::<isize>() - size_of::<i32>()],
    pub trigger_tstamp: Timespec,
    pub tstamp: Timespec,
    pub appl_ptr: usize,
    pub hw_ptr: usize,
    pub delay: isize,
    pub avail: usize,
    pub avail_max: usize,
    pub overrange: usize,
    pub suspended_state: i32,
    pub audio_tstamp_data: u32,
    pub audio_tstamp: Timespec,
    pub driver_tstamp: Timespec,
    pub audio_tstamp_accuracy: u32,
    pub reserved: [u8; 52 - 2 * size_of::<Timespec>()],
}

/// `SNDRV_PCM_STATE_*`。
pub const STATE_OPEN: i32 = 0;
pub const STATE_SETUP: i32 = 1;
pub const STATE_PREPARED: i32 = 2;
pub const STATE_RUNNING: i32 = 3;
pub const STATE_XRUN: i32 = 4;
pub const STATE_DRAINING: i32 = 5;
pub const STATE_PAUSED: i32 = 6;
pub const STATE_SUSPENDED: i32 = 7;
pub const STATE_DISCONNECTED: i32 = 8;

/// `struct snd_xferi`（缓冲指针由调用方在系统调用边界填写）。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Xferi {
    pub result: isize,
    pub buf: usize,
    pub frames: usize,
}

/// `struct snd_pcm_info`（`union snd_pcm_sync_id` 为 16 字节）。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmInfo {
    pub device: u32,
    pub subdevice: u32,
    pub stream: i32,
    pub card: i32,
    pub id: [u8; 64],
    pub name: [u8; 80],
    pub subname: [u8; 32],
    pub dev_class: i32,
    pub dev_subclass: i32,
    pub subdevices_count: u32,
    pub subdevices_avail: u32,
    pub sync: [u32; 4],
    pub reserved: [u8; 64],
}

/// `SNDRV_PCM_STREAM_PLAYBACK` / `CAPTURE`。
pub const STREAM_PLAYBACK: i32 = 0;
pub const STREAM_CAPTURE: i32 = 1;

// ---- 控制接口 ----

/// `struct snd_ctl_card_info`。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CardInfo {
    pub card: i32,
    pub pad: i32,
    pub id: [u8; 16],
    pub driver: [u8; 16],
    pub name: [u8; 32],
    pub longname: [u8; 80],
    pub reserved_: [u8; 16],
    pub mixername: [u8; 80],
    pub components: [u8; 128],
}

/// `SNDRV_CTL_ELEM_ID_NAME_MAXLEN`。
pub const ELEM_ID_NAME_MAXLEN: usize = 44;

/// `struct snd_ctl_elem_id`。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElemId {
    pub numid: u32,
    pub iface: i32,
    pub device: u32,
    pub subdevice: u32,
    pub name: [u8; ELEM_ID_NAME_MAXLEN],
    pub index: u32,
}

/// `SNDRV_CTL_ELEM_IFACE_PCM`。
pub const ELEM_IFACE_PCM: i32 = 3;
/// `SNDRV_CTL_ELEM_TYPE_INTEGER`。
pub const ELEM_TYPE_INTEGER: i32 = 2;

/// 值联合（`long value[128]` / `long long value[64]` / `unsigned char data[512]` …）的存储：
/// 大小为 128 个 `long`，按 8 字节对齐（联合含 `long long`）。按 `long` 读写见
/// [`ElemValue::integer`]。
pub const ELEM_VALUE_WORDS: usize = 128 * size_of::<isize>() / 8;

/// `struct snd_ctl_elem_value`（位域 `indirect:1` 占一个 `unsigned int`）。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElemValue {
    pub id: ElemId,
    pub indirect: u32,
    pub value: [u64; ELEM_VALUE_WORDS],
    pub reserved: [u8; 128],
}

impl ElemValue {
    /// 第 `i` 个 `long`（`value.integer.value[i]`）；越界为 `None`。
    pub fn integer(&self, i: usize) -> Option<i64> {
        if size_of::<isize>() == 8 {
            self.value.get(i).map(|&w| w as i64)
        } else {
            let w = *self.value.get(i / 2)?;
            let half = if i.is_multiple_of(2) {
                w as u32
            } else {
                (w >> 32) as u32
            };
            Some(i64::from(half as i32))
        }
    }

    /// 设定第 `i` 个 `long`；越界或超出 `long` 的范围时返回 `false`。
    pub fn set_integer(&mut self, i: usize, v: i64) -> bool {
        if size_of::<isize>() == 8 {
            match self.value.get_mut(i) {
                Some(w) => {
                    *w = v as u64;
                    true
                }
                None => false,
            }
        } else {
            let Ok(v) = i32::try_from(v) else {
                return false;
            };
            let Some(w) = self.value.get_mut(i / 2) else {
                return false;
            };
            let half = u64::from(v as u32);
            *w = if i.is_multiple_of(2) {
                (*w & !0xffff_ffff) | half
            } else {
                (*w & 0xffff_ffff) | (half << 32)
            };
            true
        }
    }
}

/// `struct snd_ctl_elem_info`（值联合 128 字节，按 8 字节对齐）。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElemInfo {
    pub id: ElemId,
    pub elem_type: i32,
    pub access: u32,
    pub count: u32,
    pub owner: i32,
    pub value: [u64; 16],
    pub reserved: [u8; 64],
}

/// `struct snd_ctl_tlv` 的头（其后为 `length` 字节的 TLV 数据）。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TlvHeader {
    pub numid: u32,
    pub length: u32,
}

/// TLV 类型（`sound/tlv.h`）。
pub const TLVT_CONTAINER: u32 = 0;
pub const TLVT_CHMAP_FIXED: u32 = 0x101;
pub const TLVT_CHMAP_VAR: u32 = 0x102;
pub const TLVT_CHMAP_PAIRED: u32 = 0x103;
/// `SNDRV_CHMAP_POSITION_MASK`：位置值的有效位，之上为标志（反相、驱动自定义）。
pub const CHMAP_POSITION_MASK: u32 = 0xffff;

// ---- ioctl 编号 ----

pub const PCM_IOCTL_PVERSION: u32 = ior(b'A', 0x00, size_of::<i32>());
pub const PCM_IOCTL_INFO: u32 = ior(b'A', 0x01, size_of::<PcmInfo>());
pub const PCM_IOCTL_USER_PVERSION: u32 = iow(b'A', 0x04, size_of::<i32>());
pub const PCM_IOCTL_HW_REFINE: u32 = iowr(b'A', 0x10, size_of::<HwParams>());
pub const PCM_IOCTL_HW_PARAMS: u32 = iowr(b'A', 0x11, size_of::<HwParams>());
pub const PCM_IOCTL_HW_FREE: u32 = io(b'A', 0x12);
pub const PCM_IOCTL_SW_PARAMS: u32 = iowr(b'A', 0x13, size_of::<SwParams>());
pub const PCM_IOCTL_STATUS: u32 = ior(b'A', 0x20, size_of::<Status>());
pub const PCM_IOCTL_DELAY: u32 = ior(b'A', 0x21, size_of::<isize>());
pub const PCM_IOCTL_PREPARE: u32 = io(b'A', 0x40);
pub const PCM_IOCTL_START: u32 = io(b'A', 0x42);
pub const PCM_IOCTL_DROP: u32 = io(b'A', 0x43);
pub const PCM_IOCTL_DRAIN: u32 = io(b'A', 0x44);
pub const PCM_IOCTL_WRITEI_FRAMES: u32 = iow(b'A', 0x50, size_of::<Xferi>());
pub const PCM_IOCTL_READI_FRAMES: u32 = ior(b'A', 0x51, size_of::<Xferi>());

pub const CTL_IOCTL_PVERSION: u32 = ior(b'U', 0x00, size_of::<i32>());
pub const CTL_IOCTL_CARD_INFO: u32 = ior(b'U', 0x01, size_of::<CardInfo>());
pub const CTL_IOCTL_ELEM_INFO: u32 = iowr(b'U', 0x11, size_of::<ElemInfo>());
pub const CTL_IOCTL_ELEM_READ: u32 = iowr(b'U', 0x12, size_of::<ElemValue>());
pub const CTL_IOCTL_ELEM_WRITE: u32 = iowr(b'U', 0x13, size_of::<ElemValue>());
pub const CTL_IOCTL_TLV_READ: u32 = iowr(b'U', 0x1a, size_of::<TlvHeader>());
pub const CTL_IOCTL_PCM_NEXT_DEVICE: u32 = ior(b'U', 0x30, size_of::<i32>());
pub const CTL_IOCTL_PCM_INFO: u32 = iowr(b'U', 0x31, size_of::<PcmInfo>());
pub const CTL_IOCTL_PCM_PREFER_SUBDEVICE: u32 = iow(b'U', 0x32, size_of::<i32>());

// 编译期核对：与头文件在该指针宽度下的大小一致（逐项偏移见测试）。
const W64: bool = size_of::<usize>() == 8;
const _: () = assert!(size_of::<HwParams>() == if W64 { 608 } else { 604 });
const _: () = assert!(size_of::<SwParams>() == if W64 { 136 } else { 104 });
const _: () = assert!(size_of::<Status>() == if W64 { 152 } else { 108 });
const _: () = assert!(size_of::<Xferi>() == if W64 { 24 } else { 12 });
const _: () = assert!(size_of::<PcmInfo>() == 288);
const _: () = assert!(size_of::<CardInfo>() == 376);
const _: () = assert!(size_of::<ElemId>() == 64);
const _: () = assert!(size_of::<ElemValue>() == if W64 { 1224 } else { 712 });
const _: () = assert!(size_of::<ElemInfo>() == 272);
const _: () = assert!(size_of::<TlvHeader>() == 8);

/// 全零的值（所有结构体的字段都是整数或整数数组，全零是合法值）。
pub trait Zeroed: Sized {
    fn zeroed() -> Self;
}

macro_rules! zeroed {
    ($($t:ty => $e:expr),* $(,)?) => {
        $(impl Zeroed for $t {
            fn zeroed() -> Self {
                $e
            }
        })*
    };
}

const ZERO_ID: ElemId = ElemId {
    numid: 0,
    iface: 0,
    device: 0,
    subdevice: 0,
    name: [0; ELEM_ID_NAME_MAXLEN],
    index: 0,
};
const ZERO_INTERVAL: Interval = Interval {
    min: 0,
    max: 0,
    flags: 0,
};

zeroed! {
    HwParams => HwParams {
        flags: 0,
        masks: [Mask { bits: [0; 8] }; 3],
        mres: [Mask { bits: [0; 8] }; 5],
        intervals: [ZERO_INTERVAL; 12],
        ires: [ZERO_INTERVAL; 9],
        rmask: 0,
        cmask: 0,
        info: 0,
        msbits: 0,
        rate_num: 0,
        rate_den: 0,
        fifo_size: 0,
        reserved: [0; 64],
    },
    SwParams => SwParams {
        tstamp_mode: 0,
        period_step: 0,
        sleep_min: 0,
        avail_min: 0,
        xfer_align: 0,
        start_threshold: 0,
        stop_threshold: 0,
        silence_threshold: 0,
        silence_size: 0,
        boundary: 0,
        proto: 0,
        tstamp_type: 0,
        reserved: [0; 56],
    },
    Status => Status {
        state: 0,
        pad1: [0; size_of::<isize>() - size_of::<i32>()],
        trigger_tstamp: Timespec { sec: 0, nsec: 0 },
        tstamp: Timespec { sec: 0, nsec: 0 },
        appl_ptr: 0,
        hw_ptr: 0,
        delay: 0,
        avail: 0,
        avail_max: 0,
        overrange: 0,
        suspended_state: 0,
        audio_tstamp_data: 0,
        audio_tstamp: Timespec { sec: 0, nsec: 0 },
        driver_tstamp: Timespec { sec: 0, nsec: 0 },
        audio_tstamp_accuracy: 0,
        reserved: [0; 52 - 2 * size_of::<Timespec>()],
    },
    PcmInfo => PcmInfo {
        device: 0,
        subdevice: 0,
        stream: 0,
        card: 0,
        id: [0; 64],
        name: [0; 80],
        subname: [0; 32],
        dev_class: 0,
        dev_subclass: 0,
        subdevices_count: 0,
        subdevices_avail: 0,
        sync: [0; 4],
        reserved: [0; 64],
    },
    CardInfo => CardInfo {
        card: 0,
        pad: 0,
        id: [0; 16],
        driver: [0; 16],
        name: [0; 32],
        longname: [0; 80],
        reserved_: [0; 16],
        mixername: [0; 80],
        components: [0; 128],
    },
    ElemId => ZERO_ID,
    ElemValue => ElemValue {
        id: ZERO_ID,
        indirect: 0,
        value: [0; ELEM_VALUE_WORDS],
        reserved: [0; 128],
    },
    ElemInfo => ElemInfo {
        id: ZERO_ID,
        elem_type: 0,
        access: 0,
        count: 0,
        owner: 0,
        value: [0; 16],
        reserved: [0; 64],
    },
}

/// 以 0 结尾的定长字节数组中的文字（不是 UTF-8 的字节按替换字符显示）。
pub fn c_text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// 把文字写入定长字节数组（以 0 结尾）；放不下时返回 `false`。
pub fn set_c_text(dst: &mut [u8], s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() >= dst.len() {
        return false;
    }
    dst.fill(0);
    dst[..b.len()].copy_from_slice(b);
    true
}

#[cfg(test)]
mod tests {
    use core::mem::offset_of;

    use super::*;

    /// 由 `include/uapi/sound/asound.h` 与 `sound/tlv.h` 编译得到的参考值（clang 按各目标的
    /// ABI 编译、读出常量；x86_64 另用 gcc 与 glibc 头文件编译运行，结果相同）。64 位一栏对
    /// x86_64、aarch64、riscv64、loongarch64 都相同；32 位一栏为 armv7（32 位时间）。
    /// 顺序与 [`computed`] 相同。
    #[cfg(target_pointer_width = "64")]
    const REFERENCE: [u64; 75] = [
        608, 4, 260, 512, 516, 520, 524, 528, 532, 536, 136, 16, 32, 40, 64, 72, 152, 8, 40, 64,
        88, 128, 24, 288, 16, 80, 160, 208, 376, 8, 40, 72, 64, 16, 60, 1224, 72, 1096, 272, 64,
        72, 76, 80, 8, 2147762432, 2166374657, 3261088016, 3261088017, 16658, 3230155027,
        2157461792, 2148024609, 16704, 16706, 16707, 16708, 1075331408, 2149073233, 2147767552,
        2172146945, 3239073041, 3301463314, 3301463315, 3221771546, 2147767600, 3240121649, 131089,
        131081, 0, 257, 258, 259, 65535, 2, 44,
    ];
    #[cfg(target_pointer_width = "32")]
    const REFERENCE: [u64; 75] = [
        604, 4, 260, 512, 516, 520, 524, 528, 532, 536, 104, 12, 20, 24, 36, 40, 108, 4, 20, 32,
        44, 68, 12, 288, 16, 80, 160, 208, 376, 8, 40, 72, 64, 16, 60, 712, 72, 584, 272, 64, 72,
        76, 80, 8, 2147762432, 2166374657, 3260825872, 3260825873, 16658, 3228057875, 2154578208,
        2147762465, 16704, 16706, 16707, 16708, 1074544976, 2148286801, 2147767552, 2172146945,
        3239073041, 3267908882, 3267908883, 3221771546, 2147767600, 3240121649, 131089, 131081, 0,
        257, 258, 259, 65535, 2, 44,
    ];

    fn computed() -> [u64; 75] {
        let s = |v: usize| v as u64;
        let r = |v: u32| u64::from(v);
        [
            s(size_of::<HwParams>()),
            s(offset_of!(HwParams, masks)),
            s(offset_of!(HwParams, intervals)),
            s(offset_of!(HwParams, rmask)),
            s(offset_of!(HwParams, cmask)),
            s(offset_of!(HwParams, info)),
            s(offset_of!(HwParams, msbits)),
            s(offset_of!(HwParams, rate_num)),
            s(offset_of!(HwParams, rate_den)),
            s(offset_of!(HwParams, fifo_size)),
            s(size_of::<SwParams>()),
            s(offset_of!(SwParams, avail_min)),
            s(offset_of!(SwParams, start_threshold)),
            s(offset_of!(SwParams, stop_threshold)),
            s(offset_of!(SwParams, boundary)),
            s(offset_of!(SwParams, proto)),
            s(size_of::<Status>()),
            s(offset_of!(Status, trigger_tstamp)),
            s(offset_of!(Status, appl_ptr)),
            s(offset_of!(Status, avail)),
            s(offset_of!(Status, suspended_state)),
            s(offset_of!(Status, audio_tstamp_accuracy)),
            s(size_of::<Xferi>()),
            s(size_of::<PcmInfo>()),
            s(offset_of!(PcmInfo, id)),
            s(offset_of!(PcmInfo, name)),
            s(offset_of!(PcmInfo, subname)),
            s(offset_of!(PcmInfo, sync)),
            s(size_of::<CardInfo>()),
            s(offset_of!(CardInfo, id)),
            s(offset_of!(CardInfo, name)),
            s(offset_of!(CardInfo, longname)),
            s(size_of::<ElemId>()),
            s(offset_of!(ElemId, name)),
            s(offset_of!(ElemId, index)),
            s(size_of::<ElemValue>()),
            s(offset_of!(ElemValue, value)),
            s(offset_of!(ElemValue, reserved)),
            s(size_of::<ElemInfo>()),
            s(offset_of!(ElemInfo, elem_type)),
            s(offset_of!(ElemInfo, count)),
            s(offset_of!(ElemInfo, owner)),
            s(offset_of!(ElemInfo, value)),
            s(size_of::<TlvHeader>()),
            r(PCM_IOCTL_PVERSION),
            r(PCM_IOCTL_INFO),
            r(PCM_IOCTL_HW_REFINE),
            r(PCM_IOCTL_HW_PARAMS),
            r(PCM_IOCTL_HW_FREE),
            r(PCM_IOCTL_SW_PARAMS),
            r(PCM_IOCTL_STATUS),
            r(PCM_IOCTL_DELAY),
            r(PCM_IOCTL_PREPARE),
            r(PCM_IOCTL_START),
            r(PCM_IOCTL_DROP),
            r(PCM_IOCTL_DRAIN),
            r(PCM_IOCTL_WRITEI_FRAMES),
            r(PCM_IOCTL_READI_FRAMES),
            r(CTL_IOCTL_PVERSION),
            r(CTL_IOCTL_CARD_INFO),
            r(CTL_IOCTL_ELEM_INFO),
            r(CTL_IOCTL_ELEM_READ),
            r(CTL_IOCTL_ELEM_WRITE),
            r(CTL_IOCTL_TLV_READ),
            r(CTL_IOCTL_PCM_NEXT_DEVICE),
            r(CTL_IOCTL_PCM_INFO),
            r(PCM_VERSION),
            r(CTL_VERSION),
            r(TLVT_CONTAINER),
            r(TLVT_CHMAP_FIXED),
            r(TLVT_CHMAP_VAR),
            r(TLVT_CHMAP_PAIRED),
            r(CHMAP_POSITION_MASK),
            u64::from(ELEM_TYPE_INTEGER as u32),
            s(ELEM_ID_NAME_MAXLEN),
        ]
    }

    #[test]
    fn layouts_and_ioctls_match_the_kernel_header() {
        let got = computed();
        for (i, (g, r)) in got.iter().zip(REFERENCE.iter()).enumerate() {
            assert_eq!(g, r, "第 {i} 项与头文件不同");
        }
    }

    /// 头文件中其余用到的编号：`_IOW('A', 0x04, int)`、`_IOW('U', 0x32, int)`。
    #[test]
    fn other_ioctls() {
        assert_eq!(PCM_IOCTL_USER_PVERSION, 0x4004_4104);
        assert_eq!(CTL_IOCTL_PCM_PREFER_SUBDEVICE, 0x4004_5532);
        assert_eq!(protocol_major(PCM_VERSION), 2);
    }

    #[test]
    fn elem_value_longs() {
        let mut v = ElemValue::zeroed();
        assert!(v.set_integer(0, 3) && v.set_integer(1, -4) && v.set_integer(127, 9));
        assert!(!v.set_integer(128, 1));
        assert_eq!(
            (v.integer(0), v.integer(1), v.integer(127)),
            (Some(3), Some(-4), Some(9))
        );
        assert_eq!(v.integer(128), None);
        // 32 位上 long 只有 32 位：超出范围拒绝。
        assert_eq!(v.set_integer(2, i64::MAX), size_of::<isize>() == 8);
    }

    #[test]
    fn c_text_round_trip() {
        let mut b = [0xFFu8; 8];
        assert!(set_c_text(&mut b, "PCH"));
        assert_eq!(c_text(&b), "PCH");
        assert!(!set_c_text(&mut b, "12345678"));
        assert_eq!(c_text(b"abc"), "abc");
    }
}
