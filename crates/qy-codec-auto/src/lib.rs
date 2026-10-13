//! 按文件头选择自有解码器（M1 T3）。
//!
//! `#![no_std]`、无 alloc、静态分发：同时持有 [`WavDecoder`] 与 [`FlacDecoder`]，各自使用
//! 宿主启动时提供的独立工作区；每首曲目开始时（`reset` 之后）读取前 4 字节：
//! `RIFF`/`RIFX`/`RF64`/`BW64` 交给 WAV 解码器（后者再区分支持与否），`fLaC` 交给 FLAC
//! 解码器，其他返回 `Unsupported`。不依据文件扩展名。
#![no_std]

use qy_codec_flac::{Capacity, FlacDecoder, FlacError, InitError};
use qy_codec_wav::WavDecoder;
use qy_core::{DecodeError, Decoder, Progress, SourceFormat, Status};

/// 当前曲目使用的解码器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// 尚未读到足够的文件头。
    Unknown,
    Wav,
    Flac,
    /// 文件头不是支持的容器。
    Unrecognized,
}

/// 构造错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoInitError {
    Wav(DecodeError),
    Flac(InitError),
}

pub struct AutoDecoder {
    wav: WavDecoder,
    flac: FlacDecoder,
    codec: Codec,
}

impl AutoDecoder {
    /// `wav_pcm` 为 WAV 工作区；FLAC 工作区与 PCM 按 `flac_cap` 检查。
    pub fn new(
        wav_pcm: &'static mut [i32],
        flac_cap: Capacity,
        flac_workspace: &'static mut [i64],
        flac_pcm: &'static mut [i32],
    ) -> Result<Self, AutoInitError> {
        Ok(Self {
            wav: WavDecoder::new(wav_pcm).map_err(AutoInitError::Wav)?,
            flac: FlacDecoder::new(flac_cap, flac_workspace, flac_pcm)
                .map_err(AutoInitError::Flac)?,
            codec: Codec::Unknown,
        })
    }

    /// 核心输入窗口须满足的最小容量（FLAC 整帧窗口）。
    pub fn required_window(&self) -> usize {
        self.flac.capacity().input_window
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// FLAC 错误明细（当前曲目为 FLAC 且已出错时）。
    pub fn flac_error(&self) -> Option<FlacError> {
        match self.codec {
            Codec::Flac => self.flac.last_error(),
            _ => None,
        }
    }

    pub fn flac(&self) -> &FlacDecoder {
        &self.flac
    }

    pub fn wav(&self) -> &WavDecoder {
        &self.wav
    }
}

impl Decoder for AutoDecoder {
    fn reset(&mut self) {
        self.codec = Codec::Unknown;
        self.wav.reset();
        self.flac.reset();
    }

    fn format(&self) -> Option<SourceFormat> {
        match self.codec {
            Codec::Wav => self.wav.format(),
            Codec::Flac => self.flac.format(),
            _ => None,
        }
    }

    fn decode(&mut self, input: &[u8], eof: bool) -> Result<Progress, DecodeError> {
        if self.codec == Codec::Unknown {
            if input.len() < 4 {
                return if eof {
                    // 不足 4 字节的文件：无法识别，也不是完整的 WAV/FLAC。
                    Err(DecodeError::Truncated)
                } else {
                    Ok(Progress {
                        consumed: 0,
                        status: Status::NeedInput,
                    })
                };
            }
            self.codec = match &input[..4] {
                b"RIFF" | b"RIFX" | b"RF64" | b"BW64" => Codec::Wav,
                b"fLaC" => Codec::Flac,
                _ => Codec::Unrecognized,
            };
        }
        match self.codec {
            Codec::Wav => self.wav.decode(input, eof),
            Codec::Flac => self.flac.decode(input, eof),
            _ => Err(DecodeError::Unsupported),
        }
    }

    fn pcm(&self) -> &[i32] {
        match self.codec {
            Codec::Wav => self.wav.pcm(),
            Codec::Flac => self.flac.pcm(),
            _ => &[],
        }
    }

    fn release(&mut self, frames: usize) {
        match self.codec {
            Codec::Wav => self.wav.release(frames),
            Codec::Flac => self.flac.release(frames),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::boxed::Box;
    use std::vec;

    fn leak<T: Copy + Default>(n: usize) -> &'static mut [T] {
        Box::leak(vec![T::default(); n].into_boxed_slice())
    }

    fn auto() -> AutoDecoder {
        let c = Capacity::MCU;
        match AutoDecoder::new(
            leak(qy_core::PCM_WORK_SAMPLES),
            c,
            leak(c.workspace_len()),
            leak(c.pcm_len()),
        ) {
            Ok(d) => d,
            Err(e) => panic!("{e:?}"),
        }
    }

    #[test]
    fn sniffs_container_and_resets_per_track() {
        let mut d = auto();
        assert_eq!(d.required_window(), Capacity::MCU.input_window);
        assert_eq!(
            d.decode(b"fL", false).map(|p| p.status),
            Ok(Status::NeedInput)
        );
        assert_eq!(d.codec(), Codec::Unknown);
        assert_eq!(d.decode(b"fL", true), Err(DecodeError::Truncated));
        d.reset();
        assert_eq!(
            d.decode(b"ID3\x04....", false),
            Err(DecodeError::Unsupported)
        );
        assert_eq!(d.codec(), Codec::Unrecognized);
        d.reset();
        // fLaC 后跟非 STREAMINFO 块：交由 FLAC 解码器判定。
        assert_eq!(
            d.decode(b"fLaC\x81\x00\x00\x00", true),
            Err(DecodeError::Malformed)
        );
        assert_eq!(d.codec(), Codec::Flac);
        assert!(d.flac_error().is_some());
        d.reset();
        assert_eq!(
            d.decode(b"RIFF\x04\x00\x00\x00WAVE", true),
            Err(DecodeError::Malformed)
        );
        assert_eq!(d.codec(), Codec::Wav);
        assert_eq!(d.format(), None);
        assert!(d.pcm().is_empty());
    }

    #[test]
    fn capacity_errors_propagate() {
        let c = Capacity::MCU;
        let r = AutoDecoder::new(leak(10), c, leak(c.workspace_len()), leak(c.pcm_len()));
        assert!(matches!(
            r,
            Err(AutoInitError::Wav(DecodeError::CapacityExceeded))
        ));
        let r = AutoDecoder::new(
            leak(qy_core::PCM_WORK_SAMPLES),
            c,
            leak(1),
            leak(c.pcm_len()),
        );
        assert!(matches!(r, Err(AutoInitError::Flac(_))));
    }
}
