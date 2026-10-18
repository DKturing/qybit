//! 窗口后端 B：x11rb 0.13 的纯 Rust X11 协议实现（`RustConnection`，不链接 libX11/libxcb，
//! 不加载任何系统库），可用 musl 完全静态链接。Wayland 桌面经 XWayland 运行。
//! 已实现：窗口、标题、关闭、Expose 时只重送像素、尺寸变化、鼠标与滚轮、核心键盘映射、
//! `Xft.dpi` 缩放、XDND 文件拖放（接收方，版本 5）。未实现：输入法（XIM）、剪贴板。
//! 环境变量：`QY_PROTO_STATIC=1` 不刷新状态行；`QY_PROTO_BENCH=1` 运行测量后退出。

#[path = "../ui.rs"]
mod ui;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ui::{App, Input, Key, Region, TICK};
use x11rb::COPY_DEPTH_FROM_PARENT;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask,
    ImageFormat, ImageOrder, PropMode, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

type Res<T> = Result<T, Box<dyn std::error::Error>>;

struct Atoms {
    protocols: u32,
    delete: u32,
    net_wm_name: u32,
    utf8: u32,
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
    target: u32,
}

fn atom(c: &RustConnection, name: &str) -> Res<u32> {
    Ok(c.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

struct X {
    conn: Arc<RustConnection>,
    win: u32,
    gc: u32,
    atoms: Atoms,
    max_req: usize,
    sync_each: bool,
}

impl X {
    /// 把画布的一块送到窗口（ZPixmap，32 位每像素 BGRX）；按最大请求长度分条。
    fn put(&self, app: &App, r: ui::Rect) -> Res<()> {
        let (w, h) = (app.width(), app.height());
        let (x1, y1) = ((r.x + r.w).min(w), (r.y + r.h).min(h));
        if r.x >= x1 || r.y >= y1 {
            return Ok(());
        }
        let cw = (x1 - r.x) as usize;
        let rows_per = ((self.max_req - 64) / (cw * 4)).max(1);
        let src = app.pixmap.data();
        let mut y = r.y;
        let mut data = Vec::with_capacity(cw * 4 * rows_per.min((y1 - r.y) as usize));
        while y < y1 {
            let n = ((y1 - y) as usize).min(rows_per) as u32;
            data.clear();
            for yy in y..y + n {
                let o = (yy * w + r.x) as usize * 4;
                for p in src[o..o + cw * 4].chunks_exact(4) {
                    data.extend_from_slice(&[p[2], p[1], p[0], 0]);
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
            y += n;
        }
        if self.sync_each {
            self.conn.sync()?;
        } else {
            self.conn.flush()?;
        }
        Ok(())
    }

    fn reply_status(&self, source: u32) -> Res<()> {
        let ev = ClientMessageEvent::new(
            32,
            source,
            self.atoms.status,
            [self.win, 1, 0, 0, self.atoms.copy],
        );
        self.conn
            .send_event(false, source, EventMask::NO_EVENT, ev)?;
        self.conn.flush()?;
        Ok(())
    }

    fn finished(&self, source: u32) -> Res<()> {
        let ev = ClientMessageEvent::new(
            32,
            source,
            self.atoms.finished,
            [self.win, 1, self.atoms.copy, 0, 0],
        );
        self.conn
            .send_event(false, source, EventMask::NO_EVENT, ev)?;
        self.conn.flush()?;
        Ok(())
    }
}

/// `text/uri-list` → 本地路径（只接受 `file://`，按 RFC 3986 解百分号编码）。
fn uri_paths(data: &[u8]) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut out = Vec::new();
    for line in data.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
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
            if path[i] == b'%'
                && i + 2 < path.len()
                && let (Some(h), Some(l)) = (hex(path[i + 1]), hex(path[i + 2]))
            {
                bytes.push(h * 16 + l);
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

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 根窗口 `RESOURCE_MANAGER` 中的 `Xft.dpi`（桌面设置的缩放），缺省 96。
fn dpi_scale(c: &RustConnection, root: u32) -> f32 {
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

fn run() -> Res<()> {
    let start = Instant::now();
    let (conn, screen_num) = RustConnection::connect(None).map_err(|e| {
        format!(
            "连接不到 X 显示服务（{e}）。本程序需要图形桌面（X11，或 Wayland 桌面自带的 XWayland）：\n\
             - 在桌面的终端窗口里运行；\n\
             - 经 SSH 从另一台有桌面的电脑运行时，用 ssh -X 登录；\n\
             - 没有桌面的系统（文本控制台）上本原型不能运行。"
        )
    })?;
    let conn = Arc::new(conn);
    let setup = conn.setup();
    let screen = setup.roots.get(screen_num).ok_or("没有该屏幕")?;
    let root = screen.root;
    let bpp32 = setup
        .pixmap_formats
        .iter()
        .any(|f| f.depth == 24 && f.bits_per_pixel == 32);
    if screen.root_depth != 24 || !bpp32 || setup.image_byte_order != ImageOrder::LSB_FIRST {
        return Err(format!(
            "原型只支持 24 位深度、32 位每像素、低字节在前的显示（根深度 {}）",
            screen.root_depth
        )
        .into());
    }
    let scale = dpi_scale(&conn, root);
    let (w, h) = ((900.0 * scale) as u16, (600.0 * scale) as u16);

    let atoms = Atoms {
        protocols: atom(&conn, "WM_PROTOCOLS")?,
        delete: atom(&conn, "WM_DELETE_WINDOW")?,
        net_wm_name: atom(&conn, "_NET_WM_NAME")?,
        utf8: atom(&conn, "UTF8_STRING")?,
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
        target: atom(&conn, "QYBIT_DROP")?,
    };

    let win = conn.generate_id()?;
    conn.create_window(
        COPY_DEPTH_FROM_PARENT,
        win,
        root,
        0,
        0,
        w,
        h,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        // 不设背景：服务器不在 Expose 前清屏，避免闪烁；内容由本程序重送。
        &CreateWindowAux::new()
            .background_pixmap(x11rb::NONE)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::STRUCTURE_NOTIFY
                    | EventMask::BUTTON_PRESS
                    | EventMask::KEY_PRESS,
            ),
    )?;
    let title = "QyBit 原型（x11rb）";
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
        b"QyBit prototype (x11rb)",
    )?;
    conn.change_property8(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        b"qybit\0QyBit\0",
    )?;
    conn.change_property32(
        PropMode::REPLACE,
        win,
        atoms.protocols,
        AtomEnum::ATOM,
        &[atoms.delete],
    )?;
    conn.change_property32(PropMode::REPLACE, win, atoms.aware, AtomEnum::ATOM, &[5])?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, win, &CreateGCAux::new().graphics_exposures(0))?;
    conn.map_window(win)?;
    conn.flush()?;

    // 键盘：核心协议的键码 → 键符号表（每个键码取第一个键符号）。
    let min_kc = setup.min_keycode;
    let count = setup.max_keycode - min_kc + 1;
    let map = conn.get_keyboard_mapping(min_kc, count)?.reply()?;
    let per = usize::from(map.keysyms_per_keycode.max(1));
    let keysym = |kc: u8| {
        map.keysyms
            .get(usize::from(kc.saturating_sub(min_kc)) * per)
            .copied()
    };

    let max_req = conn.maximum_request_bytes();
    let bench = ui::env_on("QY_PROTO_BENCH");
    let still = ui::env_on("QY_PROTO_STATIC");
    let x = X {
        conn: conn.clone(),
        win,
        gc,
        atoms,
        max_req,
        sync_each: bench,
    };
    let mut app = App::new(u32::from(w), u32::from(h), scale);
    app.paint(Region::All);

    // 事件线程：阻塞等待服务器事件，经通道交给主线程；主线程按状态行周期等待。
    let (tx, rx) = mpsc::channel();
    let ev_conn = conn.clone();
    std::thread::spawn(move || {
        while let Ok(ev) = ev_conn.wait_for_event() {
            if tx.send(ev).is_err() {
                break;
            }
        }
    });

    let mut first = true;
    let mut next_tick = Instant::now() + TICK;
    let mut drag_source = 0u32;
    let mut present_total = Duration::ZERO;
    let mut presents = 0u32;
    let mut present = |app: &mut App, region: Option<Region>, rect: Option<ui::Rect>| -> Res<()> {
        let r = match (region, rect) {
            (Some(reg), _) => app.paint(reg),
            (None, Some(r)) => r,
            (None, None) => return Ok(()),
        };
        let t = Instant::now();
        x.put(app, r)?;
        present_total += t.elapsed();
        presents += 1;
        Ok(())
    };
    loop {
        let ev = if still {
            match rx.recv() {
                Ok(e) => Some(e),
                Err(_) => break,
            }
        } else {
            let now = Instant::now();
            match rx.recv_timeout(next_tick.saturating_duration_since(now)) {
                Ok(e) => Some(e),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        };
        let Some(ev) = ev else {
            while next_tick <= Instant::now() {
                next_tick += TICK;
            }
            let r = app.update(Input::Tick);
            present(&mut app, r, None)?;
            continue;
        };
        let input = match ev {
            Event::Expose(e) => {
                present(
                    &mut app,
                    None,
                    Some(ui::Rect {
                        x: u32::from(e.x),
                        y: u32::from(e.y),
                        w: u32::from(e.width),
                        h: u32::from(e.height),
                    }),
                )?;
                if first && e.count == 0 {
                    first = false;
                    eprintln!(
                        "首帧：{:.1} ms（其中加载字体库 {:.1} ms）",
                        start.elapsed().as_secs_f64() * 1000.0,
                        app.font_load.as_secs_f64() * 1000.0
                    );
                    if bench {
                        let t = Instant::now();
                        for i in ui::bench_inputs() {
                            let r = app.update(i);
                            present(&mut app, r, None)?;
                        }
                        eprintln!(
                            "测量：400 次输入共 {:.1} ms",
                            t.elapsed().as_secs_f64() * 1000.0
                        );
                        break;
                    }
                }
                None
            }
            Event::ConfigureNotify(e) => {
                Some(Input::Resize(u32::from(e.width), u32::from(e.height)))
            }
            Event::ButtonPress(e) => match e.detail {
                1 => Some(Input::Click(f32::from(e.event_x), f32::from(e.event_y))),
                4 => Some(Input::Wheel(-1)),
                5 => Some(Input::Wheel(1)),
                _ => None,
            },
            Event::KeyPress(e) => match keysym(e.detail) {
                Some(0xff52) => Some(Input::Key(Key::Up)),
                Some(0xff54) => Some(Input::Key(Key::Down)),
                Some(0xff55) => Some(Input::Key(Key::PageUp)),
                Some(0xff56) => Some(Input::Key(Key::PageDown)),
                Some(0xff50) => Some(Input::Key(Key::Home)),
                Some(0xff57) => Some(Input::Key(Key::End)),
                Some(0xff0d | 0xff8d) => Some(Input::Key(Key::Enter)),
                Some(0x20) => Some(Input::Key(Key::Space)),
                _ => None,
            },
            Event::ClientMessage(e) => {
                let d = e.data.as_data32();
                if e.type_ == x.atoms.protocols && d[0] == x.atoms.delete {
                    break;
                } else if e.type_ == x.atoms.enter {
                    drag_source = d[0];
                } else if e.type_ == x.atoms.position {
                    x.reply_status(d[0])?;
                } else if e.type_ == x.atoms.drop {
                    drag_source = d[0];
                    conn.convert_selection(
                        win,
                        x.atoms.selection,
                        x.atoms.uri_list,
                        x.atoms.target,
                        d[2],
                    )?;
                    conn.flush()?;
                } else if e.type_ == x.atoms.leave {
                    drag_source = 0;
                }
                None
            }
            Event::SelectionNotify(e) if e.property == x.atoms.target => {
                let reply = conn
                    .get_property(true, win, x.atoms.target, AtomEnum::ANY, 0, 1 << 22)?
                    .reply()?;
                if drag_source != 0 {
                    x.finished(drag_source)?;
                    drag_source = 0;
                }
                Some(Input::Drop(uri_paths(&reply.value)))
            }
            _ => None,
        };
        if let Some(i) = input {
            let r = app.update(i);
            present(&mut app, r, None)?;
        }
    }
    eprintln!("{}", app.full.line("整窗重绘"));
    eprintln!("{}", app.list.line("列表重绘"));
    eprintln!("{}", app.status.line("状态行重绘"));
    if presents > 0 {
        eprintln!(
            "提交（转换为 BGRX 并 PutImage{}）：{} 次，平均 {:.2} ms",
            if bench {
                "，每次等服务器处理完"
            } else {
                ""
            },
            presents,
            present_total.as_secs_f64() * 1000.0 / f64::from(presents)
        );
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("错误：{e}");
            std::process::ExitCode::FAILURE
        }
    }
}
