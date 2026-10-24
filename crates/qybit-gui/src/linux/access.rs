//! 读屏支持（LG2）：把界面控件描述为 AccessKit 无障碍树，经 `accesskit_unix` 以 AT-SPI 提供给
//! 读屏软件（Orca 等）。`accesskit_unix` 用 zbus（纯 Rust）连接 D-Bus 会话总线，不链接 libdbus /
//! libatspi，程序仍可 musl 完全静态链接。
//!
//! - 只在读屏软件等辅助技术启用时激活（`accesskit_unix` 读取 `org.a11y.Status`）；没有会话总线时
//!   不激活，界面照常运行。不带 `a11y` 特性构建时不连接 D-Bus，从不激活。
//! - 树由视图在每次重绘后生成（[`super::view::View::a11y_tree`]），这里与上次送出的树比较，只送出
//!   变化的节点。
//! - 读屏软件请求的动作（点击、设置值、移动焦点）转为界面线程的消息，由视图按控件的语义执行，
//!   与鼠标和键盘的操作走同一套逻辑。

use std::collections::HashMap;
use std::sync::mpsc::Sender;

use accesskit::{
    ActionHandler, ActionRequest, ActivationHandler, DeactivationHandler, Node, NodeId, TreeId,
    TreeInfo, TreeUpdate,
};
#[cfg(feature = "a11y")]
use accesskit_unix::Adapter;

/// 不带 `a11y` 特性时的替身：从不激活，读屏软件读不到控件。
#[cfg(not(feature = "a11y"))]
struct Adapter;

#[cfg(not(feature = "a11y"))]
impl Adapter {
    fn new(_: impl ActivationHandler, _: impl ActionHandler, _: impl DeactivationHandler) -> Self {
        Self
    }
    fn update_if_active(&mut self, _: impl FnOnce() -> TreeUpdate) {}
    fn set_root_window_bounds(&mut self, _: accesskit::Rect, _: accesskit::Rect) {}
    fn update_window_focus_state(&mut self, _: bool) {}
}

use super::paint::Rect;

/// 节点编号。固定控件用小编号，按行生成的节点用各自的区段加行号。
pub mod id {
    pub const ROOT: u64 = 1;
    pub const MENU_BAR: u64 = 2;
    pub const TOOLBAR: u64 = 3;
    pub const LIST: u64 = 4;
    pub const LOG: u64 = 5;
    pub const STATUS: u64 = 6;
    pub const MENU: u64 = 7;
    pub const COMBO_LIST: u64 = 8;
    pub const DIALOG: u64 = 9;
    /// 菜单栏的各个菜单（+ 序号）。
    pub const MENU_TITLE: u64 = 0x100;
    /// 弹出菜单的条目（+ 条目序号，含分隔线的位置）。
    pub const MENU_ITEM: u64 = 0x200;
    /// 按钮行的按钮（+ 序号：播放、停止、上一首、下一首、添加文件）。
    pub const BUTTON: u64 = 0x300;
    pub const COMBO: u64 = 0x310;
    pub const MONO: u64 = 0x311;
    pub const FLOAT: u64 = 0x312;
    pub const FORMAT: u64 = 0x313;
    pub const VOLUME_TEXT: u64 = 0x314;
    pub const VOLUME: u64 = 0x315;
    pub const SEEK: u64 = 0x316;
    /// 状态栏的三段（+ 序号）。
    pub const STATUS_PART: u64 = 0x400;
    pub const ABOUT_TEXT: u64 = 0x500;
    pub const ABOUT_OK: u64 = 0x501;
    pub const FILE_PATH: u64 = 0x600;
    pub const FILE_PATH_TEXT: u64 = 0x601;
    /// 文件面板的按钮（+ 序号：转到、上一级、主目录、根目录）。
    pub const FILE_BUTTON: u64 = 0x602;
    pub const FILE_ALL: u64 = 0x606;
    pub const FILE_LIST: u64 = 0x607;
    pub const FILE_ADD: u64 = 0x608;
    pub const FILE_CANCEL: u64 = 0x609;
    pub const FILE_INFO: u64 = 0x60a;
    /// 日志行（+ 行号）。
    pub const LOG_LINE: u64 = 0x1000_0000;
    /// 播放列表的行（+ 行号）。
    pub const ROW: u64 = 0x2000_0000;
    /// 设备下拉框的选项（+ 序号）。
    pub const COMBO_ITEM: u64 = 0x3000_0000;
    /// 文件面板列表的行（+ 行号）。
    pub const FILE_ROW: u64 = 0x4000_0000;
    /// 每个区段的大小。
    pub const SPAN: u64 = 0x1000_0000;

    /// `id` 属于区段 `base` 时返回其中的序号。
    pub fn index(id: u64, base: u64, span: u64) -> Option<usize> {
        id.checked_sub(base)
            .filter(|&k| k < span)
            .and_then(|k| usize::try_from(k).ok())
    }
}

/// 视图生成的树：全部节点与焦点所在的节点。
pub struct Tree {
    pub nodes: Vec<(NodeId, Node)>,
    pub focus: NodeId,
}

/// 窗口坐标中的矩形（物理像素）→ AccessKit 的矩形。
pub fn bounds(r: Rect) -> accesskit::Rect {
    accesskit::Rect {
        x0: f64::from(r.x),
        y0: f64::from(r.y),
        x1: f64::from(r.right()),
        y1: f64::from(r.bottom()),
    }
}

/// 读屏软件一侧的请求（在 `accesskit_unix` 的线程中产生，经通道交给界面线程）。
pub enum Request {
    /// 辅助技术启用：需要送出完整的树。
    Activate,
    /// 辅助技术停用。
    Deactivate,
    Action(ActionRequest),
}

struct Handler<M> {
    tx: Sender<M>,
    wrap: fn(Request) -> M,
}

impl<M: Send> ActivationHandler for Handler<M> {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        // 树只在界面线程生成：通知界面线程，之后由 `update_if_active` 送出完整的树。
        let _ = self.tx.send((self.wrap)(Request::Activate));
        None
    }
}

impl<M: Send> ActionHandler for Handler<M> {
    fn do_action(&mut self, request: ActionRequest) {
        let _ = self.tx.send((self.wrap)(Request::Action(request)));
    }
}

impl<M: Send> DeactivationHandler for Handler<M> {
    fn deactivate_accessibility(&mut self) {
        let _ = self.tx.send((self.wrap)(Request::Deactivate));
    }
}

pub struct Access {
    adapter: Adapter,
    /// 已送出完整的树；之后只送变化。
    active: bool,
    /// 上次送出的节点。
    sent: HashMap<NodeId, Node>,
    focus: NodeId,
}

impl Access {
    pub fn new<M: Send + 'static>(tx: Sender<M>, wrap: fn(Request) -> M) -> Self {
        let handler = || Handler {
            tx: tx.clone(),
            wrap,
        };
        Self {
            adapter: Adapter::new(handler(), handler(), handler()),
            active: false,
            sent: HashMap::new(),
            focus: NodeId(id::ROOT),
        }
    }

    /// 辅助技术已启用、需要维护树。
    pub fn active(&self) -> bool {
        self.active
    }

    pub fn activate(&mut self) {
        self.active = true;
        self.sent.clear();
    }

    pub fn deactivate(&mut self) {
        self.active = false;
        self.sent.clear();
    }

    /// 送出树：第一次送完整的树，之后只送与上次不同的节点（父节点的子节点列表变化时父节点
    /// 随之送出，移除的节点由此脱离树）。
    pub fn update(&mut self, tree: Tree) {
        if !self.active {
            return;
        }
        let first = self.sent.is_empty();
        let mut changed = Vec::new();
        let mut next = HashMap::with_capacity(tree.nodes.len());
        for (k, n) in tree.nodes {
            if first || self.sent.get(&k) != Some(&n) {
                changed.push((k, n.clone()));
            }
            next.insert(k, n);
        }
        if changed.is_empty() && tree.focus == self.focus {
            return;
        }
        self.sent = next;
        self.focus = tree.focus;
        let info = first.then(|| TreeInfo {
            root: NodeId(id::ROOT),
            toolkit_name: Some("QyBit".into()),
            toolkit_version: Some(env!("CARGO_PKG_VERSION").into()),
        });
        let update = TreeUpdate {
            nodes: changed,
            tree: info,
            tree_id: TreeId::ROOT,
            focus: tree.focus,
        };
        self.adapter.update_if_active(|| update);
    }

    /// 窗口在屏幕上的位置与大小（控件的位置相对于窗口）。
    pub fn window_bounds(&mut self, r: Rect) {
        let b = bounds(r);
        self.adapter.set_root_window_bounds(b, b);
    }

    pub fn window_focus(&mut self, focused: bool) {
        self.adapter.update_window_focus_state(focused);
    }
}
