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

pub mod assembly;
pub mod backend;
pub mod player;
pub mod render;
pub mod report;
pub mod signal;

#[cfg(any(test, feature = "test-support"))]
pub mod testing;

pub use assembly::{Builtin, Formats, Probe, TrackInfo};
pub use backend::{Backend, PendingStream, RunningStream};
pub use player::{
    Command, CommandReceiver, CommandSender, PlayControl, STATUS_INTERVAL, Status, TrackSource,
    command_channel, play, play_source, play_with,
};
pub use report::{
    AppliedParams, AttemptRecord, Evidence, Negotiation, OpenFailure, PhysicalSpeakers,
    PlaybackReport, RateProbe, StreamReport, Submission, encoding_text, fault_text,
    lossy_track_text, source_bits_text,
};

use qy_codec_auto::profile::{self, Profile};
use qy_codec_auto::{AutoDecoder, AutoInitError};
use qy_core::engine::{Engine, EngineConfig, ErrorPolicy, Resources, ReusePolicy};
use qy_core::{Core, Decoder, DeviceFormat, PCM_WORK_SAMPLES};
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

/// 桌面配置档：启用 `flac` 特性时为 `DESKTOP`，否则为 `DESKTOP_WAV`。
pub(crate) const fn desktop_profile() -> Profile {
    #[cfg(feature = "flac")]
    return profile::DESKTOP;
    #[cfg(not(feature = "flac"))]
    return profile::DESKTOP_WAV;
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

/// 解码器给播放器报告的补充信息（实时输出与报告用）。全部方法都有默认实现：组装层接入新格式时
/// 只需实现 `Decoder` 与 `Resources`，本 trait 写一行 `impl Diagnostics for … {}` 即可（X1）。
pub trait Diagnostics {
    /// 当前曲目失败时，追加失败明细，每条一行。
    fn failure_details(&self, out: &mut Vec<String>) {
        let _ = out;
    }

    /// 当前曲目开头跳过的元数据字节数（如 ID3v2 标签），关闭曲目时报告；没有时为 0。
    fn skipped_tag_bytes(&self) -> u64 {
        0
    }

    /// 当前曲目的补充说明（格式的子类型等，如“AIFF-C sowt（小端整数）”），每条一行；文件头解析
    /// 完成后有效。`qybit decode` 输出。
    fn source_notes(&self, out: &mut Vec<String>) {
        let _ = out;
    }
}

impl Diagnostics for AutoDecoder {
    fn skipped_tag_bytes(&self) -> u64 {
        AutoDecoder::skipped_tag_bytes(self)
    }

    fn source_notes(&self, out: &mut Vec<String>) {
        // F5：AIFF-C 的非 NONE 未压缩类型写明类型码。
        if let Some(t) = self.aiff().and_then(|a| a.sample_type())
            && !t.is_plain()
        {
            out.push(format!("样本类型：{}", t.describe()));
        }
    }

    fn failure_details(&self, out: &mut Vec<String>) {
        #[cfg(feature = "flac")]
        if let Some(e) = self.flac_error() {
            out.push(format!("FLAC 错误明细：{e:?}"));
        }
        if let Some(w) = self.wav_detail() {
            out.push(format!("WAV 错误明细：{w}"));
        }
        if let Some(a) = self.aiff_detail() {
            out.push(format!("AIFF 错误明细：{a}"));
        }
        if let Some(u) = self.unrecognized() {
            out.push(format!("无法识别的文件：{u}"));
        }
        if let Some(name) = self.disabled() {
            out.push(format!("本构建未启用 {name} 解码器，按不支持拒绝"));
        }
    }
}

/// 宿主长期资源：整程序生命期复用。解码器默认为按文件头选择格式的 [`AutoDecoder`]；组装层可以
/// 换成自己的解码器（如把新格式与 `AutoDecoder` 组合，X1），见 [`HostResources::with_decoder`]。
pub struct HostResources<D: Decoder + Resources = AutoDecoder> {
    /// 播放引擎（M3 T1）：持有核心、队列生产端与 pack 暂存区。
    pub engine: Engine<D, FrameProducer>,
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
        Self::with_profile(desktop_profile())
    }

    /// 按配置档一次性分配，只组装配置档启用且本构建已链接的部分（B1）：WAV 与 AIFF 共用的流式
    /// 工作区、FLAC 容量（`flac` 特性）、浮点转换工作区（`convert_samples` 为 0 时不提供）与
    /// 队列大小（`caps.queue_bytes`）。设备复用策略为 [`ReusePolicy::Accepts`]，出错即停止。
    /// 未组装的格式按“不支持”拒绝。
    pub fn with_profile(profile: Profile) -> Result<Self, HostError> {
        let decoder = decoder_for(&profile)?;
        let window = decoder.required_window();
        Self::with_decoder(decoder, window, profile)
    }
}

impl<D: Decoder + Resources> HostResources<D> {
    /// 用组装层提供的解码器一次性分配（X1）：输入窗口 `window` 字节（不小于核心最小窗口
    /// [`qy_core::INPUT_WINDOW_BYTES`]，并满足解码器最大输入单元的需要），能力声明、队列大小与
    /// 浮点转换工作区取自 `profile`（配置档中的格式开关与 FLAC 容量此时不起作用，由解码器自己
    /// 决定）。设备复用策略为 [`ReusePolicy::Accepts`]，出错即停止。
    pub fn with_decoder(decoder: D, window: usize, profile: Profile) -> Result<Self, HostError> {
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
