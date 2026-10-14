//! 按所选板的特性把该板的 memory.x 放到链接器搜索路径，并只对二进制加入 cortex-m-rt 的链接脚本。
use std::path::PathBuf;
use std::{env, fs};

/// (特性环境变量, memory.x 内容)：每块板一项。
const BOARDS: &[(&str, &[u8])] = &[(
    "CARGO_FEATURE_STM32F411",
    include_bytes!("src/stm32f411/memory.x"),
)];

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap_or_default());
    let selected: Vec<_> = BOARDS
        .iter()
        .filter(|(feature, _)| env::var_os(feature).is_some())
        .collect();
    let [(_, memory)] = selected.as_slice() else {
        // 未选或多选时由 main.rs 的 compile_error! 或链接冲突报告；这里不写 memory.x。
        return;
    };
    if let Err(e) = fs::write(out.join("memory.x"), memory) {
        println!("cargo::error=写入 memory.x 失败: {e}");
    }
    println!("cargo::rustc-link-search={}", out.display());
    println!("cargo::rerun-if-changed=src/stm32f411/memory.x");
    println!("cargo::rustc-link-arg-bins=-Tlink.x");
}
