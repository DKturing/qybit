//! 生产端播放驱动（M0 T5 起，M2 T1 移入平台无关层，M3 T1 起由 `qy_core::engine` 调度），
//! 对输出后端泛型。
//!
//! 在调用线程（非实时）运行引擎：真实 `qy-core`、自有解码（按文件头选 WAV/FLAC）与 pack 都在
//! 引擎内执行，本模块只执行引擎的 [`HostAction`]：同步读文件、经后端打开设备、启动/排空
//! 渲染线程，并以 [`HostEvent`] 回报。播放规则由引擎负责，与模拟宿主、MCU 宿主一致：
//! - 设备启动前预填当前采样率下至少 500 ms，或在播放结束/失败时直接启动排空；
//! - 新源格式能被当前设备格式无损承载（采样率、声道相同且 N ≤ V）时继续使用
//!   （[`ReusePolicy::Accepts`](qy_core::engine::ReusePolicy::Accepts)），否则先排空当前流
//!   （置 EOF、等渲染线程播完退出），再按新格式打开；
//! - 超出配置档能力的源在打开设备前拒绝，并给出具体原因；
//! - 打开失败（设备不支持、独占失败等）回报 `DeviceOpenFailed`，不做任何回退；
//! - 播放失败时排空已入队的有效音频后停止；渲染线程出错（设备失效等）时停止核心。
//!
//! 桌面渲染循环尚未实现暂停；本驱动不发起暂停，收到暂停动作按协议错误记录。

use std::fs::File;
use std::io::{self, ErrorKind, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use qy_core::engine::{HostAction, HostEvent, Poll, Wait};
use qy_core::{DeviceFormat, Generation, SourceFormat};

use crate::HostResources;
use crate::backend::{Backend, PendingStream, RunningStream};
use crate::render::{RenderControl, RenderEnd};
use crate::report::{Negotiation, PlaybackReport, StreamReport, Submission};

enum Stream<B: Backend> {
    Pending(B::Pending),
    Running(<B::Pending as PendingStream>::Running),
}

struct Active<B: Backend> {
    stream: Stream<B>,
    format: DeviceFormat,
    submission: Submission,
    negotiation: Negotiation<B::Attempt>,
    /// 打开本流时的播放计划（R1）。
    plan: Option<qy_core::engine::PlaybackPlan>,
    ctl: Arc<RenderControl>,
    /// 打开本流时引擎的累计入队帧数。
    pushed_base: u64,
}

/// 曲目来源：按编号打开曲目，得到同步读取器。桌面播放用文件路径（[`play`]）；测试与基准可
/// 提供内存数据或注入读取延迟的来源（[`play_source`]）。
pub trait TrackSource {
    type Reader: Read;
    /// 曲目数。
    fn count(&self) -> u32;
    /// 打开第 `track` 首。
    fn open(&mut self, track: u32) -> io::Result<Self::Reader>;
}

/// 文件路径列表。
struct Files<'a>(&'a [PathBuf]);

impl TrackSource for Files<'_> {
    type Reader = File;

    fn count(&self) -> u32 {
        u32::try_from(self.0.len()).unwrap_or(u32::MAX)
    }

    fn open(&mut self, track: u32) -> io::Result<File> {
        match self.0.get(track as usize) {
            Some(p) => File::open(p),
            None => Err(io::Error::new(ErrorKind::NotFound, "曲目编号越界")),
        }
    }
}

/// 生产端等待队列空间时的休眠间隔。
const FULL_QUEUE_SLEEP: Duration = Duration::from_millis(2);

struct Player<'a, B: Backend, S: TrackSource> {
    backend: &'a mut B,
    res: &'a mut HostResources,
    source: &'a mut S,
    file: Option<S::Reader>,
    active: Option<Active<B>>,
    report: PlaybackReport<B::Attempt>,
}

/// 依次播放 `files`，直到完成、失败或设备出错。
pub fn play<B: Backend>(
    backend: &mut B,
    res: &mut HostResources,
    files: &[PathBuf],
) -> PlaybackReport<B::Attempt> {
    play_source(backend, res, &mut Files(files))
}

/// 依次播放 `source` 中的曲目，直到完成、失败或设备出错。
pub fn play_source<B: Backend, S: TrackSource>(
    backend: &mut B,
    res: &mut HostResources,
    source: &mut S,
) -> PlaybackReport<B::Attempt> {
    let count = source.count();
    // 清理上一次播放可能残留的状态（旧代次），丢弃其控制动作。
    res.engine.stop();
    while res.engine.take_control().is_some() {}
    res.engine.play(0, count);
    let mut p = Player {
        backend,
        res,
        source,
        file: None,
        active: None,
        report: PlaybackReport::default(),
    };
    p.run();
    p.report
}

impl<B: Backend, S: TrackSource> Player<'_, B, S> {
    fn handle(&mut self, event: HostEvent) {
        if self.res.engine.handle(event).is_err() {
            self.report.protocol_errors += 1;
        }
    }

    fn run(&mut self) {
        loop {
            let poll = self.res.engine.poll();
            self.sync_pushed();
            match poll {
                Poll::Action(a) => {
                    if !self.execute(a) {
                        break;
                    }
                }
                Poll::Yield => {}
                Poll::Wait(Wait::QueueFull) => {
                    if !self.wait_for_space() {
                        // 渲染线程已出错结束：停止核心，旧代次回报此后一律拒绝。
                        self.res.engine.device_lost();
                        while let Some(a) = self.res.engine.take_control() {
                            self.execute(a);
                        }
                        break;
                    }
                }
                Poll::Wait(w) => {
                    // 本宿主同步回报所有读取与设备事件，不应等待。
                    self.report.protocol_errors += 1;
                    self.report.messages.push(format!("引擎意外等待：{w:?}"));
                    break;
                }
                Poll::Idle => break,
            }
        }
        // 未正常排空的流（设备错误路径）在此回收。
        self.finish_stream(false);
        self.report.protocol_errors += self.res.engine.stats().protocol_errors;
    }

    /// 把当前流的入队帧数同步给渲染线程。
    fn sync_pushed(&self) {
        if let Some(a) = &self.active {
            let total = self.res.engine.stats().frames_pushed;
            a.ctl
                .pushed_frames
                .store(total - a.pushed_base, Ordering::Release);
        }
    }

    /// 执行引擎动作；返回 false 表示播放结束。
    fn execute(&mut self, action: HostAction) -> bool {
        match action {
            HostAction::Open { generation, track } => match self.source.open(track) {
                Ok(f) => {
                    self.file = Some(f);
                    self.handle(HostEvent::Opened { generation });
                }
                Err(e) => {
                    self.report
                        .messages
                        .push(format!("打开曲目 {track} 失败: {e}"));
                    self.handle(HostEvent::OpenFailed { generation });
                }
            },
            HostAction::Read {
                generation,
                request,
                ..
            } => self.read(generation, request),
            HostAction::Close { .. } => self.file = None,
            HostAction::SelectDevice { .. } => {
                // 复用策略为 Accepts，引擎不会请求选择设备格式。
                self.report.protocol_errors += 1;
                self.handle(HostEvent::DeviceUnsupported);
            }
            HostAction::OpenDevice { source, .. } => {
                let event = match self.open_device(&source) {
                    Some(format) => HostEvent::DeviceOpened { format },
                    None => HostEvent::DeviceOpenFailed,
                };
                self.handle(event);
                // 设备已确定：记录本流的播放计划（R1）。
                let plan = self.res.engine.plan();
                if let Some(a) = self.active.as_mut() {
                    a.plan = plan;
                }
            }
            HostAction::StartDevice => self.start_if_pending(),
            HostAction::DrainDevice => {
                self.finish_stream(false);
                self.handle(HostEvent::DeviceDrained);
            }
            HostAction::StopDevice => self.finish_stream(true),
            HostAction::PauseDevice | HostAction::ResumeDevice => {
                self.report.protocol_errors += 1;
                self.report.messages.push("桌面渲染循环尚未实现暂停".into());
            }
            HostAction::TrackSkipped { track, fault } => self.report.skipped.push((track, fault)),
            HostAction::Finished => {
                self.report.finished = !self.report.device_error;
                return false;
            }
            HostAction::Failed { track, fault } => {
                self.report.fault = Some((track, fault));
                if let Some(e) = self.res.engine.decoder().flac_error() {
                    self.report.messages.push(format!("FLAC 错误明细：{e:?}"));
                }
                if let Some(d) = self.res.engine.decoder().wav_detail() {
                    self.report.messages.push(format!("WAV 错误明细：{d}"));
                }
                return false;
            }
        }
        true
    }

    fn read(&mut self, generation: Generation, request: u32) {
        let result = match (
            self.file.as_mut(),
            self.res.engine.read_target(generation, request),
        ) {
            (Some(file), Some(dst)) => loop {
                match file.read(dst) {
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    r => break r,
                }
            },
            _ => {
                self.report.protocol_errors += 1;
                Err(std::io::Error::other("无读取目标"))
            }
        };
        let event = match result {
            // 文件读取返回 0 表示到达结尾。
            Ok(0) => HostEvent::ReadEof {
                generation,
                request,
            },
            Ok(len) => HostEvent::ReadDone {
                generation,
                request,
                len,
            },
            Err(e) => {
                self.report.messages.push(format!("读取失败: {e}"));
                HostEvent::ReadFailed {
                    generation,
                    request,
                }
            }
        };
        self.handle(event);
    }

    /// 打开设备并按协商格式重配置队列；返回设备格式。引擎保证此时没有活动流。
    fn open_device(&mut self, source: &SourceFormat) -> Option<DeviceFormat> {
        if self.report.device_error {
            return None;
        }
        if self.active.is_some() {
            self.report.protocol_errors += 1;
            self.finish_stream(false);
        }
        match self.backend.open(source) {
            Ok(pending) => {
                let format = pending.format();
                if let Err(e) = self.res.configure_ring(&format) {
                    self.report.messages.push(format!("队列重配置失败: {e:?}"));
                    return None;
                }
                self.active = Some(Active {
                    negotiation: pending.negotiation().clone(),
                    submission: pending.submission(),
                    stream: Stream::Pending(pending),
                    format,
                    plan: None,
                    ctl: {
                        let ctl = RenderControl::default();
                        let prefill = self
                            .res
                            .engine
                            .config()
                            .caps
                            .prefill_frames(format.sample_rate());
                        ctl.prefill_frames
                            .store(prefill, std::sync::atomic::Ordering::Release);
                        Arc::new(ctl)
                    },
                    pushed_base: self.res.engine.stats().frames_pushed,
                });
                Some(format)
            }
            Err(f) => {
                self.report.open_failures.push(f);
                None
            }
        }
    }

    /// 队列满：启动尚未开始的流，或休眠等待渲染线程取走数据。
    /// 渲染线程已结束（出错）时返回 false。
    fn wait_for_space(&mut self) -> bool {
        self.start_if_pending();
        match self.active.as_ref().map(|a| &a.stream) {
            Some(Stream::Running(r)) if r.is_finished() => {
                self.report.device_error = true;
                false
            }
            _ => {
                std::thread::sleep(FULL_QUEUE_SLEEP);
                true
            }
        }
    }

    fn start_if_pending(&mut self) {
        let Some(active) = self.active.take() else {
            return;
        };
        let stream = match active.stream {
            Stream::Pending(p) => match self.res.consumer.take() {
                Some(consumer) => Stream::Running(p.start(consumer, Arc::clone(&active.ctl))),
                None => {
                    self.report.protocol_errors += 1;
                    Stream::Pending(p)
                }
            },
            running => running,
        };
        self.active = Some(Active { stream, ..active });
    }

    /// 结束当前流并归还消费端、记录报告：`stop` 为 false 时置 EOF 等它播完退出（排空），
    /// 为 true 时立即停止（停止/切歌，丢弃队列中的数据）。
    fn finish_stream(&mut self, stop: bool) {
        let Some(active) = self.active.take() else {
            return;
        };
        active.ctl.eof.store(true, Ordering::Release);
        if stop || self.report.device_error {
            active.ctl.stop.store(true, Ordering::Release);
        }
        let running = match active.stream {
            Stream::Pending(p) => match self.res.consumer.take() {
                Some(consumer) => p.start(consumer, Arc::clone(&active.ctl)),
                None => {
                    self.report.protocol_errors += 1;
                    return;
                }
            },
            Stream::Running(r) => r,
        };
        let (consumer, render) = running.join();
        if consumer.is_none() {
            self.report.messages.push("渲染线程异常结束".into());
            self.report.device_error = true;
        }
        self.res.consumer = consumer;
        if let Some(r) = &render
            && matches!(r.end, RenderEnd::Error(_))
        {
            self.report.device_error = true;
        }
        self.report.streams.push(StreamReport {
            negotiation: active.negotiation,
            plan: active.plan,
            format: active.format,
            submission: active.submission,
            pushed_frames: self.res.engine.stats().frames_pushed - active.pushed_base,
            render,
            evidence: Default::default(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{
        FakeBackend, FakeDevice, FakeOutput, TestAttempt, WCall, check_calls, check_write_calls,
        data_chunk, fixture, fixture_path, flac_fixture_path, flac_reference, place,
    };
    use qy_core::engine::EngineFault;
    use qy_core::{DecodeError, Fault};

    const ALL: [(u8, u8); 4] = [(16, 16), (24, 24), (32, 24), (32, 32)];
    /// 整包设备与两个种子的短写设备：同一组断言对两种提交语义都必须成立。
    const DEVICES: [FakeDevice; 3] = [
        FakeDevice::Packet,
        FakeDevice::ShortWrite { seed: 1 },
        FakeDevice::ShortWrite { seed: 2 },
    ];

    fn backend(supported: &[(u8, u8)], device: FakeDevice) -> FakeBackend {
        let mut b = FakeBackend::new(supported);
        b.device = device;
        b
    }

    fn run(
        backend: &mut FakeBackend,
        names: &[&str],
    ) -> (PlaybackReport<TestAttempt>, Vec<FakeOutput>) {
        let Ok(mut res) = HostResources::new() else {
            panic!("res");
        };
        let files: Vec<PathBuf> = names
            .iter()
            .map(|n| {
                if n.ends_with(".flac") {
                    flac_fixture_path(n)
                } else {
                    fixture_path(n)
                }
            })
            .collect();
        let report = play(backend, &mut res, &files);
        assert!(res.consumer.is_some(), "消费端须归还");
        (report, backend.outputs())
    }

    /// 有效帧部分必须等于期望。整包设备：其后（尾部填充 + 排空）全为静音；
    /// 短写设备：设备只播放真实写入的字节，不得有任何填充。
    fn check_output(o: &FakeOutput, expected: &[u8]) {
        let ba = usize::from(o.format.block_align());
        let valid = o.report.valid_frames as usize * ba;
        assert_eq!(valid, expected.len(), "有效帧数");
        assert!(o.rendered[..valid] == *expected, "有效字节不一致");
        assert_eq!(o.report.underrun_events, 0, "{:?}", o.report);
        assert_eq!(o.report.priority.reverted, Some(Ok(())));
        if o.write_calls.is_empty() {
            assert!(o.rendered[valid..].iter().all(|&b| b == 0), "填充须为静音");
            check_calls(&o.calls, o.format.sample_rate() / 100);
        } else {
            assert!(o.calls.is_empty());
            assert_eq!(o.rendered.len(), valid, "短写设备不得补静音");
            assert_eq!(o.report.tail_fill_frames + o.report.drain_frames, 0);
            check_write_calls(&o.write_calls);
            assert_eq!(o.write_calls.last(), Some(&WCall::Drain));
        }
    }

    fn short_writes(outs: &[FakeOutput]) -> usize {
        outs.iter()
            .flat_map(|o| &o.write_calls)
            .filter(|c| matches!(c, WCall::Write(given, Some(k)) if k < given))
            .count()
    }

    #[test]
    fn same_format_tracks_share_one_stream_gaplessly() {
        for device in DEVICES {
            let mut b = backend(&ALL, device);
            let (r, outs) = run(&mut b, &["pcm16_stereo_44k.wav", "pcm16_stereo_44k.wav"]);
            assert!(r.finished && r.software_path_intact(), "{device:?} {r:?}");
            assert_eq!(outs.len(), 1);
            let raw = data_chunk(&fixture("pcm16_stereo_44k.wav"));
            check_output(&outs[0], &[raw.clone(), raw].concat());
            assert_eq!(r.streams[0].render.map(|x| x.end), Some(RenderEnd::Drained));
            if device != FakeDevice::Packet {
                assert!(short_writes(&outs) > 0, "短写未被覆盖");
            }
        }
    }

    #[test]
    fn format_change_drains_then_reopens() {
        let names = [
            "pcm16_stereo_44k.wav",
            "pcm24_stereo_large.wav",
            "ext20in24_mono_center.wav",
            "ext24in32_stereo_192k.wav",
        ];
        // 20 位源选 V=24 的最小容器 24/24；32/24 源选 24/24 容器（V 相同取小容器），
        // 设备字节与 24 位容器文件不同，因此按独立公式计算期望。
        let expect = [
            data_chunk(&fixture(names[0])),
            data_chunk(&fixture(names[1])),
            data_chunk(&fixture(names[2])),
            data_chunk(&fixture(names[3]))
                .chunks_exact(4)
                .flat_map(|c| [c[1], c[2], c[3]])
                .collect(),
        ];
        let fmts = [
            (44_100, 2, 16, 16),
            (88_200, 2, 24, 24),
            (96_000, 1, 24, 24),
            (192_000, 2, 24, 24),
        ];
        for device in DEVICES {
            let mut b = backend(&ALL, device);
            let (r, outs) = run(&mut b, &names);
            assert!(r.software_path_intact(), "{device:?} {r:?}");
            assert_eq!(outs.len(), 4);
            for ((o, e), (rate, ch, c, v)) in outs.iter().zip(&expect).zip(fmts) {
                assert_eq!(
                    (
                        o.format.sample_rate(),
                        o.format.channels(),
                        o.format.container_bits(),
                        o.format.valid_bits()
                    ),
                    (rate, ch, c, v)
                );
                check_output(o, e);
            }
            assert_eq!(
                r.streams
                    .iter()
                    .map(|s| s.negotiation.chosen)
                    .collect::<Vec<_>>(),
                vec![Some(0); 4]
            );
        }
    }

    #[test]
    fn unsupported_format_fails_without_fallback() {
        // 设备只支持 16/16 与 32/24：32 位源无可无损承载的格式 → 配置失败，不降精度。
        for device in DEVICES {
            let mut b = backend(&[(16, 16), (32, 24)], device);
            let (r, outs) = run(
                &mut b,
                &[
                    "pcm16_stereo_44k.wav",
                    "pcm32_stereo_96k.wav",
                    "pcm16_stereo_44k.wav",
                ],
            );
            assert_eq!(r.fault, Some((1, EngineFault::Core(Fault::Configure))));
            assert!(!r.finished && !r.software_path_intact());
            assert_eq!(r.open_failures.len(), 1);
            let n = &r.open_failures[0].negotiation;
            assert_eq!(n.chosen, None);
            assert_eq!(n.attempts.len(), 1);
            assert!(!n.attempts[0].accepted);
            assert_eq!(n.attempts[0].format.valid_bits(), 32);
            // 失败前的曲目已完整排空。
            assert_eq!(outs.len(), 1);
            check_output(&outs[0], &data_chunk(&fixture("pcm16_stereo_44k.wav")));
        }
    }

    #[test]
    fn input_faults_drain_queued_audio() {
        for device in DEVICES {
            let mut b = backend(&ALL, device);
            let (r, outs) = run(&mut b, &["pcm16_stereo_44k.wav", "does_not_exist.wav"]);
            assert_eq!(r.fault, Some((1, EngineFault::Core(Fault::Open))));
            assert_eq!(outs.len(), 1);
            check_output(&outs[0], &data_chunk(&fixture("pcm16_stereo_44k.wav")));

            let mut b = backend(&ALL, device);
            let (r, outs) = run(&mut b, &["trunc_mid_frame.wav"]);
            assert_eq!(
                r.fault,
                Some((0, EngineFault::Core(Fault::Decode(DecodeError::Truncated))))
            );
            // 截断前 50 个完整帧全部播放。
            let wav = fixture("trunc_mid_frame.wav");
            check_output(&outs[0], &wav[44..44 + 50 * 4]);
        }
    }

    /// R1：每个输出流报告播放计划与队列水位（两种渲染循环）。
    #[test]
    fn summary_reports_plan_and_watermark() {
        for device in [FakeDevice::Packet, FakeDevice::ShortWrite { seed: 5 }] {
            let mut b = backend(&ALL, device);
            let (r, _) = run(&mut b, &["pcm24_stereo_large.wav"]);
            let s = r.summary();
            assert!(s.contains("播放计划：源 "), "{s}");
            assert!(s.contains("队列水位："), "{s}");
            let Some(render) = r.streams.first().and_then(|st| st.render.as_ref()) else {
                panic!("缺少渲染报告：{s}");
            };
            let w = render.watermark;
            assert!(w.low_frames() > 0 && w.critical_frames() > 0, "{w:?}");
            assert_eq!(r.exit_code(), 0, "{s}");
        }
    }

    #[test]
    fn summary_reports_four_categories_and_conclusion() {
        let mut b = backend(&ALL, FakeDevice::Packet);
        let (r, _) = run(&mut b, &["ext20in24_mono_center.wav"]);
        let s = r.summary();
        assert!(s.contains("源格式：96000 Hz / 1 声道 / 20 位有效"), "{s}");
        // 请求格式与协商结果。
        assert!(
            s.contains(
                "请求 #0：96000 Hz / 1 声道 / 容器 24 位 / 有效 24 位（假设备）接受 ← 已协商"
            ),
            "{s}"
        );
        // 可查询的已应用参数与回录证据分别列出。
        assert!(
            s.contains("已应用参数：格式 96000 Hz / 1 声道 / 容器 24 位 / 有效 24 位，缓冲 960 帧（假设备报告）"),
            "{s}"
        );
        assert!(s.contains("回录证据：无（未回录）"), "{s}");
        assert!(s.contains("提交方式：整包，每包 960 帧"), "{s}");
        assert!(s.contains("测试优先级：注册成功，已撤销"), "{s}");
        assert!(s.contains("欠载 0 次 / 0 帧，已排空"), "{s}");
        assert!(s.contains("不证明设备端 bit-perfect"));
        assert_eq!(r.exit_code(), 0);

        let mut b = backend(&ALL, FakeDevice::ShortWrite { seed: 3 });
        let (r, _) = run(&mut b, &["ext20in24_mono_center.wav"]);
        let s = r.summary();
        assert!(
            s.contains("提交方式：短写（只提交实际写出的帧，其余保留）"),
            "{s}"
        );
        assert!(
            s.contains("尾部填充 0 帧，排空静音 0 帧，欠载 0 次 / 0 帧，已排空"),
            "{s}"
        );
        assert_eq!(r.exit_code(), 0);

        let mut b = backend(&[(16, 16)], FakeDevice::Packet);
        let (r, _) = run(&mut b, &["pcm24_stereo_large.wav"]);
        let s = r.summary();
        assert!(s.contains("打开失败：设备不支持该格式"), "{s}");
        assert!(s.contains("（假设备）不支持"), "{s}");
        assert!(
            !s.contains("已应用参数"),
            "失败的协商不得列出已应用参数：{s}"
        );
        assert!(s.contains("曲目 1 失败：Configure"), "{s}");
        assert!(s.contains("结论：播放未完整完成。"));
        assert!(!s.contains("无欠载、无错误"));
        assert_eq!(r.exit_code(), 1);
    }

    #[test]
    fn device_invalidated_stops_playback() {
        for device in DEVICES {
            let mut b = backend(&ALL, device);
            b.invalidate = Some((0, 5));
            let (r, outs) = run(
                &mut b,
                &["pcm24_stereo_large.wav", "pcm24_stereo_large.wav"],
            );
            assert!(r.device_error, "{device:?} {r:?}");
            assert!(!r.finished && !r.software_path_intact());
            assert_eq!(outs.len(), 1);
            assert!(matches!(outs[0].report.end, RenderEnd::Error(_)));
            assert_eq!(outs[0].report.priority.reverted, Some(Ok(())));
        }
    }

    /// M1 T3：FLAC 与 WAV 按文件头识别混排；期望字节由 libFLAC 参考样本按独立公式放置。
    #[test]
    fn flac_and_wav_mixed_playlist() {
        let names = [
            "pcm16_stereo_44k.wav",
            "cli_16_stereo_44k_default.flac",
            "fixed_orders_and_lpc32.flac",
            "cli_32_stereo_192k.flac",
            "max_block_verbatim_32_stereo.flac",
        ];
        let wav = data_chunk(&fixture(names[0]));
        let first = [
            wav,
            place(&flac_reference("cli_16_stereo_44k_default"), 16, 16),
        ]
        .concat();
        let expect = [
            (first, (44_100, 2, 16, 16)),
            (
                place(&flac_reference("fixed_orders_and_lpc32"), 20, 24),
                (48_000, 1, 24, 24),
            ),
            (
                place(&flac_reference("cli_32_stereo_192k"), 32, 32),
                (192_000, 2, 32, 32),
            ),
            (
                place(&flac_reference("max_block_verbatim_32_stereo"), 32, 32),
                (48_000, 2, 32, 32),
            ),
        ];
        for device in DEVICES {
            let mut b = backend(&ALL, device);
            let (r, outs) = run(&mut b, &names);
            assert!(r.finished && r.software_path_intact(), "{device:?} {r:?}");
            // 同格式 WAV → FLAC 共用一个流（无缝），其余各自重开。
            assert_eq!(outs.len(), 4);
            for (o, (e, (rate, ch, c, v))) in outs.iter().zip(&expect) {
                assert!(!e.is_empty());
                assert_eq!(
                    (
                        o.format.sample_rate(),
                        o.format.channels(),
                        o.format.container_bits(),
                        o.format.valid_bits()
                    ),
                    (*rate, *ch, *c, *v)
                );
                check_output(o, e);
            }
            assert_eq!(outs[3].report.valid_frames, 65_535);
        }
    }

    #[test]
    fn flac_decode_error_drains_verified_frames() {
        for device in DEVICES {
            // 第二帧 CRC-16 错误：只播放已校验的第一帧（512 帧），然后报告 Malformed。
            let mut b = backend(&ALL, device);
            let (r, outs) = run(&mut b, &["err_frame_crc.flac", "pcm16_stereo_44k.wav"]);
            assert_eq!(
                r.fault,
                Some((0, EngineFault::Core(Fault::Decode(DecodeError::Malformed))))
            );
            assert!(!r.finished && !r.software_path_intact());
            assert_eq!(outs.len(), 1);
            assert!(
                r.messages.iter().any(|m| m == "FLAC 错误明细：FrameCrc"),
                "{:?}",
                r.messages
            );
            assert!(r.summary().contains("FLAC 错误明细：FrameCrc"));
            let reference = flac_reference("err_frame_crc");
            assert_eq!(reference.len(), 512 * 2);
            check_output(&outs[0], &place(&reference, 16, 16));

            // 无 fLaC/RIFF 文件头：按内容识别为不支持，不依据扩展名。
            let mut b = backend(&ALL, device);
            let (r, outs) = run(&mut b, &["err_no_marker.flac"]);
            assert_eq!(
                r.fault,
                Some((
                    0,
                    EngineFault::Core(Fault::Decode(DecodeError::Unsupported))
                ))
            );
            assert!(outs.is_empty());
        }
    }

    /// F1：多声道 FLAC（5.1 后环绕、5.1 侧环绕、3.0）经播放器与假设备：设备格式携带源布局
    /// （掩码顺序），布局不同则重开；有效字节等于 libFLAC 参考按独立公式放置。
    #[test]
    fn multichannel_streams_keep_layout() {
        let names = [
            "cli_24_6ch_96k_51.flac",
            "cli_16_6ch_48k_51_side.flac",
            "crafted_3ch_no_tag.flac",
        ];
        let expect = [
            (
                place(&flac_reference("cli_24_6ch_96k_51"), 24, 24),
                0x3Fu32,
                96_000,
            ),
            (
                place(&flac_reference("cli_16_6ch_48k_51_side"), 16, 16),
                0x60F,
                48_000,
            ),
            (
                place(&flac_reference("crafted_3ch_no_tag"), 16, 16),
                0x7,
                44_100,
            ),
        ];
        for device in DEVICES {
            let mut b = backend(&ALL, device);
            let (r, outs) = run(&mut b, &names);
            assert!(r.finished && r.software_path_intact(), "{device:?} {r:?}");
            assert_eq!(outs.len(), 3);
            for (o, (bytes, mask, rate)) in outs.iter().zip(&expect) {
                assert_eq!(o.format.layout().mask(), *mask);
                assert!(o.format.layout().is_wave_order());
                assert_eq!(o.format.sample_rate(), *rate);
                check_output(o, bytes);
            }
        }
    }
}
