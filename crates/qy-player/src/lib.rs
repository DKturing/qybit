//! QyBit 平台无关播放器层（M2 T1）。
//!
//! 各平台宿主（WASAPI、ALSA）共用的部分：
//! - [`HostResources`]：输入窗口、解码工作区、pack 暂存区与设备帧队列按配置档（桌面为
//!   `qy_codec_auto::profile::DESKTOP`）在启动时一次性分配，整程序复用（与模拟宿主同一契约）；
//! - [`player`]：生产端播放驱动：执行 `qy_core::engine` 的宿主操作（读文件、开关设备），
//!   播放规则由引擎负责（M3 T1），对输出后端泛型；
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
pub use player::{PlayControl, TrackSource, play, play_source, play_with};
pub use report::{
    AppliedParams, AttemptRecord, Evidence, Negotiation, OpenFailure, PhysicalSpeakers,
    PlaybackReport, RateProbe, StreamReport, Submission, fault_text,
};

use qy_codec_auto::profile::{DESKTOP, Profile};
use qy_codec_auto::{AutoDecoder, AutoInitError};
use qy_core::engine::{Engine, EngineConfig, ErrorPolicy, ReusePolicy};
use qy_core::{Core, DeviceFormat, PCM_WORK_SAMPLES};
use qy_pcm_format::ring::{
    FrameConsumer, FrameProducer, PACK_SCRATCH_BYTES, RingError, frame_ring, reconfigure_for,
};

/// 宿主初始化/重配置错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostError {
    Decoder(AutoInitError),
    Core(qy_core::CoreError),
    Ring(RingError),
    /// 队列消费端仍在渲染线程中。
    ConsumerBusy,
    /// 配置档未启用 FLAC（桌面宿主要求 WAV 与 FLAC 都启用）。
    Profile,
}

/// 一次性分配零初始化切片并泄漏为 `'static`。只在宿主启动时调用。
fn leak_zeroed<T: Copy + Default>(len: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); len].into_boxed_slice())
}

/// 宿主长期资源：整程序生命期复用。
pub struct HostResources {
    /// 播放引擎（M3 T1）：持有核心、队列生产端与 pack 暂存区。
    pub engine: Engine<AutoDecoder, FrameProducer>,
    /// 队列消费端；播放时移交渲染线程，结束后归还。
    pub consumer: Option<FrameConsumer>,
    pub profile: Profile,
    ring_generation: u64,
}

impl HostResources {
    /// 按桌面配置档（[`DESKTOP`]）启动时一次性分配：WAV 工作区、FLAC 桌面容量工作区、
    /// 1 MiB 输入窗口、pack 暂存区与 3 072 000 字节设备帧队列。
    pub fn new() -> Result<Self, HostError> {
        Self::with_profile(DESKTOP)
    }

    /// 按配置档一次性分配。设备复用策略为 [`ReusePolicy::Accepts`]，出错即停止。
    pub fn with_profile(profile: Profile) -> Result<Self, HostError> {
        let cap = profile.flac_capacity().ok_or(HostError::Profile)?;
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
        let (producer, consumer) = frame_ring(profile.queue_storage_bytes());
        let config = EngineConfig {
            caps: profile.caps,
            reuse: ReusePolicy::Accepts,
            on_error: ErrorPolicy::Stop,
        };
        Ok(Self {
            engine: Engine::new(
                core,
                producer,
                leak_zeroed::<u8>(PACK_SCRATCH_BYTES),
                config,
            ),
            consumer: Some(consumer),
            profile,
            ring_generation: 0,
        })
    }

    /// 按设备格式重配置同一队列（新队列代次，丢弃旧数据）。消费端必须在手。
    pub fn configure_ring(&mut self, format: &DeviceFormat) -> Result<(), HostError> {
        let consumer = self.consumer.as_mut().ok_or(HostError::ConsumerBusy)?;
        let next = self.ring_generation + 1;
        let millis = self.engine.config().caps.prefill_ms;
        reconfigure_for(self.engine.sink_mut(), consumer, format, millis, next)
            .map_err(HostError::Ring)?;
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
        let d = r.engine.decoder();
        assert_eq!(
            d.wav().map(|w| w.capacity_samples()),
            Some(PCM_WORK_SAMPLES)
        );
        // F1：桌面配置档用多声道 FLAC 容量（8 声道），窗口为 8 声道不压缩帧上界。
        assert_eq!(
            d.flac().map(|f| f.capacity()),
            Some(qy_codec_flac::Capacity::DESKTOP_MULTICHANNEL)
        );
        assert_eq!(d.required_window(), 2_097_146);
        assert_eq!(r.profile.budget().queue, 3_072_000);
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
