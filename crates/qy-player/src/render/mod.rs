//! 渲染线程循环的共用类型（M2 T1）。
//!
//! 两种提交语义各有一个循环，产出同一种 [`RenderReport`]：
//! - [`packet`]：要求整包提交的后端（WASAPI 独占事件驱动）。每包请求与释放的帧数固定，
//!   队列不足时补设备格式静音：未确认 EOF 记为欠载，已确认 EOF 记为尾部填充。
//! - [`write`]：支持短写的后端（ALSA `writei`）。只提交队列中已有的完整帧，设备只接受
//!   一部分时其余留在队列，下次继续；不补静音，设备取空由后端报告为 xrun（欠载）。
//!
//! 暂停（[`RenderControl::pause`]）时两个循环都向设备送静音、不从队列取数，静音帧单独计数，
//! 不记为欠载（引擎规定的暂停语义）。
//!
//! 两个循环都只做有界拷贝/搬运与计数：不解码、不 pack、不分配、不读文件、不等待 UI。

pub mod packet;
pub mod write;

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// 暂停时短写后端写出的静音来源（只读，不分配）。按整帧取用，帧宽不超过 18 声道 × 4 字节。
static SILENCE: [u8; 4608] = [0; 4608];

/// 保留的欠载明细条数（超出部分只计数）。
pub const MAX_UNDERRUN_RECORDS: usize = 64;

/// 生产端与渲染线程共享的控制与计数（启动前分配，循环内只做原子读写）。
#[derive(Debug, Default)]
pub struct RenderControl {
    /// 当前格式不再有新数据：生产端在推入最后一帧之后置位。
    pub eof: AtomicBool,
    /// 立即停止，不排空。
    pub stop: AtomicBool,
    /// 暂停：设备输出静音，不从队列取数（可随时恢复）。
    pub pause: AtomicBool,
    /// 暂停期间送出的静音帧数（实时可读，不是欠载）。
    pub paused_frames: AtomicU64,
    /// 生产端累计入队帧数。
    pub pushed_frames: AtomicU64,
    /// 渲染线程累计提交的有效帧数。
    pub valid_frames: AtomicU64,
    /// 欠载事件数与缺失帧数（实时可读）。
    pub underrun_events: AtomicU64,
    pub underrun_frames: AtomicU64,
    /// 本流的预填帧数（生产端在启动渲染前写入），用于水位统计的低水位线（R1）。
    pub prefill_frames: AtomicU64,
    /// 生产端线程（L1）：渲染循环在生产端等待空间、队列降到 [`Self::wake_below_frames`] 以下时，
    /// 以及循环结束时唤醒它（`Thread::unpark`，不阻塞、不分配）。`None` 时不唤醒。
    pub producer: Option<std::thread::Thread>,
    /// 生产端正在等待队列空间（L1）：生产端置位后休眠；渲染循环清除并唤醒，每次等待至多唤醒一次。
    pub space_wanted: AtomicBool,
    /// 唤醒生产端的队列帧数阈值（L1，生产端在启动渲染前写入）。
    pub wake_below_frames: AtomicU64,
    /// 渲染循环已结束（L1）：结束时先置位再唤醒生产端，生产端据此不再等待。
    pub ended: AtomicBool,
}

impl RenderControl {
    /// 由生产端线程创建：渲染循环按 [`Self::space_freed`] 与结束时唤醒该线程。
    pub fn for_producer(producer: std::thread::Thread) -> Self {
        Self {
            producer: Some(producer),
            ..Self::default()
        }
    }

    /// 渲染循环取数后调用（L1）：队列只剩 `queued` 帧、不超过阈值且生产端在等待空间时唤醒它。
    /// 未在等待时只有两次原子读。
    pub fn space_freed(&self, queued: usize) {
        if queued as u64 <= self.wake_below_frames.load(Ordering::Relaxed)
            && self.space_wanted.load(Ordering::Relaxed)
            && self.space_wanted.swap(false, Ordering::AcqRel)
        {
            self.wake();
        }
    }

    /// 渲染循环结束（L1）：置位后唤醒生产端。
    fn end(&self) {
        self.ended.store(true, Ordering::Release);
        self.wake();
    }

    fn wake(&self) {
        if let Some(t) = &self.producer {
            t.unpark();
        }
    }
}

/// 渲染循环结束时（含 panic 展开）通知生产端。
struct EndGuard<'a>(&'a RenderControl);

impl Drop for EndGuard<'_> {
    fn drop(&mut self) {
        self.0.end();
    }
}

/// 平台 API 操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiOp {
    Start,
    Stop,
    Wait,
    /// 整包后端：请求端点缓冲。
    GetBuffer,
    /// 整包后端：释放端点缓冲。
    ReleaseBuffer,
    /// 短写后端：查询可写帧数。
    Avail,
    /// 短写后端：写入。
    Write,
    /// 短写后端：xrun 后恢复。
    Recover,
    /// 短写后端：播完已写入数据。
    Drain,
}

/// 平台错误码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// Windows HRESULT。
    Hresult(i32),
    /// Win32 错误码（如 MMCSS）。
    Win32(u32),
    /// POSIX errno（ALSA 返回值取负）。
    Errno(i32),
    /// 渲染循环等待设备超时。
    Timeout,
    /// 后端交出的缓冲长度与请求不符。
    BufferLength,
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Hresult(h) => write!(f, "HRESULT 0x{:08X}", h as u32),
            Self::Win32(c) => write!(f, "Win32 错误 {c}"),
            Self::Errno(e) => write!(f, "errno {e}"),
            Self::Timeout => f.write_str("等待设备超时"),
            Self::BufferLength => f.write_str("缓冲长度与请求不符"),
        }
    }
}

/// 平台 API 错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiError {
    pub op: ApiOp,
    pub code: ErrorCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitResult {
    Signaled,
    Timeout,
}

/// 渲染线程优先级提升（WASAPI 为 MMCSS，ALSA 为实时调度）。只作尝试：失败写入报告，
/// 不阻止播放。
pub trait ThreadPriority {
    /// 报告中的名称，如 `MMCSS “Pro Audio”`。
    fn label(&self) -> &'static str;
    fn register(&mut self) -> Result<(), ErrorCode>;
    fn revert(&mut self) -> Result<(), ErrorCode>;
}

/// 优先级注册与撤销结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriorityRecord {
    pub label: &'static str,
    pub registered: Result<(), ErrorCode>,
    /// 注册成功时的撤销结果；注册失败则为 `None`。
    pub reverted: Option<Result<(), ErrorCode>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnderrunRecord {
    /// 提交序号：整包后端为包序号（0 为启动前的首包），短写后端为写入调用序号。
    pub packet: u64,
    /// 欠载处的设备帧位置。
    pub device_frame: u64,
    /// 缺失帧数：整包后端为补入的静音帧；短写后端为后端报告的 xrun 缺失帧
    /// （后端无法测得时为 0，事件仍计数）。
    pub missing_frames: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderEnd {
    /// 已确认 EOF，有效音频全部提交并等待播放完成。
    Drained,
    /// 收到停止请求，未排空。
    Stopped,
    Error(ApiError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderReport {
    pub priority: PriorityRecord,
    pub end: RenderEnd,
    /// 提交次数：整包后端为包数，短写后端为写入调用数。
    pub packets: u64,
    pub valid_frames: u64,
    /// EOF 后最后有效包内的填充帧（仅整包后端）。
    pub tail_fill_frames: u64,
    /// 排空阶段整包静音帧（仅整包后端）。
    pub drain_frames: u64,
    /// 暂停期间送出的静音帧（不是欠载）。
    pub paused_frames: u64,
    pub underrun_frames: u64,
    pub underrun_events: u64,
    pub underruns: [Option<UnderrunRecord>; MAX_UNDERRUN_RECORDS],
    /// 软件队列水位（R1）：启动后、未到 EOF 时每次取数前观测；不含设备缓冲中的帧。
    pub watermark: qy_pcm_format::queue::Watermark,
}

impl RenderReport {
    fn new(priority: PriorityRecord) -> Self {
        Self {
            priority,
            end: RenderEnd::Stopped,
            packets: 0,
            valid_frames: 0,
            tail_fill_frames: 0,
            drain_frames: 0,
            paused_frames: 0,
            underrun_frames: 0,
            underrun_events: 0,
            underruns: [None; MAX_UNDERRUN_RECORDS],
            watermark: qy_pcm_format::queue::Watermark::new(0, 0),
        }
    }

    pub fn underrun_records(&self) -> impl Iterator<Item = &UnderrunRecord> {
        self.underruns.iter().flatten()
    }

    /// 记录一次欠载（报告与实时计数）。
    fn underrun(&mut self, ctl: &RenderControl, record: UnderrunRecord) {
        let idx = self.underrun_events as usize;
        if let Some(slot) = self.underruns.get_mut(idx) {
            *slot = Some(record);
        }
        self.underrun_events += 1;
        self.underrun_frames += u64::from(record.missing_frames);
        ctl.underrun_events.fetch_add(1, Ordering::Release);
        ctl.underrun_frames
            .fetch_add(u64::from(record.missing_frames), Ordering::Release);
    }

    fn paused(&mut self, ctl: &RenderControl, frames: u64) {
        self.paused_frames += frames;
        ctl.paused_frames.fetch_add(frames, Ordering::Release);
    }

    fn valid(&mut self, ctl: &RenderControl, frames: u64) {
        self.valid_frames += frames;
        ctl.valid_frames.fetch_add(frames, Ordering::Release);
    }
}

/// 注册优先级、运行循环、撤销优先级（正常、停止、错误路径都撤销）。
fn with_priority<P: ThreadPriority>(
    priority: &mut P,
    ctl: &RenderControl,
    run: impl FnOnce(&mut RenderReport) -> RenderEnd,
) -> RenderReport {
    let _end = EndGuard(ctl);
    let registered = priority.register();
    let mut report = RenderReport::new(PriorityRecord {
        label: priority.label(),
        registered,
        reverted: None,
    });
    report.end = run(&mut report);
    if registered.is_ok() {
        report.priority.reverted = Some(priority.revert());
    }
    report
}
