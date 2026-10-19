//! 播放调度（M3 T1）：`no_std`、无 `alloc` 的引擎层，桌面、模拟与 MCU 宿主共用。
//!
//! 引擎包住 [`Core`]，负责原先散落在各宿主里的播放规则：
//! - 能力检查：宿主声明的 [`Capabilities`] 不足以承载源格式时，在打开设备之前拒绝并给出原因；
//! - 设备复用：当前设备格式能承载新源时继续使用（[`ReusePolicy`]），否则先排空再重开；
//! - 预填：已入队帧达到 [`Capabilities::prefill_ms`] 或队列已满时才启动设备；
//! - 背压：队列满时返回 [`Poll::Wait`]，宿主取走数据后再调用 [`Engine::poll`]；
//! - 排空：播放结束、失败或换格式前，先让设备播完已入队的有效音频；
//! - 控制：播放、停止、暂停/恢复、上一首/下一首、跳到第 k 首；出错按 [`ErrorPolicy`] 停止或跳过。
//!
//! 宿主只执行 [`HostAction`]（读文件、开关设备、搬运字节）并以 [`HostEvent`] 回报，不再各自实现
//! 播放器逻辑。引擎不访问文件、线程或时钟。pack 在引擎内经 [`FrameSink`] 完成，仍是唯一的
//! 输出表示转换边界。冻结接口 `Action`/`Event`/`Decoder` 不变。

use core::fmt;

use crate::{
    Action, Core, CoreError, Decoder, DeviceFormat, Event, Fault, Generation, SourceFormat,
};
use qy_pcm_format::frames_for_millis;

mod options;
pub use options::{ChannelPolicy, FloatPolicy, FloatReport};
use options::{Fallback, Next, Options, Pushed};

/// 浮点转整数的统计（F3）。
pub use qy_pcm_format::convert::ConvertStats;

/// 设备帧队列的生产端（宿主提供：桌面为 `rtrb`，MCU 为静态 `bbqueue`）。
pub use qy_pcm_format::queue::{
    BIP_MARGIN_FRAMES, FrameSink, FrameSource, MAX_FRAME_BYTES, PACK_SCRATCH_BYTES, RingError,
    queue_bytes,
};

/// 宿主声明的能力与资源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// 最高采样率（Hz）。
    pub max_rate: u32,
    /// 最多声道数。
    pub max_channels: u16,
    /// 最高源有效位 N。
    pub max_bits: u8,
    /// 设备帧队列字节数。
    pub queue_bytes: usize,
    /// 启动设备前的预填时长（毫秒），也是队列须能容纳的最短时长。
    pub prefill_ms: u32,
}

/// 解码器所说的“单元”在报告中的名称（B1）：核心只按资源表达（解码单元的帧数、输入单元的字节数），
/// 具体格式的叫法由解码器给出，例如 FLAC 的解码单元是“块”、输入单元是“帧”。核心不认识任何格式。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Unit {
    /// 格式名（如 `"FLAC"`）；空串表示不写格式名。
    pub codec: &'static str,
    /// 单元名（如 `"块"`）。
    pub name: &'static str,
}

impl Unit {
    /// 未给出格式叫法时的解码单元。
    pub const DECODE: Unit = Unit {
        codec: "",
        name: "解码单元",
    };
    /// 未给出格式叫法时的输入单元。
    pub const INPUT: Unit = Unit {
        codec: "",
        name: "输入单元",
    };
}

/// 手写 `Debug`：两个名称按原文写出，不经 `str` 的转义格式化。派生的 `Debug` 会把转义与 Unicode
/// 表一并链接进来（MCU 演示实测约 3.7 KB 代码，B1）。
impl fmt::Debug for Unit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Unit {{ codec: \"{}\", name: \"{}\" }}",
            self.codec, self.name
        )
    }
}

impl fmt::Display for Unit {
    /// “FLAC 最大块”或“最大解码单元”。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.codec.is_empty() {
            write!(f, "最大{}", self.name)
        } else {
            write!(f, "{} 最大{}", self.codec, self.name)
        }
    }
}

/// 能力不足的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityError {
    Rate {
        rate: u32,
        max: u32,
    },
    Channels {
        channels: u16,
        max: u16,
    },
    Bits {
        bits: u8,
        max: u8,
    },
    /// 预填时长所需的队列字节数超过本机队列。
    Queue {
        required: usize,
        available: usize,
    },
    /// 文件头声明的最大解码单元（帧数）超出本机解码容量（R1；B1 起与格式无关，FLAC 为“块”）。
    DecodeUnit {
        frames: u32,
        max: u32,
        unit: &'static Unit,
    },
    /// 文件头声明的最大输入单元（字节）超出本机输入窗口（R1；B1 起与格式无关，FLAC 为“帧”）。
    InputUnit {
        bytes: u32,
        window: u32,
        unit: &'static Unit,
    },
    /// 开启了浮点转整数，设备也不接受源的浮点格式，但本机配置没有转换工作区（F3；如 Embedded
    /// Lite）。
    ConvertWorkspace,
}

impl fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Rate { rate, max } => write!(f, "采样率 {rate} Hz 超出本机配置的上限 {max} Hz"),
            Self::Channels { channels, max } => {
                write!(f, "{channels} 声道超出本机配置的上限 {max} 声道")
            }
            Self::Bits { bits, max } => write!(f, "{bits} 位超出本机配置的上限 {max} 位"),
            Self::Queue {
                required,
                available,
            } => write!(
                f,
                "本机配置的队列不足以承载该格式：需要 {required} 字节，只有 {available} 字节"
            ),
            Self::DecodeUnit { frames, max, unit } => {
                write!(f, "{unit} {frames} 帧超出本机配置的 {max} 帧")
            }
            Self::InputUnit {
                bytes,
                window,
                unit,
            } => {
                if !unit.codec.is_empty() {
                    write!(f, "{} ", unit.codec)?;
                }
                write!(
                    f,
                    "声明的最大{} {bytes} 字节超出本机输入窗口 {window} 字节",
                    unit.name
                )
            }
            Self::ConvertWorkspace => write!(
                f,
                "设备不接受该浮点格式；已开启浮点转整数，但本机配置没有转换工作区"
            ),
        }
    }
}

/// 承载源样本的最小设备容器字节数：整数为 [`min_container_bytes`]，浮点为浮点位宽（4 / 8，F3）；
/// DSD 每帧每声道一个 32 位字，无论哪种传输至少 4 字节（D1）。
pub const fn min_container_bytes_for(src: &SourceFormat) -> usize {
    if src.encoding().is_dsd() {
        return 4;
    }
    match src.encoding().float_bits() {
        Some(bits) => bits as usize / 8,
        None => min_container_bytes(src.bits()),
    }
}

/// 承载 N 位样本的最小容器字节数（2/3/4）。
pub const fn min_container_bytes(bits: u8) -> usize {
    if bits <= 16 {
        2
    } else if bits <= 24 {
        3
    } else {
        4
    }
}

impl Capabilities {
    /// 预填帧数（向上取整）。
    pub const fn prefill_frames(&self, rate: u32) -> u64 {
        frames_for_millis(rate, self.prefill_ms)
    }

    /// 源格式是否在本机能力之内。队列按最小容器估计；设备实际选用更大的容器时，
    /// 宿主在重配置队列时还会再次检查。
    pub fn check(&self, src: &SourceFormat) -> Result<(), CapabilityError> {
        if src.sample_rate() > self.max_rate {
            return Err(CapabilityError::Rate {
                rate: src.sample_rate(),
                max: self.max_rate,
            });
        }
        if src.channels() > self.max_channels {
            return Err(CapabilityError::Channels {
                channels: src.channels(),
                max: self.max_channels,
            });
        }
        // 浮点源（F3）的位宽不是整数有效位 N：不按 `max_bits` 限制，只受队列容量限制。
        if !src.encoding().is_float() && src.bits() > self.max_bits {
            return Err(CapabilityError::Bits {
                bits: src.bits(),
                max: self.max_bits,
            });
        }
        let required = usize::try_from(self.prefill_frames(src.sample_rate()))
            .ok()
            .and_then(|f| f.checked_mul(usize::from(src.channels())))
            .and_then(|s| s.checked_mul(min_container_bytes_for(src)))
            .unwrap_or(usize::MAX);
        if required > self.queue_bytes {
            return Err(CapabilityError::Queue {
                required,
                available: self.queue_bytes,
            });
        }
        Ok(())
    }
}

/// 解码器对本机资源的需求（R1）。可选接口，冻结的 [`Decoder`] 不变；默认实现表示“无额外说明”。
///
/// 文件头声明不可信：这里的数值只用于提前给出具体原因与播放计划，逐帧的边界检查仍在解码器里。
pub trait Resources {
    /// 当前曲目在文件头处因本机解码容量不足被拒绝时，换算成能力不足的具体原因。只报告文件头阶段的
    /// 拒绝；解码过程中实际帧超出容量仍按解码错误报告。
    fn capacity_rejection(&self) -> Option<CapabilityError> {
        None
    }

    /// 当前曲目文件头声明的需求与本机容量（用于播放计划）。
    fn demand(&self) -> Option<DecoderDemand> {
        None
    }

    /// 当前曲目已做的结构修正条数（W1）。修正只决定“读到哪里为止”和“丢弃不完整的尾部”，
    /// 不改变、不编造样本。
    fn repair_count(&self) -> usize {
        0
    }

    /// 第 `index` 条结构修正；越界时为 `None`。
    fn repair(&self, index: usize) -> Option<Repair> {
        let _ = index;
        None
    }
}

/// 每首曲目最多报告的结构修正条数。
pub const MAX_REPAIRS: usize = 4;

/// 解码器对源文件结构做的修正（W1，报告用）。只修正结构：样本值都来自文件原样。
/// `chunk` 为容器中块的 4 字节标识（如 WAV 的 `RIFF`、`data`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repair {
    /// 长度字段是流式写入程序留下的占位值（未回填），按读到源末尾处理；`read` 为实际读到的字节数。
    /// `declared` 为长度字段的原值（RF64 / BW64 的 64 位长度也能如实报告，F4）。
    LengthPlaceholder {
        chunk: [u8; 4],
        declared: u64,
        read: u64,
    },
    /// 长度字段与源的实际长度不符，以数据块的声明长度为准；`actual` 为源中该块实际的字节数。
    LengthMismatch {
        chunk: [u8; 4],
        declared: u64,
        actual: u64,
    },
    /// 数据长度不是整帧，丢弃末尾不足一帧的字节。
    PartialFrameDropped { bytes: u32 },
}

/// 4 字节块标识按 ASCII 显示（不可显示的字节以 `?` 代替）。
struct ChunkId([u8; 4]);

impl fmt::Display for ChunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &b in &self.0 {
            let c = if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '?'
            };
            fmt::Write::write_char(f, c)?;
        }
        Ok(())
    }
}

impl fmt::Display for Repair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::LengthPlaceholder {
                chunk,
                declared,
                read,
            } => write!(
                f,
                "{} 长度为流式占位值 {declared:#x}（写入程序未回填），按读到文件末尾处理，实际 {read} 字节",
                ChunkId(chunk)
            ),
            Self::LengthMismatch {
                chunk,
                declared,
                actual,
            } => write!(
                f,
                "{} 声明长度 {declared} 字节与文件实际的 {actual} 字节不符，以 data 的声明长度为准",
                ChunkId(chunk)
            ),
            Self::PartialFrameDropped { bytes } => {
                write!(f, "丢弃数据末尾不足一帧的 {bytes} 字节")
            }
        }
    }
}

/// 解码器的需求与容量（文件头声明值，不可信，只用于报告）。按资源表达，单元的叫法由解码器给出
/// （B1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecoderDemand {
    /// 声明的最大解码单元（帧数）与本机容量。
    pub max_unit_frames: u32,
    pub unit_frames_capacity: u32,
    /// 声明的最大输入单元（字节，0 表示未声明）与本机输入窗口。
    pub max_unit_bytes: u32,
    pub window: u32,
    /// 两种单元在报告中的叫法（引用：错误与需求中只多一个指针）。
    pub decode_unit: &'static Unit,
    pub input_unit: &'static Unit,
}

/// 播放计划（R1）：当前源在本机上的资源安排，供宿主报告。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackPlan {
    pub source: SourceFormat,
    pub device: Option<DeviceFormat>,
    /// 启动前的预填帧数（按设备采样率）与时长。
    pub prefill_frames: u64,
    pub prefill_ms: u32,
    /// 预填所需的队列字节数（按设备帧宽；设备未定时按最小容器估计）与本机队列字节数。
    pub queue_required: usize,
    pub queue_available: usize,
    pub decoder: Option<DecoderDemand>,
}

impl fmt::Display for PlaybackPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = &self.source;
        match s.dsd_bit_rate() {
            // D1：DSD 的“帧”是每声道一个 32 位字，帧率为字率。
            Some(b) => write!(
                f,
                "源 DSD {b} 位/秒（字率 {}）/ {} 声道",
                s.sample_rate(),
                s.channels()
            )?,
            None => write!(
                f,
                "源 {} Hz / {} 声道 / {} 位",
                s.sample_rate(),
                s.channels(),
                s.bits()
            )?,
        }
        match &self.device {
            Some(d) if d.dsd_transport().is_some() => write!(
                f,
                "，设备 DSD 字率 {}（设备帧率 {} Hz，每字 {} 帧）/ {} 声道 / {}-{} 位",
                d.sample_rate(),
                d.device_rate(),
                d.expansion(),
                d.channels(),
                d.container_bits(),
                d.valid_bits()
            )?,
            Some(d) => write!(
                f,
                "，设备 {} Hz / {} 声道 / {}-{} 位",
                d.sample_rate(),
                d.channels(),
                d.container_bits(),
                d.valid_bits()
            )?,
            None => f.write_str("，设备未定")?,
        }
        write!(
            f,
            "，预填 {} 帧（{} ms），队列 {}/{} 字节",
            self.prefill_frames, self.prefill_ms, self.queue_required, self.queue_available
        )?;
        if let Some(d) = &self.decoder {
            write!(
                f,
                "，{} {}/{} 帧，最大{} ",
                d.decode_unit, d.max_unit_frames, d.unit_frames_capacity, d.input_unit.name
            )?;
            if d.max_unit_bytes == 0 {
                f.write_str("未声明")?;
            } else {
                write!(f, "{}", d.max_unit_bytes)?;
            }
            write!(f, "/{} 字节", d.window)?;
        }
        Ok(())
    }
}

/// 换曲时设备的复用策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReusePolicy {
    /// 当前设备格式能无损承载新源（采样率、声道相同且 N ≤ V）时继续使用（真实后端）。
    Accepts,
    /// 宿主为每个源选择设备格式（[`HostAction::SelectDevice`]），与当前格式完全相同时才继续使用。
    Identical,
}

/// 曲目出错时的策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorPolicy {
    /// 排空已入队音频后停止播放列表。
    Stop,
    /// 报告后跳到下一首继续（打开、读取、解码、配置与能力不足均可跳过）。
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineConfig {
    pub caps: Capabilities,
    pub reuse: ReusePolicy,
    pub on_error: ErrorPolicy,
}

/// 引擎层的故障。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineFault {
    /// 核心报告的故障（打开、读取、解码、配置、输入窗口）。
    Core(Fault),
    /// 本机能力不足，未打开设备。
    Capability(CapabilityError),
    /// 帧队列/pack 拒绝（不应发生，出现即停止）。
    Output,
    /// 明确开启的浮点转整数遇到 NaN 或无穷（F3 §8 决定 2：不猜测）：该曲目在第 `frame` 帧
    /// （曲目内帧号）停止，此前已入队的音频照常播放。
    NonFinite { frame: u64 },
}

/// 引擎请求宿主执行的操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostAction {
    /// 打开第 `track` 首的音源（核心动作，原样转交）。
    Open { generation: Generation, track: u32 },
    /// 读取至多 `max` 字节到 [`Engine::read_target`]。
    Read {
        generation: Generation,
        request: u32,
        max: usize,
    },
    /// 把音源读取位置移到距开头 `offset` 字节处（S1 定位，核心动作，原样转交），回报
    /// [`HostEvent::SeekDone`] 或 [`HostEvent::SeekFailed`]。
    Seek {
        generation: Generation,
        request: u32,
        offset: u64,
    },
    /// 关闭音源。
    Close { generation: Generation },
    /// （[`ReusePolicy::Identical`]）为源选择设备格式，回报 [`HostEvent::DeviceSelected`] 或
    /// [`HostEvent::DeviceUnsupported`]；不得有副作用。
    SelectDevice { source: SourceFormat },
    /// 打开设备并按设备格式重配置帧队列（`format` 为已选定的设备格式，`None` 时由宿主协商），
    /// 回报 [`HostEvent::DeviceOpened`] 或 [`HostEvent::DeviceOpenFailed`]。
    OpenDevice {
        source: SourceFormat,
        format: Option<DeviceFormat>,
    },
    /// 预填完成，开始从队列取数。
    StartDevice,
    /// 当前格式不再有新数据：设备尚未启动时立即启动，播完已入队数据后回报
    /// [`HostEvent::DeviceDrained`]。
    DrainDevice,
    /// 立即停止设备并丢弃队列中的数据（停止、换曲、设备错误）。
    StopDevice,
    /// 暂停：设备输出静音（单独计数，不算欠载），不再从队列取数。只对已启动的设备发出；
    /// 暂停时尚未启动的设备在恢复之前不会启动。
    PauseDevice,
    /// 恢复从队列取数。
    ResumeDevice,
    /// 曲目出错，按策略跳过并继续下一首。
    TrackSkipped { track: u32, fault: EngineFault },
    /// 第 `track` 首在不改样本的前提下做过的一条结构修正（W1）。在该曲目关闭时、按解码器记录的
    /// 顺序逐条给出；宿主据此在报告中写明。
    Repaired { track: u32, repair: Repair },
    /// 第 `track` 首的定位改为顺序解码到目标帧（解码器不支持定位或音源不可定位；结果同样
    /// 精确，只是更慢）。宿主据此在报告中写明。
    SeekSequential { track: u32 },
    /// 第 `track` 首的定位落在目标之前，按解码器给出的预滚（E5，[`crate::Decoder::take_preroll`]）
    /// 解码并丢弃了目标帧之前的 `frames` 帧（送出的第一帧仍是目标帧）。宿主据此在报告中写明。
    SeekPreroll { track: u32, frames: u64 },
    /// 第 `track` 首按 [`ChannelPolicy::DuplicateMono`] 把单声道复制到左右两路（声道布局已改变，
    /// 样本值未改变）。在设备按该曲目配置成功（新打开或续用）后给出；配置失败的曲目不给出。
    /// 宿主据此在报告中标为“已改变”。
    ChannelsDuplicated { track: u32 },
    /// 第 `track` 首的浮点样本按明确开启的选项（[`FloatPolicy::ToInt`]）转换为 `bits` 位整数
    /// （样本已改变，F3）。在该曲目第一次入队时给出。
    FloatConverted { track: u32, bits: u8 },
    /// 第 `track` 首浮点曲目的统计（F3）。在该曲目关闭（播完、出错、停止或定位）时给出；同一曲目
    /// 定位后会再给出一次，宿主按曲目累加。
    FloatStats { track: u32, report: FloatReport },
    /// 播放列表正常结束，设备中的音频已播完。
    Finished,
    /// 播放失败并停止；失败前已入队的音频已播完。
    Failed { track: u32, fault: EngineFault },
}

/// 宿主回报。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostEvent {
    Opened {
        generation: Generation,
    },
    OpenFailed {
        generation: Generation,
    },
    ReadDone {
        generation: Generation,
        request: u32,
        len: usize,
    },
    ReadEof {
        generation: Generation,
        request: u32,
    },
    ReadFailed {
        generation: Generation,
        request: u32,
    },
    SeekDone {
        generation: Generation,
        request: u32,
    },
    SeekFailed {
        generation: Generation,
        request: u32,
    },
    DeviceSelected {
        format: DeviceFormat,
    },
    DeviceUnsupported,
    DeviceOpened {
        format: DeviceFormat,
    },
    DeviceOpenFailed,
    DeviceDrained,
}

/// 引擎正在等待的事。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// 等待读取回报。
    Io,
    /// 队列已满，等设备取走数据。
    QueueFull,
    /// 等待设备回报（选择、打开、排空）。
    Device,
}

/// [`Engine::poll`] 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Poll {
    Action(HostAction),
    Wait(Wait),
    /// 本次调用已连续推进多步（例如多次入队），先让出；宿主可立即再次调用。
    Yield,
    /// 未播放，或播放列表已结束/失败/停止。
    Idle,
}

/// 统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EngineStats {
    /// 宿主回报属于旧代次被拒绝的次数。
    pub stale: u32,
    /// 引擎内部消化的、回报与状态不符的次数（应为 0；[`Engine::handle`] 返回给宿主的错误不计入）。
    pub protocol_errors: u32,
    /// 跳过的曲目数。
    pub skipped: u32,
    /// 累计入队的设备帧数（宿主可据此统计每个设备流入队的帧数）。
    pub frames_pushed: u64,
    /// 累计报告的结构修正条数（W1）。
    pub repairs: u32,
    /// 按 [`ChannelPolicy::DuplicateMono`] 改变了声道布局的曲目数（O1）。
    pub duplicated_tracks: u32,
    /// 按 [`FloatPolicy::ToInt`] 把浮点转为整数的曲目数（F3）。
    pub converted_tracks: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum After {
    Open {
        generation: Generation,
        source: SourceFormat,
        format: Option<DeviceFormat>,
    },
    Finish,
    Fail {
        track: u32,
        fault: EngineFault,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Idle,
    Run,
    WaitSelect {
        generation: Generation,
        source: SourceFormat,
    },
    WaitOpen {
        generation: Generation,
    },
    Drain(After),
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Dev {
    format: DeviceFormat,
    running: bool,
}

const OUTBOX: usize = 6;
/// 队列中同时可记录的曲目边界数。队列里的曲目更多时引擎暂停解码，等设备播完旧曲目。
const MARKS: usize = 8;

/// 播放引擎。
pub struct Engine<D: Decoder + Resources, S: FrameSink> {
    core: Core<D>,
    sink: S,
    scratch: &'static mut [u8],
    cfg: EngineConfig,
    mode: Mode,
    device: Option<Dev>,
    paused: bool,
    track_count: u32,
    reject: Option<EngineFault>,
    /// 当前打开的曲目（核心关闭它时据此报告结构修正，W1）。
    open_track: Option<u32>,
    outbox: [Option<HostAction>; OUTBOX],
    out_len: usize,
    /// 曲目边界：(曲目, 该曲目第一帧在累计入队帧中的位置, 该帧在曲目内的帧号)，按入队顺序。
    /// 帧号从定位目标开始（S1），否则为 0。
    marks: [(u32, u64, u64); MARKS],
    /// 定位后第一次入队时记入边界的 (曲目, 目标帧)。
    seek_base: Option<(u32, u64)>,
    marks_len: usize,
    stats: EngineStats,
    /// 明确开启的选项（单声道复制、浮点转整数）的全部状态（B1，见 `options`）。
    options: Options,
    /// 当前曲目已入队的帧在曲目内的位置（定位后从目标帧算起；报告非有限样本的位置用）。
    track_frame: u64,
    /// 已为其报告过“第一次入队”的曲目（声道复制与浮点转换按曲目各报告一次）。
    announced: Option<u32>,
}

impl<D: Decoder + Resources, S: FrameSink> Engine<D, S> {
    /// 由宿主一次性提供的核心、帧队列生产端与 pack 暂存区构造。
    pub fn new(core: Core<D>, sink: S, scratch: &'static mut [u8], cfg: EngineConfig) -> Self {
        Self {
            core,
            sink,
            scratch,
            cfg,
            mode: Mode::Idle,
            device: None,
            paused: false,
            track_count: 0,
            reject: None,
            open_track: None,
            outbox: [None; OUTBOX],
            out_len: 0,
            marks: [(0, 0, 0); MARKS],
            marks_len: 0,
            seek_base: None,
            stats: EngineStats::default(),
            options: Options::new(),
            track_frame: 0,
            announced: None,
        }
    }

    /// 设置浮点策略（默认 [`FloatPolicy::Strict`]）。对之后配置的曲目生效；正在播放的曲目不变。
    pub fn set_float_policy(&mut self, policy: FloatPolicy) {
        self.options.set_float_policy(policy);
    }

    pub fn float_policy(&self) -> FloatPolicy {
        self.options.float_policy()
    }

    /// 提供浮点转整数的工作区（宿主启动时一次性提供，换曲与重置复用）。没有工作区时开启
    /// [`FloatPolicy::ToInt`] 也不能转换，按 [`CapabilityError::ConvertWorkspace`] 拒绝。
    pub fn set_convert_workspace(&mut self, ws: &'static mut [i32]) {
        self.options.set_convert_workspace(ws);
    }

    /// 当前配置把浮点转为几位整数（打开设备时宿主据此在协商记录中写明）。
    pub fn converting(&self) -> Option<u8> {
        self.options.converting()
    }

    /// 设置声道策略（默认 [`ChannelPolicy::Strict`]）。对之后配置的曲目生效；正在播放的曲目不变。
    pub fn set_channel_policy(&mut self, policy: ChannelPolicy) {
        self.options.set_channel_policy(policy);
    }

    pub fn channel_policy(&self) -> ChannelPolicy {
        self.options.channel_policy()
    }

    pub fn core(&self) -> &Core<D> {
        &self.core
    }

    pub fn decoder(&self) -> &D {
        self.core.decoder()
    }

    pub fn sink(&self) -> &S {
        &self.sink
    }

    /// 宿主重配置帧队列时使用。
    pub fn sink_mut(&mut self) -> &mut S {
        &mut self.sink
    }

    pub fn config(&self) -> &EngineConfig {
        &self.cfg
    }

    /// 更换出错策略（下一次出错时生效）。
    pub fn set_error_policy(&mut self, policy: ErrorPolicy) {
        self.cfg.on_error = policy;
    }

    pub fn stats(&self) -> EngineStats {
        self.stats
    }

    pub fn generation(&self) -> Generation {
        self.core.generation()
    }

    /// 正在解码的曲目序号（可能领先于正在播放的曲目一个队列的时长）。
    pub fn track(&self) -> u32 {
        self.core.track()
    }

    /// 设备已取走的累计帧数（入队总数减去队列中剩余；队列计数只会偏大，因此结果只会偏小）。
    fn consumed_frames(&self) -> u64 {
        self.stats
            .frames_pushed
            .saturating_sub(self.sink.queued_frames() as u64)
    }

    /// 正在播放的曲目：队列中最后一个已开始播放的曲目边界；尚无输出时为正在解码的曲目。
    /// 上一首/下一首以此为准。
    pub fn playing_track(&self) -> u32 {
        self.playing_progress().0
    }

    /// 正在播放的曲目与该曲目内的播放位置（帧；定位后从目标帧算起，含设备缓冲中尚未发声的
    /// 部分，最多一个设备缓冲）。尚无输出时为正在解码的曲目与起始位置。供界面显示播放进度。
    pub fn playing_progress(&self) -> (u32, u64) {
        let consumed = self.consumed_frames();
        self.marks[..self.marks_len]
            .iter()
            .rev()
            .find(|m| m.1 <= consumed)
            .or(self.marks[..self.marks_len].first())
            .map_or_else(
                // 尚无输出：正在解码的曲目；刚定位时从目标帧算起。
                || {
                    let track = self.core.track();
                    let base = match self.seek_base {
                        Some((t, frame)) if t == track => frame,
                        _ => 0,
                    };
                    (track, base)
                },
                |m| (m.0, consumed.saturating_sub(m.1).saturating_add(m.2)),
            )
    }

    /// 丢弃已播完曲目的边界；返回是否还能记录新边界。
    fn reserve_mark(&mut self) -> bool {
        let consumed = self.consumed_frames();
        let keep_from = self.marks[..self.marks_len]
            .iter()
            .rposition(|m| m.1 <= consumed)
            .unwrap_or(0);
        self.marks.copy_within(keep_from..self.marks_len, 0);
        self.marks_len -= keep_from;
        self.marks_len < MARKS
    }

    pub fn track_count(&self) -> u32 {
        self.track_count
    }

    /// 当前设备格式（已打开时）。
    pub fn device_format(&self) -> Option<DeviceFormat> {
        self.device.map(|d| d.format)
    }

    /// 当前源的播放计划（源格式已知时）。
    pub fn plan(&self) -> Option<PlaybackPlan> {
        let source = self.core.decoder().format()?;
        let device = self.device.map(|d| d.format);
        let rate = device.map_or(source.sample_rate(), |d| d.sample_rate());
        let caps = &self.cfg.caps;
        let prefill_frames = caps.prefill_frames(rate);
        let frame_bytes = match device {
            Some(d) => usize::from(d.block_align()),
            None => usize::from(source.channels()) * min_container_bytes_for(&source),
        };
        let queue_required = usize::try_from(prefill_frames)
            .ok()
            .and_then(|f| f.checked_mul(frame_bytes))
            .unwrap_or(usize::MAX);
        Some(PlaybackPlan {
            source,
            device,
            prefill_frames,
            prefill_ms: caps.prefill_ms,
            queue_required,
            queue_available: caps.queue_bytes,
            decoder: self.core.decoder().demand(),
        })
    }

    /// 播放列表是否在进行中（含排空尾部）；停止、正常结束或失败后为假。
    pub fn is_active(&self) -> bool {
        !matches!(self.mode, Mode::Idle | Mode::Done)
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// 宿主为读取请求写入数据的目标切片。
    pub fn read_target(&mut self, generation: Generation, request: u32) -> Option<&mut [u8]> {
        self.core.read_target(generation, request)
    }

    fn push(&mut self, a: HostAction) {
        if let Some(slot) = self.outbox.get_mut(self.out_len) {
            *slot = Some(a);
            self.out_len += 1;
        } else {
            self.stats.protocol_errors += 1;
        }
    }

    /// 只取出已排队的控制动作（不推进核心）。停止、切歌或设备断开后，宿主可先立即执行这些
    /// 动作（关闭音源、停止设备并丢弃队列）。
    pub fn take_control(&mut self) -> Option<HostAction> {
        if self.out_len == 0 {
            return None;
        }
        let a = self.outbox[0].take();
        self.outbox.copy_within(1..self.out_len, 0);
        self.out_len -= 1;
        self.outbox[self.out_len] = None;
        a
    }

    fn core_handle(&mut self, e: Event) {
        match self.core.handle(e) {
            Ok(()) => {}
            Err(CoreError::Stale) => self.stats.stale += 1,
            Err(_) => self.stats.protocol_errors += 1,
        }
    }

    /// 播放第 `first` 首起共 `count` 首。
    pub fn play(&mut self, first: u32, count: u32) {
        self.start(first, count, 0);
    }

    fn start(&mut self, first: u32, count: u32, frame: u64) {
        self.track_count = count;
        self.reject = None;
        self.options.new_session();
        self.marks_len = 0;
        self.track_frame = frame;
        self.announced = None;
        self.seek_base = (frame > 0).then_some((first, frame));
        self.core.play_at(first, count, frame);
        self.mode = Mode::Run;
    }

    /// 在正在播放的曲目内定位到第 `frame` 帧（S1）：丢弃已入队的音频，从该帧重新开始这首曲目，
    /// 暂停状态保持。等于或超过曲目长度时按播完该曲处理。未在播放时忽略。
    pub fn seek(&mut self, frame: u64) {
        if !self.is_active() {
            return;
        }
        let track = self.playing_track();
        let count = self.track_count;
        let paused = self.paused;
        self.stop();
        self.paused = paused;
        self.start(track, count, frame);
    }

    /// 停止：关闭音源（旧代次的回报此后一律拒绝），停止设备并丢弃队列。
    pub fn stop(&mut self) {
        self.report_float();
        if let Some(Action::Close { generation }) = self.core.stop() {
            self.push(HostAction::Close { generation });
        }
        if self.device.take().is_some() {
            self.push(HostAction::StopDevice);
        }
        self.paused = false;
        self.reject = None;
        self.options.new_session();
        self.mode = Mode::Idle;
    }

    /// 跳到第 `track` 首：丢弃当前已入队的音频，从该曲重新开始。
    pub fn skip_to(&mut self, track: u32) {
        let count = self.track_count;
        let paused = self.paused;
        self.stop();
        self.paused = paused;
        self.play(track, count);
    }

    /// 下一首（相对正在播放的曲目）；已是最后一首时停止。
    pub fn next(&mut self) {
        let t = self.playing_track().saturating_add(1);
        if t < self.track_count {
            self.skip_to(t);
        } else {
            self.stop();
        }
    }

    /// 上一首（相对正在播放的曲目；第一首时从头重播）。
    pub fn prev(&mut self) {
        self.skip_to(self.playing_track().saturating_sub(1));
    }

    /// 暂停：设备输出静音、不再取数；生产端照常填满队列后等待。
    pub fn pause(&mut self) {
        if self.paused {
            return;
        }
        self.paused = true;
        if self.device.is_some_and(|d| d.running) {
            self.push(HostAction::PauseDevice);
        }
    }

    /// 恢复。
    pub fn resume(&mut self) {
        if !self.paused {
            return;
        }
        self.paused = false;
        match self.device.as_mut() {
            Some(d) if d.running => self.push(HostAction::ResumeDevice),
            // 暂停期间已无新数据（等待排空）：此时才以排空方式启动。
            Some(d) if matches!(self.mode, Mode::Drain(_)) => {
                d.running = true;
                self.push(HostAction::DrainDevice);
            }
            Some(_) => self.maybe_start(false),
            None => {}
        }
    }

    /// 设备断开或出错：停止核心，不再排空。
    pub fn device_lost(&mut self) {
        if let Some(Action::Close { generation }) = self.core.stop() {
            self.push(HostAction::Close { generation });
        }
        self.device = None;
        self.paused = false;
        self.mode = Mode::Done;
    }

    fn maybe_start(&mut self, queue_full: bool) {
        let Some(d) = self.device.as_mut() else {
            return;
        };
        if d.running || self.paused {
            return;
        }
        let prefill = self.cfg.caps.prefill_frames(d.format.sample_rate());
        if queue_full || self.sink.queued_frames() as u64 >= prefill {
            d.running = true;
            self.push(HostAction::StartDevice);
        }
    }

    fn begin_drain(&mut self, after: After) {
        match self.device.as_mut() {
            Some(d) => {
                // 尚未启动的设备由 DrainDevice 直接以排空方式启动（宿主可据此在启动前就置 EOF，
                // 不把“数据不足一包”误判为欠载）。暂停中且尚未启动时推迟到恢复。
                self.mode = Mode::Drain(after);
                if d.running || !self.paused {
                    d.running = true;
                    self.push(HostAction::DrainDevice);
                }
            }
            None => self.after_drain(after),
        }
    }

    fn after_drain(&mut self, after: After) {
        match after {
            After::Open {
                generation,
                source,
                format,
            } => {
                self.device = None;
                self.push(HostAction::OpenDevice { source, format });
                self.mode = Mode::WaitOpen { generation };
            }
            After::Finish => {
                self.push(HostAction::Finished);
                self.mode = Mode::Done;
            }
            After::Fail { track, fault } => {
                self.push(HostAction::Failed { track, fault });
                self.mode = Mode::Done;
            }
        }
    }

    /// 处理宿主回报。核心回报属于旧代次时返回 [`CoreError::Stale`]（宿主不得写入其数据）。
    pub fn handle(&mut self, event: HostEvent) -> Result<(), CoreError> {
        let core_event = match event {
            HostEvent::Opened { generation } => Some(Event::Opened { generation }),
            HostEvent::OpenFailed { generation } => Some(Event::OpenFailed { generation }),
            HostEvent::ReadDone {
                generation,
                request,
                len,
            } => Some(Event::ReadDone {
                generation,
                request,
                len,
            }),
            HostEvent::ReadEof {
                generation,
                request,
            } => Some(Event::ReadEof {
                generation,
                request,
            }),
            HostEvent::ReadFailed {
                generation,
                request,
            } => Some(Event::ReadFailed {
                generation,
                request,
            }),
            HostEvent::SeekDone {
                generation,
                request,
            } => Some(Event::SeekDone {
                generation,
                request,
            }),
            HostEvent::SeekFailed {
                generation,
                request,
            } => Some(Event::SeekFailed {
                generation,
                request,
            }),
            _ => None,
        };
        if let Some(e) = core_event {
            return self.core.handle(e);
        }
        match (event, self.mode) {
            (HostEvent::DeviceSelected { format }, Mode::WaitSelect { generation, source }) => {
                match self.device {
                    Some(d) if d.format == format => {
                        self.mode = Mode::Run;
                        self.configured(generation);
                    }
                    Some(_) => self.begin_drain(After::Open {
                        generation,
                        source,
                        format: Some(format),
                    }),
                    None => {
                        self.push(HostAction::OpenDevice {
                            source,
                            format: Some(format),
                        });
                        self.mode = Mode::WaitOpen { generation };
                    }
                }
            }
            (HostEvent::DeviceUnsupported, Mode::WaitSelect { generation, .. })
            | (HostEvent::DeviceOpenFailed, Mode::WaitOpen { generation }) => {
                self.mode = Mode::Run;
                match self.options.fallback() {
                    Fallback::Retry(laid) => self.request_device(generation, laid),
                    Fallback::Reject(e) => {
                        self.reject = Some(EngineFault::Capability(e));
                        self.core_handle(Event::ConfigureFailed { generation });
                    }
                    Fallback::None => self.core_handle(Event::ConfigureFailed { generation }),
                }
            }
            (HostEvent::DeviceOpened { format }, Mode::WaitOpen { generation }) => {
                self.device = Some(Dev {
                    format,
                    running: false,
                });
                self.mode = Mode::Run;
                self.configured(generation);
            }
            (HostEvent::DeviceDrained, Mode::Drain(after)) => {
                if let Some(d) = self.device.as_mut() {
                    d.running = false;
                }
                self.after_drain(after);
            }
            // 错误已返回给宿主，由宿主计数；`stats.protocol_errors` 只记录引擎内部消化的错误。
            _ => return Err(CoreError::Protocol),
        }
        Ok(())
    }

    /// 取得下一个宿主操作，或说明正在等待什么。
    pub fn poll(&mut self) -> Poll {
        for _ in 0..64 {
            if let Some(a) = self.take_control() {
                return Poll::Action(a);
            }
            match self.mode {
                Mode::Idle | Mode::Done => return Poll::Idle,
                Mode::WaitSelect { .. } | Mode::WaitOpen { .. } | Mode::Drain(_) => {
                    return Poll::Wait(Wait::Device);
                }
                Mode::Run => {}
            }
            let polled = self.core.poll();
            if self.core.take_sequential_seek() {
                self.push(HostAction::SeekSequential {
                    track: self.core.track(),
                });
            }
            let frames = self.core.take_prerolled();
            if frames > 0 {
                self.push(HostAction::SeekPreroll {
                    track: self.core.track(),
                    frames,
                });
            }
            let Some(action) = polled else {
                return if self.core.is_idle() {
                    Poll::Idle
                } else {
                    Poll::Wait(Wait::Io)
                };
            };
            match action {
                Action::Open { generation, track } => {
                    self.open_track = Some(track);
                    return Poll::Action(HostAction::Open { generation, track });
                }
                Action::Read {
                    generation,
                    request,
                    max,
                } => {
                    return Poll::Action(HostAction::Read {
                        generation,
                        request,
                        max,
                    });
                }
                Action::Seek {
                    generation,
                    request,
                    offset,
                } => {
                    return Poll::Action(HostAction::Seek {
                        generation,
                        request,
                        offset,
                    });
                }
                Action::Close { generation } => {
                    self.report_repairs();
                    return Poll::Action(HostAction::Close { generation });
                }
                Action::Configure { generation, format } => self.on_configure(generation, format),
                Action::Output { generation, .. } => {
                    if let Some(w) = self.on_output(generation) {
                        if let Some(a) = self.take_control() {
                            return Poll::Action(a);
                        }
                        return Poll::Wait(w);
                    }
                }
                Action::Finished { .. } => self.begin_drain(After::Finish),
                Action::Failed { track, fault, .. } => self.on_failed(track, fault),
            }
        }
        // 每轮都有进展；超过轮数只是让出，下次继续。
        Poll::Yield
    }

    /// 设备已按本曲目配置好（新打开或续用）。浮点源配到整数设备时（只在明确开启转换、经回退
    /// 协商时发生）锁定转换位数。“声道已改变”“样本已改变”在每首曲目第一次入队时报告
    /// （[`Engine::announce`]）：相同格式的相邻曲目不重新配置，但每首都要报告；配置失败的曲目没有
    /// 输出，不报告。
    fn configured(&mut self, generation: Generation) {
        self.options.configured(self.device.map(|d| d.format));
        self.core_handle(Event::Configured { generation });
    }

    /// 最近一次产出样本的曲目序号与源格式（[`Core::track_format`]）。
    pub fn track_format(&self) -> Option<(u32, SourceFormat)> {
        self.core.track_format()
    }

    /// 当前曲目是否按 [`ChannelPolicy::DuplicateMono`] 配置（打开设备时宿主据此在协商记录中写明
    /// 源的实际格式）。
    pub fn duplicating(&self) -> bool {
        self.options.duplicating()
    }

    fn on_configure(&mut self, generation: Generation, actual: SourceFormat) {
        let laid = self.options.begin(actual);
        if self.options.reuse_converted(
            laid,
            self.device.map(|d| d.format),
            self.cfg.reuse,
            &self.cfg.caps,
        ) {
            self.configured(generation);
            return;
        }
        self.request_device(generation, laid);
    }

    /// 为当前回退阶段的源格式检查能力，并续用、选择或打开设备。
    fn request_device(&mut self, generation: Generation, laid: SourceFormat) {
        let source = loop {
            let Some(source) = self.options.presentation(laid) else {
                self.core_handle(Event::ConfigureFailed { generation });
                return;
            };
            match self.cfg.caps.check(&source) {
                Ok(()) => break source,
                Err(e) => match self.options.check_failed(laid, e) {
                    Next::Retry => {}
                    Next::Reject(e) => {
                        self.reject = Some(EngineFault::Capability(e));
                        self.core_handle(Event::ConfigureFailed { generation });
                        return;
                    }
                },
            }
        };
        if let Some(d) = self.device
            && self.cfg.reuse == ReusePolicy::Accepts
            && d.format.accepts(&source)
        {
            self.configured(generation);
            return;
        }
        match self.cfg.reuse {
            ReusePolicy::Identical => {
                self.push(HostAction::SelectDevice { source });
                self.mode = Mode::WaitSelect { generation, source };
            }
            ReusePolicy::Accepts => {
                if self.device.is_some() {
                    self.begin_drain(After::Open {
                        generation,
                        source,
                        format: None,
                    });
                } else {
                    self.push(HostAction::OpenDevice {
                        source,
                        format: None,
                    });
                    self.mode = Mode::WaitOpen { generation };
                }
            }
        }
    }

    /// pack 并入队；队列满时返回等待原因。
    fn on_output(&mut self, generation: Generation) -> Option<Wait> {
        let track = self.core.track();
        let new_track = self.marks[..self.marks_len]
            .last()
            .is_none_or(|m| m.0 != track);
        if self.device.is_some() && new_track && !self.reserve_mark() {
            // 队列里的曲目边界已满：先不入队，等设备播完旧曲目（与队列满同样处理）。
            self.core_handle(Event::Output {
                generation,
                frames: 0,
            });
            self.maybe_start(true);
            return Some(Wait::QueueFull);
        }
        let (Some(pcm), Some(dev)) = (self.core.pcm(), self.device) else {
            self.stats.protocol_errors += 1;
            self.fail_now(EngineFault::Output);
            return None;
        };
        let start = self.stats.frames_pushed;
        let source = pcm.format();
        let mode = self.options.pack_mode(source, dev.format);
        let outcome = self
            .options
            .push(&pcm, &mut self.sink, &dev.format, mode, self.scratch);
        let (pushed, stats, nonfinite, stop_at) = match outcome {
            Pushed::Frames {
                frames,
                stats,
                nonfinite,
            } => (frames, stats, nonfinite, None),
            Pushed::NonFinite { frames, stats } => (
                frames,
                stats,
                0,
                Some(self.track_frame.saturating_add(frames as u64)),
            ),
            Pushed::Error => {
                self.fail_now(EngineFault::Output);
                return None;
            }
        };
        if pushed > 0 {
            self.announce(track, source);
            self.options.add_stats(nonfinite, &stats);
        }
        self.track_frame = self.track_frame.saturating_add(pushed as u64);
        if new_track
            && pushed > 0
            && let Some(m) = self.marks.get_mut(self.marks_len)
        {
            let base = match self.seek_base.take() {
                Some((t, frame)) if t == track => frame,
                _ => 0,
            };
            *m = (track, start, base);
            self.marks_len += 1;
        }
        self.stats.frames_pushed += pushed as u64;
        self.core_handle(Event::Output {
            generation,
            frames: pushed,
        });
        if let Some(frame) = stop_at {
            // 非有限样本之前的帧已入队；该曲目在此停止（F3 §4）。
            self.fail_track(EngineFault::NonFinite { frame });
            return None;
        }
        self.maybe_start(pushed == 0);
        (pushed == 0).then_some(Wait::QueueFull)
    }

    /// 曲目第一次入队：每首曲目报告一次声道复制（O1）与浮点转换（F3），并开始浮点统计。
    fn announce(&mut self, track: u32, source: SourceFormat) {
        if self.announced == Some(track) {
            return;
        }
        self.announced = Some(track);
        let a = self.options.first_output(track, source);
        if a.duplicated {
            self.stats.duplicated_tracks += 1;
            self.push(HostAction::ChannelsDuplicated { track });
        }
        if let Some(bits) = a.converted {
            self.stats.converted_tracks += 1;
            self.push(HostAction::FloatConverted { track, bits });
        }
        if let Some((track, report)) = a.finished {
            self.push(HostAction::FloatStats { track, report });
        }
    }

    /// 给出当前浮点曲目的统计（关闭、停止或定位时）。
    fn report_float(&mut self) {
        if let Some((track, report)) = self.options.take_float_report() {
            self.push(HostAction::FloatStats { track, report });
        }
    }

    /// 引擎判定的曲目失败（F3：转换遇到非有限样本）：关闭音源后按出错策略跳过或停止，已入队的
    /// 音频照常播放。
    fn fail_track(&mut self, fault: EngineFault) {
        let track = self.core.track();
        self.report_repairs();
        if let Some(Action::Close { generation }) = self.core.stop() {
            self.push(HostAction::Close { generation });
        }
        self.reject = Some(fault);
        self.on_failed(track, Fault::Decode(crate::DecodeError::Malformed));
    }

    /// 核心关闭曲目时（解码器尚未复位），把解码器记录的结构修正逐条交给宿主，并给出浮点统计。
    /// 此时控制队列为空（`poll` 先取完控制动作才推进核心），修正至多 [`MAX_REPAIRS`] 条，加上
    /// 浮点统计一条，放得下。
    fn report_repairs(&mut self) {
        self.report_float();
        self.track_frame = 0;
        let Some(track) = self.open_track.take() else {
            return;
        };
        let n = self.core.decoder().repair_count().min(MAX_REPAIRS);
        for i in 0..n {
            if let Some(repair) = self.core.decoder().repair(i) {
                self.stats.repairs = self.stats.repairs.saturating_add(1);
                self.push(HostAction::Repaired { track, repair });
            }
        }
    }

    fn fail_now(&mut self, fault: EngineFault) {
        let track = self.core.track();
        if let Some(Action::Close { generation }) = self.core.stop() {
            self.push(HostAction::Close { generation });
        }
        self.begin_drain(After::Fail { track, fault });
    }

    fn on_failed(&mut self, track: u32, fault: Fault) {
        // R1：文件头阶段因本机解码容量不足的拒绝，按能力不足报告具体原因（与引擎的能力检查同类）。
        let fault = match (self.reject.take(), fault) {
            (Some(r), _) => r,
            (None, Fault::Decode(crate::DecodeError::CapacityExceeded)) => {
                match self.core.decoder().capacity_rejection() {
                    Some(c) => EngineFault::Capability(c),
                    None => EngineFault::Core(fault),
                }
            }
            (None, f) => EngineFault::Core(f),
        };
        let next = track.saturating_add(1);
        if self.cfg.on_error == ErrorPolicy::Skip
            && !matches!(fault, EngineFault::Output)
            && next < self.track_count
        {
            self.stats.skipped += 1;
            self.push(HostAction::TrackSkipped { track, fault });
            self.core.play(next, self.track_count);
            return;
        }
        self.begin_drain(After::Fail { track, fault });
    }
}

impl<D: Decoder + Resources, S: FrameSink> fmt::Debug for Engine<D, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine")
            .field("mode", &self.mode)
            .field("device", &self.device)
            .field("paused", &self.paused)
            .field("stats", &self.stats)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use qy_pcm_format::{Endian, Placement};
    use std::format;

    /// M2 T5：只用默认实现的解码器“无额外说明”：没有修正、没有需求、不提前拒绝。
    #[test]
    fn resources_defaults_report_nothing() {
        struct Plain;
        impl Resources for Plain {}
        let p = core::hint::black_box(Plain);
        assert_eq!(p.repair_count(), 0);
        assert_eq!(p.repair(0), None);
        assert_eq!(p.demand(), None);
        assert_eq!(p.capacity_rejection(), None);
    }

    /// 块标识按 ASCII 显示：空格原样保留，不可显示的字节显示为 `?`。
    #[test]
    fn repair_text_shows_chunk_id() {
        let r = Repair::LengthMismatch {
            chunk: *b"fmt ",
            declared: 16,
            actual: 18,
        };
        assert_eq!(
            format!("{r}"),
            "fmt  声明长度 16 字节与文件实际的 18 字节不符，以 data 的声明长度为准"
        );
        let r = Repair::LengthPlaceholder {
            chunk: [b'd', 0, 0x7F, b'\n'],
            declared: 0xFFFF_FFFF,
            read: 4,
        };
        assert!(format!("{r}").starts_with("d??? 长度为流式占位值 0xffffffff"));
    }

    /// 播放计划的显示：设备未定、最大输入单元未声明（0）与已声明；单元叫法由解码器给出（这里用
    /// 测试格式“TST”），未给出时用通用叫法。
    #[test]
    fn plan_text_distinguishes_undeclared_max_frame() {
        let (Ok(source), Ok(device)) = (
            SourceFormat::new(44_100, 2, 24),
            DeviceFormat::new(44_100, 2, 32, 24, Endian::Little, Placement::MsbAligned),
        ) else {
            panic!("格式");
        };
        let demand = DecoderDemand {
            max_unit_frames: 4_608,
            unit_frames_capacity: 65_535,
            max_unit_bytes: 0,
            window: 1_048_576,
            decode_unit: &Unit {
                codec: "TST",
                name: "块",
            },
            input_unit: &Unit {
                codec: "TST",
                name: "帧",
            },
        };
        let mut plan = PlaybackPlan {
            source,
            device: None,
            prefill_frames: 22_050,
            prefill_ms: 500,
            queue_required: 132_300,
            queue_available: 262_144,
            decoder: Some(demand),
        };
        assert_eq!(
            format!("{plan}"),
            "源 44100 Hz / 2 声道 / 24 位，设备未定，预填 22050 帧（500 ms），队列 132300/262144 字节，\
             TST 最大块 4608/65535 帧，最大帧 未声明/1048576 字节"
        );
        plan.device = Some(device);
        plan.decoder = Some(DecoderDemand {
            max_unit_bytes: 18_000,
            ..demand
        });
        let text = format!("{plan}");
        assert!(text.contains("设备 44100 Hz / 2 声道 / 32-24 位"), "{text}");
        assert!(text.ends_with("最大帧 18000/1048576 字节"), "{text}");
        plan.decoder = Some(DecoderDemand {
            decode_unit: &Unit::DECODE,
            input_unit: &Unit::INPUT,
            ..demand
        });
        let text = format!("{plan}");
        assert!(
            text.ends_with("最大解码单元 4608/65535 帧，最大输入单元 未声明/1048576 字节"),
            "{text}"
        );
    }

    /// 能力不足的文字：单元叫法由解码器给出时与 R1 起的原文相同；未给出时用通用叫法。
    #[test]
    fn unit_capability_text() {
        use std::string::ToString;
        let block = &Unit {
            codec: "TST",
            name: "块",
        };
        let frame = &Unit {
            codec: "TST",
            name: "帧",
        };
        let d = CapabilityError::DecodeUnit {
            frames: 4_608,
            max: 1_152,
            unit: block,
        };
        assert_eq!(d.to_string(), "TST 最大块 4608 帧超出本机配置的 1152 帧");
        let i = CapabilityError::InputUnit {
            bytes: 40_000,
            window: 28_244,
            unit: frame,
        };
        assert_eq!(
            i.to_string(),
            "TST 声明的最大帧 40000 字节超出本机输入窗口 28244 字节"
        );
        let g = CapabilityError::InputUnit {
            bytes: 40_000,
            window: 28_244,
            unit: &Unit::INPUT,
        };
        assert_eq!(
            g.to_string(),
            "声明的最大输入单元 40000 字节超出本机输入窗口 28244 字节"
        );
    }
}
