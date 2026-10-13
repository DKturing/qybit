//! ALSA PCM 的最小操作集合（与 alsa-lib 一一对应），真实实现见 `linux::real`，
//! 测试用假实现见 [`crate::testing`]。错误一律为 alsa-lib 调用名与正 errno。

use std::fmt;

use qy_pcm_format::ChannelLayout;
use qy_pcm_format::alsa::{AlsaFormat, ChmapType, HwApplied, HwRequest};

/// alsa-lib 调用失败：调用名与正 errno。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmError {
    pub call: &'static str,
    pub errno: i32,
}

impl fmt::Display for PcmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} errno {}（{}）",
            self.call,
            self.errno,
            errno_name(self.errno)
        )
    }
}

/// Linux errno（`asm-generic/errno-base.h`、`errno.h`）。
pub mod errno {
    pub const EPERM: i32 = 1;
    pub const ENOENT: i32 = 2;
    pub const EINTR: i32 = 4;
    pub const EIO: i32 = 5;
    pub const EAGAIN: i32 = 11;
    pub const EACCES: i32 = 13;
    pub const EBUSY: i32 = 16;
    pub const ENODEV: i32 = 19;
    pub const EINVAL: i32 = 22;
    pub const EPIPE: i32 = 32;
    pub const EBADFD: i32 = 77;
    pub const ESTRPIPE: i32 = 86;
}

pub fn errno_name(e: i32) -> &'static str {
    match e {
        errno::EPERM => "EPERM",
        errno::ENOENT => "ENOENT",
        errno::EINTR => "EINTR",
        errno::EIO => "EIO",
        errno::EAGAIN => "EAGAIN",
        errno::EACCES => "EACCES",
        errno::EBUSY => "EBUSY",
        errno::ENODEV => "ENODEV",
        errno::EINVAL => "EINVAL",
        errno::EPIPE => "EPIPE",
        errno::EBADFD => "EBADFD",
        errno::ESTRPIPE => "ESTRPIPE",
        _ => "?",
    }
}

/// 硬件参数计划：格式、采样率与声道精确设置；周期与缓冲取设备在该配置空间内
/// 接受的最近值（不影响样本），设置后作为请求值参与读回核对。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HwPlan {
    pub format: AlsaFormat,
    pub sample_rate: u32,
    pub channels: u16,
    /// 设备声道布局（设备上的样本顺序）。
    pub layout: ChannelLayout,
    /// 目标周期帧数。
    pub period_frames: u32,
    /// 缓冲为周期的整数倍。
    pub periods: u32,
}

/// 一次参数设置的结果：实际请求（含设备接受的周期/缓冲）与读回值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Configured {
    pub request: HwRequest,
    pub applied: HwApplied,
}

/// 一个已打开的播放 PCM。
pub trait Pcm: Send + 'static {
    /// 设备在 RW_INTERLEAVED 下接受的格式（`snd_pcm_hw_params_test_format`），按
    /// [`AlsaFormat::ALL`] 的顺序。
    fn supported_formats(&mut self) -> Result<Vec<AlsaFormat>, PcmError>;
    /// 设置硬件参数（访问方式、格式、声道、精确采样率、关闭重采样、周期、缓冲），
    /// 应用后用 `snd_pcm_hw_params_current` 读回；并设置软件参数：只由显式 `start`
    /// 启动、可写阈值为一个周期。
    fn configure(&mut self, plan: &HwPlan) -> Result<Configured, PcmError>;
    /// 释放硬件参数（换下一个候选格式前）。
    fn hw_free(&mut self) -> Result<(), PcmError>;
    fn start(&mut self) -> Result<(), PcmError>;
    /// `snd_pcm_drop`：立即停止，丢弃未播放数据。
    fn drop_now(&mut self) -> Result<(), PcmError>;
    /// `snd_pcm_drain`：播完已写入数据后停止。
    fn drain(&mut self) -> Result<(), PcmError>;
    /// `snd_pcm_prepare`：xrun 后恢复。
    fn prepare(&mut self) -> Result<(), PcmError>;
    /// `snd_pcm_wait`：等待可写，`Ok(false)` 为超时。
    fn wait(&mut self, timeout_ms: u32) -> Result<bool, PcmError>;
    /// `snd_pcm_avail`：可写帧数；xrun 时返回 `EPIPE`。
    fn avail(&mut self) -> Result<u64, PcmError>;
    /// `snd_pcm_writei`：写入完整帧字节，返回实际写入帧数（可少于所给帧数）。
    fn writei(&mut self, frames: &[u8]) -> Result<usize, PcmError>;
    /// xrun 期间缺失的帧数估计（由触发时间戳与当前时间换算）；无法测得为 0。
    fn xrun_missing_frames(&mut self) -> u32;
    /// `snd_pcm_query_chmaps`：设备支持的声道映射（类型, 原始位置值）；不支持或无法解析的
    /// 映射不列出（F1）。
    fn query_chmaps(&mut self) -> Vec<(ChmapType, Vec<u32>)>;
    /// `snd_pcm_set_chmap`（在 hw_params 之后调用）。位置值不得含标志位。
    fn set_chmap(&mut self, positions: &[u32]) -> Result<(), PcmError>;
    /// `snd_pcm_get_chmap`：当前映射的原始位置值；设备不支持读回为 `Ok(None)`。
    fn get_chmap(&mut self) -> Result<Option<Vec<u32>>, PcmError>;
}

/// 打开 PCM。
pub trait Opener {
    type Pcm: Pcm;
    /// `snd_pcm_open`（阻塞模式，播放方向）。
    fn open(&mut self, name: &str) -> Result<Self::Pcm, PcmError>;
    /// 设备被占用时，占用 `/dev/snd/pcmC{card}D{device}p` 的进程（`pid 名称`）。
    fn occupants(&self, _card: u32, _device: u32) -> Vec<String> {
        Vec::new()
    }
}
