//! 界面模型与界面线程上的操作。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use qy_core::Fault;
use qy_core::engine::EngineFault;
use qy_player::report::{AttemptRecord, fault_text};
use qy_player::{Backend, Command, PlaybackReport, Status};

use crate::probe::{self, Probe, ProbeRequest};
use crate::session::{self, Notify, PlayRequest, UiEvent};

/// 日志保留的行数。
pub const LOG_LINES: usize = 500;

/// 播放列表条目。`id` 在整个程序运行期间唯一，播放会话以它对应条目（列表增删不影响正在播放的会话）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: u64,
    pub path: PathBuf,
    pub probe: Probe,
}

/// 一次播放的结束结果（来自播放器报告）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// 完整报告文字（与 CLI 的播放报告相同）。
    pub summary: String,
    pub finished: bool,
    pub interrupted: bool,
    pub device_error: bool,
    /// 软件路径完整：全部播完、无欠载、无错误（不证明设备端 bit-perfect）。
    pub software_intact: bool,
    /// 严格路径完整：软件路径完整且没有按选项改变声道布局。
    pub strict_intact: bool,
    pub underrun_events: u64,
    pub user_skips: u32,
    /// 播放前即失败的原因（如设备不可用）。
    pub error: Option<String>,
    /// 播放因曲目失败而停止时的原因（如设备不支持该曲目的格式），含曲目编号。
    pub stopped: Option<String>,
}

impl Outcome {
    pub(crate) fn from_report<A: AttemptRecord>(r: &PlaybackReport<A>) -> Self {
        Self {
            summary: r.summary(),
            finished: r.finished,
            interrupted: r.interrupted,
            device_error: r.device_error,
            software_intact: r.software_path_intact(),
            strict_intact: r.strict_path_intact(),
            underrun_events: r.underrun_events(),
            user_skips: r.user_skips,
            error: None,
            stopped: r.fault.map(|(track, fault)| {
                let n = u64::from(track) + 1;
                match (fault, r.open_failures.last()) {
                    (EngineFault::Core(Fault::Configure), Some(f)) => {
                        format!("曲目 {n} 无法打开输出设备：{}", f.message)
                    }
                    _ => format!("曲目 {n} 失败：{}", fault_text(&fault)),
                }
            }),
        }
    }

    pub(crate) fn failed(reason: String) -> Self {
        Self {
            summary: format!("错误：{reason}\n"),
            finished: false,
            interrupted: false,
            device_error: false,
            software_intact: false,
            strict_intact: false,
            underrun_events: 0,
            user_skips: 0,
            error: Some(reason),
            stopped: None,
        }
    }
}

/// 当前播放会话（界面一侧）。
#[derive(Debug)]
pub struct Playing {
    pub session: u64,
    /// 会话的曲目（条目编号，按播放顺序）。
    pub entries: Vec<u64>,
    /// 会话开始前收到的命令（会话开始后按顺序发出）。
    pending: Vec<Command>,
    commands: Option<Sender<Command>>,
    /// 最近的状态快照。
    pub status: Option<Status>,
    /// 已请求停止。
    pub stopping: bool,
    /// 已发出的定位命令数与最近一次的目标帧（S1）。状态快照的 `user_seeks` 赶上之前，播放位置按
    /// 目标帧显示：定位命令在播放线程执行前到达的旧快照不把进度条与时间拉回原位置。
    seek_hold: Option<(u32, u64)>,
}

/// 界面模型。
#[derive(Debug, Default)]
pub struct Model {
    pub entries: Vec<Entry>,
    pub playing: Option<Playing>,
    /// 最近一次播放的结束结果。
    pub last: Option<Outcome>,
    /// 实时进度文字（最多 [`LOG_LINES`] 行）。
    pub log: VecDeque<String>,
    /// 输出设备（平台端点标识）；`None` 为默认设备。
    pub device: Option<String>,
    /// 单声道复制到左右两路（tasks/O1.md；明确开启，默认关闭，报告标为“已改变”）。
    pub mono_to_stereo: bool,
    /// 故障注入（只供模拟设备核对与测试，默认 `None`）：播放线程读取音频文件的速率上限（字节/秒），
    /// 低于曲目码率时产生真实的欠载。
    pub read_limit: Option<u64>,
    /// 后台线程的致命错误。
    pub fatal: Option<String>,
    next_entry: u64,
    next_session: u64,
}

impl Model {
    /// 条目在列表中的位置。
    pub fn index_of(&self, id: u64) -> Option<usize> {
        self.entries.iter().position(|e| e.id == id)
    }

    /// 正在播放的条目编号（尚无状态时为会话的第一首）。
    pub fn playing_entry(&self) -> Option<u64> {
        let p = self.playing.as_ref()?;
        let k = p.status.map_or(0, |s| s.track) as usize;
        p.entries.get(k).copied()
    }

    /// 正在播放的条目信息。
    pub fn playing_info(&self) -> Option<&Entry> {
        self.playing_entry()
            .and_then(|id| self.index_of(id))
            .map(|i| &self.entries[i])
    }

    /// 正在播放的曲目的播放位置与总长度（帧，文件头声明）及采样率（S1）。总长度未知（如 FLAC
    /// 总样本数为 0、WAV 占位长度）或尚无状态时为 `None`。
    /// 已发出、播放线程尚未执行的定位按目标帧计。
    pub fn playing_position(&self) -> Option<(u64, u64, u32)> {
        let p = self.playing.as_ref()?;
        let s = p.status?;
        let Probe::Ready(info) = &self.playing_info()?.probe else {
            return None;
        };
        let total = info.frames.filter(|&n| n > 0)?;
        let pos = match p.seek_hold {
            Some((sent, frame)) if s.user_seeks < sent => frame,
            _ => s.position_frames,
        };
        Some((pos.min(total), total, info.format.sample_rate()))
    }

    pub fn is_paused(&self) -> bool {
        self.playing
            .as_ref()
            .and_then(|p| p.status)
            .is_some_and(|s| s.paused)
    }

    fn push_log(&mut self, text: &str) {
        for line in text.lines() {
            if self.log.len() == LOG_LINES {
                self.log.pop_front();
            }
            self.log.push_back(line.to_string());
        }
    }
}

/// 一次处理事件后哪些部分需要重绘。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Changes {
    /// 播放列表的行（条目增删、文件头读取结果、正在播放标记）。
    pub list: bool,
    pub status: bool,
    pub log: bool,
    /// 播放会话开始或结束。
    pub session: bool,
}

impl Changes {
    pub fn any(&self) -> bool {
        self.list || self.status || self.log || self.session
    }

    fn merge(&mut self, o: Changes) {
        self.list |= o.list;
        self.status |= o.status;
        self.log |= o.log;
        self.session |= o.session;
    }
}

/// 界面线程的操作入口：持有模型与两个后台线程（播放、文件头读取）的通道。
pub struct Ui {
    pub model: Model,
    events: Receiver<UiEvent>,
    play_tx: Option<Sender<PlayRequest>>,
    probe_tx: Option<Sender<ProbeRequest>>,
    threads: Vec<JoinHandle<()>>,
}

impl Ui {
    /// 启动播放线程与读取线程。`make_backend` 在播放线程中按设备标识创建输出后端；`notify` 在
    /// 后台线程每发出一个事件后调用（前端据此安排一次 [`Ui::pump`]）。
    pub fn new<B, F>(make_backend: F, notify: Notify) -> Self
    where
        B: Backend + 'static,
        F: FnMut(Option<&str>) -> Result<B, String> + Send + 'static,
    {
        let (ev_tx, events) = channel();
        let (play_tx, play_rx) = channel();
        let (probe_tx, probe_rx) = channel();
        let (n1, n2) = (notify.clone(), notify);
        let (e1, e2) = (ev_tx.clone(), ev_tx);
        let threads = vec![
            std::thread::Builder::new()
                .name("qybit-play".into())
                .spawn(move || session::run(play_rx, e1, n1, make_backend)),
            std::thread::Builder::new()
                .name("qybit-probe".into())
                .spawn(move || probe::run(probe_rx, e2, n2)),
        ]
        .into_iter()
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
        let mut model = Model::default();
        if threads.len() < 2 {
            model.fatal = Some("无法创建后台线程".into());
        }
        Self {
            model,
            events,
            play_tx: Some(play_tx),
            probe_tx: Some(probe_tx),
            threads,
        }
    }

    /// 加入文件或文件夹（文件夹在读取线程中扫描）。不是音频扩展名的文件忽略。
    pub fn add_paths(&mut self, paths: Vec<PathBuf>) -> Changes {
        let mut files = Vec::new();
        for p in paths {
            if p.is_dir() {
                if let Some(tx) = &self.probe_tx {
                    let _ = tx.send(ProbeRequest::Folder(p));
                }
            } else if probe::is_audio_path(&p) {
                files.push(p);
            }
        }
        self.add_files(files)
    }

    fn add_files(&mut self, files: Vec<PathBuf>) -> Changes {
        if files.is_empty() {
            return Changes::default();
        }
        let mut req = Vec::with_capacity(files.len());
        for path in files {
            let id = self.model.next_entry;
            self.model.next_entry += 1;
            req.push((id, path.clone()));
            self.model.entries.push(Entry {
                id,
                path,
                probe: Probe::Pending,
            });
        }
        if let Some(tx) = &self.probe_tx {
            let _ = tx.send(ProbeRequest::Files(req));
        }
        Changes {
            list: true,
            ..Default::default()
        }
    }

    /// 移除指定位置的条目（不影响正在播放的会话）。
    pub fn remove(&mut self, indices: &[usize]) -> Changes {
        let mut sorted: Vec<usize> = indices.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        for i in sorted.into_iter().rev() {
            if i < self.model.entries.len() {
                self.model.entries.remove(i);
            }
        }
        Changes {
            list: true,
            ..Default::default()
        }
    }

    pub fn clear(&mut self) -> Changes {
        self.model.entries.clear();
        Changes {
            list: true,
            ..Default::default()
        }
    }

    /// 从列表第 `index` 首开始播放。正在播放且该条目属于当前会话时直接跳曲；否则停止当前
    /// 会话（不排空），按当前列表开始新会话。
    pub fn play_from(&mut self, index: usize) -> Changes {
        let Some(entry) = self.model.entries.get(index) else {
            return Changes::default();
        };
        let id = entry.id;
        if let Some(p) = self.model.playing.as_mut()
            && !p.stopping
            && let Some(k) = p.entries.iter().position(|&e| e == id)
        {
            send(p, Command::Goto(u32::try_from(k).unwrap_or(u32::MAX)));
            return Changes::default();
        }
        self.stop_session();
        let session = self.model.next_session;
        self.model.next_session += 1;
        let entries: Vec<u64> = self.model.entries.iter().map(|e| e.id).collect();
        let files = self.model.entries.iter().map(|e| e.path.clone()).collect();
        let req = PlayRequest {
            session,
            files,
            first: u32::try_from(index).unwrap_or(u32::MAX),
            device: self.model.device.clone(),
            mono_to_stereo: self.model.mono_to_stereo,
            read_limit: self.model.read_limit,
        };
        if let Some(tx) = &self.play_tx
            && tx.send(req).is_err()
        {
            self.model.fatal = Some("播放线程已退出".into());
            return Changes::default();
        }
        self.model.playing = Some(Playing {
            session,
            entries,
            pending: Vec::new(),
            commands: None,
            status: None,
            stopping: false,
            seek_hold: None,
        });
        self.model.last = None;
        Changes {
            list: true,
            status: true,
            session: true,
            ..Default::default()
        }
    }

    /// 播放/暂停：未播放时从 `selected`（或第一首）开始；播放中切换暂停与继续。
    pub fn play_pause(&mut self, selected: Option<usize>) -> Changes {
        match self.model.playing.as_mut() {
            Some(p) if !p.stopping => {
                let paused = p.status.is_some_and(|s| s.paused);
                send(
                    p,
                    if paused {
                        Command::Resume
                    } else {
                        Command::Pause
                    },
                );
                Changes::default()
            }
            _ => self.play_from(selected.unwrap_or(0)),
        }
    }

    /// 单声道复制到左右两路（O1，非严格选项）。记为之后新会话的设置；有会话在进行时同时发给
    /// 该会话，对之后开始的曲目生效（含会话内跳回的曲目），正在播放的曲目不变。
    pub fn set_mono_to_stereo(&mut self, on: bool) -> Changes {
        if self.model.mono_to_stereo == on {
            return Changes::default();
        }
        self.model.mono_to_stereo = on;
        if let Some(p) = self.model.playing.as_mut()
            && !p.stopping
        {
            send(p, Command::MonoToStereo(on));
        }
        Changes {
            status: true,
            ..Default::default()
        }
    }

    /// 在正在播放的曲目内定位到第 `frame` 帧（S1）：丢弃已入队的音频，从该帧开始；暂停状态保持；
    /// 等于或超过曲目长度时进入下一首。
    /// 返回是否已发出定位命令（没有正在进行的播放时不发）。
    pub fn seek(&mut self, frame: u64) -> bool {
        if let Some(p) = self.model.playing.as_mut()
            && !p.stopping
        {
            send(p, Command::Seek(frame));
            let sent = p.seek_hold.map_or(0, |h| h.0);
            p.seek_hold = Some((sent.saturating_add(1), frame));
            return true;
        }
        false
    }

    /// 按总长度的千分比定位（进度条）。总长度未知时不做。返回定位目标帧。
    pub fn seek_permille(&mut self, permille: u32) -> Option<u64> {
        let (_, total, _) = self.model.playing_position()?;
        let frame = u128::from(total) * u128::from(permille.min(1000)) / 1000;
        let frame = u64::try_from(frame)
            .unwrap_or(u64::MAX)
            .min(total.saturating_sub(1));
        self.seek(frame).then_some(frame)
    }

    /// 相对当前位置前后移动 `seconds` 秒（键盘 ←/→），不早于曲目开头；越过结尾时进入下一首。
    /// 返回定位目标帧。
    pub fn seek_by(&mut self, seconds: i64) -> Option<u64> {
        let (pos, _, rate) = self.model.playing_position()?;
        let delta = seconds.unsigned_abs().saturating_mul(u64::from(rate));
        let frame = if seconds < 0 {
            pos.saturating_sub(delta)
        } else {
            pos.saturating_add(delta)
        };
        self.seek(frame).then_some(frame)
    }

    pub fn pause(&mut self) {
        if let Some(p) = self.model.playing.as_mut() {
            send(p, Command::Pause);
        }
    }

    pub fn resume(&mut self) {
        if let Some(p) = self.model.playing.as_mut() {
            send(p, Command::Resume);
        }
    }

    pub fn next(&mut self) {
        if let Some(p) = self.model.playing.as_mut() {
            send(p, Command::Next);
        }
    }

    pub fn prev(&mut self) {
        if let Some(p) = self.model.playing.as_mut() {
            send(p, Command::Prev);
        }
    }

    /// 停止（不排空）。
    pub fn stop(&mut self) -> Changes {
        self.stop_session();
        Changes {
            status: true,
            ..Default::default()
        }
    }

    fn stop_session(&mut self) {
        if let Some(p) = self.model.playing.as_mut()
            && !p.stopping
        {
            send(p, Command::Stop);
            p.stopping = true;
        }
    }

    /// 取出并处理已到达的事件（不等待）。
    pub fn pump(&mut self) -> Changes {
        let mut c = Changes::default();
        while let Ok(e) = self.events.try_recv() {
            c.merge(self.apply(e));
        }
        c
    }

    /// 等待第一个事件至多 `timeout`，然后处理所有已到达的事件（测试与无消息循环的前端用）。
    pub fn wait(&mut self, timeout: Duration) -> Changes {
        let mut c = Changes::default();
        if let Ok(e) = self.events.recv_timeout(timeout) {
            c.merge(self.apply(e));
        }
        c.merge(self.pump());
        c
    }

    fn apply(&mut self, e: UiEvent) -> Changes {
        let e = match e {
            UiEvent::Scanned { files } => return self.add_files(files),
            other => other,
        };
        let m = &mut self.model;
        let current = |s: u64, m: &Model| m.playing.as_ref().is_some_and(|p| p.session == s);
        match e {
            UiEvent::Probed { entry, result } => {
                if let Some(i) = m.index_of(entry) {
                    m.entries[i].probe = match result {
                        Ok(info) => Probe::Ready(info),
                        Err(r) => Probe::Failed(r),
                    };
                }
                Changes {
                    list: true,
                    ..Default::default()
                }
            }
            UiEvent::Scanned { .. } => Changes::default(),
            UiEvent::Started { session, commands } => {
                if current(session, m) {
                    if let Some(p) = m.playing.as_mut() {
                        for cmd in p.pending.drain(..) {
                            let _ = commands.send(cmd);
                        }
                        p.commands = Some(commands);
                    }
                } else {
                    // 界面已开始更新的会话：旧会话立即停止。
                    let _ = commands.send(Command::Stop);
                }
                Changes::default()
            }
            UiEvent::Status { session, status } => {
                if !current(session, m) {
                    return Changes::default();
                }
                let mut c = Changes {
                    status: true,
                    ..Default::default()
                };
                if let Some(p) = m.playing.as_mut() {
                    c.list = p.status.map(|s| s.track) != Some(status.track);
                    p.status = Some(status);
                }
                c
            }
            UiEvent::Line { session, text } => {
                if !current(session, m) {
                    return Changes::default();
                }
                m.push_log(&text);
                Changes {
                    log: true,
                    ..Default::default()
                }
            }
            UiEvent::Ended { session, outcome } => {
                if !current(session, m) {
                    return Changes::default();
                }
                m.push_log(&outcome.summary);
                m.playing = None;
                m.last = Some(outcome);
                Changes {
                    list: true,
                    status: true,
                    log: true,
                    session: true,
                }
            }
            UiEvent::Fatal(reason) => {
                m.push_log(&reason);
                m.fatal = Some(reason);
                Changes {
                    status: true,
                    log: true,
                    ..Default::default()
                }
            }
        }
    }
}

/// 向会话发送命令；会话尚未开始时先排队。
fn send(p: &mut Playing, cmd: Command) {
    match &p.commands {
        Some(tx) => {
            let _ = tx.send(cmd);
        }
        None => p.pending.push(cmd),
    }
}

impl Drop for Ui {
    /// 停止当前播放、关闭请求通道并等待后台线程结束（正在读取的文件头读完即退出）。
    fn drop(&mut self) {
        self.stop_session();
        self.play_tx = None;
        self.probe_tx = None;
        // 尚未开始的会话在开始时立即停止；后台线程全部结束后事件通道断开。
        while let Ok(e) = self.events.recv() {
            if let UiEvent::Started { commands, .. } = e {
                let _ = commands.send(Command::Stop);
            }
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}
