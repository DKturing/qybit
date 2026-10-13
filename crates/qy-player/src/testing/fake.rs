//! 假平台 API 与假后端。
//!
//! - [`FakeApi`]：整包 API，检查调用约束：每次 `get_buffer` 之前没有未释放的包，
//!   `release_buffer` 的帧数等于对应 `get_buffer`。
//! - [`FakeWriteApi`]：短写 API，可注入随机短写、xrun 与写入错误；设备只“播放”真实写入的
//!   字节，不补静音。
//! - [`FakeBackend`]：按支持列表协商，渲染线程运行真实渲染循环与“耐心”假设备
//!   （从不让设备空转，因此不产生欠载）。协商记录由 [`Negotiator`] 产生，平台宿主可提供
//!   自己的记录类型（如 WASAPI 的 HRESULT）。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use qy_core::{DeviceFormat, SourceFormat};
use qy_pcm_format::ring::FrameConsumer;

use crate::backend::{Backend, PendingStream, RunningStream};
use crate::render::packet::{PacketApi, PacketConfig};
use crate::render::write::{Avail, WriteApi, WriteConfig, Written};
use crate::render::{
    ApiError, ApiOp, ErrorCode, RenderControl, RenderReport, ThreadPriority, WaitResult, packet,
    write,
};
use crate::report::{AppliedParams, AttemptRecord, Negotiation, OpenFailure, Submission};

/// 假设备失效错误（取 WASAPI `AUDCLNT_E_DEVICE_INVALIDATED` 的值）。
pub const DEVICE_INVALIDATED: ErrorCode = ErrorCode::Hresult(0x8889_0004_u32 as i32);
/// 假短写设备消失错误（`ENODEV`）。
pub const DEVICE_GONE: ErrorCode = ErrorCode::Errno(19);
/// 整包释放时的“静音”标志位（WASAPI `AUDCLNT_BUFFERFLAGS_SILENT`）；渲染循环不得使用。
pub const SILENT_FLAG: u32 = 0x2;

/// 可复现的伪随机数（SplitMix64）。
#[derive(Debug, Clone)]
pub struct FakeRng(u64);

impl FakeRng {
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

    /// `[lo, hi]` 闭区间。
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            return lo;
        }
        lo + self.next_u64() % (hi - lo + 1)
    }
}

// ---------------------------------------------------------------- 整包

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    Start,
    Stop,
    Wait,
    Get(u32),
    Release(u32, u32),
}

/// 等待行为。
pub enum WaitMode {
    /// 立即返回 Signaled，返回前调用钩子（参数为第几次等待，从 0 起）。
    Scripted(Box<dyn FnMut(u64) + Send>),
    /// 模拟从不缺数据的设备：等到队列中至少一包、EOF 或停止才返回。
    Patient(Arc<RenderControl>),
}

pub struct FakeApi {
    pub calls: Vec<Call>,
    /// 按顺序拼接的已释放包字节。
    pub rendered: Vec<u8>,
    pub block_align: usize,
    buf: Vec<u8>,
    outstanding: Option<u32>,
    gets: u64,
    waits: u64,
    pub fail_get_at: Option<u64>,
    pub timeout_at_wait: Option<u64>,
    mode: WaitMode,
    buffer_frames: u32,
}

impl FakeApi {
    pub fn new(buffer_frames: u32, block_align: usize, mode: WaitMode) -> Self {
        Self {
            calls: Vec::new(),
            rendered: Vec::new(),
            block_align,
            buf: vec![0xAA; buffer_frames as usize * block_align],
            outstanding: None,
            gets: 0,
            waits: 0,
            fail_get_at: None,
            timeout_at_wait: None,
            mode,
            buffer_frames,
        }
    }
}

impl PacketApi for FakeApi {
    fn start(&mut self) -> Result<(), ApiError> {
        self.calls.push(Call::Start);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), ApiError> {
        self.calls.push(Call::Stop);
        Ok(())
    }

    fn wait(&mut self, _timeout_ms: u32) -> Result<WaitResult, ApiError> {
        self.calls.push(Call::Wait);
        let index = self.waits;
        self.waits += 1;
        if self.timeout_at_wait == Some(index) {
            return Ok(WaitResult::Timeout);
        }
        match &mut self.mode {
            WaitMode::Scripted(hook) => {
                hook(index);
                Ok(WaitResult::Signaled)
            }
            WaitMode::Patient(ctl) => {
                let start = Instant::now();
                loop {
                    let pushed = ctl.pushed_frames.load(Ordering::Acquire);
                    let valid = ctl.valid_frames.load(Ordering::Acquire);
                    if ctl.eof.load(Ordering::Acquire)
                        || ctl.stop.load(Ordering::Acquire)
                        || pushed.saturating_sub(valid) >= u64::from(self.buffer_frames)
                    {
                        return Ok(WaitResult::Signaled);
                    }
                    if start.elapsed() > Duration::from_secs(20) {
                        return Ok(WaitResult::Timeout);
                    }
                    std::thread::sleep(Duration::from_micros(50));
                }
            }
        }
    }

    fn get_buffer(&mut self, frames: u32) -> Result<&mut [u8], ApiError> {
        assert!(self.outstanding.is_none(), "GetBuffer 前有未释放的包");
        let index = self.gets;
        self.gets += 1;
        if self.fail_get_at == Some(index) {
            return Err(ApiError {
                op: ApiOp::GetBuffer,
                code: DEVICE_INVALIDATED,
            });
        }
        self.calls.push(Call::Get(frames));
        self.outstanding = Some(frames);
        // 先填垃圾字节，确认渲染循环写满整包（含静音部分）。
        self.buf.fill(0xAA);
        let len = frames as usize * self.block_align;
        Ok(&mut self.buf[..len])
    }

    fn release_buffer(&mut self, frames: u32, flags: u32) -> Result<(), ApiError> {
        assert_eq!(
            self.outstanding.take(),
            Some(frames),
            "ReleaseBuffer 未与 GetBuffer 配对"
        );
        self.calls.push(Call::Release(frames, flags));
        let len = frames as usize * self.block_align;
        self.rendered.extend_from_slice(&self.buf[..len]);
        Ok(())
    }
}

/// 检查整包调用序列：首包在 Start 之前；每个 Get 后紧跟同帧数、标志为 0 的 Release。
pub fn check_calls(calls: &[Call], frames: u32) {
    let mut started = false;
    let mut i = 0;
    while i < calls.len() {
        match calls[i] {
            Call::Get(n) => {
                assert_eq!(n, frames, "每包请求帧数须等于端点缓冲帧数");
                assert_eq!(
                    calls.get(i + 1),
                    Some(&Call::Release(frames, 0)),
                    "第 {i} 个调用未配对"
                );
                i += 2;
                continue;
            }
            Call::Release(..) => panic!("孤立的 Release"),
            Call::Start => {
                assert!(!started);
                assert_eq!(i, 2, "Start 须在首包之后");
                started = true;
            }
            Call::Wait | Call::Stop => assert!(started || matches!(calls[i], Call::Stop)),
        }
        i += 1;
    }
    assert!(
        !calls
            .iter()
            .any(|c| matches!(c, Call::Release(_, f) if f & SILENT_FLAG != 0)),
        "不得使用静音标志"
    );
}

// ---------------------------------------------------------------- 短写

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WCall {
    Start,
    Stop,
    Drain,
    Wait,
    Avail(Avail),
    /// 所给帧数、接受帧数（xrun 时为 `None`）。
    Write(u32, Option<u32>),
    Recover,
}

/// 短写设备的等待行为。
pub enum WriteWait {
    /// 每次 `avail` 先调用钩子（参数为第几次查询，从 0 起；未启动时也调用，模拟时间流逝），
    /// 每次等待播放 `play` 帧；设备中不足 `play` 帧时播完剩余数据后进入 xrun
    /// （缺失帧 = 不足部分）。
    Scripted {
        hook: Box<dyn FnMut(u64) + Send>,
        play: u32,
    },
    /// 从不让设备空转：设备中有数据时随机播放 1..=全部；没有数据时等到队列有数据、
    /// EOF 或停止。
    Patient(Arc<RenderControl>),
}

pub struct FakeWriteApi {
    pub calls: Vec<WCall>,
    /// 设备按顺序播放出的字节（只含真实写入的数据）。
    pub rendered: Vec<u8>,
    pub block_align: usize,
    pub buffer_frames: u32,
    device: VecDeque<u8>,
    started: bool,
    xrun: Option<u32>,
    waits: u64,
    idle_waits: u64,
    avails: u64,
    writes: u64,
    rng: FakeRng,
    /// 每次写入随机只接受一部分（短写）。
    pub short_writes: bool,
    pub fail_write_at: Option<u64>,
    pub timeout_at_wait: Option<u64>,
    mode: WriteWait,
}

impl FakeWriteApi {
    pub fn new(buffer_frames: u32, block_align: usize, seed: u64, mode: WriteWait) -> Self {
        Self {
            calls: Vec::new(),
            rendered: Vec::new(),
            block_align,
            buffer_frames,
            device: VecDeque::new(),
            started: false,
            xrun: None,
            waits: 0,
            idle_waits: 0,
            avails: 0,
            writes: 0,
            rng: FakeRng::new(seed),
            short_writes: false,
            fail_write_at: None,
            timeout_at_wait: None,
            mode,
        }
    }

    fn in_device(&self) -> u32 {
        (self.device.len() / self.block_align) as u32
    }

    fn play(&mut self, frames: u32) {
        let n = (frames as usize * self.block_align).min(self.device.len());
        self.rendered.extend(self.device.drain(..n));
    }
}

impl WriteApi for FakeWriteApi {
    fn start(&mut self) -> Result<(), ApiError> {
        assert!(!self.started, "重复 start");
        assert!(self.in_device() > 0, "设备中无数据就 start");
        self.calls.push(WCall::Start);
        self.started = true;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), ApiError> {
        self.calls.push(WCall::Stop);
        self.device.clear();
        self.started = false;
        Ok(())
    }

    fn drain(&mut self) -> Result<(), ApiError> {
        self.calls.push(WCall::Drain);
        let n = self.in_device();
        self.play(n);
        self.started = false;
        Ok(())
    }

    fn wait(&mut self, _timeout_ms: u32) -> Result<WaitResult, ApiError> {
        assert!(self.started, "未启动时等待");
        self.calls.push(WCall::Wait);
        let index = self.waits;
        self.waits += 1;
        if self.timeout_at_wait == Some(index) {
            return Ok(WaitResult::Timeout);
        }
        match &mut self.mode {
            WriteWait::Scripted { play, .. } => {
                let play = *play;
                let have = self.in_device();
                self.play(play);
                if have < play {
                    self.xrun = Some(play - have);
                }
                Ok(WaitResult::Signaled)
            }
            WriteWait::Patient(ctl) => {
                let ctl = Arc::clone(ctl);
                let start = Instant::now();
                loop {
                    let have = self.in_device();
                    if have > 0 {
                        let k = self.rng.range(1, u64::from(have)) as u32;
                        self.play(k);
                        return Ok(WaitResult::Signaled);
                    }
                    let pushed = ctl.pushed_frames.load(Ordering::Acquire);
                    let valid = ctl.valid_frames.load(Ordering::Acquire);
                    if ctl.eof.load(Ordering::Acquire) && pushed <= valid {
                        // EOF 且全部数据已播完，循环却仍在等待（应已排空）：多次后报告超时，
                        // 让缺陷表现为明确失败而不是挂起。
                        self.idle_waits += 1;
                        if self.idle_waits > 1000 {
                            return Ok(WaitResult::Timeout);
                        }
                    }
                    if ctl.eof.load(Ordering::Acquire)
                        || ctl.stop.load(Ordering::Acquire)
                        || pushed > valid
                    {
                        return Ok(WaitResult::Signaled);
                    }
                    if start.elapsed() > Duration::from_secs(20) {
                        return Ok(WaitResult::Timeout);
                    }
                    std::thread::sleep(Duration::from_micros(50));
                }
            }
        }
    }

    fn avail(&mut self) -> Result<Avail, ApiError> {
        if let WriteWait::Scripted { hook, .. } = &mut self.mode {
            hook(self.avails);
        }
        self.avails += 1;
        let a = match self.xrun {
            Some(missing_frames) => Avail::Xrun { missing_frames },
            None => Avail::Ready(self.buffer_frames - self.in_device()),
        };
        self.calls.push(WCall::Avail(a));
        Ok(a)
    }

    fn write(&mut self, frames: &[u8]) -> Result<Written, ApiError> {
        assert_eq!(frames.len() % self.block_align, 0, "写入了不完整的帧");
        let given = (frames.len() / self.block_align) as u32;
        assert!(given > 0, "写入 0 帧");
        let index = self.writes;
        self.writes += 1;
        if self.fail_write_at == Some(index) {
            return Err(ApiError {
                op: ApiOp::Write,
                code: DEVICE_GONE,
            });
        }
        if let Some(missing_frames) = self.xrun {
            self.calls.push(WCall::Write(given, None));
            return Ok(Written::Xrun { missing_frames });
        }
        let room = self.buffer_frames - self.in_device();
        let mut k = given.min(room);
        if self.short_writes && k > 0 {
            k = self.rng.range(0, u64::from(k)) as u32;
        }
        let n = k as usize * self.block_align;
        self.device.extend(&frames[..n]);
        self.calls.push(WCall::Write(given, Some(k)));
        Ok(Written::Frames(k))
    }

    fn recover(&mut self) -> Result<(), ApiError> {
        assert!(self.xrun.is_some(), "无 xrun 时 recover");
        self.calls.push(WCall::Recover);
        self.xrun = None;
        self.started = false;
        self.device.clear();
        Ok(())
    }
}

/// 检查短写调用序列：Start 前已有被接受的写入；未启动时不等待；Stop/Drain 之后不再写入；
/// Recover 只跟在报告 xrun 的 Avail/Write 之后。
pub fn check_write_calls(calls: &[WCall]) {
    let mut started = false;
    let mut accepted_since_prepare = 0u64;
    let mut xrun_seen = false;
    let mut ended = false;
    for (i, c) in calls.iter().enumerate() {
        assert!(!ended, "结束后仍有调用 #{i} {c:?}");
        match *c {
            WCall::Start => {
                assert!(
                    !started && accepted_since_prepare > 0,
                    "#{i} Start 前无数据"
                );
                started = true;
            }
            WCall::Wait => assert!(started, "#{i} 未启动时等待"),
            WCall::Avail(Avail::Xrun { .. }) | WCall::Write(_, None) => xrun_seen = true,
            WCall::Avail(Avail::Ready(_)) => {}
            WCall::Write(_, Some(k)) => accepted_since_prepare += u64::from(k),
            WCall::Recover => {
                assert!(xrun_seen, "#{i} 无 xrun 时 Recover");
                xrun_seen = false;
                started = false;
                accepted_since_prepare = 0;
            }
            WCall::Stop | WCall::Drain => ended = true,
        }
    }
}

// ---------------------------------------------------------------- 优先级

#[derive(Clone, Default)]
pub struct FakePriority {
    /// 报告中的名称；默认 `测试优先级`。
    pub label: Option<&'static str>,
    pub fail_register: Option<ErrorCode>,
    pub registers: Arc<AtomicU32>,
    pub reverts: Arc<AtomicU32>,
}

impl ThreadPriority for FakePriority {
    fn label(&self) -> &'static str {
        self.label.unwrap_or("测试优先级")
    }

    fn register(&mut self) -> Result<(), ErrorCode> {
        self.registers.fetch_add(1, Ordering::SeqCst);
        match self.fail_register {
            Some(code) => Err(code),
            None => Ok(()),
        }
    }

    fn revert(&mut self) -> Result<(), ErrorCode> {
        self.reverts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

// ---------------------------------------------------------------- 后端

/// 产生协商记录的策略。
pub trait Negotiator: Send + 'static {
    type Attempt: AttemptRecord;
    /// 依据设备支持的 (C, V) 列表协商；成功返回设备格式与协商记录。
    #[allow(clippy::type_complexity)]
    fn negotiate(
        &mut self,
        source: &SourceFormat,
        supported: &[(u8, u8)],
    ) -> Result<(DeviceFormat, Negotiation<Self::Attempt>), OpenFailure<Self::Attempt>>;
}

/// 通用假协商记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestAttempt {
    pub format: DeviceFormat,
    pub accepted: bool,
}

impl AttemptRecord for TestAttempt {
    fn requested(&self) -> DeviceFormat {
        self.format
    }

    fn describe(&self) -> String {
        if self.accepted {
            "（假设备）接受".into()
        } else {
            "（假设备）不支持".into()
        }
    }

    fn applied(&self) -> Option<AppliedParams> {
        self.accepted.then_some(AppliedParams {
            format: Some(self.format),
            buffer_frames: Some(self.format.sample_rate() / 100),
            period_frames: None,
            note: Some("假设备报告"),
        })
    }
}

/// 设备格式候选 (C, V)：V ≥ N 中 V 最小者优先，同 V 先小容器。
pub fn test_candidates(source: &SourceFormat) -> Vec<(u8, u8)> {
    [(16u8, 16u8), (24, 24), (32, 24), (32, 32)]
        .into_iter()
        .filter(|&(_, v)| v >= source.bits())
        .collect()
}

/// 依次尝试 [`test_candidates`]，设备支持即接受。
#[derive(Debug, Clone, Copy, Default)]
pub struct SimpleNegotiator;

impl Negotiator for SimpleNegotiator {
    type Attempt = TestAttempt;

    fn negotiate(
        &mut self,
        source: &SourceFormat,
        supported: &[(u8, u8)],
    ) -> Result<(DeviceFormat, Negotiation<TestAttempt>), OpenFailure<TestAttempt>> {
        let mut n = Negotiation::new(*source);
        for (c, v) in test_candidates(source) {
            let Ok(format) = DeviceFormat::wasapi(source.sample_rate(), source.channels(), c, v)
            else {
                continue;
            };
            let accepted = supported.contains(&(c, v));
            n.attempts.push(TestAttempt { format, accepted });
            if accepted {
                n.chosen = Some(n.attempts.len() - 1);
                return Ok((format, n));
            }
        }
        Err(OpenFailure {
            negotiation: n,
            message: "设备不支持该格式".into(),
        })
    }
}

/// 假设备的提交语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeDevice {
    /// 整包：端点缓冲 = rate/100 帧，排空 2 包。
    Packet,
    /// 短写：设备缓冲 = rate/50 帧，按种子随机短写与随机播放量。
    ShortWrite { seed: u64 },
}

/// 一个假输出流的完整记录。
#[derive(Debug, Clone)]
pub struct FakeOutput {
    pub format: DeviceFormat,
    pub rendered: Vec<u8>,
    pub calls: Vec<Call>,
    pub write_calls: Vec<WCall>,
    pub report: RenderReport,
}

/// 假后端：按设备支持列表协商，渲染线程运行真实渲染循环与“耐心”假设备。
#[derive(Clone)]
pub struct FakeBackend<N: Negotiator = SimpleNegotiator> {
    pub supported: Vec<(u8, u8)>,
    pub outputs: Arc<Mutex<Vec<FakeOutput>>>,
    /// 第 n 个流在第 k 次 get_buffer / write 时报告设备失效。
    pub invalidate: Option<(usize, u64)>,
    pub priority: FakePriority,
    pub device: FakeDevice,
    pub negotiator: N,
    opened: usize,
}

impl FakeBackend<SimpleNegotiator> {
    pub fn new(supported: &[(u8, u8)]) -> Self {
        Self::with_negotiator(supported, SimpleNegotiator)
    }
}

impl<N: Negotiator> FakeBackend<N> {
    pub fn with_negotiator(supported: &[(u8, u8)], negotiator: N) -> Self {
        Self {
            supported: supported.to_vec(),
            outputs: Arc::default(),
            invalidate: None,
            priority: FakePriority::default(),
            device: FakeDevice::Packet,
            negotiator,
            opened: 0,
        }
    }

    pub fn outputs(&self) -> Vec<FakeOutput> {
        self.outputs.lock().map(|o| o.clone()).unwrap_or_default()
    }
}

pub struct FakePending<A> {
    format: DeviceFormat,
    negotiation: Negotiation<A>,
    outputs: Arc<Mutex<Vec<FakeOutput>>>,
    fail_at: Option<u64>,
    priority: FakePriority,
    device: FakeDevice,
}

pub struct FakeRunning(JoinHandle<(FrameConsumer, RenderReport)>);

impl<N: Negotiator> Backend for FakeBackend<N> {
    type Attempt = N::Attempt;
    type Pending = FakePending<N::Attempt>;

    fn open(&mut self, source: &SourceFormat) -> Result<Self::Pending, OpenFailure<N::Attempt>> {
        let index = self.opened;
        self.opened += 1;
        let (format, negotiation) = self.negotiator.negotiate(source, &self.supported)?;
        Ok(FakePending {
            format,
            negotiation,
            outputs: Arc::clone(&self.outputs),
            fail_at: self.invalidate.and_then(|(n, k)| (n == index).then_some(k)),
            priority: self.priority.clone(),
            device: self.device,
        })
    }
}

impl<A: AttemptRecord> PendingStream for FakePending<A> {
    type Attempt = A;
    type Running = FakeRunning;

    fn format(&self) -> DeviceFormat {
        self.format
    }

    fn negotiation(&self) -> &Negotiation<A> {
        &self.negotiation
    }

    fn submission(&self) -> Submission {
        match self.device {
            FakeDevice::Packet => Submission::Packet {
                frames: self.format.sample_rate() / 100,
            },
            FakeDevice::ShortWrite { .. } => Submission::ShortWrite {
                buffer_frames: Some(self.format.sample_rate() / 50),
            },
        }
    }

    fn start(self, mut consumer: FrameConsumer, ctl: Arc<RenderControl>) -> FakeRunning {
        let format = self.format;
        let outputs = self.outputs;
        let mut priority = self.priority;
        let fail_at = self.fail_at;
        let block_align = usize::from(format.block_align());
        let device = self.device;
        FakeRunning(std::thread::spawn(move || {
            let (report, rendered, calls, write_calls) = match device {
                FakeDevice::Packet => {
                    let buffer_frames = format.sample_rate() / 100;
                    let cfg = PacketConfig {
                        buffer_frames,
                        block_align,
                        drain_packets: 2,
                        event_timeout_ms: 2000,
                    };
                    let mut api = FakeApi::new(
                        buffer_frames,
                        block_align,
                        WaitMode::Patient(Arc::clone(&ctl)),
                    );
                    api.fail_get_at = fail_at;
                    let r = packet::render(&mut api, &mut priority, &mut consumer, &ctl, cfg);
                    (r, api.rendered, api.calls, Vec::new())
                }
                FakeDevice::ShortWrite { seed } => {
                    let cfg = WriteConfig {
                        block_align,
                        event_timeout_ms: 2000,
                        idle_sleep: Duration::from_micros(100),
                    };
                    let mut api = FakeWriteApi::new(
                        format.sample_rate() / 50,
                        block_align,
                        seed,
                        WriteWait::Patient(Arc::clone(&ctl)),
                    );
                    api.short_writes = true;
                    api.fail_write_at = fail_at;
                    let r = write::render(&mut api, &mut priority, &mut consumer, &ctl, cfg);
                    (r, api.rendered, Vec::new(), api.calls)
                }
            };
            if let Ok(mut o) = outputs.lock() {
                o.push(FakeOutput {
                    format,
                    rendered,
                    calls,
                    write_calls,
                    report,
                });
            }
            (consumer, report)
        }))
    }
}

impl RunningStream for FakeRunning {
    fn is_finished(&self) -> bool {
        self.0.is_finished()
    }

    fn join(self) -> (Option<FrameConsumer>, Option<RenderReport>) {
        match self.0.join() {
            Ok((c, r)) => (Some(c), Some(r)),
            Err(_) => (None, None),
        }
    }
}
