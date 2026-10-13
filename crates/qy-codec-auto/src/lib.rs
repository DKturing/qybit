//! 按文件头选择自有解码器（M1 T3；M3 T1 起各格式可按需链接）。
//!
//! `#![no_std]`、无 alloc、静态分发：持有 [`WavDecoder`] 与/或 [`FlacDecoder`]，各自使用
//! 宿主启动时提供的独立工作区；每首曲目开始时（`reset` 之后）读取前 4 字节：
//! `RIFF`/`RIFX`/`RF64`/`BW64` 交给 WAV 解码器（后者再区分支持与否），`fLaC` 交给 FLAC
//! 解码器，其他返回 `Unsupported`。不依据文件扩展名。
//!
//! 特性 `wav`、`flac`（默认都开）决定链接哪些解码器；只开 `wav` 时不链接 FLAC 代码。
//! 已链接但宿主未提供工作区的格式，与未链接的格式一样按 `Unsupported` 拒绝（[`Codec::Disabled`]）。
//! 各配置档的资源预算见 [`profile`]。
#![no_std]

pub mod profile;

#[cfg(feature = "flac")]
use qy_codec_flac::{Capacity, FlacDecoder, FlacError, InitError};
#[cfg(feature = "wav")]
use qy_codec_wav::WavDecoder;
use qy_core::{DecodeError, Decoder, INPUT_WINDOW_BYTES, Progress, SourceFormat, Status};

/// 当前曲目使用的解码器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// 尚未读到足够的文件头。
    Unknown,
    Wav,
    Flac,
    /// 文件头不是支持的容器。
    Unrecognized,
    /// 可识别的容器，但本构建未链接或宿主未启用该解码器。
    Disabled,
}

/// 构造错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoInitError {
    #[cfg(feature = "wav")]
    Wav(DecodeError),
    #[cfg(feature = "flac")]
    Flac(InitError),
}

pub struct AutoDecoder {
    #[cfg(feature = "wav")]
    wav: Option<WavDecoder>,
    #[cfg(feature = "flac")]
    flac: Option<FlacDecoder>,
    codec: Codec,
}

impl Default for AutoDecoder {
    fn default() -> Self {
        Self::empty()
    }
}

impl AutoDecoder {
    /// 不含任何解码器；用 [`AutoDecoder::with_wav`] / [`AutoDecoder::with_flac`] 加入。
    pub const fn empty() -> Self {
        Self {
            #[cfg(feature = "wav")]
            wav: None,
            #[cfg(feature = "flac")]
            flac: None,
            codec: Codec::Unknown,
        }
    }

    /// WAV 与 FLAC 都启用：`wav_pcm` 为 WAV 工作区；FLAC 工作区与 PCM 按 `flac_cap` 检查。
    #[cfg(all(feature = "wav", feature = "flac"))]
    pub fn new(
        wav_pcm: &'static mut [i32],
        flac_cap: Capacity,
        flac_workspace: &'static mut [i64],
        flac_pcm: &'static mut [i32],
    ) -> Result<Self, AutoInitError> {
        Self::empty()
            .with_wav(wav_pcm)?
            .with_flac(flac_cap, flac_workspace, flac_pcm)
    }

    /// 启用 WAV 解码器（工作区不少于 `PCM_WORK_SAMPLES`）。
    #[cfg(feature = "wav")]
    pub fn with_wav(mut self, pcm: &'static mut [i32]) -> Result<Self, AutoInitError> {
        self.wav = Some(WavDecoder::new(pcm).map_err(AutoInitError::Wav)?);
        Ok(self)
    }

    /// 启用 FLAC 解码器；工作区与 PCM 按 `cap` 检查。
    #[cfg(feature = "flac")]
    pub fn with_flac(
        mut self,
        cap: Capacity,
        workspace: &'static mut [i64],
        pcm: &'static mut [i32],
    ) -> Result<Self, AutoInitError> {
        self.flac = Some(FlacDecoder::new(cap, workspace, pcm).map_err(AutoInitError::Flac)?);
        Ok(self)
    }

    /// 核心输入窗口须满足的最小容量：FLAC 整帧窗口（启用时），且不小于核心的最小窗口。
    pub fn required_window(&self) -> usize {
        #[cfg(feature = "flac")]
        if let Some(f) = &self.flac {
            return f.capacity().input_window.max(INPUT_WINDOW_BYTES);
        }
        INPUT_WINDOW_BYTES
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// FLAC 错误明细（当前曲目为 FLAC 且已出错时）。
    #[cfg(feature = "flac")]
    pub fn flac_error(&self) -> Option<FlacError> {
        match (self.codec, &self.flac) {
            (Codec::Flac, Some(f)) => f.last_error(),
            _ => None,
        }
    }

    #[cfg(feature = "flac")]
    pub fn flac(&self) -> Option<&FlacDecoder> {
        self.flac.as_ref()
    }

    #[cfg(feature = "wav")]
    pub fn wav(&self) -> Option<&WavDecoder> {
        self.wav.as_ref()
    }

    fn sniff(&self, magic: &[u8]) -> Codec {
        match magic {
            b"RIFF" | b"RIFX" | b"RF64" | b"BW64" => {
                #[cfg(feature = "wav")]
                if self.wav.is_some() {
                    return Codec::Wav;
                }
                Codec::Disabled
            }
            b"fLaC" => {
                #[cfg(feature = "flac")]
                if self.flac.is_some() {
                    return Codec::Flac;
                }
                Codec::Disabled
            }
            _ => Codec::Unrecognized,
        }
    }
}

/// 按当前曲目的解码器静态分发；未选中或未启用时求值 `$none`。
macro_rules! dispatch {
    ($self:ident, $d:ident => $e:expr, $none:expr) => {
        match $self.codec {
            #[cfg(feature = "wav")]
            Codec::Wav => match &mut $self.wav {
                Some($d) => $e,
                None => $none,
            },
            #[cfg(feature = "flac")]
            Codec::Flac => match &mut $self.flac {
                Some($d) => $e,
                None => $none,
            },
            _ => $none,
        }
    };
}

macro_rules! dispatch_ref {
    ($self:ident, $d:ident => $e:expr, $none:expr) => {
        match $self.codec {
            #[cfg(feature = "wav")]
            Codec::Wav => match &$self.wav {
                Some($d) => $e,
                None => $none,
            },
            #[cfg(feature = "flac")]
            Codec::Flac => match &$self.flac {
                Some($d) => $e,
                None => $none,
            },
            _ => $none,
        }
    };
}

impl Decoder for AutoDecoder {
    fn reset(&mut self) {
        self.codec = Codec::Unknown;
        #[cfg(feature = "wav")]
        if let Some(d) = &mut self.wav {
            d.reset();
        }
        #[cfg(feature = "flac")]
        if let Some(d) = &mut self.flac {
            d.reset();
        }
    }

    fn format(&self) -> Option<SourceFormat> {
        dispatch_ref!(self, d => d.format(), None)
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
            self.codec = self.sniff(&input[..4]);
        }
        dispatch!(self, d => d.decode(input, eof), Err(DecodeError::Unsupported))
    }

    fn pcm(&self) -> &[i32] {
        dispatch_ref!(self, d => d.pcm(), &[])
    }

    #[cfg_attr(not(any(feature = "wav", feature = "flac")), allow(unused_variables))]
    fn release(&mut self, frames: usize) {
        dispatch!(self, d => d.release(frames), ())
    }
}

#[cfg(all(test, feature = "wav", feature = "flac"))]
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

    /// 已链接但未启用的格式按 `Unsupported` 拒绝，不误交给其他解码器。
    #[test]
    fn disabled_codecs_are_unsupported() {
        let mut d = AutoDecoder::empty();
        assert_eq!(d.required_window(), qy_core::INPUT_WINDOW_BYTES);
        assert_eq!(
            d.decode(b"fLaC\0\0\0\x22", false),
            Err(DecodeError::Unsupported)
        );
        assert_eq!(d.codec(), Codec::Disabled);
        d.reset();
        assert_eq!(
            d.decode(b"RIFF\0\0\0\0WAVE", false),
            Err(DecodeError::Unsupported)
        );
        assert_eq!(d.codec(), Codec::Disabled);
        let Ok(mut w) = AutoDecoder::empty().with_wav(leak(qy_core::PCM_WORK_SAMPLES)) else {
            panic!("wav");
        };
        assert_eq!(
            w.decode(b"fLaC\0\0\0\x22", false),
            Err(DecodeError::Unsupported)
        );
        assert_eq!(w.codec(), Codec::Disabled);
        assert!(w.flac().is_none() && w.wav().is_some());
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
