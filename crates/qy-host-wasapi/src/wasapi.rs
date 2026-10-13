//! WASAPI 独占事件驱动后端与 MMCSS（M0 T5，仅 Windows）。
//!
//! 平台边界的 `unsafe` 集中于此模块，每处注明依据。
//!
//! 协商：在渲染线程内初始化 COM（MTA），按 [`candidates`] 依次尝试设备格式；
//! 每次尝试激活新的 `IAudioClient`，先以独占模式 `IsFormatSupported`，成功再
//! `Initialize(EXCLUSIVE, EVENTCALLBACK, period, period)`。若返回
//! `AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED`，按文档用 `GetBufferSize` 计算对齐周期
//! `period = 10_000_000 × frames / rate`（四舍五入），释放旧客户端并在新激活的客户端上
//! 重试一次。不使用 `AUTOCONVERTPCM`、不回退共享模式，也不调用 `GetMixFormat`；
//! 协商依据只记录独占 `IsFormatSupported`/`Initialize` 的 HRESULT。
//!
//! 出处：Microsoft Learn `IAudioClient::Initialize`（独占事件驱动要求 periodicity 等于
//! buffer duration；对齐重试流程）、`IAudioRenderClient::GetBuffer`/`ReleaseBuffer`、
//! `AvSetMmThreadCharacteristicsW`/`AvRevertMmThreadCharacteristics`。
#![allow(unsafe_code)]

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

use qy_core::{DeviceFormat, SourceFormat};
use qy_pcm_format::ring::FrameConsumer;
use qy_pcm_format::wave::{WaveFormatEx, WaveFormatExtensible};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED, AUDCLNT_SHAREMODE_EXCLUSIVE,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, IAudioClient, IAudioRenderClient, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
    WaitForSingleObject,
};
use windows::core::{HRESULT, HSTRING, w};

use crate::player::{
    Attempt, Backend, Descriptor, Negotiation, OpenFailure, PendingStream, RunningStream,
    candidates,
};
use crate::render::{
    ApiError, ApiOp, RenderApi, RenderConfig, RenderControl, RenderReport, ThreadPriority,
    WaitResult, render,
};

/// 事件等待超时：远大于任何设备周期，超时视为设备停止送出事件。
const EVENT_TIMEOUT_MS: u32 = 2000;
/// 最后有效包之后提交的静音包数。
const DRAIN_PACKETS: u32 = 2;

/// 当前线程的 COM 初始化守卫；Drop 时配对 `CoUninitialize`。
struct ComGuard;

impl ComGuard {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: 在当前线程初始化 COM（MTA）；成功后由 Drop 配对反初始化。
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: 仅在 CoInitializeEx 成功后构造。
        unsafe { CoUninitialize() };
    }
}

/// 自动关闭的事件句柄。
struct Event(HANDLE);

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: 句柄由 CreateEventW 创建且只关闭一次。
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn hr(e: &windows::core::Error) -> i32 {
    e.code().0
}

/// 从 HRESULT 取 Win32 错误码（FACILITY_WIN32 时取低 16 位）。
fn win32_code(code: HRESULT) -> u32 {
    let v = code.0 as u32;
    if (v >> 16) & 0x1FFF == 7 {
        v & 0xFFFF
    } else {
        v
    }
}

/// WASAPI 独占后端。
pub struct WasapiBackend {
    pub device_id: String,
    /// 16/16 格式使用的描述类型；默认 EXTENSIBLE（T6 矩阵另测 WAVEFORMATEX）。
    pub descriptor_16: Descriptor,
}

impl WasapiBackend {
    pub fn new(device_id: impl Into<String>) -> Self {
        Self {
            device_id: device_id.into(),
            descriptor_16: Descriptor::Extensible,
        }
    }
}

type Go = (FrameConsumer, Arc<RenderControl>);
type ThreadOut = Option<(FrameConsumer, RenderReport)>;

pub struct WasapiPending {
    format: DeviceFormat,
    negotiation: Negotiation,
    go: Sender<Go>,
    handle: JoinHandle<ThreadOut>,
}

pub struct WasapiRunning(JoinHandle<ThreadOut>);

impl Backend for WasapiBackend {
    type Pending = WasapiPending;

    fn open(&mut self, source: &SourceFormat) -> Result<WasapiPending, OpenFailure> {
        let (info_tx, info_rx) = channel::<Result<(DeviceFormat, Negotiation), OpenFailure>>();
        let (go_tx, go_rx) = channel::<Go>();
        let id = self.device_id.clone();
        let source = *source;
        let descriptor_16 = self.descriptor_16;
        let handle = std::thread::spawn(move || {
            stream_thread(&id, &source, descriptor_16, &info_tx, &go_rx)
        });
        let failure = |message: String| OpenFailure {
            negotiation: Negotiation {
                source,
                attempts: Vec::new(),
                chosen: None,
            },
            message,
        };
        match info_rx.recv() {
            Ok(Ok((format, negotiation))) => Ok(WasapiPending {
                format,
                negotiation,
                go: go_tx,
                handle,
            }),
            Ok(Err(f)) => {
                let _ = handle.join();
                Err(f)
            }
            Err(_) => {
                let _ = handle.join();
                Err(failure("渲染线程异常结束".into()))
            }
        }
    }
}

impl PendingStream for WasapiPending {
    type Running = WasapiRunning;

    fn format(&self) -> DeviceFormat {
        self.format
    }

    fn negotiation(&self) -> &Negotiation {
        &self.negotiation
    }

    fn start(self, consumer: FrameConsumer, ctl: Arc<RenderControl>) -> WasapiRunning {
        // 发送失败意味着线程已退出；join 时得到 None。
        let _ = self.go.send((consumer, ctl));
        WasapiRunning(self.handle)
    }
}

impl RunningStream for WasapiRunning {
    fn is_finished(&self) -> bool {
        self.0.is_finished()
    }

    fn join(self) -> (Option<FrameConsumer>, Option<RenderReport>) {
        match self.0.join() {
            Ok(Some((c, r))) => (Some(c), Some(r)),
            _ => (None, None),
        }
    }
}

/// 已初始化的独占流（只在渲染线程内使用）。
struct Stream {
    client: IAudioClient,
    render: IAudioRenderClient,
    event: Event,
    buffer_frames: u32,
    format: DeviceFormat,
}

fn stream_thread(
    id: &str,
    source: &SourceFormat,
    descriptor_16: Descriptor,
    info: &Sender<Result<(DeviceFormat, Negotiation), OpenFailure>>,
    go: &Receiver<Go>,
) -> ThreadOut {
    let _com = match ComGuard::new() {
        Ok(g) => g,
        Err(e) => {
            let _ = info.send(Err(OpenFailure {
                negotiation: Negotiation {
                    source: *source,
                    attempts: Vec::new(),
                    chosen: None,
                },
                message: format!("CoInitializeEx 失败: {e}"),
            }));
            return None;
        }
    };
    let (stream, negotiation) = match negotiate(id, source, descriptor_16) {
        Ok(v) => v,
        Err(f) => {
            let _ = info.send(Err(f));
            return None;
        }
    };
    if info.send(Ok((stream.format, negotiation))).is_err() {
        return None;
    }
    // 生产端预填完成后移交消费端；若生产端放弃（通道关闭），释放流并退出。
    let (mut consumer, ctl) = go.recv().ok()?;
    let cfg = RenderConfig {
        buffer_frames: stream.buffer_frames,
        block_align: usize::from(stream.format.block_align()),
        drain_packets: DRAIN_PACKETS,
        event_timeout_ms: EVENT_TIMEOUT_MS,
    };
    let mut api = WasapiApi {
        stream: &stream,
        block_align: cfg.block_align,
    };
    let mut mmcss = Mmcss(None);
    let report = render(&mut api, &mut mmcss, &mut consumer, &ctl, cfg);
    Some((consumer, report))
}

fn negotiate(
    id: &str,
    source: &SourceFormat,
    descriptor_16: Descriptor,
) -> Result<(Stream, Negotiation), OpenFailure> {
    let mut negotiation = Negotiation {
        source: *source,
        attempts: Vec::new(),
        chosen: None,
    };
    let fail = |negotiation: Negotiation, message: String| OpenFailure {
        negotiation,
        message,
    };
    // SAFETY: COM 已在本线程初始化；接口引用计数由 windows crate 管理。
    let device: IMMDevice = match unsafe {
        CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .and_then(|e| e.GetDevice(&HSTRING::from(id)))
    } {
        Ok(d) => d,
        Err(e) => return Err(fail(negotiation, format!("找不到设备 {id}: {e}"))),
    };
    for (c, v) in candidates(source) {
        let Ok(format) = DeviceFormat::wasapi(source.sample_rate(), source.channels(), c, v) else {
            continue;
        };
        let descriptor = if c == 16 && v == 16 {
            descriptor_16
        } else {
            Descriptor::Extensible
        };
        let mut attempt = Attempt {
            format,
            descriptor,
            is_format_supported: None,
            initialize: None,
            aligned_retry: None,
            buffer_frames: None,
            period_hns: None,
        };
        let result = try_format(&device, &format, descriptor, &mut attempt);
        negotiation.attempts.push(attempt);
        if let Some(stream) = result {
            negotiation.chosen = Some(negotiation.attempts.len() - 1);
            return Ok((stream, negotiation));
        }
    }
    Err(fail(
        negotiation,
        "设备不支持任何可无损承载该源格式的独占格式".into(),
    ))
}

/// 纯描述 → SDK 结构；返回的结构在调用期间保持存活。
enum SdkFormat {
    Ext(windows::Win32::Media::Audio::WAVEFORMATEXTENSIBLE),
    Ex(WAVEFORMATEX),
}

impl SdkFormat {
    fn ptr(&self) -> *const WAVEFORMATEX {
        match self {
            SdkFormat::Ext(f) => {
                (f as *const windows::Win32::Media::Audio::WAVEFORMATEXTENSIBLE).cast()
            }
            SdkFormat::Ex(f) => f as *const WAVEFORMATEX,
        }
    }
}

fn sdk_format(format: &DeviceFormat, descriptor: Descriptor) -> Option<SdkFormat> {
    match descriptor {
        Descriptor::Extensible => WaveFormatExtensible::from_device(format)
            .ok()
            .map(|f| SdkFormat::Ext(crate::sdk_format::extensible_to_sdk(&f))),
        Descriptor::WaveFormatEx => WaveFormatEx::from_device(format)
            .ok()
            .map(|f| SdkFormat::Ex(crate::sdk_format::wave_format_ex_to_sdk(&f))),
    }
}

fn activate(device: &IMMDevice) -> Option<IAudioClient> {
    // SAFETY: 设备接口有效；激活参数为空。
    unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }.ok()
}

fn try_format(
    device: &IMMDevice,
    format: &DeviceFormat,
    descriptor: Descriptor,
    attempt: &mut Attempt,
) -> Option<Stream> {
    let sdk = sdk_format(format, descriptor)?;
    let mut client = activate(device)?;
    // SAFETY: sdk 在本函数内存活，指针指向完整的 WAVEFORMATEX(TENSIBLE) 结构；
    // 独占模式下 ppClosestMatch 必须为空。
    let supported =
        unsafe { client.IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, sdk.ptr(), None) };
    attempt.is_format_supported = Some(supported.0);
    if supported.0 != 0 {
        return None;
    }
    let mut period: i64 = 0;
    // SAFETY: 输出指针指向本地变量。
    if unsafe { client.GetDevicePeriod(Some(&mut period), None) }.is_err() || period <= 0 {
        return None;
    }
    attempt.period_hns = Some(period);
    let init = |client: &IAudioClient, period: i64| {
        // SAFETY: 同上；独占事件驱动要求 buffer duration 等于 periodicity。
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_EXCLUSIVE,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                period,
                period,
                sdk.ptr(),
                None,
            )
        }
    };
    match init(&client, period) {
        Ok(()) => attempt.initialize = Some(0),
        Err(e) if e.code() == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED => {
            attempt.initialize = Some(hr(&e));
            // SAFETY: 文档规定未对齐失败后 GetBufferSize 返回下一个对齐的缓冲帧数。
            let frames = unsafe { client.GetBufferSize() }.ok()?;
            let rate = i64::from(format.sample_rate());
            period = (10_000_000 * i64::from(frames) + rate / 2) / rate;
            attempt.period_hns = Some(period);
            // 已失效的实例不得复用：释放后激活新客户端。
            drop(client);
            client = activate(device)?;
            match init(&client, period) {
                Ok(()) => attempt.aligned_retry = Some(0),
                Err(e) => {
                    attempt.aligned_retry = Some(hr(&e));
                    return None;
                }
            }
        }
        Err(e) => {
            attempt.initialize = Some(hr(&e));
            return None;
        }
    }
    // SAFETY: 创建自动复位事件；所有权交给 Event 守卫。
    let event = Event(unsafe { CreateEventW(None, false, false, None) }.ok()?);
    // SAFETY: 客户端已初始化为事件回调模式，事件句柄有效。
    unsafe { client.SetEventHandle(event.0) }.ok()?;
    // SAFETY: 客户端已初始化。
    let buffer_frames = unsafe { client.GetBufferSize() }.ok()?;
    attempt.buffer_frames = Some(buffer_frames);
    // SAFETY: 客户端已初始化。
    let render = unsafe { client.GetService::<IAudioRenderClient>() }.ok()?;
    Some(Stream {
        client,
        render,
        event,
        buffer_frames,
        format: *format,
    })
}

struct WasapiApi<'a> {
    stream: &'a Stream,
    block_align: usize,
}

impl RenderApi for WasapiApi<'_> {
    fn start(&mut self) -> Result<(), ApiError> {
        // SAFETY: 客户端已初始化。
        unsafe { self.stream.client.Start() }.map_err(|e| ApiError {
            op: ApiOp::Start,
            hresult: hr(&e),
        })
    }

    fn stop(&mut self) -> Result<(), ApiError> {
        // SAFETY: 客户端已初始化。
        unsafe { self.stream.client.Stop() }.map_err(|e| ApiError {
            op: ApiOp::Stop,
            hresult: hr(&e),
        })
    }

    fn wait(&mut self, timeout_ms: u32) -> Result<WaitResult, ApiError> {
        // SAFETY: 事件句柄在 Stream 生命周期内有效。
        let r = unsafe { WaitForSingleObject(self.stream.event.0, timeout_ms) };
        if r == WAIT_OBJECT_0 {
            Ok(WaitResult::Signaled)
        } else if r == WAIT_TIMEOUT {
            Ok(WaitResult::Timeout)
        } else {
            Err(ApiError {
                op: ApiOp::Wait,
                hresult: windows::core::Error::from_thread().code().0,
            })
        }
    }

    fn get_buffer(&mut self, frames: u32) -> Result<&mut [u8], ApiError> {
        // SAFETY: 渲染客户端有效；成功时返回的指针指向 frames × blockAlign 字节的
        // 端点缓冲，在本线程配对的 ReleaseBuffer 之前有效且只由本线程写入。
        // 返回切片借用 &mut self，借用检查保证 ReleaseBuffer 前不会再次取得。
        unsafe {
            let ptr = self.stream.render.GetBuffer(frames).map_err(|e| ApiError {
                op: ApiOp::GetBuffer,
                hresult: hr(&e),
            })?;
            if ptr.is_null() {
                return Err(ApiError {
                    op: ApiOp::GetBuffer,
                    hresult: crate::render::HRESULT_UNEXPECTED,
                });
            }
            Ok(std::slice::from_raw_parts_mut(
                ptr,
                frames as usize * self.block_align,
            ))
        }
    }

    fn release_buffer(&mut self, frames: u32, flags: u32) -> Result<(), ApiError> {
        // SAFETY: 与本线程上一次 GetBuffer 配对，帧数相同。
        unsafe { self.stream.render.ReleaseBuffer(frames, flags) }.map_err(|e| ApiError {
            op: ApiOp::ReleaseBuffer,
            hresult: hr(&e),
        })
    }
}

/// MMCSS “Pro Audio” 注册（在渲染线程内调用）。
struct Mmcss(Option<HANDLE>);

impl ThreadPriority for Mmcss {
    fn register(&mut self) -> Result<(), u32> {
        let mut task_index = 0u32;
        // SAFETY: 任务名为静态宽字符串，task_index 指向本地变量。
        match unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) } {
            Ok(h) => {
                self.0 = Some(h);
                Ok(())
            }
            Err(e) => Err(win32_code(e.code())),
        }
    }

    fn revert(&mut self) -> Result<(), u32> {
        let Some(h) = self.0.take() else {
            return Ok(());
        };
        // SAFETY: 句柄来自本线程的 AvSetMmThreadCharacteristicsW。
        unsafe { AvRevertMmThreadCharacteristics(h) }.map_err(|e| win32_code(e.code()))
    }
}
