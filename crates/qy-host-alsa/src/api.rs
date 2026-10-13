//! 把 ALSA PCM 操作适配为 `qy_player` 短写循环的 [`WriteApi`]。
//!
//! 错误映射（出处：alsa-lib PCM 文档对 `snd_pcm_writei`/`snd_pcm_avail`/`snd_pcm_wait` 返回值的说明）：
//! - `EPIPE`：xrun（设备已取空）。`avail`/`writei` 报告为 [`Avail::Xrun`]/[`Written::Xrun`]，
//!   由循环记为欠载并调用 `recover`（`snd_pcm_prepare`）；`wait` 遇到 `EPIPE` 视为已唤醒，
//!   随后的 `avail` 报告 xrun。
//! - `EAGAIN`/`EINTR`：本次写入 0 帧，下次继续。
//! - `ESTRPIPE`（挂起）、`ENODEV`（设备消失）及其他：设备错误，循环结束并报告。
//! - `drain` 返回 `EPIPE`：设备在排空前已取空，即已写入的数据都已播放完，按排空完成处理。

use qy_player::render::write::{Avail, WriteApi, Written};
use qy_player::render::{ApiError, ApiOp, ErrorCode, WaitResult};

use crate::pcm::{Pcm, PcmError, errno};

fn api_err(op: ApiOp, e: PcmError) -> ApiError {
    ApiError {
        op,
        code: ErrorCode::Errno(e.errno),
    }
}

/// 渲染线程独占的 ALSA PCM。
pub struct AlsaWriteApi<P: Pcm> {
    pub pcm: P,
}

impl<P: Pcm> WriteApi for AlsaWriteApi<P> {
    fn start(&mut self) -> Result<(), ApiError> {
        self.pcm.start().map_err(|e| api_err(ApiOp::Start, e))
    }

    fn stop(&mut self) -> Result<(), ApiError> {
        self.pcm.drop_now().map_err(|e| api_err(ApiOp::Stop, e))
    }

    fn drain(&mut self) -> Result<(), ApiError> {
        match self.pcm.drain() {
            Ok(()) => Ok(()),
            Err(e) if e.errno == errno::EPIPE => Ok(()),
            Err(e) => Err(api_err(ApiOp::Drain, e)),
        }
    }

    fn wait(&mut self, timeout_ms: u32) -> Result<WaitResult, ApiError> {
        match self.pcm.wait(timeout_ms) {
            Ok(true) => Ok(WaitResult::Signaled),
            Ok(false) => Ok(WaitResult::Timeout),
            Err(e) if matches!(e.errno, errno::EPIPE | errno::EINTR) => Ok(WaitResult::Signaled),
            Err(e) => Err(api_err(ApiOp::Wait, e)),
        }
    }

    fn avail(&mut self) -> Result<Avail, ApiError> {
        match self.pcm.avail() {
            Ok(n) => Ok(Avail::Ready(u32::try_from(n).unwrap_or(u32::MAX))),
            Err(e) if e.errno == errno::EPIPE => Ok(Avail::Xrun {
                missing_frames: self.pcm.xrun_missing_frames(),
            }),
            Err(e) => Err(api_err(ApiOp::Avail, e)),
        }
    }

    fn write(&mut self, frames: &[u8]) -> Result<Written, ApiError> {
        match self.pcm.writei(frames) {
            Ok(k) => Ok(Written::Frames(u32::try_from(k).unwrap_or(u32::MAX))),
            Err(e) if matches!(e.errno, errno::EAGAIN | errno::EINTR) => Ok(Written::Frames(0)),
            Err(e) if e.errno == errno::EPIPE => Ok(Written::Xrun {
                missing_frames: self.pcm.xrun_missing_frames(),
            }),
            Err(e) => Err(api_err(ApiOp::Write, e)),
        }
    }

    fn recover(&mut self) -> Result<(), ApiError> {
        self.pcm.prepare().map_err(|e| api_err(ApiOp::Recover, e))
    }
}
