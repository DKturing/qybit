//! 自绘的文件选择面板（“添加文件…”“添加文件夹…”）：没有系统文件对话框可用（自绘界面、
//! 精简系统上没有 xdg-desktop-portal），按 Windows 前端的文件对话框提供相同的选择方式。
//!
//! - 目录在前、文件在后，按名称排序（不区分大小写）；默认只列音频文件（格式集合的扩展名，与
//!   Windows 对话框的筛选相同）与目录，可切换为显示所有文件（含隐藏文件）。
//! - 单击选中，Ctrl / Shift 多选，双击目录进入、双击文件加入；回车同双击；Backspace 上一级；
//!   Esc 取消。路径栏可输入路径后回车或“转到”。
//! - “添加文件夹”模式：加入选中的目录，没有选中时加入当前目录（文件夹在读取线程中扫描）。

use std::cmp::Ordering;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use accesskit::{Action, ActionData, Node, NodeId, Role, TextPosition, TextSelection, Toggled};

use super::access::{bounds, id};
use super::paint::{Align, Canvas, Rect};
use super::x11::{Mods, keysym};

/// 面板交给主视图的结果。
pub enum FileResult {
    None,
    Redraw,
    /// 取消；带上当前目录（下次从这里打开）。
    Cancel(PathBuf),
    /// 加入这些文件或文件夹。
    Add(PathBuf, Vec<PathBuf>),
}

struct Item {
    name: OsString,
    label: String,
    dir: bool,
    size: Option<u64>,
}

#[derive(Default, Clone, Copy)]
struct Geom {
    bx: Rect,
    path: Rect,
    go: Rect,
    up: Rect,
    home: Rect,
    root: Rect,
    all: Rect,
    list: Rect,
    add: Rect,
    cancel: Rect,
}

pub struct FileDialog {
    folder: bool,
    /// 默认只列这些扩展名的文件（E2：来自格式集合）。
    filter: crate::app::FileFilter,
    dir: PathBuf,
    items: Vec<Item>,
    selected: Vec<bool>,
    focus: usize,
    anchor: usize,
    top: usize,
    show_all: bool,
    path_text: String,
    /// 光标位置（字符数）。
    path_cursor: usize,
    path_focus: bool,
    error: Option<String>,
    last_click: Option<(Instant, usize)>,
    pressed: Option<u8>,
    g: Geom,
    s: f32,
}

impl FileDialog {
    pub fn new(dir: PathBuf, folder: bool, filter: crate::app::FileFilter) -> Self {
        let mut d = Self {
            folder,
            filter,
            dir: PathBuf::new(),
            items: Vec::new(),
            selected: Vec::new(),
            focus: 0,
            anchor: 0,
            top: 0,
            show_all: false,
            path_text: String::new(),
            path_cursor: 0,
            path_focus: false,
            error: None,
            last_click: None,
            pressed: None,
            g: Geom::default(),
            s: 1.0,
        };
        if d.enter(&dir).is_err() {
            let _ = d.enter(Path::new("/"));
        }
        d
    }

    /// 进入目录；读取失败时留在原目录并显示原因。
    fn enter(&mut self, dir: &Path) -> Result<(), ()> {
        let rd = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(e) => {
                self.error = Some(format!("无法打开 {}：{e}", dir.display()));
                return Err(());
            }
        };
        let mut items = Vec::new();
        for e in rd.flatten() {
            let name = e.file_name();
            let label = name.to_string_lossy().into_owned();
            let hidden = label.starts_with('.');
            // 跟随符号链接取类型与大小。
            let meta = std::fs::metadata(e.path()).ok();
            let dir = meta.as_ref().is_some_and(std::fs::Metadata::is_dir);
            if !self.show_all {
                if hidden {
                    continue;
                }
                let audio = self.filter.accepts(Path::new(&name));
                if !dir && (!audio || self.folder) {
                    continue;
                }
            } else if self.folder && !dir {
                continue;
            }
            items.push(Item {
                name,
                label,
                dir,
                size: meta.filter(|m| m.is_file()).map(|m| m.len()),
            });
        }
        items.sort_by(|a, b| match (a.dir, b.dir) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => a
                .label
                .to_lowercase()
                .cmp(&b.label.to_lowercase())
                .then_with(|| a.label.cmp(&b.label)),
        });
        self.dir = dir.to_path_buf();
        self.selected = vec![false; items.len()];
        self.items = items;
        self.focus = 0;
        self.anchor = 0;
        self.top = 0;
        self.error = None;
        self.path_text = self.dir.display().to_string();
        self.path_cursor = self.path_text.chars().count();
        Ok(())
    }

    fn reload(&mut self) {
        let dir = self.dir.clone();
        let _ = self.enter(&dir);
    }

    fn px(&self, v: i32) -> i32 {
        (v as f32 * self.s).round() as i32
    }

    fn row_h(&self) -> i32 {
        self.px(24).max(1)
    }

    fn visible(&self) -> usize {
        ((self.g.list.h - 2) / self.row_h()).max(1) as usize
    }

    fn ensure_visible(&mut self, i: usize) {
        let vis = self.visible();
        if i < self.top {
            self.top = i;
        } else if i >= self.top + vis {
            self.top = i + 1 - vis;
        }
    }

    fn select_only(&mut self, i: usize) {
        self.selected.iter_mut().for_each(|s| *s = false);
        if let Some(s) = self.selected.get_mut(i) {
            *s = true;
        }
        self.focus = i;
        self.anchor = i;
        self.ensure_visible(i);
    }

    fn chosen(&self) -> Vec<PathBuf> {
        self.items
            .iter()
            .zip(&self.selected)
            .filter(|(it, s)| **s && it.dir == self.folder)
            .map(|(it, _)| self.dir.join(&it.name))
            .collect()
    }

    /// “添加”按钮：文件模式加入选中的文件（选中目录时进入该目录）；文件夹模式加入选中的目录，
    /// 没有选中时加入当前目录。
    fn add(&mut self) -> FileResult {
        let chosen = self.chosen();
        if self.folder {
            let paths = if chosen.is_empty() {
                vec![self.dir.clone()]
            } else {
                chosen
            };
            return FileResult::Add(self.dir.clone(), paths);
        }
        if chosen.is_empty() {
            if let Some(it) = self.items.get(self.focus).filter(|it| it.dir) {
                let p = self.dir.join(&it.name);
                let _ = self.enter(&p);
                return FileResult::Redraw;
            }
            return FileResult::None;
        }
        FileResult::Add(self.dir.clone(), chosen)
    }

    /// 打开一项：目录进入，文件加入（连同其他选中的文件）。
    fn open(&mut self, i: usize) -> FileResult {
        let Some(it) = self.items.get(i) else {
            return FileResult::None;
        };
        let p = self.dir.join(&it.name);
        if it.dir {
            let _ = self.enter(&p);
            return FileResult::Redraw;
        }
        let mut paths = self.chosen();
        if !paths.contains(&p) {
            paths.push(p);
        }
        FileResult::Add(self.dir.clone(), paths)
    }

    fn go_path(&mut self) -> FileResult {
        let p = PathBuf::from(self.path_text.trim());
        if p.is_dir() {
            let _ = self.enter(&p);
            self.path_focus = false;
            return FileResult::Redraw;
        }
        if p.is_file() {
            let dir = p
                .parent()
                .map_or_else(|| self.dir.clone(), Path::to_path_buf);
            return FileResult::Add(dir, vec![p]);
        }
        self.error = Some(format!("找不到 {}", p.display()));
        FileResult::Redraw
    }

    fn parent(&mut self) -> FileResult {
        if let Some(p) = self.dir.parent().map(Path::to_path_buf) {
            let from = self.dir.file_name().map(OsString::from);
            let _ = self.enter(&p);
            if let Some(i) = from.and_then(|n| self.items.iter().position(|it| it.name == n)) {
                self.select_only(i);
            }
        }
        FileResult::Redraw
    }

    pub fn paint(&mut self, c: &mut Canvas, area: Rect) -> Rect {
        self.s = c.scale();
        let w = self.px(760).min(area.w - self.px(24)).max(self.px(300));
        let h = self.px(540).min(area.h - self.px(24)).max(self.px(240));
        let bx = Rect::new(area.x + (area.w - w) / 2, area.y + (area.h - h) / 2, w, h);
        let pad = self.px(12);
        let title = Rect::new(bx.x, bx.y, bx.w, self.px(30));
        let row = self.px(28);
        let go_w = self.px(64);
        let path = Rect::new(
            bx.x + pad,
            title.bottom() + self.px(8),
            bx.w - 2 * pad - go_w - self.px(6),
            row,
        );
        let go = Rect::new(path.right() + self.px(6), path.y, go_w, row);
        let nav_y = path.bottom() + self.px(6);
        let up = Rect::new(bx.x + pad, nav_y, self.px(72), row);
        let home = Rect::new(up.right() + self.px(6), nav_y, self.px(72), row);
        let root = Rect::new(home.right() + self.px(6), nav_y, self.px(72), row);
        let all = Rect::new(root.right() + self.px(16), nav_y, self.px(160), row);
        let bottom_h = self.px(44);
        let list = Rect::new(
            bx.x + pad,
            nav_y + row + self.px(8),
            bx.w - 2 * pad,
            (bx.bottom() - bottom_h - (nav_y + row + self.px(8))).max(self.px(40)),
        );
        let add_w = if self.folder {
            self.px(120)
        } else {
            self.px(84)
        };
        let cancel = Rect::new(
            bx.right() - pad - self.px(84),
            bx.bottom() - self.px(36),
            self.px(84),
            row,
        );
        let add = Rect::new(cancel.x - self.px(8) - add_w, cancel.y, add_w, row);
        self.g = Geom {
            bx,
            path,
            go,
            up,
            home,
            root,
            all,
            list,
            add,
            cancel,
        };
        // 框与标题
        c.fill(bx, 0xf0f0f0);
        c.frame(bx, 0x707070);
        c.fill(title, 0xe6e6e6);
        c.text_bold(title, pad, &self.heading(), 0x1a1a1a, Align::Left);
        // 路径栏
        c.fill(path, 0xffffff);
        c.frame(path, if self.path_focus { 0x0078d7 } else { 0xadadad });
        let shown: String = self.path_text.clone();
        c.text(path, self.px(6), &shown, 0x1a1a1a, Align::Left);
        if self.path_focus {
            let before: String = self.path_text.chars().take(self.path_cursor).collect();
            let x = (path.x + self.px(6) + c.text_width(&before)).min(path.right() - 2);
            c.fill(
                Rect::new(
                    x,
                    path.y + self.px(5),
                    1.max(self.px(1)),
                    path.h - self.px(10),
                ),
                0x000000,
            );
        }
        for (i, (r, l)) in [
            (go, "转到"),
            (up, "上一级"),
            (home, "主目录"),
            (root, "根目录"),
        ]
        .into_iter()
        .enumerate()
        {
            let pressed = self.pressed == Some(i as u8);
            c.rounded(
                r,
                self.px(3) as f32,
                if pressed { 0xcce4f7 } else { 0xfdfdfd },
                Some(0xadadad),
            );
            c.text(r, 0, l, 0x1a1a1a, Align::Center);
        }
        if !self.folder {
            let b = self.px(13);
            let bxx = Rect::new(all.x, all.y + (all.h - b) / 2, b, b);
            c.fill(bxx, if self.show_all { 0x0078d7 } else { 0xffffff });
            c.frame(bxx, if self.show_all { 0x0078d7 } else { 0x333333 });
            if self.show_all {
                let (x, y, k) = (bxx.x as f32, bxx.y as f32, b as f32);
                c.polyline(
                    &[
                        (x + k * 0.22, y + k * 0.52),
                        (x + k * 0.42, y + k * 0.72),
                        (x + k * 0.78, y + k * 0.3),
                    ],
                    1.6 * self.s,
                    0xffffff,
                );
            }
            c.text(
                Rect::new(bxx.right() + self.px(6), all.y, all.w, all.h),
                0,
                "显示所有文件",
                0x1a1a1a,
                Align::Left,
            );
        }
        // 列表
        c.fill(list, 0xffffff);
        c.frame(list, 0xadadad);
        let rh = self.row_h();
        let inner = list.inset(1);
        let vis = self.visible();
        if self.items.is_empty() {
            c.text(
                Rect::new(inner.x, inner.y, inner.w, rh),
                self.px(8),
                if self.folder {
                    "（没有子文件夹）"
                } else {
                    "（没有音频文件或文件夹）"
                },
                0x8a8a8a,
                Align::Left,
            );
        }
        let size_w = self.px(110);
        for k in 0..vis {
            let i = self.top + k;
            let Some(it) = self.items.get(i) else { break };
            let r = Rect::new(inner.x, inner.y + k as i32 * rh, inner.w, rh).intersect(&inner);
            if self.selected.get(i).copied().unwrap_or(false) {
                c.fill(r, 0xcce8ff);
            }
            if i == self.focus && !self.path_focus {
                c.frame(r, 0x99d1ff);
            }
            // 图标：目录为黄色文件夹形状，文件为蓝色小方块。
            let ix = r.x + self.px(8);
            let iy = r.y + r.h / 2;
            if it.dir {
                c.fill(
                    Rect::new(ix, iy - self.px(5), self.px(14), self.px(10)),
                    0xe8b84a,
                );
                c.fill(
                    Rect::new(ix, iy - self.px(7), self.px(6), self.px(3)),
                    0xe8b84a,
                );
            } else {
                c.fill(
                    Rect::new(ix + self.px(2), iy - self.px(6), self.px(10), self.px(12)),
                    0x5b8fd6,
                );
            }
            let name_r = Rect::new(ix + self.px(22), r.y, r.w - self.px(30) - size_w, r.h);
            c.text(name_r, 0, &it.label, 0x1a1a1a, Align::Left);
            if let Some(sz) = it.size {
                c.text(
                    Rect::new(r.right() - size_w, r.y, size_w, r.h),
                    self.px(8),
                    &human_size(sz),
                    0x6d6d6d,
                    Align::Right,
                );
            }
        }
        // 底部
        let info = self.info();
        c.text(
            Rect::new(bx.x + pad, add.y, (add.x - bx.x - 2 * pad).max(0), add.h),
            0,
            &info,
            if self.error.is_some() {
                0xc81e1e
            } else {
                0x444444
            },
            Align::Left,
        );
        let add_label = if self.folder {
            "添加文件夹"
        } else {
            "添加"
        };
        c.rounded(add, self.px(3) as f32, 0xe5f1fb, Some(0x0078d7));
        c.text(add, 0, add_label, 0x1a1a1a, Align::Center);
        c.rounded(cancel, self.px(3) as f32, 0xfdfdfd, Some(0xadadad));
        c.text(cancel, 0, "取消", 0x1a1a1a, Align::Center);
        bx
    }

    pub fn press(&mut self, _c: &mut Canvas, x: i32, y: i32, mods: Mods) -> FileResult {
        let g = self.g;
        if !g.bx.contains(x, y) {
            return FileResult::None;
        }
        self.path_focus = g.path.contains(x, y);
        if self.path_focus {
            // 光标放到末尾（不按点击位置定位）。
            self.path_cursor = self.path_text.chars().count();
            return FileResult::Redraw;
        }
        for (i, r) in [g.go, g.up, g.home, g.root].iter().enumerate() {
            if r.contains(x, y) {
                self.pressed = Some(i as u8);
                return FileResult::Redraw;
            }
        }
        if !self.folder && g.all.contains(x, y) {
            self.show_all = !self.show_all;
            self.reload();
            return FileResult::Redraw;
        }
        if g.add.contains(x, y) {
            return self.add();
        }
        if g.cancel.contains(x, y) {
            return FileResult::Cancel(self.dir.clone());
        }
        let inner = g.list.inset(1);
        if inner.contains(x, y) {
            let i = self.top + ((y - inner.y) / self.row_h()) as usize;
            if i >= self.items.len() {
                self.selected.iter_mut().for_each(|s| *s = false);
                return FileResult::Redraw;
            }
            let now = Instant::now();
            let double = matches!(self.last_click, Some((t, r)) if r == i && now.duration_since(t) < Duration::from_millis(450));
            self.last_click = Some((now, i));
            if double {
                self.last_click = None;
                return self.open(i);
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
                self.focus = i;
            } else if mods.ctrl {
                if let Some(s) = self.selected.get_mut(i) {
                    *s = !*s;
                }
                self.focus = i;
                self.anchor = i;
            } else {
                self.select_only(i);
            }
            return FileResult::Redraw;
        }
        FileResult::Redraw
    }

    pub fn release(&mut self, _c: &mut Canvas, x: i32, y: i32) -> FileResult {
        let Some(i) = self.pressed.take() else {
            return FileResult::None;
        };
        let g = self.g;
        let r = [g.go, g.up, g.home, g.root][usize::from(i).min(3)];
        if !r.contains(x, y) {
            return FileResult::Redraw;
        }
        match i {
            0 => self.go_path(),
            1 => self.parent(),
            2 => {
                let home =
                    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
                let _ = self.enter(&home);
                FileResult::Redraw
            }
            _ => {
                let _ = self.enter(Path::new("/"));
                FileResult::Redraw
            }
        }
    }

    pub fn motion(&mut self, _c: &mut Canvas, _x: i32, _y: i32) -> bool {
        false
    }

    pub fn wheel(&mut self, x: i32, y: i32, dir: i32) -> bool {
        if !self.g.list.contains(x, y) {
            return false;
        }
        let max = self.items.len().saturating_sub(self.visible());
        let t = (self.top as i64 + i64::from(dir) * 3).clamp(0, max as i64) as usize;
        let changed = t != self.top;
        self.top = t;
        changed
    }

    /// 在路径栏光标处插入文字（键盘直接给出的字符，或输入法提交的文字，LG2）；控制字符去掉。
    /// 路径栏没有输入焦点或没有可插入的字符时返回 `false`。
    pub fn insert(&mut self, text: &str) -> bool {
        if !self.path_focus {
            return false;
        }
        let add: Vec<char> = text.chars().filter(|c| !c.is_control()).collect();
        if add.is_empty() {
            return false;
        }
        let mut v: Vec<char> = self.path_text.chars().collect();
        let at = self.path_cursor.min(v.len());
        v.splice(at..at, add.iter().copied());
        self.path_text = v.into_iter().collect();
        self.path_cursor = at + add.len();
        true
    }

    /// 路径栏有输入焦点时光标的位置（窗口坐标：光标左下角）；输入法据此放置候选框（LG2）。
    pub fn caret(&self, c: &mut Canvas) -> Option<(i32, i32)> {
        if !self.path_focus {
            return None;
        }
        let path = self.g.path;
        let before: String = self.path_text.chars().take(self.path_cursor).collect();
        let x = (path.x + self.px(6) + c.text_width(&before)).min(path.right() - 2);
        Some((x, path.bottom() - self.px(5)))
    }

    fn heading(&self) -> String {
        if self.folder {
            "添加文件夹".to_string()
        } else {
            format!("添加文件（{}）", self.filter.names)
        }
    }

    /// 底部的说明文字（选中数量或错误）。
    fn info(&self) -> String {
        let n = self.chosen().len();
        self.error.clone().unwrap_or_else(|| {
            if self.folder {
                if n == 0 {
                    "未选中文件夹时加入当前文件夹".to_string()
                } else {
                    format!("已选中 {n} 个文件夹")
                }
            } else {
                format!("{} 项，已选中 {n} 个文件", self.items.len())
            }
        })
    }

    /// 面板的无障碍节点（LG2）：对话框 [`id::DIALOG`] 及其控件。返回焦点所在的节点。
    pub fn a11y_nodes(&self, nodes: &mut Vec<(NodeId, Node)>) -> NodeId {
        let g = self.g;
        let mut dialog = Node::new(Role::Dialog);
        dialog.set_label(self.heading());
        dialog.set_bounds(bounds(g.bx));
        dialog.set_modal();
        let mut kids = Vec::new();
        // 路径栏：文字放在一个文字段子节点中，读屏软件可读出内容与光标位置。
        let mut path = Node::new(Role::TextInput);
        path.set_label("路径");
        path.set_bounds(bounds(g.path));
        path.add_action(Action::Focus);
        path.add_action(Action::SetValue);
        let mut run = Node::new(Role::TextRun);
        run.set_value(self.path_text.as_str());
        run.set_bounds(bounds(g.path));
        let lengths: Vec<u8> = self
            .path_text
            .chars()
            .map(|ch| ch.len_utf8() as u8)
            .collect();
        // 不给出每个字符的位置（可选项；逐字排版的开销随路径长度平方增长），读屏软件照常读文字
        // 与光标。
        run.set_character_lengths(lengths);
        nodes.push((NodeId(id::FILE_PATH_TEXT), run));
        path.push_child(NodeId(id::FILE_PATH_TEXT));
        let caret = TextPosition {
            node: NodeId(id::FILE_PATH_TEXT),
            character_index: self.path_cursor.min(self.path_text.chars().count()),
        };
        path.set_text_selection(TextSelection {
            anchor: caret,
            focus: caret,
        });
        nodes.push((NodeId(id::FILE_PATH), path));
        kids.push(NodeId(id::FILE_PATH));
        for (i, (r, l)) in [
            (g.go, "转到"),
            (g.up, "上一级"),
            (g.home, "主目录"),
            (g.root, "根目录"),
        ]
        .into_iter()
        .enumerate()
        {
            let k = NodeId(id::FILE_BUTTON + i as u64);
            let mut n = Node::new(Role::Button);
            n.set_label(l);
            n.set_bounds(bounds(r));
            n.add_action(Action::Click);
            nodes.push((k, n));
            kids.push(k);
        }
        if !self.folder {
            let mut n = Node::new(Role::CheckBox);
            n.set_label("显示所有文件");
            n.set_bounds(bounds(g.all));
            n.set_toggled(if self.show_all {
                Toggled::True
            } else {
                Toggled::False
            });
            n.add_action(Action::Click);
            nodes.push((NodeId(id::FILE_ALL), n));
            kids.push(NodeId(id::FILE_ALL));
        }
        let mut list = Node::new(Role::ListBox);
        list.set_label(self.dir.display().to_string());
        list.set_bounds(bounds(g.list));
        list.set_multiselectable();
        let inner = g.list.inset(1);
        let rh = self.row_h();
        for (i, it) in self.items.iter().enumerate() {
            let k = NodeId(id::FILE_ROW + i as u64);
            let mut n = Node::new(Role::ListBoxOption);
            n.set_label(match (it.dir, it.size) {
                (true, _) => format!("{}，文件夹", it.label),
                (false, Some(sz)) => format!("{}，{}", it.label, human_size(sz)),
                (false, None) => it.label.clone(),
            });
            n.set_selected(self.selected.get(i).copied().unwrap_or(false));
            n.set_position_in_set(i + 1);
            n.set_size_of_set(self.items.len());
            if i >= self.top && i < self.top + self.visible() {
                let r = Rect::new(inner.x, inner.y + (i - self.top) as i32 * rh, inner.w, rh);
                n.set_bounds(bounds(r.intersect(&inner)));
            }
            n.add_action(Action::Click);
            n.add_action(Action::Focus);
            list.push_child(k);
            nodes.push((k, n));
        }
        nodes.push((NodeId(id::FILE_LIST), list));
        kids.push(NodeId(id::FILE_LIST));
        let mut info = Node::new(Role::Label);
        info.set_value(self.info());
        nodes.push((NodeId(id::FILE_INFO), info));
        kids.push(NodeId(id::FILE_INFO));
        for (k, r, l) in [
            (
                id::FILE_ADD,
                g.add,
                if self.folder {
                    "添加文件夹"
                } else {
                    "添加"
                },
            ),
            (id::FILE_CANCEL, g.cancel, "取消"),
        ] {
            let mut n = Node::new(Role::Button);
            n.set_label(l);
            n.set_bounds(bounds(r));
            n.add_action(Action::Click);
            nodes.push((NodeId(k), n));
            kids.push(NodeId(k));
        }
        dialog.set_children(kids);
        nodes.push((NodeId(id::DIALOG), dialog));
        if self.path_focus {
            NodeId(id::FILE_PATH)
        } else if self.items.is_empty() {
            NodeId(id::FILE_LIST)
        } else {
            NodeId(id::FILE_ROW + self.focus.min(self.items.len() - 1) as u64)
        }
    }

    /// 读屏软件请求的动作（LG2），与鼠标、键盘的同一操作效果相同。
    pub fn a11y_action(
        &mut self,
        c: &mut Canvas,
        target: u64,
        action: Action,
        data: Option<&ActionData>,
    ) -> FileResult {
        let click = matches!(action, Action::Click);
        let g = self.g;
        let centre = |r: Rect| (r.x + r.w / 2, r.y + r.h / 2);
        if let Some(i) = id::index(target, id::FILE_ROW, id::SPAN) {
            if matches!(action, Action::Click | Action::Focus) && i < self.items.len() {
                self.path_focus = false;
                self.select_only(i);
                return FileResult::Redraw;
            }
            return FileResult::None;
        }
        if let Some(i) = id::index(target, id::FILE_BUTTON, 4) {
            if !click {
                return FileResult::None;
            }
            let (x, y) = centre([g.go, g.up, g.home, g.root][i]);
            let _ = self.press(c, x, y, Mods::default());
            return self.release(c, x, y);
        }
        match (target, action, data) {
            (id::FILE_PATH, Action::Focus, _) => {
                self.path_focus = true;
                self.path_cursor = self.path_text.chars().count();
                FileResult::Redraw
            }
            (id::FILE_PATH, Action::SetValue, Some(ActionData::Value(v))) => {
                self.path_focus = true;
                self.path_text = v.to_string();
                self.path_cursor = self.path_text.chars().count();
                FileResult::Redraw
            }
            (id::FILE_ALL, Action::Click, _) if !self.folder => {
                self.show_all = !self.show_all;
                self.reload();
                FileResult::Redraw
            }
            (id::FILE_ADD, Action::Click, _) => self.add(),
            (id::FILE_CANCEL, Action::Click, _) => FileResult::Cancel(self.dir.clone()),
            (id::FILE_LIST, Action::Focus, _) => {
                self.path_focus = false;
                FileResult::Redraw
            }
            _ => FileResult::None,
        }
    }

    pub fn key(&mut self, _c: &mut Canvas, sym: u32, ch: Option<char>, mods: Mods) -> FileResult {
        if sym == keysym::ESCAPE {
            return FileResult::Cancel(self.dir.clone());
        }
        if sym == keysym::TAB {
            self.path_focus = !self.path_focus;
            return FileResult::Redraw;
        }
        if self.path_focus {
            let n = self.path_text.chars().count();
            match sym {
                keysym::RETURN | keysym::KP_ENTER => return self.go_path(),
                keysym::BACKSPACE if self.path_cursor > 0 => {
                    let mut v: Vec<char> = self.path_text.chars().collect();
                    v.remove(self.path_cursor - 1);
                    self.path_text = v.into_iter().collect();
                    self.path_cursor -= 1;
                }
                keysym::DELETE if self.path_cursor < n => {
                    let mut v: Vec<char> = self.path_text.chars().collect();
                    v.remove(self.path_cursor);
                    self.path_text = v.into_iter().collect();
                }
                keysym::LEFT => self.path_cursor = self.path_cursor.saturating_sub(1),
                keysym::RIGHT => self.path_cursor = (self.path_cursor + 1).min(n),
                keysym::HOME => self.path_cursor = 0,
                keysym::END => self.path_cursor = n,
                _ => {
                    let typed = ch.filter(|_| !mods.ctrl).map(String::from);
                    if !typed.is_some_and(|t| self.insert(&t)) {
                        return FileResult::None;
                    }
                }
            }
            return FileResult::Redraw;
        }
        let n = self.items.len();
        match sym {
            keysym::RETURN | keysym::KP_ENTER => {
                if self.chosen().len() > 1 {
                    return self.add();
                }
                return self.open(self.focus);
            }
            keysym::BACKSPACE => return self.parent(),
            keysym::UP
            | keysym::DOWN
            | keysym::PAGE_UP
            | keysym::PAGE_DOWN
            | keysym::HOME
            | keysym::END
                if n > 0 =>
            {
                let page = self.visible();
                let cur = self.focus;
                let to = match sym {
                    keysym::UP => cur.saturating_sub(1),
                    keysym::DOWN => cur + 1,
                    keysym::PAGE_UP => cur.saturating_sub(page),
                    keysym::PAGE_DOWN => cur + page,
                    keysym::HOME => 0,
                    _ => n,
                }
                .min(n - 1);
                if mods.shift {
                    let (a, b) = (self.anchor.min(to), self.anchor.max(to));
                    self.selected.iter_mut().for_each(|s| *s = false);
                    for k in a..=b {
                        if let Some(s) = self.selected.get_mut(k) {
                            *s = true;
                        }
                    }
                    self.focus = to;
                    self.ensure_visible(to);
                } else {
                    self.select_only(to);
                }
            }
            _ => {
                if mods.ctrl && matches!(ch, Some('a' | 'A')) {
                    for (s, it) in self.selected.iter_mut().zip(&self.items) {
                        *s = it.dir == self.folder;
                    }
                    return FileResult::Redraw;
                }
                // 按首字母跳到下一项
                let Some(ch) = ch
                    .filter(|c| !c.is_control())
                    .map(|c| c.to_lowercase().to_string())
                else {
                    return FileResult::None;
                };
                let start = self.focus + 1;
                let hit = (0..n)
                    .map(|k| (start + k) % n.max(1))
                    .find(|&i| self.items[i].label.to_lowercase().starts_with(&ch));
                match hit {
                    Some(i) => self.select_only(i),
                    None => return FileResult::None,
                }
            }
        }
        FileResult::Redraw
    }
}

fn human_size(b: u64) -> String {
    const K: f64 = 1024.0;
    let f = b as f64;
    if f < K {
        format!("{b} B")
    } else if f < K * K {
        format!("{:.1} KiB", f / K)
    } else if f < K * K * K {
        format!("{:.1} MiB", f / K / K)
    } else {
        format!("{:.2} GiB", f / K / K / K)
    }
}

#[cfg(test)]
mod tests {
    fn builtin() -> crate::app::FileFilter {
        crate::app::FileFilter {
            extensions: qy_player::assembly::BUILTIN_EXTENSIONS.to_vec(),
            names: "WAV / FLAC / AIFF".into(),
        }
    }

    use std::path::PathBuf;

    use super::{FileDialog, FileResult, human_size};

    #[test]
    fn lists_audio_and_dirs() {
        let dir = std::env::temp_dir().join(format!("qybit-files-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(dir.join("子目录"));
        for f in ["b.FLAC", "a.wav", "c.txt", ".hidden.wav", "d.aiff"] {
            let _ = std::fs::write(dir.join(f), b"x");
        }
        let mut d = FileDialog::new(dir.clone(), false, builtin());
        let names: Vec<&str> = d.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(names, ["子目录", "a.wav", "b.FLAC", "d.aiff"]);
        d.show_all = true;
        d.reload();
        assert_eq!(d.items.len(), 6);
        // 文件夹模式只列目录；没有选中时加入当前目录。
        let mut f = FileDialog::new(dir.clone(), true, builtin());
        assert_eq!(f.items.len(), 1);
        match f.add() {
            FileResult::Add(_, p) => assert_eq!(p, vec![dir.clone()]),
            _ => panic!("应加入当前目录"),
        }
        // 打开文件：加入该文件。
        let mut g = FileDialog::new(dir.clone(), false, builtin());
        match g.open(1) {
            FileResult::Add(_, p) => assert_eq!(p, vec![dir.join("a.wav")]),
            _ => panic!("应加入文件"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(matches!(
            FileDialog::new(PathBuf::from("/nonexistent-qybit"), false, builtin())
                .dir
                .to_str(),
            Some("/")
        ));
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KiB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MiB");
    }
}
