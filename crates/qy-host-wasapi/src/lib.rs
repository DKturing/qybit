//! QyBit WASAPI 宿主（M0 T5；M2 T1 起与平台无关的部分移入 `qy-player`）。
//!
//! - [`attempt`]：WASAPI 独占协商记录（HRESULT、描述类型）与格式候选；
//! - [`matrix`]：设备格式矩阵与报告；
//! - `wasapi`（仅 Windows）：WASAPI 独占事件驱动后端与 MMCSS，渲染使用
//!   `qy_player::render::packet` 整包循环；
//! - `sdk_format`（仅 Windows）：纯格式描述到 SDK 结构的字段映射；
//! - `endpoints`（仅 Windows）：端点枚举；
//! - `volume`（仅 Windows）：端点的硬件音量（GUI T5，只用设备在硬件上实现的音量）；
//! - `interrupt`（仅 Windows）：Ctrl+C 中断标志。
//!
//! 宿主资源、播放驱动与报告见 `qy-player`。

pub mod attempt;
pub mod matrix;

#[cfg(windows)]
pub mod endpoints;
#[cfg(windows)]
pub mod interrupt;
#[cfg(windows)]
pub mod sdk_format;
#[cfg(windows)]
pub mod volume;
#[cfg(windows)]
pub mod wasapi;

#[cfg(test)]
mod fake;

/// MMCSS 优先级在报告中的名称。
pub const MMCSS_LABEL: &str = "MMCSS “Pro Audio”";
