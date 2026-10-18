//! 测试用 XDND 拖放源：把文件以 `text/uri-list` 拖放到指定窗口，模拟文件管理器的拖放
//! （XDND 协议版本 5：Enter → Position → 等 Status → Drop → 应答 SelectionRequest → 等 Finished）。
//! 用法：xdnd-send <目标窗口 id> <文件>...

use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

type Res<T> = Result<T, Box<dyn std::error::Error>>;

fn atom(c: &RustConnection, name: &str) -> Res<u32> {
    Ok(c.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

/// 本地路径 → `file://` URI（RFC 3986：非保留字符以外按字节百分号编码）。
fn uri(path: &str) -> String {
    let mut s = String::from("file://");
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            s.push(char::from(b));
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

fn run() -> Res<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(target), files) = (args.first(), &args[1..]) else {
        return Err("用法：xdnd-send <目标窗口 id> <文件>...".into());
    };
    let target: u32 = target.parse()?;
    let (conn, screen_num) = RustConnection::connect(None)?;
    let root = conn.setup().roots[screen_num].root;
    let a = |n: &str| atom(&conn, n);
    let (aware, enter, position, status, drop, finished, selection, copy, uri_list) = (
        a("XdndAware")?,
        a("XdndEnter")?,
        a("XdndPosition")?,
        a("XdndStatus")?,
        a("XdndDrop")?,
        a("XdndFinished")?,
        a("XdndSelection")?,
        a("XdndActionCopy")?,
        a("text/uri-list")?,
    );
    let version = conn
        .get_property(false, target, aware, AtomEnum::ATOM, 0, 1)?
        .reply()?;
    let version = version
        .value32()
        .and_then(|mut v| v.next())
        .ok_or("目标窗口没有 XdndAware")?;
    println!("目标声明 XDND 版本 {version}");

    let src = conn.generate_id()?;
    conn.create_window(
        0,
        src,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new(),
    )?;
    conn.set_selection_owner(src, selection, x11rb::CURRENT_TIME)?;
    let list: String = files.iter().map(|f| uri(f) + "\r\n").collect();

    let send = |kind: u32, data: [u32; 5]| -> Res<()> {
        let ev = ClientMessageEvent::new(32, target, kind, data);
        conn.send_event(false, target, EventMask::NO_EVENT, ev)?;
        conn.flush()?;
        Ok(())
    };
    send(enter, [src, 5 << 24, uri_list, 0, 0])?;
    send(position, [src, 0, (100 << 16) | 200, 0, copy])?;

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut dropped = false;
    while Instant::now() < deadline {
        let Some(ev) = conn.poll_for_event()? else {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        };
        match ev {
            Event::ClientMessage(e) if e.type_ == status => {
                let d = e.data.as_data32();
                println!(
                    "收到 XdndStatus：接受 = {}，动作 = {}",
                    d[1] & 1,
                    d[4] == copy
                );
                if !dropped {
                    dropped = true;
                    send(drop, [src, 0, 0, 0, 0])?;
                }
            }
            Event::SelectionRequest(r) => {
                conn.change_property8(
                    PropMode::REPLACE,
                    r.requestor,
                    r.property,
                    uri_list,
                    list.as_bytes(),
                )?;
                let n = SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: r.time,
                    requestor: r.requestor,
                    selection: r.selection,
                    target: r.target,
                    property: r.property,
                };
                conn.send_event(false, r.requestor, EventMask::NO_EVENT, n)?;
                conn.flush()?;
                println!("已交出 text/uri-list（{} 字节）", list.len());
            }
            Event::ClientMessage(e) if e.type_ == finished => {
                let d = e.data.as_data32();
                println!("收到 XdndFinished：成功 = {}", d[1] & 1);
                return Ok(());
            }
            _ => {}
        }
    }
    Err("5 秒内没有完成拖放".into())
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
