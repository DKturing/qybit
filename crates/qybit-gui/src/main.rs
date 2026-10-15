//! QyBit 图形界面（GUI T2）。Windows 使用系统原生控件（`win` 模块）；Linux 前端（GTK4）在
//! Linux 阶段提供。界面模型、播放线程与显示文字都在 `qy-ui`，前端只负责映射到控件。
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    win::run()
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!(
        "qybit-gui 目前只有 Windows 前端；Linux 前端在 Linux 阶段提供，请先使用命令行 qybit。"
    );
    std::process::ExitCode::FAILURE
}
