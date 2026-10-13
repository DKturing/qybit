//! QyBit WASAPI 宿主（M0 T5）。
//!
//! - [`HostResources`]：输入窗口、PCM 工作区、pack 暂存区与设备帧队列在启动时一次性分配，
//!   整程序复用（与模拟宿主同一契约）。
//! - [`render`]：渲染线程循环，与平台 API 解耦，可注入假 API 测试。
//! - [`player`]：生产端播放驱动（真实核心、解码与 pack），对输出后端泛型。
//! - `wasapi`（仅 Windows）：WASAPI 独占事件驱动后端与 MMCSS。
//! - `sdk_format`（仅 Windows）：纯格式描述到 SDK 结构的字段映射。

pub mod matrix;
pub mod player;
pub mod render;

#[cfg(windows)]
pub mod endpoints;
#[cfg(windows)]
pub mod sdk_format;
#[cfg(windows)]
pub mod wasapi;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod testutil;

use qy_codec_wav::WavDecoder;
use qy_core::{Core, DeviceFormat, INPUT_WINDOW_BYTES, PCM_WORK_SAMPLES};
use qy_pcm_format::ring::{
    DEFAULT_CAPACITY_BYTES, FrameConsumer, FrameProducer, PACK_SCRATCH_BYTES, RingError,
    frame_ring, reconfigure,
};

/// 宿主初始化/重配置错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostError {
    Decoder(qy_core::DecodeError),
    Core(qy_core::CoreError),
    Ring(RingError),
    /// 队列消费端仍在渲染线程中。
    ConsumerBusy,
}

/// 一次性分配零初始化切片并泄漏为 `'static`。只在宿主启动时调用。
fn leak_zeroed<T: Copy + Default>(len: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); len].into_boxed_slice())
}

/// 宿主长期资源：整程序生命期复用。
pub struct HostResources {
    pub core: Core<WavDecoder>,
    /// 生产端 pack 暂存区（启动时分配一次）。
    pub pack_scratch: &'static mut [u8],
    pub producer: FrameProducer,
    /// 队列消费端；播放时移交渲染线程，结束后归还。
    pub consumer: Option<FrameConsumer>,
    ring_generation: u64,
}

impl HostResources {
    pub fn new() -> Result<Self, HostError> {
        let input = leak_zeroed::<u8>(INPUT_WINDOW_BYTES);
        let decoder =
            WavDecoder::new(leak_zeroed::<i32>(PCM_WORK_SAMPLES)).map_err(HostError::Decoder)?;
        let core = Core::new(input, decoder).map_err(HostError::Core)?;
        let (producer, consumer) = frame_ring(DEFAULT_CAPACITY_BYTES);
        Ok(Self {
            core,
            pack_scratch: leak_zeroed::<u8>(PACK_SCRATCH_BYTES),
            producer,
            consumer: Some(consumer),
            ring_generation: 0,
        })
    }

    /// 按设备格式重配置同一队列（新队列代次，丢弃旧数据）。消费端必须在手。
    pub fn configure_ring(&mut self, format: &DeviceFormat) -> Result<(), HostError> {
        let consumer = self.consumer.as_mut().ok_or(HostError::ConsumerBusy)?;
        let next = self.ring_generation + 1;
        reconfigure(&mut self.producer, consumer, format, next).map_err(HostError::Ring)?;
        self.ring_generation = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resources_have_declared_capacity() {
        let Ok(mut r) = HostResources::new() else {
            panic!("init");
        };
        assert_eq!(r.core.decoder().capacity_samples(), PCM_WORK_SAMPLES);
        let Ok(f) = DeviceFormat::wasapi(192_000, 2, 32, 32) else {
            panic!("fmt");
        };
        assert_eq!(r.configure_ring(&f), Ok(()));
        let c = r.consumer.take();
        assert_eq!(r.configure_ring(&f), Err(HostError::ConsumerBusy));
        r.consumer = c;
        assert_eq!(r.configure_ring(&f), Ok(()));
    }
}
