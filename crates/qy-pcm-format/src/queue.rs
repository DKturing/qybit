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

use crate::pack::{Exact, PackError, Pcm};
use crate::{DeviceFormat, MAX_CHANNELS};

/// 当前实现范围内的最大设备帧字节数：双声道 × C32（多声道见 tasks/FORMATS.md F1）。
pub const MAX_FRAME_BYTES: usize = MAX_CHANNELS as usize * 4;

/// 需要暂存区的队列实现（`rtrb`）每批 pack 的最大字节数：工作区最大帧数 × 最大帧宽。
pub const PACK_SCRATCH_BYTES: usize = 4096 * MAX_FRAME_BYTES;

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

    /// 把 `pcm` 中能放入队列的完整帧 pack 为设备格式后入队，返回入队帧数；队列满时返回 0。
    /// `scratch` 为宿主启动时分配的 pack 暂存区；能直接 pack 进队列存储的实现可以不用它。
    fn push(
        &mut self,
        pcm: &Pcm<'_, Exact>,
        device: &DeviceFormat,
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
