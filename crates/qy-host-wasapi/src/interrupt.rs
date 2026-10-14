//! Ctrl+C / Ctrl+Break（仅 Windows）：第一次只置位中断标志，由播放驱动停止设备并给出完整报告；
//! 再次按下时交给系统默认处理（结束进程），保证总能退出。
//!
//! 出处：Microsoft Learn `SetConsoleCtrlHandler`、`HandlerRoutine`（处理函数在系统创建的新线程中
//! 调用；返回 TRUE 表示已处理，FALSE 交给下一个处理函数，最终为默认的 `ExitProcess`）。
#![allow(unsafe_code)]

use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};
use windows::core::BOOL;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// 只做一次原子交换，不分配、不加锁。
unsafe extern "system" fn handler(ctrl: u32) -> BOOL {
    let ours = ctrl == CTRL_C_EVENT || ctrl == CTRL_BREAK_EVENT;
    if ours && !INTERRUPTED.swap(true, Ordering::AcqRel) {
        BOOL(1)
    } else {
        BOOL(0)
    }
}

/// 安装处理函数，返回中断标志。
pub fn install() -> Result<&'static AtomicBool, String> {
    // SAFETY: handler 是 'static 的 extern "system" 函数，签名与 PHANDLER_ROUTINE 一致，
    // 只访问静态原子变量。
    unsafe { SetConsoleCtrlHandler(Some(handler), true) }
        .map_err(|e| format!("安装 Ctrl+C 处理失败：{e}"))?;
    Ok(&INTERRUPTED)
}
