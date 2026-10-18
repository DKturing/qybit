//! 带示例格式 QYT1 的 QyBit 命令行（E2）：与官方 `qybit` 同一个主体，只是格式集合换成
//! [`qy_ext_demo::ExtFormats`]。

fn main() -> std::process::ExitCode {
    qybit::run(qy_ext_demo::ExtFormats)
}
