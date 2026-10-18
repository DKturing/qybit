//! 主循环宿主：执行引擎的宿主操作，接存储、音频输出与共享喂数器；处理串口命令。
//!
//! 主循环形如：
//!
//! ```text
//! loop {
//!     while let Some(b) = 串口.读字节() { if let Some(c) = 行缓冲.push(b) { player.command(c?, 串口)?; } }
//!     while let Some(n) = player.pop_notice() { 打印 n }
//!     if player.step() == Step::Idle { 板级.等待中断() }   // WFI：DMA 或串口中断唤醒
//! }
//! ```
//!
//! [`Player::step`] 每次至多执行一个引擎动作，其中读取至多 `read_chunk` 字节，因此
//! 串口命令的响应延迟有界。播放规则（预填、续用、排空、暂停、切歌、出错跳过、能力检查）全部在
//! `qy_core::engine`，这里不重复实现。

use core::fmt;

use qy_core::engine::{
    ChannelPolicy, Engine, EngineFault, EngineStats, ErrorPolicy, FloatPolicy, FloatReport,
    HostAction, HostEvent, PlaybackPlan, Poll, Repair, Resources, Wait,
};
use qy_core::{Decoder, DeviceFormat, Fault};
use qy_pcm_format::frames_for_millis;
use qy_pcm_format::queue::Watermark;
use qy_pcm_format::static_queue::{StaticProducer, reconfigure};

use crate::budget::{self, BudgetError, Load, Margins};
use crate::command::{Command, ParseError};
use crate::feeder::{FeederAccess, OutputStats};
use crate::output::{AudioOutput, SelectError};
use crate::power::Activity;
use crate::storage::Storage;

/// [`Player::step`] 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// 做了事，可以立即再调用。
    Busy,
    /// 在等中断（DMA 取数、排空）或无事可做：可以进入低功耗等待。
    Idle,
}

/// 异步通知（经串口报告）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// 开始播放第 `track` 首（音源已打开）。
    Opened { track: u32 },
    /// 设备按格式打开。
    Device { format: DeviceFormat },
    /// 设备打开后的播放计划（R1）。
    Plan { plan: PlaybackPlan },
    /// 设备格式选择失败。
    Unsupported { error: SelectError },
    /// 曲目出错，已跳过。
    Skipped { track: u32, fault: EngineFault },
    /// 曲目在不改样本的前提下做过的一条结构修正（W1）。
    Repaired { track: u32, repair: Repair },
    /// 曲目按明确开启的选项把单声道复制到左右两路（O1：声道布局已改变，样本值未改变）。
    ChannelsDuplicated { track: u32 },
    /// 曲目的定位改为顺序解码到目标位置（S1：存储不支持定位或位置越界；结果相同，只是更慢）。
    SeekSequential { track: u32 },
    /// 曲目的浮点样本按明确开启的选项转换为 `bits` 位整数（F3：样本已改变）。
    FloatConverted { track: u32, bits: u8 },
    /// 浮点曲目的统计（F3）。
    FloatStats { track: u32, report: FloatReport },
    /// 播放列表结束。
    Finished,
    /// 播放失败并停止。
    Failed { track: u32, fault: EngineFault },
    /// 输出设备出错（配置、启动或喂数器不可用），播放停止。
    OutputError,
}

/// 宿主计数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub open_errors: u32,
    pub read_errors: u32,
    /// 解码错误（含截断、格式错误、超出解码器容量）而跳过或停止的曲目数。
    pub decode_errors: u32,
    /// 本机能力不足或设备格式不支持而跳过或停止的曲目数。
    pub unsupported: u32,
    pub skipped: u32,
    pub output_errors: u32,
    /// 引擎拒绝的回报（旧代次以外的协议错误，应为 0）。
    pub protocol_errors: u32,
    /// 通知队列满而丢弃的通知数。
    pub notices_dropped: u32,
    /// 结构修正条数（W1：只修正结构，不改样本）。
    pub repairs: u32,
    /// 单声道复制到左右两路的曲目数（O1：明确开启，声道布局已改变）。
    pub duplicated: u32,
    /// 浮点转为整数的曲目数（F3：明确开启，样本已改变）。
    pub converted: u32,
}

/// 状态快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub active: bool,
    pub paused: bool,
    pub track: u32,
    pub tracks: u32,
    pub device: Option<DeviceFormat>,
    pub queued_frames: usize,
    pub counters: Counters,
    pub output: OutputStats,
    pub engine: EngineStats,
    /// 单声道复制到左右两路是否开启（O1，非严格选项）。
    pub mono_to_stereo: bool,
    /// 浮点转整数是否开启（F3，非严格选项）。
    pub float_to_int: bool,
}

const NOTICES: usize = 8;

/// 通用嵌入式宿主。`N` 为静态帧队列存储字节数。
pub struct Player<D, S, O, F, const N: usize>
where
    D: Decoder + Resources,
    S: Storage,
    O: AudioOutput,
    F: FeederAccess<N>,
{
    engine: Engine<D, StaticProducer<N>>,
    storage: S,
    output: O,
    feeder: F,
    read_chunk: u32,
    margins: Margins,
    /// 设备已打开（DMA 可能在运行）。
    device: Option<DeviceFormat>,
    running: bool,
    draining: bool,
    ring_generation: u64,
    open_track: Option<u32>,
    counters: Counters,
    notices: [Option<Notice>; NOTICES],
    notice_len: usize,
    /// 串口 `seek` 收到时曲目格式还不知道（刚开始播放）：毫秒数待格式确定后换算并定位（S1）。
    pending_seek: Option<u32>,
}

impl<D, S, O, F, const N: usize> Player<D, S, O, F, N>
where
    D: Decoder + Resources,
    S: Storage,
    O: AudioOutput,
    F: FeederAccess<N>,
{
    /// 创建宿主，并按 [`budget::check`] 逐格式核对板级时序与配置档：预填或负载不足时返回错误，
    /// 不启动。`read_chunk` 为主循环每步至多读取的字节数，`loads` 为启用的每种格式的需求与实测时序。
    pub fn new(
        engine: Engine<D, StaticProducer<N>>,
        storage: S,
        output: O,
        feeder: F,
        read_chunk: u32,
        loads: &[Load],
    ) -> Result<Self, BudgetError> {
        let caps = engine.config().caps;
        let margins = budget::check(&caps, loads, output.caps().max_period_us())?;
        Ok(Self {
            engine,
            storage,
            output,
            feeder,
            read_chunk,
            margins,
            device: None,
            running: false,
            draining: false,
            ring_generation: 0,
            open_track: None,
            counters: Counters::default(),
            notices: [None; NOTICES],
            notice_len: 0,
            pending_seek: None,
        })
    }

    pub fn margins(&self) -> Margins {
        self.margins
    }

    pub fn engine(&self) -> &Engine<D, StaticProducer<N>> {
        &self.engine
    }

    pub fn storage(&self) -> &S {
        &self.storage
    }

    pub fn storage_mut(&mut self) -> &mut S {
        &mut self.storage
    }

    pub fn output(&self) -> &O {
        &self.output
    }

    pub fn set_error_policy(&mut self, policy: ErrorPolicy) {
        self.engine.set_error_policy(policy);
    }

    /// 声道策略（默认严格）。[`ChannelPolicy::DuplicateMono`] 是明确开启的非严格选项（O1）：
    /// 单声道 `[FC]` 按立体声 `[FL FR]` 打开设备，样本原样复制到左右两路，每首都发出
    /// [`Notice::ChannelsDuplicated`]。板级可在启动时按配置设置，也可用串口命令 `mono-to-stereo`。
    /// 对之后开始的曲目生效。
    pub fn set_channel_policy(&mut self, policy: ChannelPolicy) {
        self.engine.set_channel_policy(policy);
    }

    /// 浮点策略（默认严格）。[`FloatPolicy::ToInt`] 是明确开启的非严格选项（F3）：I²S 只输出整数，
    /// 浮点文件只在开启时转换播放（样本已改变），每首都发出 [`Notice::FloatConverted`]；需要
    /// 板级经 [`Player::set_convert_workspace`] 提供工作区（Embedded Lite 不提供，开启时按资源不足
    /// 拒绝）。板级可在启动时按配置设置，也可用串口命令 `float-to-int`。对之后开始的曲目生效。
    pub fn set_float_policy(&mut self, policy: FloatPolicy) {
        self.engine.set_float_policy(policy);
    }

    /// 浮点转整数的工作区（板级启动时提供，静态区）。
    pub fn set_convert_workspace(&mut self, ws: &'static mut [i32]) {
        self.engine.set_convert_workspace(ws);
    }

    fn notice(&mut self, n: Notice) {
        if let Some(slot) = self.notices.get_mut(self.notice_len) {
            *slot = Some(n);
            self.notice_len += 1;
        } else {
            self.counters.notices_dropped = self.counters.notices_dropped.saturating_add(1);
        }
    }

    /// 取出最早的通知。
    pub fn pop_notice(&mut self) -> Option<Notice> {
        if self.notice_len == 0 {
            return None;
        }
        let n = self.notices[0].take();
        self.notices.copy_within(1..self.notice_len, 0);
        self.notice_len -= 1;
        self.notices[self.notice_len] = None;
        n
    }

    pub fn status(&self) -> Status {
        let (output, queued_frames) = self
            .feeder
            .with(|f| (f.stats(), f.queued_frames()))
            .unwrap_or_default();
        Status {
            // 以引擎状态为准（播放结束后的上一首/下一首会重新开始播放）。
            active: self.engine.is_active(),
            paused: self.engine.is_paused(),
            track: self.engine.playing_track(),
            tracks: self.engine.track_count(),
            device: self.device,
            queued_frames,
            counters: self.counters,
            output,
            engine: self.engine.stats(),
            mono_to_stereo: self.engine.channel_policy() == ChannelPolicy::DuplicateMono,
            float_to_int: self.engine.float_policy() == FloatPolicy::ToInt,
        }
    }

    // ---- 控制 ----

    /// 从第 `first` 首开始播放整个列表。
    pub fn play(&mut self, first: u32) -> bool {
        self.pending_seek = None;
        let count = self.storage.track_count();
        if first >= count {
            return false;
        }
        self.engine.stop();
        self.flush_control();
        self.engine.play(first, count);
        true
    }

    pub fn stop(&mut self) {
        self.pending_seek = None;
        self.engine.stop();
        self.flush_control();
    }

    pub fn pause(&mut self) {
        self.engine.pause();
        self.flush_control();
    }

    pub fn resume(&mut self) {
        self.engine.resume();
        self.flush_control();
    }

    pub fn next(&mut self) {
        self.pending_seek = None;
        self.engine.next();
        self.flush_control();
    }

    pub fn prev(&mut self) {
        self.pending_seek = None;
        self.engine.prev();
        self.flush_control();
    }

    /// 立即执行控制动作（关闭音源、停止设备、暂停/恢复），不推进解码。
    fn flush_control(&mut self) {
        while let Some(a) = self.engine.take_control() {
            self.execute(a);
        }
    }

    // ---- 主循环 ----

    /// 推进一步。
    pub fn step(&mut self) -> Step {
        if self.draining && self.feeder.with(|f| f.is_drained()).unwrap_or(true) {
            self.draining = false;
            self.output.stop();
            self.running = false;
            let _ = self.feeder.with(|f| f.set_running(false));
            self.handle(HostEvent::DeviceDrained);
            return Step::Busy;
        }
        // 待定的定位：曲目格式确定后（入队任何音频之前）换算成帧并定位。
        if let Some(ms) = self.pending_seek {
            if !self.engine.is_active() {
                self.pending_seek = None;
            } else if let Some(rate) = self.track_rate() {
                self.pending_seek = None;
                self.engine.seek(u64::from(ms) * u64::from(rate) / 1000);
                self.flush_control();
                return Step::Busy;
            }
        }
        match self.engine.poll() {
            Poll::Action(a) => {
                self.execute(a);
                Step::Busy
            }
            Poll::Yield => Step::Busy,
            Poll::Wait(Wait::QueueFull | Wait::Device | Wait::Io) => Step::Idle,
            // 未播放，或播放列表已结束/失败/停止。
            Poll::Idle => Step::Idle,
        }
    }

    /// 正在播放的曲目的采样率：设备已打开时取设备格式，否则取已解析的源格式。
    fn track_rate(&self) -> Option<u32> {
        self.engine
            .device_format()
            .map(|d| d.sample_rate())
            .or_else(|| qy_core::Decoder::format(self.engine.decoder()).map(|f| f.sample_rate()))
    }

    fn handle(&mut self, e: HostEvent) {
        match self.engine.handle(e) {
            Ok(()) | Err(qy_core::CoreError::Stale) => {}
            Err(_) => {
                self.counters.protocol_errors = self.counters.protocol_errors.saturating_add(1)
            }
        }
    }

    fn output_failed(&mut self) {
        self.output.stop();
        self.running = false;
        self.draining = false;
        self.device = None;
        let _ = self.feeder.with(|f| f.set_running(false));
        self.counters.output_errors = self.counters.output_errors.saturating_add(1);
        self.engine.device_lost();
        self.flush_control();
        self.notice(Notice::OutputError);
    }

    fn count_fault(&mut self, fault: EngineFault) {
        let c = &mut self.counters;
        match fault {
            EngineFault::Core(Fault::Decode(_) | Fault::InputWindow)
            | EngineFault::NonFinite { .. } => c.decode_errors = c.decode_errors.saturating_add(1),
            EngineFault::Core(Fault::Configure) | EngineFault::Capability(_) => {
                c.unsupported = c.unsupported.saturating_add(1)
            }
            // 打开与读取错误在发生时已计数。
            EngineFault::Core(Fault::Open | Fault::Read) | EngineFault::Output => {}
        }
    }

    fn execute(&mut self, a: HostAction) {
        match a {
            HostAction::Open { generation, track } => match self.storage.open(track) {
                Ok(()) => {
                    self.open_track = Some(track);
                    self.notice(Notice::Opened { track });
                    self.handle(HostEvent::Opened { generation });
                }
                Err(_) => {
                    self.counters.open_errors = self.counters.open_errors.saturating_add(1);
                    self.handle(HostEvent::OpenFailed { generation });
                }
            },
            HostAction::Read {
                generation,
                request,
                max,
            } => self.read(generation, request, max),
            HostAction::Seek {
                generation,
                request,
                offset,
            } => {
                let event = if self.storage.seek(offset) {
                    HostEvent::SeekDone {
                        generation,
                        request,
                    }
                } else {
                    HostEvent::SeekFailed {
                        generation,
                        request,
                    }
                };
                self.handle(event);
            }
            HostAction::SeekSequential { track } => self.notice(Notice::SeekSequential { track }),
            HostAction::Close { .. } => {
                self.storage.close();
                self.open_track = None;
            }
            // 本宿主用 `ReusePolicy::Accepts`，引擎不会请求选择；收到即按不支持回报。
            HostAction::SelectDevice { .. } => self.handle(HostEvent::DeviceUnsupported),
            HostAction::OpenDevice { source, .. } => match self.open_device(&source) {
                Ok(format) => {
                    self.notice(Notice::Device { format });
                    self.handle(HostEvent::DeviceOpened { format });
                    if let Some(plan) = self.engine.plan() {
                        self.notice(Notice::Plan { plan });
                    }
                }
                Err(e) => {
                    if let Some(error) = e {
                        self.notice(Notice::Unsupported { error });
                    }
                    self.handle(HostEvent::DeviceOpenFailed);
                }
            },
            HostAction::StartDevice => self.start_output(false),
            HostAction::DrainDevice => self.start_output(true),
            HostAction::StopDevice => {
                self.output.stop();
                self.running = false;
                self.draining = false;
                self.device = None;
                let _ = self.feeder.with(|f| f.set_running(false));
            }
            HostAction::PauseDevice => {
                let _ = self.feeder.with(|f| f.set_paused(true));
            }
            HostAction::ResumeDevice => {
                let _ = self.feeder.with(|f| f.set_paused(false));
            }
            HostAction::TrackSkipped { track, fault } => {
                self.counters.skipped = self.counters.skipped.saturating_add(1);
                self.count_fault(fault);
                self.notice(Notice::Skipped { track, fault });
            }
            HostAction::Repaired { track, repair } => {
                self.counters.repairs = self.counters.repairs.saturating_add(1);
                self.notice(Notice::Repaired { track, repair });
            }
            HostAction::ChannelsDuplicated { track } => {
                self.counters.duplicated = self.counters.duplicated.saturating_add(1);
                self.notice(Notice::ChannelsDuplicated { track });
            }
            HostAction::FloatConverted { track, bits } => {
                self.counters.converted = self.counters.converted.saturating_add(1);
                self.notice(Notice::FloatConverted { track, bits });
            }
            HostAction::FloatStats { track, report } => {
                self.notice(Notice::FloatStats { track, report });
            }
            HostAction::Finished => {
                self.notice(Notice::Finished);
            }
            HostAction::Failed { track, fault } => {
                self.count_fault(fault);
                self.notice(Notice::Failed { track, fault });
            }
        }
    }

    fn read(&mut self, generation: qy_core::Generation, request: u32, max: usize) {
        let chunk = self.read_chunk as usize;
        let Some(dst) = self.engine.read_target(generation, request) else {
            // 旧代次：按失败回报，引擎丢弃（Stale）。
            self.handle(HostEvent::ReadFailed {
                generation,
                request,
            });
            return;
        };
        let n = dst.len().min(max).min(chunk.max(1));
        let event = match self.storage.read(&mut dst[..n]) {
            Ok(0) => HostEvent::ReadEof {
                generation,
                request,
            },
            Ok(len) => HostEvent::ReadDone {
                generation,
                request,
                len: len.min(n),
            },
            Err(_) => {
                self.counters.read_errors = self.counters.read_errors.saturating_add(1);
                HostEvent::ReadFailed {
                    generation,
                    request,
                }
            }
        };
        self.handle(event);
    }

    /// 打开设备：停止 DMA、选择格式、配置外设、重配置队列。`Err(Some)` 为格式不支持，
    /// `Err(None)` 为外设或队列错误。
    fn open_device(
        &mut self,
        source: &qy_core::SourceFormat,
    ) -> Result<DeviceFormat, Option<SelectError>> {
        self.output.stop();
        self.running = false;
        self.draining = false;
        self.device = None;
        let format = self.output.caps().select(source).map_err(Some)?;
        if self.output.configure(&format).is_err() {
            self.counters.output_errors = self.counters.output_errors.saturating_add(1);
            return Err(None);
        }
        let prefill = self.engine.config().caps.prefill_ms;
        let required = usize::try_from(frames_for_millis(format.sample_rate(), prefill))
            .ok()
            .and_then(|f| f.checked_mul(usize::from(format.block_align())))
            .ok_or(None)?;
        self.ring_generation += 1;
        let generation = self.ring_generation;
        let sink = self.engine.sink_mut();
        let prefill_frames = frames_for_millis(format.sample_rate(), prefill);
        let period = u64::from(self.output.caps().buffer_frames);
        let ok = self
            .feeder
            .with(|f| {
                let ok =
                    reconfigure(sink, f.reset_for_open(), &format, required, generation).is_ok();
                f.set_watermark(Watermark::for_stream(prefill_frames, period));
                ok
            })
            .unwrap_or(false);
        if !ok {
            self.counters.output_errors = self.counters.output_errors.saturating_add(1);
            return Err(None);
        }
        self.device = Some(format);
        Ok(format)
    }

    fn start_output(&mut self, drain: bool) {
        let ok = self
            .feeder
            .with(|f| {
                if !f.is_running() {
                    f.set_running(true);
                }
                if drain {
                    f.begin_drain();
                }
            })
            .is_some();
        if !ok {
            self.output_failed();
            return;
        }
        self.draining |= drain;
        if !self.running {
            if self.output.start().is_err() {
                self.output_failed();
                return;
            }
            self.running = true;
        }
    }

    // ---- 串口命令 ----

    /// 执行一条命令并把回应写到 `w`（`OK ...` / `ERR ...` 开头的行）。
    pub fn command(&mut self, cmd: Command, w: &mut impl fmt::Write) -> fmt::Result {
        match cmd {
            Command::List => {
                let n = self.storage.track_count();
                writeln!(w, "OK ls tracks={n}")?;
                for i in 0..n {
                    write!(w, "TRACK {i} ")?;
                    self.storage.track_name(i, w)?;
                    writeln!(w)?;
                }
                Ok(())
            }
            Command::Play(first) => {
                if self.play(first) {
                    writeln!(w, "OK play from={first}")
                } else {
                    writeln!(
                        w,
                        "ERR play no-track={first} tracks={}",
                        self.storage.track_count()
                    )
                }
            }
            Command::Pause => {
                self.pause();
                writeln!(w, "OK pause")
            }
            Command::Resume => {
                self.resume();
                writeln!(w, "OK resume")
            }
            Command::Stop => {
                self.stop();
                writeln!(w, "OK stop")
            }
            Command::Next => {
                self.next();
                writeln!(w, "OK next")
            }
            Command::Prev => {
                self.prev();
                writeln!(w, "OK prev")
            }
            Command::Status => write_status(w, &self.status()),
            // 板级测量主循环时间时自行处理 `stats`（[`Self::write_stats`]）；这里只有喂数器的水位。
            Command::Stats => self.write_stats(w, None),
            Command::Seek(ms) => {
                // 定位目标按正在播放的曲目的采样率换算（严格路径不重采样，源与设备采样率相同）；
                // 刚开始播放、格式还不知道时先记下，格式确定后再定位。
                let track = self.engine.playing_track();
                match (self.engine.is_active(), self.track_rate()) {
                    (true, Some(rate)) => {
                        let frame = u64::from(ms) * u64::from(rate) / 1000;
                        self.engine.seek(frame);
                        self.flush_control();
                        writeln!(w, "OK seek track={track} frame={frame}")
                    }
                    (true, None) => {
                        self.pending_seek = Some(ms);
                        writeln!(w, "OK seek track={track} pending ms={ms}")
                    }
                    _ => writeln!(w, "ERR seek idle"),
                }
            }
            Command::MonoToStereo(on) => {
                self.set_channel_policy(if on {
                    ChannelPolicy::DuplicateMono
                } else {
                    ChannelPolicy::Strict
                });
                if on {
                    writeln!(
                        w,
                        "OK mono-to-stereo=on altered mono[FC]->stereo[FL FR] samples-unchanged \
                         applies-to=next-tracks"
                    )
                } else {
                    writeln!(w, "OK mono-to-stereo=off strict applies-to=next-tracks")
                }
            }
            Command::FloatToInt(on) => {
                self.set_float_policy(if on {
                    FloatPolicy::ToInt
                } else {
                    FloatPolicy::Strict
                });
                if on {
                    writeln!(
                        w,
                        "OK float-to-int=on altered samples-changed applies-to=next-tracks"
                    )
                } else {
                    writeln!(w, "OK float-to-int=off strict applies-to=next-tracks")
                }
            }
            Command::Scan => {
                if self.engine.is_active() {
                    return writeln!(w, "ERR scan busy");
                }
                match self.storage.rescan() {
                    Ok(n) => writeln!(w, "OK scan tracks={n}"),
                    Err(_) => writeln!(w, "ERR scan storage"),
                }
            }
        }
    }

    /// 统计行（L1）：`STATS key=value ...`。`timing` 为板级测量的主循环时间与计时器频率（Hz）；
    /// 没有时写 `activity=unmeasured`。DMA 截止余量复用 R1 水位：中断取数前队列中的最少帧数。
    pub fn write_stats(
        &self,
        w: &mut impl fmt::Write,
        timing: Option<(&Activity, u32)>,
    ) -> fmt::Result {
        w.write_str("STATS")?;
        match timing {
            Some((a, hz)) if hz > 0 => {
                match a.active_permille() {
                    Some(p) => write!(w, " active_permille={p}")?,
                    None => w.write_str(" active_permille=none")?,
                }
                let max_us = u64::from(a.max_step) * 1_000_000 / u64::from(hz);
                write!(
                    w,
                    " steps={} max_step_us={max_us} sleeps={}",
                    a.steps, a.sleeps
                )?;
            }
            _ => w.write_str(" activity=unmeasured")?,
        }
        let s = self.status();
        let m = &s.output.watermark;
        match (
            m.min_frames,
            s.device.and_then(|d| m.min_ms(d.sample_rate())),
        ) {
            (Some(f), Some(ms)) => write!(w, " dma_min_frames={f} dma_min_ms={ms}")?,
            _ => w.write_str(" dma_min_frames=none")?,
        }
        writeln!(w, " underruns={}", s.output.underruns)
    }
}

/// 状态行：`STATUS key=value ...`。
pub fn write_status(w: &mut impl fmt::Write, s: &Status) -> fmt::Result {
    let state = match (s.active, s.paused) {
        (false, _) => "stopped",
        (true, true) => "paused",
        (true, false) => "playing",
    };
    write!(
        w,
        "STATUS state={state} track={} tracks={} ",
        s.track, s.tracks
    )?;
    match s.device {
        Some(d) => write!(
            w,
            "device={}/{}/{}-{} ",
            d.sample_rate(),
            d.channels(),
            d.container_bits(),
            d.valid_bits()
        )?,
        None => w.write_str("device=none ")?,
    }
    let c = s.counters;
    let o = s.output;
    write!(
        w,
        "queued={} underruns={} underrun_frames={} paused_fills={} data_frames={} \
         open_errors={} read_errors={} decode_errors={} unsupported={} skipped={} \
         output_errors={} feeder_errors={} protocol_errors={} stale={} notices_dropped={} \
         repairs={} duplicated={} mono_to_stereo={} converted={} float_to_int={}",
        s.queued_frames,
        o.underruns,
        o.underrun_frames,
        o.paused_fills,
        o.data_frames,
        c.open_errors,
        c.read_errors,
        c.decode_errors,
        c.unsupported,
        c.skipped,
        c.output_errors,
        o.errors,
        c.protocol_errors + s.engine.protocol_errors,
        s.engine.stale,
        c.notices_dropped,
        c.repairs,
        c.duplicated,
        if s.mono_to_stereo { "on" } else { "off" },
        c.converted,
        if s.float_to_int { "on" } else { "off" }
    )?;
    // R1 水位：当前设备流的最低队列帧数（及毫秒）、低水位与临界次数。
    let m = &o.watermark;
    match (
        m.min_frames,
        s.device.and_then(|d| m.min_ms(d.sample_rate())),
    ) {
        (Some(f), Some(ms)) => write!(w, " wm_min_frames={f} wm_min_ms={ms}")?,
        _ => w.write_str(" wm_min_frames=none")?,
    }
    writeln!(w, " wm_low={} wm_critical={}", m.low, m.critical)
}

/// 通知行：`EVENT ...`。
pub fn write_notice(w: &mut impl fmt::Write, n: &Notice) -> fmt::Result {
    match n {
        Notice::Opened { track } => writeln!(w, "EVENT open track={track}"),
        Notice::Device { format } => writeln!(
            w,
            "EVENT device rate={} ch={} c={} v={}",
            format.sample_rate(),
            format.channels(),
            format.container_bits(),
            format.valid_bits()
        ),
        Notice::Plan { plan } => write_plan(w, plan),
        Notice::Unsupported { error } => writeln!(w, "EVENT unsupported reason={error:?}"),
        Notice::Skipped { track, fault } => writeln!(w, "EVENT skip track={track} fault={fault:?}"),
        Notice::Repaired { track, repair } => writeln!(w, "EVENT repaired track={track} {repair}"),
        Notice::ChannelsDuplicated { track } => writeln!(
            w,
            "EVENT channels-altered track={track} mono[FC]->stereo[FL FR] samples-unchanged"
        ),
        Notice::SeekSequential { track } => {
            writeln!(w, "EVENT seek-sequential track={track} decoded-to-target")
        }
        Notice::FloatConverted { track, bits } => writeln!(
            w,
            "EVENT samples-altered track={track} float->int{bits} explicit"
        ),
        Notice::FloatStats { track, report } => writeln!(
            w,
            "EVENT float-stats track={track} encoding={:?} converted={} samples={} clipped={} \
             inexact={} nonfinite={}",
            report.encoding,
            report.converted_to.unwrap_or(0),
            report.convert.samples,
            report.convert.clipped,
            report.convert.inexact,
            report.nonfinite
        ),
        Notice::Finished => writeln!(w, "EVENT finished"),
        Notice::Failed { track, fault } => {
            writeln!(w, "EVENT failed track={track} fault={fault:?}")
        }
        Notice::OutputError => writeln!(w, "EVENT output-error"),
    }
}

/// 播放计划行：`EVENT plan key=value ...`（R1）。
pub fn write_plan(w: &mut impl fmt::Write, p: &PlaybackPlan) -> fmt::Result {
    let s = &p.source;
    write!(
        w,
        "EVENT plan rate={} ch={} bits={} ",
        s.sample_rate(),
        s.channels(),
        s.bits()
    )?;
    if s.is_lossy() {
        // E1：有损编码，bits 是解码输出精度。
        w.write_str("lossy=1 ")?;
    }
    if let Some(d) = &p.device {
        write!(w, "device={}-{} ", d.container_bits(), d.valid_bits())?;
    }
    write!(
        w,
        "prefill_frames={} prefill_ms={} queue={}/{}",
        p.prefill_frames, p.prefill_ms, p.queue_required, p.queue_available
    )?;
    if let Some(d) = &p.decoder {
        write!(
            w,
            " decode_unit={}/{} input_unit={}/{}",
            d.max_unit_frames, d.unit_frames_capacity, d.max_unit_bytes, d.window
        )?;
    }
    writeln!(w)
}

/// 命令解析错误的回应行。
pub fn write_parse_error(w: &mut impl fmt::Write, e: ParseError) -> fmt::Result {
    let what = match e {
        ParseError::Empty => "empty",
        ParseError::Unknown => "unknown-command",
        ParseError::BadArgument => "bad-argument",
        ParseError::TooLong => "too-long",
    };
    writeln!(w, "ERR {what}")
}
