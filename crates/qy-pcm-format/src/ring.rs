//! 已 pack 设备帧字节的 SPSC 队列（宿主专用，需要 alloc）。
//!
//! 底层使用成熟的 `rtrb` 环形缓冲（AGENTS §2/§4：不自行编写无锁缓冲）。
//! 队列在宿主启动时分配一次；每次设备重配置通过 [`reconfigure`] 核对帧宽、
//! 容量与代次并清空旧数据，换曲不分配新队列（M0 T1）。
//!
//! 生产端只按完整帧入队，消费端只按完整帧出队；环形回绕时最多两段拷贝。
//! 消费端不做位移、字节序或样本格式转换。

use crate::pack::{ChannelMode, Exact, Pcm, pack_with};
pub use crate::queue::RingError;
use crate::queue::{FrameSink, FrameSource};
use crate::{DeviceFormat, frames_for_millis};

/// 预填 / 队列预算：当前采样率下 500 ms（M0 T1）。
pub const BUFFER_MILLIS: u32 = 500;
pub use crate::queue::MAX_FRAME_BYTES;
/// 默认队列预算：192 kHz 下 500 ms 的双声道 C32 帧，即 96000 帧 = 768000 字节。
/// 这是宿主的默认资源配置，不是软件上限：更高采样率由宿主按资源预算分配更大的队列，
/// 预算不足时 [`reconfigure`] 返回容量错误（播放前明确报告）。
pub const DEFAULT_CAPACITY_BYTES: usize =
    frames_for_millis(192_000, BUFFER_MILLIS) as usize * STEREO_C32_FRAME_BYTES;
/// 双声道 C32 一帧的字节数（默认队列预算按此计算）。
const STEREO_C32_FRAME_BYTES: usize = 2 * 4;

/// 启动前预填目标：当前采样率下 500 ms 的帧数（向上取整）。
/// 短曲目由宿主预填全部有效帧并明确 EOF。
pub fn prefill_frames(format: &DeviceFormat) -> u64 {
    frames_for_millis(format.sample_rate(), BUFFER_MILLIS)
}

pub use crate::queue::PACK_SCRATCH_BYTES;

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
    required_bytes_for(format, BUFFER_MILLIS)
}

/// 当前格式下 `millis` 毫秒缓冲预算所需字节数（向上取整到帧）。
pub fn required_bytes_for(format: &DeviceFormat, millis: u32) -> usize {
    usize::try_from(frames_for_millis(format.sample_rate(), millis))
        .ok()
        .and_then(|f| f.checked_mul(usize::from(format.block_align())))
        .unwrap_or(usize::MAX)
}

/// 设备重配置（缓冲预算 500 ms）：见 [`reconfigure_for`]。
pub fn reconfigure(
    producer: &mut FrameProducer,
    consumer: &mut FrameConsumer,
    format: &DeviceFormat,
    generation: u64,
) -> Result<(), RingError> {
    reconfigure_for(producer, consumer, format, BUFFER_MILLIS, generation)
}

/// 设备重配置：核对两端属于同一队列、容量满足 `millis` 毫秒的预算（宿主配置档的预填时长）、
/// 代次递增，然后丢弃旧代次的残留字节并设置新帧宽。必须在两端都停止使用时调用。
pub fn reconfigure_for(
    producer: &mut FrameProducer,
    consumer: &mut FrameConsumer,
    format: &DeviceFormat,
    millis: u32,
    generation: u64,
) -> Result<(), RingError> {
    if !core::ptr::eq(producer.inner.buffer(), consumer.inner.buffer()) {
        return Err(RingError::Mismatched);
    }
    if generation <= producer.generation || generation <= consumer.generation {
        return Err(RingError::StaleGeneration);
    }
    let capacity = producer.inner.buffer().capacity();
    let required = required_bytes_for(format, millis);
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

    /// 生产端：把精确 PCM 中能放入队列的完整帧 pack 到 `scratch` 后入队。
    /// 返回入队帧数，调用方据此从解码器释放同样帧数；队列满时返回 0。
    pub fn pack_push(
        &mut self,
        pcm: &Pcm<'_, Exact>,
        device: &DeviceFormat,
        scratch: &mut [u8],
    ) -> Result<usize, RingError> {
        self.pack_push_with(pcm, device, ChannelMode::Exact, scratch)
    }

    /// 同 [`FrameProducer::pack_push`]，声道处理方式由 `mode` 给出（[`ChannelMode`]）。
    pub fn pack_push_with(
        &mut self,
        pcm: &Pcm<'_, Exact>,
        device: &DeviceFormat,
        mode: ChannelMode,
        scratch: &mut [u8],
    ) -> Result<usize, RingError> {
        let fb = self.frame_bytes;
        if fb == 0 {
            return Err(RingError::NotConfigured);
        }
        if usize::from(device.block_align()) != fb {
            return Err(RingError::FrameWidth);
        }
        let room = self.free_frames().min(scratch.len() / fb);
        let frames = pcm.frames().min(room);
        let packed =
            pack_with(pcm, device, mode, &mut scratch[..frames * fb]).map_err(RingError::Pack)?;
        let pushed = self.push_frames(&scratch[..packed * fb])?;
        debug_assert_eq!(pushed, packed);
        Ok(pushed)
    }

    pub fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }

    /// 生产端视角的已入队完整帧数（消费端可能同时取走，结果只会偏大）。
    pub fn queued_frames_hint(&self) -> usize {
        let capacity = self.inner.buffer().capacity();
        capacity
            .saturating_sub(self.inner.slots())
            .checked_div(self.frame_bytes)
            .unwrap_or(0)
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

impl FrameConsumer {
    /// 短写后端：把队列首部一段连续的完整帧（至多 `max_frames` 帧）交给 `write`，
    /// 按其返回的帧数提交；未写出的帧留在队列中，下次从同一位置继续，不重复、不丢弃。
    /// 返回提交的帧数（`write` 返回值超过所给帧数时按所给帧数截断）。
    ///
    /// 不拷贝：直接借出环形缓冲的第一段。若回绕点恰好切开一个帧，则把这一帧拼到栈上的
    /// [`MAX_FRAME_BYTES`] 字节临时区再交出。有界、不分配、不阻塞。
    pub fn consume_contiguous(
        &mut self,
        max_frames: usize,
        write: impl FnOnce(&[u8]) -> usize,
    ) -> Result<usize, RingError> {
        let fb = self.frame_bytes;
        if fb == 0 {
            return Err(RingError::NotConfigured);
        }
        if fb > MAX_FRAME_BYTES {
            return Err(RingError::FrameWidth);
        }
        let slots = self.inner.slots();
        let frames = (slots / fb).min(max_frames);
        if frames == 0 {
            return Ok(0);
        }
        let n = frames * fb;
        let chunk = self
            .inner
            .read_chunk(n)
            .map_err(|_| RingError::CapacityExceeded {
                required: n,
                capacity: slots,
            })?;
        let (a, b) = chunk.as_slices();
        let given = a.len() / fb;
        let done = if given > 0 {
            write(&a[..given * fb]).min(given)
        } else {
            // 回绕点切开首帧：拼出这一帧。
            let mut tmp = [0u8; MAX_FRAME_BYTES];
            let split = a.len();
            tmp[..split].copy_from_slice(a);
            tmp[split..fb].copy_from_slice(&b[..fb - split]);
            write(&tmp[..fb]).min(1)
        };
        chunk.commit(done * fb);
        Ok(done)
    }
}

impl FrameSink for FrameProducer {
    type Error = RingError;

    fn push(
        &mut self,
        pcm: &Pcm<'_, Exact>,
        device: &DeviceFormat,
        mode: ChannelMode,
        scratch: &mut [u8],
    ) -> Result<usize, RingError> {
        self.pack_push_with(pcm, device, mode, scratch)
    }

    fn queued_frames(&self) -> usize {
        self.queued_frames_hint()
    }
}

impl FrameSource for FrameConsumer {
    type Error = RingError;

    fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }

    fn available_frames(&self) -> usize {
        FrameConsumer::available_frames(self)
    }

    fn pop_frames(&mut self, out: &mut [u8]) -> Result<usize, RingError> {
        FrameConsumer::pop_frames(self, out)
    }

    fn consume_contiguous(
        &mut self,
        max_frames: usize,
        write: impl FnOnce(&[u8]) -> usize,
    ) -> Result<usize, RingError> {
        FrameConsumer::consume_contiguous(self, max_frames, write)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Endian, Placement};
    use alloc::vec;
    use alloc::vec::Vec;

    fn fmt(rate: u32, ch: u16, c: u8, v: u8) -> DeviceFormat {
        match DeviceFormat::new(rate, ch, c, v, Endian::Little, Placement::MsbAligned) {
            Ok(f) => f,
            Err(e) => panic!("{e:?}"),
        }
    }

    /// 短写：部分提交保留余下帧；回绕切开帧时拼帧交出；字节序列与入队完全一致。
    #[test]
    fn consume_contiguous_partial_commit_and_split_frame() {
        // 24000 字节容量（8 kHz 双声道 C24 的 500 ms）：把读位置推进到 23998，再换 6 字节帧，
        // 首帧被回绕切开。
        let (mut p, mut c) = frame_ring(24_000);
        let f4 = fmt(8_000, 2, 16, 16);
        let f6 = fmt(8_000, 2, 24, 24);
        assert_eq!(reconfigure(&mut p, &mut c, &f4, 1), Ok(()));
        assert_eq!(c.consume_contiguous(1, |_| 1), Ok(0));
        let filler = vec![0u8; 23_996];
        assert_eq!(p.push_frames(&filler), Ok(5_999));
        let mut sink = vec![0u8; 23_996];
        assert_eq!(c.pop_frames(&mut sink), Ok(5_999));
        // 读位置 23996：以 2 字节单声道帧再推进 2 字节。
        assert_eq!(
            reconfigure(&mut p, &mut c, &fmt(8_000, 1, 16, 16), 2),
            Ok(())
        );
        assert_eq!(p.push_frames(&[0, 0]), Ok(1));
        assert_eq!(c.pop_frames(&mut [0u8; 2]), Ok(1));
        // 位置 23998：6 字节帧跨越回绕点（2 + 4）。
        assert_eq!(reconfigure(&mut p, &mut c, &f6, 3), Ok(()));
        let data: Vec<u8> = (0..60u8).collect();
        assert_eq!(p.push_frames(&data), Ok(10));
        let mut out = Vec::new();
        // 第一次：首帧被切开，只交出 1 帧。
        let n = c.consume_contiguous(10, |bytes| {
            assert_eq!(bytes.len(), 6);
            out.extend_from_slice(bytes);
            1
        });
        assert_eq!(n, Ok(1));
        // 第二次：连续 9 帧，只“写出” 4 帧，其余保留。
        let n = c.consume_contiguous(10, |bytes| {
            assert_eq!(bytes.len(), 54);
            out.extend_from_slice(&bytes[..24]);
            4
        });
        assert_eq!(n, Ok(4));
        assert_eq!(c.available_frames(), 5);
        // 写出 0 帧：不提交。
        assert_eq!(c.consume_contiguous(10, |_| 0), Ok(0));
        // 返回值超出所给帧数时截断。
        let n = c.consume_contiguous(2, |bytes| {
            out.extend_from_slice(bytes);
            99
        });
        assert_eq!(n, Ok(2));
        let n = c.consume_contiguous(usize::MAX, |bytes| {
            out.extend_from_slice(bytes);
            bytes.len() / 6
        });
        assert_eq!(n, Ok(3));
        assert_eq!(out, data);
        assert_eq!(c.available_frames(), 0);
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
