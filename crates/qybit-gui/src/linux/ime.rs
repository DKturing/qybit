//! 输入法（LG2）：XIM 协议客户端（`xim` crate，经 x11rb 收发，纯 Rust，不链接 libX11）。
//!
//! - 按 `XMODIFIERS=@im=<名称>` 连接输入法；没有设置时不连接，按键照常直接处理。
//! - 输入上下文只在有文字输入焦点（文件面板的路径栏）且窗口在前台时获得焦点；其余时候按键不经过
//!   输入法，快捷键与列表操作不受输入法影响。
//! - 输入风格：输入法支持时用“光标处”（`PreeditPosition`：给出路径栏光标位置，输入法在那里显示
//!   预编辑文字与候选框），否则用“根窗口”（`PreeditNothing`：输入法在自己的窗口中显示）。不实现
//!   `PreeditCallbacks`（在路径栏内自绘预编辑文字）。
//! - 有焦点时按键先同步转发给输入法：输入法提交的文字（`Commit`）插入路径栏；输入法不处理的按键
//!   原样送回（`ForwardEvent`），按普通按键处理。
//! - 连接或通信失败时停用输入法并说明原因（之后路径栏仍可输入键盘直接给出的字符）。`xim` 在收到
//!   无法解码的文字时会 panic，这里捕获后同样停用，不让输入法的错误数据结束程序。

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use x11rb::protocol::Event;
use x11rb::protocol::xproto::{KEY_PRESS_EVENT, KEY_RELEASE_EVENT, KeyPressEvent};
use x11rb::rust_connection::RustConnection;
use xim::x11rb::X11rbClient;
use xim::{
    AttributeName, Client, ClientError, ClientHandler, ForwardEventFlag, InputStyle,
    InputStyleList, Point,
};

type Conn = X11rbClient<Arc<RustConnection>>;

/// XIM 事件掩码中的 KeyRelease（X 协议 `KeyReleaseMask`）：输入法要求转发松开按键时置位。
const KEY_RELEASE_MASK: u32 = 1 << 1;

pub struct Ime {
    client: Conn,
    h: Handler,
    /// 输入法名称（`XMODIFIERS` 中 `@im=` 之后的部分）。
    pub name: String,
}

struct Handler {
    win: u32,
    im: Option<u16>,
    ic: Option<u16>,
    style: Option<InputStyle>,
    /// 输入法要求转发的事件（`SetEventMask`）。
    forward_mask: u32,
    /// 输入上下文当前在输入法一侧是否有焦点。
    focused: bool,
    /// 应有焦点时的光标位置（窗口坐标）；`None` 为不应有焦点。
    want: Option<(i32, i32)>,
    /// 上次告诉输入法的光标位置。
    spot: Option<(i32, i32)>,
    commits: Vec<String>,
    keys: Vec<KeyPressEvent>,
}

fn err(e: ClientError) -> String {
    format!("输入法（XIM）通信失败：{e}")
}

/// `XMODIFIERS` 中的输入法名称；没有设置、没有 `@im=` 或为 `none` 时为 `None`。
pub fn configured_name(xmodifiers: Option<&str>) -> Option<String> {
    let v = xmodifiers?;
    let name = v.split('@').find_map(|p| p.strip_prefix("im="))?.trim();
    (!name.is_empty() && !name.eq_ignore_ascii_case("none")).then(|| name.to_string())
}

/// 打开输入法时告诉它的语言区域（与 libX11 相同，取程序的字符类型区域）：`LC_ALL` / `LC_CTYPE` /
/// `LANG` 中第一个非空值，都没有时为 `C`。提交的文字按 COMPOUND_TEXT 编码协商，与区域无关。
fn locale() -> String {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.is_empty())
        .unwrap_or_else(|| "C".into())
}

/// 从输入法支持的风格中选一个：光标处优先，其次根窗口。
fn choose(styles: &[InputStyle]) -> Option<InputStyle> {
    let prefer = [
        InputStyle::PREEDIT_POSITION | InputStyle::STATUS_NOTHING,
        InputStyle::PREEDIT_POSITION | InputStyle::STATUS_NONE,
        InputStyle::PREEDIT_NOTHING | InputStyle::STATUS_NOTHING,
        InputStyle::PREEDIT_NOTHING | InputStyle::STATUS_NONE,
        InputStyle::PREEDIT_NONE | InputStyle::STATUS_NONE,
    ];
    prefer.into_iter().find(|p| styles.contains(p))
}

fn point((x, y): (i32, i32)) -> Point {
    let c = |v: i32| v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
    Point { x: c(x), y: c(y) }
}

impl Handler {
    /// 让输入法一侧的焦点与光标位置符合 `want`。
    fn sync(&mut self, client: &mut Conn) -> Result<(), ClientError> {
        let (Some(im), Some(ic)) = (self.im, self.ic) else {
            return Ok(());
        };
        match self.want {
            Some(at) => {
                let position = self
                    .style
                    .is_some_and(|s| s.contains(InputStyle::PREEDIT_POSITION));
                if position && self.spot != Some(at) {
                    let attrs = client
                        .build_ic_attributes()
                        .nested_list(AttributeName::PreeditAttributes, |b| {
                            b.push(AttributeName::SpotLocation, point(at));
                        })
                        .build();
                    client.set_ic_values(im, ic, attrs)?;
                    self.spot = Some(at);
                }
                if !self.focused {
                    client.set_focus(im, ic)?;
                    self.focused = true;
                }
            }
            None => {
                if self.focused {
                    client.unset_focus(im, ic)?;
                    self.focused = false;
                }
            }
        }
        Ok(())
    }
}

impl ClientHandler<Conn> for Handler {
    fn handle_connect(&mut self, client: &mut Conn) -> Result<(), ClientError> {
        client.open(&locale())
    }

    fn handle_open(&mut self, client: &mut Conn, input_method_id: u16) -> Result<(), ClientError> {
        self.im = Some(input_method_id);
        client.get_im_values(input_method_id, &[AttributeName::QueryInputStyle])
    }

    fn handle_get_im_values(
        &mut self,
        client: &mut Conn,
        input_method_id: u16,
        attributes: xim::AHashMap<AttributeName, Vec<u8>>,
    ) -> Result<(), ClientError> {
        let styles = attributes
            .get(&AttributeName::QueryInputStyle)
            .and_then(|v| xim::read::<InputStyleList>(v).ok())
            .map(|l| l.styles)
            .unwrap_or_default();
        let Some(style) = choose(&styles) else {
            return Err(ClientError::InvalidReply);
        };
        self.style = Some(style);
        let mut b = client
            .build_ic_attributes()
            .push(AttributeName::InputStyle, style)
            .push(AttributeName::ClientWindow, self.win)
            .push(AttributeName::FocusWindow, self.win);
        if style.contains(InputStyle::PREEDIT_POSITION) {
            let at = self.want.unwrap_or((0, 0));
            b = b.nested_list(AttributeName::PreeditAttributes, |n| {
                n.push(AttributeName::SpotLocation, point(at));
            });
            self.spot = Some(at);
        }
        let attrs = b.build();
        client.create_ic(input_method_id, attrs)
    }

    fn handle_create_ic(
        &mut self,
        client: &mut Conn,
        _input_method_id: u16,
        input_context_id: u16,
    ) -> Result<(), ClientError> {
        self.ic = Some(input_context_id);
        self.focused = false;
        self.sync(client)
    }

    fn handle_destroy_ic(
        &mut self,
        _client: &mut Conn,
        _input_method_id: u16,
        _input_context_id: u16,
    ) -> Result<(), ClientError> {
        self.ic = None;
        self.focused = false;
        Ok(())
    }

    fn handle_disconnect(&mut self) {
        self.im = None;
        self.ic = None;
        self.focused = false;
    }

    fn handle_set_event_mask(
        &mut self,
        _client: &mut Conn,
        _input_method_id: u16,
        _input_context_id: u16,
        forward_event_mask: u32,
        _synchronous_event_mask: u32,
    ) -> Result<(), ClientError> {
        self.forward_mask = forward_event_mask;
        Ok(())
    }

    fn handle_commit(
        &mut self,
        _client: &mut Conn,
        _input_method_id: u16,
        _input_context_id: u16,
        text: &str,
    ) -> Result<(), ClientError> {
        self.commits.push(text.to_string());
        Ok(())
    }

    fn handle_forward_event(
        &mut self,
        _client: &mut Conn,
        _input_method_id: u16,
        _input_context_id: u16,
        _flag: ForwardEventFlag,
        xev: KeyPressEvent,
    ) -> Result<(), ClientError> {
        if xev.response_type & 0x7f == KEY_PRESS_EVENT {
            self.keys.push(xev);
        }
        Ok(())
    }
}

/// 执行一步 XIM 处理；把 `xim` 内部的 panic（输入法发来无法解码的文字）转为错误。
fn guarded<T>(f: impl FnOnce() -> Result<T, ClientError>) -> Result<T, String> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r.map_err(err),
        Err(_) => Err("输入法（XIM）发来无法解码的数据".into()),
    }
}

impl Ime {
    /// 连接 `XMODIFIERS` 指定的输入法。没有指定时返回 `Ok(None)`。连接过程的后续步骤在事件
    /// 到达时完成（[`Ime::filter`]）。
    pub fn connect(
        conn: Arc<RustConnection>,
        screen: usize,
        win: u32,
    ) -> Result<Option<Ime>, String> {
        let Some(name) = configured_name(std::env::var("XMODIFIERS").ok().as_deref()) else {
            return Ok(None);
        };
        let client = guarded(|| X11rbClient::init(conn, screen, Some(&name)))
            .map_err(|e| format!("{e}（输入法 {name}）"))?;
        Ok(Some(Ime {
            client,
            h: Handler {
                win,
                im: None,
                ic: None,
                style: None,
                forward_mask: 0,
                focused: false,
                want: None,
                spot: None,
                commits: Vec::new(),
                keys: Vec::new(),
            },
            name,
        }))
    }

    /// 交给输入法客户端的 X 事件（与输入法通信的窗口上的事件）；返回 `true` 表示已处理。
    pub fn filter(&mut self, ev: &Event) -> Result<bool, String> {
        let Ime { client, h, .. } = self;
        guarded(|| client.filter_event(ev, h))
    }

    /// 已建立输入上下文时的输入风格说明。
    pub fn ready(&self) -> Option<&'static str> {
        self.h.ic?;
        Some(match self.h.style {
            Some(s) if s.contains(InputStyle::PREEDIT_POSITION) => "光标处",
            _ => "根窗口",
        })
    }

    /// 输入上下文有焦点时把按键（按下或松开）转发给输入法，返回 `true`；之后按 [`Ime::take`]
    /// 取回提交的文字与输入法送回的按键。没有焦点时返回 `false`，由调用方直接处理。
    pub fn key(&mut self, e: &KeyPressEvent) -> Result<bool, String> {
        let (Some(im), Some(ic)) = (self.h.im, self.h.ic) else {
            return Ok(false);
        };
        if !self.h.focused {
            return Ok(false);
        }
        let release = e.response_type & 0x7f == KEY_RELEASE_EVENT;
        if release && self.h.forward_mask & KEY_RELEASE_MASK == 0 {
            return Ok(true);
        }
        let client = &mut self.client;
        guarded(|| client.forward_event(im, ic, ForwardEventFlag::SYNCHRONOUS, e))?;
        Ok(true)
    }

    /// 设置应有的焦点：`Some(光标位置)` 为路径栏有输入焦点且窗口在前台，`None` 为没有。
    pub fn want(&mut self, at: Option<(i32, i32)>) -> Result<(), String> {
        if self.h.want == at {
            return Ok(());
        }
        self.h.want = at;
        let Ime { client, h, .. } = self;
        guarded(|| h.sync(client))
    }

    /// 取走输入法提交的文字与送回的按键。
    pub fn take(&mut self) -> (Vec<String>, Vec<KeyPressEvent>) {
        (
            std::mem::take(&mut self.h.commits),
            std::mem::take(&mut self.h.keys),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{InputStyle, choose, configured_name};

    #[test]
    fn xmodifiers() {
        assert_eq!(configured_name(None), None);
        assert_eq!(configured_name(Some("")), None);
        assert_eq!(configured_name(Some("@im=fcitx")).as_deref(), Some("fcitx"));
        assert_eq!(configured_name(Some("@im=ibus")).as_deref(), Some("ibus"));
        assert_eq!(configured_name(Some("@im=none")), None);
        assert_eq!(configured_name(Some("@im=")), None);
        assert_eq!(configured_name(Some("foo")), None);
    }

    #[test]
    fn style_preference() {
        let pos = InputStyle::PREEDIT_POSITION | InputStyle::STATUS_NOTHING;
        let root = InputStyle::PREEDIT_NOTHING | InputStyle::STATUS_NOTHING;
        let cb = InputStyle::PREEDIT_CALLBACKS | InputStyle::STATUS_CALLBACKS;
        assert_eq!(choose(&[root, pos, cb]), Some(pos));
        assert_eq!(choose(&[cb, root]), Some(root));
        assert_eq!(choose(&[cb]), None);
        assert_eq!(choose(&[]), None);
    }
}
