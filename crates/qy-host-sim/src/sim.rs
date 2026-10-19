//! 真实路径播放模拟（M0 T4；M3 T1 起播放规则由 `qy_core::engine` 负责）。
//!
//! 数据路径全部真实：`qy-core` 引擎与状态机、自有解码（`qy-codec-auto` 按文件头选 WAV/FLAC）、
//! pack、`rtrb` SPSC 队列。只模拟三样东西：
//! - 文件：内存中的字节，读取长度、延迟、零长度读、读错误与打开失败由场景决定；
//! - 时间：离散时钟，1 tick = 1 ms，生产端可被注入停顿；
//! - 设备：每 10 ms 一个周期。[`DeviceModel::Wasapi`] 每周期整包消费
//!   `rate / 100` 帧，不足部分补静音：已确认该格式不再有数据（排空中）时
//!   计为尾部填充，否则计为欠载；暂停时整包输出静音并单独计数；[`DeviceModel::ShortWrite`]
//!   每周期消费随机帧数（允许部分消费、不补零），数据不足时只记录欠载，不假设补零。
//!
//! 预填、换格式排空、结束/失败排空、背压、暂停与切歌都由引擎决定；本模块只执行引擎的
//! [`HostAction`] 并回报 [`HostEvent`]。设备复用策略为 [`ReusePolicy::Identical`]：每首曲目都
//! 选择 V ≥ N 中最小的设备格式（专门覆盖换格式路径）。停止/重启/切歌后旧代次的读取完成不
//! 写入输入窗口，交给核心拒绝并计数。

use qy_core::engine::{EngineFault, ErrorPolicy, HostAction, HostEvent, Poll};
use qy_core::{ChannelLayout, DeviceFormat, Generation, SampleEncoding};

use crate::SimHost;

/// 一个模拟时钟单位（毫秒）。
pub const TICK_MS: u64 = 1;
/// 设备周期（tick）。
pub const DEVICE_PERIOD_TICKS: u64 = 10;
/// 单 tick 内生产端最多执行的引擎动作数（防止实现错误导致死循环）。
const MAX_POLLS_PER_TICK: usize = 1_000_000;

/// 可复现的伪随机数（SplitMix64）。
///
/// [`Rng::from_tape`] 先按顺序取用给定字节（每次 8 字节，小端；不足时补 0），用完后再按 SplitMix64
/// 继续。场景 fuzz 用它让 fuzzer 的输入直接决定场景中的每个选择（M2 T5）。
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
    tape: Vec<u8>,
    pos: usize,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed,
            tape: Vec::new(),
            pos: 0,
        }
    }

    /// 以 `tape` 为前缀的随机源，用完后按 `seed` 的 SplitMix64 继续。
    pub fn from_tape(seed: u64, tape: &[u8]) -> Self {
        Self {
            state: seed,
            tape: tape.to_vec(),
            pos: 0,
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        if self.pos < self.tape.len() {
            let end = self.tape.len().min(self.pos + 8);
            let mut b = [0u8; 8];
            b[..end - self.pos].copy_from_slice(&self.tape[self.pos..end]);
            self.pos = end;
            return u64::from_le_bytes(b);
        }
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// `[lo, hi]` 闭区间均匀整数。
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            return lo;
        }
        lo + self.next_u64() % (hi - lo + 1)
    }

    /// 以千分之 `per_mille` 的概率返回 true。
    pub fn chance(&mut self, per_mille: u64) -> bool {
        self.range(0, 999) < per_mille
    }
}

/// 一首模拟曲目。
#[derive(Debug, Clone)]
pub struct TrackSpec {
    pub data: Vec<u8>,
    pub open_fails: bool,
    /// 读取到该字节偏移（含跨越）时报告读取错误。
    pub read_error_at: Option<usize>,
    /// 音源不可定位（如管道）：`Seek` 回报失败（S1）。
    pub unseekable: bool,
}

/// 读取行为。
#[derive(Debug, Clone, Copy)]
pub struct ReadModel {
    /// 返回 0 字节（非 EOF）的千分比。
    pub zero_per_mille: u64,
    /// 返回 1..=16 字节短读的千分比。
    pub tiny_per_mille: u64,
    /// 读取延迟 ≥ 1 tick 的千分比。
    pub slow_per_mille: u64,
    /// 慢读最大延迟（tick）。
    pub slow_max_ticks: u64,
}

/// 设备消费模型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceModel {
    /// WASAPI 独占事件驱动：每周期整包请求并释放 `rate/100` 帧。
    Wasapi,
    /// 允许短写的后端：每周期消费 `0..=2×rate/100` 帧的随机部分，不补零。
    ShortWrite,
}

/// 模拟场景。
#[derive(Debug, Clone)]
pub struct Scenario {
    pub seed: u64,
    pub tracks: Vec<TrackSpec>,
    pub reads: ReadModel,
    pub device: DeviceModel,
    /// 设备支持的 (容器位宽, 有效位) 组合，按设备偏好排序；均为 WASAPI 布局。
    pub supported: Vec<(u8, u8)>,
    /// 生产端停顿窗口 `(起始 tick, 时长 tick)`。
    pub stalls: Vec<(u64, u64)>,
    /// 设备在该 tick 断开。
    pub disconnect_at: Option<u64>,
    /// 自该 tick 起、在当前代次有在途读取时停止并从指定曲目重新播放
    /// （最多顺延 1000 tick，之后无条件重启），用于验证旧读取被拒绝。
    pub restart: Option<(u64, u32)>,
    /// 超时上限（tick）。
    pub max_ticks: u64,
    /// 曲目出错时的策略（默认停止）。
    pub on_error: ErrorPolicy,
    /// 在指定 tick 执行的播放控制。
    pub controls: Vec<(u64, Control)>,
    /// 设备支持的声道布局（设备上的样本顺序，F1）。`None`：设备按源布局输出（WASAPI 掩码顺序）；
    /// `Some`：只支持列出的布局，位置集合与源相同而顺序不同时由 pack 纯置换，否则设备不支持。
    pub device_layouts: Option<Vec<ChannelLayout>>,
    /// 设备接受的浮点编码（F3，小端，WASAPI 布局）。浮点源只送到同一浮点编码的设备格式；不在
    /// 此列时按引擎的浮点策略拒绝或改请求整数。
    pub float_formats: Vec<SampleEncoding>,
}

/// 播放控制。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Pause,
    Resume,
    Next,
    Prev,
    Goto(u32),
    Stop,
    /// 在正在播放的曲目内定位到第 n 帧（S1）。
    Seek(u64),
}

/// 一次设备配置期间输出的数据。`epoch` 在每次停止重启后递增。
#[derive(Debug, Clone)]
pub struct Segment {
    pub epoch: u32,
    pub format: DeviceFormat,
    /// 设备实际播放的有效帧字节（不含任何填充）。
    pub valid: Vec<u8>,
    /// 已确认无后续数据时整包补齐的尾部静音帧数。
    pub tail_padding_frames: u64,
    /// 队列水位（R1；运行中、未暂停、未排空时每个设备周期取数前观测；不计入摘要）。
    pub watermark: qy_pcm_format::queue::Watermark,
}

/// 欠载事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Underrun {
    pub tick: u64,
    /// 本段内此前设备已处理的帧位置。
    pub device_frame: u64,
    /// 缺失帧数（WASAPI 为补入的静音帧，短写后端为未满足的请求帧）。
    pub missing_frames: u64,
}

/// 模拟报告。
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub segments: Vec<Segment>,
    pub underruns: Vec<Underrun>,
    pub fault: Option<(u32, EngineFault)>,
    /// 按策略跳过的曲目。
    pub skipped: Vec<(u32, EngineFault)>,
    /// 曲目的结构修正（W1，不计入摘要）。
    pub repairs: Vec<(u32, qy_core::engine::Repair)>,
    /// 按明确开启的选项把单声道复制到左右两路的曲目（O1：声道布局已改变）。
    pub duplicated: Vec<u32>,
    /// 定位改为顺序解码的曲目（S1：解码器不支持定位或音源不可定位）。
    pub sequential_seeks: Vec<u32>,
    /// 定位预滚（E5）：曲目与丢弃的帧数。
    pub seek_prerolls: Vec<(u32, u64)>,
    /// 浮点按明确开启的选项转为整数的曲目与位数（F3：样本已改变）。
    pub float_converted: Vec<(u32, u8)>,
    /// 浮点曲目的统计（F3）。
    pub float_stats: Vec<(u32, qy_core::engine::FloatReport)>,
    /// 执行过的 `Seek`：(曲目, 字节位置)。
    pub seeks: Vec<(usize, u64)>,
    /// 暂停期间设备输出的静音帧（不计欠载）。
    pub pause_silence_frames: u64,
    /// 已执行的播放控制（tick、控制）。
    pub controls_done: Vec<(u64, Control)>,
    pub finished: bool,
    pub device_lost: bool,
    pub timed_out: bool,
    /// 被核心拒绝的旧代次/旧请求回报数。
    pub stale_rejected: u32,
    /// 重启发生的 tick。
    pub restarted_at: Option<u64>,
    /// 重启时仍在途的读取数。
    pub reads_in_flight_at_restart: u32,
    /// 宿主回报被核心判为协议错误的次数（应为 0）。
    pub protocol_errors: u32,
    /// pack/队列错误（应为 0）。
    pub output_errors: u32,
    pub ticks: u64,
}

impl Report {
    /// 报告摘要：设备字节（哈希）、欠载、故障、结束状态与计数。用于在重构前后逐种子比对
    /// 结果（`QY_DIGEST=1` 时种子测试逐行打印）。故障只取核心故障本身，与引擎层包装无关。
    pub fn digest(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for s in &self.segments {
            s.epoch.hash(&mut h);
            let f = &s.format;
            // 字段与 M3 T1 时 `DeviceFormat` 的 Debug 输出逐字一致，使单/双声道摘要可与 F1 之前比对；
            // 非默认布局（多声道等）另行计入。
            format!(
                "DeviceFormat {{ sample_rate: {}, channels: {}, container_bits: {}, valid_bits: {}, \
                 endian: {:?}, placement: {:?}, block_align: {}, bytes_per_sec: {} }}",
                f.sample_rate(),
                f.channels(),
                f.container_bits(),
                f.valid_bits(),
                f.endian(),
                f.placement(),
                f.block_align(),
                f.bytes_per_sec()
            )
            .hash(&mut h);
            if qy_core::ChannelLayout::default_for(f.channels()) != Some(f.layout()) {
                format!("{:?}", f.layout()).hash(&mut h);
            }
            s.valid.hash(&mut h);
            s.tail_padding_frames.hash(&mut h);
        }
        let fault = self.fault.map(|(t, f)| match f {
            EngineFault::Core(c) => (t, format!("{c:?}")),
            other => (t, format!("{other:?}")),
        });
        format!(
            "{:x} {:?} {:?} {} {} {} {:?} {} {} {} {}",
            h.finish(),
            self.underruns,
            fault,
            self.finished,
            self.device_lost,
            self.stale_rejected,
            self.restarted_at,
            self.reads_in_flight_at_restart,
            self.ticks,
            self.output_errors,
            self.protocol_errors
        )
    }

    /// 本次播放记录能否描述为完整保真：无欠载、无失败、无跳过、无断开、未超时，且没有按明确开启的
    /// 选项改变声道布局（O1）。
    pub fn fidelity_intact(&self) -> bool {
        self.duplicated.is_empty()
            && self.underruns.is_empty()
            && self.fault.is_none()
            && self.skipped.is_empty()
            && !self.device_lost
            && !self.timed_out
            && self.protocol_errors == 0
            && self.output_errors == 0
    }
}

/// 设备格式选择：在支持列表中取 V ≥ N 的最小 V，同 V 按设备偏好顺序。
/// 只选择不改变样本的格式（N ≤ V），采样率与声道照搬源格式。
pub fn choose_device_format(
    src: &qy_core::SourceFormat,
    supported: &[(u8, u8)],
) -> Option<DeviceFormat> {
    choose_device_format_with(src, supported, None)
}

/// 同 [`choose_device_format`]，设备只支持 `layouts` 中的声道布局时取位置集合与源相同的一个。
pub fn choose_device_format_with(
    src: &qy_core::SourceFormat,
    supported: &[(u8, u8)],
    layouts: Option<&[ChannelLayout]>,
) -> Option<DeviceFormat> {
    let Some(list) = layouts else {
        return choose_wave_order(src, supported);
    };
    let layout = list
        .iter()
        .copied()
        .find(|l| l.same_positions(&src.layout()))?;
    let base = choose_wave_order(src, supported)?;
    DeviceFormat::with_layout(
        base.sample_rate(),
        layout,
        base.container_bits(),
        base.valid_bits(),
        base.endian(),
        base.placement(),
    )
    .ok()
}

fn choose_wave_order(src: &qy_core::SourceFormat, supported: &[(u8, u8)]) -> Option<DeviceFormat> {
    if src.encoding().is_float() {
        return None;
    }
    let min_v = supported
        .iter()
        .filter(|(_, v)| *v >= src.bits())
        .map(|(_, v)| *v)
        .min()?;
    let (c, v) = supported.iter().find(|(_, v)| *v == min_v)?;
    // 设备布局与源布局相同（WASAPI 语义：掩码顺序），F1。
    DeviceFormat::wasapi_layout(src.sample_rate(), src.layout(), *c, *v).ok()
}

/// 浮点设备格式（F3）：设备接受源的浮点编码时按源布局（或 `layouts` 中位置集合相同的一个）输出。
pub fn choose_float(
    src: &qy_core::SourceFormat,
    floats: &[SampleEncoding],
    layouts: Option<&[ChannelLayout]>,
) -> Option<DeviceFormat> {
    if !floats.contains(&src.encoding()) {
        return None;
    }
    let layout = match layouts {
        None => src.layout(),
        Some(list) => list
            .iter()
            .copied()
            .find(|l| l.same_positions(&src.layout()))?,
    };
    DeviceFormat::float_layout(
        src.sample_rate(),
        layout,
        src.encoding(),
        qy_core::Endian::Little,
    )
    .ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// 设备未启动，正在预填。
    Prefill,
    Playing,
    /// 该格式不再有新数据，等待设备播完。
    Draining,
    Done,
}

#[derive(Debug, Clone, Copy)]
enum ReadOutcome {
    Data { offset: usize, len: usize },
    Eof,
    Error,
}

#[derive(Debug, Clone, Copy)]
struct PendingRead {
    ready_tick: u64,
    generation: Generation,
    request: u32,
    track: usize,
    outcome: ReadOutcome,
}

struct Device {
    format: DeviceFormat,
    running: bool,
    paused: bool,
    frames_processed: u64,
}

struct Sim<'a> {
    host: &'a mut SimHost,
    sc: &'a Scenario,
    rng: Rng,
    report: Report,
    phase: Phase,
    device: Option<Device>,
    start_requested: bool,
    epoch: u32,
    pending: Vec<PendingRead>,
    open_track: Option<usize>,
    cursor: usize,
    packet: Vec<u8>,
    next_control: usize,
}

/// 运行一个场景。`host` 的缓冲在多个场景间复用。
pub fn run(host: &mut SimHost, sc: &Scenario) -> Report {
    // 清理上一场景可能残留的状态：停止引擎（旧代次）并丢弃其控制动作。
    host.engine.stop();
    while host.engine.take_control().is_some() {}
    host.engine.set_error_policy(sc.on_error);
    let count = u32::try_from(sc.tracks.len()).unwrap_or(u32::MAX);
    host.engine.play(0, count);
    let mut sim = Sim {
        host,
        sc,
        rng: Rng::new(sc.seed ^ 0xD1CE_5EED),
        report: Report::default(),
        phase: Phase::Prefill,
        device: None,
        start_requested: false,
        epoch: 0,
        pending: Vec::new(),
        open_track: None,
        cursor: 0,
        packet: Vec::new(),
        next_control: 0,
    };
    sim.main_loop();
    let stats = sim.host.engine.stats();
    sim.report.protocol_errors += stats.protocol_errors;
    sim.report
}

impl Sim<'_> {
    fn main_loop(&mut self) {
        let mut tick = 0u64;
        loop {
            if tick >= self.sc.max_ticks {
                self.report.timed_out = true;
                break;
            }
            self.inject_events(tick);
            self.deliver_reads(tick);
            if self.phase != Phase::Done && !self.stalled(tick) {
                self.produce(tick);
            }
            if tick.is_multiple_of(DEVICE_PERIOD_TICKS) {
                self.device_period(tick);
            }
            if self.phase == Phase::Done && self.pending.is_empty() {
                break;
            }
            tick += TICK_MS;
        }
        self.report.ticks = tick;
    }

    fn stalled(&self, tick: u64) -> bool {
        self.sc
            .stalls
            .iter()
            .any(|&(start, len)| tick >= start && tick < start.saturating_add(len))
    }

    /// 立即执行引擎排队的控制动作（关闭音源、停止设备等），不推进核心。
    fn run_controls(&mut self, tick: u64) {
        while let Some(a) = self.host.engine.take_control() {
            self.execute(a, tick);
        }
    }

    fn inject_events(&mut self, tick: u64) {
        if self.sc.disconnect_at == Some(tick) && self.phase != Phase::Done {
            self.report.device_lost = true;
            self.host.engine.device_lost();
            self.run_controls(tick);
            if let Some(d) = self.device.as_mut() {
                d.running = false;
            }
            self.phase = Phase::Done;
        }
        let current = self.host.engine.generation();
        let in_flight = self
            .pending
            .iter()
            .filter(|p| p.generation == current)
            .count() as u32;
        if let Some((at, from)) = self.sc.restart
            && self.report.restarted_at.is_none()
            && tick >= at
            && (in_flight > 0 || tick >= at + 1000)
            && self.phase != Phase::Done
        {
            self.report.restarted_at = Some(tick);
            self.report.reads_in_flight_at_restart = in_flight;
            // 停止：关闭音源、停止设备并丢弃队列中旧代次的已 pack 数据。
            self.host.engine.stop();
            self.run_controls(tick);
            self.epoch += 1;
            self.phase = Phase::Prefill;
            let count = u32::try_from(self.sc.tracks.len()).unwrap_or(u32::MAX);
            self.host.engine.play(from, count);
        }
        while let Some(&(at, c)) = self.sc.controls.get(self.next_control) {
            if at > tick || self.phase == Phase::Done {
                break;
            }
            self.next_control += 1;
            self.report.controls_done.push((tick, c));
            let e = &mut self.host.engine;
            match c {
                Control::Pause => e.pause(),
                Control::Resume => e.resume(),
                Control::Next => e.next(),
                Control::Prev => e.prev(),
                Control::Goto(k) => e.skip_to(k),
                Control::Stop => e.stop(),
                Control::Seek(frame) => e.seek(frame),
            }
            self.run_controls(tick);
            match c {
                Control::Pause | Control::Resume => {}
                Control::Next | Control::Prev | Control::Goto(_) | Control::Seek(_) => {
                    self.epoch += 1;
                    self.phase = Phase::Prefill;
                }
                Control::Stop => self.phase = Phase::Done,
            }
        }
    }

    fn handle(&mut self, event: HostEvent) {
        match self.host.engine.handle(event) {
            Ok(()) => {}
            Err(qy_core::CoreError::Stale) => self.report.stale_rejected += 1,
            Err(_) => self.report.protocol_errors += 1,
        }
    }

    fn deliver_reads(&mut self, tick: u64) {
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].ready_tick <= tick {
                let p = self.pending.remove(i);
                self.complete_read(p);
            } else {
                i += 1;
            }
        }
    }

    fn complete_read(&mut self, p: PendingRead) {
        let event = match p.outcome {
            ReadOutcome::Data { offset, len } => {
                // 旧代次/旧请求没有读取目标：不得写入，回报交核心拒绝。
                if let Some(dst) = self.host.engine.read_target(p.generation, p.request) {
                    let src = &self.sc.tracks[p.track].data[offset..offset + len];
                    dst[..len].copy_from_slice(src);
                }
                HostEvent::ReadDone {
                    generation: p.generation,
                    request: p.request,
                    len,
                }
            }
            ReadOutcome::Eof => HostEvent::ReadEof {
                generation: p.generation,
                request: p.request,
            },
            ReadOutcome::Error => HostEvent::ReadFailed {
                generation: p.generation,
                request: p.request,
            },
        };
        self.handle(event);
    }

    fn produce(&mut self, tick: u64) {
        for _ in 0..MAX_POLLS_PER_TICK {
            match self.host.engine.poll() {
                Poll::Action(a) => {
                    if !self.execute(a, tick) {
                        break;
                    }
                }
                Poll::Yield => {}
                Poll::Wait(_) => break,
                // 引擎不再播放（例如在最后一首时“下一首”）：场景结束。
                Poll::Idle => {
                    self.phase = Phase::Done;
                    break;
                }
            }
        }
        self.maybe_start_device(tick);
    }

    /// 执行引擎动作；返回 false 表示本 tick 生产端应让出。
    fn execute(&mut self, action: HostAction, tick: u64) -> bool {
        match action {
            HostAction::Open { generation, track } => {
                let t = track as usize;
                if self.sc.tracks.get(t).is_none_or(|t| t.open_fails) {
                    self.handle(HostEvent::OpenFailed { generation });
                } else {
                    self.open_track = Some(t);
                    self.cursor = 0;
                    self.handle(HostEvent::Opened { generation });
                }
            }
            HostAction::Read {
                generation,
                request,
                max,
            } => self.issue_read(generation, request, max, tick),
            HostAction::Seek {
                generation,
                request,
                offset,
            } => {
                let seekable = self
                    .open_track
                    .and_then(|t| self.sc.tracks.get(t))
                    .is_some_and(|t| !t.unseekable);
                if let (true, Some(t), Ok(at)) =
                    (seekable, self.open_track, usize::try_from(offset))
                {
                    self.report.seeks.push((t, offset));
                    self.cursor = at;
                    self.handle(HostEvent::SeekDone {
                        generation,
                        request,
                    });
                } else {
                    self.handle(HostEvent::SeekFailed {
                        generation,
                        request,
                    });
                }
            }
            HostAction::Close { .. } => self.open_track = None,
            HostAction::SelectDevice { source } => {
                let chosen = if source.encoding().is_float() {
                    choose_float(
                        &source,
                        &self.sc.float_formats,
                        self.sc.device_layouts.as_deref(),
                    )
                } else {
                    choose_device_format_with(
                        &source,
                        &self.sc.supported,
                        self.sc.device_layouts.as_deref(),
                    )
                };
                match chosen {
                    Some(format) => self.handle(HostEvent::DeviceSelected { format }),
                    None => self.handle(HostEvent::DeviceUnsupported),
                }
            }
            HostAction::OpenDevice { format, .. } => match format {
                Some(f) if self.setup_device(f) => {
                    self.handle(HostEvent::DeviceOpened { format: f })
                }
                Some(_) => self.handle(HostEvent::DeviceOpenFailed),
                None => {
                    self.report.protocol_errors += 1;
                    self.handle(HostEvent::DeviceOpenFailed);
                }
            },
            HostAction::StartDevice => self.start_requested = true,
            HostAction::DrainDevice => {
                self.phase = Phase::Draining;
                if self.device.as_ref().is_some_and(|d| !d.running) {
                    self.start_requested = true;
                }
            }
            HostAction::StopDevice => {
                // 停止设备并丢弃队列：进入新的队列代次（输出段 epoch 由停止的发起方递增）。
                if let Some(d) = self.device.take()
                    && self.host.configure_device(&d.format).is_err()
                {
                    self.report.output_errors += 1;
                }
                self.start_requested = false;
                self.phase = Phase::Prefill;
            }
            HostAction::PauseDevice => {
                if let Some(d) = self.device.as_mut() {
                    d.paused = true;
                }
            }
            HostAction::ResumeDevice => {
                if let Some(d) = self.device.as_mut() {
                    d.paused = false;
                }
            }
            HostAction::TrackSkipped { track, fault } => self.report.skipped.push((track, fault)),
            HostAction::Repaired { track, repair } => self.report.repairs.push((track, repair)),
            HostAction::ChannelsDuplicated { track } => self.report.duplicated.push(track),
            HostAction::FloatConverted { track, bits } => {
                self.report.float_converted.push((track, bits));
            }
            HostAction::FloatStats { track, report } => {
                self.report.float_stats.push((track, report));
            }
            HostAction::SeekSequential { track } => self.report.sequential_seeks.push(track),
            HostAction::SeekPreroll { track, frames } => {
                self.report.seek_prerolls.push((track, frames));
            }
            HostAction::Finished => {
                self.report.finished = true;
                self.phase = Phase::Done;
                return false;
            }
            HostAction::Failed { track, fault } => {
                self.report.fault = Some((track, fault));
                self.phase = Phase::Done;
                return false;
            }
        }
        true
    }

    fn issue_read(&mut self, generation: Generation, request: u32, max: usize, tick: u64) {
        let r = self.sc.reads;
        let want = if self.rng.chance(r.zero_per_mille) {
            0
        } else if self.rng.chance(r.tiny_per_mille) {
            self.rng.range(1, 16) as usize
        } else {
            self.rng.range(1, max as u64) as usize
        }
        .min(max);
        let latency = if self.rng.chance(r.slow_per_mille) {
            self.rng.range(1, r.slow_max_ticks.max(1))
        } else {
            0
        };
        let Some(track) = self.open_track else {
            self.report.protocol_errors += 1;
            return;
        };
        let spec = &self.sc.tracks[track];
        let remaining = spec.data.len().saturating_sub(self.cursor);
        let outcome = if spec
            .read_error_at
            .is_some_and(|e| self.cursor + want.max(1) > e)
        {
            ReadOutcome::Error
        } else if remaining == 0 && want > 0 {
            ReadOutcome::Eof
        } else {
            let len = want.min(remaining);
            let o = ReadOutcome::Data {
                offset: self.cursor,
                len,
            };
            self.cursor += len;
            o
        };
        let p = PendingRead {
            ready_tick: tick + latency,
            generation,
            request,
            track,
            outcome,
        };
        if latency == 0 {
            self.complete_read(p);
        } else {
            self.pending.push(p);
        }
    }

    /// 按设备格式重配置队列；预算不足时返回 false（明确的打开失败）。
    fn setup_device(&mut self, format: DeviceFormat) -> bool {
        if self.host.configure_device(&format).is_err() {
            return false;
        }
        self.device = Some(Device {
            format,
            running: false,
            paused: false,
            frames_processed: 0,
        });
        self.start_requested = false;
        let packet = 2 * (format.sample_rate() / 100) as usize * usize::from(format.block_align());
        if self.packet.len() < packet {
            self.packet.resize(packet, 0);
        }
        self.phase = Phase::Prefill;
        let prefill = self
            .host
            .engine
            .config()
            .caps
            .prefill_frames(format.sample_rate());
        self.report.segments.push(Segment {
            epoch: self.epoch,
            format,
            valid: Vec::new(),
            tail_padding_frames: 0,
            watermark: qy_pcm_format::queue::Watermark::for_stream(
                prefill,
                u64::from(format.sample_rate() / 100),
            ),
        });
        true
    }

    fn maybe_start_device(&mut self, tick: u64) {
        let Some(dev) = self.device.as_mut() else {
            return;
        };
        if dev.running || !self.start_requested {
            return;
        }
        dev.running = true;
        self.start_requested = false;
        if self.phase == Phase::Prefill {
            self.phase = Phase::Playing;
        }
        // 启动前准备初始端点缓冲：立即处理第一个周期。
        self.device_period(tick);
    }

    fn device_period(&mut self, tick: u64) {
        let Some(dev) = self.device.as_ref() else {
            return;
        };
        if !dev.running {
            return;
        }
        let format = dev.format;
        let paused = dev.paused;
        let fb = usize::from(format.block_align());
        let packet_frames = (format.sample_rate() / 100) as usize;
        let draining = self.phase == Phase::Draining;
        if paused {
            // 暂停：整包静音（WASAPI）或不取数（短写），单独计数，不算欠载。
            if self.sc.device == DeviceModel::Wasapi {
                self.report.pause_silence_frames += packet_frames as u64;
                if let Some(d) = self.device.as_mut() {
                    d.frames_processed += packet_frames as u64;
                }
            }
            return;
        }
        let available = self.host.consumer.available_frames();
        if !draining && let Some(seg) = self.report.segments.last_mut() {
            seg.watermark.observe(available as u64);
        }
        if draining && available == 0 {
            // 上一包有效音频已在上个周期播放完成。
            if let Some(d) = self.device.as_mut() {
                d.running = false;
            }
            self.phase = Phase::Prefill;
            self.handle(HostEvent::DeviceDrained);
            // 排空后的动作（打开新格式、结束、失败）立即执行。
            self.run_controls(tick);
            return;
        }
        let want = match self.sc.device {
            DeviceModel::Wasapi => packet_frames,
            DeviceModel::ShortWrite => self.rng.range(0, 2 * packet_frames as u64) as usize,
        };
        let take = want.min(available);
        let got = match self.host.consumer.pop_frames(&mut self.packet[..take * fb]) {
            Ok(n) => n,
            Err(_) => {
                self.report.output_errors += 1;
                0
            }
        };
        let missing = (want - got) as u64;
        let Some(seg) = self.report.segments.last_mut() else {
            self.report.output_errors += 1;
            return;
        };
        seg.valid.extend_from_slice(&self.packet[..got * fb]);
        let processed = self.device.as_ref().map_or(0, |d| d.frames_processed);
        if missing > 0 {
            match (self.sc.device, draining) {
                (DeviceModel::Wasapi, true) => seg.tail_padding_frames += missing,
                (_, false) => self.report.underruns.push(Underrun {
                    tick,
                    device_frame: processed,
                    missing_frames: missing,
                }),
                // 短写后端排空时请求未满足不是欠载，也不补零。
                (DeviceModel::ShortWrite, true) => {}
            }
        }
        if let Some(d) = self.device.as_mut() {
            d.frames_processed += match self.sc.device {
                DeviceModel::Wasapi => want as u64,
                DeviceModel::ShortWrite => got as u64,
            };
        }
    }
}
