//! 渲染线程实时调度（`SCHED_FIFO`）。只作尝试：失败（常见为无 `CAP_SYS_NICE` 或
//! `RLIMIT_RTPRIO` 为 0 时的 `EPERM`）写入报告，不阻止播放；结束时恢复原调度策略。
//!
//! 出处：POSIX `pthread_getschedparam(3)`、`pthread_setschedparam(3)`（作用于调用线程）。
//! 不用 `sched_setscheduler(2)`：musl 按 POSIX 的进程级语义不实现它（固定返回 `ENOSYS`），
//! 而线程级的 pthread 接口在 glibc 与 musl 中都直接对线程 id 调用内核（A1 静态程序用 musl）。
#![allow(unsafe_code)]

use qy_player::render::{ErrorCode, ThreadPriority};

/// `SCHED_FIFO` 优先级（1..=99 中取中等值，低于常见音频服务器的设置）。
pub const RT_PRIORITY: i32 = 70;

#[derive(Debug, Clone, Default)]
pub struct RtPriority {
    saved: Option<(i32, i32)>,
}

/// pthread 接口直接返回错误号（不设 errno）。
fn check(r: i32) -> Result<(), ErrorCode> {
    if r == 0 {
        Ok(())
    } else {
        Err(ErrorCode::Errno(r))
    }
}

impl ThreadPriority for RtPriority {
    fn label(&self) -> &'static str {
        "SCHED_FIFO 实时调度"
    }

    fn register(&mut self) -> Result<(), ErrorCode> {
        // SAFETY: 只查询调用线程（pthread_self）的调度策略与参数；输出为本地变量。
        let (policy, prio) = unsafe {
            let mut policy = 0;
            let mut param: libc::sched_param = std::mem::zeroed();
            check(libc::pthread_getschedparam(
                libc::pthread_self(),
                &mut policy,
                &mut param,
            ))?;
            (policy, param.sched_priority)
        };
        // SAFETY: 只修改调用线程的调度策略；param 为有效的本地变量。
        check(unsafe {
            let mut param: libc::sched_param = std::mem::zeroed();
            param.sched_priority = RT_PRIORITY;
            libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param)
        })?;
        self.saved = Some((policy, prio));
        Ok(())
    }

    fn revert(&mut self) -> Result<(), ErrorCode> {
        let Some((policy, prio)) = self.saved.take() else {
            return Ok(());
        };
        // SAFETY: 恢复调用线程注册前的调度策略与参数。
        check(unsafe {
            let mut param: libc::sched_param = std::mem::zeroed();
            param.sched_priority = prio;
            libc::pthread_setschedparam(libc::pthread_self(), policy, &param)
        })
    }
}
