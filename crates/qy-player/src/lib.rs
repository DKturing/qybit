//! QyBit 平台无关播放器层（M2 T1）。
//!
//! 各平台宿主（WASAPI、ALSA）共用的部分：
//! - [`HostResources`]：输入窗口、解码工作区、pack 暂存区与设备帧队列在启动时一次性分配，
//!   整程序复用（与模拟宿主同一契约）；
//! - [`player`]：生产端播放驱动（真实核心、自有解码与 pack），对输出后端泛型；
//! - [`backend`]：后端 trait；
//! - [`render`]：渲染线程循环，与平台 API 解耦：[`render::packet`] 用于要求整包提交的
//!   后端（WASAPI 独占事件驱动），[`render::write`] 用于支持短写的后端（ALSA `writei`）；
//! - [`report`]：协商与播放报告，区分请求格式、协商结果、可查询的已应用参数与回录证据。
//!
//! `test-support` 特性提供 [`testing`]：夹具、独立参考、假平台 API 与假后端。

pub mod backend;
pub mod player;
pub mod render;
pub mod report;
pub mod signal;

#[cfg(any(test, feature = "test-support"))]
pub mod testing;

pub use backend::{Backend, PendingStream, RunningStream};
pub use player::play;
pub use report::{
    AppliedParams, AttemptRecord, Evidence, Negotiation, OpenFailure, PlaybackReport, StreamReport,
    Submission,
};

use qy_codec_auto::{AutoDecoder, AutoInitError};
use qy_codec_flac::Capacity;
use qy_core::{Core, DeviceFormat, PCM_WORK_SAMPLES};
use qy_pcm_format::ring::{
    DEFAULT_CAPACITY_BYTES, FrameConsumer, FrameProducer, PACK_SCRATCH_BYTES, RingError,
    frame_ring, reconfigure,
};

/// 宿主初始化/重配置错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostError {
    Decoder(AutoInitError),
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
    pub core: Core<AutoDecoder>,
    /// 生产端 pack 暂存区（启动时分配一次）。
    pub pack_scratch: &'static mut [u8],
    pub producer: FrameProducer,
    /// 队列消费端；播放时移交渲染线程，结束后归还。
    pub consumer: Option<FrameConsumer>,
    ring_generation: u64,
}

impl HostResources {
    /// 启动时一次性分配：WAV 工作区、FLAC 桌面容量工作区与 1 MiB 输入窗口。
    pub fn new() -> Result<Self, HostError> {
        let cap = Capacity::DESKTOP;
        let decoder = AutoDecoder::new(
            leak_zeroed::<i32>(PCM_WORK_SAMPLES),
            cap,
            leak_zeroed::<i64>(cap.workspace_len()),
            leak_zeroed::<i32>(cap.pcm_len()),
        )
        .map_err(HostError::Decoder)?;
        let window = decoder.required_window();
        let input = leak_zeroed::<u8>(window);
        let core = Core::with_window(input, window, decoder).map_err(HostError::Core)?;
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
        assert_eq!(r.core.decoder().wav().capacity_samples(), PCM_WORK_SAMPLES);
        assert_eq!(r.core.decoder().flac().capacity(), Capacity::DESKTOP);
        assert_eq!(r.core.decoder().required_window(), 1 << 20);
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
