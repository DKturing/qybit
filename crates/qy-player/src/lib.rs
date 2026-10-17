//! QyBit 平台无关播放器层（M2 T1）。
//!
//! 各平台宿主（WASAPI、ALSA）共用的部分：
//! - [`HostResources`]：输入窗口、解码工作区、pack 暂存区与设备帧队列按配置档（桌面为
//!   `qy_codec_auto::profile::DESKTOP`，不含 `flac` 特性时为 `DESKTOP_WAV`）按需组装、在启动时一次性分配，整程序复用（与模拟宿主同一契约）；
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
pub use player::{
    Command, PlayControl, STATUS_INTERVAL, Status, TrackSource, play, play_source, play_with,
};
pub use report::{
    AppliedParams, AttemptRecord, Evidence, Negotiation, OpenFailure, PhysicalSpeakers,
    PlaybackReport, RateProbe, StreamReport, Submission, encoding_text, fault_text,
    source_bits_text,
};

use qy_codec_auto::profile::{self, Profile};
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
}

/// 一次性分配零初始化切片并泄漏为 `'static`。只在宿主启动时调用。
fn leak_zeroed<T: Copy + Default>(len: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); len].into_boxed_slice())
}

/// 按配置档组装解码器：只加入配置档启用、且本构建已链接的格式。
fn decoder_for(profile: &Profile) -> Result<AutoDecoder, HostError> {
    let mut d = AutoDecoder::empty();
    if profile.wav_enabled() || profile.aiff_enabled() {
        // WAV 与 AIFF 共用一块流式 PCM 工作区（F2）。
        let pcm = leak_zeroed::<i32>(PCM_WORK_SAMPLES);
        d = if profile.wav_enabled() {
            d.with_wav(pcm)
        } else {
            d.with_stream_pcm(pcm)
        }
        .map_err(HostError::Decoder)?;
        if profile.aiff_enabled() {
            d = d.with_aiff().map_err(HostError::Decoder)?;
        }
    }
    #[cfg(feature = "flac")]
    if let Some(cap) = profile.flac_capacity() {
        d = d
            .with_flac(
                cap,
                leak_zeroed::<i64>(cap.workspace_len()),
                leak_zeroed::<i32>(cap.pcm_len()),
            )
            .map_err(HostError::Decoder)?;
    }
    Ok(d)
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
    /// 按桌面配置档启动时一次性分配：启用 `flac` 特性时为 `DESKTOP`（WAV 工作区、FLAC 桌面
    /// 容量工作区、整帧输入窗口），否则为 [`DESKTOP_WAV`](profile::DESKTOP_WAV)（只有 WAV / AIFF）；另有 pack 暂存区与
    /// 3 072 000 字节设备帧队列。
    pub fn new() -> Result<Self, HostError> {
        #[cfg(feature = "flac")]
        return Self::with_profile(profile::DESKTOP);
        #[cfg(not(feature = "flac"))]
        return Self::with_profile(profile::DESKTOP_WAV);
    }

    /// 按配置档一次性分配，只组装配置档启用且本构建已链接的部分（B1）：WAV 与 AIFF 共用的流式
    /// 工作区、FLAC 容量（`flac` 特性）、浮点转换工作区（`convert_samples` 为 0 时不提供）与
    /// 队列大小（`caps.queue_bytes`）。设备复用策略为 [`ReusePolicy::Accepts`]，出错即停止。
    /// 未组装的格式按“不支持”拒绝。
    pub fn with_profile(profile: Profile) -> Result<Self, HostError> {
        let decoder = decoder_for(&profile)?;
        let window = decoder.required_window();
        let input = leak_zeroed::<u8>(window);
        let core = Core::with_window(input, window, decoder).map_err(HostError::Core)?;
        let (producer, consumer) = frame_ring(profile.queue_storage_bytes());
        let config = EngineConfig {
            caps: profile.caps,
            reuse: ReusePolicy::Accepts,
            on_error: ErrorPolicy::Stop,
        };
        let mut engine = Engine::new(
            core,
            producer,
            leak_zeroed::<u8>(PACK_SCRATCH_BYTES),
            config,
        );
        // F3：浮点转整数工作区（明确开启时使用；配置档不提供时开启也按资源不足拒绝）。
        if profile.convert_samples > 0 {
            engine.set_convert_workspace(leak_zeroed::<i32>(profile.convert_samples));
        }
        Ok(Self {
            engine,
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
        #[cfg(feature = "flac")]
        {
            assert_eq!(
                d.flac().map(|f| f.capacity()),
                Some(qy_codec_flac::Capacity::DESKTOP_MULTICHANNEL)
            );
            assert_eq!(d.required_window(), 2_097_146);
        }
        // B1：WAV-only 构建不分配 FLAC 工作区，窗口为核心最小窗口。
        #[cfg(not(feature = "flac"))]
        assert_eq!(d.required_window(), qy_core::INPUT_WINDOW_BYTES);
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
