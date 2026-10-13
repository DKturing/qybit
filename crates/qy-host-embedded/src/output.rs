//! 音频输出接口（I²S / SAI + DMA）。
//!
//! `embedded-hal` 1.0 没有 I²S/DMA 的通用接口，因此由本项目定义一个最小 trait，板级适配实现：
//! - [`AudioOutput::configure`]：按设备格式设置时钟与槽位，DMA 处于停止状态；
//! - [`AudioOutput::start`] / [`AudioOutput::stop`]：启动/停止循环 DMA。DMA 每完成一个缓冲，
//!   板级中断处理调用 [`crate::feeder::Feeder::fill`] 填下一个缓冲（只做有界搬运与计数）。
//!
//! 板子能产生哪些格式由 [`OutputCaps`] 声明。**采样率必须能被时钟精确产生**：分频后与请求值不等
//! 的采样率（例如 61.44 MHz I²S 时钟下的 44.1 kHz）不得列出，否则等于改变了播放速度与音高。
//! 槽位格式描述 DMA 内存中的样本表示（容器位宽、有效位、字节序、放置），由 pack 按它放置样本。

use core::fmt;

use qy_core::{DeviceFormat, SourceFormat};
use qy_pcm_format::{Endian, Placement};

/// 一种槽位格式：DMA 内存中每个样本的表示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotFormat {
    pub container_bits: u8,
    pub valid_bits: u8,
    pub endian: Endian,
    pub placement: Placement,
}

/// 板级输出能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputCaps {
    /// 时钟能精确产生的采样率。
    pub rates: &'static [u32],
    /// 支持的槽位格式，按优先顺序（一般从小容器到大容器）。
    pub slots: &'static [SlotFormat],
    /// 支持的声道数（标准 I²S 为 2；能输出单声道的 SAI 可列出 1）。
    pub channels: &'static [u16],
    /// 每个 DMA 缓冲的帧数（每次 `fill` 的帧数）。
    pub buffer_frames: u32,
    /// 已填好、尚未播完的缓冲数：第 m 次 `fill` 时，第 m − `in_flight` 次及更早填入的数据已经
    /// 播完（经典乒乓双缓冲为 2）。用于判定排空完成。
    pub in_flight: u32,
}

/// 选不出设备格式的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectError {
    /// 时钟不能精确产生该采样率。
    Rate(u32),
    /// 不支持该声道数。
    Channels(u16),
    /// 声道布局与设备不同（I²S 只有默认的单声道 / 左右声道）。
    Layout,
    /// 没有能无损承载该位深的槽位格式（N > V）。
    Bits(u8),
}

impl fmt::Display for SelectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rate(r) => write!(f, "本板时钟不能精确产生 {r} Hz"),
            Self::Channels(c) => write!(f, "本板不支持 {c} 声道输出"),
            Self::Layout => f.write_str("声道布局与本板输出不同"),
            Self::Bits(b) => write!(f, "本板没有能无损承载 {b} 位的槽位格式"),
        }
    }
}

impl OutputCaps {
    /// 为源选择设备格式：采样率与声道精确相同、布局相同，取第一个能无损承载源（N ≤ V）的槽位。
    pub fn select(&self, src: &SourceFormat) -> Result<DeviceFormat, SelectError> {
        if !self.rates.contains(&src.sample_rate()) {
            return Err(SelectError::Rate(src.sample_rate()));
        }
        if !self.channels.contains(&src.channels()) {
            return Err(SelectError::Channels(src.channels()));
        }
        let mut layout_ok = false;
        for s in self.slots {
            let Ok(d) = DeviceFormat::new(
                src.sample_rate(),
                src.channels(),
                s.container_bits,
                s.valid_bits,
                s.endian,
                s.placement,
            ) else {
                continue;
            };
            if d.layout() != src.layout() {
                continue;
            }
            layout_ok = true;
            if d.accepts(src) {
                return Ok(d);
            }
        }
        if layout_ok {
            Err(SelectError::Bits(src.bits()))
        } else {
            Err(SelectError::Layout)
        }
    }

    /// 最长的 DMA 缓冲周期（微秒，向上取整）：最低采样率下一个缓冲的时长。
    pub fn max_period_us(&self) -> u64 {
        let min_rate = self.rates.iter().copied().min().unwrap_or(0);
        if min_rate == 0 {
            return 0;
        }
        (u64::from(self.buffer_frames) * 1_000_000).div_ceil(u64::from(min_rate))
    }
}

/// 板级音频输出（I²S/SAI 外设与 DMA）。
pub trait AudioOutput {
    type Error: fmt::Debug;

    fn caps(&self) -> OutputCaps;

    /// 按设备格式设置时钟与槽位（DMA 停止时调用）。只接受 [`OutputCaps::select`] 选出的格式；
    /// 硬件不能精确产生时必须返回错误，不得近似。
    fn configure(&mut self, format: &DeviceFormat) -> Result<(), Self::Error>;

    /// 启动循环 DMA。此后每个缓冲完成时，板级中断调用 [`crate::feeder::Feeder::fill`]。
    fn start(&mut self) -> Result<(), Self::Error>;

    /// 停止 DMA（可重复调用）。
    fn stop(&mut self);
}
