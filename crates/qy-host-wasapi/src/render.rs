//! 渲染线程循环（M0 T5），与平台 API 解耦以便注入假 API 测试。
//!
//! 规则（WASAPI 独占事件驱动）：
//! - 每包请求与释放的帧数都等于初始化后 `GetBufferSize` 返回的端点缓冲帧数，
//!   不按队列可用量缩小；`GetBuffer`/`ReleaseBuffer` 在本线程内严格配对。
//! - 启动前先填充第一包（初始端点缓冲），再 `Start`。
//! - 每包从已 pack 的队列按完整帧拷贝（回绕最多两段），不足部分写入设备格式静音
//!   （有符号 PCM 即全零字节）。释放时标志恒为 0：不使用 `AUDCLNT_BUFFERFLAGS_SILENT`，
//!   因为部分有效包若标记 SILENT 会把有效音频也静音。
//! - 播放中且未确认 EOF 时的不足记为欠载（包序号、设备帧位置、缺失帧数）；
//!   已确认 EOF（该格式不再有数据）后的不足是尾部整包填充，单独计数。
//! - 排空：最后一包有效音频提交后，再提交 [`RenderConfig::drain_packets`] 个静音包，
//!   等其对应事件到达（即最后的有效音频已由设备取走播放）后 `Stop`。
//! - 收到停止请求立即 `Stop`，不排空；API 错误（如设备失效）立即结束并报告。
//! - 线程开始时注册 MMCSS “Pro Audio”，结束（正常、停止、错误）时在本线程撤销。
//!
//! 本循环只做有界拷贝、填零与计数：不解码、不 pack、不分配、不读文件、不等待 UI。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use qy_pcm_format::ring::FrameConsumer;

/// `AUDCLNT_BUFFERFLAGS_SILENT`；本实现从不使用，仅供测试断言。
pub const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 0x2;
/// 等待超时的伪 HRESULT（`HRESULT_FROM_WIN32(WAIT_TIMEOUT)`）。
pub const HRESULT_WAIT_TIMEOUT: i32 = 0x8007_0102_u32 as i32;
/// 端点缓冲长度与请求不符时的伪 HRESULT（`E_UNEXPECTED`）。
pub const HRESULT_UNEXPECTED: i32 = 0x8000_FFFF_u32 as i32;
/// 保留的欠载明细条数（超出部分只计数）。
pub const MAX_UNDERRUN_RECORDS: usize = 64;

/// 生产端与渲染线程共享的控制与计数（启动前分配，循环内只做原子读写）。
#[derive(Debug, Default)]
pub struct RenderControl {
    /// 当前格式不再有新数据：生产端在推入最后一帧之后置位。
    pub eof: AtomicBool,
    /// 立即停止，不排空。
    pub stop: AtomicBool,
    /// 生产端累计入队帧数。
    pub pushed_frames: AtomicU64,
    /// 渲染线程累计提交的有效帧数。
    pub valid_frames: AtomicU64,
    /// 欠载事件数与缺失帧数（实时可读）。
    pub underrun_events: AtomicU64,
    pub underrun_frames: AtomicU64,
}

/// 平台 API 操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiOp {
    Start,
    Stop,
    Wait,
    GetBuffer,
    ReleaseBuffer,
}

/// 平台 API 错误（HRESULT）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiError {
    pub op: ApiOp,
    pub hresult: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitResult {
    Signaled,
    Timeout,
}

/// 渲染所需的平台 API（WASAPI 独占事件驱动的最小集合）。
pub trait RenderApi {
    fn start(&mut self) -> Result<(), ApiError>;
    fn stop(&mut self) -> Result<(), ApiError>;
    fn wait(&mut self, timeout_ms: u32) -> Result<WaitResult, ApiError>;
    /// 返回 `frames` 帧的端点缓冲（字节长度 = frames × blockAlign）。
    fn get_buffer(&mut self, frames: u32) -> Result<&mut [u8], ApiError>;
    fn release_buffer(&mut self, frames: u32, flags: u32) -> Result<(), ApiError>;
}

/// 线程优先级注册（MMCSS）。错误值为 Win32 错误码。
pub trait ThreadPriority {
    fn register(&mut self) -> Result<(), u32>;
    fn revert(&mut self) -> Result<(), u32>;
}

/// 渲染参数。
#[derive(Debug, Clone, Copy)]
pub struct RenderConfig {
    /// `GetBufferSize` 返回的端点缓冲帧数。
    pub buffer_frames: u32,
    pub block_align: usize,
    /// 最后有效包之后再提交的静音包数。
    pub drain_packets: u32,
    pub event_timeout_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnderrunRecord {
    /// 包序号（0 为启动前的首包）。
    pub packet: u64,
    /// 该包首帧的设备帧位置。
    pub device_frame: u64,
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

/// MMCSS 注册与撤销结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MmcssRecord {
    pub registered: Result<(), u32>,
    /// 注册成功时的撤销结果；注册失败则为 `None`。
    pub reverted: Option<Result<(), u32>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderReport {
    pub mmcss: MmcssRecord,
    pub end: RenderEnd,
    pub packets: u64,
    pub valid_frames: u64,
    /// EOF 后最后有效包内的填充帧。
    pub tail_fill_frames: u64,
    /// 排空阶段整包静音帧。
    pub drain_frames: u64,
    pub underrun_frames: u64,
    pub underrun_events: u64,
    pub underruns: [Option<UnderrunRecord>; MAX_UNDERRUN_RECORDS],
}

impl RenderReport {
    fn new(mmcss: MmcssRecord) -> Self {
        Self {
            mmcss,
            end: RenderEnd::Stopped,
            packets: 0,
            valid_frames: 0,
            tail_fill_frames: 0,
            drain_frames: 0,
            underrun_frames: 0,
            underrun_events: 0,
            underruns: [None; MAX_UNDERRUN_RECORDS],
        }
    }

    pub fn underrun_records(&self) -> impl Iterator<Item = &UnderrunRecord> {
        self.underruns.iter().flatten()
    }
}

struct Loop<'a, A: RenderApi> {
    api: &'a mut A,
    consumer: &'a mut FrameConsumer,
    ctl: &'a RenderControl,
    cfg: RenderConfig,
    report: RenderReport,
    /// 最后有效包提交后剩余的排空包数。
    drain_left: Option<u32>,
}

/// 运行渲染循环直到排空、停止或出错。调用方在专用渲染线程中调用。
pub fn render<A: RenderApi, P: ThreadPriority>(
    api: &mut A,
    priority: &mut P,
    consumer: &mut FrameConsumer,
    ctl: &RenderControl,
    cfg: RenderConfig,
) -> RenderReport {
    let registered = priority.register();
    let mut l = Loop {
        api,
        consumer,
        ctl,
        cfg,
        report: RenderReport::new(MmcssRecord {
            registered,
            reverted: None,
        }),
        drain_left: None,
    };
    let end = match l.run() {
        Ok(end) => end,
        Err(e) => {
            // 出错后尽力停止设备；停止本身的错误不覆盖原始错误。
            let _ = l.api.stop();
            RenderEnd::Error(e)
        }
    };
    let mut report = l.report;
    report.end = end;
    if registered.is_ok() {
        report.mmcss.reverted = Some(priority.revert());
    }
    report
}

impl<A: RenderApi> Loop<'_, A> {
    fn run(&mut self) -> Result<RenderEnd, ApiError> {
        if self.ctl.stop.load(Ordering::Acquire) {
            return Ok(RenderEnd::Stopped);
        }
        self.packet()?;
        self.api.start()?;
        loop {
            if self.ctl.stop.load(Ordering::Acquire) {
                self.api.stop()?;
                return Ok(RenderEnd::Stopped);
            }
            match self.api.wait(self.cfg.event_timeout_ms)? {
                WaitResult::Signaled => {}
                WaitResult::Timeout => {
                    return Err(ApiError {
                        op: ApiOp::Wait,
                        hresult: HRESULT_WAIT_TIMEOUT,
                    });
                }
            }
            if self.drain_left == Some(0) {
                self.api.stop()?;
                return Ok(RenderEnd::Drained);
            }
            self.packet()?;
        }
    }

    /// 请求、填充、释放一整包。
    fn packet(&mut self) -> Result<(), ApiError> {
        // 先读 EOF 再取数据：生产端在推入最后一帧之后才置位，
        // 因此读到 EOF 时该格式的全部数据已对本线程可见。
        let eof = self.ctl.eof.load(Ordering::Acquire);
        let frames = self.cfg.buffer_frames;
        let ba = self.cfg.block_align;
        let packet_index = self.report.packets;
        let device_frame = packet_index * u64::from(frames);
        let buf = self.api.get_buffer(frames)?;
        let len_ok = buf.len() == frames as usize * ba;
        let got = if len_ok && self.drain_left.is_none() {
            self.consumer.pop_frames(buf).unwrap_or(0)
        } else {
            0
        };
        // 其余部分写设备格式静音（有符号 PCM 为全零）。
        if let Some(rest) = buf.get_mut(got * ba..) {
            rest.fill(0);
        }
        // 释放与请求严格配对，再报告长度异常。
        self.api.release_buffer(frames, 0)?;
        if !len_ok {
            return Err(ApiError {
                op: ApiOp::GetBuffer,
                hresult: HRESULT_UNEXPECTED,
            });
        }
        let missing = frames - got as u32;
        self.report.packets += 1;
        self.report.valid_frames += got as u64;
        self.ctl
            .valid_frames
            .fetch_add(got as u64, Ordering::Release);
        match self.drain_left {
            Some(left) => {
                self.report.drain_frames += u64::from(frames);
                self.drain_left = Some(left.saturating_sub(1));
            }
            None if missing > 0 && eof => {
                self.report.tail_fill_frames += u64::from(missing);
            }
            None if missing > 0 => {
                let idx = self.report.underrun_events as usize;
                if let Some(slot) = self.report.underruns.get_mut(idx) {
                    *slot = Some(UnderrunRecord {
                        packet: packet_index,
                        device_frame,
                        missing_frames: missing,
                    });
                }
                self.report.underrun_events += 1;
                self.report.underrun_frames += u64::from(missing);
                self.ctl.underrun_events.fetch_add(1, Ordering::Release);
                self.ctl
                    .underrun_frames
                    .fetch_add(u64::from(missing), Ordering::Release);
            }
            None => {}
        }
        if self.drain_left.is_none() && eof && self.consumer.available_frames() == 0 {
            self.drain_left = Some(self.cfg.drain_packets);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    use qy_core::DeviceFormat;
    use qy_pcm_format::ring::{FrameProducer, frame_ring, reconfigure};

    use super::*;
    use crate::fake::{
        AUDCLNT_E_DEVICE_INVALIDATED, Call, FakeApi, FakePriority, WaitMode, check_calls,
    };
    use crate::testutil::{Feeder, data_chunk, fixture};

    const B: u32 = 441; // 44.1 kHz 下 10 ms

    fn fmt(rate: u32, ch: u16, c: u8, v: u8) -> DeviceFormat {
        match DeviceFormat::wasapi(rate, ch, c, v) {
            Ok(f) => f,
            Err(e) => panic!("{e:?}"),
        }
    }

    fn ring(format: &DeviceFormat, capacity: usize) -> (FrameProducer, FrameConsumer) {
        let (mut p, mut c) = frame_ring(capacity);
        if let Err(e) = reconfigure(&mut p, &mut c, format, 1) {
            panic!("{e:?}");
        }
        (p, c)
    }

    fn cfg(buffer_frames: u32, f: &DeviceFormat) -> RenderConfig {
        RenderConfig {
            buffer_frames,
            block_align: usize::from(f.block_align()),
            drain_packets: 2,
            event_timeout_ms: 2000,
        }
    }

    #[test]
    fn full_packets_eof_tail_and_drain() {
        let f = fmt(44_100, 2, 16, 16);
        let (mut p, mut c) = ring(&f, 768_000);
        let mut feeder = Feeder::new("pcm16_stereo_44k.wav", f, 1);
        feeder.feed(&mut p, usize::MAX);
        let ctl = RenderControl::default();
        ctl.eof.store(true, Ordering::Release);
        let mut api = FakeApi::new(B, 4, WaitMode::Scripted(Box::new(|_| {})));
        let mut prio = FakePriority::default();
        let r = render(&mut api, &mut prio, &mut c, &ctl, cfg(B, &f));
        check_calls(&api.calls, B);
        assert_eq!(api.calls.last(), Some(&Call::Stop));
        let raw = data_chunk(&fixture("pcm16_stereo_44k.wav"));
        let frames = raw.len() as u64 / 4; // 300
        assert_eq!(r.end, RenderEnd::Drained);
        assert_eq!(r.valid_frames, frames);
        assert_eq!(r.underrun_events, 0);
        assert_eq!(r.tail_fill_frames, u64::from(B) - frames);
        assert_eq!(r.drain_frames, 2 * u64::from(B));
        assert_eq!(r.packets, 3);
        assert_eq!(&api.rendered[..raw.len()], &raw[..]);
        assert!(
            api.rendered[raw.len()..].iter().all(|&b| b == 0),
            "静音须写满整包"
        );
        assert_eq!(api.rendered.len(), 3 * B as usize * 4);
        assert_eq!(
            r.mmcss,
            MmcssRecord {
                registered: Ok(()),
                reverted: Some(Ok(()))
            }
        );
        assert_eq!(prio.reverts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn wraparound_copy_and_underrun_vs_tail() {
        // 8 kHz 单声道 16 位：500 ms = 8000 字节的最小队列，257 帧 × 60 遍强制多次回绕。
        let f = fmt(8_000, 1, 16, 16);
        let b = 80u32;
        let (p, mut c) = ring(&f, 8_000);
        let feeder = Arc::new(Mutex::new((
            Feeder::new("pcm16_mono_8k_fmt18.wav", f, 60),
            p,
        )));
        let ctl = Arc::new(RenderControl::default());
        let hook_ctl = Arc::clone(&ctl);
        let hook_feeder = Arc::clone(&feeder);
        // 先预填 3 包；之后每次等待补 1 包，但第 10–12 次等待不补（生产端停顿）。
        if let Ok(mut g) = feeder.lock() {
            let (fd, p) = &mut *g;
            fd.feed(p, 3 * b as usize);
        }
        let hook = move |w: u64| {
            if let Ok(mut g) = hook_feeder.lock() {
                let (fd, p) = &mut *g;
                if !(10..=12).contains(&w) {
                    fd.feed(p, b as usize);
                }
                if fd.exhausted() {
                    hook_ctl.eof.store(true, Ordering::Release);
                }
            }
        };
        let mut api = FakeApi::new(b, 2, WaitMode::Scripted(Box::new(hook)));
        let mut prio = FakePriority::default();
        let r = render(&mut api, &mut prio, &mut c, &ctl, cfg(b, &f));
        check_calls(&api.calls, b);
        assert_eq!(r.end, RenderEnd::Drained);
        let raw = data_chunk(&fixture("pcm16_mono_8k_fmt18.wav"));
        let expected: Vec<u8> = raw.iter().copied().cycle().take(raw.len() * 60).collect();
        assert_eq!(r.valid_frames as usize, expected.len() / 2);
        // 欠载：停顿期间队列耗尽，播放中且未 EOF → 记为欠载，不是尾部填充。
        assert!(r.underrun_events >= 1, "{r:?}");
        let recs: Vec<_> = r.underrun_records().copied().collect();
        assert_eq!(recs.len() as u64, r.underrun_events);
        for u in &recs {
            assert!(u.missing_frames > 0 && u.missing_frames <= b);
            assert_eq!(u.device_frame, u.packet * u64::from(b));
        }
        assert_eq!(
            r.underrun_frames,
            recs.iter()
                .map(|u| u64::from(u.missing_frames))
                .sum::<u64>()
        );
        // 按欠载记录去掉每包的静音部分，有效字节须与源逐字节一致且无重复。
        let pb = b as usize * 2;
        let mut valid = Vec::new();
        let total_packets = r.packets as usize - 2; // 去掉排空包
        for k in 0..total_packets {
            let pkt = &api.rendered[k * pb..(k + 1) * pb];
            let missing = recs
                .iter()
                .find(|u| u.packet == k as u64)
                .map_or(0, |u| u.missing_frames as usize);
            let tail = if k + 1 == total_packets {
                r.tail_fill_frames as usize
            } else {
                0
            };
            let n = b as usize - missing - tail;
            assert!(
                pkt[n * 2..].iter().all(|&x| x == 0),
                "包 {k} 的填充须为静音"
            );
            valid.extend_from_slice(&pkt[..n * 2]);
        }
        assert_eq!(valid.len(), expected.len());
        assert!(valid == expected, "有效字节不一致");
        assert!(r.tail_fill_frames < u64::from(b));
    }

    #[test]
    fn stop_request_skips_drain_and_reverts_mmcss() {
        let f = fmt(44_100, 2, 16, 16);
        let (mut p, mut c) = ring(&f, 768_000);
        let mut feeder = Feeder::new("pcm16_stereo_44k.wav", f, 3);
        feeder.feed(&mut p, usize::MAX);
        let ctl = Arc::new(RenderControl::default());
        let c2 = Arc::clone(&ctl);
        let mut api = FakeApi::new(
            B,
            4,
            WaitMode::Scripted(Box::new(move |w| {
                if w == 0 {
                    c2.stop.store(true, Ordering::Release);
                }
            })),
        );
        let mut prio = FakePriority::default();
        let r = render(&mut api, &mut prio, &mut c, &ctl, cfg(B, &f));
        assert_eq!(r.end, RenderEnd::Stopped);
        assert_eq!(r.drain_frames, 0);
        assert_eq!(api.calls.last(), Some(&Call::Stop));
        assert_eq!(r.mmcss.reverted, Some(Ok(())));
        check_calls(&api.calls, B);
    }

    #[test]
    fn device_invalidated_and_timeout_are_errors_with_cleanup() {
        let f = fmt(44_100, 2, 16, 16);
        for case in 0..2 {
            let (mut p, mut c) = ring(&f, 768_000);
            let mut feeder = Feeder::new("pcm16_stereo_44k.wav", f, 10);
            feeder.feed(&mut p, usize::MAX);
            let ctl = RenderControl::default();
            let mut api = FakeApi::new(B, 4, WaitMode::Scripted(Box::new(|_| {})));
            if case == 0 {
                api.fail_get_at = Some(3);
            } else {
                api.timeout_at_wait = Some(1);
            }
            let mut prio = FakePriority::default();
            let r = render(&mut api, &mut prio, &mut c, &ctl, cfg(B, &f));
            let want = if case == 0 {
                ApiError {
                    op: ApiOp::GetBuffer,
                    hresult: AUDCLNT_E_DEVICE_INVALIDATED,
                }
            } else {
                ApiError {
                    op: ApiOp::Wait,
                    hresult: HRESULT_WAIT_TIMEOUT,
                }
            };
            assert_eq!(r.end, RenderEnd::Error(want));
            assert_eq!(api.calls.last(), Some(&Call::Stop), "出错后须尝试停止");
            assert_eq!(r.mmcss.reverted, Some(Ok(())), "出错路径也须撤销 MMCSS");
            check_calls(&api.calls, B);
        }
    }

    #[test]
    fn mmcss_failure_is_recorded_and_not_reverted() {
        let f = fmt(44_100, 2, 16, 16);
        let (mut p, mut c) = ring(&f, 768_000);
        Feeder::new("pcm16_stereo_44k.wav", f, 1).feed(&mut p, usize::MAX);
        let ctl = RenderControl::default();
        ctl.eof.store(true, Ordering::Release);
        let mut api = FakeApi::new(B, 4, WaitMode::Scripted(Box::new(|_| {})));
        let mut prio = FakePriority {
            fail_register: Some(1552), // ERROR_INVALID_TASK_NAME 一类的 Win32 错误
            ..FakePriority::default()
        };
        let r = render(&mut api, &mut prio, &mut c, &ctl, cfg(B, &f));
        assert_eq!(
            r.mmcss,
            MmcssRecord {
                registered: Err(1552),
                reverted: None
            }
        );
        assert_eq!(prio.reverts.load(Ordering::SeqCst), 0);
        assert_eq!(r.end, RenderEnd::Drained);
    }
}
