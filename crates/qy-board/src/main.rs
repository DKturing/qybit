//! QyBit 板级适配层（M3 T2）：每块板一个模块，用 Cargo 特性选择，一次只构建一块板。
//!
//! 板级代码只做外设初始化与 `qy-host-embedded` 接口（存储、音频输出、串口）的实现；播放规则、
//! 文件系统读取、喂数与命令都在 `qy-host-embedded` / `qy-core`，与板无关。新增一块板：加一个
//! 模块、一个同名特性（带上该板的 HAL 依赖）和该模块目录下的 `memory.x`。
//!
//! 已有的板（都只验证能编译，**未在实物上运行**，不代表推荐型号）：
//! - `stm32f411`：见 [`stm32f411`] 模块。
#![no_std]
#![no_main]

#[cfg(not(feature = "stm32f411"))]
compile_error!("请用 Cargo 特性选择一块板，例如 `--features stm32f411`");

#[cfg(feature = "stm32f411")]
mod stm32f411;
