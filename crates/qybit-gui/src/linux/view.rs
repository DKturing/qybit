//! 主窗口的自绘控件：布局、绘制、命中测试与输入处理。外观与操作对照 Windows 前端（原生控件）：
//! 菜单栏（文件 / 播放 / 列表 / 查看 / 帮助）、按钮行与设备下拉框、两个非严格选项、格式行与设备
//! 硬件音量、进度条、播放列表（列标题排序、调整列宽、多选、拖动调整顺序）、可拖动的分隔条、日志区、
//! 三栏状态栏（播放状态、保真状态、采样率指示灯）。
//!
//! 本模块不持有界面模型：输入转成 [`Act`] 交给 [`super::run`]，模型的变化由 [`crate::app::Op`]
//! 经 setter 送回；只重绘变化的区域（[`View::paint`] 返回需要提交的矩形）。

use std::time::{Duration, Instant};

use qy_ui::SortKey;
use qy_ui::text::RateLamp;

use super::files::{FileDialog, FileResult};
use super::paint::{Align, Canvas, Rect};
use super::x11::{Cursor, Mods, keysym};
use crate::app::{
    COLUMNS, ID_ABOUT, ID_ADD_FILES, ID_ADD_FOLDER, ID_CLEAR, ID_CLEAR_LOG, ID_EXIT, ID_MEDIA_KEYS,
    ID_MOVE_DOWN, ID_MOVE_UP, ID_NEXT, ID_PLAY, ID_PREV, ID_REMEMBER, ID_REMOVE, ID_SORT_ADDED,
    ID_SORT_FORMAT, ID_SORT_LENGTH, ID_SORT_NAME, ID_STOP, ID_TOGGLE_LOG, column_key,
};

/// Linux 前端自己的菜单命令（不经界面逻辑）。
pub const ID_COPY_LOG: u16 = 150;
/// “播放”菜单：播放时调整设备硬件音量（tasks/A4.md）。
pub const ID_VOLUME_ENABLE: u16 = 151;

const BG: u32 = 0xf0f0f0;
const WHITE: u32 = 0xffffff;
const TEXT: u32 = 0x1a1a1a;
const DIM: u32 = 0x8a8a8a;
const BORDER: u32 = 0xadadad;
const LIGHT: u32 = 0xe3e3e3;
const HOVER: u32 = 0xe5f1fb;
const HOVER_EDGE: u32 = 0x0078d7;
const PRESSED: u32 = 0xcce4f7;
const SEL: u32 = 0xcce8ff;
const SEL_EDGE: u32 = 0x99d1ff;
const DROP: u32 = 0xfff2b3;
const MENU_HOT: u32 = 0x91c9f7;
const ACCENT: u32 = 0x0078d7;

/// 视图交给前端的动作。
#[derive(Debug)]
pub enum Act {
    Command(u16),
    Activate(usize),
    Delete(Vec<usize>),
    Device(usize),
    /// 打开设备下拉框前重新读取设备列表。
    RefreshDevices,
    Mono(bool),
    Float(bool),
    /// 进度条拖动中的位置（千分比）：状态栏第一栏显示“松开后定位到 …”。
    SeekPreview(u32),
    Seek(u32),
    SeekBy(i64),
    Sort(SortKey),
    Move {
        rows: Vec<usize>,
        to: usize,
    },
    MoveBy(i32),
    Volume(u32),
    Paths(Vec<std::path::PathBuf>),
    Cursor(Cursor),
}

/// 可单独重绘的区域。
#[derive(Debug, Clone, Copy)]
enum Area {
    Menu = 1,
    Toolbar = 2,
    Options = 4,
    Format = 8,
    Seek = 16,
    List = 32,
    Log = 64,
    Status = 128,
    Splitter = 256,
    /// 只有进度条滑块移动（播放进度）：整行重画到画布，只提交滑块经过的一段（`seek_span`），
    /// 经 SSH 的 X11 转发时大幅减少数据量。不在 `ALL` 中：整行重绘时不需要它。
    SeekThumb = 512,
}
const ALL: u16 = 0x1ff;

enum Entry {
    Item(u16, &'static str, &'static str),
    Sep,
}

const MENUS: [(&str, char); 5] = [
    ("文件(F)", 'f'),
    ("播放(P)", 'p'),
    ("列表(L)", 'l'),
    ("查看(V)", 'v'),
    ("帮助(H)", 'h'),
];

fn menu_entries(i: usize) -> Vec<Entry> {
    use Entry::{Item, Sep};
    match i {
        0 => vec![
            Item(ID_ADD_FILES, "添加文件(O)…", ""),
            Item(ID_ADD_FOLDER, "添加文件夹(D)…", ""),
            Sep,
            Item(ID_REMOVE, "移除选中(R)", "Del"),
            Item(ID_CLEAR, "清空列表(C)", ""),
            Sep,
            Item(ID_REMEMBER, "记住播放列表与设置(M)", ""),
            Sep,
            Item(ID_EXIT, "退出(X)", ""),
        ],
        1 => vec![
            Item(ID_PLAY, "播放 / 暂停(P)", "Space"),
            Item(ID_STOP, "停止(S)", ""),
            Item(ID_PREV, "上一首(V)", ""),
            Item(ID_NEXT, "下一首(N)", ""),
            Sep,
            Item(ID_MEDIA_KEYS, "全局媒体键（窗口不在前台时也响应）(K)", ""),
            Item(ID_VOLUME_ENABLE, "播放时调整设备硬件音量(H)", ""),
        ],
        2 => vec![
            Item(ID_SORT_ADDED, "按添加顺序排序(A)", ""),
            Item(ID_SORT_NAME, "按文件名排序(N)", ""),
            Item(ID_SORT_LENGTH, "按时长排序(T)", ""),
            Item(ID_SORT_FORMAT, "按格式排序(F)", ""),
            Sep,
            Item(ID_MOVE_UP, "上移选中(U)", "Ctrl+↑"),
            Item(ID_MOVE_DOWN, "下移选中(D)", "Ctrl+↓"),
        ],
        3 => vec![
            Item(ID_TOGGLE_LOG, "显示日志(L)", ""),
            Item(ID_CLEAR_LOG, "清除日志(E)", ""),
            Item(ID_COPY_LOG, "复制日志(C)", "Ctrl+C"),
        ],
        _ => vec![Item(ID_ABOUT, "关于 QyBit(A)", "")],
    }
}

/// 菜单项文字中括号里的助记字母。
fn mnemonic(label: &str) -> Option<char> {
    let i = label.find('(')?;
    label[i + 1..]
        .chars()
        .next()
        .map(|c| c.to_ascii_lowercase())
}

#[derive(Default, Clone, Copy)]
struct Slider {
    rect: Rect,
    enabled: bool,
    /// 0..=max。
    value: u32,
    max: u32,
}

impl Slider {
    fn track(&self, s: f32) -> (i32, i32) {
        let half = (6.0 * s) as i32;
        (self.rect.x + half, self.rect.right() - half)
    }

    fn value_at(&self, x: i32, s: f32) -> u32 {
        let (a, b) = self.track(s);
        if b <= a {
            return 0;
        }
        let x = x.clamp(a, b);
        (i64::from(x - a) * i64::from(self.max) / i64::from(b - a)) as u32
    }
}

enum Drag {
    None,
    Seek,
    Volume,
    Split {
        offset: i32,
    },
    Column {
        col: usize,
        start_x: i32,
        start_w: i32,
    },
    /// 按下列表行后尚未移动足够距离（区分点击与拖动）。
    RowPress {
        x: i32,
        y: i32,
    },
    Rows {
        rows: Vec<usize>,
        mark: Option<usize>,
    },
    ListThumb {
        offset: i32,
    },
    LogThumb {
        offset: i32,
    },
    Button(usize),
}

enum Popup {
    None,
    Menu { index: usize, hover: Option<usize> },
    Combo { hover: Option<usize>, top: usize },
}

enum Modal {
    None,
    About(String),
    Files(Box<FileDialog>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    List,
    Log,
}

/// 一行日志的排版结果（按日志区宽度折行）。
struct LogLine {
    text: String,
    buf: cosmic_text::Buffer,
    height: i32,
}

pub struct View {
    dirty: u16,
    /// 弹出层或对话框需要重绘。
    overlay_dirty: bool,
    /// 弹出层上次绘制的矩形（关闭时重绘其下的区域）。
    overlay_rect: Rect,
    // 布局（物理像素）
    w: i32,
    h: i32,
    s: f32,
    menu_rect: Rect,
    menu_items: Vec<Rect>,
    buttons: Vec<Rect>,
    combo: Rect,
    mono_rect: Rect,
    float_rect: Rect,
    format_rect: Rect,
    vol_label: Rect,
    seek: Slider,
    volume: Slider,
    list_rect: Rect,
    split_rect: Rect,
    log_rect: Rect,
    status_rect: Rect,
    // 状态
    play_label: &'static str,
    hover_button: Option<usize>,
    pressed_button: Option<usize>,
    devices: Vec<String>,
    device: usize,
    mono: bool,
    float: bool,
    format: String,
    vol_text: String,
    status: String,
    fidelity: String,
    status_override: Option<String>,
    lamp: RateLamp,
    lamp_text: String,
    checks: Vec<(u16, bool)>,
    log_visible: bool,
    /// 用户拖动分隔条设定的日志区高度（96 DPI 像素）；0 为按默认比例。
    pub log_height: u32,
    // 列表
    count: usize,
    selected: Vec<bool>,
    focus_row: usize,
    anchor: usize,
    top: usize,
    col_w: Vec<i32>,
    sort_arrow: Option<(usize, bool)>,
    last_click: Option<(Instant, usize)>,
    focus: Focus,
    // 日志
    log: Vec<LogLine>,
    /// 现有排版所用的宽度。
    log_width: i32,
    /// 日志文字变化后尚未重新排版。
    log_changed: bool,
    log_scroll: i32,
    log_text: String,
    drag: Drag,
    popup: Popup,
    modal: Modal,
    /// 文件对话框上次所在的目录。
    last_dir: Option<std::path::PathBuf>,
    /// 播放进度移动时需要提交的一段（见 `Area::SeekThumb`）。
    seek_span: Option<Rect>,
}

impl View {
    pub fn new(s: f32) -> Self {
        Self {
            dirty: ALL,
            overlay_dirty: false,
            overlay_rect: Rect::default(),
            w: 0,
            h: 0,
            s,
            menu_rect: Rect::default(),
            menu_items: Vec::new(),
            buttons: Vec::new(),
            combo: Rect::default(),
            mono_rect: Rect::default(),
            float_rect: Rect::default(),
            format_rect: Rect::default(),
            vol_label: Rect::default(),
            seek: Slider {
                max: 1000,
                ..Slider::default()
            },
            volume: Slider {
                max: 100,
                ..Slider::default()
            },
            list_rect: Rect::default(),
            split_rect: Rect::default(),
            log_rect: Rect::default(),
            status_rect: Rect::default(),
            play_label: "播放",
            hover_button: None,
            pressed_button: None,
            devices: Vec::new(),
            device: 0,
            mono: false,
            float: false,
            format: String::new(),
            vol_text: String::new(),
            status: String::new(),
            fidelity: String::new(),
            status_override: None,
            lamp: RateLamp::Off,
            lamp_text: String::new(),
            checks: Vec::new(),
            log_visible: true,
            log_height: 0,
            count: 0,
            selected: Vec::new(),
            focus_row: 0,
            anchor: 0,
            top: 0,
            col_w: COLUMNS.iter().map(|c| c.1).collect(),
            sort_arrow: None,
            last_click: None,
            focus: Focus::List,
            log: Vec::new(),
            log_width: 0,
            log_changed: false,
            log_scroll: 0,
            log_text: String::new(),
            drag: Drag::None,
            popup: Popup::None,
            modal: Modal::None,
            last_dir: None,
            seek_span: None,
        }
    }

    fn px(&self, v: i32) -> i32 {
        (v as f32 * self.s).round() as i32
    }

    fn mark(&mut self, a: Area) {
        self.dirty |= a as u16;
    }

    pub fn mark_all(&mut self) {
        self.dirty = ALL;
        self.overlay_dirty = true;
    }

    // ---------- 由界面逻辑的显示操作设置 ----------

    pub fn set_count(&mut self, n: usize) {
        self.count = n;
        self.selected.resize(n, false);
        if self.focus_row >= n {
            self.focus_row = n.saturating_sub(1);
        }
        self.top = self.top.min(self.max_top());
        self.mark(Area::List);
    }

    pub fn redraw_list(&mut self) {
        self.mark(Area::List);
    }

    pub fn set_status(&mut self, status: String, fidelity: String, format: String) {
        if status != self.status || fidelity != self.fidelity {
            self.status = status;
            self.fidelity = fidelity;
            self.mark(Area::Status);
        }
        if format != self.format {
            self.format = format;
            self.mark(Area::Format);
        }
    }

    pub fn set_status_override(&mut self, t: Option<String>) {
        if t != self.status_override {
            self.status_override = t;
            self.mark(Area::Status);
        }
    }

    pub fn set_play_label(&mut self, l: &'static str) {
        if l != self.play_label {
            self.play_label = l;
            self.mark(Area::Toolbar);
        }
    }

    pub fn set_seek(&mut self, pos: Option<u32>) {
        let enabled = pos.is_some();
        let value = if matches!(self.drag, Drag::Seek) {
            self.seek.value
        } else {
            pos.unwrap_or(0)
        };
        if enabled != self.seek.enabled {
            self.seek.enabled = enabled;
            self.seek.value = value;
            self.mark(Area::Seek);
        } else if value != self.seek.value {
            let before = self.thumb_rect(&self.seek);
            self.seek.value = value;
            let after = self.thumb_rect(&self.seek);
            let span = before.union(&after);
            self.seek_span = Some(self.seek_span.map_or(span, |s| s.union(&span)));
            self.mark(Area::SeekThumb);
        }
    }

    /// 滑块所在的一段（含进度填充的两端与滑块本身）。
    fn thumb_rect(&self, sl: &Slider) -> Rect {
        let (a, b) = sl.track(self.s);
        let x = a + if sl.max == 0 {
            0
        } else {
            ((i64::from(b - a) * i64::from(sl.value.min(sl.max))) / i64::from(sl.max)) as i32
        };
        let half = self.px(8);
        Rect::new(x - half, sl.rect.y, 2 * half, sl.rect.h)
    }

    pub fn set_volume(&mut self, text: String, pos: Option<u32>) {
        let enabled = pos.is_some();
        let value = if matches!(self.drag, Drag::Volume) {
            self.volume.value
        } else {
            pos.unwrap_or(0)
        };
        if text != self.vol_text || enabled != self.volume.enabled || value != self.volume.value {
            self.vol_text = text;
            self.volume.enabled = enabled;
            self.volume.value = value;
            self.mark(Area::Format);
        }
    }

    pub fn set_rate(&mut self, lamp: RateLamp, text: String) {
        if lamp != self.lamp || text != self.lamp_text {
            self.lamp = lamp;
            self.lamp_text = text;
            self.mark(Area::Status);
        }
    }

    pub fn set_devices(&mut self, names: Vec<String>, selected: usize) {
        self.devices = names;
        self.device = selected;
        self.mark(Area::Toolbar);
        if matches!(self.popup, Popup::Combo { .. }) {
            self.overlay_dirty = true;
        }
    }

    pub fn set_check(&mut self, id: u16, on: bool) {
        match self.checks.iter_mut().find(|c| c.0 == id) {
            Some(c) => c.1 = on,
            None => self.checks.push((id, on)),
        }
        if matches!(self.popup, Popup::Menu { .. }) {
            self.overlay_dirty = true;
        }
    }

    fn checked(&self, id: u16) -> Option<bool> {
        self.checks.iter().find(|c| c.0 == id).map(|c| c.1)
    }

    pub fn set_log_visible(&mut self, on: bool) {
        self.log_visible = on;
        self.set_check(ID_TOGGLE_LOG, on);
        if !on && self.focus == Focus::Log {
            self.focus = Focus::List;
        }
        self.w = 0; // 重新布局
        self.mark_all();
    }

    pub fn set_sort_arrow(&mut self, a: Option<(usize, bool)>) {
        self.sort_arrow = a;
        self.mark(Area::List);
    }

    pub fn select(&mut self, rows: &[usize]) {
        for s in &mut self.selected {
            *s = false;
        }
        for &r in rows {
            if let Some(s) = self.selected.get_mut(r) {
                *s = true;
            }
        }
        if let Some(&first) = rows.first() {
            self.focus_row = first;
            self.anchor = first;
            self.ensure_visible(first);
        }
        self.mark(Area::List);
    }

    /// 选中的行（按位置升序）。
    pub fn selected(&self) -> Vec<usize> {
        self.selected
            .iter()
            .enumerate()
            .filter_map(|(i, &s)| s.then_some(i))
            .collect()
    }

    pub fn log_text(&self) -> &str {
        &self.log_text
    }

    /// 日志全文（`\r\n` 分行，与 Windows 前端相同）；新内容出现时滚到末尾。
    pub fn set_log(&mut self, text: String) {
        self.log_text = text;
        self.log_changed = true;
        self.log_scroll = i32::MAX;
        self.mark(Area::Log);
    }

    pub fn about(&mut self, text: String) {
        self.modal = Modal::About(text);
        self.popup = Popup::None;
        self.overlay_dirty = true;
    }

    pub fn open_files(&mut self, folder: bool) {
        let dir = self
            .last_dir
            .clone()
            .or_else(|| std::env::var_os("HOME").map(Into::into))
            .unwrap_or_else(|| "/".into());
        self.modal = Modal::Files(Box::new(FileDialog::new(dir, folder)));
        self.popup = Popup::None;
        self.overlay_dirty = true;
    }

    pub fn modal_open(&self) -> bool {
        !matches!(self.modal, Modal::None)
    }

    // ---------- 布局 ----------

    fn layout(&mut self, c: &Canvas) {
        let (w, h) = (c.width(), c.height());
        if w == self.w && h == self.h && self.s == c.scale() {
            return;
        }
        self.w = w;
        self.h = h;
        self.s = c.scale();
        let s = |v: i32| (v as f32 * self.s).round() as i32;
        self.menu_rect = Rect::new(0, 0, w, s(24));
        let bar_y = self.menu_rect.bottom() + s(6);
        let bar_h = s(28);
        let mut x = s(6);
        self.buttons.clear();
        for bw in [72, 72, 72, 72, 96] {
            self.buttons.push(Rect::new(x, bar_y, s(bw), bar_h));
            x += s(bw) + s(6);
        }
        let dev_w = (w - x - s(12)).clamp(0, s(420));
        self.combo = Rect::new(x + s(6), bar_y + s(1), dev_w, bar_h - s(2));
        let opt_y = bar_y + bar_h + s(4);
        let opt_h = s(24);
        self.mono_rect = Rect::new(s(8), opt_y, s(290), opt_h);
        self.float_rect = Rect::new(s(8) + s(300), opt_y, s(240), opt_h);
        let fmt_y = opt_y + opt_h + s(2);
        let fmt_h = s(24);
        let vol_w = s(140);
        let label_w = s(330);
        let vol_x = (w - s(6) - vol_w).max(0);
        let label_x = (vol_x - s(4) - label_w).max(0);
        self.volume.rect = Rect::new(vol_x, fmt_y, vol_w, fmt_h);
        self.vol_label = Rect::new(label_x, fmt_y, label_w, fmt_h);
        self.format_rect = Rect::new(s(8), fmt_y, (label_x - s(16)).max(0), fmt_h);
        let seek_y = fmt_y + fmt_h;
        self.seek.rect = Rect::new(s(2), seek_y, (w - s(4)).max(0), s(26));
        let top = seek_y + s(26) + s(2);
        let status_h = s(24);
        self.status_rect = Rect::new(0, h - status_h, w, status_h);
        let free = (self.status_rect.y - top).max(0);
        if self.log_visible {
            let gap = s(6);
            let want = match self.log_height {
                0 => free / 4,
                v => s(v as i32),
            };
            let max = (free - gap - s(60)).max(0);
            let log_h = want.clamp(s(40).min(max), max);
            let list_h = (free - gap - log_h).max(0);
            self.list_rect = Rect::new(0, top, w, list_h);
            self.split_rect = Rect::new(0, top + list_h, w, gap);
            self.log_rect = Rect::new(0, top + list_h + gap, w, log_h);
        } else {
            self.list_rect = Rect::new(0, top, w, free);
            self.split_rect = Rect::default();
            self.log_rect = Rect::default();
        }
        self.top = self.top.min(self.max_top());
        self.dirty = ALL;
        self.overlay_dirty = true;
    }

    fn header_h(&self) -> i32 {
        self.px(24)
    }

    fn row_h(&self) -> i32 {
        self.px(22).max(1)
    }

    fn rows_rect(&self) -> Rect {
        let r = self.list_rect;
        let hh = self.header_h();
        Rect::new(
            r.x + 1,
            r.y + hh,
            (r.w - 2 - self.px(14)).max(0),
            (r.h - hh - 1).max(0),
        )
    }

    fn list_scroll_rect(&self) -> Rect {
        let rr = self.rows_rect();
        Rect::new(rr.right(), rr.y, self.px(14), rr.h)
    }

    fn visible_rows(&self) -> usize {
        (self.rows_rect().h / self.row_h()).max(0) as usize
    }

    fn max_top(&self) -> usize {
        self.count.saturating_sub(self.visible_rows().max(1))
    }

    fn ensure_visible(&mut self, row: usize) {
        let vis = self.visible_rows().max(1);
        if row < self.top {
            self.top = row;
        } else if row >= self.top + vis {
            self.top = row + 1 - vis;
        }
        self.top = self.top.min(self.max_top());
    }

    fn row_at(&self, y: i32) -> Option<usize> {
        let rr = self.rows_rect();
        if y < rr.y || y >= rr.bottom() {
            return None;
        }
        let i = self.top + ((y - rr.y) / self.row_h()) as usize;
        (i < self.count).then_some(i)
    }

    fn col_x(&self) -> Vec<(i32, i32)> {
        let mut x = self.list_rect.x + 1;
        self.col_w
            .iter()
            .map(|&w| {
                let w = self.px(w);
                let r = (x, w);
                x += w;
                r
            })
            .collect()
    }

    // ---------- 绘制 ----------

    /// 重绘变化的区域，返回需要提交的矩形。
    pub fn paint(
        &mut self,
        c: &mut Canvas,
        cells: &mut dyn FnMut(usize, usize) -> String,
    ) -> Vec<Rect> {
        self.layout(c);
        let mut out = Vec::new();
        let mut d = std::mem::take(&mut self.dirty);
        let thumb_span = self.seek_span.take();
        if d & Area::Seek as u16 != 0 {
            d &= !(Area::SeekThumb as u16);
        }
        let parts: [(Area, Rect); 9] = [
            (Area::Menu, self.menu_rect),
            (
                Area::Toolbar,
                Rect::new(
                    0,
                    self.menu_rect.bottom(),
                    self.w,
                    self.mono_rect.y - self.menu_rect.bottom(),
                ),
            ),
            (
                Area::Options,
                Rect::new(
                    0,
                    self.mono_rect.y,
                    self.w,
                    self.format_rect.y - self.mono_rect.y,
                ),
            ),
            (
                Area::Format,
                Rect::new(0, self.format_rect.y, self.w, self.format_rect.h),
            ),
            (
                Area::Seek,
                Rect::new(
                    0,
                    self.seek.rect.y,
                    self.w,
                    self.list_rect.y - self.seek.rect.y,
                ),
            ),
            (Area::List, self.list_rect),
            (Area::Splitter, self.split_rect),
            (Area::Log, self.log_rect),
            (Area::Status, self.status_rect),
        ];
        if d & Area::SeekThumb as u16 != 0
            && let Some(span) = thumb_span
        {
            let row = Rect::new(
                0,
                self.seek.rect.y,
                self.w,
                self.list_rect.y - self.seek.rect.y,
            );
            c.fill(row, BG);
            let sl = self.seek;
            self.paint_slider(c, &sl);
            out.push(span.intersect(&row));
        }
        for (a, r) in parts {
            if d & a as u16 == 0 || r.is_empty() {
                continue;
            }
            match a {
                Area::Menu => self.paint_menu_bar(c),
                Area::Toolbar => self.paint_toolbar(c, r),
                Area::Options => self.paint_options(c, r),
                Area::Format => self.paint_format(c, r),
                Area::Seek => {
                    c.fill(r, BG);
                    let sl = self.seek;
                    self.paint_slider(c, &sl);
                }
                Area::List => self.paint_list(c, cells),
                Area::Splitter => c.fill(r, BG),
                Area::Log => self.paint_log(c),
                Area::Status => self.paint_status(c),
                Area::SeekThumb => {}
            }
            out.push(r);
        }
        let overlay = !matches!(self.popup, Popup::None) || self.modal_open();
        if overlay && (self.overlay_dirty || !out.is_empty()) {
            let r = self.paint_overlay(c, cells);
            out.push(r);
        }
        self.overlay_dirty = false;
        out
    }

    fn paint_menu_bar(&mut self, c: &mut Canvas) {
        let r = self.menu_rect;
        c.fill(r, WHITE);
        self.menu_items.clear();
        let mut x = self.px(4);
        let open = match self.popup {
            Popup::Menu { index, .. } => Some(index),
            _ => None,
        };
        for (i, (label, _)) in MENUS.iter().enumerate() {
            let w = c.text_width(label) + self.px(16);
            let item = Rect::new(x, r.y, w, r.h);
            if open == Some(i) {
                c.fill(item, PRESSED);
            }
            c.text(item, 0, label, TEXT, Align::Center);
            self.menu_items.push(item);
            x += w;
        }
        c.fill(Rect::new(0, r.bottom() - 1, r.w, 1), LIGHT);
    }

    fn paint_button(&self, c: &mut Canvas, i: usize, r: Rect, label: &str, enabled: bool) {
        let (fill, edge) = if !enabled {
            (0xf4f4f4, 0xcccccc)
        } else if self.pressed_button == Some(i) {
            (PRESSED, 0x005499)
        } else if self.hover_button == Some(i) {
            (HOVER, HOVER_EDGE)
        } else {
            (0xfdfdfd, BORDER)
        };
        c.rounded(r, self.px(3) as f32, fill, Some(edge));
        c.text(
            r,
            self.px(4),
            label,
            if enabled { TEXT } else { DIM },
            Align::Center,
        );
    }

    fn paint_toolbar(&mut self, c: &mut Canvas, area: Rect) {
        c.fill(area, BG);
        let labels = [self.play_label, "停止", "上一首", "下一首", "添加文件…"];
        for (i, (r, l)) in self.buttons.clone().into_iter().zip(labels).enumerate() {
            self.paint_button(c, i, r, l, true);
        }
        // 设备下拉框
        let r = self.combo;
        if r.w > 0 {
            let open = matches!(self.popup, Popup::Combo { .. });
            let hover = self.hover_button == Some(100);
            c.fill(r, if hover || open { HOVER } else { WHITE });
            c.frame(r, if hover || open { HOVER_EDGE } else { BORDER });
            let name = self.devices.get(self.device).cloned().unwrap_or_default();
            let arrow_w = self.px(20);
            c.text(
                Rect::new(r.x, r.y, r.w - arrow_w, r.h),
                self.px(6),
                &name,
                TEXT,
                Align::Left,
            );
            let (ax, ay) = ((r.right() - arrow_w / 2) as f32, (r.y + r.h / 2) as f32);
            let k = 4.0 * self.s;
            c.polygon(
                &[
                    (ax - k, ay - k / 2.0),
                    (ax + k, ay - k / 2.0),
                    (ax, ay + k / 2.0),
                ],
                0x505050,
            );
        }
    }

    fn paint_checkbox(&self, c: &mut Canvas, r: Rect, label: &str, on: bool) {
        let b = self.px(13);
        let bx = Rect::new(r.x, r.y + (r.h - b) / 2, b, b);
        c.fill(bx, if on { ACCENT } else { WHITE });
        c.frame(bx, if on { ACCENT } else { 0x333333 });
        if on {
            let (x, y, k) = (bx.x as f32, bx.y as f32, b as f32);
            c.polyline(
                &[
                    (x + k * 0.22, y + k * 0.52),
                    (x + k * 0.42, y + k * 0.72),
                    (x + k * 0.78, y + k * 0.3),
                ],
                1.6 * self.s,
                WHITE,
            );
        }
        c.text(
            Rect::new(bx.right() + self.px(6), r.y, r.w - b - self.px(6), r.h),
            0,
            label,
            TEXT,
            Align::Left,
        );
    }

    fn paint_options(&self, c: &mut Canvas, area: Rect) {
        c.fill(area, BG);
        self.paint_checkbox(
            c,
            self.mono_rect,
            "单声道复制到左右两路（会改变声道）",
            self.mono,
        );
        self.paint_checkbox(c, self.float_rect, "浮点转整数（会改变样本）", self.float);
    }

    fn paint_format(&self, c: &mut Canvas, area: Rect) {
        c.fill(area, BG);
        c.text(self.format_rect, 0, &self.format, TEXT, Align::Left);
        c.text(self.vol_label, 0, &self.vol_text, TEXT, Align::Right);
        let v = self.volume;
        self.paint_slider(c, &v);
    }

    fn paint_slider(&self, c: &mut Canvas, sl: &Slider) {
        let r = sl.rect;
        let (a, b) = sl.track(self.s);
        let cy = r.y + r.h / 2;
        let th = self.px(4);
        c.fill(Rect::new(a, cy - th / 2, (b - a).max(0), th), 0xd6d6d6);
        if !sl.enabled {
            return;
        }
        let x = a + if sl.max == 0 {
            0
        } else {
            ((i64::from(b - a) * i64::from(sl.value.min(sl.max))) / i64::from(sl.max)) as i32
        };
        c.fill(Rect::new(a, cy - th / 2, (x - a).max(0), th), ACCENT);
        let tw = self.px(8);
        let thh = self.px(18);
        c.rounded(
            Rect::new(x - tw / 2, cy - thh / 2, tw, thh),
            self.px(4) as f32,
            ACCENT,
            None,
        );
    }

    fn paint_list(&mut self, c: &mut Canvas, cells: &mut dyn FnMut(usize, usize) -> String) {
        let r = self.list_rect;
        c.fill(r, WHITE);
        c.frame(r, BORDER);
        // 列标题
        let hh = self.header_h();
        let cols = self.col_x();
        let head = Rect::new(r.x + 1, r.y + 1, r.w - 2, hh - 1);
        c.fill(head, WHITE);
        for (i, &(x, w)) in cols.iter().enumerate() {
            let cell = Rect::new(x, head.y, w, head.h).intersect(&head);
            c.text(cell, self.px(6), COLUMNS[i].0, 0x333333, Align::Left);
            c.fill(
                Rect::new(x + w - 1, head.y + self.px(4), 1, head.h - self.px(8)),
                LIGHT,
            );
            if let Some((col, desc)) = self.sort_arrow
                && col == i
            {
                let (ax, ay) = (
                    (x + w - self.px(12)) as f32,
                    (head.y + head.h / 2) as f32 - 2.0 * self.s,
                );
                let k = 4.0 * self.s;
                let pts = if desc {
                    [(ax - k, ay), (ax + k, ay), (ax, ay + k)]
                } else {
                    [(ax - k, ay + k), (ax + k, ay + k), (ax, ay)]
                };
                c.polygon(&pts, 0x606060);
            }
        }
        c.fill(Rect::new(head.x, head.bottom(), head.w, 1), LIGHT);
        // 行
        let rr = self.rows_rect();
        let rh = self.row_h();
        let mark = match &self.drag {
            Drag::Rows { mark, .. } => *mark,
            _ => None,
        };
        for i in self.top..self.count.min(self.top + self.visible_rows() + 1) {
            let y = rr.y + (i - self.top) as i32 * rh;
            if y >= rr.bottom() {
                break;
            }
            let row = Rect::new(rr.x, y, rr.w, rh).intersect(&rr);
            if mark == Some(i) {
                c.fill(row, DROP);
            } else if self.selected.get(i).copied().unwrap_or(false) {
                c.fill(row, SEL);
            }
            if self.focus == Focus::List && i == self.focus_row && !self.modal_open() {
                c.frame(row, SEL_EDGE);
            }
            for (k, &(x, w)) in cols.iter().enumerate() {
                let cell = Rect::new(x, y, w, rh).intersect(&row);
                let t = cells(i, k);
                if !t.is_empty() {
                    let align = if k == 0 { Align::Center } else { Align::Left };
                    c.text(cell, self.px(6), &t, TEXT, align);
                }
            }
            c.fill(Rect::new(row.x, y + rh - 1, row.w, 1), 0xf3f3f3);
        }
        // 滚动条
        let sb = self.list_scroll_rect();
        self.paint_scrollbar(
            c,
            sb,
            self.top as i64,
            self.visible_rows() as i64,
            self.count as i64,
        );
    }

    fn paint_scrollbar(&self, c: &mut Canvas, r: Rect, pos: i64, page: i64, total: i64) {
        c.fill(r, 0xf6f6f6);
        if let Some(t) = thumb(r, pos, page, total, self.px(20)) {
            c.rounded(t.inset(self.px(3)), self.px(3) as f32, 0xc2c2c2, None);
        }
    }

    // ---------- 日志 ----------

    /// 按日志区宽度排版；文字变化时宽度相同的行沿用原排版（追加日志时只排新行）。
    fn layout_log(&mut self, c: &mut Canvas) {
        let width = (self.log_rect.w - self.px(8) - self.px(16)).max(1);
        if !self.log_changed && width == self.log_width {
            return;
        }
        let reuse = width == self.log_width;
        let mut pool: std::collections::HashMap<String, Vec<LogLine>> =
            std::collections::HashMap::new();
        if reuse {
            for l in std::mem::take(&mut self.log) {
                pool.entry(l.text.clone()).or_default().push(l);
            }
        } else {
            self.log.clear();
        }
        let text = std::mem::take(&mut self.log_text);
        for line in text.split("\r\n").filter(|l| !l.is_empty()) {
            let l = match pool.get_mut(line).and_then(Vec::pop) {
                Some(l) => l,
                None => {
                    let buf = c.wrapped(line, width);
                    let height = buf
                        .layout_runs()
                        .map(|r| (r.line_top + r.line_height).ceil() as i32)
                        .max()
                        .unwrap_or_else(|| c.line_height());
                    LogLine {
                        text: line.to_string(),
                        buf,
                        height,
                    }
                }
            };
            self.log.push(l);
        }
        self.log_text = text;
        self.log_width = width;
        self.log_changed = false;
    }

    fn log_inner(&self) -> Rect {
        self.log_rect.inset(1)
    }

    fn log_view_h(&self) -> i32 {
        (self.log_inner().h - self.px(4)).max(0)
    }

    fn log_total(&self) -> i32 {
        self.log.iter().map(|l| l.height).sum()
    }

    fn log_scroll_rect(&self) -> Rect {
        let inner = self.log_inner();
        Rect::new(inner.right() - self.px(14), inner.y, self.px(14), inner.h)
    }

    fn paint_log(&mut self, c: &mut Canvas) {
        let r = self.log_rect;
        if r.is_empty() {
            return;
        }
        self.layout_log(c);
        c.fill(r, WHITE);
        c.frame(
            r,
            if self.focus == Focus::Log {
                ACCENT
            } else {
                BORDER
            },
        );
        let inner = self.log_inner();
        let view_h = self.log_view_h();
        let total = self.log_total();
        self.log_scroll = self.log_scroll.clamp(0, (total - view_h).max(0));
        let clip = Rect::new(inner.x, inner.y, inner.w - self.px(14), inner.h);
        let x = inner.x + self.px(4);
        let mut y = inner.y + self.px(2) - self.log_scroll;
        for l in &self.log {
            if y + l.height > inner.y && y < inner.bottom() {
                c.draw_wrapped(&l.buf, x, y, clip, TEXT);
            }
            y += l.height;
        }
        let sb = self.log_scroll_rect();
        self.paint_scrollbar(
            c,
            sb,
            i64::from(self.log_scroll),
            i64::from(view_h),
            i64::from(total),
        );
    }

    // ---------- 状态栏 ----------

    fn status_parts(&self) -> [Rect; 3] {
        let r = self.status_rect;
        let lamp_w = self.px(190).min(r.w);
        let rest = r.w - lamp_w;
        let first = if self.fidelity.is_empty() {
            rest
        } else {
            rest / 2
        };
        [
            Rect::new(r.x, r.y, first, r.h),
            Rect::new(r.x + first, r.y, rest - first, r.h),
            Rect::new(r.x + rest, r.y, lamp_w, r.h),
        ]
    }

    fn paint_status(&self, c: &mut Canvas) {
        let r = self.status_rect;
        c.fill(r, BG);
        c.fill(Rect::new(r.x, r.y, r.w, 1), LIGHT);
        let [a, b, l] = self.status_parts();
        let first = self.status_override.as_deref().unwrap_or(&self.status);
        c.text(a, self.px(6), first, TEXT, Align::Left);
        if !self.fidelity.is_empty() {
            c.fill(
                Rect::new(b.x, b.y + self.px(4), 1, b.h - self.px(8)),
                BORDER,
            );
            c.text(b, self.px(6), &self.fidelity, TEXT, Align::Left);
        }
        c.fill(
            Rect::new(l.x, l.y + self.px(4), 1, l.h - self.px(8)),
            BORDER,
        );
        let color = match self.lamp {
            RateLamp::Off => 0x808080,
            RateLamp::Opening => 0xe6a000,
            RateLamp::Native => 0x00a03c,
            RateLamp::Rejected | RateLamp::Mismatch => 0xc81e1e,
        };
        let d = self.px(5) as f32;
        c.dot((l.x + self.px(12)) as f32, (l.y + l.h / 2) as f32, d, color);
        c.text(
            Rect::new(l.x + self.px(22), l.y, l.w - self.px(22), l.h),
            self.px(2),
            &self.lamp_text,
            TEXT,
            Align::Left,
        );
    }

    // ---------- 弹出层与对话框 ----------

    fn paint_overlay(
        &mut self,
        c: &mut Canvas,
        _cells: &mut dyn FnMut(usize, usize) -> String,
    ) -> Rect {
        let prev = self.overlay_rect;
        let r = match &mut self.modal {
            Modal::About(text) => {
                let text = text.clone();
                self.paint_about(c, &text)
            }
            Modal::Files(d) => {
                let area = Rect::new(0, 0, self.w, self.h);
                d.paint(c, area)
            }
            Modal::None => match self.popup {
                Popup::Menu { index, hover } => self.paint_menu(c, index, hover),
                Popup::Combo { hover, top } => self.paint_combo(c, hover, top),
                Popup::None => Rect::default(),
            },
        };
        self.overlay_rect = r;
        r.union(&prev)
    }

    fn menu_box(&self, c: &mut Canvas, index: usize) -> (Rect, Vec<(Rect, Option<u16>)>) {
        let entries = menu_entries(index);
        let item_h = self.px(26);
        let sep_h = self.px(9);
        let mut text_w = 0;
        let mut accel_w = 0;
        for e in &entries {
            if let Entry::Item(_, l, a) = e {
                text_w = text_w.max(c.text_width(l));
                if !a.is_empty() {
                    accel_w = accel_w.max(c.text_width(a));
                }
            }
        }
        let w = self.px(32) + text_w + self.px(32) + accel_w + self.px(16);
        let h: i32 = entries
            .iter()
            .map(|e| match e {
                Entry::Item(..) => item_h,
                Entry::Sep => sep_h,
            })
            .sum::<i32>()
            + self.px(6);
        let x = self.menu_items.get(index).map_or(0, |r| r.x);
        let bx = Rect::new(x.min((self.w - w).max(0)), self.menu_rect.bottom(), w, h);
        let mut y = bx.y + self.px(3);
        let mut items = Vec::new();
        for e in &entries {
            match e {
                Entry::Item(id, ..) => {
                    items.push((
                        Rect::new(bx.x + self.px(2), y, bx.w - self.px(4), item_h),
                        Some(*id),
                    ));
                    y += item_h;
                }
                Entry::Sep => {
                    items.push((Rect::new(bx.x, y, bx.w, sep_h), None));
                    y += sep_h;
                }
            }
        }
        (bx, items)
    }

    fn paint_menu(&mut self, c: &mut Canvas, index: usize, hover: Option<usize>) -> Rect {
        self.paint_menu_bar(c);
        let (bx, items) = self.menu_box(c, index);
        c.fill(bx, 0xf9f9f9);
        c.frame(bx, 0xcccccc);
        let entries = menu_entries(index);
        for (k, ((r, _), e)) in items.iter().zip(&entries).enumerate() {
            match e {
                Entry::Sep => c.fill(
                    Rect::new(r.x + self.px(28), r.y + r.h / 2, r.w - self.px(36), 1),
                    0xd7d7d7,
                ),
                Entry::Item(id, label, accel) => {
                    if hover == Some(k) {
                        c.fill(*r, MENU_HOT);
                    }
                    if self.checked(*id) == Some(true) {
                        let (x, y, k) = (
                            (r.x + self.px(9)) as f32,
                            (r.y + r.h / 2 - self.px(5)) as f32,
                            self.px(10) as f32,
                        );
                        c.polyline(
                            &[
                                (x, y + k * 0.5),
                                (x + k * 0.38, y + k * 0.9),
                                (x + k, y + k * 0.1),
                            ],
                            1.5 * self.s,
                            TEXT,
                        );
                    }
                    c.text(
                        Rect::new(r.x + self.px(30), r.y, r.w - self.px(30), r.h),
                        0,
                        label,
                        TEXT,
                        Align::Left,
                    );
                    if !accel.is_empty() {
                        c.text(*r, self.px(14), accel, 0x555555, Align::Right);
                    }
                }
            }
        }
        bx.union(&self.menu_rect)
    }

    fn combo_rows(&self) -> usize {
        self.devices.len().min(12)
    }

    fn combo_box(&self) -> Rect {
        let item_h = self.px(24);
        Rect::new(
            self.combo.x,
            self.combo.bottom(),
            self.combo.w,
            item_h * self.combo_rows() as i32 + 2,
        )
    }

    fn paint_combo(&mut self, c: &mut Canvas, hover: Option<usize>, top: usize) -> Rect {
        let bx = self.combo_box();
        let item_h = self.px(24);
        c.fill(bx, WHITE);
        c.frame(bx, 0x646464);
        for k in 0..self.combo_rows() {
            let i = top + k;
            let Some(name) = self.devices.get(i) else {
                break;
            };
            let r = Rect::new(bx.x + 1, bx.y + 1 + k as i32 * item_h, bx.w - 2, item_h);
            if hover == Some(i) {
                c.fill(r, ACCENT);
                c.text(r, self.px(6), name, WHITE, Align::Left);
            } else {
                if i == self.device {
                    c.fill(r, SEL);
                }
                c.text(r, self.px(6), name, TEXT, Align::Left);
            }
        }
        bx
    }

    fn about_geom(&self, c: &mut Canvas, text: &str) -> (Rect, Rect, cosmic_text::Buffer) {
        let w = self.px(480).min(self.w - self.px(20)).max(self.px(200));
        let buf = c.wrapped(text, w - self.px(40));
        let text_h = buf
            .layout_runs()
            .map(|r| (r.line_top + r.line_height).ceil() as i32)
            .max()
            .unwrap_or(0);
        let h = self.px(36) + text_h + self.px(60);
        let bx = Rect::new((self.w - w) / 2, ((self.h - h) / 2).max(0), w, h);
        let ok = Rect::new(
            bx.right() - self.px(96),
            bx.bottom() - self.px(42),
            self.px(80),
            self.px(28),
        );
        (bx, ok, buf)
    }

    fn paint_about(&mut self, c: &mut Canvas, text: &str) -> Rect {
        let (bx, ok, buf) = self.about_geom(c, text);
        c.fill(bx, WHITE);
        c.frame(bx, 0x707070);
        let title = Rect::new(bx.x, bx.y, bx.w, self.px(30));
        c.fill(title, BG);
        c.text_bold(title, self.px(12), "关于 QyBit", TEXT, Align::Left);
        c.draw_wrapped(
            &buf,
            bx.x + self.px(20),
            title.bottom() + self.px(8),
            bx,
            TEXT,
        );
        c.rounded(ok, self.px(3) as f32, HOVER, Some(HOVER_EDGE));
        c.text(ok, 0, "确定", TEXT, Align::Center);
        bx
    }

    /// 关闭弹出层或对话框：其下的区域全部重绘。
    fn close_overlay(&mut self) {
        self.popup = Popup::None;
        self.modal = Modal::None;
        self.dirty = ALL;
        self.overlay_dirty = true;
    }

    // ---------- 输入 ----------

    /// 鼠标按下（`button` 1 左键，4/5 滚轮）。
    pub fn press(&mut self, c: &mut Canvas, x: i32, y: i32, button: u8, mods: Mods) -> Vec<Act> {
        let mut acts = Vec::new();
        if button == 4 || button == 5 {
            self.wheel(x, y, if button == 4 { -1 } else { 1 });
            return acts;
        }
        if button != 1 {
            return acts;
        }
        // 对话框
        match &mut self.modal {
            Modal::About(text) => {
                let text = text.clone();
                let (_, ok, _) = self.about_geom(c, &text);
                if ok.contains(x, y) {
                    self.close_overlay();
                }
                return acts;
            }
            Modal::Files(d) => {
                let r = d.press(c, x, y, mods);
                self.files_result(r, &mut acts);
                return acts;
            }
            Modal::None => {}
        }
        // 弹出层：点在其外时关闭并吞掉这次点击
        match self.popup {
            Popup::Menu { index, .. } => {
                if let Some(i) = self.menu_items.iter().position(|r| r.contains(x, y)) {
                    if i == index {
                        self.close_overlay();
                    } else {
                        self.popup = Popup::Menu {
                            index: i,
                            hover: None,
                        };
                        self.dirty = ALL;
                        self.overlay_dirty = true;
                    }
                    return acts;
                }
                let (_, items) = self.menu_box(c, index);
                let hit = items
                    .iter()
                    .find(|(r, _)| r.contains(x, y))
                    .and_then(|(_, id)| *id);
                self.close_overlay();
                if let Some(id) = hit {
                    acts.push(Act::Command(id));
                }
                return acts;
            }
            Popup::Combo { top, .. } => {
                let bx = self.combo_box();
                self.close_overlay();
                if bx.contains(x, y) {
                    let i = top + ((y - bx.y - 1) / self.px(24)).max(0) as usize;
                    if i < self.devices.len() && i != self.device {
                        self.device = i;
                        acts.push(Act::Device(i));
                    }
                }
                return acts;
            }
            Popup::None => {}
        }
        if let Some(i) = self.menu_items.iter().position(|r| r.contains(x, y)) {
            self.popup = Popup::Menu {
                index: i,
                hover: None,
            };
            self.overlay_dirty = true;
            self.mark(Area::Menu);
            return acts;
        }
        if let Some(i) = self.buttons.iter().position(|r| r.contains(x, y)) {
            self.pressed_button = Some(i);
            self.drag = Drag::Button(i);
            self.mark(Area::Toolbar);
            return acts;
        }
        if self.combo.contains(x, y) {
            acts.push(Act::RefreshDevices);
            let top = self
                .device
                .saturating_sub(self.combo_rows().saturating_sub(1));
            self.popup = Popup::Combo {
                hover: Some(self.device),
                top: top.min(self.devices.len().saturating_sub(self.combo_rows())),
            };
            self.overlay_dirty = true;
            self.mark(Area::Toolbar);
            return acts;
        }
        if self.mono_rect.contains(x, y) {
            self.mono = !self.mono;
            self.mark(Area::Options);
            acts.push(Act::Mono(self.mono));
            return acts;
        }
        if self.float_rect.contains(x, y) {
            self.float = !self.float;
            self.mark(Area::Options);
            acts.push(Act::Float(self.float));
            return acts;
        }
        if self.seek.enabled && self.seek.rect.contains(x, y) {
            self.drag = Drag::Seek;
            self.seek.value = self.seek.value_at(x, self.s);
            self.mark(Area::Seek);
            acts.push(Act::SeekPreview(self.seek.value));
            return acts;
        }
        if self.volume.enabled && self.volume.rect.contains(x, y) {
            self.drag = Drag::Volume;
            self.volume.value = self.volume.value_at(x, self.s);
            self.mark(Area::Format);
            acts.push(Act::Volume(self.volume.value));
            return acts;
        }
        if self.split_rect.contains(x, y) {
            self.drag = Drag::Split {
                offset: y - self.split_rect.y,
            };
            return acts;
        }
        if self.list_rect.contains(x, y) {
            self.list_press(x, y, mods, &mut acts);
            return acts;
        }
        if self.log_rect.contains(x, y) {
            if self.focus != Focus::Log {
                self.focus = Focus::Log;
                self.mark(Area::Log);
                self.mark(Area::List);
            }
            let sb = self.log_scroll_rect();
            if sb.contains(x, y) {
                let total = i64::from(self.log_total());
                let page = i64::from(self.log_view_h());
                match thumb(sb, i64::from(self.log_scroll), page, total, self.px(20)) {
                    Some(t) if t.contains(x, y) => self.drag = Drag::LogThumb { offset: y - t.y },
                    Some(t) => {
                        let d = if y < t.y { -page } else { page };
                        self.log_scroll = (i64::from(self.log_scroll) + d).max(0) as i32;
                        self.mark(Area::Log);
                    }
                    None => {}
                }
            }
        }
        acts
    }

    fn list_press(&mut self, x: i32, y: i32, mods: Mods, acts: &mut Vec<Act>) {
        if self.focus != Focus::List {
            self.focus = Focus::List;
            self.mark(Area::Log);
        }
        self.mark(Area::List);
        let r = self.list_rect;
        // 列标题：分隔线附近拖动调整列宽，否则按该列排序
        if y < r.y + self.header_h() {
            let grip = self.px(4);
            for (i, &(cx, w)) in self.col_x().iter().enumerate() {
                if (x - (cx + w)).abs() <= grip {
                    self.drag = Drag::Column {
                        col: i,
                        start_x: x,
                        start_w: self.col_w[i],
                    };
                    return;
                }
            }
            if let Some(i) = self
                .col_x()
                .iter()
                .position(|&(cx, w)| x >= cx && x < cx + w)
                && let Some(key) = column_key(i)
            {
                acts.push(Act::Sort(key));
            }
            return;
        }
        let sb = self.list_scroll_rect();
        if sb.contains(x, y) {
            let page = self.visible_rows() as i64;
            match thumb(sb, self.top as i64, page, self.count as i64, self.px(20)) {
                Some(t) if t.contains(x, y) => self.drag = Drag::ListThumb { offset: y - t.y },
                Some(t) => {
                    let d = if y < t.y { -page } else { page };
                    self.top = (self.top as i64 + d).clamp(0, self.max_top() as i64) as usize;
                }
                None => {}
            }
            return;
        }
        let Some(i) = self.row_at(y) else {
            if !mods.ctrl {
                self.select(&[]);
            }
            return;
        };
        let now = Instant::now();
        let double = matches!(self.last_click, Some((t, r)) if r == i && now.duration_since(t) < Duration::from_millis(450));
        self.last_click = Some((now, i));
        if double && !mods.ctrl && !mods.shift {
            self.last_click = None;
            acts.push(Act::Activate(i));
            return;
        }
        if mods.shift {
            let (a, b) = (self.anchor.min(i), self.anchor.max(i));
            if !mods.ctrl {
                self.selected.iter_mut().for_each(|s| *s = false);
            }
            for k in a..=b {
                if let Some(s) = self.selected.get_mut(k) {
                    *s = true;
                }
            }
            self.focus_row = i;
        } else if mods.ctrl {
            if let Some(s) = self.selected.get_mut(i) {
                *s = !*s;
            }
            self.focus_row = i;
            self.anchor = i;
        } else {
            if !self.selected.get(i).copied().unwrap_or(false) {
                self.select(&[i]);
            }
            self.focus_row = i;
            self.anchor = i;
            // 可能开始拖动选中的行（移动足够距离后）
            self.drag = Drag::RowPress { x, y };
        }
    }

    pub fn release(&mut self, c: &mut Canvas, x: i32, y: i32, button: u8) -> Vec<Act> {
        let mut acts = Vec::new();
        if button != 1 {
            return acts;
        }
        if let Modal::Files(d) = &mut self.modal {
            let r = d.release(c, x, y);
            self.files_result(r, &mut acts);
            return acts;
        }
        match std::mem::replace(&mut self.drag, Drag::None) {
            Drag::Button(i) => {
                self.pressed_button = None;
                self.mark(Area::Toolbar);
                if self.buttons.get(i).is_some_and(|r| r.contains(x, y)) {
                    acts.push(Act::Command(match i {
                        0 => ID_PLAY,
                        1 => ID_STOP,
                        2 => ID_PREV,
                        3 => ID_NEXT,
                        _ => ID_ADD_FILES,
                    }));
                }
            }
            Drag::Seek => {
                self.set_status_override(None);
                acts.push(Act::Seek(self.seek.value));
            }
            Drag::Volume => acts.push(Act::Volume(self.volume.value)),
            Drag::RowPress { .. } => {
                // 没有拖动：点击已选中的行时只选中这一行
                if let Some(i) = self.row_at(y)
                    && self.selected().len() > 1
                {
                    self.select(&[i]);
                }
            }
            Drag::Rows { rows, .. } => {
                self.set_status_override(None);
                self.mark(Area::List);
                if let Some(to) = self.drop_index(y) {
                    acts.push(Act::Move { rows, to });
                }
            }
            Drag::Column { .. } | Drag::Split { .. } => self.mark_all(),
            _ => {}
        }
        acts
    }

    /// 拖动行的落点：光标在行的上半部为该行之前，下半部为之后；最后一行以下为末尾。
    fn drop_index(&self, y: i32) -> Option<usize> {
        let rr = self.rows_rect();
        if self.count == 0 {
            return None;
        }
        if y < rr.y {
            return Some(self.top);
        }
        let rh = self.row_h();
        let rel = y - rr.y;
        let i = self.top + (rel / rh) as usize;
        if i >= self.count {
            return Some(self.count);
        }
        Some(if rel % rh < rh / 2 { i } else { i + 1 })
    }

    pub fn motion(&mut self, c: &mut Canvas, x: i32, y: i32) -> Vec<Act> {
        let mut acts = Vec::new();
        if let Modal::Files(d) = &mut self.modal {
            if d.motion(c, x, y) {
                self.overlay_dirty = true;
            }
            return acts;
        }
        match &mut self.drag {
            Drag::Seek => {
                let v = self.seek.value_at(x, self.s);
                if v != self.seek.value {
                    self.seek.value = v;
                    self.mark(Area::Seek);
                    acts.push(Act::SeekPreview(v));
                }
                return acts;
            }
            Drag::Volume => {
                let v = self.volume.value_at(x, self.s);
                if v != self.volume.value {
                    self.volume.value = v;
                    self.mark(Area::Format);
                    acts.push(Act::Volume(v));
                }
                return acts;
            }
            Drag::Split { offset } => {
                let new_split = y - *offset;
                let bottom = self.status_rect.y;
                let log_h = bottom - new_split - self.split_rect.h;
                let v = (log_h as f32 / self.s).round() as i32;
                self.log_height = v.max(40) as u32;
                self.w = 0;
                self.mark_all();
                return acts;
            }
            Drag::Column {
                col,
                start_x,
                start_w,
            } => {
                let (col, start_x, start_w) = (*col, *start_x, *start_w);
                let d = ((x - start_x) as f32 / self.s).round() as i32;
                self.col_w[col] = (start_w + d).max(20);
                self.mark(Area::List);
                return acts;
            }
            Drag::ListThumb { offset } => {
                let offset = *offset;
                let sb = self.list_scroll_rect();
                let page = self.visible_rows() as i64;
                if let Some(t) = thumb(sb, 0, page, self.count as i64, self.px(20)) {
                    let span = (sb.h - t.h).max(1);
                    let pos = i64::from(y - offset - sb.y).clamp(0, i64::from(span));
                    self.top = (pos * self.max_top() as i64 / i64::from(span)) as usize;
                    self.mark(Area::List);
                }
                return acts;
            }
            Drag::LogThumb { offset } => {
                let offset = *offset;
                let sb = self.log_scroll_rect();
                let total = i64::from(self.log_total());
                let page = i64::from(self.log_view_h());
                if let Some(t) = thumb(sb, 0, page, total, self.px(20)) {
                    let span = (sb.h - t.h).max(1);
                    let pos = i64::from(y - offset - sb.y).clamp(0, i64::from(span));
                    self.log_scroll = (pos * (total - page).max(0) / i64::from(span)) as i32;
                    self.mark(Area::Log);
                }
                return acts;
            }
            Drag::RowPress { x: px, y: py } => {
                if (x - *px).abs() + (y - *py).abs() > self.px(6) {
                    let rows = self.selected();
                    if !rows.is_empty() {
                        self.drag = Drag::Rows { rows, mark: None };
                    }
                }
            }
            Drag::Button(i) => {
                let inside = self.buttons.get(*i).is_some_and(|r| r.contains(x, y));
                let p = inside.then_some(*i);
                if p != self.pressed_button {
                    self.pressed_button = p;
                    self.mark(Area::Toolbar);
                }
                return acts;
            }
            Drag::None => {}
            Drag::Rows { .. } => {}
        }
        if let Drag::Rows { .. } = self.drag {
            // 靠近上下边缘时滚动
            let rr = self.rows_rect();
            let rh = self.row_h();
            if y < rr.y + rh / 2 && self.top > 0 {
                self.top -= 1;
            } else if y > rr.bottom() - rh / 2 && self.top < self.max_top() {
                self.top += 1;
            }
            let to = self.drop_index(y);
            let hint = match to {
                Some(t) if t >= self.count => "松开后移到列表末尾".to_string(),
                Some(t) => format!("松开后移到第 {} 行之前", t + 1),
                None => String::new(),
            };
            if let Drag::Rows { mark, .. } = &mut self.drag {
                *mark = to.filter(|&t| t < self.count);
            }
            self.set_status_override(Some(hint));
            self.mark(Area::List);
            return acts;
        }
        // 悬停
        match self.popup {
            Popup::Menu { index, hover } => {
                if let Some(i) = self.menu_items.iter().position(|r| r.contains(x, y))
                    && i != index
                {
                    self.popup = Popup::Menu {
                        index: i,
                        hover: None,
                    };
                    self.dirty = ALL;
                    self.overlay_dirty = true;
                    return acts;
                }
                let (_, items) = self.menu_box(c, index);
                let h = items
                    .iter()
                    .position(|(r, id)| id.is_some() && r.contains(x, y));
                if h != hover {
                    self.popup = Popup::Menu { index, hover: h };
                    self.overlay_dirty = true;
                }
                return acts;
            }
            Popup::Combo { hover, top } => {
                let bx = self.combo_box();
                let h = bx
                    .contains(x, y)
                    .then(|| top + ((y - bx.y - 1) / self.px(24)).max(0) as usize)
                    .filter(|&i| i < self.devices.len());
                if h != hover && h.is_some() {
                    self.popup = Popup::Combo { hover: h, top };
                    self.overlay_dirty = true;
                }
                return acts;
            }
            Popup::None => {}
        }
        if self.modal_open() {
            return acts;
        }
        let hb = self
            .buttons
            .iter()
            .position(|r| r.contains(x, y))
            .or_else(|| self.combo.contains(x, y).then_some(100));
        if hb != self.hover_button {
            self.hover_button = hb;
            self.mark(Area::Toolbar);
        }
        let on_split = self.split_rect.contains(x, y);
        let on_col = y >= self.list_rect.y
            && y < self.list_rect.y + self.header_h()
            && self
                .col_x()
                .iter()
                .any(|&(cx, w)| (x - (cx + w)).abs() <= self.px(4));
        acts.push(Act::Cursor(if on_split {
            Cursor::ResizeV
        } else if on_col {
            Cursor::ResizeH
        } else {
            Cursor::Arrow
        }));
        acts
    }

    pub fn leave(&mut self) {
        if self.hover_button.take().is_some() {
            self.mark(Area::Toolbar);
        }
    }

    /// 失去焦点：取消拖动行（与 Windows 前端失去鼠标捕获时相同）。
    pub fn focus_lost(&mut self) {
        if let Drag::Rows { .. } = self.drag {
            self.drag = Drag::None;
            self.set_status_override(None);
            self.mark(Area::List);
        }
    }

    fn wheel(&mut self, x: i32, y: i32, dir: i32) {
        if let Modal::Files(d) = &mut self.modal {
            if d.wheel(x, y, dir) {
                self.overlay_dirty = true;
            }
            return;
        }
        if let Popup::Combo { hover, top } = self.popup {
            let max = self.devices.len().saturating_sub(self.combo_rows());
            let t = (top as i64 + i64::from(dir)).clamp(0, max as i64) as usize;
            self.popup = Popup::Combo { hover, top: t };
            self.overlay_dirty = true;
            return;
        }
        if self.list_rect.contains(x, y) {
            let t = (self.top as i64 + i64::from(dir) * 3).clamp(0, self.max_top() as i64) as usize;
            if t != self.top {
                self.top = t;
                self.mark(Area::List);
            }
        } else if self.log_rect.contains(x, y) {
            self.log_scroll = (self.log_scroll + dir * 3 * self.px(18)).max(0);
            self.mark(Area::Log);
        }
    }

    fn files_result(&mut self, r: FileResult, acts: &mut Vec<Act>) {
        match r {
            FileResult::None => {}
            FileResult::Redraw => self.overlay_dirty = true,
            FileResult::Cancel(dir) => {
                self.last_dir = Some(dir);
                self.close_overlay();
            }
            FileResult::Add(dir, paths) => {
                self.last_dir = Some(dir);
                self.close_overlay();
                if !paths.is_empty() {
                    acts.push(Act::Paths(paths));
                }
            }
        }
    }

    /// 按键。`ch` 为该键对应的可打印字符（文件对话框的路径输入用）。
    pub fn key(&mut self, c: &mut Canvas, sym: u32, ch: Option<char>, mods: Mods) -> Vec<Act> {
        let mut acts = Vec::new();
        match &mut self.modal {
            Modal::About(_) => {
                if matches!(
                    sym,
                    keysym::RETURN | keysym::KP_ENTER | keysym::ESCAPE | keysym::SPACE
                ) {
                    self.close_overlay();
                }
                return acts;
            }
            Modal::Files(d) => {
                let r = d.key(c, sym, ch, mods);
                self.files_result(r, &mut acts);
                return acts;
            }
            Modal::None => {}
        }
        if let Popup::Menu { index, hover } = self.popup {
            self.menu_key(c, index, hover, sym, ch, &mut acts);
            return acts;
        }
        if let Popup::Combo { hover, top } = self.popup {
            let n = self.devices.len();
            match sym {
                keysym::ESCAPE => self.close_overlay(),
                keysym::UP | keysym::DOWN => {
                    let cur = hover.unwrap_or(self.device);
                    let h = if sym == keysym::UP {
                        cur.saturating_sub(1)
                    } else {
                        (cur + 1).min(n.saturating_sub(1))
                    };
                    let rows = self.combo_rows();
                    let top = if h < top {
                        h
                    } else if h >= top + rows {
                        h + 1 - rows
                    } else {
                        top
                    };
                    self.popup = Popup::Combo {
                        hover: Some(h),
                        top,
                    };
                    self.overlay_dirty = true;
                }
                keysym::RETURN | keysym::KP_ENTER => {
                    self.close_overlay();
                    if let Some(h) = hover
                        && h < n
                        && h != self.device
                    {
                        self.device = h;
                        acts.push(Act::Device(h));
                    }
                }
                _ => {}
            }
            return acts;
        }
        if let Drag::Rows { .. } = self.drag
            && sym == keysym::ESCAPE
        {
            self.focus_lost();
            return acts;
        }
        // Alt+字母打开菜单；F10 打开第一个菜单
        if mods.alt
            && let Some(ch) = ch
            && let Some(i) = MENUS.iter().position(|m| m.1 == ch.to_ascii_lowercase())
        {
            self.popup = Popup::Menu {
                index: i,
                hover: None,
            };
            self.overlay_dirty = true;
            self.mark(Area::Menu);
            return acts;
        }
        if sym == keysym::F10 {
            self.popup = Popup::Menu {
                index: 0,
                hover: None,
            };
            self.overlay_dirty = true;
            self.mark(Area::Menu);
            return acts;
        }
        if mods.ctrl && matches!(ch, Some('c' | 'C')) && self.focus == Focus::Log {
            acts.push(Act::Command(ID_COPY_LOG));
            return acts;
        }
        if mods.ctrl && matches!(ch, Some('a' | 'A')) && self.focus == Focus::List {
            let all: Vec<usize> = (0..self.count).collect();
            let keep = self.focus_row;
            self.select(&all);
            self.focus_row = keep;
            return acts;
        }
        match sym {
            keysym::SPACE => acts.push(Act::Command(ID_PLAY)),
            keysym::LEFT => acts.push(Act::SeekBy(-5)),
            keysym::RIGHT => acts.push(Act::SeekBy(5)),
            keysym::DELETE if self.focus == Focus::List => {
                let rows = self.selected();
                if !rows.is_empty() {
                    acts.push(Act::Delete(rows));
                }
            }
            keysym::RETURN | keysym::KP_ENTER if self.focus == Focus::List && self.count > 0 => {
                acts.push(Act::Activate(self.focus_row.min(self.count - 1)));
            }
            keysym::UP | keysym::DOWN if mods.ctrl => {
                acts.push(Act::MoveBy(if sym == keysym::UP { -1 } else { 1 }));
            }
            keysym::UP
            | keysym::DOWN
            | keysym::PAGE_UP
            | keysym::PAGE_DOWN
            | keysym::HOME
            | keysym::END
                if self.focus == Focus::List && self.count > 0 =>
            {
                let page = self.visible_rows().max(1);
                let cur = self.focus_row;
                let to = match sym {
                    keysym::UP => cur.saturating_sub(1),
                    keysym::DOWN => cur + 1,
                    keysym::PAGE_UP => cur.saturating_sub(page),
                    keysym::PAGE_DOWN => cur + page,
                    keysym::HOME => 0,
                    _ => self.count,
                }
                .min(self.count - 1);
                if mods.shift {
                    let (a, b) = (self.anchor.min(to), self.anchor.max(to));
                    self.selected.iter_mut().for_each(|s| *s = false);
                    for k in a..=b {
                        if let Some(s) = self.selected.get_mut(k) {
                            *s = true;
                        }
                    }
                    self.focus_row = to;
                    self.ensure_visible(to);
                    self.mark(Area::List);
                } else {
                    self.select(&[to]);
                }
            }
            keysym::UP | keysym::DOWN | keysym::PAGE_UP | keysym::PAGE_DOWN
                if self.focus == Focus::Log =>
            {
                let step = match sym {
                    keysym::UP => -self.px(18),
                    keysym::DOWN => self.px(18),
                    keysym::PAGE_UP => -self.log_view_h(),
                    _ => self.log_view_h(),
                };
                self.log_scroll = (self.log_scroll + step).max(0);
                self.mark(Area::Log);
            }
            _ => {}
        }
        acts
    }

    fn menu_key(
        &mut self,
        c: &mut Canvas,
        index: usize,
        hover: Option<usize>,
        sym: u32,
        ch: Option<char>,
        acts: &mut Vec<Act>,
    ) {
        let entries = menu_entries(index);
        let items: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter_map(|(k, e)| matches!(e, Entry::Item(..)).then_some(k))
            .collect();
        let open = |v: &mut Self, i: usize| {
            v.popup = Popup::Menu {
                index: i,
                hover: None,
            };
            v.dirty = ALL;
            v.overlay_dirty = true;
        };
        match sym {
            keysym::ESCAPE => self.close_overlay(),
            keysym::LEFT => open(self, (index + MENUS.len() - 1) % MENUS.len()),
            keysym::RIGHT => open(self, (index + 1) % MENUS.len()),
            keysym::UP | keysym::DOWN => {
                let pos = hover.and_then(|h| items.iter().position(|&k| k == h));
                let n = items.len().max(1);
                let next = match (pos, sym == keysym::UP) {
                    (None, true) => n - 1,
                    (None, false) => 0,
                    (Some(p), true) => (p + n - 1) % n,
                    (Some(p), false) => (p + 1) % n,
                };
                self.popup = Popup::Menu {
                    index,
                    hover: items.get(next).copied(),
                };
                self.overlay_dirty = true;
            }
            keysym::RETURN | keysym::KP_ENTER => {
                if let Some(Entry::Item(id, ..)) = hover.and_then(|h| entries.get(h)) {
                    let id = *id;
                    self.close_overlay();
                    acts.push(Act::Command(id));
                }
            }
            _ => {
                let Some(ch) = ch.map(|c| c.to_ascii_lowercase()) else {
                    return;
                };
                let hit = entries.iter().find_map(|e| match e {
                    Entry::Item(id, label, _) if mnemonic(label) == Some(ch) => Some(*id),
                    _ => None,
                });
                if let Some(id) = hit {
                    self.close_overlay();
                    acts.push(Act::Command(id));
                }
            }
        }
        let _ = c;
    }
}

/// 滚动条滑块的位置：`pos` 为第一项、`page` 为可见数、`total` 为总数；全部可见时为 `None`。
fn thumb(track: Rect, pos: i64, page: i64, total: i64, min: i32) -> Option<Rect> {
    if total <= page || total <= 0 || track.h <= 0 {
        return None;
    }
    let h = ((i64::from(track.h) * page / total) as i32).clamp(min.min(track.h), track.h);
    let span = i64::from(track.h - h);
    let max = (total - page).max(1);
    let y = track.y + (span * pos.clamp(0, max) / max) as i32;
    Some(Rect::new(track.x, y, track.w, h))
}

#[cfg(test)]
mod tests {
    use super::{Rect, mnemonic, thumb};

    #[test]
    fn mnemonics() {
        assert_eq!(mnemonic("添加文件(O)…"), Some('o'));
        assert_eq!(mnemonic("关于 QyBit(A)"), Some('a'));
        assert_eq!(mnemonic("无"), None);
    }

    #[test]
    fn scroll_thumb() {
        let t = Rect::new(0, 0, 10, 100);
        assert_eq!(thumb(t, 0, 10, 5, 20), None);
        assert_eq!(thumb(t, 0, 10, 100, 20), Some(Rect::new(0, 0, 10, 20)));
        assert_eq!(thumb(t, 90, 10, 100, 20), Some(Rect::new(0, 80, 10, 20)));
        assert_eq!(thumb(t, 45, 50, 100, 20), Some(Rect::new(0, 45, 10, 50)));
    }
}
