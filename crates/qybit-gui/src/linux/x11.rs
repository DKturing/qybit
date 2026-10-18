//! X11 边界：用 x11rb 的纯 Rust 协议实现（`RustConnection`，不链接 libX11 / libxcb，不启用
//! `allow-unsafe-code`），程序可 musl 完全静态链接；Wayland 桌面经 XWayland 运行。
//!
//! 负责：连接与窗口、像素提交（`PutImage`，按最大请求长度分条）、键码到键符号、XDND 文件拖放
//! （接收方，版本 5）、剪贴板（`CLIPBOARD` 选择的所有者，复制日志用）、全局媒体键（在根窗口上
//! `GrabKey`）、光标形状、窗口位置的读取与恢复、`Xft.dpi` 缩放。事件由单独的线程阻塞读取，经通道
//! 交给界面线程（[`super::run`]）。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use qy_ui::settings::Window as Placement;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConfigureWindowAux,
    ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, GrabMode, ImageFormat, ImageOrder,
    KeyButMask, ModMask, PropMode, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent,
    SelectionRequestEvent, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE};

use super::paint::{Canvas, Rect};

pub type Res<T> = Result<T, Box<dyn std::error::Error>>;

/// 键符号（X11 `keysymdef.h` 与 `XF86keysym.h`）。
pub mod keysym {
    pub const BACKSPACE: u32 = 0xff08;
    pub const TAB: u32 = 0xff09;
    pub const RETURN: u32 = 0xff0d;
    pub const ESCAPE: u32 = 0xff1b;
    pub const HOME: u32 = 0xff50;
    pub const LEFT: u32 = 0xff51;
    pub const UP: u32 = 0xff52;
    pub const RIGHT: u32 = 0xff53;
    pub const DOWN: u32 = 0xff54;
    pub const PAGE_UP: u32 = 0xff55;
    pub const PAGE_DOWN: u32 = 0xff56;
    pub const END: u32 = 0xff57;
    pub const KP_ENTER: u32 = 0xff8d;
    pub const F10: u32 = 0xffc7;
    pub const DELETE: u32 = 0xffff;
    pub const SPACE: u32 = 0x20;
    pub const AUDIO_PLAY: u32 = 0x1008_ff14;
    pub const AUDIO_STOP: u32 = 0x1008_ff15;
    pub const AUDIO_PREV: u32 = 0x1008_ff16;
    pub const AUDIO_NEXT: u32 = 0x1008_ff17;
    pub const AUDIO_PAUSE: u32 = 0x1008_ff31;
    /// 全局媒体键（在根窗口上抓取）。
    pub const MEDIA: [u32; 5] = [AUDIO_PLAY, AUDIO_STOP, AUDIO_PREV, AUDIO_NEXT, AUDIO_PAUSE];
}

/// 修饰键状态。
#[derive(Debug, Clone, Copy, Default)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

impl Mods {
    pub fn from_state(s: KeyButMask) -> Self {
        Self {
            shift: s.contains(KeyButMask::SHIFT),
            ctrl: s.contains(KeyButMask::CONTROL),
            alt: s.contains(KeyButMask::MOD1),
        }
    }
}

/// 光标形状（X 核心光标字体 `cursor` 中的字形号，`cursorfont.h`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
    Arrow,
    /// 上下双箭头（`XC_sb_v_double_arrow` 116）：列表与日志之间的分隔条。
    ResizeV,
    /// 左右双箭头（`XC_sb_h_double_arrow` 108）：列标题的分隔线。
    ResizeH,
}

struct Atoms {
    protocols: u32,
    delete: u32,
    net_wm_name: u32,
    net_wm_pid: u32,
    net_wm_state: u32,
    max_v: u32,
    max_h: u32,
    utf8: u32,
    clipboard: u32,
    targets: u32,
    aware: u32,
    enter: u32,
    position: u32,
    status: u32,
    drop: u32,
    finished: u32,
    leave: u32,
    selection: u32,
    copy: u32,
    uri_list: u32,
    drop_target: u32,
}

fn atom(c: &RustConnection, name: &str) -> Res<u32> {
    Ok(c.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

pub struct X {
    pub conn: Arc<RustConnection>,
    pub win: u32,
    root: u32,
    gc: u32,
    atoms: Atoms,
    max_req: usize,
    /// 服务器的图像字节序：低字节在前时每像素为 B G R X，否则 X R G B。
    lsb: bool,
    min_keycode: u8,
    per_keycode: usize,
    keysyms: Vec<u32>,
    cursors: [u32; 3],
    cursor: Cursor,
    /// 剪贴板内容（本程序为 `CLIPBOARD` 的所有者时）。
    clipboard: String,
    /// 正在进行的拖放的来源窗口。
    drag_source: u32,
    /// 已抓取的媒体键键码。
    grabbed: Vec<u8>,
    /// 累计提交的像素字节数（诊断：经 SSH 转发时界面更新的数据量）。
    pub sent_bytes: u64,
    pub scale: f32,
}

impl X {
    /// 连接显示服务并创建主窗口（尚未映射）。`place` 为上次保存的窗口位置。
    pub fn open(title: &str, place: Option<Placement>) -> Res<X> {
        let (conn, screen_num) = RustConnection::connect(None).map_err(|e| {
            format!(
                "连接不到 X 显示服务（{e}）。qybit-gui 需要图形桌面（X11，或 Wayland 桌面自带的 \
                 XWayland）：在桌面的终端窗口里运行；经 SSH 从另一台有桌面的电脑运行时用 ssh -X 登录\
                 （Windows 可用自带 X 服务器的 MobaXterm）。没有桌面的系统请使用命令行 qybit。"
            )
        })?;
        let conn = Arc::new(conn);
        let setup = conn.setup();
        let screen = setup.roots.get(screen_num).ok_or("显示服务没有该屏幕")?;
        let root = screen.root;
        let bpp32 = setup
            .pixmap_formats
            .iter()
            .any(|f| f.depth == 24 && f.bits_per_pixel == 32);
        if screen.root_depth != 24 || !bpp32 {
            return Err(format!(
                "显示服务的颜色深度为 {} 位，qybit-gui 目前只支持 24 位真彩色（32 位每像素）",
                screen.root_depth
            )
            .into());
        }
        let lsb = setup.image_byte_order == ImageOrder::LSB_FIRST;
        let scale = dpi_scale(&conn, root);
        let atoms = Atoms {
            protocols: atom(&conn, "WM_PROTOCOLS")?,
            delete: atom(&conn, "WM_DELETE_WINDOW")?,
            net_wm_name: atom(&conn, "_NET_WM_NAME")?,
            net_wm_pid: atom(&conn, "_NET_WM_PID")?,
            net_wm_state: atom(&conn, "_NET_WM_STATE")?,
            max_v: atom(&conn, "_NET_WM_STATE_MAXIMIZED_VERT")?,
            max_h: atom(&conn, "_NET_WM_STATE_MAXIMIZED_HORZ")?,
            utf8: atom(&conn, "UTF8_STRING")?,
            clipboard: atom(&conn, "CLIPBOARD")?,
            targets: atom(&conn, "TARGETS")?,
            aware: atom(&conn, "XdndAware")?,
            enter: atom(&conn, "XdndEnter")?,
            position: atom(&conn, "XdndPosition")?,
            status: atom(&conn, "XdndStatus")?,
            drop: atom(&conn, "XdndDrop")?,
            finished: atom(&conn, "XdndFinished")?,
            leave: atom(&conn, "XdndLeave")?,
            selection: atom(&conn, "XdndSelection")?,
            copy: atom(&conn, "XdndActionCopy")?,
            uri_list: atom(&conn, "text/uri-list")?,
            drop_target: atom(&conn, "QYBIT_DROP")?,
        };
        let (x, y, w, h) = match place {
            Some(p) if p.right > p.left && p.bottom > p.top => (
                p.left.clamp(-10_000, 10_000) as i16,
                p.top.clamp(-10_000, 10_000) as i16,
                (p.right - p.left).clamp(200, 16_000) as u16,
                (p.bottom - p.top).clamp(150, 16_000) as u16,
            ),
            _ => (
                0,
                0,
                (1000.0 * scale).min(f32::from(screen.width_in_pixels)) as u16,
                (700.0 * scale).min(f32::from(screen.height_in_pixels)) as u16,
            ),
        };
        let win = conn.generate_id()?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            win,
            root,
            x,
            y,
            w,
            h,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            // 不设背景：服务器不在 Expose 前清屏，避免闪烁；内容由本程序重送。
            &CreateWindowAux::new().background_pixmap(NONE).event_mask(
                EventMask::EXPOSURE
                    | EventMask::STRUCTURE_NOTIFY
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::KEY_PRESS
                    | EventMask::FOCUS_CHANGE
                    | EventMask::LEAVE_WINDOW,
            ),
        )?;
        conn.change_property8(
            PropMode::REPLACE,
            win,
            atoms.net_wm_name,
            atoms.utf8,
            title.as_bytes(),
        )?;
        conn.change_property8(
            PropMode::REPLACE,
            win,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            b"QyBit",
        )?;
        conn.change_property8(
            PropMode::REPLACE,
            win,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"qybit-gui\0QyBit\0",
        )?;
        conn.change_property32(
            PropMode::REPLACE,
            win,
            atoms.protocols,
            AtomEnum::ATOM,
            &[atoms.delete],
        )?;
        conn.change_property32(
            PropMode::REPLACE,
            win,
            atoms.net_wm_pid,
            AtomEnum::CARDINAL,
            &[std::process::id()],
        )?;
        conn.change_property32(PropMode::REPLACE, win, atoms.aware, AtomEnum::ATOM, &[5])?;
        // WM_NORMAL_HINTS（ICCCM 4.1.2.3）：最小尺寸；恢复保存的位置时另设 USPosition。
        let flags: u32 = 0x10 | if place.is_some() { 0x1 | 0x2 } else { 0 };
        let (min_w, min_h) = ((560.0 * scale) as u32, (360.0 * scale) as u32);
        let mut hints = [0u32; 18];
        hints[0] = flags;
        hints[1] = x as u32;
        hints[2] = y as u32;
        hints[3] = u32::from(w);
        hints[4] = u32::from(h);
        hints[5] = min_w;
        hints[6] = min_h;
        conn.change_property32(
            PropMode::REPLACE,
            win,
            AtomEnum::WM_NORMAL_HINTS,
            AtomEnum::WM_SIZE_HINTS,
            &hints,
        )?;
        if place.is_some_and(|p| p.maximized) {
            conn.change_property32(
                PropMode::REPLACE,
                win,
                atoms.net_wm_state,
                AtomEnum::ATOM,
                &[atoms.max_v, atoms.max_h],
            )?;
        }
        let gc = conn.generate_id()?;
        conn.create_gc(gc, win, &CreateGCAux::new().graphics_exposures(0))?;
        // 光标：X 核心光标字体（cursorfont.h：left_ptr 68、sb_h_double_arrow 108、sb_v_double_arrow 116）。
        let font = conn.generate_id()?;
        conn.open_font(font, b"cursor")?;
        let mut cursors = [0u32; 3];
        for (c, glyph) in cursors.iter_mut().zip([68u16, 116, 108]) {
            *c = conn.generate_id()?;
            conn.create_glyph_cursor(
                *c,
                font,
                font,
                glyph,
                glyph + 1,
                0,
                0,
                0,
                0xffff,
                0xffff,
                0xffff,
            )?;
        }
        conn.close_font(font)?;
        conn.change_window_attributes(win, &ChangeWindowAttributesAux::new().cursor(cursors[0]))?;
        // 键盘：核心协议的键码 → 键符号表。
        let min_keycode = setup.min_keycode;
        let count = setup
            .max_keycode
            .saturating_sub(min_keycode)
            .saturating_add(1);
        let map = conn.get_keyboard_mapping(min_keycode, count)?.reply()?;
        let max_req = conn.maximum_request_bytes();
        Ok(X {
            conn,
            win,
            root,
            gc,
            atoms,
            max_req,
            lsb,
            min_keycode,
            per_keycode: usize::from(map.keysyms_per_keycode.max(1)),
            keysyms: map.keysyms,
            cursors,
            cursor: Cursor::Arrow,
            clipboard: String::new(),
            drag_source: 0,
            grabbed: Vec::new(),
            sent_bytes: 0,
            scale,
        })
    }

    pub fn map(&self) -> Res<()> {
        self.conn.map_window(self.win)?;
        self.conn.flush()?;
        Ok(())
    }

    /// 启动读取事件的线程：每个事件经 `wrap` 包装后送入通道。
    pub fn spawn_reader<M: Send + 'static>(&self, tx: Sender<M>, wrap: fn(Event) -> M, closed: M) {
        let conn = self.conn.clone();
        let _ = std::thread::Builder::new()
            .name("qybit-x11".into())
            .spawn(move || {
                while let Ok(ev) = conn.wait_for_event() {
                    if tx.send(wrap(ev)).is_err() {
                        return;
                    }
                }
                let _ = tx.send(closed);
            });
    }

    /// 把画布的一块送到窗口（ZPixmap，24 位深度、32 位每像素）。
    pub fn put(&mut self, canvas: &Canvas, r: Rect) -> Res<()> {
        let r = r.intersect(&canvas.bounds());
        if r.is_empty() {
            return Ok(());
        }
        let w = canvas.width() as usize;
        let cw = r.w as usize;
        let rows_per = (self.max_req.saturating_sub(64) / (cw * 4)).max(1);
        let src = canvas.pix.data();
        let mut data = Vec::with_capacity(cw * 4 * rows_per.min(r.h as usize));
        let mut y = r.y;
        while y < r.bottom() {
            let n = ((r.bottom() - y) as usize).min(rows_per) as i32;
            data.clear();
            for yy in y..y + n {
                let o = (yy as usize * w + r.x as usize) * 4;
                let Some(row) = src.get(o..o + cw * 4) else {
                    break;
                };
                for p in row.chunks_exact(4) {
                    if self.lsb {
                        data.extend_from_slice(&[p[2], p[1], p[0], 0]);
                    } else {
                        data.extend_from_slice(&[0, p[0], p[1], p[2]]);
                    }
                }
            }
            self.conn.put_image(
                ImageFormat::Z_PIXMAP,
                self.win,
                self.gc,
                cw as u16,
                n as u16,
                r.x as i16,
                y as i16,
                0,
                24,
                &data,
            )?;
            self.sent_bytes += data.len() as u64;
            y += n;
        }
        Ok(())
    }

    pub fn flush(&self) -> Res<()> {
        self.conn.flush()?;
        Ok(())
    }

    pub fn set_cursor(&mut self, c: Cursor) {
        if c == self.cursor {
            return;
        }
        self.cursor = c;
        let id = self.cursors[match c {
            Cursor::Arrow => 0,
            Cursor::ResizeV => 1,
            Cursor::ResizeH => 2,
        }];
        let _ = self
            .conn
            .change_window_attributes(self.win, &ChangeWindowAttributesAux::new().cursor(id));
        let _ = self.conn.flush();
    }

    /// 键码 → 键符号（按 Shift 取第二级）。
    pub fn keysym(&self, keycode: u8, shift: bool) -> u32 {
        let base = usize::from(keycode.saturating_sub(self.min_keycode)) * self.per_keycode;
        let first = self.keysyms.get(base).copied().unwrap_or(0);
        if shift {
            match self.keysyms.get(base + 1).copied() {
                Some(s) if s != 0 => return s,
                _ => {}
            }
        }
        first
    }

    /// 键符号对应的可打印字符（Latin-1 与 Unicode 键符号；文件对话框的路径输入用）。
    pub fn keysym_char(sym: u32) -> Option<char> {
        match sym {
            0x20..=0x7e | 0xa0..=0xff => char::from_u32(sym),
            0x0100_0100..=0x0110_ffff => char::from_u32(sym - 0x0100_0000),
            _ => None,
        }
    }

    /// 抓取或释放全局媒体键（在根窗口上，任意修饰键）。返回被其他程序占用的键数。
    pub fn grab_media_keys(&mut self, on: bool) -> usize {
        for kc in std::mem::take(&mut self.grabbed) {
            let _ = self.conn.ungrab_key(kc, self.root, ModMask::ANY);
        }
        let mut busy = 0;
        if on {
            let codes: Vec<u8> = keysym::MEDIA
                .iter()
                .filter_map(|&s| self.keycode(s))
                .collect();
            for kc in codes {
                let ok = self
                    .conn
                    .grab_key(
                        true,
                        self.root,
                        ModMask::ANY,
                        kc,
                        GrabMode::ASYNC,
                        GrabMode::ASYNC,
                    )
                    .ok()
                    .and_then(|c| c.check().ok())
                    .is_some();
                if ok {
                    self.grabbed.push(kc);
                } else {
                    busy += 1;
                }
            }
        }
        let _ = self.conn.flush();
        busy
    }

    fn keycode(&self, sym: u32) -> Option<u8> {
        let per = self.per_keycode;
        let i = self.keysyms.iter().position(|&s| s == sym)?;
        u8::try_from(i / per).ok()?.checked_add(self.min_keycode)
    }

    /// 窗口当前的位置（客户区在屏幕上的矩形）与是否最大化。
    pub fn placement(&self) -> Option<Placement> {
        let g = self.conn.get_geometry(self.win).ok()?.reply().ok()?;
        let t = self
            .conn
            .translate_coordinates(self.win, self.root, 0, 0)
            .ok()?
            .reply()
            .ok()?;
        let state = self
            .conn
            .get_property(
                false,
                self.win,
                self.atoms.net_wm_state,
                AtomEnum::ATOM,
                0,
                64,
            )
            .ok()?
            .reply()
            .ok()?;
        let maximized = state
            .value32()
            .is_some_and(|mut v| v.any(|a| a == self.atoms.max_v || a == self.atoms.max_h));
        Some(Placement {
            left: i32::from(t.dst_x),
            top: i32::from(t.dst_y),
            right: i32::from(t.dst_x) + i32::from(g.width),
            bottom: i32::from(t.dst_y) + i32::from(g.height),
            maximized,
        })
    }

    /// 移到保存的位置（窗口管理器可能忽略创建时给出的位置，映射后再设一次）。
    pub fn move_to(&self, p: &Placement) {
        if p.maximized {
            return;
        }
        let _ = self
            .conn
            .configure_window(self.win, &ConfigureWindowAux::new().x(p.left).y(p.top));
        let _ = self.conn.flush();
    }

    /// 把文字放到剪贴板（成为 `CLIPBOARD` 的所有者，按请求提供 UTF-8 文字）。
    pub fn set_clipboard(&mut self, text: String) -> Res<()> {
        self.clipboard = text;
        self.conn
            .set_selection_owner(self.win, self.atoms.clipboard, CURRENT_TIME)?;
        self.conn.flush()?;
        Ok(())
    }

    /// 处理与界面无关的协议事件（剪贴板请求、XDND）。返回拖放进来的路径与关闭请求；其他事件
    /// 原样返回给调用方。
    pub fn protocol(&mut self, ev: Event) -> Res<Protocol> {
        match ev {
            Event::SelectionRequest(r) => {
                self.answer_selection(&r)?;
                Ok(Protocol::Handled)
            }
            Event::SelectionClear(_) => {
                self.clipboard.clear();
                Ok(Protocol::Handled)
            }
            Event::ClientMessage(e) => {
                let d = e.data.as_data32();
                if e.type_ == self.atoms.protocols && d[0] == self.atoms.delete {
                    return Ok(Protocol::Close);
                }
                if e.type_ == self.atoms.enter {
                    self.drag_source = d[0];
                } else if e.type_ == self.atoms.position {
                    // 接受拖放（动作：复制）；空矩形表示每次移动都发送位置。
                    let ev = ClientMessageEvent::new(
                        32,
                        d[0],
                        self.atoms.status,
                        [self.win, 1, 0, 0, self.atoms.copy],
                    );
                    self.conn.send_event(false, d[0], EventMask::NO_EVENT, ev)?;
                    self.conn.flush()?;
                } else if e.type_ == self.atoms.drop {
                    self.drag_source = d[0];
                    self.conn.convert_selection(
                        self.win,
                        self.atoms.selection,
                        self.atoms.uri_list,
                        self.atoms.drop_target,
                        d[2],
                    )?;
                    self.conn.flush()?;
                } else if e.type_ == self.atoms.leave {
                    self.drag_source = 0;
                }
                Ok(Protocol::Handled)
            }
            Event::SelectionNotify(e) if e.property == self.atoms.drop_target => {
                let reply = self
                    .conn
                    .get_property(
                        true,
                        self.win,
                        self.atoms.drop_target,
                        AtomEnum::ANY,
                        0,
                        1 << 22,
                    )?
                    .reply()?;
                if self.drag_source != 0 {
                    let ev = ClientMessageEvent::new(
                        32,
                        self.drag_source,
                        self.atoms.finished,
                        [self.win, 1, self.atoms.copy, 0, 0],
                    );
                    self.conn
                        .send_event(false, self.drag_source, EventMask::NO_EVENT, ev)?;
                    self.conn.flush()?;
                    self.drag_source = 0;
                }
                Ok(Protocol::Dropped(uri_paths(&reply.value)))
            }
            other => Ok(Protocol::Pass(other)),
        }
    }

    fn answer_selection(&self, r: &SelectionRequestEvent) -> Res<()> {
        let mut property = r.property;
        if r.selection != self.atoms.clipboard || self.clipboard.is_empty() {
            property = NONE;
        } else if r.target == self.atoms.targets {
            self.conn.change_property32(
                PropMode::REPLACE,
                r.requestor,
                r.property,
                AtomEnum::ATOM,
                &[
                    self.atoms.targets,
                    self.atoms.utf8,
                    u32::from(AtomEnum::STRING),
                ],
            )?;
        } else if r.target == self.atoms.utf8 || r.target == u32::from(AtomEnum::STRING) {
            self.conn.change_property8(
                PropMode::REPLACE,
                r.requestor,
                r.property,
                r.target,
                self.clipboard.as_bytes(),
            )?;
        } else {
            property = NONE;
        }
        let n = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: r.time,
            requestor: r.requestor,
            selection: r.selection,
            target: r.target,
            property,
        };
        self.conn
            .send_event(false, r.requestor, EventMask::NO_EVENT, n)?;
        self.conn.flush()?;
        Ok(())
    }
}

/// [`X::protocol`] 的结果。
pub enum Protocol {
    Handled,
    Close,
    Dropped(Vec<PathBuf>),
    Pass(Event),
}

/// `text/uri-list`（RFC 2483）→ 本地路径：只接受 `file://`，按 RFC 3986 解百分号编码。
pub fn uri_paths(data: &[u8]) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut out = Vec::new();
    for line in data.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.starts_with(b"#") {
            continue;
        }
        let Some(rest) = line.strip_prefix(b"file://") else {
            continue;
        };
        // file://host/path：跳过主机名（本机通常为空或 localhost）。
        let Some(slash) = rest.iter().position(|&b| b == b'/') else {
            continue;
        };
        let path = &rest[slash..];
        let mut bytes = Vec::with_capacity(path.len());
        let mut i = 0;
        while i < path.len() {
            let hex = |k: usize| path.get(k).and_then(|&b| char::from(b).to_digit(16));
            if path[i] == b'%'
                && let (Some(h), Some(l)) = (hex(i + 1), hex(i + 2))
            {
                bytes.push((h * 16 + l) as u8);
                i += 3;
            } else {
                bytes.push(path[i]);
                i += 1;
            }
        }
        out.push(PathBuf::from(std::ffi::OsString::from_vec(bytes)));
    }
    out
}

/// 根窗口 `RESOURCE_MANAGER` 中的 `Xft.dpi`（桌面设置的缩放），缺省 96。环境变量
/// `QYBIT_SCALE`（如 `1.5`）优先。
fn dpi_scale(c: &RustConnection, root: u32) -> f32 {
    if let Some(s) = std::env::var("QYBIT_SCALE")
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|s| s.is_finite() && *s > 0.0)
    {
        return s.clamp(0.5, 4.0);
    }
    let Ok(cookie) = c.get_property(
        false,
        root,
        AtomEnum::RESOURCE_MANAGER,
        AtomEnum::STRING,
        0,
        1 << 20,
    ) else {
        return 1.0;
    };
    let Ok(reply) = cookie.reply() else {
        return 1.0;
    };
    let text = String::from_utf8_lossy(&reply.value);
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("Xft.dpi:")
            && let Ok(dpi) = v.trim().parse::<f32>()
            && dpi > 0.0
        {
            return (dpi / 96.0).clamp(0.5, 4.0);
        }
    }
    1.0
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{X, uri_paths};

    #[test]
    fn uri_list_decoding() {
        let data = b"# comment\r\nfile:///music/a%20b.flac\r\nfile://localhost/x/%E4%B8%AD.wav\r\nhttp://e.com/a\r\nfile:///bad%zz%2\n";
        assert_eq!(
            uri_paths(data),
            vec![
                PathBuf::from("/music/a b.flac"),
                PathBuf::from("/x/中.wav"),
                PathBuf::from("/bad%zz%2"),
            ]
        );
    }

    #[test]
    fn keysym_chars() {
        assert_eq!(X::keysym_char(0x41), Some('A'));
        assert_eq!(X::keysym_char(0xe9), Some('é'));
        assert_eq!(X::keysym_char(0x0100_4e2d), Some('中'));
        assert_eq!(X::keysym_char(0xff0d), None);
    }
}
