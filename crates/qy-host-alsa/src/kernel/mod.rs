//! Linux 内核 ALSA 接口的纯数据部分（A1）：结构体与 ioctl 编号、硬件参数空间的操作、TLV 解析
//! （声道映射与 dB 信息）、混音器控制的分组与换算（A2、A4）。不含 `unsafe` 与系统调用，可在任何
//! 系统上编译与测试；系统调用与 [`crate::pcm::Pcm`] 的内核实现在 `crate::linux`。

pub mod abi;
pub mod hw;
pub mod mixer;
pub mod tlv;
