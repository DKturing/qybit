//! 带示例格式 QYT1 的 QyBit 图形界面（E2，`gui` 特性）：与官方 `qybit-gui` 同一个主体，只是格式
//! 集合换成 [`qy_ext_demo::ExtFormats`]。
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> std::process::ExitCode {
    qybit_gui::run(qy_ext_demo::ExtFormats)
}
