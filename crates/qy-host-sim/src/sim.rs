//! 真实路径播放模拟（M0 T4）。
//!
//! 数据路径全部真实：`qy-core` 状态机、`qy-codec-wav` 解码、pack、`rtrb` SPSC 队列。
//! 只模拟三样东西：
//! - 文件：内存中的字节，读取长度、延迟、零长度读、读错误与打开失败由场景决定；
//! - 时间：离散时钟，1 tick = 1 ms，生产端可被注入停顿；
//! - 设备：每 10 ms 一个周期。[`DeviceModel::Wasapi`] 每周期整包消费
//!   `rate / 100` 帧，不足部分补静音：已确认该格式不再有数据（EOF、换格式、失败）时
//!   计为尾部填充，否则计为欠载；[`DeviceModel::ShortWrite`] 每周期消费随机帧数
//!   （允许部分消费、不补零），数据不足时只记录欠载，不假设补零。
//!
//! 宿主规则：
//! - 设备启动前预填当前采样率下至少 500 ms，或在播放列表结束/失败时预填全部有效帧；
//! - 输出格式变化时先排空旧格式（设备播完最后一包有效音频）再重配置；
//! - 播放失败时排空已入队的有效音频后停止；设备断开时立即停止核心；
//! - 停止/重启后旧代次的读取完成不写入输入窗口，交给核心拒绝并计数。

use qy_core::{Action, DeviceFormat, Event, Fault, Generation};
use qy_pcm_format::ring::prefill_frames;

use crate::SimHost;

/// 一个模拟时钟单位（毫秒）。
pub const TICK_MS: u64 = 1;
/// 设备周期（tick）。
pub const DEVICE_PERIOD_TICKS: u64 = 10;
/// 单 tick 内生产端最多执行的 `poll` 次数（防止实现错误导致死循环）。
const MAX_POLLS_PER_TICK: usize = 1_000_000;

/// 可复现的伪随机数（SplitMix64）。
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
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
    pub fault: Option<(u32, Fault)>,
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
    /// 本次播放记录能否描述为完整保真：无欠载、无失败、无断开、未超时。
    pub fn fidelity_intact(&self) -> bool {
        self.underruns.is_empty()
            && self.fault.is_none()
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
    let min_v = supported
        .iter()
        .filter(|(_, v)| *v >= src.bits())
        .map(|(_, v)| *v)
        .min()?;
    let (c, v) = supported.iter().find(|(_, v)| *v == min_v)?;
    DeviceFormat::wasapi(src.sample_rate(), src.channels(), *c, *v).ok()
}

#[derive(Debug, Clone, Copy)]
enum After {
    Reconfigure {
        format: DeviceFormat,
        generation: Generation,
    },
    Finish,
    Fail,
}

#[derive(Debug, Clone, Copy)]
enum Phase {
    /// 设备未启动，正在预填。
    Prefill,
    Playing,
    /// 该格式不再有新数据，等待设备播完。
    Draining(After),
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
    frames_processed: u64,
}

struct Sim<'a> {
    host: &'a mut SimHost,
    sc: &'a Scenario,
    rng: Rng,
    report: Report,
    phase: Phase,
    device: Option<Device>,
    epoch: u32,
    pending: Vec<PendingRead>,
    open_track: Option<usize>,
    cursor: usize,
    packet: Vec<u8>,
}

/// 运行一个场景。`host` 的缓冲在多个场景间复用。
pub fn run(host: &mut SimHost, sc: &Scenario) -> Report {
    // 清理上一场景可能残留的状态：停止核心（旧代次）。
    let _ = host.core.stop();
    let count = u32::try_from(sc.tracks.len()).unwrap_or(u32::MAX);
    host.core.play(0, count);
    let mut sim = Sim {
        host,
        sc,
        rng: Rng::new(sc.seed ^ 0xD1CE_5EED),
        report: Report::default(),
        phase: Phase::Prefill,
        device: None,
        epoch: 0,
        pending: Vec::new(),
        open_track: None,
        cursor: 0,
        packet: vec![0u8; 2 * 192_000 / 100 * 8 * 2],
    };
    sim.main_loop();
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
            if !matches!(self.phase, Phase::Done) && !self.stalled(tick) {
                self.produce(tick);
            }
            if tick % DEVICE_PERIOD_TICKS == 0 {
                self.device_period(tick);
            }
            if matches!(self.phase, Phase::Done) && self.pending.is_empty() {
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

    fn inject_events(&mut self, tick: u64) {
        if self.sc.disconnect_at == Some(tick) && !matches!(self.phase, Phase::Done) {
            self.report.device_lost = true;
            if let Some(Action::Close { .. }) = self.host.core.stop() {
                self.open_track = None;
            }
            if let Some(d) = self.device.as_mut() {
                d.running = false;
            }
            self.phase = Phase::Done;
        }
        let current = self.host.core.generation();
        let in_flight = self
            .pending
            .iter()
            .filter(|p| p.generation == current)
            .count() as u32;
        if let Some((at, from)) = self.sc.restart
            && self.report.restarted_at.is_none()
            && tick >= at
            && (in_flight > 0 || tick >= at + 1000)
            && !matches!(self.phase, Phase::Done)
        {
            self.report.restarted_at = Some(tick);
            self.report.reads_in_flight_at_restart = in_flight;
            if let Some(Action::Close { .. }) = self.host.core.stop() {
                self.open_track = None;
            }
            // 停止设备并丢弃队列中旧代次的已 pack 数据。
            if let Some(d) = self.device.take()
                && self.host.configure_device(&d.format).is_err()
            {
                self.report.output_errors += 1;
            }
            self.epoch += 1;
            self.phase = Phase::Prefill;
            let count = u32::try_from(self.sc.tracks.len()).unwrap_or(u32::MAX);
            self.host.core.play(from, count);
        }
    }

    fn handle(&mut self, event: Event) {
        match self.host.core.handle(event) {
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
                if let Some(dst) = self.host.core.read_target(p.generation, p.request) {
                    let src = &self.sc.tracks[p.track].data[offset..offset + len];
                    dst[..len].copy_from_slice(src);
                }
                Event::ReadDone {
                    generation: p.generation,
                    request: p.request,
                    len,
                }
            }
            ReadOutcome::Eof => Event::ReadEof {
                generation: p.generation,
                request: p.request,
            },
            ReadOutcome::Error => Event::ReadFailed {
                generation: p.generation,
                request: p.request,
            },
        };
        self.handle(event);
    }

    fn produce(&mut self, tick: u64) {
        for _ in 0..MAX_POLLS_PER_TICK {
            let Some(action) = self.host.core.poll() else {
                break;
            };
            if !self.execute(action, tick) {
                break;
            }
        }
        self.maybe_start_device(tick);
    }

    /// 执行动作；返回 false 表示本 tick 生产端应让出（队列满或等待排空）。
    fn execute(&mut self, action: Action, tick: u64) -> bool {
        match action {
            Action::Open { generation, track } => {
                let t = track as usize;
                if self.sc.tracks.get(t).is_none_or(|t| t.open_fails) {
                    self.handle(Event::OpenFailed { generation });
                } else {
                    self.open_track = Some(t);
                    self.cursor = 0;
                    self.handle(Event::Opened { generation });
                }
                true
            }
            Action::Read {
                generation,
                request,
                max,
            } => {
                self.issue_read(generation, request, max, tick);
                true
            }
            Action::Configure { generation, format } => {
                let Some(dev) = choose_device_format(&format, &self.sc.supported) else {
                    self.handle(Event::ConfigureFailed { generation });
                    return true;
                };
                match &self.device {
                    Some(d) if d.format == dev => {
                        self.handle(Event::Configured { generation });
                        true
                    }
                    Some(_) => {
                        self.phase = Phase::Draining(After::Reconfigure {
                            format: dev,
                            generation,
                        });
                        false
                    }
                    None => {
                        self.setup_device(dev);
                        self.handle(Event::Configured { generation });
                        true
                    }
                }
            }
            Action::Output { generation, .. } => {
                let pushed = match (self.host.core.pcm(), self.device.as_ref()) {
                    (Some(pcm), Some(dev)) => {
                        match self.host.producer.pack_push(
                            &pcm,
                            &dev.format,
                            self.host.pack_scratch,
                        ) {
                            Ok(n) => n,
                            Err(_) => {
                                self.report.output_errors += 1;
                                0
                            }
                        }
                    }
                    _ => {
                        self.report.output_errors += 1;
                        0
                    }
                };
                self.handle(Event::Output {
                    generation,
                    frames: pushed,
                });
                pushed > 0
            }
            Action::Close { .. } => {
                self.open_track = None;
                true
            }
            Action::Finished { .. } => {
                self.phase = Phase::Draining(After::Finish);
                false
            }
            Action::Failed { track, fault, .. } => {
                self.report.fault = Some((track, fault));
                self.phase = Phase::Draining(After::Fail);
                false
            }
        }
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

    fn setup_device(&mut self, format: DeviceFormat) {
        if self.host.configure_device(&format).is_err() {
            self.report.output_errors += 1;
        }
        self.device = Some(Device {
            format,
            running: false,
            frames_processed: 0,
        });
        self.phase = Phase::Prefill;
        self.report.segments.push(Segment {
            epoch: self.epoch,
            format,
            valid: Vec::new(),
            tail_padding_frames: 0,
        });
    }

    fn maybe_start_device(&mut self, tick: u64) {
        let Some(dev) = self.device.as_mut() else {
            // 从未配置输出就结束或失败：无音频需排空。
            if let Phase::Draining(after) = self.phase {
                self.finish_drain(after);
            }
            return;
        };
        if dev.running {
            return;
        }
        let ready = match self.phase {
            Phase::Prefill => {
                self.host.consumer.available_frames() as u64 >= prefill_frames(&dev.format)
            }
            Phase::Draining(_) => true,
            Phase::Playing | Phase::Done => false,
        };
        if ready {
            dev.running = true;
            if matches!(self.phase, Phase::Prefill) {
                self.phase = Phase::Playing;
            }
            // 启动前准备初始端点缓冲：立即处理第一个周期。
            self.device_period(tick);
        }
    }

    fn device_period(&mut self, tick: u64) {
        let Some(dev) = self.device.as_ref() else {
            return;
        };
        if !dev.running {
            return;
        }
        let format = dev.format;
        let fb = usize::from(format.block_align());
        let packet_frames = (format.sample_rate() / 100) as usize;
        let draining = match self.phase {
            Phase::Draining(after) => Some(after),
            _ => None,
        };
        let available = self.host.consumer.available_frames();
        if let Some(after) = draining
            && available == 0
        {
            // 上一包有效音频已在上个周期播放完成。
            self.finish_drain(after);
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
                (DeviceModel::Wasapi, Some(_)) => seg.tail_padding_frames += missing,
                (_, None) => self.report.underruns.push(Underrun {
                    tick,
                    device_frame: processed,
                    missing_frames: missing,
                }),
                // 短写后端排空时请求未满足不是欠载，也不补零。
                (DeviceModel::ShortWrite, Some(_)) => {}
            }
        }
        if let Some(d) = self.device.as_mut() {
            d.frames_processed += match self.sc.device {
                DeviceModel::Wasapi => want as u64,
                DeviceModel::ShortWrite => got as u64,
            };
        }
    }

    fn finish_drain(&mut self, after: After) {
        if let Some(d) = self.device.as_mut() {
            d.running = false;
        }
        match after {
            After::Reconfigure { format, generation } => {
                self.setup_device(format);
                self.handle(Event::Configured { generation });
            }
            After::Finish => {
                self.report.finished = true;
                self.phase = Phase::Done;
            }
            After::Fail => self.phase = Phase::Done,
        }
    }
}
