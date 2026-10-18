//! QyBit 图形界面。Windows 使用系统原生控件（`win` 模块）；Linux 为自绘界面（`linux` 模块：X11
//! 协议的纯 Rust 实现，软件绘制，docs/gui-linux-prototype.md）。界面模型、播放线程与显示文字在
//! `qy-ui`，菜单命令与选项的行为在共用的 [`app`]，前端只负责映射到控件。
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(any(windows, target_os = "linux"))]
mod app;
#[cfg(any(windows, target_os = "linux"))]
mod sim;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    win::run()
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    linux::run()
}

#[cfg(not(any(windows, target_os = "linux")))]
fn main() -> std::process::ExitCode {
    eprintln!("qybit-gui 目前只有 Windows 与 Linux 前端，请先使用命令行 qybit。");
    std::process::ExitCode::FAILURE
}
