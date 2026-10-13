//! 渲染线程实时调度（`SCHED_FIFO`）。只作尝试：失败（常见为无 `CAP_SYS_NICE` 或
//! `RLIMIT_RTPRIO` 为 0 时的 `EPERM`）写入报告，不阻止播放；结束时恢复原调度策略。
//!
//! 出处：Linux `sched_setscheduler(2)`、`sched_getscheduler(2)`、`sched_getparam(2)`：
//! pid 为 0 时作用于调用线程。
#![allow(unsafe_code)]

use qy_player::render::{ErrorCode, ThreadPriority};

/// `SCHED_FIFO` 优先级（1..=99 中取中等值，低于常见音频服务器的设置）。
pub const RT_PRIORITY: i32 = 70;

#[derive(Debug, Clone, Default)]
pub struct RtPriority {
    saved: Option<(i32, i32)>,
}

fn last_errno() -> ErrorCode {
    ErrorCode::Errno(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
}

impl ThreadPriority for RtPriority {
    fn label(&self) -> &'static str {
        "SCHED_FIFO 实时调度"
    }

    fn register(&mut self) -> Result<(), ErrorCode> {
        // SAFETY: 只查询调用线程（pid 0）的调度策略与参数；param 为本地变量。
        let (policy, prio) = unsafe {
            let policy = libc::sched_getscheduler(0);
            let mut param: libc::sched_param = std::mem::zeroed();
            if policy < 0 || libc::sched_getparam(0, &mut param) != 0 {
                return Err(last_errno());
            }
            (policy, param.sched_priority)
        };
        // SAFETY: 只修改调用线程的调度策略；param 为有效的本地变量。
        let r = unsafe {
            let mut param: libc::sched_param = std::mem::zeroed();
            param.sched_priority = RT_PRIORITY;
            libc::sched_setscheduler(0, libc::SCHED_FIFO, &param)
        };
        if r != 0 {
            return Err(last_errno());
        }
        self.saved = Some((policy, prio));
        Ok(())
    }

    fn revert(&mut self) -> Result<(), ErrorCode> {
        let Some((policy, prio)) = self.saved.take() else {
            return Ok(());
        };
        // SAFETY: 恢复调用线程注册前的调度策略与参数。
        let r = unsafe {
            let mut param: libc::sched_param = std::mem::zeroed();
            param.sched_priority = prio;
            libc::sched_setscheduler(0, policy, &param)
        };
        if r != 0 {
            return Err(last_errno());
        }
        Ok(())
    }
}
