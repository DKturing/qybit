//! 把 memory.x 放到链接器搜索路径，并只对二进制加入 cortex-m-rt 的链接脚本。
use std::path::PathBuf;
use std::{env, fs};

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap_or_default());
    if let Err(e) = fs::write(out.join("memory.x"), include_bytes!("memory.x")) {
        println!("cargo::error=写入 memory.x 失败: {e}");
    }
    println!("cargo::rustc-link-search={}", out.display());
    println!("cargo::rerun-if-changed=memory.x");
    println!("cargo::rustc-link-arg-bins=-Tlink.x");
}
