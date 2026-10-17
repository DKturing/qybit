//! 直接使用内核 ALSA 接口的 PCM（A1，不依赖 alsa-lib）。
//!
//! 打开（出处：alsa-lib `src/pcm/pcm_hw.c` `snd_pcm_hw_open` 的同一流程）：
//! - 设备名经 [`check_device_name`]（只接受 `hw:`），声卡名按各卡 id 解析为卡号；
//! - 打开控制设备，设定子设备偏好（未指定为 −1），以非阻塞方式打开
//!   `/dev/snd/pcmC{卡}D{设备}{p|c}`（被占用即 `EBUSY`），再恢复阻塞读写；指定了子设备时用
//!   `SNDRV_PCM_IOCTL_INFO` 核对，不符则重试（至多 4 次，之后 `EBUSY`）；
//! - 核对 PCM 接口主版本为 2，并用 `SNDRV_PCM_IOCTL_USER_PVERSION` 告知内核本程序转写的
//!   接口版本（旧内核没有这个 ioctl，忽略其失败）。
//!
//! 参数（出处：内核 `sound/core/pcm_native.c` 的 `snd_pcm_hw_refine` / `snd_pcm_hw_params` /
//! `snd_pcm_sw_params`）：
//! - 用 `HW_REFINE` 逐项收窄：访问方式 RW_INTERLEAVED、格式、声道（精确）、
//!   `SNDRV_PCM_HW_PARAMS_NORESAMPLE`、采样率（精确，不取近似值）；周期与缓冲取设备在该配置
//!   空间内接受的最近值（不改变样本，见 [`crate::kernel::hw::set_near`]），作为请求值；
//! - `HW_PARAMS` 应用；内核把最终参数写回同一结构体，直接读回（格式、访问方式、声道、周期、
//!   缓冲、精确有理数采样率 `rate_num/rate_den`、有效位 `msbits`、重采样标志），不经文本转储；
//! - 软件参数：`start_threshold` 为回绕点（只由显式 `START` 启动）、`avail_min` 为一个周期、
//!   `stop_threshold` 为缓冲长度（取空即 xrun）。播放方向另设 `silence_threshold = 0`、
//!   `silence_size ≥ boundary`（内核 `snd_pcm_playback_silence` 的“整个空闲区填静音”方式）：
//!   内核在推进硬件指针时（持流锁）把刚播完的区域清零，应用写入前该区域已是静音。排空在周期
//!   边界才结束、硬件指针会越过最后写入的数据（实测约一个周期内），这段播放的是静音而不是缓冲中
//!   上一轮的旧音频；欠载被发现前同理。不改变写入的样本；
//! - 应用后 `PREPARE`（与 alsa-lib `snd_pcm_hw_params` 自动准备相同）。
//!
//! 写入用 `WRITEI_FRAMES`（阻塞；调用方按可写量写，内核可能少写），帧字节数取自内核读回的
//! `FRAME_BITS`，保证内核访问的字节不超出所给切片。可写量与状态用 `STATUS`（运行中时内核先
//! 更新硬件指针）；等待用 `poll`。错误报告为 ioctl 名与正 errno。

use std::fs::File;
use std::path::PathBuf;
use std::time::Instant;

use qy_pcm_format::alsa::{Access, AlsaFormat, ChmapType, HwApplied, HwRequest, check_device_name};

use super::ctl::{Ctl, SND_DIR, resolve_card};
use super::sys;
use crate::kernel::abi::{
    ACCESS_MMAP_INTERLEAVED, ACCESS_RW_INTERLEAVED, BUFFER_SIZE, CHANNELS, FRAME_BITS,
    HW_PARAMS_NORESAMPLE, HwParams, MASK_ACCESS, MASK_FORMAT, PCM_IOCTL_DELAY, PCM_IOCTL_DRAIN,
    PCM_IOCTL_DROP, PCM_IOCTL_HW_FREE, PCM_IOCTL_HW_PARAMS, PCM_IOCTL_HW_REFINE, PCM_IOCTL_INFO,
    PCM_IOCTL_PREPARE, PCM_IOCTL_PVERSION, PCM_IOCTL_READI_FRAMES, PCM_IOCTL_START,
    PCM_IOCTL_STATUS, PCM_IOCTL_SW_PARAMS, PCM_IOCTL_USER_PVERSION, PCM_IOCTL_WRITEI_FRAMES,
    PCM_VERSION, PERIOD_SIZE, PcmInfo, RATE, STATE_DISCONNECTED, STATE_OPEN, STATE_SUSPENDED,
    STATE_XRUN, STREAM_CAPTURE, STREAM_PLAYBACK, Status, SwParams, Zeroed, protocol_major,
};
use crate::kernel::hw::{self, Refine};
use crate::pcm::{Configured, HwPlan, Opener, Pcm, PcmError, errno};

/// 打开子设备时的重试次数（与 alsa-lib 相同）。
const OPEN_ATTEMPTS: u32 = 4;

fn pe(call: &'static str) -> impl Fn(i32) -> PcmError {
    move |errno| PcmError { call, errno }
}

/// 内核直连的打开器（播放方向）。
#[derive(Debug, Clone, Copy, Default)]
pub struct KernelOpener;

impl Opener for KernelOpener {
    type Pcm = KernelPcm;

    fn open(&mut self, name: &str) -> Result<KernelPcm, PcmError> {
        KernelPcm::open(name, STREAM_PLAYBACK)
    }

    fn occupants(&self, card: u32, device: u32) -> Vec<String> {
        super::occupants::find(card, device)
    }
}

/// 设置参数后的结果（内部用）。
struct Setup {
    period: u32,
    buffer: u32,
    applied: HwApplied,
}

/// 一个打开的内核 PCM 子设备。
pub struct KernelPcm {
    file: File,
    card: u32,
    device: u32,
    subdevice: u32,
    stream: i32,
    /// 内核读回的帧字节数；未设置参数时为 0（读写一律拒绝）。
    frame_bytes: usize,
    channels: u32,
    rate: u32,
    /// 上次写入后的时间与设备中待播帧数（`DELAY`），用于估计 xrun 缺失帧。
    last_write: Option<(Instant, i64)>,
}

impl KernelPcm {
    /// 打开 `hw:` 设备的播放或回录方向。
    pub fn open(name: &str, stream: i32) -> Result<Self, PcmError> {
        let hw = check_device_name(name).map_err(|_| PcmError {
            call: "hw 设备名",
            errno: errno::EINVAL,
        })?;
        let card = resolve_card(hw.card)?;
        let ctl = Ctl::open(card)?;
        let dir = if stream == STREAM_CAPTURE { 'c' } else { 'p' };
        let path = PathBuf::from(format!("{SND_DIR}/pcmC{card}D{}{dir}", hw.device));
        let want = hw
            .subdevice
            .and_then(|s| i32::try_from(s).ok())
            .unwrap_or(-1);
        let mut attempt = 0;
        let (file, info) = loop {
            attempt += 1;
            if attempt > OPEN_ATTEMPTS {
                return Err(PcmError {
                    call: "open /dev/snd/pcm*（子设备）",
                    errno: errno::EBUSY,
                });
            }
            ctl.prefer_subdevice(want)?;
            let file = sys::open_rw(&path, true).map_err(pe("open /dev/snd/pcm*"))?;
            let mut info = PcmInfo::zeroed();
            sys::ioctl(&file, PCM_IOCTL_INFO, &mut info).map_err(pe("SNDRV_PCM_IOCTL_INFO"))?;
            if want < 0 || info.subdevice == want as u32 {
                break (file, info);
            }
        };
        drop(ctl);
        let mut v = 0i32;
        sys::ioctl(&file, PCM_IOCTL_PVERSION, &mut v).map_err(pe("SNDRV_PCM_IOCTL_PVERSION"))?;
        if protocol_major(v as u32) != 2 {
            return Err(PcmError {
                call: "SNDRV_PCM_IOCTL_PVERSION（PCM 接口主版本不是 2）",
                errno: errno::EINVAL,
            });
        }
        let mut user = PCM_VERSION as i32;
        let _ = sys::ioctl(&file, PCM_IOCTL_USER_PVERSION, &mut user);
        Ok(Self {
            file,
            card,
            device: hw.device,
            subdevice: info.subdevice,
            stream,
            frame_bytes: 0,
            channels: 0,
            rate: 0,
            last_write: None,
        })
    }

    /// 实际打开的子设备号。
    pub fn subdevice(&self) -> u32 {
        self.subdevice
    }

    fn refiner(&self) -> impl Refine + '_ {
        |p: &mut HwParams| {
            // 每次都请求全部参数（内核收窄后清零 rmask），收窄结果与只请求改动项相同。
            p.rmask = u32::MAX;
            sys::ioctl(&self.file, PCM_IOCTL_HW_REFINE, p).map(|_| ())
        }
    }

    /// 全部参数空间，限定 RW_INTERLEAVED。
    fn space(&self) -> Result<HwParams, PcmError> {
        let mut r = self.refiner();
        let mut p = hw::any();
        r.refine(&mut p).map_err(pe("SNDRV_PCM_IOCTL_HW_REFINE"))?;
        hw::set_mask(&mut p, MASK_ACCESS, ACCESS_RW_INTERLEAVED);
        r.refine(&mut p)
            .map_err(pe("SNDRV_PCM_IOCTL_HW_REFINE（访问方式 RW_INTERLEAVED）"))?;
        Ok(p)
    }

    fn setup(
        &mut self,
        format: AlsaFormat,
        rate: u32,
        channels: u16,
        period_frames: u32,
        periods: u32,
    ) -> Result<Setup, PcmError> {
        self.frame_bytes = 0;
        let mut p = self.space()?;
        let (period, buffer) = {
            let mut r = self.refiner();
            hw::set_mask(&mut p, MASK_FORMAT, format.code() as u32);
            r.refine(&mut p)
                .map_err(pe("SNDRV_PCM_IOCTL_HW_REFINE（格式）"))?;
            hw::set_exact(&mut p, CHANNELS, u32::from(channels), &mut r)
                .map_err(pe("SNDRV_PCM_IOCTL_HW_REFINE（声道）"))?;
            p.flags |= HW_PARAMS_NORESAMPLE;
            hw::set_exact(&mut p, RATE, rate, &mut r)
                .map_err(pe("SNDRV_PCM_IOCTL_HW_REFINE（采样率）"))?;
            let period = hw::set_near(&mut p, PERIOD_SIZE, period_frames, &mut r)
                .map_err(pe("SNDRV_PCM_IOCTL_HW_REFINE（周期）"))?;
            let buffer = hw::set_near(&mut p, BUFFER_SIZE, period.saturating_mul(periods), &mut r)
                .map_err(pe("SNDRV_PCM_IOCTL_HW_REFINE（缓冲）"))?;
            (period, buffer)
        };
        p.rmask = u32::MAX;
        sys::ioctl(&self.file, PCM_IOCTL_HW_PARAMS, &mut p)
            .map_err(pe("SNDRV_PCM_IOCTL_HW_PARAMS"))?;
        let applied = read_back(&p);
        let frame_bits = hw::interval_single(&p, FRAME_BITS).ok_or(PcmError {
            call: "SNDRV_PCM_IOCTL_HW_PARAMS（读回帧位数不是单值）",
            errno: errno::EINVAL,
        })?;
        let frame_bytes = if frame_bits > 0 && frame_bits.is_multiple_of(8) {
            frame_bits as usize / 8
        } else {
            return Err(PcmError {
                call: "SNDRV_PCM_IOCTL_HW_PARAMS（读回帧位数不是整字节）",
                errno: errno::EINVAL,
            });
        };
        let applied_period = applied.period_frames as usize;
        let applied_buffer = applied.buffer_frames as usize;
        let boundary = hw::boundary(applied_buffer);
        let mut sw = SwParams::zeroed();
        sw.period_step = 1;
        sw.avail_min = applied_period.max(1);
        sw.start_threshold = boundary;
        sw.stop_threshold = applied_buffer;
        if self.stream == STREAM_PLAYBACK {
            sw.silence_threshold = 0;
            sw.silence_size = boundary;
        }
        sw.boundary = boundary;
        sw.proto = PCM_VERSION;
        sys::ioctl(&self.file, PCM_IOCTL_SW_PARAMS, &mut sw)
            .map_err(pe("SNDRV_PCM_IOCTL_SW_PARAMS"))?;
        sys::ioctl_none(&self.file, PCM_IOCTL_PREPARE).map_err(pe("SNDRV_PCM_IOCTL_PREPARE"))?;
        self.frame_bytes = frame_bytes;
        self.channels = applied.channels;
        self.rate = rate;
        self.last_write = None;
        Ok(Setup {
            period,
            buffer,
            applied,
        })
    }

    fn status(&self) -> Result<Status, i32> {
        let mut s = Status::zeroed();
        sys::ioctl(&self.file, PCM_IOCTL_STATUS, &mut s)?;
        Ok(s)
    }

    /// 状态对应的错误（与 alsa-lib `pcm_state_to_error` 相同）；正常状态为 `None`。
    fn state_error(state: i32) -> Option<i32> {
        match state {
            STATE_XRUN => Some(errno::EPIPE),
            STATE_SUSPENDED => Some(errno::ESTRPIPE),
            STATE_DISCONNECTED => Some(errno::ENODEV),
            STATE_OPEN => Some(errno::EBADFD),
            _ => None,
        }
    }

    /// `poll` 报告错误事件后，按状态给出原因。
    fn poll_error(&self, call: &'static str) -> PcmError {
        let e = match self.status() {
            Ok(s) => Self::state_error(s.state).unwrap_or(errno::EIO),
            Err(e) => e,
        };
        PcmError { call, errno: e }
    }

    fn wait_for(&mut self, events: i16, timeout_ms: u32) -> Result<bool, PcmError> {
        match sys::poll(&self.file, events, timeout_ms) {
            Err(e) => Err(PcmError {
                call: "poll",
                errno: e,
            }),
            Ok(None) => Ok(false),
            Ok(Some(rev)) if rev & (sys::POLLERR | sys::POLLHUP | sys::POLLNVAL) != 0 => {
                Err(self.poll_error("poll"))
            }
            Ok(Some(_)) => Ok(true),
        }
    }

    /// 读取交错帧（回录方向），返回帧数。
    pub fn readi(&mut self, buf: &mut [u8]) -> Result<usize, PcmError> {
        if self.frame_bytes == 0 {
            return Err(PcmError {
                call: "SNDRV_PCM_IOCTL_READI_FRAMES",
                errno: errno::EBADFD,
            });
        }
        sys::readi(&self.file, PCM_IOCTL_READI_FRAMES, buf, self.frame_bytes)
            .map_err(pe("SNDRV_PCM_IOCTL_READI_FRAMES"))
    }

    /// 回录用：按与播放端相同的严格规则设置参数（缓冲为 `periods` 个周期），返回读回值。
    pub fn configure_capture(
        &mut self,
        format: AlsaFormat,
        rate: u32,
        channels: u16,
        period_frames: u32,
        periods: u32,
    ) -> Result<HwApplied, PcmError> {
        self.setup(format, rate, channels, period_frames, periods)
            .map(|s| s.applied)
    }
}

/// 从 `HW_PARAMS` 写回的结构体取已应用参数；不是单值的项记为不可能的值，由核对报告不一致。
fn read_back(p: &HwParams) -> HwApplied {
    let access = match hw::mask_single(p, MASK_ACCESS) {
        Some(ACCESS_RW_INTERLEAVED) => Access::RwInterleaved,
        Some(ACCESS_MMAP_INTERLEAVED) => Access::MmapInterleaved,
        _ => Access::Other,
    };
    HwApplied {
        access,
        format_code: hw::mask_single(p, MASK_FORMAT)
            .and_then(|f| i32::try_from(f).ok())
            .unwrap_or(-1),
        rate_num: p.rate_num,
        rate_den: p.rate_den,
        channels: hw::interval_single(p, CHANNELS).unwrap_or(0),
        period_frames: hw::interval_single(p, PERIOD_SIZE).unwrap_or(0),
        buffer_frames: hw::interval_single(p, BUFFER_SIZE).unwrap_or(0),
        resample: p.flags & HW_PARAMS_NORESAMPLE == 0,
        significant_bits: (p.msbits != 0).then_some(p.msbits),
    }
}

impl Pcm for KernelPcm {
    fn supported_formats(&mut self) -> Result<Vec<AlsaFormat>, PcmError> {
        let p = self.space()?;
        let mut r = self.refiner();
        Ok(AlsaFormat::QUERY
            .into_iter()
            .filter(|&f| {
                let mut q = p;
                hw::set_mask(&mut q, MASK_FORMAT, f.code() as u32);
                r.refine(&mut q).is_ok()
            })
            .collect())
    }

    fn configure(&mut self, plan: &HwPlan) -> Result<Configured, PcmError> {
        let s = self.setup(
            plan.format,
            plan.sample_rate,
            plan.channels,
            plan.period_frames,
            plan.periods,
        )?;
        Ok(Configured {
            request: HwRequest {
                format: plan.format,
                sample_rate: plan.sample_rate,
                channels: plan.channels,
                layout: plan.layout,
                period_frames: s.period,
                buffer_frames: s.buffer,
            },
            applied: s.applied,
        })
    }

    fn hw_free(&mut self) -> Result<(), PcmError> {
        self.frame_bytes = 0;
        sys::ioctl_none(&self.file, PCM_IOCTL_HW_FREE)
            .map(|_| ())
            .map_err(pe("SNDRV_PCM_IOCTL_HW_FREE"))
    }

    fn start(&mut self) -> Result<(), PcmError> {
        sys::ioctl_none(&self.file, PCM_IOCTL_START)
            .map(|_| ())
            .map_err(pe("SNDRV_PCM_IOCTL_START"))
    }

    fn drop_now(&mut self) -> Result<(), PcmError> {
        sys::ioctl_none(&self.file, PCM_IOCTL_DROP)
            .map(|_| ())
            .map_err(pe("SNDRV_PCM_IOCTL_DROP"))
    }

    fn drain(&mut self) -> Result<(), PcmError> {
        sys::ioctl_none(&self.file, PCM_IOCTL_DRAIN)
            .map(|_| ())
            .map_err(pe("SNDRV_PCM_IOCTL_DRAIN"))
    }

    fn prepare(&mut self) -> Result<(), PcmError> {
        self.last_write = None;
        sys::ioctl_none(&self.file, PCM_IOCTL_PREPARE)
            .map(|_| ())
            .map_err(pe("SNDRV_PCM_IOCTL_PREPARE"))
    }

    fn wait(&mut self, timeout_ms: u32) -> Result<bool, PcmError> {
        self.wait_for(sys::POLLOUT, timeout_ms)
    }

    fn avail(&mut self) -> Result<u64, PcmError> {
        let s = self.status().map_err(pe("SNDRV_PCM_IOCTL_STATUS"))?;
        if let Some(e) = Self::state_error(s.state) {
            return Err(PcmError {
                call: "SNDRV_PCM_IOCTL_STATUS",
                errno: e,
            });
        }
        Ok(s.avail as u64)
    }

    fn writei(&mut self, frames: &[u8]) -> Result<usize, PcmError> {
        if self.frame_bytes == 0 {
            return Err(PcmError {
                call: "SNDRV_PCM_IOCTL_WRITEI_FRAMES",
                errno: errno::EBADFD,
            });
        }
        let n = sys::writei(
            &self.file,
            PCM_IOCTL_WRITEI_FRAMES,
            frames,
            self.frame_bytes,
        )
        .map_err(pe("SNDRV_PCM_IOCTL_WRITEI_FRAMES"))?;
        let mut delay = 0isize;
        if sys::ioctl(&self.file, PCM_IOCTL_DELAY, &mut delay).is_ok() {
            self.last_write = Some((Instant::now(), delay as i64));
        }
        Ok(n)
    }

    fn xrun_missing_frames(&mut self) -> u32 {
        // 估计：上次写入后设备还有 delay 帧可播；其后的时间都在缺数据。
        let Some((at, delay)) = self.last_write else {
            return 0;
        };
        let elapsed = at.elapsed().as_nanos();
        let played = elapsed.saturating_mul(u128::from(self.rate)) / 1_000_000_000;
        let missing = played.saturating_sub(u128::try_from(delay.max(0)).unwrap_or(0));
        u32::try_from(missing).unwrap_or(u32::MAX)
    }

    fn query_chmaps(&mut self) -> Vec<(ChmapType, Vec<u32>)> {
        match Ctl::open(self.card) {
            Ok(ctl) => ctl.query_chmaps(self.device, self.subdevice, self.stream),
            Err(_) => Vec::new(),
        }
    }

    fn set_chmap(&mut self, positions: &[u32]) -> Result<(), PcmError> {
        Ctl::open(self.card)?.write_chmap(self.device, self.subdevice, self.stream, positions)
    }

    fn get_chmap(&mut self) -> Result<Option<Vec<u32>>, PcmError> {
        // 读不到即设备不支持读回（与 alsa-lib 路径相同）。
        Ok(Ctl::open(self.card).ok().and_then(|ctl| {
            ctl.read_chmap(self.device, self.subdevice, self.stream, self.channels)
        }))
    }
}

/// 回录方向的等待（`POLLIN`）。
impl KernelPcm {
    pub fn wait_readable(&mut self, timeout_ms: u32) -> Result<bool, PcmError> {
        self.wait_for(sys::POLLIN, timeout_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::abi::{INTERVAL_INTEGER, Interval};

    /// `HW_PARAMS` 写回结构体的读回：单值取出，不是单值时记为核对必然拒绝的值。
    #[test]
    fn read_back_from_written_params() {
        let mut p = hw::any();
        hw::set_mask(&mut p, MASK_ACCESS, ACCESS_RW_INTERLEAVED);
        hw::set_mask(&mut p, MASK_FORMAT, AlsaFormat::S24_3Le.code() as u32);
        hw::set_interval(&mut p, CHANNELS, 2, 2);
        hw::set_interval(&mut p, PERIOD_SIZE, 441, 441);
        hw::set_interval(&mut p, BUFFER_SIZE, 1764, 1764);
        p.rate_num = 44_100;
        p.rate_den = 1;
        p.msbits = 24;
        p.flags = HW_PARAMS_NORESAMPLE;
        assert_eq!(
            read_back(&p),
            HwApplied {
                access: Access::RwInterleaved,
                format_code: 32,
                rate_num: 44_100,
                rate_den: 1,
                channels: 2,
                period_frames: 441,
                buffer_frames: 1764,
                resample: false,
                significant_bits: Some(24),
            }
        );
        // 开区间端点按整数换算：(440, 442) 即 441。
        p.intervals[PERIOD_SIZE] = Interval {
            min: 440,
            max: 442,
            flags: INTERVAL_INTEGER | 0b11,
        };
        assert_eq!(read_back(&p).period_frames, 441);
        // 不是单值、未置标志、无有效位。
        p.masks[MASK_FORMAT].bits[0] |= 1 << 2;
        hw::set_interval(&mut p, CHANNELS, 1, 2);
        p.flags = 0;
        p.msbits = 0;
        let a = read_back(&p);
        assert_eq!(a.format_code, -1);
        assert_eq!(a.channels, 0);
        assert!(a.resample);
        assert_eq!(a.significant_bits, None);
    }

    /// 本机有声卡时：打开每个可播放设备、查询格式与声道映射，不设参数、不发声。没有声卡或
    /// 无权打开时打印“未运行”。
    #[test]
    fn open_and_query_local_devices() {
        let list = match super::super::devices::list_playback() {
            Ok(l) => l,
            Err(e) => {
                eprintln!("未运行：无法枚举声卡（{e}）");
                return;
            }
        };
        if list.is_empty() {
            eprintln!("未运行：本机没有可播放的声卡");
        }
        for d in list {
            match KernelOpener.open(&d.name) {
                Ok(mut pcm) => {
                    let f = pcm.supported_formats();
                    let m = pcm.query_chmaps();
                    eprintln!("{}：格式 {f:?}，声道映射 {m:?}", d.name);
                }
                Err(e) => eprintln!("{}：未打开（{e}）", d.name),
            }
        }
    }
}
