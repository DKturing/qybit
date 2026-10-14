//! Linux 专有部分：alsa-lib 绑定、设备枚举、占用进程查找与实时调度。

pub mod capture;
pub mod devices;
pub mod interrupt;
pub mod occupants;
pub mod priority;
pub mod real;

pub use real::{RealOpener, RealPcm};
