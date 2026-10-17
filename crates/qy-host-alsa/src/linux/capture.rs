//! 回录线程（M2 T4 B 层，snd-aloop 的 `hw:Loopback,1,N`；A1 起直接使用内核接口）。
//!
//! 与播放端同样严格地设置参数（RW_INTERLEAVED、格式、声道、精确采样率、关闭重采样），
//! 读回并核对后启动；在独立线程中持续 `READI_FRAMES`，直到收到停止请求。
//! 回录线程只用于测试与验证，不在播放路径上。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use qy_pcm_format::alsa::{AlsaFormat, HwApplied};

use super::kernel_pcm::KernelPcm;
use crate::kernel::abi::STREAM_CAPTURE;
use crate::pcm::{Pcm, PcmError, errno};

/// 回录缓冲的周期数。
const CAPTURE_PERIODS: u32 = 8;

/// 运行中的回录。
pub struct Capture {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Result<Vec<u8>, PcmError>>,
}

impl Capture {
    /// 停止并取回全部回录字节。
    pub fn finish(self) -> Result<Vec<u8>, PcmError> {
        self.stop.store(true, Ordering::Release);
        match self.handle.join() {
            Ok(r) => r,
            Err(_) => Err(PcmError {
                call: "capture thread",
                errno: 0,
            }),
        }
    }
}

/// 打开并启动回录，返回回录句柄与读回值（格式代码、精确采样率、声道等）。
pub fn start_capture(
    name: &str,
    format: AlsaFormat,
    rate: u32,
    channels: u16,
    period_frames: u32,
) -> Result<(Capture, HwApplied), PcmError> {
    let mut pcm = KernelPcm::open(name, STREAM_CAPTURE)?;
    let applied = pcm.configure_capture(format, rate, channels, period_frames, CAPTURE_PERIODS)?;
    if applied.format_code != format.code()
        || (applied.rate_num, applied.rate_den) != (rate, 1)
        || applied.channels != u32::from(channels)
    {
        return Err(PcmError {
            call: "capture HW_PARAMS（读回与请求不一致）",
            errno: errno::EINVAL,
        });
    }
    pcm.start()?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);
    let frame = usize::from(format.container_bits() / 8) * usize::from(channels);
    let chunk = applied.period_frames.max(64) as usize * frame;
    let handle = std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut buf = vec![0u8; chunk];
        while !stop2.load(Ordering::Acquire) {
            match pcm.readi(&mut buf) {
                Ok(frames) => out.extend_from_slice(&buf[..(frames * frame).min(buf.len())]),
                Err(e) if e.errno == errno::EPIPE => {
                    // 回录溢出：记为错误，回环结果不可信。
                    return Err(PcmError {
                        call: "SNDRV_PCM_IOCTL_READI_FRAMES（回录溢出）",
                        errno: e.errno,
                    });
                }
                Err(e) => return Err(e),
            }
        }
        let _ = pcm.drop_now();
        Ok(out)
    });
    Ok((Capture { stop, handle }, applied))
}
