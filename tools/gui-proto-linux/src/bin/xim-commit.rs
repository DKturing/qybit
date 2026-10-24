//! 测试用输入法（XIM 服务端，`xim` crate）：核对 qybit-gui 的输入法客户端（LG2）。
//! 以 `@server=<名称>` 注册；收到转发的 `z` 键时提交给定的文字（模拟输入法选字后上屏），其他按键
//! 原样送回（模拟输入法不处理的键）。连接、焦点、光标位置与提交写到标准输出，供核对脚本检查。
//! 用法：xim-commit <名称> <提交的文字>

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, KEY_PRESS_EVENT, KeyPressEvent};
use x11rb::rust_connection::RustConnection;
use xim::x11rb::X11rbServer;
use xim::{InputStyle, Server, ServerError, ServerHandler, UserInputContext, XimConnections};

type Res<T> = Result<T, Box<dyn std::error::Error>>;

struct Handler {
    text: String,
    /// `z` 键的键码。
    z: u8,
}

fn spot(ic: &UserInputContext<()>) -> String {
    let p = ic.ic.preedit_spot();
    format!("{},{}", p.x, p.y)
}

impl<S: Server> ServerHandler<S> for Handler
where
    S: Server<XEvent = KeyPressEvent>,
{
    type InputStyleArray = [InputStyle; 2];
    type InputContextData = ();

    fn new_ic_data(&mut self, _: &mut S, style: InputStyle) -> Result<(), ServerError> {
        println!("创建输入上下文：风格 {:#x}", style.bits());
        Ok(())
    }

    fn input_styles(&self) -> Self::InputStyleArray {
        [
            InputStyle::PREEDIT_POSITION | InputStyle::STATUS_NOTHING,
            InputStyle::PREEDIT_NOTHING | InputStyle::STATUS_NOTHING,
        ]
    }

    /// 要求转发按下与松开（X 协议 KeyPressMask | KeyReleaseMask）。
    fn filter_events(&self) -> u32 {
        0b11
    }

    fn handle_connect(&mut self, _: &mut S) -> Result<(), ServerError> {
        println!("连接");
        Ok(())
    }

    fn handle_create_ic(
        &mut self,
        _: &mut S,
        _: &mut UserInputContext<()>,
    ) -> Result<(), ServerError> {
        Ok(())
    }

    fn handle_destroy_ic(&mut self, _: &mut S, _: UserInputContext<()>) -> Result<(), ServerError> {
        Ok(())
    }

    fn handle_reset_ic(
        &mut self,
        _: &mut S,
        _: &mut UserInputContext<()>,
    ) -> Result<String, ServerError> {
        Ok(String::new())
    }

    fn handle_set_focus(
        &mut self,
        _: &mut S,
        ic: &mut UserInputContext<()>,
    ) -> Result<(), ServerError> {
        println!("获得焦点：光标 {}", spot(ic));
        Ok(())
    }

    fn handle_unset_focus(
        &mut self,
        _: &mut S,
        _: &mut UserInputContext<()>,
    ) -> Result<(), ServerError> {
        println!("失去焦点");
        Ok(())
    }

    fn handle_set_ic_values(
        &mut self,
        _: &mut S,
        ic: &mut UserInputContext<()>,
    ) -> Result<(), ServerError> {
        println!("光标位置：{}", spot(ic));
        Ok(())
    }

    fn handle_forward_event(
        &mut self,
        server: &mut S,
        ic: &mut UserInputContext<()>,
        xev: &KeyPressEvent,
    ) -> Result<bool, ServerError> {
        if xev.response_type & 0x7f == KEY_PRESS_EVENT && xev.detail == self.z {
            println!("提交：{}", self.text);
            server.commit(&ic.ic, &self.text)?;
            return Ok(true);
        }
        Ok(false)
    }
}

fn run() -> Res<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [name, text] = args.as_slice() else {
        return Err("用法：xim-commit <名称> <提交的文字>".into());
    };
    let (conn, screen) = RustConnection::connect(None)?;
    let setup = conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let map = conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
    let per = usize::from(map.keysyms_per_keycode.max(1));
    let z = map
        .keysyms
        .chunks(per)
        .position(|k| k.first() == Some(&0x7a))
        .and_then(|i| u8::try_from(usize::from(min) + i).ok())
        .ok_or("键盘映射中没有 z 键")?;
    let mut server = X11rbServer::init(&conn, screen, name, xim::ALL_LOCALES)?;
    let mut connections = XimConnections::new();
    let mut handler = Handler {
        text: text.clone(),
        z,
    };
    println!("已注册 @server={name}");
    loop {
        let ev = conn.wait_for_event()?;
        if let Err(e) = server.filter_event(&ev, &mut connections, &mut handler) {
            println!("错误：{e}");
        }
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("xim-commit：{e}");
        std::process::exit(1);
    }
}
