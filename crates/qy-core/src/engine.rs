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
    /// FLAC 文件头声明的最大块超出本机解码容量（R1）。
    FlacBlock {
        block: u32,
        max: u32,
    },
    /// FLAC 文件头声明的最大帧超出本机输入窗口（R1）。
    FlacFrame {
        bytes: u32,
        window: u32,
    },
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
            Self::FlacBlock { block, max } => {
                write!(f, "FLAC 最大块 {block} 帧超出本机配置的 {max} 帧")
            }
            Self::FlacFrame { bytes, window } => write!(
                f,
                "FLAC 声明的最大帧 {bytes} 字节超出本机输入窗口 {window} 字节"
            ),
        }
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
        if src.bits() > self.max_bits {
            return Err(CapabilityError::Bits {
                bits: src.bits(),
                max: self.max_bits,
            });
        }
        let required = usize::try_from(self.prefill_frames(src.sample_rate()))
            .ok()
            .and_then(|f| f.checked_mul(usize::from(src.channels())))
            .and_then(|s| s.checked_mul(min_container_bytes(src.bits())))
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
}

/// 解码器的需求与容量（文件头声明值，不可信，只用于报告）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecoderDemand {
    /// 声明的最大块（帧）与本机容量。
    pub max_block: u32,
    pub block_capacity: u32,
    /// 声明的最大帧字节数（0 表示未声明）与本机输入窗口。
    pub max_frame: u32,
    pub window: u32,
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
        write!(
            f,
            "源 {} Hz / {} 声道 / {} 位",
            s.sample_rate(),
            s.channels(),
            s.bits()
        )?;
        match &self.device {
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
                "，FLAC 最大块 {}/{} 帧，最大帧 ",
                d.max_block, d.block_capacity
            )?;
            if d.max_frame == 0 {
                f.write_str("未声明")?;
            } else {
                write!(f, "{}", d.max_frame)?;
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
    outbox: [Option<HostAction>; OUTBOX],
    out_len: usize,
    /// 曲目边界：(曲目, 该曲目第一帧在累计入队帧中的位置)，按入队顺序。
    marks: [(u32, u64); MARKS],
    marks_len: usize,
    stats: EngineStats,
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
            outbox: [None; OUTBOX],
            out_len: 0,
            marks: [(0, 0); MARKS],
            marks_len: 0,
            stats: EngineStats::default(),
        }
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
        let consumed = self.consumed_frames();
        self.marks[..self.marks_len]
            .iter()
            .rev()
            .find(|m| m.1 <= consumed)
            .or(self.marks[..self.marks_len].first())
            .map_or(self.core.track(), |m| m.0)
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
            None => usize::from(source.channels()) * min_container_bytes(source.bits()),
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
        self.track_count = count;
        self.reject = None;
        self.marks_len = 0;
        self.core.play(first, count);
        self.mode = Mode::Run;
    }

    /// 停止：关闭音源（旧代次的回报此后一律拒绝），停止设备并丢弃队列。
    pub fn stop(&mut self) {
        if let Some(Action::Close { generation }) = self.core.stop() {
            self.push(HostAction::Close { generation });
        }
        if self.device.take().is_some() {
            self.push(HostAction::StopDevice);
        }
        self.paused = false;
        self.reject = None;
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
                        self.core_handle(Event::Configured { generation });
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
                self.core_handle(Event::ConfigureFailed { generation });
            }
            (HostEvent::DeviceOpened { format }, Mode::WaitOpen { generation }) => {
                self.device = Some(Dev {
                    format,
                    running: false,
                });
                self.mode = Mode::Run;
                self.core_handle(Event::Configured { generation });
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
            let Some(action) = self.core.poll() else {
                return if self.core.is_idle() {
                    Poll::Idle
                } else {
                    Poll::Wait(Wait::Io)
                };
            };
            match action {
                Action::Open { generation, track } => {
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
                Action::Close { generation } => {
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

    fn on_configure(&mut self, generation: Generation, source: SourceFormat) {
        if let Err(e) = self.cfg.caps.check(&source) {
            self.reject = Some(EngineFault::Capability(e));
            self.core_handle(Event::ConfigureFailed { generation });
            return;
        }
        if let Some(d) = self.device
            && self.cfg.reuse == ReusePolicy::Accepts
            && d.format.accepts(&source)
        {
            self.core_handle(Event::Configured { generation });
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
        let pushed = match self.sink.push(&pcm, &dev.format, self.scratch) {
            Ok(n) => n,
            Err(_) => {
                self.fail_now(EngineFault::Output);
                return None;
            }
        };
        if new_track
            && pushed > 0
            && let Some(m) = self.marks.get_mut(self.marks_len)
        {
            *m = (track, start);
            self.marks_len += 1;
        }
        self.stats.frames_pushed += pushed as u64;
        self.core_handle(Event::Output {
            generation,
            frames: pushed,
        });
        self.maybe_start(pushed == 0);
        (pushed == 0).then_some(Wait::QueueFull)
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
