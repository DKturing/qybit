//! 严格路径之外、明确开启的选项（B1：从播放状态机中隔离）。
//!
//! 当前两项（AGENTS §3 批准，默认关闭）：
//! - 单声道复制到左右两路（O1，[`ChannelPolicy`]）；
//! - 浮点转整数（F3，[`FloatPolicy`]）。
//!
//! 两项的全部状态都在 [`Options`] 中。引擎只经下列调用点使用它，新增选项时按这些调用点接入：
//! 1. **配置开始**：[`Options::begin`] 由源格式得到交给协商的格式（声道策略）；
//!    [`Options::reuse_converted`] 判断能否续用当前整数设备。
//! 2. **协商**：
//!    - [`Options::presentation`] 给出当前回退阶段请求的格式；
//!    - [`Options::check_failed`] 决定能力检查失败后是否改试下一档；
//!    - [`Options::fallback`] 决定设备拒绝后是否改试下一档。
//! 3. **配置完成**：[`Options::configured`] 锁定本曲目的转换位数。
//! 4. **入队**：[`Options::pack_mode`] 与 [`Options::push`]。
//! 5. **报告**：
//!    - [`Options::first_output`] 在每首曲目第一次入队时给出通知；
//!    - [`Options::add_stats`] 累计统计；
//!    - [`Options::take_float_report`] 在关闭、停止或定位时给出统计。
//! 6. **播放开始与停止**：[`Options::new_session`]。

use qy_pcm_format::convert::{ConvertError, float_to_int};
use qy_pcm_format::pack::ChannelMode;

use super::{Capabilities, CapabilityError, ConvertStats, FrameSink, ReusePolicy};
use crate::{DeviceFormat, Pcm, SampleEncoding, SourceFormat};

/// 声道策略（AGENTS §3：严格路径之外只允许任务卡定义、明确开启并在报告中标注的选项）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChannelPolicy {
    /// 严格路径：设备格式与源的扬声器位置相同（默认）。
    #[default]
    Strict,
    /// 明确开启（tasks/O1.md）：单声道 `[FC]` 的源按立体声 `[FL FR]` 打开设备，每个样本原样复制到
    /// 左右两路；样本值不变、不混音。每首这样处理的曲目都发出
    /// [`super::HostAction::ChannelsDuplicated`]。
    DuplicateMono,
}

/// 浮点源的策略（F3，AGENTS §3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FloatPolicy {
    /// 严格路径：浮点源只送到接受同一浮点格式的设备（按位复制），否则按设备不支持拒绝（默认）。
    #[default]
    Strict,
    /// 明确开启（tasks/F3.md §4）：设备不接受源的浮点格式时，依次按 32、24、16 位整数协商，把样本
    /// 转换为设备有效位 V 的整数（样本已改变）。每首这样处理的曲目发出
    /// [`super::HostAction::FloatConverted`]；需要宿主经 [`super::Engine::set_convert_workspace`]
    /// 提供工作区。
    ToInt,
}

/// 浮点转整数回退时依次请求的整数位数（设备支持的有效位最大者优先）。
const FLOAT_FALLBACK_BITS: [u8; 3] = [32, 24, 16];

/// 一首浮点曲目的统计（F3，报告用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloatReport {
    pub encoding: SampleEncoding,
    /// 转换成的整数位数（明确开启的浮点转整数）；`None` 为严格路径按位输出。
    pub converted_to: Option<u8>,
    /// 严格路径按位输出的 NaN 与无穷样本数（原样保留）。
    pub nonfinite: u64,
    /// 转换统计（只在转换时有）。
    pub convert: ConvertStats,
}

/// 一次入队的结果。
pub(super) enum Pushed {
    Frames {
        frames: usize,
        stats: ConvertStats,
        nonfinite: u64,
    },
    /// 转换遇到非有限样本：之前的 `frames` 帧已入队（统计为 `stats`）。
    NonFinite {
        frames: usize,
        stats: ConvertStats,
    },
    Error,
}

/// 能力检查失败后的下一步。
pub(super) enum Next {
    /// 已改为下一档，重新检查。
    Retry,
    /// 按此原因拒绝。
    Reject(CapabilityError),
}

/// 设备拒绝后的下一步。
pub(super) enum Fallback {
    /// 已改为下一档，按 `laid` 重新请求设备。
    Retry(SourceFormat),
    /// 按此原因拒绝（没有转换工作区）。
    Reject(CapabilityError),
    /// 不回退，按配置失败处理。
    None,
}

/// 曲目第一次入队时的通知。
#[derive(Default)]
pub(super) struct Announce {
    /// 声道已改变（O1）。
    pub duplicated: bool,
    /// 样本已改变：浮点转为该位数整数（F3）。
    pub converted: Option<u8>,
    /// 上一首浮点曲目的统计（换到新曲目时先给出）。
    pub finished: Option<(u32, FloatReport)>,
}

/// 两项选项的全部状态。
pub(super) struct Options {
    channels: ChannelPolicy,
    /// 当前曲目配置时是否按 [`ChannelPolicy::DuplicateMono`] 复制声道。按曲目锁定：播放中改变
    /// 策略只影响之后配置的曲目，已打开的设备流不会改用另一种 pack。
    duplicating: bool,
    floats: FloatPolicy,
    /// 浮点转整数的工作区（宿主启动时提供；没有时不能转换）。
    convert_ws: Option<&'static mut [i32]>,
    /// 当前配置把浮点转为几位整数（配置时锁定）；`None` 为不转换。
    converting: Option<u8>,
    /// 正在配置的源格式（声道策略之后）与浮点回退阶段（0 为源浮点格式，k 为
    /// `FLOAT_FALLBACK_BITS[k − 1]` 位整数）。
    configuring: Option<SourceFormat>,
    stage: u8,
    /// 当前设备不接受、已改为转换的浮点源格式（同格式的后续曲目据此续用设备）。只在一次播放
    /// 期间有效：停止与重新开始时清除（下一次播放可能是另一台设备）。
    rejected: Option<SourceFormat>,
    /// 当前输出曲目的浮点统计（第一次入队时开始，关闭时给出）。
    float_track: Option<(u32, FloatReport)>,
}

impl Options {
    pub(super) const fn new() -> Self {
        Self {
            channels: ChannelPolicy::Strict,
            duplicating: false,
            floats: FloatPolicy::Strict,
            convert_ws: None,
            converting: None,
            configuring: None,
            stage: 0,
            rejected: None,
            float_track: None,
        }
    }

    pub(super) fn set_channel_policy(&mut self, p: ChannelPolicy) {
        self.channels = p;
    }

    pub(super) fn channel_policy(&self) -> ChannelPolicy {
        self.channels
    }

    pub(super) fn set_float_policy(&mut self, p: FloatPolicy) {
        self.floats = p;
    }

    pub(super) fn float_policy(&self) -> FloatPolicy {
        self.floats
    }

    pub(super) fn set_convert_workspace(&mut self, ws: &'static mut [i32]) {
        self.convert_ws = Some(ws);
    }

    pub(super) fn converting(&self) -> Option<u8> {
        self.converting
    }

    pub(super) fn duplicating(&self) -> bool {
        self.duplicating
    }

    /// 播放开始与停止：与设备相关的记录失效。
    pub(super) fn new_session(&mut self) {
        self.rejected = None;
    }

    /// 配置开始：按声道策略得到交给能力检查与设备协商的格式（开启单声道复制且源为 `[FC]` 时为
    /// 同采样率、同位数的 `[FL FR]`，否则为源格式本身），回退阶段归零。
    pub(super) fn begin(&mut self, actual: SourceFormat) -> SourceFormat {
        let laid = if self.channels == ChannelPolicy::DuplicateMono
            && actual.layout() == crate::ChannelLayout::MONO
            && let Ok(f) = actual.relayout(crate::ChannelLayout::STEREO)
        {
            f
        } else {
            actual
        };
        self.duplicating = laid.layout() != actual.layout();
        self.stage = 0;
        self.configuring = Some(laid);
        laid
    }

    /// F3：同一浮点格式此前被当前设备拒绝、已改为转换：能否续用当前整数设备，不再重新协商。
    pub(super) fn reuse_converted(
        &self,
        laid: SourceFormat,
        device: Option<DeviceFormat>,
        reuse: ReusePolicy,
        caps: &Capabilities,
    ) -> bool {
        if laid.encoding().is_float()
            && self.floats == FloatPolicy::ToInt
            && reuse == ReusePolicy::Accepts
            && self.rejected == Some(laid)
            && self.convert_ws.is_some()
            && let Some(d) = device
            && !d.encoding().is_float()
            && let Ok(int) = laid.with_encoding(SampleEncoding::Int, d.valid_bits())
        {
            d.accepts(&int) && caps.check(&int).is_ok()
        } else {
            false
        }
    }

    /// 当前回退阶段请求的源格式：阶段 0 为 `laid` 本身，其后为同采样率、同布局的
    /// `FLOAT_FALLBACK_BITS` 位整数。
    pub(super) fn presentation(&self, laid: SourceFormat) -> Option<SourceFormat> {
        match self.stage {
            0 => Some(laid),
            k => {
                let bits = *FLOAT_FALLBACK_BITS.get(usize::from(k) - 1)?;
                laid.with_encoding(SampleEncoding::Int, bits).ok()
            }
        }
    }

    /// 能力检查失败：
    /// - 回退的整数格式超出本机能力（如 Embedded Full 只到 24 位）：试下一档；
    /// - 浮点源本身超出本机能力（如按浮点宽度估计的队列放不下），而已明确开启转换：从第一档整数
    ///   起试，没有转换工作区时按资源不足拒绝；
    /// - 其余按检查的原因拒绝。
    pub(super) fn check_failed(&mut self, laid: SourceFormat, err: CapabilityError) -> Next {
        if self.stage > 0 && usize::from(self.stage) < FLOAT_FALLBACK_BITS.len() {
            self.stage += 1;
            return Next::Retry;
        }
        if self.stage == 0 && laid.encoding().is_float() && self.floats == FloatPolicy::ToInt {
            if self.convert_ws.is_none() {
                return Next::Reject(CapabilityError::ConvertWorkspace);
            }
            self.rejected = Some(laid);
            self.stage = 1;
            return Next::Retry;
        }
        Next::Reject(err)
    }

    /// 浮点源被设备拒绝（选择或打开失败）时，按 [`FloatPolicy::ToInt`] 改请求下一档整数格式。
    pub(super) fn fallback(&mut self) -> Fallback {
        let Some(laid) = self.configuring else {
            return Fallback::None;
        };
        if !laid.encoding().is_float()
            || self.floats != FloatPolicy::ToInt
            || usize::from(self.stage) >= FLOAT_FALLBACK_BITS.len()
        {
            return Fallback::None;
        }
        if self.convert_ws.is_none() {
            return Fallback::Reject(CapabilityError::ConvertWorkspace);
        }
        if self.stage == 0 {
            self.rejected = Some(laid);
        }
        self.stage += 1;
        Fallback::Retry(laid)
    }

    /// 设备已按本曲目配置好（新打开或续用）：浮点源配到整数设备时（只在明确开启转换、经回退协商时
    /// 发生）锁定转换位数。
    pub(super) fn configured(&mut self, device: Option<DeviceFormat>) {
        self.converting = match (self.configuring, device) {
            (Some(src), Some(d)) if src.encoding().is_float() && !d.encoding().is_float() => {
                Some(d.valid_bits())
            }
            _ => None,
        };
        self.configuring = None;
    }

    /// 单声道复制（O1）只用于配置时已按策略复制的曲目（源 `[FC]`、设备 `[FL FR]`）；其余一律严格
    /// pack。
    pub(super) fn pack_mode(&self, source: SourceFormat, device: DeviceFormat) -> ChannelMode {
        if self.duplicating
            && source.layout() == crate::ChannelLayout::MONO
            && device.layout() == crate::ChannelLayout::STEREO
        {
            ChannelMode::DuplicateMono
        } else {
            ChannelMode::Exact
        }
    }

    /// 入队：配置为转换时经浮点转整数，否则按严格路径 pack（浮点按位复制，计数非有限样本）。
    pub(super) fn push<S: FrameSink>(
        &mut self,
        pcm: &Pcm<'_, crate::Exact>,
        sink: &mut S,
        device: &DeviceFormat,
        mode: ChannelMode,
        scratch: &mut [u8],
    ) -> Pushed {
        match self.converting {
            Some(bits) => convert_push(
                pcm,
                bits,
                self.convert_ws.as_deref_mut(),
                sink,
                device,
                mode,
                scratch,
            ),
            None => match sink.push(pcm, device, mode, scratch) {
                Ok(n) => Pushed::Frames {
                    frames: n,
                    stats: ConvertStats::default(),
                    nonfinite: count_nonfinite(pcm, n),
                },
                Err(_) => Pushed::Error,
            },
        }
    }

    /// 曲目第一次入队：给出声道复制（O1）与浮点转换（F3）的通知，并开始该曲目的浮点统计（换到新
    /// 的浮点曲目时先交出上一首的统计）。
    pub(super) fn first_output(&mut self, track: u32, source: SourceFormat) -> Announce {
        let mut a = Announce {
            duplicated: self.duplicating,
            ..Announce::default()
        };
        if source.encoding().is_float() {
            a.converted = self.converting;
            if self.float_track.is_none_or(|(t, _)| t != track) {
                a.finished = self.float_track.take();
                self.float_track = Some((
                    track,
                    FloatReport {
                        encoding: source.encoding(),
                        converted_to: self.converting,
                        nonfinite: 0,
                        convert: ConvertStats::default(),
                    },
                ));
            }
        }
        a
    }

    /// 累计当前浮点曲目的统计。
    pub(super) fn add_stats(&mut self, nonfinite: u64, stats: &ConvertStats) {
        if let Some((_, r)) = self.float_track.as_mut() {
            r.nonfinite += nonfinite;
            r.convert.add(stats);
        }
    }

    /// 交出当前浮点曲目的统计（关闭、停止或定位时）。
    pub(super) fn take_float_report(&mut self) -> Option<(u32, FloatReport)> {
        self.float_track.take()
    }
}

/// 严格路径按位输出的浮点样本中 NaN 与无穷的个数（前 `frames` 帧；指数位全 1）。
fn count_nonfinite<M: qy_pcm_format::pack::Fidelity>(pcm: &Pcm<'_, M>, frames: usize) -> u64 {
    let f = pcm.format();
    let words = f.encoding().words();
    let n = frames * f.words_per_frame();
    let samples = pcm.samples().get(..n).unwrap_or_default();
    match f.encoding() {
        SampleEncoding::Int => 0,
        SampleEncoding::Float32 => samples
            .iter()
            .filter(|&&w| (w as u32) & 0x7F80_0000 == 0x7F80_0000)
            .count() as u64,
        SampleEncoding::Float64 => samples
            .chunks_exact(words)
            .filter(|w| (w[1] as u32) & 0x7FF0_0000 == 0x7FF0_0000)
            .count() as u64,
    }
}

/// `pcm` 的前 `frames` 帧转为 `bits` 位整数，写入 `ws`。
fn convert_prefix<'w>(
    pcm: &Pcm<'_, crate::Exact>,
    frames: usize,
    bits: u8,
    ws: &'w mut [i32],
    stats: &mut ConvertStats,
) -> Result<Pcm<'w, qy_pcm_format::pack::Altered>, ConvertError> {
    let format = pcm.format();
    let prefix = pcm
        .samples()
        .get(..frames * format.words_per_frame())
        .unwrap_or_default();
    let prefix = Pcm::exact(prefix, format).map_err(|_| ConvertError::Format)?;
    float_to_int(&prefix, bits, ws, stats)
}

/// 浮点转整数（明确开启，F3）后入队。一次转换的帧数受工作区容量限制；统计只计入实际入队的帧。
/// 遇到非有限样本时先入队它之前的完整帧，再报告。
fn convert_push<S: FrameSink>(
    pcm: &Pcm<'_, crate::Exact>,
    bits: u8,
    ws: Option<&mut [i32]>,
    sink: &mut S,
    dev: &DeviceFormat,
    mode: ChannelMode,
    scratch: &mut [u8],
) -> Pushed {
    let Some(ws) = ws else {
        return Pushed::Error;
    };
    let channels = usize::from(pcm.format().channels()).max(1);
    let mut stats = ConvertStats::default();
    let (converted, pushed, nonfinite) =
        match convert_prefix(pcm, pcm.frames(), bits, ws, &mut stats) {
            Ok(alt) => {
                let n = alt.frames();
                match sink.push(&alt, dev, mode, scratch) {
                    Ok(p) => (n, p, false),
                    Err(_) => return Pushed::Error,
                }
            }
            Err(ConvertError::NonFinite { index }) => {
                let limit = index / channels;
                stats = ConvertStats::default();
                if limit == 0 {
                    (0, 0, true)
                } else {
                    match convert_prefix(pcm, limit, bits, ws, &mut stats) {
                        Ok(alt) => match sink.push(&alt, dev, mode, scratch) {
                            Ok(p) => (limit, p, true),
                            Err(_) => return Pushed::Error,
                        },
                        Err(_) => return Pushed::Error,
                    }
                }
            }
            Err(ConvertError::Format) => return Pushed::Error,
        };
    if pushed < converted {
        // 队列放不下全部：只按实际入队的前缀统计，非有限样本留到下次再遇到时报告。
        stats = ConvertStats::default();
        if pushed > 0 && convert_prefix(pcm, pushed, bits, ws, &mut stats).is_err() {
            return Pushed::Error;
        }
        return Pushed::Frames {
            frames: pushed,
            stats,
            nonfinite: 0,
        };
    }
    if nonfinite {
        return Pushed::NonFinite {
            frames: pushed,
            stats,
        };
    }
    Pushed::Frames {
        frames: pushed,
        stats,
        nonfinite: 0,
    }
}
