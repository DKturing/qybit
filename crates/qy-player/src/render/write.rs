//! 短写提交的渲染循环（M2 T1，供 ALSA `writei` 一类后端使用）。
//!
//! 规则：
//! - 按后端报告的可写帧数，从队列借出连续的完整帧直接交给 `write`；后端只接受一部分
//!   （短写）时，只提交实际写出的帧，其余留在队列中下次继续，不重复、不丢弃。
//! - 启动前先写入（预填设备缓冲）；设备缓冲写满，或已确认 EOF 且队列已空时才 `start`。
//! - 不补静音：队列暂时为空时不写入。设备取空由后端报告为 xrun，记为欠载
//!   （写入序号、设备帧位置、后端报告的缺失帧），用 `recover` 恢复后重新预填、再启动；
//!   该段播放记录因此不能描述为完整保真。
//! - 已确认 EOF 且队列已空：`drain` 播完已写入的音频后结束。
//! - 暂停时（已启动）按可写空间写入静音、不从队列取数，单独计数，不记为欠载；尚未启动时
//!   暂停则不启动。已确认 EOF 且队列已空时照常排空。
//! - 收到停止请求立即 `stop`（丢弃设备中数据），不排空；API 错误立即结束并报告。
//!
//! 本循环只做有界搬运与计数：不解码、不 pack、不分配、不读文件、不等待 UI；
//! 无事可做时短暂休眠（[`WriteConfig::idle_sleep`]），不忙等。

use std::sync::atomic::Ordering;
use std::time::Duration;

use qy_pcm_format::ring::FrameConsumer;

use super::{
    ApiError, ApiOp, ErrorCode, RenderControl, RenderEnd, RenderReport, SILENCE, ThreadPriority,
    UnderrunRecord, WaitResult, with_priority,
};

/// 可写空间查询结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Avail {
    /// 当前可写入的帧数。
    Ready(u32),
    /// 设备已取空（xrun），需要 `recover`。
    Xrun { missing_frames: u32 },
}

/// 写入结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
    /// 实际写入的帧数（可少于所给帧数）。
    Frames(u32),
    /// 设备已取空（xrun），本次未写入。
    Xrun { missing_frames: u32 },
}

/// 短写后端的平台 API。
pub trait WriteApi {
    /// 开始播放已写入的数据。
    fn start(&mut self) -> Result<(), ApiError>;
    /// 立即停止，丢弃设备中未播放的数据。
    fn stop(&mut self) -> Result<(), ApiError>;
    /// 播完已写入的数据后停止（阻塞到播放完成）。
    fn drain(&mut self) -> Result<(), ApiError>;
    /// 已启动时等待设备可写。
    fn wait(&mut self, timeout_ms: u32) -> Result<WaitResult, ApiError>;
    fn avail(&mut self) -> Result<Avail, ApiError>;
    /// 写入完整帧字节（长度为 blockAlign 的整数倍），返回实际写入帧数。
    fn write(&mut self, frames: &[u8]) -> Result<Written, ApiError>;
    /// xrun 后恢复到可写入、未启动状态。
    fn recover(&mut self) -> Result<(), ApiError>;
}

/// 短写渲染参数。
#[derive(Debug, Clone, Copy)]
pub struct WriteConfig {
    pub block_align: usize,
    /// 设备周期帧数（水位统计的临界线为 2 个周期，R1）。
    pub period_frames: u32,
    pub event_timeout_ms: u32,
    /// 队列暂时为空、无事可做时的休眠间隔。
    pub idle_sleep: Duration,
}

struct Loop<'a, A: WriteApi> {
    api: &'a mut A,
    consumer: &'a mut FrameConsumer,
    ctl: &'a RenderControl,
    cfg: WriteConfig,
}

/// 一轮写入的结果。
struct Fill {
    frames: u64,
    /// 设备不再接受（写满可写空间或发生短写）。
    full: bool,
    xrun: Option<u32>,
}

/// 运行短写渲染循环直到排空、停止或出错。调用方在专用渲染线程中调用。
pub fn render<A: WriteApi, P: ThreadPriority>(
    api: &mut A,
    priority: &mut P,
    consumer: &mut FrameConsumer,
    ctl: &RenderControl,
    cfg: WriteConfig,
) -> RenderReport {
    with_priority(priority, ctl, |report| {
        let mut l = Loop {
            api,
            consumer,
            ctl,
            cfg,
        };
        match l.run(report) {
            Ok(end) => end,
            Err(e) => {
                let _ = l.api.stop();
                RenderEnd::Error(e)
            }
        }
    })
}

impl<A: WriteApi> Loop<'_, A> {
    fn run(&mut self, report: &mut RenderReport) -> Result<RenderEnd, ApiError> {
        report.watermark = qy_pcm_format::queue::Watermark::for_stream(
            self.ctl.prefill_frames.load(Ordering::Acquire),
            u64::from(self.cfg.period_frames),
        );
        let mut started = false;
        // 自上次准备（初始或 recover）以来写入的帧数。
        let mut since_prepare = 0u64;
        loop {
            if self.ctl.stop.load(Ordering::Acquire) {
                self.api.stop()?;
                return Ok(RenderEnd::Stopped);
            }
            // 先读 EOF 再取数据：读到 EOF 时该格式的全部数据已对本线程可见。
            let eof = self.ctl.eof.load(Ordering::Acquire);
            if started && eof && self.consumer.available_frames() == 0 {
                // 全部有效帧已写入：立即排空，不再等待设备（否则可能在排空前取空）。
                self.api.drain()?;
                return Ok(RenderEnd::Drained);
            }
            if self.ctl.pause.load(Ordering::Acquire) {
                if !started {
                    std::thread::sleep(self.cfg.idle_sleep);
                    continue;
                }
                self.wait()?;
                if let Some(missing) = self.silence(report)? {
                    self.xrun(report, missing)?;
                    started = false;
                    since_prepare = 0;
                }
                continue;
            }
            if started {
                match self.api.wait(self.cfg.event_timeout_ms)? {
                    WaitResult::Signaled => {}
                    WaitResult::Timeout => {
                        return Err(ApiError {
                            op: ApiOp::Wait,
                            code: ErrorCode::Timeout,
                        });
                    }
                }
            }
            let room = match self.api.avail()? {
                Avail::Ready(n) => n,
                Avail::Xrun { missing_frames } => {
                    self.xrun(report, missing_frames)?;
                    started = false;
                    since_prepare = 0;
                    continue;
                }
            };
            if started && !eof {
                report
                    .watermark
                    .observe(self.consumer.available_frames() as u64);
            }
            let fill = self.fill(report, room)?;
            if let Some(missing) = fill.xrun {
                self.xrun(report, missing)?;
                started = false;
                since_prepare = 0;
                continue;
            }
            since_prepare += fill.frames;
            let queue_empty = self.consumer.available_frames() == 0;
            if !started {
                if since_prepare > 0 && (fill.full || (eof && queue_empty)) {
                    self.api.start()?;
                    started = true;
                } else if since_prepare == 0 && eof && queue_empty {
                    // 本次准备之后没有任何数据要播。
                    return Ok(RenderEnd::Drained);
                } else if fill.frames == 0 {
                    std::thread::sleep(self.cfg.idle_sleep);
                }
                continue;
            }
            if fill.frames == 0 && !fill.full && !(eof && queue_empty) {
                std::thread::sleep(self.cfg.idle_sleep);
            }
        }
    }

    /// 已启动时等待设备可写；超时为错误。
    fn wait(&mut self) -> Result<(), ApiError> {
        match self.api.wait(self.cfg.event_timeout_ms)? {
            WaitResult::Signaled => Ok(()),
            WaitResult::Timeout => Err(ApiError {
                op: ApiOp::Wait,
                code: ErrorCode::Timeout,
            }),
        }
    }

    /// 暂停：在可写空间内写入静音（不取队列）。设备已取空时返回缺失帧数。
    fn silence(&mut self, report: &mut RenderReport) -> Result<Option<u32>, ApiError> {
        let ba = self.cfg.block_align.max(1);
        let room = match self.api.avail()? {
            Avail::Ready(n) => n,
            Avail::Xrun { missing_frames } => return Ok(Some(missing_frames)),
        };
        let frames = (room as usize).min(SILENCE.len() / ba);
        if frames == 0 {
            std::thread::sleep(self.cfg.idle_sleep);
            return Ok(None);
        }
        match self.api.write(&SILENCE[..frames * ba])? {
            Written::Frames(n) => {
                report.paused(self.ctl, u64::from(n));
                Ok(None)
            }
            Written::Xrun { missing_frames } => Ok(Some(missing_frames)),
        }
    }

    /// 在 `room` 帧可写空间内尽量写入队列中的完整帧。
    fn fill(&mut self, report: &mut RenderReport, room: u32) -> Result<Fill, ApiError> {
        let ba = self.cfg.block_align;
        let mut out = Fill {
            frames: 0,
            full: room == 0,
            xrun: None,
        };
        let mut left = room as usize;
        while left > 0 {
            let api = &mut *self.api;
            let mut outcome = None;
            let committed = self
                .consumer
                .consume_contiguous(left, |bytes| {
                    let given = bytes.len() / ba.max(1);
                    let r = api.write(bytes);
                    let done = match r {
                        Ok(Written::Frames(k)) => (k as usize).min(given),
                        _ => 0,
                    };
                    outcome = Some((r, given));
                    done
                })
                .unwrap_or(0);
            let Some((result, given)) = outcome else {
                break; // 队列已空
            };
            report.packets += 1;
            match result? {
                Written::Frames(_) => {
                    report.valid(self.ctl, committed as u64);
                    out.frames += committed as u64;
                    left -= committed;
                    if committed < given {
                        out.full = true;
                        break;
                    }
                }
                Written::Xrun { missing_frames } => {
                    out.xrun = Some(missing_frames);
                    break;
                }
            }
        }
        if left == 0 {
            out.full = true;
        }
        if out.frames > 0 {
            self.ctl.space_freed(self.consumer.available_frames());
        }
        Ok(out)
    }

    fn xrun(&mut self, report: &mut RenderReport, missing_frames: u32) -> Result<(), ApiError> {
        let record = UnderrunRecord {
            packet: report.packets,
            device_frame: report.valid_frames,
            missing_frames,
        };
        report.underrun(self.ctl, record);
        self.api.recover()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use qy_core::DeviceFormat;
    use qy_pcm_format::ring::{FrameProducer, frame_ring, reconfigure};

    use super::*;
    use crate::render::PriorityRecord;
    use crate::testing::{
        DEVICE_GONE, FakePriority, FakeWriteApi, Feeder, WCall, WriteWait, check_write_calls,
        data_chunk, fixture,
    };

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

    fn cfg(f: &DeviceFormat) -> WriteConfig {
        WriteConfig {
            period_frames: f.sample_rate() / 100,
            block_align: usize::from(f.block_align()),
            event_timeout_ms: 2000,
            idle_sleep: Duration::from_micros(50),
        }
    }

    /// 随机短写（含接受 0 帧）：只提交实际写出的帧，设备播放的字节与源逐字节一致、
    /// 无重复无丢失、无填充。
    #[test]
    fn short_writes_commit_only_written_frames() {
        let f = fmt(44_100, 2, 16, 16);
        let raw = data_chunk(&fixture("pcm16_stereo_44k.wav"));
        let expected: Vec<u8> = raw.iter().copied().cycle().take(raw.len() * 5).collect();
        let (mut partial, mut zero) = (0, 0);
        for seed in 0..20 {
            let (mut p, mut c) = ring(&f, 768_000);
            Feeder::new("pcm16_stereo_44k.wav", f, 5).feed(&mut p, usize::MAX);
            let ctl = Arc::new(RenderControl::default());
            ctl.pushed_frames
                .store(expected.len() as u64 / 4, Ordering::Release);
            ctl.eof.store(true, Ordering::Release);
            let mut api = FakeWriteApi::new(256, 4, seed, WriteWait::Patient(Arc::clone(&ctl)));
            api.short_writes = true;
            let mut prio = FakePriority::default();
            let r = render(&mut api, &mut prio, &mut c, &ctl, cfg(&f));
            assert_eq!(r.end, RenderEnd::Drained, "seed {seed}");
            assert!(api.rendered == expected, "seed {seed}: 播放字节不一致");
            assert_eq!(r.valid_frames, expected.len() as u64 / 4);
            assert_eq!(
                (r.underrun_events, r.tail_fill_frames, r.drain_frames),
                (0, 0, 0)
            );
            check_write_calls(&api.calls);
            assert_eq!(api.calls.last(), Some(&WCall::Drain));
            assert_eq!(
                r.packets,
                api.calls
                    .iter()
                    .filter(|c| matches!(c, WCall::Write(..)))
                    .count() as u64
            );
            for c in &api.calls {
                match *c {
                    WCall::Write(given, Some(0)) if given > 0 => zero += 1,
                    WCall::Write(given, Some(k)) if k < given => partial += 1,
                    _ => {}
                }
            }
            assert_eq!(
                r.priority,
                PriorityRecord {
                    label: "测试优先级",
                    registered: Ok(()),
                    reverted: Some(Ok(()))
                }
            );
        }
        assert!(partial > 0 && zero > 0, "部分 {partial} / 零 {zero} 未覆盖");
    }

    /// 生产端停顿使设备取空：记为欠载（写入序号、设备帧位置、缺失帧），recover 后重新预填
    /// 并启动；有效字节仍逐字节完整、无重复。
    #[test]
    fn xrun_is_recorded_recovered_and_restarted() {
        let f = fmt(8_000, 1, 16, 16);
        let play = 80u32;
        let (p, mut c) = ring(&f, 8_000);
        let feeder = Arc::new(Mutex::new((
            Feeder::new("pcm16_mono_8k_fmt18.wav", f, 40),
            p,
        )));
        let ctl = Arc::new(RenderControl::default());
        if let Ok(mut g) = feeder.lock() {
            let (fd, p) = &mut *g;
            fd.feed(p, 3 * play as usize);
        }
        let (hook_ctl, hook_feeder) = (Arc::clone(&ctl), Arc::clone(&feeder));
        // 每次 avail 补 80 帧，但第 20–39 次不补（生产端停顿）。
        let hook = move |i: u64| {
            if let Ok(mut g) = hook_feeder.lock() {
                let (fd, p) = &mut *g;
                if !(20..40).contains(&i) {
                    fd.feed(p, play as usize);
                }
                if fd.exhausted() {
                    hook_ctl.eof.store(true, Ordering::Release);
                }
            }
        };
        let mut api = FakeWriteApi::new(
            2 * play,
            2,
            7,
            WriteWait::Scripted {
                hook: Box::new(hook),
                play,
            },
        );
        let mut prio = FakePriority::default();
        let r = render(&mut api, &mut prio, &mut c, &ctl, cfg(&f));
        assert_eq!(r.end, RenderEnd::Drained);
        check_write_calls(&api.calls);
        let raw = data_chunk(&fixture("pcm16_mono_8k_fmt18.wav"));
        let expected: Vec<u8> = raw.iter().copied().cycle().take(raw.len() * 40).collect();
        assert!(api.rendered == expected, "有效字节须完整且无重复");
        assert_eq!(r.valid_frames as usize, expected.len() / 2);
        assert!(r.underrun_events >= 1, "{r:?}");
        let recs: Vec<_> = r.underrun_records().copied().collect();
        assert_eq!(recs.len() as u64, r.underrun_events);
        for u in &recs {
            assert!(u.missing_frames > 0 && u.missing_frames <= play, "{u:?}");
            assert!(u.device_frame <= r.valid_frames);
        }
        assert_eq!(
            r.underrun_frames,
            recs.iter()
                .map(|u| u64::from(u.missing_frames))
                .sum::<u64>()
        );
        assert_eq!(
            api.calls.iter().filter(|c| **c == WCall::Recover).count() as u64,
            r.underrun_events
        );
        assert_eq!(
            api.calls.iter().filter(|c| **c == WCall::Start).count() as u64,
            r.underrun_events + 1,
            "每次恢复后重新启动一次"
        );
        assert_eq!(r.tail_fill_frames, 0);
    }

    #[test]
    fn stop_error_timeout_and_empty_stream() {
        let f = fmt(44_100, 2, 16, 16);
        // 停止：立即 Stop，不排空。
        let (mut p, mut c) = ring(&f, 768_000);
        Feeder::new("pcm16_stereo_44k.wav", f, 3).feed(&mut p, usize::MAX);
        let ctl = Arc::new(RenderControl::default());
        let c2 = Arc::clone(&ctl);
        let mut api = FakeWriteApi::new(
            256,
            4,
            1,
            WriteWait::Scripted {
                hook: Box::new(move |i| {
                    if i == 2 {
                        c2.stop.store(true, Ordering::Release);
                    }
                }),
                play: 64,
            },
        );
        let mut prio = FakePriority::default();
        let r = render(&mut api, &mut prio, &mut c, &ctl, cfg(&f));
        assert_eq!(r.end, RenderEnd::Stopped);
        assert_eq!(api.calls.last(), Some(&WCall::Stop));
        assert!(!api.calls.contains(&WCall::Drain));
        assert_eq!(r.priority.reverted, Some(Ok(())));
        check_write_calls(&api.calls);

        // 写入错误与等待超时：报告错误、尝试停止、撤销优先级。
        for case in 0..2 {
            let (mut p, mut c) = ring(&f, 768_000);
            Feeder::new("pcm16_stereo_44k.wav", f, 10).feed(&mut p, usize::MAX);
            let ctl = Arc::new(RenderControl::default());
            let mut api = FakeWriteApi::new(256, 4, 2, WriteWait::Patient(Arc::clone(&ctl)));
            let want = if case == 0 {
                api.fail_write_at = Some(3);
                ApiError {
                    op: ApiOp::Write,
                    code: DEVICE_GONE,
                }
            } else {
                api.timeout_at_wait = Some(1);
                ApiError {
                    op: ApiOp::Wait,
                    code: ErrorCode::Timeout,
                }
            };
            let mut prio = FakePriority::default();
            let r = render(&mut api, &mut prio, &mut c, &ctl, cfg(&f));
            assert_eq!(r.end, RenderEnd::Error(want));
            assert_eq!(api.calls.last(), Some(&WCall::Stop), "出错后须尝试停止");
            assert_eq!(r.priority.reverted, Some(Ok(())));
        }

        // 无任何数据且已 EOF：不启动设备，直接结束。
        let (_p, mut c) = ring(&f, 768_000);
        let ctl = Arc::new(RenderControl::default());
        ctl.eof.store(true, Ordering::Release);
        let mut api = FakeWriteApi::new(256, 4, 3, WriteWait::Patient(Arc::clone(&ctl)));
        let mut prio = FakePriority::default();
        let r = render(&mut api, &mut prio, &mut c, &ctl, cfg(&f));
        assert_eq!(r.end, RenderEnd::Drained);
        assert_eq!(r.valid_frames, 0);
        assert!(!api.calls.contains(&WCall::Start));
    }

    /// 暂停（GUI T2）：已启动时按可写空间写静音、不取队列、单独计数，不记为欠载；恢复后有效
    /// 字节从暂停处继续，逐字节一致，最后照常排空。
    #[test]
    fn pause_writes_silence_without_consuming_queue() {
        let f = fmt(8_000, 1, 16, 16);
        let play = 80u32;
        let (mut p, mut c) = ring(&f, 64_000);
        let mut feeder = Feeder::new("pcm16_mono_8k_fmt18.wav", f, 20);
        feeder.feed(&mut p, usize::MAX);
        let expected = {
            let raw = data_chunk(&fixture("pcm16_mono_8k_fmt18.wav"));
            raw.repeat(20)
        };
        let ctl = Arc::new(RenderControl::default());
        ctl.eof.store(true, Ordering::Release);
        let hook_ctl = Arc::clone(&ctl);
        // 第 5–14 次查询可写空间时处于暂停。
        let hook = move |i: u64| {
            hook_ctl
                .pause
                .store((5..15).contains(&i), Ordering::Release)
        };
        let mut api = FakeWriteApi::new(
            256,
            2,
            7,
            WriteWait::Scripted {
                hook: Box::new(hook),
                play,
            },
        );
        let mut prio = FakePriority::default();
        let r = render(&mut api, &mut prio, &mut c, &ctl, cfg(&f));
        assert_eq!(r.end, RenderEnd::Drained);
        assert_eq!(r.underrun_events, 0, "{r:?}");
        assert!(r.paused_frames > 0);
        assert_eq!(ctl.paused_frames.load(Ordering::Acquire), r.paused_frames);
        assert_eq!(r.valid_frames, expected.len() as u64 / 2);
        assert_eq!(
            api.rendered.len() as u64,
            (r.valid_frames + r.paused_frames) * 2
        );
        // 去掉插入的静音后与源一致：源是插入零字节后的子序列，且插入的字节数等于暂停帧。
        let (mut j, mut inserted) = (0usize, 0u64);
        for &b in &api.rendered {
            if j < expected.len() && b == expected[j] {
                j += 1;
            } else {
                assert_eq!(b, 0, "插入的字节须为静音");
                inserted += 1;
            }
        }
        assert_eq!(j, expected.len(), "有效字节不完整");
        assert_eq!(inserted, r.paused_frames * 2);
        check_write_calls(&api.calls);
    }
}
