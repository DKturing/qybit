//! 设备帧队列接口（M3 T1）。
//!
//! 生产端（非实时：解码 + pack 后入队）实现 [`FrameSink`]，消费端（音频回调、渲染线程或
//! DMA 中断：只做有界搬运）实现 [`FrameSource`]。两端都只按完整设备帧入队/出队。
//!
//! 现有两种实现，放在同一接口之后：
//! - [`crate::ring`]（`ring` 特性，需要 alloc）：`rtrb` 环形缓冲，桌面与模拟宿主使用；
//! - [`crate::static_queue`]（`static-queue` 特性，`no_std` 无 alloc）：`bbqueue` 静态
//!   BipBuffer，MCU 宿主使用。

use core::fmt;

use crate::pack::{ChannelMode, Exact, PackError, Pcm};
use crate::{DeviceFormat, MAX_CHANNELS};

/// 最大设备帧字节数：有布局可描述的最大声道数（18）× C32。
pub const MAX_FRAME_BYTES: usize = MAX_CHANNELS as usize * 4;

/// 需要暂存区的队列实现（`rtrb`）每批 pack 的最大字节数：4096 帧双声道 C32（32 KiB）。
/// 声道更多时每批帧数相应减少（至少一帧，18 声道 C32 一帧 72 字节），不改变结果。
pub const PACK_SCRATCH_BYTES: usize = 4096 * 2 * 4;

/// 按目标能力计算设备帧队列须容纳的数据字节数：`rate` 下 `millis` 毫秒（帧数向上取整）×
/// 声道 × 容器字节。整数溢出时返回 `None`。静态 BipBuffer 的存储还须另加
/// [`BIP_MARGIN_FRAMES`] 帧（见 [`crate::static_queue`]）。
pub const fn queue_bytes(
    rate: u32,
    millis: u32,
    channels: u16,
    container_bytes: usize,
) -> Option<usize> {
    let frames = crate::frames_for_millis(rate, millis);
    let Some(frame) = (channels as usize).checked_mul(container_bytes) else {
        return None;
    };
    if frames > usize::MAX as u64 {
        return None;
    }
    (frames as usize).checked_mul(frame)
}

/// 静态 BipBuffer 满时，尾部与头部的空闲段可能各不足一帧而无法写入：存储比数据预算多留两帧。
pub const BIP_MARGIN_FRAMES: usize = 2;

/// 缓冲水位统计（R1）：消费端每次取数前记录队列中的帧数。只观测，不影响取数与样本。
///
/// 只在设备运行中、未暂停、未排空时观测（排空时队列本来就会取空）。分档互斥：
/// - **临界**：队列帧数 < `critical_frames`（通常为 2 个输出周期），下一次取数可能不够；
/// - **低水位**：`critical_frames` ≤ 队列帧数 < `low_frames`（通常为预填量的一半）。
///
/// 欠载另行计数。所有操作为常数次比较与加法，可在中断或音频回调中调用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Watermark {
    low_frames: u64,
    critical_frames: u64,
    /// 观测到的最低队列帧数；尚未观测时为 `None`。
    pub min_frames: Option<u64>,
    pub observations: u64,
    pub low: u64,
    pub critical: u64,
}

impl Watermark {
    pub const fn new(low_frames: u64, critical_frames: u64) -> Self {
        Self {
            low_frames,
            critical_frames,
            min_frames: None,
            observations: 0,
            low: 0,
            critical: 0,
        }
    }

    /// 按约定阈值：低水位为预填帧数的一半，临界为 2 个输出周期。
    pub const fn for_stream(prefill_frames: u64, period_frames: u64) -> Self {
        Self::new(prefill_frames / 2, period_frames.saturating_mul(2))
    }

    pub const fn low_frames(&self) -> u64 {
        self.low_frames
    }

    pub const fn critical_frames(&self) -> u64 {
        self.critical_frames
    }

    /// 记录一次取数前的队列帧数。
    pub fn observe(&mut self, queued_frames: u64) {
        self.observations = self.observations.saturating_add(1);
        self.min_frames = Some(match self.min_frames {
            Some(m) if m <= queued_frames => m,
            _ => queued_frames,
        });
        if queued_frames < self.critical_frames {
            self.critical = self.critical.saturating_add(1);
        } else if queued_frames < self.low_frames {
            self.low = self.low.saturating_add(1);
        }
    }

    /// 最低水位折合的毫秒数（向下取整）。
    pub fn min_ms(&self, sample_rate: u32) -> Option<u64> {
        match (self.min_frames, sample_rate) {
            (Some(f), r) if r > 0 => Some(f.saturating_mul(1000) / u64::from(r)),
            _ => None,
        }
    }
}

/// 队列错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingError {
    /// 尚未用 `reconfigure` 配置帧宽与代次。
    NotConfigured,
    /// 写入的字节数不是完整帧的整数倍。
    PartialFrame,
    /// 队列容量不足以容纳当前格式下的缓冲预算。
    CapacityExceeded { required: usize, capacity: usize },
    /// 新代次必须严格大于当前代次。
    StaleGeneration,
    /// 生产端与消费端不属于同一个队列。
    Mismatched,
    /// 设备格式帧宽与队列当前配置不符。
    FrameWidth,
    /// pack 拒绝（精度、格式或样本范围）。
    Pack(PackError),
    /// 同一端已有未完成的读/写授权（静态队列；调用方违反单生产者/单消费者约定）。
    Busy,
}

/// 设备帧队列的生产端。
pub trait FrameSink {
    type Error: Copy + fmt::Debug;

    /// 把 `pcm` 中能放入队列的完整帧按 `mode` pack 为设备格式后入队，返回入队帧数；队列满时
    /// 返回 0。`scratch` 为宿主启动时分配的 pack 暂存区；能直接 pack 进队列存储的实现可以不用它。
    fn push(
        &mut self,
        pcm: &Pcm<'_, Exact>,
        device: &DeviceFormat,
        mode: ChannelMode,
        scratch: &mut [u8],
    ) -> Result<usize, Self::Error>;

    /// 已入队、尚未被消费端取走的帧数（消费端可能同时取走，结果只会偏大）。
    fn queued_frames(&self) -> usize;
}

/// 设备帧队列的消费端。所有操作有界：不分配、不阻塞、不解码、不 pack。
pub trait FrameSource {
    type Error: Copy + fmt::Debug;

    /// 当前帧宽（字节）；未配置时为 0。
    fn frame_bytes(&self) -> usize;

    /// 已入队的完整帧数。
    fn available_frames(&self) -> usize;

    /// 取出至多 `out.len() / frame_bytes` 个完整帧拷入 `out`，返回帧数。
    fn pop_frames(&mut self, out: &mut [u8]) -> Result<usize, Self::Error>;

    /// 把队列首部一段连续的完整帧（至多 `max_frames` 帧）交给 `write`，按其返回的帧数提交；
    /// 未写出的帧留在队列中，下次从同一位置继续。返回提交的帧数。
    fn consume_contiguous(
        &mut self,
        max_frames: usize,
        write: impl FnOnce(&[u8]) -> usize,
    ) -> Result<usize, Self::Error>;
}

#[cfg(test)]
mod watermark_tests {
    use super::Watermark;

    #[test]
    fn bins_are_exclusive_and_min_tracks() {
        // 预填 1000 帧 → 低水位线 500；周期 100 帧 → 临界线 200。
        let mut w = Watermark::for_stream(1000, 100);
        assert_eq!((w.low_frames(), w.critical_frames()), (500, 200));
        assert_eq!(w.min_ms(48_000), None);
        for q in [900, 500, 499, 200, 199, 0, 1000] {
            w.observe(q);
        }
        assert_eq!(w.observations, 7);
        assert_eq!(w.min_frames, Some(0));
        // 499、200 为低水位；199、0 为临界。
        assert_eq!((w.low, w.critical), (2, 2));
        let mut w = Watermark::for_stream(1000, 100);
        w.observe(4800);
        assert_eq!(w.min_ms(48_000), Some(100));
    }
}
