//! 测试用假平台 API 与假后端（仅 `cfg(test)`）。
//!
//! 假 API 只替代 WASAPI 调用本身，并检查调用约束：每次 `GetBuffer` 之前没有未释放的包，
//! `ReleaseBuffer` 的帧数等于对应 `GetBuffer`。解码与 pack 在测试中走真实实现。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use qy_core::{DeviceFormat, SourceFormat};
use qy_pcm_format::ring::FrameConsumer;

use crate::player::{
    Attempt, Backend, Descriptor, Negotiation, OpenFailure, PendingStream, RunningStream,
    candidates,
};
use crate::render::{
    AUDCLNT_BUFFERFLAGS_SILENT, ApiError, ApiOp, RenderApi, RenderConfig, RenderControl,
    RenderReport, ThreadPriority, WaitResult, render,
};

pub const AUDCLNT_E_DEVICE_INVALIDATED: i32 = 0x8889_0004_u32 as i32;
pub const AUDCLNT_E_UNSUPPORTED_FORMAT: i32 = 0x8889_0008_u32 as i32;

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

impl RenderApi for FakeApi {
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
                hresult: AUDCLNT_E_DEVICE_INVALIDATED,
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

#[derive(Clone, Default)]
pub struct FakePriority {
    pub fail_register: Option<u32>,
    pub registers: Arc<AtomicU32>,
    pub reverts: Arc<AtomicU32>,
}

impl ThreadPriority for FakePriority {
    fn register(&mut self) -> Result<(), u32> {
        self.registers.fetch_add(1, Ordering::SeqCst);
        match self.fail_register {
            Some(code) => Err(code),
            None => Ok(()),
        }
    }

    fn revert(&mut self) -> Result<(), u32> {
        self.reverts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// 一个假输出流的完整记录。
#[derive(Debug, Clone)]
pub struct FakeOutput {
    pub format: DeviceFormat,
    pub rendered: Vec<u8>,
    pub calls: Vec<Call>,
    pub report: RenderReport,
}

/// 假后端：按设备支持列表协商，渲染线程运行真实 `render` 循环与“耐心”假设备。
#[derive(Clone)]
pub struct FakeBackend {
    pub supported: Vec<(u8, u8)>,
    pub outputs: Arc<Mutex<Vec<FakeOutput>>>,
    /// 第 n 个流在第 k 次 GetBuffer 时报告设备失效。
    pub invalidate: Option<(usize, u64)>,
    pub priority: FakePriority,
    opened: usize,
}

impl FakeBackend {
    pub fn new(supported: &[(u8, u8)]) -> Self {
        Self {
            supported: supported.to_vec(),
            outputs: Arc::default(),
            invalidate: None,
            priority: FakePriority::default(),
            opened: 0,
        }
    }
}

pub struct FakePending {
    format: DeviceFormat,
    negotiation: Negotiation,
    outputs: Arc<Mutex<Vec<FakeOutput>>>,
    fail_get_at: Option<u64>,
    priority: FakePriority,
}

pub struct FakeRunning(JoinHandle<(FrameConsumer, RenderReport)>);

impl Backend for FakeBackend {
    type Pending = FakePending;

    fn open(&mut self, source: &SourceFormat) -> Result<FakePending, OpenFailure> {
        let index = self.opened;
        self.opened += 1;
        let mut negotiation = Negotiation {
            source: *source,
            attempts: Vec::new(),
            chosen: None,
        };
        for (c, v) in candidates(source) {
            let Ok(format) = DeviceFormat::wasapi(source.sample_rate(), source.channels(), c, v)
            else {
                continue;
            };
            let ok = self.supported.contains(&(c, v));
            negotiation.attempts.push(Attempt {
                format,
                descriptor: Descriptor::Extensible,
                is_format_supported: Some(if ok { 0 } else { AUDCLNT_E_UNSUPPORTED_FORMAT }),
                initialize: ok.then_some(0),
                aligned_retry: None,
                buffer_frames: ok.then_some(source.sample_rate() / 100),
                period_hns: ok.then_some(100_000),
            });
            if ok {
                negotiation.chosen = Some(negotiation.attempts.len() - 1);
                return Ok(FakePending {
                    format,
                    negotiation,
                    outputs: Arc::clone(&self.outputs),
                    fail_get_at: self.invalidate.and_then(|(n, k)| (n == index).then_some(k)),
                    priority: self.priority.clone(),
                });
            }
        }
        Err(OpenFailure {
            negotiation,
            message: "设备不支持该格式".into(),
        })
    }
}

impl PendingStream for FakePending {
    type Running = FakeRunning;

    fn format(&self) -> DeviceFormat {
        self.format
    }

    fn negotiation(&self) -> &Negotiation {
        &self.negotiation
    }

    fn start(self, mut consumer: FrameConsumer, ctl: Arc<RenderControl>) -> FakeRunning {
        let buffer_frames = self.format.sample_rate() / 100;
        let cfg = RenderConfig {
            buffer_frames,
            block_align: usize::from(self.format.block_align()),
            drain_packets: 2,
            event_timeout_ms: 2000,
        };
        let format = self.format;
        let outputs = self.outputs;
        let mut priority = self.priority;
        let fail_get_at = self.fail_get_at;
        FakeRunning(std::thread::spawn(move || {
            let mut api = FakeApi::new(
                buffer_frames,
                cfg.block_align,
                WaitMode::Patient(Arc::clone(&ctl)),
            );
            api.fail_get_at = fail_get_at;
            let report = render(&mut api, &mut priority, &mut consumer, &ctl, cfg);
            if let Ok(mut o) = outputs.lock() {
                o.push(FakeOutput {
                    format,
                    rendered: api.rendered,
                    calls: api.calls,
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

/// 检查调用序列：首包在 Start 之前；每个 Get 后紧跟同帧数、标志为 0 的 Release。
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
            .any(|c| matches!(c, Call::Release(_, f) if f & AUDCLNT_BUFFERFLAGS_SILENT != 0)),
        "不得使用 SILENT 标志"
    );
}
