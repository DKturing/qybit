//! Windows 原生控件前端。界面逻辑在共用的 [`crate::app`]（输入 → 显示操作）；`sys` 是 Win32 边界，
//! 本前端全部 `unsafe` 集中于此并逐处注明前提（AGENTS §2：平台绑定单独审计）。

mod sys;
mod volume;

pub fn run() -> std::process::ExitCode {
    sys::run()
}
