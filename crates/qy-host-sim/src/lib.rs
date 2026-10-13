//! QyFi 模拟宿主。
//!
//! 与桌面宿主遵守同一缓冲契约：输入窗口、PCM 工作区、pack 暂存区与设备帧队列在
//! [`SimHost::new`] 中一次性堆分配（`Box::leak` 为 `&'static mut`），停止、换曲与
//! 设备重配置复用，不逐曲分配。
//!
//! [`sim`] 执行真实 `qy-core`、`qy-codec-wav`、pack 与 SPSC 队列，只模拟文件、
//! 时间与设备（M0 T4）。

pub mod sim;

use qy_codec_wav::WavDecoder;
use qy_core::{Core, CoreError, DecodeError, DeviceFormat, INPUT_WINDOW_BYTES, PCM_WORK_SAMPLES};
use qy_pcm_format::ring::{
    DEFAULT_CAPACITY_BYTES, FrameConsumer, FrameProducer, PACK_SCRATCH_BYTES, RingError,
    frame_ring, reconfigure,
};

/// 宿主初始化错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostError {
    Decoder(DecodeError),
    Core(CoreError),
    Ring(RingError),
}

/// 一次性分配零初始化切片并泄漏为 `'static`。只在宿主启动时调用。
fn leak_zeroed<T: Copy + Default>(len: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); len].into_boxed_slice())
}

/// 模拟宿主持有的长期资源。
pub struct SimHost {
    pub core: Core<WavDecoder>,
    /// 生产端 pack 暂存区（启动时分配一次）。
    pub pack_scratch: &'static mut [u8],
    pub producer: FrameProducer,
    pub consumer: FrameConsumer,
    ring_generation: u64,
}

impl SimHost {
    /// 启动时分配全部大型缓冲。
    pub fn new() -> Result<Self, HostError> {
        let input = leak_zeroed::<u8>(INPUT_WINDOW_BYTES);
        let pcm = leak_zeroed::<i32>(PCM_WORK_SAMPLES);
        let decoder = WavDecoder::new(pcm).map_err(HostError::Decoder)?;
        let core = Core::new(input, decoder).map_err(HostError::Core)?;
        let (producer, consumer) = frame_ring(DEFAULT_CAPACITY_BYTES);
        Ok(Self {
            core,
            pack_scratch: leak_zeroed::<u8>(PACK_SCRATCH_BYTES),
            producer,
            consumer,
            ring_generation: 0,
        })
    }

    /// 设备重配置：复用同一队列，核对帧宽/容量，进入新队列代次并丢弃旧数据。
    pub fn configure_device(&mut self, format: &DeviceFormat) -> Result<u64, HostError> {
        let next = self.ring_generation + 1;
        reconfigure(&mut self.producer, &mut self.consumer, format, next)
            .map_err(HostError::Ring)?;
        self.ring_generation = next;
        Ok(next)
    }
}
