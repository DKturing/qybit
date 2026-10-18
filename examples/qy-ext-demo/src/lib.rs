//! QyBit 外部扩展示例（X1）：从仓库外部的视角，只用公开接口接入一个新格式、内存音源与无硬件
//! 输出，不修改任何 `qy-*` crate。
//!
//! - [`qyt1`]：示例格式 QYT1 的解码器（实现 `Decoder` + `Resources`），写法与自有解码器相同；
//! - [`ExtDecoder`]：组装层的组合解码器——文件头是 `QYT1` 时用示例解码器，否则交给
//!   `AutoDecoder`（WAV / FLAC / AIFF）。桌面经 `HostResources::with_decoder` 接入，嵌入式经
//!   `qy_host_embedded::Player::new` 接入；
//! - [`MemorySource`]：内存中的曲目（`TrackSource`），可选不可定位（像管道）；
//! - [`Recorder`]：按真实时间消费、不访问硬件的输出后端（`Backend`），记录送到“设备”的有效字节；
//! - [`ExtFormats`]：应用组装（E2）——同一套格式交给 `qybit::run` / `qybit_gui::run`，得到带
//!   QYT1 的命令行（`qybit-ext`）与图形界面（`qybit-ext-gui`，`gui` 特性）；
//! - [`embedded`]：嵌入式组装——内存曲目（`Storage`）、虚拟时间 I²S（`AudioOutput`）与共享喂数器。
//!
//! 扩展契约与接入方法见 `docs/x1-extensions.md`。

pub mod embedded;
mod formats;
pub mod qyt1;

use std::io::{self, Cursor, ErrorKind, Read, Seek, SeekFrom};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use qy_codec_auto::AutoDecoder;
use qy_codec_flac::Capacity;
use qy_core::engine::{CapabilityError, DecoderDemand, Repair, Resources};
use qy_core::{DecodeError, DeviceFormat, Progress, SeekError, SourceFormat, Status};
use qy_pcm_format::ring::FrameConsumer;
use qy_player::render::packet::{PacketApi, PacketConfig, render};
use qy_player::render::{
    ApiError, ErrorCode, RenderControl, RenderReport, ThreadPriority, WaitResult,
};
use qy_player::report::{AttemptRecord, Negotiation, OpenFailure, Submission};
use qy_player::{Backend, Diagnostics, PendingStream, RunningStream, TrackSource};

pub use formats::{EXT_EXTENSIONS, ExtFormats};
pub use qyt1::{Qyt1Decoder, Qyt1Detail};

// ---- 组装：组合解码器 ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Active {
    Qyt1,
    Auto,
}

/// 组合解码器：按文件头前 4 字节选择 QYT1 或 `AutoDecoder`。两者的工作区都由宿主提供，换曲时
/// 复用。
pub struct ExtDecoder {
    qyt1: Qyt1Decoder,
    auto: AutoDecoder,
    active: Option<Active>,
}

impl ExtDecoder {
    pub fn new(qyt1: Qyt1Decoder, auto: AutoDecoder) -> Self {
        Self {
            qyt1,
            auto,
            active: None,
        }
    }

    /// 核心输入窗口须满足的最小容量（QYT1 不需要大于一帧的窗口，取 `AutoDecoder` 的要求）。
    pub fn required_window(&self) -> usize {
        self.auto.required_window()
    }

    /// 桌面组装用：WAV、AIFF、FLAC（桌面多声道容量）与 QYT1，工作区一次性分配。
    pub fn desktop(seekable: bool) -> Self {
        let cap = Capacity::DESKTOP_MULTICHANNEL;
        let auto = match AutoDecoder::new(
            leak(qy_core::PCM_WORK_SAMPLES),
            cap,
            leak(cap.workspace_len()),
            leak(cap.pcm_len()),
        ) {
            Ok(a) => a,
            Err(e) => panic!("AutoDecoder 工作区：{e:?}"),
        };
        let pcm = leak(qy_core::PCM_WORK_SAMPLES);
        let qyt1 = if seekable {
            Qyt1Decoder::new(pcm)
        } else {
            Qyt1Decoder::without_seek(pcm)
        };
        match qyt1 {
            Ok(q) => Self::new(q, auto),
            Err(e) => panic!("QYT1 工作区：{e:?}"),
        }
    }

    /// 当前曲目选中的解码器是不是 QYT1。
    pub fn is_qyt1(&self) -> bool {
        self.active == Some(Active::Qyt1)
    }

    pub fn qyt1(&self) -> &Qyt1Decoder {
        &self.qyt1
    }

    pub fn auto(&self) -> &AutoDecoder {
        &self.auto
    }
}

/// 启动时一次性取得 `'static` 缓冲（与桌面宿主相同的所有权模型，见 `docs/arch-core-mcu.md` §6）。
pub fn leak<T: Default + Clone>(n: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); n].into_boxed_slice())
}

impl qy_core::Decoder for ExtDecoder {
    fn reset(&mut self) {
        self.qyt1.reset();
        self.auto.reset();
        self.active = None;
    }

    fn format(&self) -> Option<SourceFormat> {
        match self.active? {
            Active::Qyt1 => self.qyt1.format(),
            Active::Auto => self.auto.format(),
        }
    }

    fn decode(&mut self, input: &[u8], eof: bool) -> Result<Progress, DecodeError> {
        let active = match self.active {
            Some(a) => a,
            None => {
                let a = match input.get(..4) {
                    Some(m) if m == qyt1::MAGIC || m == qyt1::MAGIC_LOSSY => Active::Qyt1,
                    Some(_) => Active::Auto,
                    // 不足 4 字节且源未结束：先不选。
                    None if !eof => {
                        return Ok(Progress {
                            consumed: 0,
                            status: Status::NeedInput,
                        });
                    }
                    None => Active::Auto,
                };
                self.active = Some(a);
                a
            }
        };
        match active {
            Active::Qyt1 => self.qyt1.decode(input, eof),
            Active::Auto => self.auto.decode(input, eof),
        }
    }

    fn pcm(&self) -> &[i32] {
        match self.active {
            Some(Active::Qyt1) => self.qyt1.pcm(),
            Some(Active::Auto) => self.auto.pcm(),
            None => &[],
        }
    }

    fn release(&mut self, frames: usize) {
        match self.active {
            Some(Active::Qyt1) => self.qyt1.release(frames),
            Some(Active::Auto) => self.auto.release(frames),
            None => {}
        }
    }

    fn seek(&mut self, frame: u64) -> Result<u64, SeekError> {
        match self.active {
            Some(Active::Qyt1) => self.qyt1.seek(frame),
            Some(Active::Auto) => self.auto.seek(frame),
            None => Err(SeekError::Unsupported),
        }
    }

    fn seek_request(&mut self) -> Option<u64> {
        match self.active {
            Some(Active::Qyt1) => self.qyt1.seek_request(),
            Some(Active::Auto) => self.auto.seek_request(),
            None => None,
        }
    }
}

impl Resources for ExtDecoder {
    fn capacity_rejection(&self) -> Option<CapabilityError> {
        match self.active {
            Some(Active::Auto) => self.auto.capacity_rejection(),
            _ => None,
        }
    }

    fn demand(&self) -> Option<DecoderDemand> {
        match self.active {
            Some(Active::Auto) => self.auto.demand(),
            _ => None,
        }
    }

    fn repair_count(&self) -> usize {
        match self.active {
            Some(Active::Auto) => self.auto.repair_count(),
            _ => 0,
        }
    }

    fn repair(&self, index: usize) -> Option<Repair> {
        match self.active {
            Some(Active::Auto) => self.auto.repair(index),
            _ => None,
        }
    }
}

impl Diagnostics for ExtDecoder {
    fn failure_details(&self, out: &mut Vec<String>) {
        match self.active {
            Some(Active::Qyt1) => {
                if let Some(d) = self.qyt1.last_detail() {
                    out.push(format!("QYT1 错误明细：{d}"));
                }
            }
            Some(Active::Auto) => self.auto.failure_details(out),
            None => {}
        }
    }

    fn skipped_tag_bytes(&self) -> u64 {
        match self.active {
            Some(Active::Auto) => Diagnostics::skipped_tag_bytes(&self.auto),
            _ => 0,
        }
    }

    fn source_notes(&self, out: &mut Vec<String>) {
        if self.active == Some(Active::Auto) {
            self.auto.source_notes(out);
        }
    }
}

// ---- 内存音源 ----

/// 内存中的曲目。`seekable` 为假时读取器不能定位（像管道），核心改为顺序解码。
pub struct MemorySource {
    pub tracks: Vec<(String, Vec<u8>)>,
    pub seekable: bool,
}

/// 内存曲目的读取器。
pub struct MemoryReader {
    inner: Cursor<Vec<u8>>,
    seekable: bool,
}

impl Read for MemoryReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Seek for MemoryReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        if !self.seekable {
            return Err(io::Error::from(ErrorKind::Unsupported));
        }
        self.inner.seek(pos)
    }
}

impl TrackSource for MemorySource {
    type Reader = MemoryReader;

    fn count(&self) -> u32 {
        u32::try_from(self.tracks.len()).unwrap_or(u32::MAX)
    }

    fn open(&mut self, track: u32) -> io::Result<MemoryReader> {
        let Some((_, bytes)) = self.tracks.get(track as usize) else {
            return Err(io::Error::new(ErrorKind::NotFound, "曲目编号越界"));
        };
        Ok(MemoryReader {
            inner: Cursor::new(bytes.clone()),
            seekable: self.seekable,
        })
    }

    fn name(&self, track: u32) -> String {
        self.tracks
            .get(track as usize)
            .map_or_else(|| format!("第 {} 首", track + 1), |(n, _)| n.clone())
    }
}

// ---- 无硬件输出 ----

/// 一次格式请求。
#[derive(Debug, Clone)]
pub struct Attempt {
    pub format: DeviceFormat,
    pub accepted: bool,
}

impl AttemptRecord for Attempt {
    fn requested(&self) -> DeviceFormat {
        self.format
    }

    fn describe(&self) -> String {
        if self.accepted {
            "（示例设备）接受".into()
        } else {
            "（示例设备）不支持".into()
        }
    }

    fn applied(&self) -> Option<qy_player::AppliedParams> {
        None
    }
}

/// 一个输出流送到“设备”的内容。
#[derive(Debug, Clone)]
pub struct Output {
    pub format: DeviceFormat,
    /// 有效帧的字节（不含暂停时送出的静音、尾部填充与排空静音）。
    pub bytes: Vec<u8>,
    pub report: Option<RenderReport>,
}

/// 按真实时间消费的无硬件后端：每 `period` 取一包（整包提交，与 WASAPI 独占同一种渲染循环），
/// 记录有效字节。`supported` 为接受的（容器位数, 有效位数）；不接受浮点。
#[derive(Clone)]
pub struct Recorder {
    pub supported: Vec<(u8, u8)>,
    pub period: Duration,
    pub outputs: Arc<Mutex<Vec<Output>>>,
}

impl Recorder {
    pub fn new(supported: &[(u8, u8)]) -> Self {
        Self {
            supported: supported.to_vec(),
            period: Duration::from_millis(5),
            outputs: Arc::default(),
        }
    }

    pub fn outputs(&self) -> Vec<Output> {
        self.outputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// 已协商的流。
pub struct Pending {
    format: DeviceFormat,
    negotiation: Negotiation<Attempt>,
    period: Duration,
    outputs: Arc<Mutex<Vec<Output>>>,
}

/// 渲染中的流。
pub struct Running(JoinHandle<(FrameConsumer, RenderReport)>);

impl Backend for Recorder {
    type Attempt = Attempt;
    type Pending = Pending;

    fn open(&mut self, source: &SourceFormat) -> Result<Pending, OpenFailure<Attempt>> {
        let mut n = Negotiation::new(*source);
        let bits = source.bits();
        if !source.encoding().is_float() {
            for (c, v) in [(16u8, 16u8), (24, 24), (32, 24), (32, 32)] {
                if bits > v {
                    continue;
                }
                let Ok(format) =
                    DeviceFormat::wasapi_layout(source.sample_rate(), source.layout(), c, v)
                else {
                    continue;
                };
                let accepted = self.supported.contains(&(c, v));
                n.attempts.push(Attempt { format, accepted });
                if accepted {
                    n.chosen = Some(n.attempts.len() - 1);
                    return Ok(Pending {
                        format,
                        negotiation: n,
                        period: self.period,
                        outputs: Arc::clone(&self.outputs),
                    });
                }
            }
        }
        Err(OpenFailure {
            negotiation: n,
            message: "示例设备不支持该格式".into(),
        })
    }
}

impl PendingStream for Pending {
    type Attempt = Attempt;
    type Running = Running;

    fn format(&self) -> DeviceFormat {
        self.format
    }

    fn negotiation(&self) -> &Negotiation<Attempt> {
        &self.negotiation
    }

    fn submission(&self) -> Submission {
        Submission::Packet {
            frames: self.packet_frames(),
        }
    }

    fn start(self, mut consumer: FrameConsumer, ctl: Arc<RenderControl>) -> Running {
        let frames = self.packet_frames();
        let ba = usize::from(self.format.block_align());
        let format = self.format;
        let outputs = self.outputs;
        let period = self.period;
        Running(std::thread::spawn(move || {
            let mut api = ClockApi {
                period,
                next: None,
                buf: vec![0; frames as usize * ba],
                block_align: ba,
                ctl: Arc::clone(&ctl),
                counted: 0,
                bytes: Vec::new(),
            };
            let cfg = PacketConfig {
                buffer_frames: frames,
                block_align: ba,
                drain_packets: 2,
                event_timeout_ms: 2000,
            };
            let report = render(&mut api, &mut NoPriority, &mut consumer, &ctl, cfg);
            api.settle();
            outputs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(Output {
                    format,
                    bytes: api.bytes,
                    report: Some(report),
                });
            (consumer, report)
        }))
    }
}

impl Pending {
    fn packet_frames(&self) -> u32 {
        let per_sec = 1_000_000 / self.period.as_micros().max(1) as u32;
        (self.format.sample_rate() / per_sec.max(1)).max(1)
    }
}

impl RunningStream for Running {
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

struct NoPriority;

impl ThreadPriority for NoPriority {
    fn label(&self) -> &'static str {
        "无（示例设备）"
    }

    fn register(&mut self) -> Result<(), ErrorCode> {
        Ok(())
    }

    fn revert(&mut self) -> Result<(), ErrorCode> {
        Ok(())
    }
}

/// 按真实时间取包的“设备”。每包的有效帧数按渲染循环的计数（`valid_frames`）确定：暂停时送出的
/// 静音、尾部填充与排空静音都不计入有效字节。
struct ClockApi {
    period: Duration,
    next: Option<Instant>,
    buf: Vec<u8>,
    block_align: usize,
    ctl: Arc<RenderControl>,
    /// 已记录的有效帧数。
    counted: u64,
    bytes: Vec<u8>,
}

impl ClockApi {
    /// 记录上一包中的有效帧（渲染循环在释放一包之后才累加计数）。
    fn settle(&mut self) {
        let valid = self.ctl.valid_frames.load(Ordering::Acquire);
        let n = valid.saturating_sub(self.counted) as usize;
        let n = n.min(self.buf.len() / self.block_align);
        self.bytes
            .extend_from_slice(&self.buf[..n * self.block_align]);
        self.counted += n as u64;
    }
}

impl PacketApi for ClockApi {
    fn start(&mut self) -> Result<(), ApiError> {
        Ok(())
    }

    fn stop(&mut self) -> Result<(), ApiError> {
        self.settle();
        Ok(())
    }

    fn wait(&mut self, _timeout_ms: u32) -> Result<WaitResult, ApiError> {
        let now = Instant::now();
        let due = self.next.unwrap_or(now + self.period);
        if due > now {
            std::thread::sleep(due - now);
        }
        // 设备时钟不等软件：落后超过一个周期时从现在重新排。
        let after = due + self.period;
        self.next = Some(if after < Instant::now() {
            Instant::now() + self.period
        } else {
            after
        });
        Ok(WaitResult::Signaled)
    }

    fn get_buffer(&mut self, _frames: u32) -> Result<&mut [u8], ApiError> {
        self.settle();
        Ok(&mut self.buf)
    }

    fn release_buffer(&mut self, _frames: u32, _flags: u32) -> Result<(), ApiError> {
        Ok(())
    }
}
