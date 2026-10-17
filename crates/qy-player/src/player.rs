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
//! 播放控制（GUI T2）：[`PlayControl::commands`] 接收暂停、恢复、上一首、下一首、跳曲与停止，
//! 转交引擎执行（规则在引擎中，CLI 与 GUI 共用）；暂停时渲染线程送静音、不取队列，静音单独
//! 计数，不算欠载。[`PlayControl::on_status`] 给出节流后的状态快照（界面显示用）。

use std::fs::File;
use std::io::{self, ErrorKind, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use qy_core::engine::{ChannelPolicy, FloatPolicy, HostAction, HostEvent, Poll, Wait};
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
    /// 读取与定位（S1）。不可定位的音源在 `seek` 中返回错误，播放器改为顺序解码到目标位置。
    type Reader: Read + Seek;
    /// 曲目数。
    fn count(&self) -> u32;
    /// 打开第 `track` 首。
    fn open(&mut self, track: u32) -> io::Result<Self::Reader>;
    /// 第 `track` 首的显示名（实时输出用）。
    fn name(&self, track: u32) -> String {
        format!("第 {} 首", u64::from(track) + 1)
    }
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

    fn name(&self, track: u32) -> String {
        self.0.get(track as usize).map_or_else(
            || format!("第 {} 首", track + 1),
            |p| p.display().to_string(),
        )
    }
}

/// 播放控制命令（GUI T2）。由引擎按其规则执行：切歌丢弃已入队的音频并从目标曲目开头播放，
/// 暂停时设备输出静音（单独计数，不算欠载）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Pause,
    Resume,
    /// 下一首（相对正在播放的曲目）；已是最后一首时停止。
    Next,
    /// 上一首（第一首时从头重播）。
    Prev,
    /// 跳到第 k 首（从 0 起）；超出列表时忽略。
    Goto(u32),
    /// 停止：立即停止设备、丢弃队列（不排空），报告标记为已中断。
    Stop,
    /// 在正在播放的曲目内定位到第 n 帧（源格式帧，从 0 起）；暂停状态保持，等于或超过曲目长度时
    /// 进入下一首（S1）。
    Seek(u64),
    /// 单声道复制到左右两路（O1，非严格选项）开启或关闭：对之后开始解码的曲目生效，正在播放与
    /// 已预读的曲目不变；之后跳曲（含跳回当前曲目）一定按新设置。
    MonoToStereo(bool),
    /// 浮点转整数（tasks/F3.md，明确开启的非严格选项）：开启后，设备不接受源的浮点格式时转为整数
    /// （样本已改变）。对之后配置的曲目生效。
    FloatToInt(bool),
}

/// 播放状态快照（界面显示用；生产端线程生成，不涉及渲染线程的实时路径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    /// 播放列表在进行中（含排空尾部）。
    pub active: bool,
    pub paused: bool,
    /// 正在播放的曲目（从 0 起）。
    pub track: u32,
    /// 该曲目已由设备取走的帧数（设备格式帧；最多含一个设备缓冲尚未发声）。
    pub position_frames: u64,
    /// 正在播放的曲目的源格式（不是正在预读的下一首）。
    pub source: Option<SourceFormat>,
    /// 当前设备流的格式。
    pub device: Option<DeviceFormat>,
    /// 本次播放累计的欠载事件与缺失帧（含正在进行的流）。
    pub underrun_events: u64,
    pub underrun_frames: u64,
    /// 本次播放累计的暂停静音帧（不是欠载）。
    pub paused_frames: u64,
    /// 本次播放累计按单声道复制（O1，声道已改变）送到设备的曲目数，与报告的
    /// `channels_altered` 一致。
    pub channels_altered: u32,
    /// 本次播放因解码或文件错误跳过的曲目数（与报告的 `skipped` 一致）。
    pub skipped_tracks: u32,
    /// 本次播放累计按明确开启的选项把浮点转为整数的曲目数（F3，样本已改变），与报告的
    /// `samples_altered` 一致。
    pub samples_altered: u32,
    /// 本次播放已执行的按操作定位次数（与报告的 `user_seeks` 一致）。界面据此判断状态快照是否已
    /// 反映最近一次定位（S1）。
    pub user_seeks: u32,
}

/// 播放时的实时输出、中断与控制。
pub struct PlayControl<'a> {
    /// 每条进度信息（曲目开始、设备协商结果、跳过、结构修正、失败、中断）调用一次，每次一行或
    /// 几行完整文本；播放结束后的完整报告仍由 [`PlaybackReport::summary`] 给出。
    pub on_line: &'a mut dyn FnMut(&str),
    /// 置位后停止播放：立即停止设备、丢弃队列（不排空），报告照常返回并标记为已中断。
    pub stop: Option<&'a AtomicBool>,
    /// 控制命令（非阻塞读取；发送端断开不影响播放）。
    pub commands: Option<&'a Receiver<Command>>,
    /// 从第几首开始播放（从 0 起；超出列表时直接结束）。上一首/跳曲仍可回到之前的曲目。
    pub first: u32,
    /// 状态快照：状态变化时立即调用，播放中至多每 [`STATUS_INTERVAL`] 调用一次，结束时调用
    /// 一次（`active` 为假）。回调在生产端线程执行，应很快返回（如存入共享状态并通知界面）。
    pub on_status: Option<&'a mut dyn FnMut(&Status)>,
}

/// 播放中状态快照的最小间隔。
pub const STATUS_INTERVAL: Duration = Duration::from_millis(100);

/// 生产端等待队列空间时的休眠间隔。
const FULL_QUEUE_SLEEP: Duration = Duration::from_millis(2);

struct Player<'a, B: Backend, S: TrackSource> {
    backend: &'a mut B,
    res: &'a mut HostResources,
    source: &'a mut S,
    file: Option<S::Reader>,
    active: Option<Active<B>>,
    report: PlaybackReport<B::Attempt>,
    ctl: PlayControl<'a>,
    count: u32,
    /// 上次给出的状态与时间。
    last_status: Option<(Status, Instant)>,
    /// 当前流已置 EOF、等待渲染线程播完（排空期间照常处理控制命令与状态）。
    draining: bool,
    /// 仍在播放或排队的曲目与其源格式（按解码顺序）：解码可能领先正在播放的曲目若干首（至多引擎
    /// 队列可容纳的曲目边界数），状态显示按正在播放的曲目查找。记录新曲目时只淘汰已经播完的曲目。
    formats: std::collections::VecDeque<(u32, SourceFormat)>,
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
    play_with(
        backend,
        res,
        source,
        PlayControl {
            on_line: &mut |_| {},
            stop: None,
            commands: None,
            first: 0,
            on_status: None,
        },
    )
}

/// 同 [`play_source`]，并给出实时进度与中断（命令行播放用）。
pub fn play_with<'a, B: Backend, S: TrackSource>(
    backend: &'a mut B,
    res: &'a mut HostResources,
    source: &'a mut S,
    ctl: PlayControl<'a>,
) -> PlaybackReport<B::Attempt> {
    let count = source.count();
    // 清理上一次播放可能残留的状态（旧代次），丢弃其控制动作。
    res.engine.stop();
    while res.engine.take_control().is_some() {}
    res.engine.play(ctl.first, count);
    let mut p = Player {
        backend,
        res,
        source,
        file: None,
        active: None,
        report: PlaybackReport::default(),
        ctl,
        count,
        last_status: None,
        draining: false,
        formats: std::collections::VecDeque::new(),
    };
    p.run();
    p.status(true);
    p.report
}

impl<B: Backend, S: TrackSource> Player<'_, B, S> {
    fn handle(&mut self, event: HostEvent) {
        if self.res.engine.handle(event).is_err() {
            self.report.protocol_errors += 1;
        }
    }

    fn line(&mut self, text: &str) {
        (self.ctl.on_line)(text);
    }

    /// 中断：立即停止核心与设备（不排空），之后旧代次的回报一律拒绝。
    fn interrupt(&mut self) {
        // 中断前已排队的动作（如最后一首排空后的“播放结束”）先执行：中断若落在播放已经结束
        // 之后，报告应是“完成”而不是“中断”。
        while let Some(a) = self.res.engine.take_control() {
            if !self.execute(a) {
                return;
            }
        }
        self.report.interrupted = true;
        self.line("已中断：停止播放（不排空队列）。");
        self.res.engine.stop();
        while let Some(a) = self.res.engine.take_control() {
            self.execute(a);
        }
        self.finish_stream(true);
    }

    /// 执行已收到的控制命令；返回 false 表示按命令停止。
    fn commands(&mut self) -> bool {
        let Some(rx) = self.ctl.commands else {
            return true;
        };
        while let Ok(cmd) = rx.try_recv() {
            let e = &mut self.res.engine;
            match cmd {
                Command::Pause => e.pause(),
                Command::Resume => e.resume(),
                Command::Next => e.next(),
                Command::Prev => e.prev(),
                Command::Goto(k) if k < self.count => e.skip_to(k),
                Command::Goto(_) => continue,
                Command::Stop => return false,
                Command::Seek(frame) => e.seek(frame),
                Command::MonoToStereo(on) => e.set_channel_policy(if on {
                    ChannelPolicy::DuplicateMono
                } else {
                    ChannelPolicy::Strict
                }),
                Command::FloatToInt(on) => e.set_float_policy(if on {
                    FloatPolicy::ToInt
                } else {
                    FloatPolicy::Strict
                }),
            }
            match cmd {
                Command::Next | Command::Prev | Command::Goto(_) => {
                    self.report.user_skips += 1;
                    let (track, _) = self.res.engine.playing_progress();
                    let what = if self.res.engine.is_active() {
                        format!("从曲目 {} 开始", u64::from(track) + 1)
                    } else {
                        "已是最后一首，停止".to_string()
                    };
                    self.line(&format!("⏭ 按操作切换：丢弃已入队的音频，{what}"));
                }
                Command::Seek(frame) => {
                    self.report.user_seeks += 1;
                    let (track, _) = self.res.engine.playing_progress();
                    let at = self
                        .formats
                        .iter()
                        .rev()
                        .find(|(t, _)| *t == track)
                        .map_or(String::new(), |(_, f)| {
                            let s = frame / u64::from(f.sample_rate().max(1));
                            format!("{}:{:02}，", s / 60, s % 60)
                        });
                    self.line(&format!(
                        "⇥ 按操作定位：丢弃已入队的音频，曲目 {} 从 {at}第 {frame} 帧开始",
                        u64::from(track) + 1
                    ));
                }
                Command::Pause => self.line("‖ 暂停（设备输出静音，不算欠载）"),
                Command::Resume => self.line("▶ 继续"),
                Command::MonoToStereo(true) => self.line(
                    "单声道复制到左右两路：已开启，重新播放或切换曲目后生效（已预读的下一首按原设置）；\
                     单声道 [FC] 按 [FL FR] 播放，样本值不变，声道已改变，不是严格路径",
                ),
                Command::MonoToStereo(false) => {
                    self.line("单声道复制到左右两路：已关闭，重新播放或切换曲目后按严格路径（已预读的下一首按原设置）")
                }
                Command::FloatToInt(true) => self.line(
                    "浮点转整数：已开启，之后配置的浮点曲目在设备不接受浮点时转为整数（样本已改变，不是严格路径）",
                ),
                Command::FloatToInt(false) => {
                    self.line("浮点转整数：已关闭，之后配置的浮点曲目按严格路径（设备不接受浮点时拒绝）")
                }
                Command::Stop => {}
            }
            // 引擎排队的控制动作（关闭音源、停止设备、暂停/恢复）立即执行。
            while let Some(a) = self.res.engine.take_control() {
                if !self.execute(a) {
                    break;
                }
            }
            self.status(true);
        }
        true
    }

    /// 记录正在解码的曲目的源格式（文件头解析完成后才有）。
    fn note_format(&mut self) {
        let e = &self.res.engine;
        let Some(f) = qy_core::Decoder::format(e.decoder()) else {
            return;
        };
        let entry = (e.track(), f);
        if self.formats.back() != Some(&entry) {
            // 引擎按顺序播放：仍需要的是正在播放的曲目到正在解码的曲目之间的各首（切歌后正在解码的
            // 曲目可能在前，此时只保留这两首）。
            let (playing, decoding) = (e.playing_progress().0, e.track());
            let keep = |t: u32| t == playing || t == decoding || (playing..=decoding).contains(&t);
            self.formats.retain(|(t, _)| keep(*t));
            self.formats.push_back(entry);
        }
    }

    /// 给出状态快照：`force` 或状态变化时立即给出，否则按 [`STATUS_INTERVAL`] 节流。
    fn status(&mut self, force: bool) {
        if self.ctl.on_status.is_none() {
            return;
        }
        let e = &self.res.engine;
        let (track, position_frames) = e.playing_progress();
        let finished = self.report.streams.iter().filter_map(|s| s.render.as_ref());
        let (mut ue, mut uf, mut pf) = (0, 0, 0);
        for r in finished {
            ue += r.underrun_events;
            uf += r.underrun_frames;
            pf += r.paused_frames;
        }
        if let Some(a) = &self.active {
            ue += a.ctl.underrun_events.load(Ordering::Acquire);
            uf += a.ctl.underrun_frames.load(Ordering::Acquire);
            pf += a.ctl.paused_frames.load(Ordering::Acquire);
        }
        let now = Status {
            active: e.is_active(),
            paused: e.is_paused(),
            track,
            position_frames,
            source: self
                .formats
                .iter()
                .rev()
                .find(|(t, _)| *t == track)
                .map(|(_, f)| *f),
            device: self.active.as_ref().map(|a| a.format),
            underrun_events: ue,
            underrun_frames: uf,
            paused_frames: pf,
            channels_altered: u32::try_from(self.report.channels_altered.len()).unwrap_or(u32::MAX),
            skipped_tracks: u32::try_from(self.report.skipped.len()).unwrap_or(u32::MAX),
            samples_altered: u32::try_from(self.report.samples_altered.len()).unwrap_or(u32::MAX),
            user_seeks: self.report.user_seeks,
        };
        let changed = self.last_status.is_none_or(|(prev, at)| {
            let discrete = |s: &Status| {
                (
                    s.active,
                    s.paused,
                    s.track,
                    s.source,
                    s.device,
                    s.underrun_events,
                    s.channels_altered,
                    s.skipped_tracks,
                    s.samples_altered,
                    s.user_seeks,
                )
            };
            discrete(&prev) != discrete(&now) || at.elapsed() >= STATUS_INTERVAL
        });
        if force || changed {
            if let Some(f) = self.ctl.on_status.as_mut() {
                f(&now);
            }
            self.last_status = Some((now, Instant::now()));
        }
    }

    fn run(&mut self) {
        loop {
            if self.ctl.stop.is_some_and(|s| s.load(Ordering::Acquire)) || !self.commands() {
                self.interrupt();
                break;
            }
            let poll = self.res.engine.poll();
            self.note_format();
            self.sync_pushed();
            self.status(false);
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
                Poll::Wait(Wait::Device) if self.draining => self.poll_drain(),
                Poll::Wait(w) => {
                    // 本宿主同步回报读取与设备打开结果，只有排空需要等待。
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
            HostAction::Open { generation, track } => {
                let name = self.source.name(track);
                let count = self.count;
                self.line(&format!("▶ 曲目 {}/{count}：{name}", u64::from(track) + 1));
                match self.source.open(track) {
                    Ok(f) => {
                        self.file = Some(f);
                        self.handle(HostEvent::Opened { generation });
                    }
                    Err(e) => {
                        let m = format!("打开曲目 {track} 失败: {e}");
                        self.line(&m);
                        self.report.messages.push(m);
                        self.handle(HostEvent::OpenFailed { generation });
                    }
                }
            }
            HostAction::Read {
                generation,
                request,
                ..
            } => self.read(generation, request),
            HostAction::Seek {
                generation,
                request,
                offset,
            } => {
                let ok = self
                    .file
                    .as_mut()
                    .is_some_and(|f| f.seek(SeekFrom::Start(offset)).is_ok());
                self.handle(if ok {
                    HostEvent::SeekDone {
                        generation,
                        request,
                    }
                } else {
                    HostEvent::SeekFailed {
                        generation,
                        request,
                    }
                });
            }
            HostAction::SeekSequential { track } => {
                self.line(&format!(
                    "  曲目 {} 的音源不支持定位：按顺序解码到目标位置（结果相同，只是更慢）",
                    u64::from(track) + 1
                ));
                self.report.sequential_seeks.push(track);
            }
            HostAction::Close { .. } => {
                self.file = None;
                let tag = self.res.engine.decoder().skipped_tag_bytes();
                if tag > 0 {
                    self.line(&format!(
                        "  已跳过文件开头的 ID3v2 标签（{tag} 字节，元数据，不含音频）"
                    ));
                }
            }
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
                // 实时输出协商结果（源格式、每次请求与结果、选定的设备格式）。
                let mut text = String::new();
                if let Some(a) = self.active.as_ref() {
                    text.push_str("  设备流已打开：\n");
                    crate::report::negotiation_text(&mut text, &a.negotiation);
                    if let Some(p) = &a.plan {
                        let _ = std::fmt::Write::write_fmt(
                            &mut text,
                            format_args!("  播放计划：{p}\n"),
                        );
                    }
                } else if let Some(f) = self.report.open_failures.last() {
                    let _ = std::fmt::Write::write_fmt(
                        &mut text,
                        format_args!("  设备打开失败：{}\n", f.message),
                    );
                    crate::report::negotiation_text(&mut text, &f.negotiation);
                }
                if !text.is_empty() {
                    self.line(text.trim_end());
                }
            }
            HostAction::StartDevice => self.start_if_pending(),
            HostAction::DrainDevice => self.begin_drain(),
            HostAction::StopDevice => self.finish_stream(true),
            HostAction::PauseDevice | HostAction::ResumeDevice => {
                let pause = action == HostAction::PauseDevice;
                match &self.active {
                    Some(a) => a.ctl.pause.store(pause, Ordering::Release),
                    // 引擎只对已启动的设备发出暂停/恢复。
                    None => self.report.protocol_errors += 1,
                }
            }
            HostAction::TrackSkipped { track, fault } => {
                self.line(&format!(
                    "  曲目 {} 已跳过：{}",
                    u64::from(track) + 1,
                    crate::report::fault_text(&fault)
                ));
                self.report.skipped.push((track, fault));
            }
            HostAction::Repaired { track, repair } => {
                self.line(&format!(
                    "  曲目 {} 按规则修正文件结构：{repair}（样本未改动）",
                    u64::from(track) + 1
                ));
                self.report.repairs.push((track, repair));
            }
            HostAction::FloatConverted { track, bits } => {
                self.line(&format!(
                    "  曲目 {} 样本已改变：浮点转为 {bits} 位整数（明确开启；设备不接受该浮点格式）",
                    u64::from(track) + 1
                ));
                self.report.samples_altered.push((track, bits));
            }
            HostAction::FloatStats { track, report } => self.report.add_float_stats(track, report),
            HostAction::ChannelsDuplicated { track } => {
                self.line(&format!(
                    "  曲目 {} 声道已改变：单声道 [FC] 复制为 [FL FR]（明确开启；样本值未改变，不混音）",
                    u64::from(track) + 1
                ));
                self.report.channels_altered.push(track);
            }
            HostAction::Finished => {
                self.report.finished = !self.report.device_error;
                return false;
            }
            HostAction::Failed { track, fault } => {
                self.report.fault = Some((track, fault));
                let mut details = Vec::new();
                let d = self.res.engine.decoder();
                #[cfg(feature = "flac")]
                if let Some(e) = d.flac_error() {
                    details.push(format!("FLAC 错误明细：{e:?}"));
                }
                if let Some(w) = d.wav_detail() {
                    details.push(format!("WAV 错误明细：{w}"));
                }
                if let Some(a) = d.aiff_detail() {
                    details.push(format!("AIFF 错误明细：{a}"));
                }
                if let Some(u) = d.unrecognized() {
                    details.push(format!("无法识别的文件：{u}"));
                }
                if let Some(name) = d.disabled() {
                    details.push(format!("本构建未启用 {name} 解码器，按不支持拒绝"));
                }
                self.line(&format!(
                    "  曲目 {} 失败：{}",
                    u64::from(track) + 1,
                    crate::report::fault_text(&fault)
                ));
                for m in details {
                    self.line(&format!("  {m}"));
                    self.report.messages.push(m);
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
        // O1：按单声道复制以立体声请求设备时，协商记录写明源文件的实际格式。
        let duplicated_mono = self.res.engine.duplicating();
        match self.backend.open(source) {
            Ok(pending) => {
                let format = pending.format();
                if let Err(e) = self.res.configure_ring(&format) {
                    self.report.messages.push(format!("队列重配置失败: {e:?}"));
                    return None;
                }
                let mut negotiation = pending.negotiation().clone();
                negotiation.duplicated_mono = duplicated_mono;
                self.active = Some(Active {
                    negotiation,
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
            Err(mut f) => {
                f.negotiation.duplicated_mono = duplicated_mono;
                self.report.open_failures.push(f);
                None
            }
        }
    }

    /// 排空开始：置 EOF（尚未启动的流以排空方式启动），之后由 [`Self::poll_drain`] 等它播完。
    /// 不在此阻塞，排空期间仍响应暂停、切歌与停止。
    fn begin_drain(&mut self) {
        match &self.active {
            Some(a) => {
                a.ctl.eof.store(true, Ordering::Release);
                self.start_if_pending();
                self.draining = true;
            }
            None => self.handle(HostEvent::DeviceDrained),
        }
    }

    /// 排空中：渲染线程结束后回收流并回报 `DeviceDrained`，否则短暂休眠。
    fn poll_drain(&mut self) {
        let done = match self.active.as_ref().map(|a| &a.stream) {
            Some(Stream::Running(r)) => r.is_finished(),
            _ => true,
        };
        if done {
            self.finish_stream(false);
            self.handle(HostEvent::DeviceDrained);
        } else {
            std::thread::sleep(FULL_QUEUE_SLEEP);
        }
    }

    /// 队列满：启动尚未开始的流，或休眠等待渲染线程取走数据。
    /// 渲染线程已结束（出错）时返回 false。
    fn wait_for_space(&mut self) -> bool {
        // 暂停时尚未启动的流在恢复之前不启动（引擎恢复时发出 StartDevice）。
        if !self.res.engine.is_paused() {
            self.start_if_pending();
        }
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
        self.draining = false;
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
    #[cfg(feature = "flac")]
    use crate::testing::flac_reference;
    use crate::testing::{
        FakeBackend, FakeDevice, FakeOutput, Negotiator, NoMonoNegotiator, TestAttempt, WCall,
        check_calls, check_write_calls, data_chunk, ffmpeg_reference, fixture, fixture_path,
        flac_fixture_path, place,
    };
    use qy_core::engine::EngineFault;
    use qy_core::{DecodeError, Fault, SampleEncoding};

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

    /// 实时输出：曲目开始、源格式、每次请求与选定的设备格式、结构修正；中断时停止设备并标记。
    #[test]
    fn live_lines_and_interrupt() {
        let files: Vec<PathBuf> = ["pcm16_stereo_44k.wav", "fix_data_partial_frame.wav"]
            .iter()
            .map(|n| fixture_path(n))
            .collect();
        let mut b = backend(&ALL, FakeDevice::Packet);
        let Ok(mut res) = HostResources::new() else {
            panic!("res");
        };
        let mut lines = Vec::new();
        let r = play_with(
            &mut b,
            &mut res,
            &mut Files(&files),
            PlayControl {
                on_line: &mut |l: &str| lines.push(l.to_string()),
                stop: None,
                commands: None,
                first: 0,
                on_status: None,
            },
        );
        let all = lines.join("\n");
        assert!(r.finished, "{all}");
        assert!(all.contains("▶ 曲目 1/2："), "{all}");
        assert!(all.contains("pcm16_stereo_44k.wav"), "{all}");
        assert!(all.contains("▶ 曲目 2/2："), "{all}");
        assert!(all.contains("设备流已打开"), "{all}");
        assert!(
            all.contains("源格式：44100 Hz / 2 声道 / 16 位有效"),
            "{all}"
        );
        assert!(all.contains("← 已协商"), "{all}");
        assert!(all.contains("曲目 2 按规则修正文件结构"), "{all}");

        // 中断：第二首开始时置位，播放立即停止，报告标记中断，消费端归还。
        let stop = AtomicBool::new(false);
        let mut b = backend(&ALL, FakeDevice::Packet);
        let mut lines = Vec::new();
        let r = play_with(
            &mut b,
            &mut res,
            &mut Files(&files),
            PlayControl {
                on_line: &mut |l: &str| {
                    if l.starts_with("▶ 曲目 2/2") {
                        stop.store(true, Ordering::Release);
                    }
                    lines.push(l.to_string());
                },
                stop: Some(&stop),
                commands: None,
                first: 0,
                on_status: None,
            },
        );
        let s = r.summary();
        assert!(r.interrupted && !r.finished, "{s}");
        assert_eq!(r.exit_code(), 1);
        assert!(s.contains("已按用户要求中断"), "{s}");
        assert!(lines.iter().any(|l| l.starts_with("已中断")), "{lines:?}");
        assert!(res.consumer.is_some(), "消费端须归还");
        // 中断之后同一资源可以再次播放。
        let mut b = backend(&ALL, FakeDevice::Packet);
        let r = play(&mut b, &mut res, &files[..1]);
        assert!(r.finished && r.software_path_intact());
    }

    /// F2：AIFF 曲目的设备字节等于独立参考（大端样本按设备格式 pack 为小端）；规范之外的
    /// AIFF-C 类型按“扩展”拒绝，实时输出写明明细。
    #[test]
    fn aiff_plays_bit_exact_and_reports_rejections() {
        use crate::testing::aiff_fixture_path;
        let files = [
            aiff_fixture_path("aiff_16_stereo_44k.aiff"),
            aiff_fixture_path("ext_sowt.aiff"),
        ];
        let mut b = backend(&ALL, FakeDevice::Packet);
        let Ok(mut res) = HostResources::new() else {
            panic!("res");
        };
        let mut lines = Vec::new();
        let r = play_with(
            &mut b,
            &mut res,
            &mut Files(&files),
            PlayControl {
                on_line: &mut |l: &str| lines.push(l.to_string()),
                stop: None,
                commands: None,
                first: 0,
                on_status: None,
            },
        );
        let all = lines.join("\n");
        assert!(!r.finished, "{all}");
        assert!(
            all.contains("AIFF 错误明细：AIFF-C 规范之外的扩展") && all.contains("'sowt'"),
            "{all}"
        );
        // 独立参考：.expect 中的样本按 16 位小端写出。
        let text = std::fs::read_to_string(
            aiff_fixture_path("aiff_16_stereo_44k.aiff").with_extension("expect"),
        )
        .unwrap_or_default();
        let want: Vec<u8> = text
            .lines()
            .skip(4)
            .flat_map(|l| {
                l.split(' ')
                    .filter_map(|x| x.parse::<i16>().ok())
                    .flat_map(i16::to_le_bytes)
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(want.len(), 600 * 4);
        let outs = b.outputs();
        assert_eq!(outs.len(), 1);
        check_output(&outs[0], &want);
    }

    /// O1：开启单声道复制后，[FC] 曲目按 [FL FR] 打开设备，每个样本原样写入左右两路（设备字节等于
    /// 独立参考：data 块每 2 字节重复一次）；实时输出与报告写明“声道已改变”，结论不再是严格路径；
    /// 默认关闭时同一文件按 [FC] 严格播放。
    /// F3：浮点 WAV。设备接受 32 位浮点时按位输出（严格路径）；只接受整数时严格路径拒绝并说明，
    /// 明确开启转换后转为设备有效位最大的整数，报告“样本已改变”与统计。
    #[test]
    fn float_strict_or_converted() {
        use crate::testing::FAKE_FLOAT32;
        let name = "float/f32_stereo_44k_tag3.wav";
        let raw = data_chunk(&fixture(name));
        let files = [fixture_path(name)];
        for device in DEVICES {
            // 设备接受浮点：按位输出。
            let mut b = backend(&[(16, 16), FAKE_FLOAT32], device);
            let (r, outs) = run(&mut b, &[name]);
            assert!(
                r.strict_path_intact() && r.samples_altered.is_empty(),
                "{}",
                r.summary()
            );
            assert_eq!(outs[0].format.encoding(), SampleEncoding::Float32);
            check_output(&outs[0], &raw);
            assert!(
                r.summary().contains("32 位浮点（IEEE_FLOAT）"),
                "{}",
                r.summary()
            );
            // 只接受整数、严格路径：拒绝。
            let mut b = backend(&ALL, device);
            let (r, outs) = run(&mut b, &[name]);
            assert!(outs.is_empty() && r.fault.is_some(), "{}", r.summary());
            // 明确开启转换：转为 32/32（设备有效位最大）。
            let mut b = backend(&ALL, device);
            let Ok(mut res) = HostResources::new() else {
                panic!("res");
            };
            res.engine.set_float_policy(FloatPolicy::ToInt);
            let mut lines = Vec::new();
            let r = play_with(
                &mut b,
                &mut res,
                &mut Files(&files),
                PlayControl {
                    on_line: &mut |l: &str| lines.push(l.to_string()),
                    stop: None,
                    commands: None,
                    first: 0,
                    on_status: None,
                },
            );
            let all = lines.join("\n");
            assert!(r.software_path_intact() && !r.strict_path_intact(), "{all}");
            assert_eq!(r.samples_altered, vec![(0, 32)], "{all}");
            assert!(
                all.contains("曲目 1 样本已改变：浮点转为 32 位整数"),
                "{all}"
            );
            let text = r.summary();
            assert!(
                text.contains("曲目 1 样本已改变：32 位浮点转为 32 位整数")
                    && text.contains("不是严格路径"),
                "{text}"
            );
            assert!(!text.contains("软件路径无损"), "{text}");
            let outs = b.outputs();
            assert_eq!(
                (outs[0].format.container_bits(), outs[0].format.valid_bits()),
                (32, 32)
            );
            // 设备字节等于按定义转换（[−1, 1) 内的值；与 ffmpeg 的 32 位参考相同）。
            let want = std::fs::read(fixture_path("float/f32_stereo_44k_tag3.ffmpeg32.s32le"))
                .unwrap_or_default();
            check_output(&outs[0], &want);
        }
    }

    /// “设备不接受该浮点格式”的记录不能跨播放会话：同一组宿主资源（GUI 中跨会话复用）先在只接受
    /// 整数的设备 A 上转换播放浮点，再在接受 32 位浮点的设备 B 上播放“16 位整数、同格式浮点”，第二首
    /// 必须按位原样送到浮点格式，不能沿用设备 A 的拒绝记录直接转成整数。
    #[test]
    fn float_rejection_does_not_outlive_session() {
        use crate::testing::FAKE_FLOAT32;
        let name = "float/f32_stereo_44k_tag3.wav";
        let raw = data_chunk(&fixture(name));
        let Ok(mut res) = HostResources::new() else {
            panic!("res");
        };
        res.engine.set_float_policy(FloatPolicy::ToInt);
        for device in DEVICES {
            // 设备 A：只接受整数，浮点转换。
            let mut a = backend(&ALL, device);
            let files = [fixture_path(name)];
            let r = play_with(
                &mut a,
                &mut res,
                &mut Files(&files),
                PlayControl {
                    on_line: &mut |_: &str| {},
                    stop: None,
                    commands: None,
                    first: 0,
                    on_status: None,
                },
            );
            assert_eq!(r.samples_altered, vec![(0, 32)], "{}", r.summary());
            // 设备 B：16 位整数与 32 位浮点。
            let mut b = backend(&[(16, 16), FAKE_FLOAT32], device);
            let files = [fixture_path("pcm16_stereo_44k.wav"), fixture_path(name)];
            let r = play_with(
                &mut b,
                &mut res,
                &mut Files(&files),
                PlayControl {
                    on_line: &mut |_: &str| {},
                    stop: None,
                    commands: None,
                    first: 0,
                    on_status: None,
                },
            );
            assert!(r.samples_altered.is_empty(), "{}", r.summary());
            let outs = b.outputs();
            let Some(last) = outs.last() else {
                panic!("无输出");
            };
            assert_eq!(
                last.format.encoding(),
                SampleEncoding::Float32,
                "{}",
                r.summary()
            );
            check_output(last, &raw);
        }
    }

    #[test]
    fn mono_to_stereo_duplicates_and_reports_altered() {
        let raw = data_chunk(&fixture("pcm16_mono_8k_fmt18.wav"));
        let dup: Vec<u8> = raw.chunks_exact(2).flat_map(|s| [s, s].concat()).collect();
        let files = [fixture_path("pcm16_mono_8k_fmt18.wav")];
        for device in DEVICES {
            let mut b = backend(&ALL, device);
            let Ok(mut res) = HostResources::new() else {
                panic!("res");
            };
            res.engine.set_channel_policy(ChannelPolicy::DuplicateMono);
            let mut lines = Vec::new();
            let r = play_with(
                &mut b,
                &mut res,
                &mut Files(&files),
                PlayControl {
                    on_line: &mut |l: &str| lines.push(l.to_string()),
                    stop: None,
                    commands: None,
                    first: 0,
                    on_status: None,
                },
            );
            let all = lines.join("\n");
            assert!(r.software_path_intact() && !r.strict_path_intact(), "{all}");
            assert_eq!(r.channels_altered, vec![0]);
            assert_eq!(r.exit_code(), 0);
            assert!(
                all.contains("曲目 1 声道已改变：单声道 [FC] 复制为 [FL FR]（明确开启；样本值未改变，不混音）"),
                "{all}"
            );
            let text = r.summary();
            assert!(
                text.contains("声道已改变") && text.contains("不是严格路径"),
                "{text}"
            );
            assert!(!text.contains("软件路径无损"), "{text}");
            // 协商记录写源文件的实际格式，并注明按 2 声道请求设备。
            assert!(
                text.contains("源格式：8000 Hz / 1 声道 / 16 位有效（单声道复制到左右两路：按 2 声道 [FL FR] 请求设备）"),
                "{text}"
            );
            // 设备打开之后才报告声道已改变。
            let opened = all.find("设备流已打开").unwrap_or(usize::MAX);
            let altered = all.find("曲目 1 声道已改变").unwrap_or(0);
            assert!(opened < altered, "{all}");
            let outs = b.outputs();
            assert_eq!(outs.len(), 1);
            assert_eq!(outs[0].format.channels(), 2);
            check_output(&outs[0], &dup);

            // 默认（严格）：同一文件按 [FC] 播放，字节与 data 块相同。
            let mut b = backend(&ALL, device);
            let (r, outs) = run(&mut b, &["pcm16_mono_8k_fmt18.wav"]);
            assert!(
                r.strict_path_intact() && r.channels_altered.is_empty(),
                "{r:?}"
            );
            assert_eq!(outs[0].format.channels(), 1);
            check_output(&outs[0], &raw);
            assert!(!r.summary().contains("声道已改变"));
        }
    }

    /// 文件开头的 ID3v2 标签：跳过后设备字节与无标签文件相同，实时输出写明跳过的字节数。
    #[test]
    fn leading_id3_tag_is_skipped_and_reported() {
        let wav = fixture("pcm16_stereo_44k.wav");
        // ID3v2.3 标签头 + 声明长度 300（syncsafe 02 2C）的帧区。
        let mut file = vec![b'I', b'D', b'3', 3, 0, 0, 0, 0, 0x02, 0x2C];
        file.resize(310, 0);
        file.extend_from_slice(&wav);
        let path = std::env::temp_dir().join(format!("qybit-id3-{}.wav", std::process::id()));
        assert!(std::fs::write(&path, &file).is_ok());
        let files = [path.clone()];
        let mut b = backend(&ALL, FakeDevice::Packet);
        let Ok(mut res) = HostResources::new() else {
            panic!("res");
        };
        let mut lines = Vec::new();
        let r = play_with(
            &mut b,
            &mut res,
            &mut Files(&files),
            PlayControl {
                on_line: &mut |l: &str| lines.push(l.to_string()),
                stop: None,
                commands: None,
                first: 0,
                on_status: None,
            },
        );
        let _ = std::fs::remove_file(&path);
        let all = lines.join("\n");
        assert!(r.finished && r.software_path_intact(), "{all}");
        assert!(
            all.contains("已跳过文件开头的 ID3v2 标签（310 字节"),
            "{all}"
        );
        let outs = b.outputs();
        assert_eq!(outs.len(), 1);
        check_output(&outs[0], &data_chunk(&wav));
    }

    /// W1：修正按曲目写入报告（不改样本：设备字节等于独立参考）；R5 截断写明缺少的字节数；
    /// 结论只说软件路径无损，不说文件完好。
    #[test]
    fn summary_reports_repairs_and_data_short() {
        let mut b = backend(&ALL, FakeDevice::Packet);
        let (r, outs) = run(
            &mut b,
            &["fix_riff_short.wav", "fix_data_partial_frame.wav"],
        );
        let s = r.summary();
        assert_eq!(r.repairs.len(), 2, "{s}");
        assert!(
            s.contains("曲目 1 按规则修正文件结构：RIFF 声明长度"),
            "{s}"
        );
        assert!(
            s.contains("曲目 2 按规则修正文件结构：丢弃数据末尾不足一帧的 2 字节（样本未改动）"),
            "{s}"
        );
        assert!(s.contains("软件路径无损，但这不说明文件本身完好"), "{s}");
        assert_eq!(r.exit_code(), 0, "{s}");
        // 期望字节取 ffmpeg 第二参考，按独立公式放置（16 位源 → 设备 16/16）。
        let mut want = place(&ffmpeg_reference("fix_riff_short.wav", 16), 16, 16);
        want.extend(place(
            &ffmpeg_reference("fix_data_partial_frame.wav", 16),
            16,
            16,
        ));
        assert!(!want.is_empty());
        assert_eq!(outs.len(), 1, "{s}");
        check_output(&outs[0], &want);

        let mut b = backend(&ALL, FakeDevice::Packet);
        let (r, _) = run(&mut b, &["trunc_data_short_frame_boundary.wav"]);
        let s = r.summary();
        assert!(
            s.contains("WAV 错误明细：文件比 data 声明的 400 字节短 280 字节"),
            "{s}"
        );
        assert!(r.repairs.is_empty(), "{s}");
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
    #[cfg(feature = "flac")]
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

    #[cfg(feature = "flac")]
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
    #[cfg(feature = "flac")]
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

    /// B1：WAV-only 构建（`--no-default-features`）中 FLAC 曲目按“不支持”拒绝并说明原因；
    /// 之前的 WAV 曲目照常完整播放。
    #[cfg(not(feature = "flac"))]
    #[test]
    fn wav_only_build_rejects_flac_with_reason() {
        let wav = data_chunk(&fixture("pcm16_stereo_44k.wav"));
        for device in DEVICES {
            let mut b = backend(&ALL, device);
            let (r, outs) = run(
                &mut b,
                &["pcm16_stereo_44k.wav", "cli_16_stereo_44k_default.flac"],
            );
            assert_eq!(
                r.fault,
                Some((
                    1,
                    EngineFault::Core(Fault::Decode(DecodeError::Unsupported))
                )),
                "{device:?} {r:?}"
            );
            assert_eq!(
                r.messages,
                ["本构建未启用 FLAC 解码器，按不支持拒绝"],
                "{device:?}"
            );
            assert_eq!(outs.len(), 1);
            check_output(&outs[0], &wav);
        }
    }

    // —— GUI T2：播放控制命令 ——

    /// 44.1 kHz 双声道 16 位、每个样本不同（`salt` 区分曲目）的 WAV；返回路径与 data 字节。
    fn distinct_track(dir: &std::path::Path, salt: u32, frames: usize) -> (PathBuf, Vec<u8>) {
        let mut data = Vec::with_capacity(frames * 4);
        for i in 0..frames as u32 * 2 {
            let x = i
                .wrapping_mul(2_654_435_761)
                .wrapping_add(salt.wrapping_mul(0x9E37_79B9));
            data.extend_from_slice(&((x >> 16) as u16).to_le_bytes());
        }
        let mut f = Vec::with_capacity(44 + data.len());
        f.extend_from_slice(b"RIFF");
        f.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        f.extend_from_slice(b"WAVEfmt ");
        f.extend_from_slice(&16u32.to_le_bytes());
        f.extend_from_slice(&1u16.to_le_bytes());
        f.extend_from_slice(&2u16.to_le_bytes());
        f.extend_from_slice(&44_100u32.to_le_bytes());
        f.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
        f.extend_from_slice(&4u16.to_le_bytes());
        f.extend_from_slice(&16u16.to_le_bytes());
        f.extend_from_slice(b"data");
        f.extend_from_slice(&(data.len() as u32).to_le_bytes());
        f.extend_from_slice(&data);
        let path = dir.join(format!("track{salt}.wav"));
        if let Err(e) = std::fs::write(&path, &f) {
            panic!("{e}");
        }
        (path, data)
    }

    fn track_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qybit-ctl-{}-{tag}", std::process::id()));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            panic!("{e}");
        }
        dir
    }

    /// 播放并在每次状态快照时调用 `on`（可发送命令）；返回报告、输出与全部状态快照。
    fn run_ctl<N: Negotiator<Attempt = TestAttempt>>(
        backend: &mut FakeBackend<N>,
        files: &[PathBuf],
        mut on: impl FnMut(&Status, &std::sync::mpsc::Sender<Command>),
    ) -> (PlaybackReport<TestAttempt>, Vec<FakeOutput>, Vec<Status>) {
        let Ok(mut res) = HostResources::new() else {
            panic!("res");
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let mut seen = Vec::new();
        let report = play_with(
            backend,
            &mut res,
            &mut Files(files),
            PlayControl {
                on_line: &mut |_| {},
                stop: None,
                commands: Some(&rx),
                first: 0,
                on_status: Some(&mut |s: &Status| {
                    seen.push(*s);
                    on(s, &tx);
                }),
            },
        );
        assert!(res.consumer.is_some(), "消费端须归还");
        (report, backend.outputs(), seen)
    }

    /// 去掉插入的静音后与期望一致：期望是输出中插入零字节后的子序列；返回插入的字节数。
    fn without_silence(rendered: &[u8], expected: &[u8]) -> usize {
        let (mut j, mut inserted) = (0usize, 0usize);
        for &b in rendered {
            if j < expected.len() && b == expected[j] {
                j += 1;
            } else {
                assert_eq!(b, 0, "插入的字节须为静音");
                inserted += 1;
            }
        }
        assert_eq!(j, expected.len(), "有效字节不完整");
        inserted
    }

    const TENTH: u64 = 4_410;

    /// 不可定位的音源（如管道）：读取照常，定位失败。
    struct Pipe<'a>(&'a [PathBuf]);

    struct PipeReader(File);

    impl Read for PipeReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl Seek for PipeReader {
        fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
            Err(io::Error::from(ErrorKind::Unsupported))
        }
    }

    impl TrackSource for Pipe<'_> {
        type Reader = PipeReader;

        fn count(&self) -> u32 {
            self.0.len() as u32
        }

        fn open(&mut self, track: u32) -> io::Result<PipeReader> {
            File::open(&self.0[track as usize]).map(PipeReader)
        }
    }

    /// 播放并在状态快照时调用 `on`；返回报告、输出、实时文字与状态快照。
    fn run_source<S: TrackSource>(
        backend: &mut FakeBackend,
        source: &mut S,
        mut on: impl FnMut(&Status, &std::sync::mpsc::Sender<Command>),
    ) -> (
        PlaybackReport<TestAttempt>,
        Vec<FakeOutput>,
        String,
        Vec<Status>,
    ) {
        let Ok(mut res) = HostResources::new() else {
            panic!("res");
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let (mut seen, mut lines) = (Vec::new(), Vec::new());
        let report = play_with(
            backend,
            &mut res,
            source,
            PlayControl {
                on_line: &mut |l: &str| lines.push(l.to_string()),
                stop: None,
                commands: Some(&rx),
                first: 0,
                on_status: Some(&mut |s: &Status| {
                    seen.push(*s);
                    on(s, &tx);
                }),
            },
        );
        (report, backend.outputs(), lines.join("\n"), seen)
    }

    /// S1：播放中按命令定位。新的输出流从目标帧起与文件数据逐字节一致；状态位置从目标帧算起；
    /// 报告写明按操作定位，播放不计为软件路径完整。音源不可定位时改为顺序解码，结果相同并报告。
    #[test]
    fn seek_command_restarts_at_target_frame() {
        let dir = track_dir("seek");
        let (path, data) = distinct_track(&dir, 9, 26_460);
        let files = [path];
        let target = 13_000u64;
        for seekable in [true, false] {
            let mut b = backend(&ALL, FakeDevice::Clock);
            let mut sent = false;
            let on = |s: &Status, tx: &std::sync::mpsc::Sender<Command>| {
                if !sent && s.position_frames >= TENTH {
                    sent = tx.send(Command::Seek(target)).is_ok();
                }
            };
            let (r, outs, lines, seen) = if seekable {
                run_source(&mut b, &mut Files(&files), on)
            } else {
                run_source(&mut b, &mut Pipe(&files), on)
            };
            assert!(sent);
            assert!(r.finished && r.user_seeks == 1, "{}", r.summary());
            assert!(!r.software_path_intact());
            assert!(r.summary().contains("按操作定位 1 次"), "{}", r.summary());
            assert_eq!(outs.len(), 2);
            check_output(&outs[1], &data[target as usize * 4..]);
            assert!(lines.contains("按操作定位"), "{lines}");
            assert_eq!(r.sequential_seeks, if seekable { vec![] } else { vec![0] });
            assert_eq!(lines.contains("不支持定位"), !seekable, "{lines}");
            // 定位后的状态位置从目标帧算起。
            let after: Vec<u64> = seen
                .iter()
                .skip_while(|s| s.position_frames < target)
                .map(|s| s.position_frames)
                .collect();
            assert!(after.len() > 2, "{seen:?}");
            assert!(after.windows(2).all(|w| w[0] <= w[1]), "{after:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 状态快照的源格式是正在播放的曲目的格式，不是已预读的下一首：严格路径下每个带设备格式的
    /// 快照，源与设备的采样率、声道数都相同（GUI T3 截图核对发现的错误：曾显示下一首的 96 kHz）。
    #[test]
    fn status_source_follows_playing_track() {
        let dir = track_dir("fmt");
        let (first, _) = distinct_track(&dir, 3, 22_050);
        let hires = dir.join("hires.wav");
        assert!(crate::signal::write_test_wav(&hires, 96_000, 2, 24, 48_000).is_ok());
        let mut b = backend(&ALL, FakeDevice::Clock);
        let (r, _, seen) = run_ctl(&mut b, &[first, hires], |_, _| {});
        assert!(r.finished, "{}", r.summary());
        let mut tracks = Vec::new();
        for s in &seen {
            if let (Some(src), Some(dev)) = (s.source, s.device) {
                assert_eq!(src.sample_rate(), dev.sample_rate(), "{s:?}");
                assert_eq!(src.channels(), dev.channels(), "{s:?}");
                tracks.push(s.track);
            }
        }
        assert!(tracks.contains(&0) && tracks.contains(&1), "{tracks:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 连续的短曲目：引擎可以在第 1 首仍在播放时解析到第 9 首以后。状态快照中正在播放的曲目必须
    /// 始终带有源格式（格式记录按仍在播放或排队的曲目保留，不按条数淘汰）。
    #[test]
    fn status_keeps_source_of_playing_track_with_many_short_tracks() {
        let dir = track_dir("short");
        let files: Vec<PathBuf> = (0..20)
            .map(|i| {
                let p = dir.join(format!("short{i:02}.wav"));
                assert!(crate::signal::write_test_wav(&p, 44_100, 2, 16, 300).is_ok());
                p
            })
            .collect();
        let mut b = backend(&ALL, FakeDevice::Clock);
        let (r, _, seen) = run_ctl(&mut b, &files, |_, _| {});
        assert!(r.finished, "{}", r.summary());
        let missing: Vec<&Status> = seen
            .iter()
            .filter(|s| s.active && s.device.is_some() && s.source.is_none())
            .collect();
        assert!(missing.is_empty(), "缺少源格式的快照：{missing:?}");
        assert!(seen.iter().any(|s| s.track >= 9 && s.source.is_some()));
    }

    /// O1 命令：设备不接受单声道。严格路径下打开失败、播放停止；另一次播放在第 1 首（立体声）
    /// 播放中开启单声道复制并跳到第 2 首（单声道），按 [FL FR] 播放，设备字节为每样本两份
    /// （独立参考）；状态快照与报告都记为声道已改变。
    #[test]
    fn mono_to_stereo_command_applies_to_later_tracks() {
        let dir = track_dir("mono");
        let mono = dir.join("mono.wav");
        assert!(crate::signal::write_test_wav(&mono, 44_100, 1, 16, 13_230).is_ok());
        let raw = data_chunk(&std::fs::read(&mono).unwrap_or_default());
        assert_eq!(raw.len(), 13_230 * 2);
        let dup: Vec<u8> = raw.chunks_exact(2).flat_map(|s| [s, s].concat()).collect();
        let (stereo, sdata) = distinct_track(&dir, 7, 26_460);
        let files = [stereo, mono];

        let mut b = FakeBackend::with_negotiator(&ALL, NoMonoNegotiator);
        b.device = FakeDevice::Clock;
        let (r, outs, _) = run_ctl(&mut b, &files, |_, _| {});
        assert!(
            r.channels_altered.is_empty() && !r.finished,
            "{}",
            r.summary()
        );
        assert!(r.summary().contains("设备不接受单声道"));
        assert_eq!(outs.len(), 1);
        check_output(&outs[0], &sdata);

        let mut b = FakeBackend::with_negotiator(&ALL, NoMonoNegotiator);
        b.device = FakeDevice::Clock;
        let mut sent = false;
        let (r, outs, seen) = run_ctl(&mut b, &files, |s, tx| {
            if !sent && s.track == 0 && s.position_frames >= TENTH {
                assert_eq!(s.channels_altered, 0);
                sent = tx.send(Command::MonoToStereo(true)).is_ok()
                    && tx.send(Command::Goto(1)).is_ok();
            }
        });
        assert!(sent, "{}", r.summary());
        assert_eq!(r.channels_altered, vec![1], "{}", r.summary());
        assert!(r.finished && r.user_skips == 1 && !r.strict_path_intact());
        // 第 1 首（被切走）、复制的第 2 首。
        assert_eq!(outs.len(), 2);
        assert_eq!(outs[1].format.channels(), 2);
        check_output(&outs[1], &dup);
        assert_eq!(seen.last().map(|s| s.channels_altered), Some(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 跳曲、下一首、上一首（按真实时间消费的设备，位置可控）：切歌丢弃已入队的音频，
    /// 新的输出流从目标曲目开头逐字节一致；被切走的流未排空、不记欠载。
    #[test]
    fn goto_next_and_prev_restart_from_target_track() {
        let dir = track_dir("skip");
        let tracks: Vec<(PathBuf, Vec<u8>)> =
            (0..3).map(|k| distinct_track(&dir, k, 26_460)).collect();
        let files: Vec<PathBuf> = tracks.iter().map(|t| t.0.clone()).collect();
        let data = |k: usize| tracks[k].1.clone();

        // 第 1 首播到 0.1 秒时跳到第 3 首。
        let mut b = backend(&ALL, FakeDevice::Clock);
        let mut sent = false;
        let (r, outs, _) = run_ctl(&mut b, &files, |s, tx| {
            if !sent && s.track == 0 && s.position_frames >= TENTH {
                sent = tx.send(Command::Goto(2)).is_ok();
            }
        });
        assert!(r.finished && r.user_skips == 1, "{r:?}");
        assert_eq!(outs.len(), 2);
        let first = &outs[0];
        assert_eq!(first.report.end, RenderEnd::Stopped);
        let played = first.report.valid_frames as usize * 4;
        assert!(played >= TENTH as usize * 4 && played < data(0).len());
        assert!(
            first.rendered[..played] == data(0)[..played],
            "被切走的曲目须是其开头"
        );
        assert_eq!(first.report.underrun_events, 0);
        check_output(&outs[1], &data(2));
        assert!(!r.software_path_intact(), "有曲目没有放完");
        assert!(r.summary().contains("按操作切换曲目 1 次"));

        // 下一首，再上一首：上一首相对正在播放的第 2 首，回到第 1 首并播完全部。
        let mut b = backend(&ALL, FakeDevice::Clock);
        let mut stage = 0;
        let (r, outs, _) = run_ctl(&mut b, &files, |s, tx| match stage {
            0 if s.track == 0 && s.position_frames >= TENTH => {
                stage += usize::from(tx.send(Command::Next).is_ok());
            }
            1 if s.track == 1 && s.position_frames >= TENTH => {
                stage += usize::from(tx.send(Command::Prev).is_ok());
            }
            _ => {}
        });
        assert!(r.finished && r.user_skips == 2, "{r:?}");
        assert_eq!(outs.len(), 3);
        let p1 = outs[1].report.valid_frames as usize * 4;
        assert!(p1 >= TENTH as usize * 4 && outs[1].rendered[..p1] == data(1)[..p1]);
        check_output(&outs[2], &[data(0), data(1), data(2)].concat());

        // 停止命令：立即停止、不排空，报告标记中断。
        let mut b = backend(&ALL, FakeDevice::Clock);
        let mut sent = false;
        let (r, outs, seen) = run_ctl(&mut b, &files, |s, tx| {
            if !sent && s.position_frames >= TENTH {
                sent = tx.send(Command::Stop).is_ok();
            }
        });
        assert!(r.interrupted && !r.finished, "{r:?}");
        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0].report.end, RenderEnd::Stopped);
        assert_eq!(
            seen.last().map(|s| s.active),
            Some(false),
            "结束时给出非活动状态"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 暂停与恢复（按真实时间消费的设备）：暂停期间设备得到静音，单独计数、不算欠载；恢复后
    /// 有效字节逐字节一致，播放记录仍可描述为软件路径完整。状态快照反映暂停。
    #[test]
    fn pause_and_resume_keep_bytes_and_count_silence_separately() {
        let dir = track_dir("pause");
        let (path, data) = distinct_track(&dir, 7, 26_460);
        let mut b = backend(&ALL, FakeDevice::Clock);
        let mut stage = 0;
        let (r, outs, seen) = run_ctl(&mut b, &[path], |s, tx| match stage {
            0 if s.position_frames >= TENTH => {
                stage += usize::from(tx.send(Command::Pause).is_ok());
            }
            1 if s.paused && s.paused_frames >= 2 * TENTH => {
                stage += usize::from(tx.send(Command::Resume).is_ok());
            }
            _ => {}
        });
        assert_eq!(stage, 2);
        assert!(r.software_path_intact(), "{}", r.summary());
        assert_eq!(outs.len(), 1);
        let o = &outs[0];
        assert!(o.report.paused_frames >= 2 * TENTH, "{:?}", o.report);
        assert_eq!(o.report.underrun_events, 0);
        assert_eq!(o.report.valid_frames as usize * 4, data.len());
        let inserted = without_silence(&o.rendered, &data);
        // 插入的静音 = 暂停静音 + 尾部填充 + 排空。
        let r2 = &o.report;
        assert_eq!(
            inserted as u64,
            (r2.paused_frames + r2.tail_fill_frames + r2.drain_frames) * 4
        );
        assert!(seen.iter().any(|s| s.paused && s.device.is_some()));
        assert!(r.summary().contains("暂停：送出静音"), "{}", r.summary());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 设备启动前暂停：预填完成后不启动，恢复后才启动；没有暂停静音，输出逐字节一致。
    #[test]
    fn pause_before_start_defers_device_start() {
        let dir = track_dir("prestart");
        let (path, data) = distinct_track(&dir, 9, 44_100);
        for device in DEVICES {
            let mut b = backend(&ALL, device);
            let mut stage = 0;
            let (r, outs, seen) = run_ctl(&mut b, std::slice::from_ref(&path), |s, tx| {
                match stage {
                    0 => stage += usize::from(tx.send(Command::Pause).is_ok()),
                    // 状态快照节流 100 ms：此时预填早已完成，设备仍未启动。
                    1 if s.paused && s.device.is_some() => {
                        stage += usize::from(tx.send(Command::Resume).is_ok());
                    }
                    _ => {}
                }
            });
            assert_eq!(stage, 2, "{device:?}");
            assert!(r.software_path_intact(), "{device:?} {}", r.summary());
            assert_eq!(outs.len(), 1);
            assert_eq!(outs[0].report.paused_frames, 0, "{device:?}");
            check_output(&outs[0], &data);
            assert!(seen.iter().any(|s| s.paused && s.position_frames == 0));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
