//! 主窗口的无障碍树与读屏软件的动作（LG2，见 [`crate::linux::access`]）。

use accesskit::{Action, ActionData, HasPopup, Node, NodeId, Role, Toggled};

use super::{
    Area, COLUMNS, Canvas, Entry, Focus, ID_ADD_FILES, ID_NEXT, ID_PLAY, ID_PREV, ID_STOP, MENUS,
    Modal, Popup, View, menu_entries,
};
use crate::linux::access::{Tree, bounds, id};

/// 播放进度条的步长（千分比）：读屏软件的“增加 / 减少”每次移动 1%。
const SEEK_STEP: u32 = 10;
/// 音量滑块的步长（百分比）。
const VOLUME_STEP: u32 = 5;

fn label(role: Role, text: &str) -> Node {
    let mut n = Node::new(role);
    n.set_label(text);
    n
}

/// 只显示文字的节点（`Label` 的名称取自值）。
fn text(value: &str, r: super::Rect) -> Node {
    let mut n = Node::new(Role::Label);
    n.set_value(value);
    n.set_bounds(bounds(r));
    n
}

impl View {
    /// 当前界面的无障碍树。`cells` 与绘制列表时相同（行、列 → 文字）。
    pub fn a11y_tree(
        &mut self,
        c: &mut Canvas,
        cells: &mut dyn FnMut(usize, usize) -> String,
    ) -> Tree {
        self.layout(c);
        let mut nodes = Vec::new();
        let mut kids = Vec::new();
        let mut focus = NodeId(id::ROOT);

        // 菜单栏
        let mut bar = Node::new(Role::MenuBar);
        bar.set_bounds(bounds(self.menu_rect));
        let open = match self.popup {
            Popup::Menu { index, hover } => Some((index, hover)),
            _ => None,
        };
        for (i, (title, _)) in MENUS.iter().enumerate() {
            let k = NodeId(id::MENU_TITLE + i as u64);
            let mut n = label(Role::MenuItem, title);
            if let Some(r) = self.menu_items.get(i) {
                n.set_bounds(bounds(*r));
            }
            n.set_has_popup(HasPopup::Menu);
            n.set_expanded(open.is_some_and(|(o, _)| o == i));
            n.add_action(Action::Click);
            if let Some((o, None)) = open
                && o == i
            {
                focus = k;
            }
            bar.push_child(k);
            nodes.push((k, n));
        }
        nodes.push((NodeId(id::MENU_BAR), bar));
        kids.push(NodeId(id::MENU_BAR));

        // 按钮行、选项、格式与音量、进度条
        let mut tb = Node::new(Role::Toolbar);
        tb.set_label("播放控制");
        let labels = [self.play_label, "停止", "上一首", "下一首", "添加文件…"];
        for (i, (r, l)) in self.buttons.iter().zip(labels).enumerate() {
            let k = NodeId(id::BUTTON + i as u64);
            let mut n = label(Role::Button, l);
            n.set_bounds(bounds(*r));
            n.add_action(Action::Click);
            tb.push_child(k);
            nodes.push((k, n));
        }
        let combo_open = matches!(self.popup, Popup::Combo { .. });
        let mut combo = label(Role::ComboBox, "输出设备");
        combo.set_bounds(bounds(self.combo));
        combo.set_value(self.devices.get(self.device).map_or("", String::as_str));
        combo.set_has_popup(HasPopup::Listbox);
        combo.set_expanded(combo_open);
        combo.add_action(Action::Click);
        combo.add_action(if combo_open {
            Action::Collapse
        } else {
            Action::Expand
        });
        if let Popup::Combo { hover, .. } = self.popup {
            let mut list = label(Role::ListBox, "输出设备");
            list.set_bounds(bounds(self.combo_box()));
            for (i, name) in self.devices.iter().enumerate() {
                let k = NodeId(id::COMBO_ITEM + i as u64);
                let mut n = label(Role::ListBoxOption, name);
                n.set_selected(i == self.device);
                n.set_position_in_set(i + 1);
                n.set_size_of_set(self.devices.len());
                n.add_action(Action::Click);
                if hover == Some(i) {
                    focus = k;
                }
                list.push_child(k);
                nodes.push((k, n));
            }
            if hover.is_none() {
                focus = NodeId(id::COMBO);
            }
            combo.push_child(NodeId(id::COMBO_LIST));
            nodes.push((NodeId(id::COMBO_LIST), list));
        }
        nodes.push((NodeId(id::COMBO), combo));
        tb.push_child(NodeId(id::COMBO));
        for (k, r, l, on) in [
            (
                id::MONO,
                self.mono_rect,
                "单声道复制到左右两路（会改变声道）",
                self.mono,
            ),
            (
                id::FLOAT,
                self.float_rect,
                "浮点转整数（会改变样本）",
                self.float,
            ),
        ] {
            let mut n = label(Role::CheckBox, l);
            n.set_bounds(bounds(r));
            n.set_toggled(if on { Toggled::True } else { Toggled::False });
            n.add_action(Action::Click);
            tb.push_child(NodeId(k));
            nodes.push((NodeId(k), n));
        }
        if !self.format.is_empty() {
            nodes.push((NodeId(id::FORMAT), text(&self.format, self.format_rect)));
            tb.push_child(NodeId(id::FORMAT));
        }
        if !self.vol_text.is_empty() {
            nodes.push((
                NodeId(id::VOLUME_TEXT),
                text(&self.vol_text, self.vol_label),
            ));
            tb.push_child(NodeId(id::VOLUME_TEXT));
        }
        for (k, sl, name, unit) in [
            (
                id::VOLUME,
                self.volume,
                "播放时的设备硬件音量（百分比）",
                1.0,
            ),
            (id::SEEK, self.seek, "播放进度（百分比）", 10.0),
        ] {
            let mut n = label(Role::Slider, name);
            n.set_bounds(bounds(sl.rect));
            if sl.enabled {
                n.set_min_numeric_value(0.0);
                n.set_max_numeric_value(f64::from(sl.max) / unit);
                n.set_numeric_value(f64::from(sl.value.min(sl.max)) / unit);
                n.set_numeric_value_step(1.0);
                for a in [Action::SetValue, Action::Increment, Action::Decrement] {
                    n.add_action(a);
                }
            } else {
                n.set_disabled();
            }
            tb.push_child(NodeId(k));
            nodes.push((NodeId(k), n));
        }
        nodes.push((NodeId(id::TOOLBAR), tb));
        kids.push(NodeId(id::TOOLBAR));

        // 播放列表
        let mut list = label(Role::ListBox, "播放列表");
        list.set_bounds(bounds(self.list_rect));
        list.set_multiselectable();
        let rr = self.rows_rect();
        let rh = self.row_h();
        for i in 0..self.count {
            let k = NodeId(id::ROW + i as u64);
            let mark = cells(i, 0);
            let mut name = match mark.as_str() {
                "▶" => "正在播放：".to_string(),
                "‖" => "已暂停：".to_string(),
                _ => String::new(),
            };
            let parts: Vec<String> = (1..COLUMNS.len())
                .map(|col| cells(i, col))
                .filter(|t| !t.is_empty())
                .collect();
            name.push_str(&parts.join("，"));
            let mut n = label(Role::ListBoxOption, &name);
            n.set_selected(self.selected.get(i).copied().unwrap_or(false));
            n.set_position_in_set(i + 1);
            n.set_size_of_set(self.count);
            if i >= self.top {
                let y = rr.y + (i - self.top) as i32 * rh;
                if y < rr.bottom() {
                    n.set_bounds(bounds(super::Rect::new(rr.x, y, rr.w, rh).intersect(&rr)));
                }
            }
            n.add_action(Action::Click);
            n.add_action(Action::Focus);
            list.push_child(k);
            nodes.push((k, n));
        }
        nodes.push((NodeId(id::LIST), list));
        kids.push(NodeId(id::LIST));
        if matches!(self.modal, Modal::None) && matches!(self.popup, Popup::None) {
            focus = match self.focus {
                Focus::List if self.count > 0 => {
                    NodeId(id::ROW + self.focus_row.min(self.count - 1) as u64)
                }
                Focus::List => NodeId(id::LIST),
                Focus::Log => NodeId(id::LOG),
            };
        }

        // 日志
        if self.log_visible {
            let mut log = label(Role::Log, "日志");
            log.set_bounds(bounds(self.log_rect));
            for (i, line) in self.log.iter().enumerate() {
                let k = NodeId(id::LOG_LINE + i as u64);
                let mut n = Node::new(Role::Label);
                n.set_value(line.text.as_str());
                log.push_child(k);
                nodes.push((k, n));
            }
            nodes.push((NodeId(id::LOG), log));
            kids.push(NodeId(id::LOG));
        }

        // 状态栏
        let mut status = label(Role::Status, "状态栏");
        status.set_bounds(bounds(self.status_rect));
        let [a, b, l] = self.status_parts();
        let first = self.status_override.as_deref().unwrap_or(&self.status);
        let mut lamp = text(&self.lamp_text, l);
        lamp.set_description("采样率指示灯");
        for (i, n) in [
            Some(text(first, a)),
            (!self.fidelity.is_empty()).then(|| text(&self.fidelity, b)),
            Some(lamp),
        ]
        .into_iter()
        .enumerate()
        {
            if let Some(n) = n {
                let k = NodeId(id::STATUS_PART + i as u64);
                status.push_child(k);
                nodes.push((k, n));
            }
        }
        nodes.push((NodeId(id::STATUS), status));
        kids.push(NodeId(id::STATUS));

        // 弹出菜单
        if let Some((index, hover)) = open {
            let (bx, items) = self.menu_box(c, index);
            let mut menu = label(Role::Menu, MENUS.get(index).map_or("", |m| m.0));
            menu.set_bounds(bounds(bx));
            for (k, ((r, _), e)) in items.iter().zip(menu_entries(index)).enumerate() {
                let Entry::Item(cmd, title, accel) = e else {
                    continue;
                };
                let nid = NodeId(id::MENU_ITEM + k as u64);
                let mut n = match self.checked(cmd) {
                    Some(on) => {
                        let mut n = label(Role::MenuItemCheckBox, title);
                        n.set_toggled(if on { Toggled::True } else { Toggled::False });
                        n
                    }
                    None => label(Role::MenuItem, title),
                };
                n.set_bounds(bounds(*r));
                if !accel.is_empty() {
                    n.set_keyboard_shortcut(accel);
                }
                n.add_action(Action::Click);
                if hover == Some(k) {
                    focus = nid;
                }
                menu.push_child(nid);
                nodes.push((nid, n));
            }
            nodes.push((NodeId(id::MENU), menu));
            kids.push(NodeId(id::MENU));
        }

        // 对话框
        match &self.modal {
            Modal::About(t) => {
                let t = t.clone();
                let (bx, ok, _) = self.about_geom(c, &t);
                let mut d = label(Role::Dialog, "关于 QyBit");
                d.set_bounds(bounds(bx));
                d.set_modal();
                nodes.push((NodeId(id::ABOUT_TEXT), text(&t, bx)));
                let mut b = label(Role::Button, "确定");
                b.set_bounds(bounds(ok));
                b.add_action(Action::Click);
                nodes.push((NodeId(id::ABOUT_OK), b));
                d.set_children(vec![NodeId(id::ABOUT_TEXT), NodeId(id::ABOUT_OK)]);
                nodes.push((NodeId(id::DIALOG), d));
                kids.push(NodeId(id::DIALOG));
                focus = NodeId(id::ABOUT_OK);
            }
            Modal::Files(d) => {
                focus = d.a11y_nodes(&mut nodes);
                kids.push(NodeId(id::DIALOG));
            }
            Modal::None => {}
        }

        let mut root = label(Role::Window, "QyBit");
        root.set_bounds(bounds(super::Rect::new(0, 0, self.w, self.h)));
        root.set_children(kids);
        nodes.push((NodeId(id::ROOT), root));
        Tree { nodes, focus }
    }

    /// 执行读屏软件请求的动作，与鼠标、键盘的同一操作效果相同。不认识或当前不可用的动作忽略。
    pub fn a11y_action(
        &mut self,
        c: &mut Canvas,
        target: u64,
        action: Action,
        data: Option<&ActionData>,
    ) -> Vec<super::Act> {
        use super::Act;
        let mut acts = Vec::new();
        let click = matches!(action, Action::Click);
        if let Modal::Files(d) = &mut self.modal {
            let r = d.a11y_action(c, target, action, data);
            self.files_result(r, &mut acts);
            return acts;
        }
        if let Modal::About(_) = self.modal {
            if target == id::ABOUT_OK && click {
                self.close_overlay();
            }
            return acts;
        }
        if let Some(i) = id::index(target, id::MENU_TITLE, 16) {
            if click && i < MENUS.len() {
                if matches!(self.popup, Popup::Menu { index, .. } if index == i) {
                    self.close_overlay();
                } else {
                    self.popup = Popup::Menu {
                        index: i,
                        hover: None,
                    };
                    self.dirty = super::ALL;
                    self.overlay_dirty = true;
                }
            }
            return acts;
        }
        if let Some(k) = id::index(target, id::MENU_ITEM, 0x100) {
            if let (true, Popup::Menu { index, .. }) = (click, self.popup) {
                let cmd = match menu_entries(index).get(k) {
                    Some(Entry::Item(cmd, ..)) => Some(*cmd),
                    _ => None,
                };
                self.close_overlay();
                if let Some(cmd) = cmd {
                    acts.push(Act::Command(cmd));
                }
            }
            return acts;
        }
        if let Some(i) = id::index(target, id::COMBO_ITEM, id::SPAN) {
            if click && i < self.devices.len() {
                self.close_overlay();
                if i != self.device {
                    self.device = i;
                    acts.push(Act::Device(i));
                }
            }
            return acts;
        }
        // 主窗口的控件：先关闭打开着的弹出层（与鼠标点在弹出层之外相同）。
        if !matches!(self.popup, Popup::None) && target != id::COMBO {
            self.close_overlay();
        }
        if let Some(i) = id::index(target, id::BUTTON, 5) {
            if click {
                acts.push(Act::Command(match i {
                    0 => ID_PLAY,
                    1 => ID_STOP,
                    2 => ID_PREV,
                    3 => ID_NEXT,
                    _ => ID_ADD_FILES,
                }));
            }
            return acts;
        }
        if let Some(i) = id::index(target, id::ROW, id::SPAN) {
            if matches!(action, Action::Click | Action::Focus) && i < self.count {
                self.focus = Focus::List;
                self.select(&[i]);
                self.mark(Area::Log);
            }
            return acts;
        }
        match target {
            id::COMBO => match (action, self.popup) {
                (Action::Click | Action::Collapse, Popup::Combo { .. }) => self.close_overlay(),
                (Action::Click | Action::Expand, Popup::None) => {
                    acts.push(Act::RefreshDevices);
                    let rows = self.combo_rows();
                    let top = self.device.saturating_sub(rows.saturating_sub(1));
                    self.popup = Popup::Combo {
                        hover: Some(self.device),
                        top: top.min(self.devices.len().saturating_sub(rows)),
                    };
                    self.overlay_dirty = true;
                    self.mark(Area::Toolbar);
                }
                _ => {}
            },
            id::MONO if click => {
                self.mono = !self.mono;
                self.mark(Area::Options);
                acts.push(Act::Mono(self.mono));
            }
            id::FLOAT if click => {
                self.float = !self.float;
                self.mark(Area::Options);
                acts.push(Act::Float(self.float));
            }
            id::SEEK | id::VOLUME => {
                let seek = target == id::SEEK;
                let (sl, step, unit) = if seek {
                    (self.seek, SEEK_STEP, 10.0)
                } else {
                    (self.volume, VOLUME_STEP, 1.0)
                };
                if !sl.enabled {
                    return acts;
                }
                let v = match (action, data) {
                    (Action::SetValue, Some(ActionData::NumericValue(n))) if n.is_finite() => {
                        (n * unit).round().clamp(0.0, f64::from(sl.max)) as u32
                    }
                    (Action::Increment, _) => (sl.value + step).min(sl.max),
                    (Action::Decrement, _) => sl.value.saturating_sub(step),
                    _ => return acts,
                };
                if seek {
                    self.seek.value = v;
                    self.mark(Area::Seek);
                    acts.push(Act::Seek(v));
                } else {
                    self.volume.value = v;
                    self.mark(Area::Format);
                    acts.push(Act::Volume(v));
                }
            }
            id::LIST if matches!(action, Action::Focus) => {
                self.focus = Focus::List;
                self.mark(Area::List);
                self.mark(Area::Log);
            }
            id::LOG if matches!(action, Action::Focus) => {
                self.focus = Focus::Log;
                self.mark(Area::List);
                self.mark(Area::Log);
            }
            _ => {}
        }
        acts
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use accesskit::{Action, ActionData, Node, NodeId, Role, Toggled};

    use super::super::{Act, Canvas, View};
    use crate::app::{ID_PLAY, ID_STOP};
    use crate::linux::access::{Tree, id};

    fn cells(r: usize, c: usize) -> String {
        match c {
            0 if r == 0 => "▶".into(),
            1 => (r + 1).to_string(),
            2 => format!("曲目{r}.flac"),
            _ => String::new(),
        }
    }

    fn tree(v: &mut View, c: &mut Canvas) -> (Tree, HashMap<u64, Node>) {
        v.paint(c, &mut cells);
        let t = v.a11y_tree(c, &mut cells);
        let map = t.nodes.iter().map(|(k, n)| (k.0, n.clone())).collect();
        (t, map)
    }

    /// 树的结构：编号不重复；每个子节点都存在且只有一个父节点；从根可以到达所有节点；焦点在树中。
    fn check(t: &Tree) {
        let mut seen = HashSet::new();
        for (k, _) in &t.nodes {
            assert!(seen.insert(k.0), "节点 {:#x} 重复", k.0);
        }
        let map: HashMap<u64, &Node> = t.nodes.iter().map(|(k, n)| (k.0, n)).collect();
        let mut parent = HashMap::new();
        for (k, n) in &t.nodes {
            for ch in n.children() {
                assert!(
                    map.contains_key(&ch.0),
                    "{:#x} 的子节点 {:#x} 不存在",
                    k.0,
                    ch.0
                );
                assert!(
                    parent.insert(ch.0, k.0).is_none(),
                    "{:#x} 有两个父节点",
                    ch.0
                );
            }
        }
        for (k, _) in &t.nodes {
            if k.0 != id::ROOT {
                assert!(parent.contains_key(&k.0), "{:#x} 不在树中", k.0);
            }
        }
        assert!(
            map.contains_key(&t.focus.0),
            "焦点 {:#x} 不在树中",
            t.focus.0
        );
    }

    fn name(n: &Node) -> String {
        n.label()
            .or(n.value())
            .map(str::to_string)
            .unwrap_or_default()
    }

    #[test]
    fn main_window_tree() {
        let mut c = Canvas::new(1000, 700, 1.0);
        let mut v = View::new(1.0);
        v.set_count(3);
        v.set_devices(vec!["（请选择输出设备）".into(), "声卡 A".into()], 1);
        v.set_log("第一行\r\n第二行\r\n".into());
        v.set_status("就绪".into(), String::new(), String::new());
        let (t, m) = tree(&mut v, &mut c);
        check(&t);
        assert_eq!(m[&(id::BUTTON + 1)].role(), Role::Button);
        assert_eq!(name(&m[&(id::BUTTON + 1)]), "停止");
        assert_eq!(m[&id::COMBO].value(), Some("声卡 A"));
        assert_eq!(m[&id::MONO].toggled(), Some(Toggled::False));
        assert_eq!(name(&m[&id::ROW]), "正在播放：1，曲目0.flac");
        assert_eq!(name(&m[&(id::ROW + 2)]), "3，曲目2.flac");
        assert_eq!(m[&id::LOG].children().len(), 2);
        assert_eq!(m[&id::LOG_LINE].value(), Some("第一行"));
        assert_eq!(m[&id::STATUS_PART].value(), Some("就绪"));
        assert_eq!(t.focus, NodeId(id::ROW));
        assert!(m[&id::SEEK].is_disabled());
    }

    #[test]
    fn actions_match_mouse_and_keyboard() {
        let mut c = Canvas::new(1000, 700, 1.0);
        let mut v = View::new(1.0);
        v.set_count(3);
        v.set_seek(Some(0));
        tree(&mut v, &mut c);
        let a = v.a11y_action(&mut c, id::BUTTON, Action::Click, None);
        assert!(matches!(a.as_slice(), [Act::Command(ID_PLAY)]));
        let a = v.a11y_action(&mut c, id::BUTTON + 1, Action::Click, None);
        assert!(matches!(a.as_slice(), [Act::Command(ID_STOP)]));
        let a = v.a11y_action(&mut c, id::MONO, Action::Click, None);
        assert!(matches!(a.as_slice(), [Act::Mono(true)]));
        let n = ActionData::NumericValue(42.5);
        let a = v.a11y_action(&mut c, id::SEEK, Action::SetValue, Some(&n));
        assert!(matches!(a.as_slice(), [Act::Seek(425)]), "{a:?}");
        let a = v.a11y_action(&mut c, id::SEEK, Action::Increment, None);
        assert!(matches!(a.as_slice(), [Act::Seek(435)]), "{a:?}");
        let bad = ActionData::NumericValue(f64::NAN);
        assert!(
            v.a11y_action(&mut c, id::SEEK, Action::SetValue, Some(&bad))
                .is_empty()
        );
        // 选中第 3 行：焦点随之移动。
        assert!(
            v.a11y_action(&mut c, id::ROW + 2, Action::Focus, None)
                .is_empty()
        );
        assert_eq!(v.selected(), vec![2]);
        let (t, m) = tree(&mut v, &mut c);
        assert_eq!(t.focus, NodeId(id::ROW + 2));
        assert!(m[&(id::ROW + 2)].is_selected() == Some(true));
        // 菜单：打开“播放”，点“停止(S)”。
        v.a11y_action(&mut c, id::MENU_TITLE + 1, Action::Click, None);
        let (t, m) = tree(&mut v, &mut c);
        check(&t);
        assert_eq!(t.focus, NodeId(id::MENU_TITLE + 1));
        assert_eq!(name(&m[&(id::MENU_ITEM + 1)]), "停止(S)");
        let a = v.a11y_action(&mut c, id::MENU_ITEM + 1, Action::Click, None);
        assert!(matches!(a.as_slice(), [Act::Command(ID_STOP)]));
        let (t, m) = tree(&mut v, &mut c);
        assert!(!m.contains_key(&id::MENU));
        check(&t);
        // 超出范围的编号与不支持的动作被忽略。
        assert!(
            v.a11y_action(&mut c, id::ROW + 99, Action::Click, None)
                .is_empty()
        );
        assert!(
            v.a11y_action(&mut c, id::STATUS, Action::Click, None)
                .is_empty()
        );
    }

    #[test]
    fn file_panel_tree_and_path_text() {
        let dir = std::env::temp_dir().join(format!("qybit-a11y-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("子目录"));
        let mut c = Canvas::new(1000, 700, 1.0);
        let mut v = View::new(1.0);
        v.last_dir = Some(dir.clone());
        v.open_files(
            false,
            crate::app::FileFilter {
                extensions: vec!["wav"],
                names: "WAV".into(),
            },
        );
        let (t, m) = tree(&mut v, &mut c);
        check(&t);
        assert_eq!(m[&id::DIALOG].role(), Role::Dialog);
        assert_eq!(name(&m[&id::FILE_ROW]), "子目录，文件夹");
        let path = dir.display().to_string();
        assert_eq!(m[&id::FILE_PATH_TEXT].value(), Some(path.as_str()));
        assert_eq!(
            m[&id::FILE_PATH_TEXT].character_lengths().len(),
            path.chars().count()
        );
        let set = ActionData::Value("/".into());
        v.a11y_action(&mut c, id::FILE_PATH, Action::SetValue, Some(&set));
        let (t, m) = tree(&mut v, &mut c);
        assert_eq!(t.focus, NodeId(id::FILE_PATH));
        assert_eq!(m[&id::FILE_PATH_TEXT].value(), Some("/"));
        let sel = m[&id::FILE_PATH].text_selection().copied();
        assert_eq!(sel.map(|s| s.focus.character_index), Some(1));
        v.a11y_action(&mut c, id::FILE_CANCEL, Action::Click, None);
        let (t, m) = tree(&mut v, &mut c);
        assert!(!m.contains_key(&id::DIALOG));
        check(&t);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
