//! Ctrl+C（SIGINT，仅 Linux）：第一次只置位中断标志，由播放驱动停止设备并给出完整报告；
//! 处理函数同时恢复默认处理，再次按下即按默认方式结束进程，保证总能退出。
//!
//! 出处：`signal(2)`、`signal-safety(7)`（处理函数中只做原子写与 `signal` 调用，二者都是
//! 异步信号安全的）。
#![allow(unsafe_code)]

use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handler(_sig: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Release);
    // SAFETY: signal 是异步信号安全函数；恢复默认处理。
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
    }
}

/// 安装处理函数，返回中断标志。
pub fn install() -> Result<&'static AtomicBool, String> {
    let f: extern "C" fn(libc::c_int) = handler;
    // SAFETY: handler 只做原子写与 signal 调用（异步信号安全）。
    let prev = unsafe { libc::signal(libc::SIGINT, f as libc::sighandler_t) };
    if prev == libc::SIG_ERR {
        return Err(format!(
            "安装 Ctrl+C 处理失败：{}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(&INTERRUPTED)
}
