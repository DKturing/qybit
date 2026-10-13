//! 生产端播放驱动（M0 T5），对输出后端泛型。
//!
//! 在调用线程（非实时）运行真实 `qy-core`、`qy-codec-wav` 与 pack：同步读文件、
//! 按核心动作执行，把已 pack 的完整帧推入 SPSC 队列；渲染线程由后端创建。
//! 宿主规则与 T4 模拟一致：
//! - 设备启动前预填当前采样率下至少 500 ms，或在播放结束/失败时直接启动排空；
//! - 新源格式能被当前设备格式无损承载（采样率、声道相同且 N ≤ V）时继续使用，
//!   否则先排空当前流（置 EOF、等渲染线程播完退出），再按新格式打开；
//! - 打开失败（设备不支持、独占失败等）回报 `ConfigureFailed`，不做任何回退；
//! - 播放失败时排空已入队的有效音频后停止；渲染线程出错（设备失效等）时停止核心。

use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use qy_core::{Action, DeviceFormat, Event, Fault, SourceFormat};
use qy_pcm_format::ring::{FrameConsumer, prefill_frames};

use crate::HostResources;
use crate::render::{RenderControl, RenderEnd, RenderReport};

/// 设备格式候选 (C, V)：V ≥ N 中 V 最小者优先，同 V 先小容器。
/// 只列出不改变样本值的表示（pack 只做 N ≤ V 的放置）。
pub fn candidates(src: &SourceFormat) -> Vec<(u8, u8)> {
    [(16u8, 16u8), (24, 24), (32, 24), (32, 32)]
        .into_iter()
        .filter(|&(_, v)| v >= src.bits())
        .collect()
}

/// 格式描述类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Descriptor {
    Extensible,
    WaveFormatEx,
}

/// 一次格式尝试的记录（每次尝试使用新的音频客户端）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub format: DeviceFormat,
    pub descriptor: Descriptor,
    /// 独占模式 `IsFormatSupported` 的 HRESULT。
    pub is_format_supported: Option<i32>,
    /// `Initialize` 的 HRESULT。
    pub initialize: Option<i32>,
    /// 缓冲未对齐后按对齐周期在新客户端上重试 `Initialize` 的 HRESULT。
    pub aligned_retry: Option<i32>,
    /// 成功初始化后 `GetBufferSize` 的端点缓冲帧数。
    pub buffer_frames: Option<u32>,
    /// 使用的周期（100 ns）。
    pub period_hns: Option<i64>,
}

/// 协商记录：请求格式与各次尝试。协商依据是独占 `IsFormatSupported`/`Initialize`
/// 的结果，不使用共享混音格式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiation {
    pub source: SourceFormat,
    pub attempts: Vec<Attempt>,
    /// 成功的尝试序号。
    pub chosen: Option<usize>,
}

/// 打开失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenFailure {
    pub negotiation: Negotiation,
    pub message: String,
}

/// 已协商成功、尚未开始渲染的流。
pub trait PendingStream {
    type Running: RunningStream;
    fn format(&self) -> DeviceFormat;
    fn negotiation(&self) -> &Negotiation;
    /// 移交队列消费端并开始渲染（先填首包再 Start）。
    fn start(self, consumer: FrameConsumer, ctl: Arc<RenderControl>) -> Self::Running;
}

/// 渲染中的流。
pub trait RunningStream {
    fn is_finished(&self) -> bool;
    /// 等待渲染线程结束，归还消费端（线程异常时为 `None`）。
    fn join(self) -> (Option<FrameConsumer>, Option<RenderReport>);
}

/// 输出后端。
pub trait Backend {
    type Pending: PendingStream;
    fn open(&mut self, source: &SourceFormat) -> Result<Self::Pending, OpenFailure>;
}

/// 一个输出流的记录。
#[derive(Debug, Clone)]
pub struct StreamReport {
    pub negotiation: Negotiation,
    pub format: DeviceFormat,
    pub pushed_frames: u64,
    pub render: Option<RenderReport>,
}

/// 播放报告。
#[derive(Debug, Clone, Default)]
pub struct PlaybackReport {
    pub streams: Vec<StreamReport>,
    pub open_failures: Vec<OpenFailure>,
    pub fault: Option<(u32, Fault)>,
    /// I/O 错误等附加说明。
    pub messages: Vec<String>,
    pub finished: bool,
    /// 渲染线程以错误结束（如设备失效）。
    pub device_error: bool,
    pub protocol_errors: u32,
}

impl PlaybackReport {
    pub fn underrun_events(&self) -> u64 {
        self.streams
            .iter()
            .filter_map(|s| s.render.as_ref())
            .map(|r| r.underrun_events)
            .sum()
    }

    /// 能否把本次播放记录描述为软件路径完整：无欠载、无失败、无设备错误。
    /// 这不证明设备端 bit-perfect。
    pub fn software_path_intact(&self) -> bool {
        self.finished
            && self.fault.is_none()
            && !self.device_error
            && self.protocol_errors == 0
            && self.underrun_events() == 0
            && self.streams.iter().all(|s| {
                s.render
                    .as_ref()
                    .is_some_and(|r| r.end == RenderEnd::Drained)
            })
    }
}

fn hr_text(v: Option<i32>) -> String {
    v.map_or_else(|| "—".into(), |x| format!("0x{:08X}", x as u32))
}

fn descriptor_text(d: Descriptor) -> &'static str {
    match d {
        Descriptor::Extensible => "WAVEFORMATEXTENSIBLE",
        Descriptor::WaveFormatEx => "WAVEFORMATEX",
    }
}

fn negotiation_text(out: &mut String, n: &Negotiation) {
    use std::fmt::Write as _;
    let s = &n.source;
    let _ = writeln!(
        out,
        "  源格式：{} Hz / {} 声道 / {} 位有效",
        s.sample_rate(),
        s.channels(),
        s.bits()
    );
    for (i, a) in n.attempts.iter().enumerate() {
        let f = &a.format;
        let _ = write!(
            out,
            "  请求 #{i}：{} Hz / {} 声道 / 容器 {} 位 / 有效 {} 位（{}）IsFormatSupported(独占) {}，Initialize {}",
            f.sample_rate(),
            f.channels(),
            f.container_bits(),
            f.valid_bits(),
            descriptor_text(a.descriptor),
            hr_text(a.is_format_supported),
            hr_text(a.initialize),
        );
        if a.aligned_retry.is_some() {
            let _ = write!(out, "，对齐重试 {}", hr_text(a.aligned_retry));
        }
        if let Some(b) = a.buffer_frames {
            let _ = write!(out, "，端点缓冲 {b} 帧");
        }
        if let Some(p) = a.period_hns {
            let _ = write!(out, "，周期 {p}×100ns");
        }
        if n.chosen == Some(i) {
            out.push_str(" ← 已协商");
        }
        out.push('\n');
    }
}

impl PlaybackReport {
    /// 面向用户的中文摘要。只陈述软件路径与独占协商结果，不宣称设备端 bit-perfect。
    pub fn summary(&self) -> String {
        use std::fmt::Write as _;
        let mut o = String::new();
        for (i, s) in self.streams.iter().enumerate() {
            let _ = writeln!(o, "输出流 {}：", i + 1);
            negotiation_text(&mut o, &s.negotiation);
            let _ = writeln!(o, "  已入队帧：{}", s.pushed_frames);
            match &s.render {
                None => o.push_str("  渲染：无报告（渲染线程异常结束）\n"),
                Some(r) => {
                    let reg = match r.mmcss.registered {
                        Ok(()) => "成功".to_string(),
                        Err(c) => format!("失败（Win32 错误 {c}）"),
                    };
                    let rev = match r.mmcss.reverted {
                        None => "未注册无需撤销".to_string(),
                        Some(Ok(())) => "已撤销".to_string(),
                        Some(Err(c)) => format!("撤销失败（Win32 错误 {c}）"),
                    };
                    let end = match r.end {
                        RenderEnd::Drained => "已排空".to_string(),
                        RenderEnd::Stopped => "已停止（未排空）".to_string(),
                        RenderEnd::Error(e) => {
                            format!("错误：{:?} HRESULT 0x{:08X}", e.op, e.hresult as u32)
                        }
                    };
                    let _ = writeln!(o, "  MMCSS “Pro Audio”：注册{reg}，{rev}");
                    let _ = writeln!(
                        o,
                        "  渲染：有效帧 {}，尾部填充 {} 帧，排空静音 {} 帧，欠载 {} 次 / {} 帧，{end}",
                        r.valid_frames,
                        r.tail_fill_frames,
                        r.drain_frames,
                        r.underrun_events,
                        r.underrun_frames
                    );
                    for u in r.underrun_records() {
                        let _ = writeln!(
                            o,
                            "    欠载：第 {} 包（设备帧 {}）缺 {} 帧",
                            u.packet, u.device_frame, u.missing_frames
                        );
                    }
                }
            }
        }
        for f in &self.open_failures {
            let _ = writeln!(o, "独占打开失败：{}", f.message);
            negotiation_text(&mut o, &f.negotiation);
        }
        for m in &self.messages {
            let _ = writeln!(o, "错误：{m}");
        }
        if let Some((track, fault)) = self.fault {
            let _ = writeln!(o, "曲目 {} 失败：{fault:?}", track + 1);
        }
        if self.device_error {
            o.push_str("输出设备错误：播放已停止。\n");
        }
        if self.software_path_intact() {
            o.push_str(
                "结论：全部曲目已播放，无欠载、无错误。这只说明软件路径与独占协商正常，不证明设备端 bit-perfect。\n",
            );
        } else if self.finished && self.underrun_events() > 0 {
            let _ = writeln!(
                o,
                "结论：播放完成但发生 {} 次欠载，本次播放不是完整保真。",
                self.underrun_events()
            );
        } else {
            o.push_str("结论：播放未完整完成。\n");
        }
        o
    }

    /// 进程退出码：0 完整，2 完成但有欠载，1 失败或设备错误。
    pub fn exit_code(&self) -> u8 {
        if self.software_path_intact() {
            0
        } else if self.finished
            && self.fault.is_none()
            && !self.device_error
            && self.protocol_errors == 0
        {
            2
        } else {
            1
        }
    }
}

enum Stream<B: Backend> {
    Pending(B::Pending),
    Running(<B::Pending as PendingStream>::Running),
}

struct Active<B: Backend> {
    stream: Stream<B>,
    format: DeviceFormat,
    negotiation: Negotiation,
    ctl: Arc<RenderControl>,
}

/// 生产端等待队列空间时的休眠间隔。
const FULL_QUEUE_SLEEP: Duration = Duration::from_millis(2);

struct Player<'a, B: Backend> {
    backend: &'a mut B,
    res: &'a mut HostResources,
    files: &'a [PathBuf],
    file: Option<File>,
    active: Option<Active<B>>,
    report: PlaybackReport,
}

/// 依次播放 `files`，直到完成、失败或设备出错。
pub fn play<B: Backend>(
    backend: &mut B,
    res: &mut HostResources,
    files: &[PathBuf],
) -> PlaybackReport {
    let count = u32::try_from(files.len()).unwrap_or(u32::MAX);
    let _ = res.core.stop();
    res.core.play(0, count);
    let mut p = Player {
        backend,
        res,
        files,
        file: None,
        active: None,
        report: PlaybackReport::default(),
    };
    p.run();
    p.report
}

impl<B: Backend> Player<'_, B> {
    fn handle(&mut self, event: Event) {
        if self.res.core.handle(event).is_err() {
            self.report.protocol_errors += 1;
        }
    }

    fn run(&mut self) {
        // 本宿主同步回报所有事件，poll 返回 None 即播放列表已结束。
        while let Some(action) = self.res.core.poll() {
            match action {
                Action::Open { generation, track } => {
                    match self.files.get(track as usize).map(File::open) {
                        Some(Ok(f)) => {
                            self.file = Some(f);
                            self.handle(Event::Opened { generation });
                        }
                        other => {
                            if let Some(Err(e)) = other {
                                self.report
                                    .messages
                                    .push(format!("打开曲目 {track} 失败: {e}"));
                            }
                            self.handle(Event::OpenFailed { generation });
                        }
                    }
                }
                Action::Read {
                    generation,
                    request,
                    ..
                } => self.read(generation, request),
                Action::Configure { generation, format } => {
                    if self.configure(&format) {
                        self.handle(Event::Configured { generation });
                    } else {
                        self.handle(Event::ConfigureFailed { generation });
                    }
                }
                Action::Output { generation, .. } => {
                    let pushed = self.output();
                    self.handle(Event::Output {
                        generation,
                        frames: pushed,
                    });
                    if pushed == 0 && !self.wait_for_space() {
                        // 渲染线程已出错结束：停止核心，旧代次回报此后一律拒绝。
                        if self.res.core.stop().is_some() {
                            self.file = None;
                        }
                        break;
                    }
                }
                Action::Close { .. } => self.file = None,
                Action::Finished { .. } => {
                    self.finish_stream();
                    self.report.finished = !self.report.device_error;
                    break;
                }
                Action::Failed { track, fault, .. } => {
                    self.report.fault = Some((track, fault));
                    if let Some(e) = self.res.core.decoder().flac_error() {
                        self.report.messages.push(format!("FLAC 错误明细：{e:?}"));
                    }
                    self.finish_stream();
                    break;
                }
            }
        }
        // 未正常排空的流（设备错误路径）在此回收。
        self.finish_stream();
    }

    fn read(&mut self, generation: qy_core::Generation, request: u32) {
        let result = match (
            self.file.as_mut(),
            self.res.core.read_target(generation, request),
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
            Ok(0) => Event::ReadEof {
                generation,
                request,
            },
            Ok(len) => Event::ReadDone {
                generation,
                request,
                len,
            },
            Err(e) => {
                self.report.messages.push(format!("读取失败: {e}"));
                Event::ReadFailed {
                    generation,
                    request,
                }
            }
        };
        self.handle(event);
    }

    /// 配置输出；返回是否成功。
    fn configure(&mut self, source: &SourceFormat) -> bool {
        if let Some(a) = &self.active
            && a.format.accepts(source)
        {
            return true;
        }
        self.finish_stream();
        if self.report.device_error {
            return false;
        }
        match self.backend.open(source) {
            Ok(pending) => {
                let format = pending.format();
                if let Err(e) = self.res.configure_ring(&format) {
                    self.report.messages.push(format!("队列重配置失败: {e:?}"));
                    return false;
                }
                self.active = Some(Active {
                    negotiation: pending.negotiation().clone(),
                    stream: Stream::Pending(pending),
                    format,
                    ctl: Arc::new(RenderControl::default()),
                });
                true
            }
            Err(f) => {
                self.report.open_failures.push(f);
                false
            }
        }
    }

    /// pack 并入队；返回入队帧数。
    fn output(&mut self) -> usize {
        let Some(active) = self.active.as_ref() else {
            self.report.protocol_errors += 1;
            return 0;
        };
        let Some(pcm) = self.res.core.pcm() else {
            self.report.protocol_errors += 1;
            return 0;
        };
        let pushed = match self
            .res
            .producer
            .pack_push(&pcm, &active.format, self.res.pack_scratch)
        {
            Ok(n) => n,
            Err(e) => {
                self.report.messages.push(format!("pack 失败: {e:?}"));
                self.report.protocol_errors += 1;
                0
            }
        };
        active
            .ctl
            .pushed_frames
            .fetch_add(pushed as u64, Ordering::Release);
        let queued = self.res.producer.queued_frames_hint();
        if queued as u64 >= prefill_frames(&active.format) {
            self.start_if_pending();
        }
        pushed
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

    /// 置 EOF 并等待当前流播完退出，归还消费端并记录报告。
    fn finish_stream(&mut self) {
        let Some(active) = self.active.take() else {
            return;
        };
        active.ctl.eof.store(true, Ordering::Release);
        if self.report.device_error {
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
            format: active.format,
            pushed_frames: active.ctl.pushed_frames.load(Ordering::Acquire),
            render,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{AUDCLNT_E_UNSUPPORTED_FORMAT, FakeBackend, FakeOutput, check_calls};
    use crate::testutil::{
        data_chunk, fixture, fixture_path, flac_fixture_path, flac_reference, place,
    };
    use qy_core::DecodeError;

    const ALL: [(u8, u8); 4] = [(16, 16), (24, 24), (32, 24), (32, 32)];

    fn run(backend: &mut FakeBackend, names: &[&str]) -> (PlaybackReport, Vec<FakeOutput>) {
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
        let outputs = backend
            .outputs
            .lock()
            .map(|o| o.clone())
            .unwrap_or_default();
        (report, outputs)
    }

    /// 有效帧部分必须等于期望；其后（尾部填充 + 排空）必须全为静音。
    fn check_output(o: &FakeOutput, expected: &[u8]) {
        let ba = usize::from(o.format.block_align());
        let valid = o.report.valid_frames as usize * ba;
        assert_eq!(valid, expected.len(), "有效帧数");
        assert!(o.rendered[..valid] == *expected, "有效字节不一致");
        assert!(o.rendered[valid..].iter().all(|&b| b == 0), "填充须为静音");
        assert_eq!(o.report.underrun_events, 0, "{:?}", o.report);
        assert_eq!(o.report.mmcss.reverted, Some(Ok(())));
        check_calls(&o.calls, o.format.sample_rate() / 100);
    }

    #[test]
    fn same_format_tracks_share_one_stream_gaplessly() {
        let mut b = FakeBackend::new(&ALL);
        let (r, outs) = run(&mut b, &["pcm16_stereo_44k.wav", "pcm16_stereo_44k.wav"]);
        assert!(r.finished && r.software_path_intact(), "{r:?}");
        assert_eq!(outs.len(), 1);
        let raw = data_chunk(&fixture("pcm16_stereo_44k.wav"));
        check_output(&outs[0], &[raw.clone(), raw].concat());
        assert_eq!(r.streams[0].render.map(|x| x.end), Some(RenderEnd::Drained));
    }

    #[test]
    fn format_change_drains_then_reopens() {
        let mut b = FakeBackend::new(&ALL);
        let names = [
            "pcm16_stereo_44k.wav",
            "pcm24_stereo_large.wav",
            "ext20in24_mono_center.wav",
            "ext24in32_stereo_192k.wav",
        ];
        let (r, outs) = run(&mut b, &names);
        assert!(r.software_path_intact(), "{r:?}");
        assert_eq!(outs.len(), 4);
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

    #[test]
    fn unsupported_format_fails_without_fallback() {
        // 设备只支持 16/16 与 32/24：32 位源无可无损承载的格式 → 配置失败，不降精度。
        let mut b = FakeBackend::new(&[(16, 16), (32, 24)]);
        let (r, outs) = run(
            &mut b,
            &[
                "pcm16_stereo_44k.wav",
                "pcm32_stereo_96k.wav",
                "pcm16_stereo_44k.wav",
            ],
        );
        assert_eq!(r.fault, Some((1, Fault::Configure)));
        assert!(!r.finished && !r.software_path_intact());
        assert_eq!(r.open_failures.len(), 1);
        let n = &r.open_failures[0].negotiation;
        assert_eq!(n.chosen, None);
        assert_eq!(n.attempts.len(), 1);
        assert_eq!(
            n.attempts[0].is_format_supported,
            Some(AUDCLNT_E_UNSUPPORTED_FORMAT)
        );
        // 失败前的曲目已完整排空。
        assert_eq!(outs.len(), 1);
        check_output(&outs[0], &data_chunk(&fixture("pcm16_stereo_44k.wav")));
    }

    #[test]
    fn input_faults_drain_queued_audio() {
        let mut b = FakeBackend::new(&ALL);
        let (r, outs) = run(&mut b, &["pcm16_stereo_44k.wav", "does_not_exist.wav"]);
        assert_eq!(r.fault, Some((1, Fault::Open)));
        assert_eq!(outs.len(), 1);
        check_output(&outs[0], &data_chunk(&fixture("pcm16_stereo_44k.wav")));

        let mut b = FakeBackend::new(&ALL);
        let (r, outs) = run(&mut b, &["trunc_mid_frame.wav"]);
        assert_eq!(r.fault, Some((0, Fault::Decode(DecodeError::Truncated))));
        // 截断前 50 个完整帧全部播放。
        let wav = fixture("trunc_mid_frame.wav");
        check_output(&outs[0], &wav[44..44 + 50 * 4]);
    }

    #[test]
    fn summary_reports_negotiation_and_conclusion() {
        let mut b = FakeBackend::new(&ALL);
        let (r, _) = run(&mut b, &["ext20in24_mono_center.wav"]);
        let s = r.summary();
        assert!(s.contains("源格式：96000 Hz / 1 声道 / 20 位有效"), "{s}");
        assert!(s.contains("请求 #0：96000 Hz / 1 声道 / 容器 24 位 / 有效 24 位（WAVEFORMATEXTENSIBLE）IsFormatSupported(独占) 0x00000000，Initialize 0x00000000"), "{s}");
        assert!(s.contains("← 已协商"));
        assert!(s.contains("MMCSS “Pro Audio”：注册成功，已撤销"));
        assert!(s.contains("欠载 0 次 / 0 帧，已排空"));
        assert!(s.contains("不证明设备端 bit-perfect"));
        assert_eq!(r.exit_code(), 0);

        let mut b = FakeBackend::new(&[(16, 16)]);
        let (r, _) = run(&mut b, &["pcm24_stereo_large.wav"]);
        let s = r.summary();
        assert!(s.contains("独占打开失败"), "{s}");
        assert!(s.contains("IsFormatSupported(独占) 0x88890008"), "{s}");
        assert!(s.contains("曲目 1 失败：Configure"), "{s}");
        assert!(s.contains("结论：播放未完整完成。"));
        assert!(!s.contains("无欠载、无错误"));
        assert_eq!(r.exit_code(), 1);
    }

    #[test]
    fn device_invalidated_stops_playback() {
        let mut b = FakeBackend::new(&ALL);
        b.invalidate = Some((0, 5));
        let (r, outs) = run(
            &mut b,
            &["pcm24_stereo_large.wav", "pcm24_stereo_large.wav"],
        );
        assert!(r.device_error, "{r:?}");
        assert!(!r.finished && !r.software_path_intact());
        assert_eq!(outs.len(), 1);
        assert!(matches!(outs[0].report.end, RenderEnd::Error(_)));
        assert_eq!(outs[0].report.mmcss.reverted, Some(Ok(())));
    }

    /// M1 T3：FLAC 与 WAV 按文件头识别混排；期望字节由 libFLAC 参考样本按独立公式放置。
    #[test]
    fn flac_and_wav_mixed_playlist() {
        let mut b = FakeBackend::new(&ALL);
        let names = [
            "pcm16_stereo_44k.wav",
            "cli_16_stereo_44k_default.flac",
            "fixed_orders_and_lpc32.flac",
            "cli_32_stereo_192k.flac",
            "max_block_verbatim_32_stereo.flac",
        ];
        let (r, outs) = run(&mut b, &names);
        assert!(r.finished && r.software_path_intact(), "{r:?}");
        // 同格式 WAV → FLAC 共用一个流（无缝），其余各自重开。
        assert_eq!(outs.len(), 4);
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

    #[test]
    fn flac_decode_error_drains_verified_frames() {
        // 第二帧 CRC-16 错误：只播放已校验的第一帧（512 帧），然后报告 Malformed。
        let mut b = FakeBackend::new(&ALL);
        let (r, outs) = run(&mut b, &["err_frame_crc.flac", "pcm16_stereo_44k.wav"]);
        assert_eq!(r.fault, Some((0, Fault::Decode(DecodeError::Malformed))));
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
        let mut b = FakeBackend::new(&ALL);
        let (r, outs) = run(&mut b, &["err_no_marker.flac"]);
        assert_eq!(r.fault, Some((0, Fault::Decode(DecodeError::Unsupported))));
        assert!(outs.is_empty());
    }
}
