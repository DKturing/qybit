//! 回录线程（M2 T4 B 层，snd-aloop 的 `hw:Loopback,1,N`）。
//!
//! 与播放端同样严格地设置参数（RW_INTERLEAVED、格式、声道、精确采样率、关闭重采样），
//! 读回并核对后启动；在独立线程中持续 `snd_pcm_readi`，直到收到停止请求。
//! 回录线程只用于测试与验证，不在播放路径上。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use alsa::pcm::{Access as A, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use qy_pcm_format::alsa::{AlsaFormat, HwApplied};

use crate::pcm::PcmError;

fn e(call: &'static str, err: alsa::Error) -> PcmError {
    PcmError {
        call,
        errno: err.errno().saturating_abs(),
    }
}

fn to_alsa(f: AlsaFormat) -> Format {
    match f {
        AlsaFormat::S16Le => Format::S16LE,
        AlsaFormat::S24_3Le => Format::S243LE,
        AlsaFormat::S24Le => Format::S24LE,
        AlsaFormat::S32Le => Format::S32LE,
    }
}

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

/// 打开并启动回录，返回回录句柄与读回的格式代码、精确采样率（`snd_pcm_hw_params_get_rate`）。
pub fn start_capture(
    name: &str,
    format: AlsaFormat,
    rate: u32,
    channels: u16,
    period_frames: u32,
) -> Result<(Capture, HwApplied), PcmError> {
    let pcm = PCM::new(name, Direction::Capture, false).map_err(|x| e("snd_pcm_open", x))?;
    let applied = {
        let hwp = HwParams::any(&pcm).map_err(|x| e("snd_pcm_hw_params_any", x))?;
        hwp.set_access(A::RWInterleaved)
            .map_err(|x| e("snd_pcm_hw_params_set_access", x))?;
        hwp.set_format(to_alsa(format))
            .map_err(|x| e("snd_pcm_hw_params_set_format", x))?;
        hwp.set_channels(u32::from(channels))
            .map_err(|x| e("snd_pcm_hw_params_set_channels", x))?;
        hwp.set_rate_resample(false)
            .map_err(|x| e("snd_pcm_hw_params_set_rate_resample", x))?;
        hwp.set_rate(rate, ValueOr::Nearest)
            .map_err(|x| e("snd_pcm_hw_params_set_rate", x))?;
        let p = hwp
            .set_period_size_near(i64::from(period_frames), ValueOr::Nearest)
            .map_err(|x| e("snd_pcm_hw_params_set_period_size_near", x))?;
        hwp.set_buffer_size_near(p.saturating_mul(8))
            .map_err(|x| e("snd_pcm_hw_params_set_buffer_size_near", x))?;
        pcm.hw_params(&hwp).map_err(|x| e("snd_pcm_hw_params", x))?;
        let cur = pcm
            .hw_params_current()
            .map_err(|x| e("snd_pcm_hw_params_current", x))?;
        HwApplied {
            access: qy_pcm_format::alsa::Access::RwInterleaved,
            format_code: cur
                .get_format()
                .map_err(|x| e("snd_pcm_hw_params_get_format", x))? as i32,
            rate_num: cur
                .get_rate()
                .map_err(|x| e("snd_pcm_hw_params_get_rate", x))?,
            rate_den: 1,
            channels: cur
                .get_channels()
                .map_err(|x| e("snd_pcm_hw_params_get_channels", x))?,
            period_frames: u32::try_from(p).unwrap_or(u32::MAX),
            buffer_frames: cur
                .get_buffer_size()
                .map(|b| u32::try_from(b).unwrap_or(u32::MAX))
                .map_err(|x| e("snd_pcm_hw_params_get_buffer_size", x))?,
            resample: cur
                .get_rate_resample()
                .map_err(|x| e("snd_pcm_hw_params_get_rate_resample", x))?,
            significant_bits: None,
        }
    };
    if applied.format_code != format.code()
        || applied.rate_num != rate
        || applied.channels != u32::from(channels)
    {
        return Err(PcmError {
            call: "capture hw_params_current（读回与请求不一致）",
            errno: crate::pcm::errno::EINVAL,
        });
    }
    pcm.start().map_err(|x| e("snd_pcm_start", x))?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);
    let frame = usize::from(format.container_bits() / 8) * usize::from(channels);
    let chunk = period_frames.max(64) as usize * frame;
    let handle = std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut buf = vec![0u8; chunk];
        while !stop2.load(Ordering::Acquire) {
            let io = pcm.io_bytes();
            match io.readi(&mut buf) {
                Ok(frames) => out.extend_from_slice(&buf[..frames * frame]),
                Err(x) if x.errno().saturating_abs() == crate::pcm::errno::EPIPE => {
                    // 回录溢出：记为错误，回环结果不可信。
                    return Err(e("snd_pcm_readi（回录溢出）", x));
                }
                Err(x) => return Err(e("snd_pcm_readi", x)),
            }
        }
        let _ = pcm.drop();
        Ok(out)
    });
    Ok((Capture { stop, handle }, applied))
}
