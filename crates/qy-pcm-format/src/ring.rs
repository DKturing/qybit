//! 已 pack 设备帧字节的 SPSC 队列（宿主专用，需要 alloc）。
//!
//! 底层使用成熟的 `rtrb` 环形缓冲（AGENTS §2/§4：不自行编写无锁缓冲）。
//! 队列在宿主启动时分配一次；每次设备重配置通过 [`reconfigure`] 核对帧宽、
//! 容量与代次并清空旧数据，换曲不分配新队列（M0 T1）。
//!
//! 生产端只按完整帧入队，消费端只按完整帧出队；环形回绕时最多两段拷贝。
//! 消费端不做位移、字节序或样本格式转换。

use crate::{DeviceFormat, MAX_CHANNELS, MAX_SAMPLE_RATE, frames_for_millis};

/// 预填 / 队列预算：当前采样率下 500 ms（M0 T1）。
pub const BUFFER_MILLIS: u32 = 500;
/// 最大设备帧字节数：最多 2 声道 × C32。
pub const MAX_FRAME_BYTES: usize = MAX_CHANNELS as usize * 4;
/// 默认队列容量：192 kHz 下 500 ms 的双声道 C32 帧，即 96000 帧 = 768000 字节。
pub const DEFAULT_CAPACITY_BYTES: usize =
    frames_for_millis(MAX_SAMPLE_RATE, BUFFER_MILLIS) as usize * MAX_FRAME_BYTES;

/// 启动前预填目标：当前采样率下 500 ms 的帧数（向上取整）。
/// 短曲目由宿主预填全部有效帧并明确 EOF。
pub fn prefill_frames(format: &DeviceFormat) -> u64 {
    frames_for_millis(format.sample_rate(), BUFFER_MILLIS)
}

/// 队列错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingError {
    /// 尚未用 [`reconfigure`] 配置帧宽与代次。
    NotConfigured,
    /// 写入的字节数不是完整帧的整数倍。
    PartialFrame,
    /// 队列容量不足以容纳当前格式下的缓冲预算。
    CapacityExceeded { required: usize, capacity: usize },
    /// 新代次必须严格大于当前代次。
    StaleGeneration,
    /// 生产端与消费端不属于同一个队列。
    Mismatched,
}

/// 生产端（非实时线程：解码 + pack 后入队）。
pub struct FrameProducer {
    inner: rtrb::Producer<u8>,
    frame_bytes: usize,
    generation: u64,
}

/// 消费端（音频回调 / 渲染线程：只做有界搬运）。
pub struct FrameConsumer {
    inner: rtrb::Consumer<u8>,
    frame_bytes: usize,
    generation: u64,
}

/// 分配队列。宿主只在启动时调用一次。
pub fn frame_ring(capacity_bytes: usize) -> (FrameProducer, FrameConsumer) {
    let (p, c) = rtrb::RingBuffer::new(capacity_bytes);
    (
        FrameProducer {
            inner: p,
            frame_bytes: 0,
            generation: 0,
        },
        FrameConsumer {
            inner: c,
            frame_bytes: 0,
            generation: 0,
        },
    )
}

/// 当前格式下缓冲预算所需字节数（500 ms，向上取整到帧）。
pub fn required_bytes(format: &DeviceFormat) -> usize {
    usize::try_from(prefill_frames(format))
        .ok()
        .and_then(|f| f.checked_mul(usize::from(format.block_align())))
        .unwrap_or(usize::MAX)
}

/// 设备重配置：核对两端属于同一队列、容量满足预算、代次递增，
/// 然后丢弃旧代次的残留字节并设置新帧宽。必须在两端都停止使用时调用。
pub fn reconfigure(
    producer: &mut FrameProducer,
    consumer: &mut FrameConsumer,
    format: &DeviceFormat,
    generation: u64,
) -> Result<(), RingError> {
    if !core::ptr::eq(producer.inner.buffer(), consumer.inner.buffer()) {
        return Err(RingError::Mismatched);
    }
    if generation <= producer.generation || generation <= consumer.generation {
        return Err(RingError::StaleGeneration);
    }
    let capacity = producer.inner.buffer().capacity();
    let required = required_bytes(format);
    if required > capacity {
        return Err(RingError::CapacityExceeded { required, capacity });
    }
    let stale = consumer.inner.slots();
    if let Ok(chunk) = consumer.inner.read_chunk(stale) {
        chunk.commit_all();
    }
    let frame_bytes = usize::from(format.block_align());
    producer.frame_bytes = frame_bytes;
    producer.generation = generation;
    consumer.frame_bytes = frame_bytes;
    consumer.generation = generation;
    Ok(())
}

impl FrameProducer {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }

    /// 可写入的完整帧数。
    pub fn free_frames(&self) -> usize {
        self.inner
            .slots()
            .checked_div(self.frame_bytes)
            .unwrap_or(0)
    }

    /// 写入尽可能多的完整帧，返回写入帧数；`bytes` 必须是完整帧。
    pub fn push_frames(&mut self, bytes: &[u8]) -> Result<usize, RingError> {
        let fb = self.frame_bytes;
        if fb == 0 {
            return Err(RingError::NotConfigured);
        }
        if bytes.len() % fb != 0 {
            return Err(RingError::PartialFrame);
        }
        let slots = self.inner.slots();
        let frames = (bytes.len() / fb).min(slots / fb);
        let n = frames * fb;
        if n > 0 {
            let mut chunk = self
                .inner
                .write_chunk(n)
                .map_err(|_| RingError::CapacityExceeded {
                    required: n,
                    capacity: slots,
                })?;
            let (a, b) = chunk.as_mut_slices();
            let split = a.len();
            a.copy_from_slice(&bytes[..split]);
            b.copy_from_slice(&bytes[split..n]);
            chunk.commit_all();
        }
        Ok(frames)
    }
}

impl FrameConsumer {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }

    /// 已入队的完整帧数。
    pub fn available_frames(&self) -> usize {
        self.inner
            .slots()
            .checked_div(self.frame_bytes)
            .unwrap_or(0)
    }

    /// 取出至多 `out.len() / frame_bytes` 个完整帧，返回帧数。
    /// 有界操作：最多两段 `copy_from_slice`，不分配、不阻塞。
    pub fn pop_frames(&mut self, out: &mut [u8]) -> Result<usize, RingError> {
        let fb = self.frame_bytes;
        if fb == 0 {
            return Err(RingError::NotConfigured);
        }
        let slots = self.inner.slots();
        let frames = (out.len() / fb).min(slots / fb);
        let n = frames * fb;
        if n > 0 {
            let chunk = self
                .inner
                .read_chunk(n)
                .map_err(|_| RingError::CapacityExceeded {
                    required: n,
                    capacity: slots,
                })?;
            let (a, b) = chunk.as_slices();
            let split = a.len();
            out[..split].copy_from_slice(a);
            out[split..n].copy_from_slice(b);
            chunk.commit_all();
        }
        Ok(frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Endian, Placement};
    use alloc::vec;

    fn fmt(rate: u32, ch: u16, c: u8, v: u8) -> DeviceFormat {
        match DeviceFormat::new(rate, ch, c, v, Endian::Little, Placement::MsbAligned) {
            Ok(f) => f,
            Err(e) => panic!("{e:?}"),
        }
    }

    #[test]
    fn default_capacity_is_half_second_at_192k_stereo_c32() {
        assert_eq!(DEFAULT_CAPACITY_BYTES, 768_000);
        assert_eq!(required_bytes(&fmt(192_000, 2, 32, 32)), 768_000);
    }

    #[test]
    fn reconfigure_checks_capacity_generation_and_pairing() {
        let (mut p, mut c) = frame_ring(DEFAULT_CAPACITY_BYTES);
        assert_eq!(p.push_frames(&[0; 4]), Err(RingError::NotConfigured));
        assert_eq!(
            reconfigure(&mut p, &mut c, &fmt(192_000, 2, 32, 24), 1),
            Ok(())
        );
        assert_eq!(
            reconfigure(&mut p, &mut c, &fmt(48_000, 2, 16, 16), 1),
            Err(RingError::StaleGeneration)
        );
        let (mut small_p, mut small_c) = frame_ring(1000);
        assert_eq!(
            reconfigure(&mut small_p, &mut small_c, &fmt(48_000, 2, 16, 16), 1),
            Err(RingError::CapacityExceeded {
                required: 96_000,
                capacity: 1000
            })
        );
        assert_eq!(
            reconfigure(&mut p, &mut small_c, &fmt(48_000, 2, 16, 16), 2),
            Err(RingError::Mismatched)
        );
    }

    #[test]
    fn whole_frames_and_wraparound() {
        let (mut p, mut c) = frame_ring(DEFAULT_CAPACITY_BYTES);
        assert_eq!(
            reconfigure(&mut p, &mut c, &fmt(44_100, 2, 16, 16), 1),
            Ok(())
        );
        assert_eq!(p.push_frames(&[1, 2, 3]), Err(RingError::PartialFrame));

        // 容量 10 字节、帧宽 4：最多 2 帧在队列中，反复写读强制回绕。
        // 直接设置帧宽以绕过 500 ms 预算检查。
        let (mut p, mut c) = frame_ring(10);
        p.frame_bytes = 4;
        c.frame_bytes = 4;
        let mut out = [0u8; 12];
        for round in 0u8..10 {
            let data: vec::Vec<u8> = (0..12)
                .map(|i| round.wrapping_mul(31).wrapping_add(i))
                .collect();
            assert_eq!(p.push_frames(&data), Ok(2));
            assert_eq!(c.available_frames(), 2);
            assert_eq!(c.pop_frames(&mut out), Ok(2));
            assert_eq!(&out[..8], &data[..8]);
        }
        assert_eq!(c.pop_frames(&mut out), Ok(0));
    }

    #[test]
    fn reconfigure_discards_old_generation_bytes() {
        let (mut p, mut c) = frame_ring(DEFAULT_CAPACITY_BYTES);
        assert_eq!(
            reconfigure(&mut p, &mut c, &fmt(48_000, 2, 16, 16), 1),
            Ok(())
        );
        assert_eq!(p.push_frames(&[7; 40]), Ok(10));
        assert_eq!(
            reconfigure(&mut p, &mut c, &fmt(96_000, 2, 32, 24), 2),
            Ok(())
        );
        assert_eq!(c.available_frames(), 0);
        assert_eq!(c.frame_bytes(), 8);
        assert_eq!(c.generation(), 2);
    }
}
