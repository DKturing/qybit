//! QyBit 模拟宿主。
//!
//! 与桌面宿主遵守同一缓冲契约：输入窗口、PCM 工作区、pack 暂存区与设备帧队列在
//! [`SimHost::new`] 中一次性堆分配（`Box::leak` 为 `&'static mut`），停止、换曲与
//! 设备重配置复用，不逐曲分配。
//!
//! [`sim`] 执行真实 `qy-core`、自有解码器（按文件头选择 WAV/FLAC，M1 T3）、pack 与
//! SPSC 队列，只模拟文件、时间与设备（M0 T4）。输入窗口为 FLAC 桌面容量 1 MiB。

pub mod sim;

use qy_codec_auto::{AutoDecoder, AutoInitError};
use qy_codec_flac::Capacity;
use qy_core::{Core, CoreError, DeviceFormat, PCM_WORK_SAMPLES};
use qy_pcm_format::ring::{
    DEFAULT_CAPACITY_BYTES, FrameConsumer, FrameProducer, PACK_SCRATCH_BYTES, RingError,
    frame_ring, reconfigure,
};

/// 宿主初始化错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostError {
    Decoder(AutoInitError),
    Core(CoreError),
    Ring(RingError),
}

/// 一次性分配零初始化切片并泄漏为 `'static`。只在宿主启动时调用。
fn leak_zeroed<T: Copy + Default>(len: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); len].into_boxed_slice())
}

/// 模拟宿主持有的长期资源。
pub struct SimHost {
    pub core: Core<AutoDecoder>,
    /// 生产端 pack 暂存区（启动时分配一次）。
    pub pack_scratch: &'static mut [u8],
    pub producer: FrameProducer,
    pub consumer: FrameConsumer,
    ring_generation: u64,
    regions: [(usize, usize); 5],
}

/// 切片占用的地址区间 `[起始, 结束)`，仅用于检查实例间工作区不重叠。
fn region<T>(s: &[T]) -> (usize, usize) {
    let start = s.as_ptr() as usize;
    (start, start + core::mem::size_of_val(s))
}

impl SimHost {
    /// 启动时分配全部大型缓冲（FLAC 桌面容量）。
    pub fn new() -> Result<Self, HostError> {
        Self::with_capacity(Capacity::DESKTOP)
    }

    /// 按给定 FLAC 容量一次性分配；每个实例拥有独立、不重叠的工作区。
    pub fn with_capacity(cap: Capacity) -> Result<Self, HostError> {
        let wav_pcm = leak_zeroed::<i32>(PCM_WORK_SAMPLES);
        let workspace = leak_zeroed::<i64>(cap.workspace_len());
        let flac_pcm = leak_zeroed::<i32>(cap.pcm_len());
        let input = leak_zeroed::<u8>(cap.input_window);
        let pack_scratch = leak_zeroed::<u8>(PACK_SCRATCH_BYTES);
        let regions = [
            region(wav_pcm),
            region(workspace),
            region(flac_pcm),
            region(input),
            region(pack_scratch),
        ];
        let decoder =
            AutoDecoder::new(wav_pcm, cap, workspace, flac_pcm).map_err(HostError::Decoder)?;
        let window = decoder.required_window();
        let core = Core::with_window(input, window, decoder).map_err(HostError::Core)?;
        let (producer, consumer) = frame_ring(DEFAULT_CAPACITY_BYTES);
        Ok(Self {
            core,
            pack_scratch,
            producer,
            consumer,
            ring_generation: 0,
            regions,
        })
    }

    /// 启动时分配的大型缓冲地址区间（WAV PCM、FLAC 工作区、FLAC PCM、输入窗口、pack
    /// 暂存区），用于并发测试核对各实例互不重叠。
    pub fn buffer_regions(&self) -> [(usize, usize); 5] {
        self.regions
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
