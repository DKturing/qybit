//! 界面逻辑（安全代码）：把控件输入转成 `qy-ui` 的操作，把模型变化转成显示操作。

use std::path::PathBuf;

use qy_host_wasapi::endpoints::EndpointInfo;
use qy_ui::{Changes, Deferred, Ui, text};

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
/// 控件。
pub const IDC_LIST: u16 = 201;
pub const IDC_DEVICE: u16 = 202;
pub const IDC_MONO: u16 = 203;
/// 浮点转整数（F3，明确开启的非严格选项）。
pub const IDC_FLOAT: u16 = 210;
pub const IDC_LOG: u16 = 204;
pub const IDC_STATUS: u16 = 205;
pub const IDC_FORMAT: u16 = 206;
/// 进度条（S1 定位）：范围 0–1000（千分比）。
pub const IDC_SEEK: u16 = 207;
/// 设备硬件音量（GUI T5）：滑块范围 0–100 与读数标签。
pub const IDC_VOLUME: u16 = 208;
pub const IDC_VOLUME_LABEL: u16 = 209;

/// 播放列表的列：标题与宽度（96 DPI 下的像素）。
pub const COLUMNS: [(&str, i32); 5] = [
    ("", 28),
    ("#", 44),
    ("文件", 380),
    ("时长", 64),
    ("格式", 260),
];

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
    /// 窗口大小变化（最小化或恢复，L1）：只按可见性补画暂存的变化。
    Resized,
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
}

pub struct App {
    ui: Ui,
    /// 窗口最小化时暂存重绘（L1）。
    view: Deferred,
    /// 日志区是否显示。
    log_visible: bool,
    /// 设备下拉框各项对应的端点标识（第 0 项为默认设备）与显示名。
    devices: Vec<(Option<String>, String)>,
}

impl App {
    pub fn new(ui: Ui, default: Option<EndpointInfo>, endpoints: Vec<EndpointInfo>) -> Self {
        let name = |e: &EndpointInfo| e.name.clone().unwrap_or_else(|| e.id.clone());
        let mut devices = vec![(
            None,
            match &default {
                Some(d) => format!("默认输出设备（{}）", name(d)),
                None => "默认输出设备".to_string(),
            },
        )];
        devices.extend(endpoints.iter().map(|e| (Some(e.id.clone()), name(e))));
        Self {
            ui,
            view: Deferred::default(),
            log_visible: true,
            devices,
        }
    }

    /// 当前选定的输出设备（端点标识；`None` 为默认设备）。
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
        };
        let changes = self.view.filter(changes);
        ops.extend(self.ops(changes));
        ops
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

    /// 启动时的完整显示。
    pub fn initial(&self) -> Vec<Op> {
        self.ops(Changes {
            list: true,
            status: true,
            log: true,
            session: true,
        })
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
        "QyBit {}\n\n本地无损播放器：WAV / FLAC / AIFF，自有解码；WASAPI 独占输出。\n严格路径不重采样、不改动样本、不混音；\
         欠载与错误如实报告。播放成功只说明软件路径与协商正常，不证明设备端 bit-perfect。\n\n\
         许可：MIT 或 Apache-2.0\n界面：Windows 原生控件",
        env!("CARGO_PKG_VERSION")
    )
}

/// 输出设备的音量状态（GUI T5）。只调节设备在硬件上实现的音量，送到设备的数据不变。
#[derive(Debug, Clone, PartialEq)]
pub enum VolumeView {
    /// 设备在硬件上实现了音量：位置 0.0–1.0、读回的 dB 值、硬件静音。
    Hardware { scalar: f32, db: f32, muted: bool },
    /// 设备没有硬件音量（系统的软件音量对独占流无效，QyBit 也不使用它）。
    NoHardware,
    /// 读不到（无设备、模拟设备、系统接口失败）。
    Unavailable(String),
}

impl VolumeView {
    /// 标签文字。
    pub fn text(&self) -> String {
        match self {
            Self::Hardware { db, muted, .. } => {
                let mute = if *muted { "（已静音）" } else { "" };
                format!("设备硬件音量 {db:.1} dB{mute}")
            }
            Self::NoHardware => "设备没有硬件音量，请在 DAC 或功放上调节".to_string(),
            Self::Unavailable(why) => format!("设备音量不可用：{why}"),
        }
    }

    /// 滑块位置（0–100）；不可调节时为 `None`（滑块不可用）。
    pub fn position(&self) -> Option<u32> {
        match self {
            Self::Hardware { scalar, .. } if scalar.is_finite() => {
                Some((scalar.clamp(0.0, 1.0) * 100.0).round() as u32)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::VolumeView;

    #[test]
    fn volume_view_text_and_position() {
        let v = VolumeView::Hardware {
            scalar: 0.456,
            db: -12.34,
            muted: false,
        };
        assert_eq!(v.text(), "设备硬件音量 -12.3 dB");
        assert_eq!(v.position(), Some(46));
        let m = VolumeView::Hardware {
            scalar: 1.5,
            db: 0.0,
            muted: true,
        };
        assert_eq!(m.text(), "设备硬件音量 0.0 dB（已静音）");
        assert_eq!(m.position(), Some(100));
        let nan = VolumeView::Hardware {
            scalar: f32::NAN,
            db: 0.0,
            muted: false,
        };
        assert_eq!(nan.position(), None);
        assert_eq!(VolumeView::NoHardware.position(), None);
        assert!(VolumeView::NoHardware.text().contains("没有硬件音量"));
        let u = VolumeView::Unavailable("模拟设备".into());
        assert_eq!(u.text(), "设备音量不可用：模拟设备");
        assert_eq!(u.position(), None);
    }
}
