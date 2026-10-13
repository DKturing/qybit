//! QyBit 命令行入口。
//!
//! M0 T1：仅骨架。设备列举为最小实现；播放（`--device <id> file...`）在 T6 实现，
//! 当前明确报告未实现，不做任何回退。

use std::process::ExitCode;

const USAGE: &str = "用法:\n  qybit list-devices\n  qybit --device <endpoint-id> <file.wav>...";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("list-devices") => list_devices(),
        Some("--device") => {
            eprintln!("错误: 播放尚未实现（M0 T6）");
            ExitCode::FAILURE
        }
        Some("--version") => {
            println!("qybit {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(windows)]
fn list_devices() -> ExitCode {
    match qy_host_wasapi::endpoints::list_render_endpoint_ids() {
        Ok(ids) => {
            for id in ids {
                println!("{id}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("错误: 枚举设备失败: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn list_devices() -> ExitCode {
    eprintln!("错误: 当前仅支持 Windows WASAPI 后端");
    ExitCode::FAILURE
}
