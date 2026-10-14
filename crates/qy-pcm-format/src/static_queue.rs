//! 静态设备帧队列（M3 T1）：`no_std`、无 alloc，供 MCU 宿主使用。
//!
//! 底层使用成熟的 `bbqueue`（=0.7.0，BipBuffer，CAS 原子协调、轮询通知；AGENTS §2/§4：不自行
//! 编写无锁缓冲）。存储是 `bbqueue` 的内联数组 `Inline<N>`，随 [`StaticFrameQueue`] 一起放在
//! `static` 中（链接进 `.bss`），不在运行时分配。自定义存储需要实现 `unsafe trait Storage`，
//! 本项目代码不写 `unsafe`，因此不采用 `&'static mut [u8]` 存储；`unsafe` 边界全部在 `bbqueue`
//! 内部（存储指针、授权切片与 `Send` 实现），审计记录见 `docs/m3-t1-engine.md`。
//!
//! 与 `rtrb` 实现（[`crate::ring`]）语义相同，放在同一接口（[`FrameSink`] / [`FrameSource`]）
//! 之后：
//! - 生产端只按完整帧提交：直接 pack 进写授权，不需要 pack 暂存区；
//! - BipBuffer 的授权总是连续的。尾部剩余空间不足一帧时，跳过尾部、从头部授权（`bbqueue`
//!   记录水位线，消费端读到水位线即回到头部），因此读授权的边界也总是帧边界；
//! - 消费端每次操作至多两次读授权，有界、不分配、不阻塞，可在 DMA 半满/全满中断中调用；
//! - 已入队帧数用两个计数器求差：生产端独写“已入队帧”，消费端独写“已取走帧”（只用原子
//!   load/store，不需要读改写）。
//!
//! 适用于有 CAS 原子的目标（如 Cortex-M4/M7 的 `thumbv7em`）；没有 CAS 的目标（如 Cortex-M0+）
//! 需要 `bbqueue` 的临界区协调，届时另行加入。

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use bbqueue::BBQueue;
use bbqueue::traits::coordination::WriteGrantError;
use bbqueue::traits::coordination::cas::AtomicCoord;
use bbqueue::traits::notifier::polling::Polling;
use bbqueue::traits::storage::Inline;

use crate::DeviceFormat;
use crate::pack::{ChannelMode, Exact, Pcm, pack_with};
use crate::queue::{FrameSink, FrameSource, RingError};

type Bbq<const N: usize> = BBQueue<Inline<N>, AtomicCoord, Polling>;

/// 静态帧队列：`N` 字节存储。放在 `static` 中，用 [`StaticFrameQueue::split`] 取得两端（只能一次）。
pub struct StaticFrameQueue<const N: usize> {
    bbq: Bbq<N>,
    split: AtomicBool,
    /// 生产端累计入队帧数（回绕计数，只由生产端写）。
    pushed: AtomicUsize,
    /// 消费端累计取走帧数（回绕计数，只由消费端写）。
    consumed: AtomicUsize,
}

impl<const N: usize> Default for StaticFrameQueue<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> StaticFrameQueue<N> {
    pub const fn new() -> Self {
        Self {
            bbq: Bbq::new(),
            split: AtomicBool::new(false),
            pushed: AtomicUsize::new(0),
            consumed: AtomicUsize::new(0),
        }
    }

    /// 存储字节数。
    pub const fn capacity(&self) -> usize {
        N
    }

    /// 取得生产端与消费端；第二次调用返回 `None`（保证单生产者、单消费者）。
    pub fn split(&'static self) -> Option<(StaticProducer<N>, StaticConsumer<N>)> {
        if self.split.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some((
            StaticProducer {
                q: self,
                frame_bytes: 0,
                generation: 0,
                pushed: 0,
            },
            StaticConsumer {
                q: self,
                frame_bytes: 0,
                generation: 0,
                consumed: 0,
            },
        ))
    }
}

/// 生产端（非实时：解码 + pack 后入队）。
pub struct StaticProducer<const N: usize> {
    q: &'static StaticFrameQueue<N>,
    frame_bytes: usize,
    generation: u64,
    pushed: usize,
}

/// 消费端（DMA 中断 / 音频回调：只做有界搬运）。
pub struct StaticConsumer<const N: usize> {
    q: &'static StaticFrameQueue<N>,
    frame_bytes: usize,
    generation: u64,
    consumed: usize,
}

/// 设备重配置：核对两端属于同一队列、容量满足 `required_bytes`（宿主按缓冲预算计算）、
/// 代次递增，然后丢弃旧数据并设置新帧宽。必须在两端都停止使用时调用（两端均为 `&mut`）。
pub fn reconfigure<const N: usize>(
    producer: &mut StaticProducer<N>,
    consumer: &mut StaticConsumer<N>,
    format: &DeviceFormat,
    required_bytes: usize,
    generation: u64,
) -> Result<(), RingError> {
    if !core::ptr::eq(producer.q, consumer.q) {
        return Err(RingError::Mismatched);
    }
    if generation <= producer.generation || generation <= consumer.generation {
        return Err(RingError::StaleGeneration);
    }
    // BipBuffer 的空闲空间可能分成尾部与头部两段，每段都不足一帧时无法再写：队列满时已入队的
    // 数据至少为 N − 2×帧宽，因此按这一保证量核对预算。
    let frame_bytes = usize::from(format.block_align());
    let usable = N.saturating_sub(crate::queue::BIP_MARGIN_FRAMES * frame_bytes);
    if required_bytes > usable {
        return Err(RingError::CapacityExceeded {
            required: required_bytes,
            capacity: usable,
        });
    }
    // 丢弃旧数据：已提交数据至多分为两段（水位线前与回绕后），第三次读取必为空。
    let cons = consumer.q.bbq.stream_consumer();
    for _ in 0..3 {
        match cons.read() {
            Ok(g) => {
                let n = g.len();
                g.release(n);
            }
            Err(_) => break,
        }
    }
    producer.frame_bytes = frame_bytes;
    producer.generation = generation;
    producer.pushed = 0;
    consumer.frame_bytes = frame_bytes;
    consumer.generation = generation;
    consumer.consumed = 0;
    producer.q.pushed.store(0, Ordering::Release);
    producer.q.consumed.store(0, Ordering::Release);
    Ok(())
}

impl<const N: usize> StaticProducer<N> {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }

    /// 把精确 PCM 中能放入的完整帧直接 pack 进队列存储，返回入队帧数；队列满时返回 0。
    pub fn pack_push(
        &mut self,
        pcm: &Pcm<'_, Exact>,
        device: &DeviceFormat,
    ) -> Result<usize, RingError> {
        self.pack_push_with(pcm, device, ChannelMode::Exact)
    }

    /// 同 [`StaticProducer::pack_push`]，声道处理方式由 `mode` 给出（[`ChannelMode`]）。
    pub fn pack_push_with(
        &mut self,
        pcm: &Pcm<'_, Exact>,
        device: &DeviceFormat,
        mode: ChannelMode,
    ) -> Result<usize, RingError> {
        let fb = self.frame_bytes;
        if fb == 0 {
            return Err(RingError::NotConfigured);
        }
        if usize::from(device.block_align()) != fb {
            return Err(RingError::FrameWidth);
        }
        let want = pcm.frames().saturating_mul(fb);
        if want == 0 {
            return Ok(0);
        }
        let prod = self.q.bbq.stream_producer();
        let mut grant = match prod.grant_max_remaining(want) {
            Ok(g) if g.len() >= fb => g,
            // 尾部不足一帧（或被读位置挡住）：放弃该授权，改为从头部授权一帧。
            Ok(g) => {
                g.commit(0);
                match prod.grant_exact(fb) {
                    Ok(g) => g,
                    Err(WriteGrantError::InsufficientSize) => return Ok(0),
                    Err(_) => return Err(RingError::Busy),
                }
            }
            Err(WriteGrantError::InsufficientSize) => match prod.grant_exact(fb) {
                Ok(g) => g,
                Err(WriteGrantError::InsufficientSize) => return Ok(0),
                Err(_) => return Err(RingError::Busy),
            },
            Err(_) => return Err(RingError::Busy),
        };
        let room = grant.len() / fb * fb;
        let packed = match pack_with(pcm, device, mode, &mut grant[..room]) {
            Ok(n) => n,
            Err(e) => {
                grant.commit(0);
                return Err(RingError::Pack(e));
            }
        };
        grant.commit(packed * fb);
        self.pushed = self.pushed.wrapping_add(packed);
        self.q.pushed.store(self.pushed, Ordering::Release);
        Ok(packed)
    }

    /// 已入队、尚未取走的帧数（消费端可能同时取走，结果只会偏大）。
    pub fn queued_frames(&self) -> usize {
        self.pushed
            .wrapping_sub(self.q.consumed.load(Ordering::Acquire))
    }
}

impl<const N: usize> FrameSink for StaticProducer<N> {
    type Error = RingError;

    fn push(
        &mut self,
        pcm: &Pcm<'_, Exact>,
        device: &DeviceFormat,
        mode: ChannelMode,
        _scratch: &mut [u8],
    ) -> Result<usize, RingError> {
        self.pack_push_with(pcm, device, mode)
    }

    fn queued_frames(&self) -> usize {
        StaticProducer::queued_frames(self)
    }
}

impl<const N: usize> StaticConsumer<N> {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn take(&mut self, frames: usize) {
        self.consumed = self.consumed.wrapping_add(frames);
        self.q.consumed.store(self.consumed, Ordering::Release);
    }
}

impl<const N: usize> FrameSource for StaticConsumer<N> {
    type Error = RingError;

    fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }

    fn available_frames(&self) -> usize {
        self.q
            .pushed
            .load(Ordering::Acquire)
            .wrapping_sub(self.consumed)
    }

    /// 至多两次读授权（水位线前一段、回绕后一段），每次只拷贝完整帧。
    fn pop_frames(&mut self, out: &mut [u8]) -> Result<usize, RingError> {
        let fb = self.frame_bytes;
        if fb == 0 {
            return Err(RingError::NotConfigured);
        }
        let mut done = 0;
        let cons = self.q.bbq.stream_consumer();
        for _ in 0..2 {
            let room = (out.len() - done * fb) / fb;
            if room == 0 {
                break;
            }
            let grant = match cons.read() {
                Ok(g) => g,
                Err(bbqueue::traits::coordination::ReadGrantError::Empty) => break,
                Err(_) => return Err(RingError::Busy),
            };
            let frames = (grant.len() / fb).min(room);
            let n = frames * fb;
            out[done * fb..done * fb + n].copy_from_slice(&grant[..n]);
            grant.release(n);
            done += frames;
            if frames == 0 {
                break;
            }
        }
        self.take(done);
        Ok(done)
    }

    fn consume_contiguous(
        &mut self,
        max_frames: usize,
        write: impl FnOnce(&[u8]) -> usize,
    ) -> Result<usize, RingError> {
        let fb = self.frame_bytes;
        if fb == 0 {
            return Err(RingError::NotConfigured);
        }
        let cons = self.q.bbq.stream_consumer();
        let grant = match cons.read() {
            Ok(g) => g,
            Err(bbqueue::traits::coordination::ReadGrantError::Empty) => return Ok(0),
            Err(_) => return Err(RingError::Busy),
        };
        let given = (grant.len() / fb).min(max_frames);
        if given == 0 {
            grant.release(0);
            return Ok(0);
        }
        let done = write(&grant[..given * fb]).min(given);
        grant.release(done * fb);
        self.take(done);
        Ok(done)
    }
}

#[cfg(test)]
mod tests;
