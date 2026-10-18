//! QyBit 图形界面。Windows 使用系统原生控件（`win` 模块）；Linux 为自绘界面（`linux` 模块：X11
//! 协议的纯 Rust 实现，软件绘制，docs/gui-linux-prototype.md）。界面模型、播放线程与显示文字在
//! `qy-ui`，菜单命令与选项的行为在共用的 [`app`]，前端只负责映射到控件。
//!
//! 主体是库（E2）：[`run`] 对格式集合泛型，官方二进制传入 [`qy_player::Builtin`]；插件仓库传入
//! 自己的 [`qy_player::Formats`] 即得到带新格式的同一个图形界面（播放列表的格式列、文件对话框的
//! 筛选与“关于”文字都来自格式集合）。

#[cfg(any(windows, target_os = "linux"))]
mod app;
#[cfg(any(windows, target_os = "linux"))]
mod sim;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod win;

/// 运行图形界面（E2：格式由调用方给出）。
#[cfg(windows)]
pub fn run<F: qy_player::Formats>(formats: F) -> std::process::ExitCode {
    win::run(formats)
}

/// 运行图形界面（E2：格式由调用方给出）。
#[cfg(target_os = "linux")]
pub fn run<F: qy_player::Formats>(formats: F) -> std::process::ExitCode {
    linux::run(formats)
}

/// 本平台没有前端。
#[cfg(not(any(windows, target_os = "linux")))]
pub fn run<F: qy_player::Formats>(_formats: F) -> std::process::ExitCode {
    eprintln!("qybit-gui 目前只有 Windows 与 Linux 前端，请先使用命令行 qybit。");
    std::process::ExitCode::FAILURE
}
