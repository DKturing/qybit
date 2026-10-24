//! Linux 前端：自绘界面（docs/gui-linux-prototype.md），与 Windows 前端相同的菜单、选项与操作。
//!
//! - [`x11`]：X11 边界（x11rb 纯 Rust 协议实现，不含 `unsafe`，可 musl 完全静态链接）；
//! - [`paint`]：软件绘制（tiny-skia 形状、cosmic-text 文字）；
//! - [`view`]：主窗口控件的布局、绘制与输入；[`files`]：文件选择面板；
//! - [`audio`]：ALSA hw 设备与播放期间的设备硬件音量（tasks/A4.md）；
//! - 界面逻辑与 Windows 共用（[`crate::app`]），播放在 `qy-ui` 的后台线程中进行（真实核心、
//!   自有解码、pack 与内核直连的 ALSA 独占输出）。
//!
//! 界面线程只处理通道里的消息：X 事件（读取线程转来）、后台线程的通知、定时检查设备列表。

mod audio;
mod files;
mod paint;
mod view;
mod x11;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use qy_ui::settings::{self, Settings};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::ConnectionExt as _;

use crate::app::{App, DefaultDevice, Device, ID_MEDIA_KEYS, Input, Media, Op};
use crate::sim::Sim;
use audio::Volume;
use paint::Canvas;
use view::{Act, ID_COPY_LOG, ID_VOLUME_ENABLE, View};
use x11::{Mods, Protocol, X, keysym};

/// 检查设备列表的间隔（内核直连 ALSA 没有设备变化通知；读声卡控制设备，开销很小）。
const DEVICE_POLL: Duration = Duration::from_secs(2);
/// 从终端启动时检查 Ctrl+C 的间隔。
const INTERRUPT_POLL: Duration = Duration::from_millis(200);

enum Msg {
    X(Event),
    /// 显示服务连接断开。
    Closed,
    /// 后台线程有新事件。
    Notify,
}

struct Front {
    x: X,
    canvas: Canvas,
    view: View,
    app: App,
    /// 播放期间的设备硬件音量；模拟设备模式下为 `None`。
    volume: Option<Arc<Volume>>,
    /// 最近一次读到的设备（标识）。
    device_ids: Vec<String>,
    sim: bool,
    /// 日志与状态同时写到标准错误：模拟设备模式（CI 核对）或环境变量 `QYBIT_GUI_TRACE=1`（诊断）。
    trace: bool,
    /// 已写到标准错误的日志行（只写新增的行）。
    traced: Vec<String>,
    /// 已写到标准错误的状态栏（相同内容不重复写）。
    traced_status: String,
    visible: bool,
    quit: bool,
    /// 已收到、尚未重送的 Expose 区域。
    exposed: Vec<paint::Rect>,
    /// 诊断：本统计周期内单次提交画面（写出并刷新到 X 连接）的最长耗时。网络带宽不足时写入
    /// 被阻塞，这个时间随之变长，画面（含读秒）就会滞后。
    slowest_put: Duration,
}

pub fn run<F: qy_player::Formats>(formats: F) -> ExitCode {
    let sim = Sim::from_env();
    let mut notes = Vec::new();
    let saved = match settings::default_path().map(|p| (settings::load(&p), p)) {
        Some((Ok(p), _)) => p,
        Some((Err(e), path)) => {
            notes.push(format!(
                "无法读取上次保存的设置（{}：{e}），按默认设置启动",
                path.display()
            ));
            None
        }
        None => None,
    };
    let title = sim.as_ref().map_or_else(|| "QyBit".to_string(), Sim::title);
    let x = match X::open(&title, saved.as_ref().and_then(|p| p.settings.window)) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("错误：{e}");
            return ExitCode::FAILURE;
        }
    };
    let (w, h) = match x.conn.get_geometry(x.win).ok().and_then(|c| c.reply().ok()) {
        Some(g) => (u32::from(g.width), u32::from(g.height)),
        None => (1000, 700),
    };
    let mut canvas = Canvas::new(w, h, x.scale);
    if !canvas.has_fonts() {
        eprintln!(
            "错误：系统中没有任何字体，无法显示界面文字。请安装字体（如 Debian / Ubuntu 的 fonts-noto-cjk \
             或 fonts-dejavu-core），或把字体文件放到 ~/.local/share/fonts/ 下，然后重新启动 qybit-gui"
        );
        return ExitCode::FAILURE;
    }
    if !canvas.has_cjk_font() {
        notes.push(
            "系统中没有找到中文字体，界面中的中文可能显示为方块：请安装中文字体（如 Debian / Ubuntu 的 \
             fonts-noto-cjk、Fedora 的 google-noto-sans-cjk-fonts），然后重新启动 qybit-gui"
                .into(),
        );
    }

    let (tx, rx) = mpsc::channel::<Msg>();
    let pending = Arc::new(AtomicBool::new(false));
    let notify: qy_ui::session::Notify = {
        let tx = tx.clone();
        let pending = pending.clone();
        Arc::new(move || {
            if !pending.swap(true, Ordering::AcqRel) {
                let _ = tx.send(Msg::Notify);
            }
        })
    };
    let (ui, sim_files, volume, default_kind, default, devices) = match &sim {
        Some(s) => (
            s.ui(formats, notify.clone()),
            s.files(),
            None,
            DefaultDevice::System,
            Some(s.endpoint()),
            Vec::new(),
        ),
        None => {
            let volume = Volume::new(audio::DEFAULT_PERCENT, true, notify.clone());
            let (devices, err) = audio::devices();
            notes.extend(err);
            (
                qy_ui::Ui::with_formats(
                    formats,
                    audio::make_backend(volume.clone()),
                    notify.clone(),
                ),
                Vec::new(),
                Some(volume),
                DefaultDevice::None,
                None,
                devices,
            )
        }
    };
    let device_ids: Vec<String> = devices.iter().map(|d| d.id.clone()).collect();
    let only_one = devices.len() == 1;
    let mut app = App::new(ui, default_kind, default, devices);
    if let Some(p) = &saved {
        app.restore(&p.settings, p.ignored);
    } else if only_one {
        // 只有一个播放设备且没有保存的选择：直接选中它。
        let _ = app.handle(Input::Device(1), &[], true);
    }
    let mut view = View::new(canvas.scale());
    if let Some(p) = &saved {
        view.log_height = p.settings.log_height;
    }
    let mut f = Front {
        x,
        canvas,
        view,
        app,
        volume,
        device_ids,
        sim: sim.is_some(),
        trace: sim.is_some() || std::env::var("QYBIT_GUI_TRACE").is_ok_and(|v| v == "1"),
        traced: Vec::new(),
        traced_status: String::new(),
        visible: true,
        quit: false,
        exposed: Vec::new(),
        slowest_put: Duration::ZERO,
    };
    for op in f.app.initial() {
        f.exec(op);
    }
    f.view.set_check(
        ID_VOLUME_ENABLE,
        f.volume.as_ref().is_some_and(|v| v.enabled()),
    );
    f.show_volume();
    f.x.spawn_reader(tx, Msg::X, Msg::Closed);
    if f.x.map().is_err() {
        eprintln!("错误：无法显示窗口");
        return ExitCode::FAILURE;
    }
    if let Some(p) = saved.as_ref().and_then(|p| p.settings.window) {
        f.x.move_to(&p);
    }
    for n in notes {
        f.dispatch(Input::Note(n));
    }
    let mut args = sim_files;
    args.extend(std::env::args_os().skip(1).map(PathBuf::from));
    if !args.is_empty() {
        f.dispatch(Input::Paths(args));
    }
    f.repaint();

    // 从终端启动时的 Ctrl+C：与关闭窗口相同地退出（停止播放、恢复设备硬件音量、保存设置）；
    // 再按一次按默认方式结束进程。
    let interrupted = qy_host_alsa::linux::interrupt::install().ok();
    let mut next_poll = Instant::now() + DEVICE_POLL;
    let mut traffic = (Instant::now(), 0u64);
    while !f.quit {
        let wait = next_poll
            .saturating_duration_since(Instant::now())
            .min(INTERRUPT_POLL);
        match rx.recv_timeout(wait) {
            Ok(m) => f.message(m, &pending),
            Err(RecvTimeoutError::Timeout) => {
                if Instant::now() >= next_poll {
                    next_poll = Instant::now() + DEVICE_POLL;
                    f.poll_devices(false);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if interrupted.is_some_and(|i| i.load(Ordering::Acquire)) {
            break;
        }
        // 一次处理完已排队的消息再重绘（连续的鼠标移动、通知合并为一次绘制）。
        while !f.quit {
            match rx.try_recv() {
                Ok(m) => f.message(m, &pending),
                Err(_) => break,
            }
        }
        f.repaint();
        // 诊断：每 10 秒写一次界面更新的数据量与单次提交的最长耗时（经 SSH 的 X11 转发时界面
        // 响应受它们影响）。
        if f.trace && traffic.0.elapsed() >= Duration::from_secs(10) {
            let kib = (f.x.sent_bytes - traffic.1) / 1024;
            eprintln!(
                "界面更新：{:.0} KiB/s，单次提交最长 {} ms",
                kib as f64 / traffic.0.elapsed().as_secs_f64(),
                std::mem::take(&mut f.slowest_put).as_millis()
            );
            traffic = (Instant::now(), f.x.sent_bytes);
        }
    }
    f.save_settings();
    // 释放界面模型：停止播放并等待后台线程结束；播放线程释放后端时恢复设备硬件音量。
    let Front { app, volume, .. } = f;
    drop(app);
    if let Some(v) = volume {
        for n in v.take_notes() {
            eprintln!("{n}");
        }
    }
    ExitCode::SUCCESS
}

impl Front {
    fn message(&mut self, m: Msg, pending: &AtomicBool) {
        match m {
            Msg::X(ev) => self.event(ev),
            Msg::Closed => self.quit = true,
            Msg::Notify => {
                pending.store(false, Ordering::Release);
                self.dispatch(Input::Notify);
                self.volume_notes();
            }
        }
    }

    fn repaint(&mut self) {
        if !self.visible {
            return;
        }
        let Front {
            view, canvas, app, ..
        } = self;
        let rects = view.paint(canvas, &mut |r, c| app.cell(r, c));
        let start = Instant::now();
        for r in rects.iter().chain(&self.exposed) {
            let _ = self.x.put(&self.canvas, *r);
        }
        self.exposed.clear();
        let _ = self.x.flush();
        self.slowest_put = self.slowest_put.max(start.elapsed());
    }

    fn event(&mut self, ev: Event) {
        let ev = match self.x.protocol(ev) {
            Ok(Protocol::Pass(ev)) => ev,
            Ok(Protocol::Close) => {
                self.quit = true;
                return;
            }
            Ok(Protocol::Dropped(paths)) => {
                if !paths.is_empty() {
                    self.dispatch(Input::Paths(paths));
                }
                return;
            }
            Ok(Protocol::Handled) | Err(_) => return,
        };
        let acts = match ev {
            Event::Expose(e) => {
                self.exposed.push(paint::Rect::new(
                    i32::from(e.x),
                    i32::from(e.y),
                    i32::from(e.width),
                    i32::from(e.height),
                ));
                Vec::new()
            }
            Event::ConfigureNotify(e) => {
                let (w, h) = (u32::from(e.width), u32::from(e.height));
                if w != self.canvas.width() as u32 || h != self.canvas.height() as u32 {
                    self.canvas.resize(w, h);
                    self.view.mark_all();
                }
                Vec::new()
            }
            Event::MapNotify(_) => {
                self.visible = true;
                self.view.mark_all();
                // 最小化期间暂存的变化（L1）。
                self.dispatch(Input::Resized);
                Vec::new()
            }
            Event::UnmapNotify(_) => {
                self.visible = false;
                Vec::new()
            }
            Event::ButtonPress(e) => self.view.press(
                &mut self.canvas,
                i32::from(e.event_x),
                i32::from(e.event_y),
                e.detail,
                Mods::from_state(e.state),
            ),
            Event::ButtonRelease(e) => self.view.release(
                &mut self.canvas,
                i32::from(e.event_x),
                i32::from(e.event_y),
                e.detail,
            ),
            Event::MotionNotify(e) => {
                self.view
                    .motion(&mut self.canvas, i32::from(e.event_x), i32::from(e.event_y))
            }
            Event::LeaveNotify(_) => {
                self.view.leave();
                Vec::new()
            }
            Event::FocusOut(_) => {
                self.view.focus_lost();
                Vec::new()
            }
            Event::KeyPress(e) => {
                let mods = Mods::from_state(e.state);
                let sym = self.x.keysym(e.detail, mods.shift);
                let media = match sym {
                    keysym::AUDIO_PLAY => Some(Media::PlayPause),
                    keysym::AUDIO_PAUSE => Some(Media::Pause),
                    keysym::AUDIO_STOP => Some(Media::Stop),
                    keysym::AUDIO_NEXT => Some(Media::Next),
                    keysym::AUDIO_PREV => Some(Media::Prev),
                    _ => None,
                };
                if let Some(m) = media {
                    self.dispatch(Input::Media(m));
                    Vec::new()
                } else {
                    self.view
                        .key(&mut self.canvas, sym, X::keysym_char(sym), mods)
                }
            }
            _ => Vec::new(),
        };
        for a in acts {
            self.act(a);
        }
    }

    fn act(&mut self, a: Act) {
        match a {
            Act::Command(ID_COPY_LOG) => {
                let text = self.view.log_text().replace("\r\n", "\n");
                let lines = text.lines().count();
                let note = match self.x.set_clipboard(text) {
                    Ok(()) => format!("已复制日志（{lines} 行）到剪贴板"),
                    Err(e) => format!("无法复制到剪贴板：{e}"),
                };
                self.view.set_status_override(Some(note));
            }
            Act::Command(ID_VOLUME_ENABLE) => {
                if let Some(v) = &self.volume {
                    let on = !v.enabled();
                    v.set_enabled(on);
                    self.view.set_check(ID_VOLUME_ENABLE, on);
                    let note = if on {
                        "播放时调整设备硬件音量：开启（开始播放时设为音量滑块的百分比，结束后恢复）"
                    } else {
                        "播放时调整设备硬件音量：关闭（不改动设备混音器）"
                    };
                    self.dispatch(Input::Note(note.into()));
                    self.volume_notes();
                    self.show_volume();
                } else {
                    self.dispatch(Input::Note("模拟设备没有硬件音量".into()));
                }
            }
            Act::Command(id) => self.dispatch(Input::Command(id)),
            Act::Activate(i) => self.dispatch(Input::Activate(i)),
            Act::Delete(rows) => self.dispatch(Input::Delete(rows)),
            Act::Device(i) => self.dispatch(Input::Device(i)),
            Act::RefreshDevices => self.poll_devices(true),
            Act::Mono(on) => self.dispatch(Input::Mono(on)),
            Act::Float(on) => self.dispatch(Input::Float(on)),
            Act::SeekPreview(p) => {
                let t = self.app.seek_preview(p);
                self.view.set_status_override(t);
            }
            Act::Seek(p) => self.dispatch(Input::Seek(p)),
            Act::SeekBy(s) => self.dispatch(Input::SeekBy(s)),
            Act::Sort(k) => self.dispatch(Input::Sort(k)),
            Act::Move { rows, to } => self.dispatch(Input::Move { rows, to }),
            Act::MoveBy(d) => self.dispatch(Input::MoveBy(d)),
            Act::Volume(p) => {
                if let Some(v) = &self.volume {
                    v.set_percent(p);
                }
                self.volume_notes();
                self.show_volume();
            }
            Act::Paths(p) => self.dispatch(Input::Paths(p)),
            Act::Cursor(c) => self.x.set_cursor(c),
        }
    }

    fn dispatch(&mut self, input: Input) {
        let selected = self.view.selected();
        let ops = self.app.handle(input, &selected, self.visible);
        for op in ops {
            self.exec(op);
        }
    }

    fn exec(&mut self, op: Op) {
        match op {
            Op::Count(n) => self.view.set_count(n),
            Op::Redraw => self.view.redraw_list(),
            Op::Status {
                status,
                fidelity,
                format,
            } => {
                // 模拟设备模式（CI 据此核对）或 QYBIT_GUI_TRACE=1（诊断）：状态栏的变化同时写到
                // 标准错误。
                if self.trace {
                    let line = format!("状态栏：{status} | {fidelity}");
                    if line != self.traced_status {
                        eprintln!("{line}");
                        self.traced_status = line;
                    }
                }
                self.view.set_status(status, fidelity, format);
                self.show_volume();
            }
            Op::Log(t) => {
                if self.trace {
                    self.trace_log(&t);
                }
                self.view.set_log(t);
            }
            Op::LogVisible(on) => self.view.set_log_visible(on),
            Op::PlayLabel(l) => self.view.set_play_label(l),
            Op::Seekbar(p) => self.view.set_seek(p),
            Op::OpenFiles(filter) => self.view.open_files(false, filter),
            Op::OpenFolder => {
                let filter = self.app.file_filter();
                self.view.open_files(true, filter);
            }
            Op::About(text) => self.view.about(text),
            Op::Close => self.quit = true,
            Op::Rate { lamp, text } => {
                if self.trace {
                    eprintln!("指示灯：{text}");
                }
                self.view.set_rate(lamp, text);
            }
            Op::Select(rows) => self.view.select(&rows),
            Op::SortArrow(a) => self.view.set_sort_arrow(a),
            Op::Devices { names, selected } => self.view.set_devices(names, selected),
            Op::Check(id, on) => self.view.set_check(id, on),
            Op::SaveSettings => {
                let note = match self.write_settings() {
                    Some(Ok((path, 0))) => format!(
                        "已开启“记住播放列表与设置”：退出时保存到 {}（单声道复制与浮点转整数不保存，每次启动默认关闭）",
                        path.display()
                    ),
                    Some(Ok((path, n))) => format!(
                        "已开启“记住播放列表与设置”：保存到 {}；{n} 个路径无法写入设置文件，未保存",
                        path.display()
                    ),
                    Some(Err(e)) => format!("无法保存设置：{e}"),
                    None => return,
                };
                self.dispatch(Input::Note(note));
            }
            Op::DeleteSettings => {
                let note = match settings::default_path() {
                    Some(path) => match settings::remove(&path) {
                        Ok(()) => format!(
                            "已关闭“记住播放列表与设置”：已删除 {}，下次启动使用默认设置",
                            path.display()
                        ),
                        Err(e) => format!("无法删除设置文件 {}：{e}", path.display()),
                    },
                    None => "已关闭“记住播放列表与设置”".into(),
                };
                self.dispatch(Input::Note(note));
            }
            Op::MediaKeys(on) => {
                let busy = if self.sim {
                    0
                } else {
                    self.x.grab_media_keys(on)
                };
                self.view.set_check(ID_MEDIA_KEYS, on);
                let note = match (on, busy) {
                    (false, _) => "全局媒体键：已关闭，媒体键只在 QyBit 窗口激活时响应".into(),
                    (true, 0) => {
                        "全局媒体键：已启用，窗口不在前台时也响应（Wayland 桌面经 XWayland 运行时，\
                                  其他程序在前台时可能收不到）"
                            .into()
                    }
                    (true, n) => format!(
                        "全局媒体键：有 {n} 个已被其他程序占用，这些键只在 QyBit 窗口激活时响应"
                    ),
                };
                self.dispatch(Input::Note(note));
            }
        }
    }

    /// 把新增的日志行写到标准错误（日志最多保留若干行，开头的行会被移除；清除日志后从头写）。
    fn trace_log(&mut self, text: &str) {
        let lines: Vec<String> = text
            .split("\r\n")
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        let old = &self.traced;
        // 找出旧内容的末尾在新内容中的位置：新内容 = 旧内容（可能去掉了开头若干行）+ 新增行。
        let start = (0..=old.len())
            .find(|&k| lines.len() >= old.len() - k && lines[..old.len() - k] == old[k..])
            .map_or(0, |k| old.len() - k);
        for l in &lines[start..] {
            eprintln!("日志：{l}");
        }
        self.traced = lines;
    }

    /// 音量标签与滑块。
    fn show_volume(&mut self) {
        match &self.volume {
            Some(v) => {
                let pos = v.enabled().then(|| v.percent());
                self.view.set_volume(v.label(), pos);
            }
            None => self.view.set_volume("模拟设备，没有音量控制".into(), None),
        }
    }

    /// 音量的说明写入日志。
    fn volume_notes(&mut self) {
        let notes = self
            .volume
            .as_ref()
            .map(|v| v.take_notes())
            .unwrap_or_default();
        for n in notes {
            self.dispatch(Input::Note(n));
        }
        self.show_volume();
    }

    /// 重新读取设备列表；与上次不同（或 `force`）时更新下拉框。
    fn poll_devices(&mut self, force: bool) {
        if self.sim {
            return;
        }
        let (devices, err) = audio::devices();
        let ids: Vec<String> = devices.iter().map(|d| d.id.clone()).collect();
        if ids == self.device_ids && !force {
            return;
        }
        let changed = ids != self.device_ids;
        self.device_ids = ids;
        if changed {
            self.dispatch(Input::Devices {
                default: None::<Device>,
                endpoints: devices,
            });
            if let Some(e) = err {
                self.dispatch(Input::Note(e));
            }
        }
    }

    fn write_settings(&self) -> Option<Result<(PathBuf, usize), String>> {
        if !self.app.remember() {
            return None;
        }
        let mut s: Settings = self.app.settings();
        s.log_height = self.view.log_height;
        s.window = self.x.placement();
        let Some(path) = settings::default_path() else {
            return Some(Err("找不到用户配置目录（HOME）".into()));
        };
        Some(
            settings::save(&path, &s)
                .map(|skipped| (path.clone(), skipped))
                .map_err(|e| format!("{}：{e}", path.display())),
        )
    }

    fn save_settings(&self) {
        if let Some(Err(e)) = self.write_settings() {
            eprintln!("无法保存设置：{e}");
        }
    }
}
