//! 输出后端 trait（M0 T5 起，M2 T1 泛化）。
//!
//! 后端负责协商并打开设备流，创建渲染线程并在其中运行 [`crate::render::packet`]
//! 或 [`crate::render::write`] 循环。播放驱动只通过这些 trait 与后端交互。

use std::sync::Arc;

use qy_core::{DeviceFormat, SourceFormat};
use qy_pcm_format::ring::FrameConsumer;

use crate::render::{RenderControl, RenderReport};
use crate::report::{AttemptRecord, Negotiation, OpenFailure, Submission};

/// 已协商成功、尚未开始渲染的流。
pub trait PendingStream {
    type Attempt: AttemptRecord;
    type Running: RunningStream;
    fn format(&self) -> DeviceFormat;
    fn negotiation(&self) -> &Negotiation<Self::Attempt>;
    fn submission(&self) -> Submission;
    /// 移交队列消费端并开始渲染（先预填设备缓冲再启动）。
    fn start(self, consumer: FrameConsumer, ctl: Arc<RenderControl>) -> Self::Running;
}

/// 渲染中的流。
pub trait RunningStream {
    fn is_finished(&self) -> bool;
    /// 等待渲染线程结束，归还消费端（线程异常时为 `None`）。
    fn join(self) -> (Option<FrameConsumer>, Option<RenderReport>);
}

/// 输出后端。打开失败（设备不支持、独占/硬件打开失败等）时不做任何回退或转换。
pub trait Backend {
    type Attempt: AttemptRecord;
    type Pending: PendingStream<Attempt = Self::Attempt>;
    fn open(&mut self, source: &SourceFormat) -> Result<Self::Pending, OpenFailure<Self::Attempt>>;
}
