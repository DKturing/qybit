//! 界面逻辑（安全代码，Windows 与 Linux 前端共用）：把控件输入转成 `qy-ui` 的操作，把模型变化转成
//! 显示操作。平台前端只负责把 [`Input`] 交进来、执行返回的 [`Op`]，菜单命令、选项、排序、设备列表
//! 与设置的行为在两个平台上相同。

use std::path::PathBuf;

use qy_ui::settings::Settings;
use qy_ui::text::RateLamp;
use qy_ui::{Changes, Deferred, SortKey, Ui, text};

/// 菜单与按钮命令。
pub const ID_PLAY: u16 = 101;
pub const ID_STOP: u16 = 102;
pub const ID_PREV: u16 = 103;
pub const ID_NEXT: u16 = 104;
pub const ID_ADD_FILES: u16 = 105;
pub const ID_ADD_FOLDER: u16 = 106;
pub const ID_REMOVE: u16 = 107;
pub const ID_CLEAR: u16 = 108;
pub const ID_EXIT: u16 = 109;
pub const ID_ABOUT: u16 = 110;
/// 查看菜单：清除日志、显示/隐藏日志区。
pub const ID_CLEAR_LOG: u16 = 111;
pub const ID_TOGGLE_LOG: u16 = 112;
/// 列表菜单（GUI T6）：按添加顺序、文件名、时长、格式排序（再选一次反向），上移、下移选中的行。
pub const ID_SORT_ADDED: u16 = 113;
pub const ID_SORT_NAME: u16 = 114;
pub const ID_SORT_LENGTH: u16 = 115;
pub const ID_SORT_FORMAT: u16 = 116;
pub const ID_MOVE_UP: u16 = 117;
pub const ID_MOVE_DOWN: u16 = 118;
/// 文件菜单：记住播放列表与设置（GUI T6，可选，默认关闭）。
pub const ID_REMEMBER: u16 = 119;
/// 播放菜单：全局媒体键（窗口不在前台时也响应）。
pub const ID_MEDIA_KEYS: u16 = 120;
/// 菜单命令的最大编号。
#[cfg_attr(not(windows), allow(dead_code))]
pub const ID_LAST: u16 = ID_MEDIA_KEYS;
/// 控件。
#[cfg_attr(not(windows), allow(dead_code))]
pub const IDC_LIST: u16 = 201;
#[cfg_attr(not(windows), allow(dead_code))]
pub const IDC_DEVICE: u16 = 202;
#[cfg_attr(not(windows), allow(dead_code))]
pub const IDC_MONO: u16 = 203;
/// 浮点转整数（F3，明确开启的非严格选项）。
#[cfg_attr(not(windows), allow(dead_code))]
pub const IDC_FLOAT: u16 = 210;
#[cfg_attr(not(windows), allow(dead_code))]
pub const IDC_LOG: u16 = 204;
#[cfg_attr(not(windows), allow(dead_code))]
pub const IDC_STATUS: u16 = 205;
#[cfg_attr(not(windows), allow(dead_code))]
pub const IDC_FORMAT: u16 = 206;
/// 进度条（S1 定位）：范围 0–1000（千分比）。
#[cfg_attr(not(windows), allow(dead_code))]
pub const IDC_SEEK: u16 = 207;
/// 设备硬件音量（GUI T5）：滑块范围 0–100 与读数标签。
#[cfg_attr(not(windows), allow(dead_code))]
pub const IDC_VOLUME: u16 = 208;
#[cfg_attr(not(windows), allow(dead_code))]
pub const IDC_VOLUME_LABEL: u16 = 209;

/// 播放列表的列：标题与宽度（96 DPI 下的像素）。
pub const COLUMNS: [(&str, i32); 5] = [
    ("", 28),
    ("#", 44),
    ("文件", 380),
    ("时长", 64),
    ("格式", 260),
];

/// 排序依据对应的列（列标题显示箭头）。
pub fn sort_column(key: SortKey) -> usize {
    match key {
        SortKey::Added => 1,
        SortKey::Name => 2,
        SortKey::Length => 3,
        SortKey::Format => 4,
    }
}

/// 列对应的排序依据（第 0 列是播放标记，不排序）。
pub fn column_key(col: usize) -> Option<SortKey> {
    match col {
        1 => Some(SortKey::Added),
        2 => Some(SortKey::Name),
        3 => Some(SortKey::Length),
        4 => Some(SortKey::Format),
        _ => None,
    }
}

/// 键盘媒体键（GUI T6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Media {
    PlayPause,
    #[cfg_attr(not(windows), allow(dead_code))]
    Play,
    Pause,
    Stop,
    Next,
    Prev,
}

/// 控件输入。
#[derive(Debug)]
pub enum Input {
    Command(u16),
    /// 双击或回车：从该行开始播放。
    Activate(usize),
    /// 删除选中的行。
    Delete(Vec<usize>),
    /// 设备下拉框的选择（0 为默认设备）。
    Device(usize),
    Mono(bool),
    /// 浮点转整数复选框（F3）。
    Float(bool),
    /// 拖放或对话框选中的文件与文件夹。
    Paths(Vec<PathBuf>),
    /// 进度条松开：按千分比定位（S1）。
    Seek(u32),
    /// 键盘 ←/→：相对当前位置前后移动若干秒（S1）。
    SeekBy(i64),
    /// 后台线程有新事件。
    Notify,
    /// 窗口大小变化（最小化或恢复，L1）或可见性变化（另一个虚拟桌面，GUI T6）：只按可见性补画
    /// 暂存的变化。
    Resized,
    /// 列表菜单的排序命令或点击列标题（GUI T6）：同一依据再来一次时反向。
    Sort(SortKey),
    /// 拖动选中的行到第 `to` 行之前（移动前的位置）。
    Move {
        rows: Vec<usize>,
        to: usize,
    },
    /// Ctrl+↑ / Ctrl+↓：选中的行上移（负）或下移（正）一行。
    MoveBy(i32),
    Media(Media),
    /// 端点变化通知后重新枚举的结果。
    Devices {
        default: Option<Device>,
        endpoints: Vec<Device>,
    },
    /// Win32 边界给出的说明（写入日志区）。
    Note(String),
    /// 重画状态栏与格式行（拖动提示结束后恢复）。
    #[cfg_attr(not(windows), allow(dead_code))]
    Redraw,
}

/// 显示操作（由 Win32 边界执行）。
#[derive(Debug, PartialEq, Eq)]
pub enum Op {
    /// 播放列表行数变化。
    Count(usize),
    /// 重绘播放列表（文字变化）。
    Redraw,
    /// 状态栏（播放位置与计数、保真状态；后者为空时状态栏只有一栏）与格式行（源 → 设备）。
    Status {
        status: String,
        fidelity: String,
        format: String,
    },
    Log(String),
    /// 显示或隐藏日志区（隐藏时播放列表占满）。
    LogVisible(bool),
    PlayLabel(&'static str),
    /// 进度条位置（千分比）；`None` 时不可用（未播放或总长度未知）。
    Seekbar(Option<u32>),
    OpenFiles,
    OpenFolder,
    About,
    Close,
    /// 采样率指示灯（GUI T6）。
    Rate {
        lamp: RateLamp,
        text: String,
    },
    /// 设定列表的选中行（排序、移动之后）。
    Select(Vec<usize>),
    /// 列标题的排序箭头：（列，是否反向）；`None` 时清除。
    SortArrow(Option<(usize, bool)>),
    /// 设备下拉框的全部项与选中项。
    Devices {
        names: Vec<String>,
        selected: usize,
    },
    /// 菜单项的勾选状态。
    Check(u16, bool),
    /// 保存设置（Win32 边界加上窗口位置与日志区高度后写入文件）。
    SaveSettings,
    /// 删除设置文件（关闭“记住”时）。
    DeleteSettings,
    /// 注册或注销全局媒体键。
    MediaKeys(bool),
}

pub struct App {
    ui: Ui,
    /// 窗口最小化时暂存重绘（L1）。
    view: Deferred,
    /// 日志区是否显示。
    log_visible: bool,
    /// 设备下拉框各项对应的端点标识（第 0 项为默认设备）与显示名。
    devices: Vec<(Option<String>, String)>,
    /// 最近一次枚举的默认设备与端点（重建下拉框用）。
    enumerated: (Option<Device>, Vec<Device>),
    /// 下拉框第 0 项的含义。
    default_kind: DefaultDevice,
    /// 当前的列表排序（列标题箭头）；手动调整顺序或加入条目后清除。
    sort: Option<(SortKey, bool)>,
    /// 记住播放列表与设置（GUI T6，默认关闭）。
    remember: bool,
    /// 全局媒体键（默认开启）。
    media_keys: bool,
}

/// 一个输出设备：平台的设备标识（Windows 为端点 ID，Linux 为 `hw:CARD=…,DEV=…`）与显示名。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub name: Option<String>,
}

/// 设备下拉框第 0 项的含义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultDevice {
    /// 系统默认输出设备（Windows：默认渲染端点，播放时再查询）。
    #[cfg_attr(not(windows), allow(dead_code))]
    System,
    /// 没有系统默认设备（Linux 内核直连 ALSA）：第 0 项为“未选择”，播放前须选定设备。
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    None,
}

/// 设备的显示名。
fn device_name(e: &Device) -> String {
    e.name.clone().unwrap_or_else(|| e.id.clone())
}

impl App {
    pub fn new(
        ui: Ui,
        default_kind: DefaultDevice,
        default: Option<Device>,
        endpoints: Vec<Device>,
    ) -> Self {
        let mut app = Self {
            default_kind,
            ui,
            view: Deferred::default(),
            log_visible: true,
            devices: Vec::new(),
            enumerated: (None, Vec::new()),
            sort: None,
            remember: false,
            media_keys: true,
        };
        app.set_devices(default, endpoints);
        app
    }

    /// 重建设备下拉框的项。选定的设备不在列表中（已拔出或停用）时保留该项并标明“未连接”，
    /// 不自动换到别的设备。返回选中项的位置。
    fn set_devices(&mut self, default: Option<Device>, endpoints: Vec<Device>) -> usize {
        self.enumerated = (default, endpoints);
        let (default, endpoints) = (&self.enumerated.0, &self.enumerated.1);
        let old = std::mem::take(&mut self.devices);
        self.devices.push((
            None,
            match (self.default_kind, default) {
                (DefaultDevice::System, Some(d)) => format!("默认输出设备（{}）", device_name(d)),
                (DefaultDevice::System, None) => "默认输出设备".to_string(),
                (DefaultDevice::None, _) => "（请选择输出设备）".to_string(),
            },
        ));
        self.devices.extend(
            endpoints
                .iter()
                .map(|e| (Some(e.id.clone()), device_name(e))),
        );
        let current = self.ui.model.device.clone();
        if let Some(id) = current {
            if let Some(i) = self.devices.iter().position(|d| d.0.as_ref() == Some(&id)) {
                return i;
            }
            let name = old.iter().find(|d| d.0.as_ref() == Some(&id)).map_or_else(
                || id.clone(),
                |d| d.1.trim_end_matches("（未连接）").to_string(),
            );
            self.devices.push((Some(id), format!("{name}（未连接）")));
            return self.devices.len() - 1;
        }
        0
    }

    fn devices_op(&self, selected: usize) -> Op {
        Op::Devices {
            names: self.device_names(),
            selected,
        }
    }

    /// 是否记住播放列表与设置。
    pub fn remember(&self) -> bool {
        self.remember
    }

    /// 要保存的设置（窗口位置与日志区高度由 Win32 边界填写）。不含单声道复制与浮点转整数：这两项
    /// 是明确开启的非严格选项，每次启动默认关闭。
    pub fn settings(&self) -> Settings {
        Settings {
            files: self
                .ui
                .model
                .entries
                .iter()
                .map(|e| e.path.clone())
                .collect(),
            device: self.ui.model.device.clone(),
            log_visible: self.log_visible,
            sort: self.sort,
            media_keys: self.media_keys,
            ..Settings::default()
        }
    }

    /// 启动时应用上次保存的设置（在 [`App::initial`] 之前调用）：开启“记住”，恢复设备、排序
    /// 箭头、媒体键与日志区显示，按保存的顺序加入播放列表。`ignored` 为设置文件中忽略的行数。
    pub fn restore(&mut self, s: &Settings, ignored: usize) {
        self.remember = true;
        self.ui.model.device.clone_from(&s.device);
        // 按最近一次枚举重建：选定的设备不在列表中时标明“未连接”。
        let (default, endpoints) = std::mem::take(&mut self.enumerated);
        self.set_devices(default, endpoints);
        self.sort = s.sort;
        self.media_keys = s.media_keys;
        self.log_visible = s.log_visible;
        self.ui.add_paths(s.files.clone());
        let mut note = format!("已恢复上次的播放列表（{} 首）与设置", s.files.len());
        if ignored > 0 {
            note.push_str(&format!("；设置文件中有 {ignored} 行无法识别，已忽略"));
        }
        self.ui.note(&note);
    }

    /// 当前选定的输出设备（端点标识；`None` 为默认设备）。
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn device_id(&self) -> Option<String> {
        self.ui.model.device.clone()
    }

    pub fn device_names(&self) -> Vec<String> {
        self.devices.iter().map(|d| d.1.clone()).collect()
    }

    /// 处理输入；`selected` 为播放列表当前选中的行（按位置升序），`visible` 为窗口当前是否可见
    /// （未最小化）。不可见时模型照常更新，显示变化暂存，恢复时一次重绘（L1）。
    pub fn handle(&mut self, input: Input, selected: &[usize], visible: bool) -> Vec<Op> {
        let shown = self.view.set_visible(visible);
        let mut ops = self.ops(shown);
        let before = self.ui.model.entries.len();
        let changes = match input {
            Input::Command(ID_PLAY) => self.ui.play_pause(selected.first().copied()),
            Input::Command(ID_STOP) => self.ui.stop(),
            Input::Command(ID_PREV) => {
                self.ui.prev();
                Changes::default()
            }
            Input::Command(ID_NEXT) => {
                self.ui.next();
                Changes::default()
            }
            Input::Command(ID_ADD_FILES) => {
                ops.push(Op::OpenFiles);
                return ops;
            }
            Input::Command(ID_ADD_FOLDER) => {
                ops.push(Op::OpenFolder);
                return ops;
            }
            Input::Command(ID_REMOVE) => self.ui.remove(selected),
            Input::Command(ID_CLEAR) => self.ui.clear(),
            Input::Command(ID_CLEAR_LOG) => self.ui.clear_log(),
            Input::Command(ID_TOGGLE_LOG) => {
                self.log_visible = !self.log_visible;
                ops.push(Op::LogVisible(self.log_visible));
                return ops;
            }
            Input::Command(ID_EXIT) => {
                ops.push(Op::Close);
                return ops;
            }
            Input::Command(ID_ABOUT) => {
                ops.push(Op::About);
                return ops;
            }
            Input::Command(ID_SORT_ADDED) => self.sort_by(SortKey::Added, selected, &mut ops),
            Input::Command(ID_SORT_NAME) => self.sort_by(SortKey::Name, selected, &mut ops),
            Input::Command(ID_SORT_LENGTH) => self.sort_by(SortKey::Length, selected, &mut ops),
            Input::Command(ID_SORT_FORMAT) => self.sort_by(SortKey::Format, selected, &mut ops),
            Input::Command(ID_MOVE_UP) => self.move_by(-1, selected, &mut ops),
            Input::Command(ID_MOVE_DOWN) => self.move_by(1, selected, &mut ops),
            Input::Command(ID_REMEMBER) => {
                self.remember = !self.remember;
                ops.push(Op::Check(ID_REMEMBER, self.remember));
                ops.push(if self.remember {
                    Op::SaveSettings
                } else {
                    Op::DeleteSettings
                });
                return ops;
            }
            Input::Command(ID_MEDIA_KEYS) => {
                self.media_keys = !self.media_keys;
                ops.push(Op::MediaKeys(self.media_keys));
                return ops;
            }
            Input::Command(_) => Changes::default(),
            Input::Activate(i) => self.ui.play_from(i),
            Input::Delete(rows) => self.ui.remove(&rows),
            Input::Device(i) => {
                self.ui.model.device = self.devices.get(i).and_then(|d| d.0.clone());
                Changes::default()
            }
            Input::Mono(on) => self.ui.set_mono_to_stereo(on),
            Input::Float(on) => self.ui.set_float_to_int(on),
            Input::Paths(paths) => self.ui.add_paths(paths),
            Input::Seek(permille) => {
                self.ui.seek_permille(permille);
                Changes::default()
            }
            Input::SeekBy(seconds) => {
                self.ui.seek_by(seconds);
                Changes::default()
            }
            Input::Notify => self.ui.pump(),
            Input::Resized => Changes::default(),
            Input::Sort(key) => self.sort_by(key, selected, &mut ops),
            Input::Move { rows, to } => self.move_rows(&rows, to, &mut ops),
            Input::MoveBy(d) => self.move_by(d, selected, &mut ops),
            Input::Media(m) => self.media(m, selected),
            Input::Devices { default, endpoints } => {
                let n = endpoints.len();
                let sel = self.set_devices(default, endpoints);
                ops.push(self.devices_op(sel));
                let missing = self
                    .devices
                    .get(sel)
                    .is_some_and(|d| d.1.ends_with("（未连接）"));
                let mut note = format!("输出设备列表已更新：{n} 个设备");
                if missing {
                    note.push_str(
                        "；选定的设备未连接，播放时会报告无法打开（不会自动换到别的设备）",
                    );
                }
                self.ui.note(&note)
            }
            Input::Note(text) => self.ui.note(&text),
            Input::Redraw => Changes {
                status: true,
                ..Default::default()
            },
        };
        // 新加入的条目排在末尾，列表不再按排序依据排列：清除列标题箭头。
        if self.ui.model.entries.len() != before && self.sort.is_some() {
            self.sort = None;
            ops.push(Op::SortArrow(None));
        }
        let changes = self.view.filter(changes);
        ops.extend(self.ops(changes));
        ops
    }

    /// 排序；同一依据连续两次时反向。保持选中的条目选中。
    fn sort_by(&mut self, key: SortKey, selected: &[usize], ops: &mut Vec<Op>) -> Changes {
        let desc = matches!(self.sort, Some((k, false)) if k == key);
        let ids: Vec<u64> = selected
            .iter()
            .filter_map(|&i| self.ui.model.entries.get(i).map(|e| e.id))
            .collect();
        let c = self.ui.sort(key, desc);
        self.sort = Some((key, desc));
        let mut rows: Vec<usize> = ids
            .iter()
            .filter_map(|&id| self.ui.model.index_of(id))
            .collect();
        rows.sort_unstable();
        ops.push(Op::SortArrow(Some((sort_column(key), desc))));
        ops.push(Op::Select(rows));
        c
    }

    fn move_rows(&mut self, rows: &[usize], to: usize, ops: &mut Vec<Op>) -> Changes {
        let (c, at) = self.ui.move_rows(rows, to);
        if c.list && self.sort.is_some() {
            self.sort = None;
            ops.push(Op::SortArrow(None));
        }
        if !at.is_empty() {
            ops.push(Op::Select(at));
        }
        c
    }

    /// 选中的行整体上移或下移一行（已在顶端或底端时不动）。
    fn move_by(&mut self, d: i32, selected: &[usize], ops: &mut Vec<Op>) -> Changes {
        let (Some(&first), Some(&last)) = (selected.first(), selected.last()) else {
            return Changes::default();
        };
        let len = self.ui.model.entries.len();
        let to = if d < 0 {
            match first.checked_sub(1) {
                Some(t) => t,
                None => return Changes::default(),
            }
        } else if last + 1 < len {
            last + 2
        } else {
            return Changes::default();
        };
        self.move_rows(selected, to, ops)
    }

    fn media(&mut self, m: Media, selected: &[usize]) -> Changes {
        let active = self.ui.model.playing.as_ref().is_some_and(|p| !p.stopping);
        let paused = self.ui.model.is_paused();
        match m {
            Media::PlayPause => self.ui.play_pause(selected.first().copied()),
            Media::Play if !active => self.ui.play_pause(selected.first().copied()),
            Media::Play if paused => {
                self.ui.resume();
                Changes::default()
            }
            Media::Pause if active && !paused => {
                self.ui.pause();
                Changes::default()
            }
            Media::Stop => self.ui.stop(),
            Media::Next => {
                self.ui.next();
                Changes::default()
            }
            Media::Prev => {
                self.ui.prev();
                Changes::default()
            }
            Media::Play | Media::Pause => Changes::default(),
        }
    }

    fn ops(&self, c: Changes) -> Vec<Op> {
        let m = &self.ui.model;
        let mut ops = Vec::new();
        if c.list {
            ops.push(Op::Count(m.entries.len()));
            ops.push(Op::Redraw);
        }
        if c.status || c.session {
            ops.push(Op::Status {
                status: text::status_line(m),
                fidelity: text::fidelity_line(m),
                format: text::format_line(m),
            });
            ops.push(Op::PlayLabel(self.play_label()));
            let (lamp, text) = text::rate_lamp(m);
            ops.push(Op::Rate { lamp, text });
            ops.push(Op::Seekbar(self.ui.model.playing_position().map(
                |(pos, total, _)| {
                    u32::try_from(u128::from(pos) * 1000 / u128::from(total.max(1))).unwrap_or(1000)
                },
            )));
        }
        if c.log {
            let lines: Vec<&str> = m.log.iter().map(String::as_str).collect();
            ops.push(Op::Log(lines.join("\r\n")));
        }
        ops
    }

    /// 启动时的完整显示（含设备下拉框、菜单勾选、排序箭头、日志区与全局媒体键）。
    pub fn initial(&self) -> Vec<Op> {
        let selected = self
            .devices
            .iter()
            .position(|d| d.0 == self.ui.model.device)
            .unwrap_or(0);
        let mut ops = vec![
            self.devices_op(selected),
            Op::Check(ID_REMEMBER, self.remember),
            Op::SortArrow(self.sort.map(|(k, d)| (sort_column(k), d))),
            Op::LogVisible(self.log_visible),
            Op::MediaKeys(self.media_keys),
        ];
        ops.extend(self.ops(Changes {
            list: true,
            status: true,
            log: true,
            session: true,
        }));
        ops
    }

    /// 拖动进度条时的预览文字（不定位）。
    pub fn seek_preview(&self, permille: u32) -> Option<String> {
        let (_, total, rate) = self.ui.model.playing_position()?;
        let frame =
            u64::try_from(u128::from(total) * u128::from(permille.min(1000)) / 1000).ok()?;
        Some(format!(
            "松开后定位到 {} / {}",
            text::duration(frame, rate),
            text::duration(total, rate)
        ))
    }

    fn play_label(&self) -> &'static str {
        match &self.ui.model.playing {
            Some(p) if !p.stopping => {
                if self.ui.model.is_paused() {
                    "继续"
                } else {
                    "暂停"
                }
            }
            _ => "播放",
        }
    }

    /// 播放列表单元格文字。
    pub fn cell(&self, row: usize, col: usize) -> String {
        let m = &self.ui.model;
        let Some(e) = m.entries.get(row) else {
            return String::new();
        };
        match col {
            0 if m.playing_entry() == Some(e.id) => {
                if m.is_paused() { "‖" } else { "▶" }.to_string()
            }
            0 => String::new(),
            1 => (row + 1).to_string(),
            _ => {
                let name = e.path.file_name().map_or_else(
                    || e.path.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                let cols = text::columns(&e.probe, &name);
                match col {
                    2 => cols[0].clone(),
                    3 => cols[1].clone(),
                    _ => cols[2].clone(),
                }
            }
        }
    }
}

/// “关于”对话框的文字（含许可）。
pub fn about_text() -> String {
    format!(
        "QyBit {}\n\n本地无损播放器：WAV / FLAC / AIFF，自有解码；{}。\n严格路径不重采样、不改动样本、不混音；\
         欠载与错误如实报告。播放成功只说明软件路径与协商正常，不证明设备端 bit-perfect。\n\n\
         许可：MIT 或 Apache-2.0\n界面：{}",
        env!("CARGO_PKG_VERSION"),
        if cfg!(windows) {
            "WASAPI 独占输出"
        } else {
            "ALSA hw 独占输出（内核接口）"
        },
        if cfg!(windows) {
            "Windows 原生控件"
        } else {
            "自绘（X11）"
        }
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use qy_player::testing::FakeBackend;
    use qy_ui::settings::Settings;
    use qy_ui::{SortKey, Ui};

    use super::{
        App, DefaultDevice, Device, ID_MEDIA_KEYS, ID_REMEMBER, ID_SORT_NAME, Input, Media, Op,
    };

    /// 没有输出设备的界面模型（只测界面逻辑，不播放）。
    fn app(endpoints: &[(&str, &str)]) -> App {
        let ui = Ui::new(
            |_d: Option<&str>| -> Result<FakeBackend, String> {
                Err("测试中没有设备".into())
            },
            Arc::new(|| {}),
        );
        let eps = endpoints
            .iter()
            .map(|(id, name)| Device {
                id: (*id).into(),
                name: Some((*name).into()),
            })
            .collect();
        App::new(ui, DefaultDevice::System, None, eps)
    }

    fn devices(ops: &[Op]) -> Option<(Vec<String>, usize)> {
        ops.iter().find_map(|o| match o {
            Op::Devices { names, selected } => Some((names.clone(), *selected)),
            _ => None,
        })
    }

    #[test]
    fn sort_toggles_direction_and_arrow() {
        let mut a = app(&[]);
        let ops = a.handle(Input::Command(ID_SORT_NAME), &[], true);
        assert!(ops.contains(&Op::SortArrow(Some((2, false)))), "{ops:?}");
        let ops = a.handle(Input::Sort(SortKey::Name), &[], true);
        assert!(ops.contains(&Op::SortArrow(Some((2, true)))), "{ops:?}");
        let ops = a.handle(Input::Sort(SortKey::Length), &[], true);
        assert!(ops.contains(&Op::SortArrow(Some((3, false)))), "{ops:?}");
        // 加入条目后列表不再按依据排列：箭头清除。
        let dir = std::env::temp_dir();
        let ops = a.handle(Input::Paths(vec![dir.join("qy-app-test.wav")]), &[], true);
        assert!(ops.contains(&Op::SortArrow(None)), "{ops:?}");
        assert_eq!(a.settings().sort, None);
    }

    #[test]
    fn move_by_stops_at_the_ends() {
        let mut a = app(&[]);
        let files: Vec<PathBuf> = (0..3)
            .map(|k| PathBuf::from(format!("/m/{k}.wav")))
            .collect();
        a.handle(Input::Paths(files), &[], true);
        let ops = a.handle(Input::MoveBy(-1), &[0], true);
        assert!(
            !ops.iter().any(|o| matches!(o, Op::Select(_))),
            "已在顶端：不动"
        );
        let ops = a.handle(Input::MoveBy(1), &[0, 1], true);
        assert!(ops.contains(&Op::Select(vec![1, 2])), "{ops:?}");
        let ops = a.handle(Input::MoveBy(1), &[1, 2], true);
        assert!(
            !ops.iter().any(|o| matches!(o, Op::Select(_))),
            "已在底端：不动"
        );
        let order: Vec<PathBuf> = a.settings().files;
        assert_eq!(order[0], PathBuf::from("/m/2.wav"));
    }

    #[test]
    fn missing_selected_device_is_kept_and_marked() {
        let mut a = app(&[("id-a", "设备 A"), ("id-b", "设备 B")]);
        a.handle(Input::Device(2), &[], true);
        assert_eq!(a.device_id().as_deref(), Some("id-b"));
        let ops = a.handle(
            Input::Devices {
                default: None,
                endpoints: vec![Device {
                    id: "id-a".into(),
                    name: Some("设备 A".into()),
                }],
            },
            &[],
            true,
        );
        let (names, sel) = devices(&ops).unwrap_or_default();
        assert_eq!(names[sel], "设备 B（未连接）");
        assert_eq!(a.device_id().as_deref(), Some("id-b"), "不自动换设备");
        // 设备重新连接后恢复原名。
        let ops = a.handle(
            Input::Devices {
                default: None,
                endpoints: vec![Device {
                    id: "id-b".into(),
                    name: Some("设备 B".into()),
                }],
            },
            &[],
            true,
        );
        let (names, sel) = devices(&ops).unwrap_or_default();
        assert_eq!((names[sel].as_str(), sel), ("设备 B", 1));
    }

    #[test]
    fn no_system_default_device() {
        let ui = Ui::new(
            |_d: Option<&str>| -> Result<FakeBackend, String> {
                Err("测试中没有设备".into())
            },
            Arc::new(|| {}),
        );
        let dev = Device {
            id: "hw:CARD=A,DEV=0".into(),
            name: Some("声卡 A".into()),
        };
        let a = App::new(ui, DefaultDevice::None, Some(dev.clone()), vec![dev]);
        assert_eq!(a.device_names(), ["（请选择输出设备）", "声卡 A"]);
        assert_eq!(a.device_id(), None);
    }

    #[test]
    fn remember_and_media_keys_toggle() {
        let mut a = app(&[]);
        assert!(!a.remember());
        let ops = a.handle(Input::Command(ID_REMEMBER), &[], true);
        assert_eq!(ops, vec![Op::Check(ID_REMEMBER, true), Op::SaveSettings]);
        let ops = a.handle(Input::Command(ID_REMEMBER), &[], true);
        assert_eq!(ops, vec![Op::Check(ID_REMEMBER, false), Op::DeleteSettings]);
        let ops = a.handle(Input::Command(ID_MEDIA_KEYS), &[], true);
        assert_eq!(ops, vec![Op::MediaKeys(false)]);
        assert!(!a.settings().media_keys);
    }

    #[test]
    fn restore_applies_saved_settings() {
        let mut a = app(&[("id-a", "设备 A")]);
        let s = Settings {
            files: vec![PathBuf::from("/r/a.wav"), PathBuf::from("/r/b.flac")],
            device: Some("id-gone".into()),
            log_visible: false,
            sort: Some((SortKey::Length, true)),
            media_keys: false,
            ..Settings::default()
        };
        a.restore(&s, 2);
        assert!(a.remember());
        let ops = a.initial();
        let (names, sel) = devices(&ops).unwrap_or_default();
        assert_eq!(names[sel], "id-gone（未连接）");
        assert!(ops.contains(&Op::Check(ID_REMEMBER, true)));
        assert!(ops.contains(&Op::SortArrow(Some((3, true)))));
        assert!(ops.contains(&Op::LogVisible(false)));
        assert!(ops.contains(&Op::MediaKeys(false)));
        assert!(ops.contains(&Op::Count(2)));
        let log = ops.iter().find_map(|o| match o {
            Op::Log(t) => Some(t.clone()),
            _ => None,
        });
        assert!(log.is_some_and(
            |t| t.contains("已恢复上次的播放列表（2 首）") && t.contains("2 行无法识别")
        ),);
        // 保存的设置不含两个非严格选项（每次启动默认关闭）。
        let back = a.settings();
        assert_eq!(back.files, s.files);
        assert_eq!(back.device, s.device);
    }

    #[test]
    fn media_play_and_pause_follow_state() {
        let mut a = app(&[]);
        a.handle(Input::Paths(vec![PathBuf::from("/p/a.wav")]), &[], true);
        // 未播放时“暂停”不做事，“播放”开始播放。
        let ops = a.handle(Input::Media(Media::Pause), &[], true);
        assert!(!ops.iter().any(|o| matches!(o, Op::PlayLabel(_))));
        let ops = a.handle(Input::Media(Media::Play), &[], true);
        assert!(ops.contains(&Op::PlayLabel("暂停")), "{ops:?}");
    }
}
