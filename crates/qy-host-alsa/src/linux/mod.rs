//! Linux 专有部分：内核 ALSA 接口（系统调用边界、控制接口、PCM、混音器）、设备枚举、回录、占用进程
//! 查找与实时调度。alsa-lib 后端只在默认关闭的 `alsa-lib` 特性下编译（A1：只供 A 层回环测试
//! 使用 alsa-lib 的 `file` 插件，发布版不含）。

#[cfg(feature = "alsa-lib")]
pub mod alsalib;
pub mod capture;
pub mod ctl;
pub mod devices;
pub mod interrupt;
pub mod kernel_pcm;
pub mod mixer;
pub mod occupants;
pub mod priority;
pub mod sys;

pub use kernel_pcm::{KernelOpener, KernelPcm};
