//! 把 ALSA PCM 操作适配为 `qy_player` 短写循环的 [`WriteApi`]。
//!
//! 错误映射（出处：alsa-lib PCM 文档对 `snd_pcm_writei`/`snd_pcm_avail`/`snd_pcm_wait` 返回值的说明）：
//! - `EPIPE`：xrun（设备已取空）。`avail`/`writei` 报告为 [`Avail::Xrun`]/[`Written::Xrun`]，
//!   由循环记为欠载并调用 `recover`（`snd_pcm_prepare`）；`wait` 遇到 `EPIPE` 视为已唤醒，
//!   随后的 `avail` 报告 xrun。
//! - `EAGAIN`/`EINTR`：本次写入 0 帧，下次继续。
//! - `ESTRPIPE`（挂起）、`ENODEV`（设备消失）及其他：设备错误，循环结束并报告。
//! - `drain` 返回 `EPIPE`：设备在排空前已取空，即已写入的数据都已播放完，按排空完成处理。
//!
//! DSD（D1）：渲染循环按 DSD 字计帧（每字 e 个设备帧，见 `DeviceFormat::expansion`），本适配层
//! 换算为设备帧。可写空间按完整的字报告；`writei` 只写了一个字的一部分设备帧时，把这个字其余的
//! 设备帧留在 `carry` 中、按已写入报告，下一次写入（以及排空之前）先写完它。这样队列中永远是完整
//! 的字，DoP 标记的相位不会因短写而错位。PCM 的 e 为 1，与原来相同。

use qy_core::DeviceFormat;
use qy_pcm_format::MAX_FRAME_BYTES;
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
    /// 每个循环帧（DSD 为一个字）的设备帧数 e；PCM 为 1。
    expansion: usize,
    /// 一个设备帧的字节数。
    device_frame: usize,
    /// 已按写入报告、尚未交给设备的设备帧（一个字中短写剩下的部分）。
    carry: [u8; MAX_FRAME_BYTES],
    carry_len: usize,
}

impl<P: Pcm> AlsaWriteApi<P> {
    pub fn new(pcm: P, format: &DeviceFormat) -> Self {
        Self {
            pcm,
            expansion: format.expansion().max(1) as usize,
            device_frame: usize::from(format.device_frame_bytes()).max(1),
            carry: [0; MAX_FRAME_BYTES],
            carry_len: 0,
        }
    }

    /// 先写完上次剩下的设备帧；返回是否已写完（设备暂时不收时为 `false`）。
    fn flush_carry(&mut self) -> Result<Result<bool, Written>, ApiError> {
        while self.carry_len > 0 {
            match self.pcm.writei(&self.carry[..self.carry_len]) {
                Ok(0) => return Ok(Ok(false)),
                Ok(k) => {
                    let done = (k * self.device_frame).min(self.carry_len);
                    self.carry.copy_within(done..self.carry_len, 0);
                    self.carry_len -= done;
                }
                Err(e) if matches!(e.errno, errno::EAGAIN | errno::EINTR) => return Ok(Ok(false)),
                Err(e) if e.errno == errno::EPIPE => {
                    return Ok(Err(Written::Xrun {
                        missing_frames: self.missing_words(),
                    }));
                }
                Err(e) => return Err(api_err(ApiOp::Write, e)),
            }
        }
        Ok(Ok(true))
    }

    /// 后端报告的 xrun 缺失设备帧，按循环帧（字）向上取整。
    fn missing_words(&mut self) -> u32 {
        let e = self.expansion as u32;
        self.pcm.xrun_missing_frames().div_ceil(e)
    }
}

impl<P: Pcm> WriteApi for AlsaWriteApi<P> {
    fn start(&mut self) -> Result<(), ApiError> {
        self.pcm.start().map_err(|e| api_err(ApiOp::Start, e))
    }

    fn stop(&mut self) -> Result<(), ApiError> {
        self.carry_len = 0;
        self.pcm.drop_now().map_err(|e| api_err(ApiOp::Stop, e))
    }

    fn drain(&mut self) -> Result<(), ApiError> {
        // 已按写入报告的设备帧先写完。
        while self.carry_len > 0 {
            match self.flush_carry()? {
                Ok(true) => {}
                Ok(false) => {
                    if !self.wait(1000)?.eq(&WaitResult::Signaled) {
                        return Err(ApiError {
                            op: ApiOp::Drain,
                            code: ErrorCode::Timeout,
                        });
                    }
                }
                // 设备已取空：剩下的半个字随排空一起放弃（欠载已由循环记录）。
                Err(_) => self.carry_len = 0,
            }
        }
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
            Ok(n) => {
                let carry = (self.carry_len / self.device_frame) as u64;
                let words = n.saturating_sub(carry) / self.expansion as u64;
                Ok(Avail::Ready(u32::try_from(words).unwrap_or(u32::MAX)))
            }
            Err(e) if e.errno == errno::EPIPE => Ok(Avail::Xrun {
                missing_frames: self.missing_words(),
            }),
            Err(e) => Err(api_err(ApiOp::Avail, e)),
        }
    }

    fn write(&mut self, frames: &[u8]) -> Result<Written, ApiError> {
        match self.flush_carry()? {
            Ok(true) => {}
            Ok(false) => return Ok(Written::Frames(0)),
            Err(x) => return Ok(x),
        }
        let word = self.device_frame * self.expansion;
        match self.pcm.writei(frames) {
            Ok(k) => {
                let mut words = k / self.expansion;
                let rest = k % self.expansion;
                if rest > 0 {
                    // 写了一个字的一部分：其余设备帧留到下次，这个字按已写入报告。
                    let from = k * self.device_frame;
                    let to = (words + 1) * word;
                    if let Some(tail) = frames.get(from..to) {
                        self.carry[..tail.len()].copy_from_slice(tail);
                        self.carry_len = tail.len();
                        words += 1;
                    }
                }
                Ok(Written::Frames(u32::try_from(words).unwrap_or(u32::MAX)))
            }
            Err(e) if matches!(e.errno, errno::EAGAIN | errno::EINTR) => Ok(Written::Frames(0)),
            Err(e) if e.errno == errno::EPIPE => Ok(Written::Xrun {
                missing_frames: self.missing_words(),
            }),
            Err(e) => Err(api_err(ApiOp::Write, e)),
        }
    }

    fn recover(&mut self) -> Result<(), ApiError> {
        self.pcm.prepare().map_err(|e| api_err(ApiOp::Recover, e))
    }
}
