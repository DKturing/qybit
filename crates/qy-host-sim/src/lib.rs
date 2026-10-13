//! QyFi 模拟宿主。
//!
//! 与桌面宿主遵守同一缓冲契约：输入窗口、PCM 工作区与设备帧队列在
//! [`SimHost::new`] 中一次性堆分配（输入窗口与工作区 `Box::leak` 为
//! `&'static mut`），停止/换曲/设备重配置复用，不逐曲分配。
//!
//! M0 T1 只建立资源骨架；文件/时间/设备模拟与故障注入在 T4 实现。

use qy_codec_wav::WavDecoder;
use qy_core::{DecodeError, DeviceFormat, INPUT_WINDOW_BYTES, PCM_WORK_SAMPLES};
use qy_pcm_format::ring::{
    DEFAULT_CAPACITY_BYTES, FrameConsumer, FrameProducer, PACK_SCRATCH_BYTES, RingError,
    frame_ring, reconfigure,
};

/// 宿主初始化错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostError {
    Decoder(DecodeError),
    Ring(RingError),
}

/// 一次性分配零初始化切片并泄漏为 `'static`。只在宿主启动时调用。
fn leak_zeroed<T: Copy + Default>(len: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); len].into_boxed_slice())
}

/// 模拟宿主持有的长期资源。
pub struct SimHost {
    /// 交给核心管理的输入窗口（T4 接入核心后移交）。
    pub input: Option<&'static mut [u8]>,
    pub decoder: WavDecoder,
    /// 生产端 pack 暂存区（启动时分配一次）。
    pub pack_scratch: &'static mut [u8],
    pub producer: FrameProducer,
    pub consumer: FrameConsumer,
    generation: u64,
}

impl SimHost {
    /// 启动时分配全部大型缓冲。
    pub fn new() -> Result<Self, HostError> {
        let input = leak_zeroed::<u8>(INPUT_WINDOW_BYTES);
        let pcm = leak_zeroed::<i32>(PCM_WORK_SAMPLES);
        let decoder = WavDecoder::new(pcm).map_err(HostError::Decoder)?;
        let (producer, consumer) = frame_ring(DEFAULT_CAPACITY_BYTES);
        Ok(Self {
            input: Some(input),
            decoder,
            pack_scratch: leak_zeroed::<u8>(PACK_SCRATCH_BYTES),
            producer,
            consumer,
            generation: 0,
        })
    }

    /// 设备重配置：复用同一队列，核对帧宽/容量并进入新代次。
    pub fn configure_device(&mut self, format: &DeviceFormat) -> Result<u64, HostError> {
        let next = self.generation + 1;
        reconfigure(&mut self.producer, &mut self.consumer, format, next)
            .map_err(HostError::Ring)?;
        self.generation = next;
        Ok(next)
    }
}
