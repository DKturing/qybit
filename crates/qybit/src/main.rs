//! QyBit 命令行的官方二进制：用官方解码器（WAV、FLAC、AIFF）组装 [`qybit::run`]（E2）。

fn main() -> std::process::ExitCode {
    qybit::run(qy_player::Builtin)
}
