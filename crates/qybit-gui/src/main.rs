//! QyBit 图形界面的官方二进制：用官方解码器（WAV、FLAC、AIFF）组装 [`qybit_gui::run`]（E2）。
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> std::process::ExitCode {
    qybit_gui::run(qy_player::Builtin)
}
