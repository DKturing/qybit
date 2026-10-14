//! 按文件头选择自有解码器（M1 T3；M3 T1 起各格式可按需链接）。
//!
//! `#![no_std]`、无 alloc、静态分发：持有 [`WavDecoder`] 与/或 [`FlacDecoder`]，各自使用
//! 宿主启动时提供的独立工作区；每首曲目开始时（`reset` 之后）读取前 4 字节：
//! `RIFF`/`RIFX`/`RF64`/`BW64` 交给 WAV 解码器（后者再区分支持与否），`fLaC` 交给 FLAC
//! 解码器，其他返回 `Unsupported`。不依据文件扩展名。
//!
//! 文件开头的 ID3v2 标签（常见于被标签软件写过的 FLAC）先按标签头声明的长度跳过，再识别其后的
//! 容器。依据：ID3v2.4.0 结构规范 §3.1（10 字节标签头：`ID3`、主/次版本各 1 字节且不为 `FF`、
//! 标志 1 字节、4 字节 syncsafe 长度，每字节最高位为 0；长度不含标签头，v2.4 标志位 4 置位时
//! 另有 10 字节标签尾）；参考解码器 libFLAC（`stream_decoder.c` 的 `skip_id3v2_tag_`）与
//! ffmpeg 的格式探测同样在识别前跳过 ID3v2。可连续多个标签；标签头不合规则时按无法识别拒绝。
//! 跳过的是元数据，不是音频，样本不受影响；跳过的字节数见 [`AutoDecoder::skipped_tag_bytes`]。
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
    /// 当前曲目文件头的前 4 字节（识别后记录，用于说明无法识别的原因）；跳过 ID3v2 标签时为
    /// 标签之后的 4 字节。
    magic: [u8; 4],
    /// 当前 ID3v2 标签尚未跳过的字节数。
    tag_left: u32,
    /// 当前曲目开头已跳过的 ID3v2 标签总字节数（含标签头与标签尾）。
    tag_bytes: u64,
}

/// ID3v2 标签头长度（ID3v2.4.0 结构规范 §3.1）。
const ID3_HEADER: usize = 10;

/// 解析 ID3v2 标签头，返回整个标签的字节数（标签头 + 声明长度 + 可选标签尾）；不合规则时为 `None`。
fn id3v2_len(h: &[u8; ID3_HEADER]) -> Option<u32> {
    if &h[..3] != b"ID3" || h[3] == 0xFF || h[4] == 0xFF || h[6..].iter().any(|&b| b & 0x80 != 0) {
        return None;
    }
    let size = h[6..]
        .iter()
        .fold(0u32, |acc, &b| (acc << 7) | u32::from(b));
    // 标签尾只在 v2.4 定义（标志位 4）。
    let footer = if h[3] == 4 && h[5] & 0x10 != 0 { 10 } else { 0 };
    // size < 2^28，加 20 不会溢出。
    Some(size + ID3_HEADER as u32 + footer)
}

/// 无法识别的文件头（报告用）：前 4 字节与按常见格式标记推断的类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnrecognizedHeader {
    /// 文件头前 4 字节；跳过了 ID3v2 标签时为标签之后的 4 字节。
    pub magic: [u8; 4],
    /// 之前跳过的 ID3v2 标签字节数。
    pub skipped_tag: u64,
}

impl core::fmt::Display for UnrecognizedHeader {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let m = self.magic;
        let guess = match &m {
            [b'I', b'D', b'3', _] => "ID3v2 标签头不合规则（版本或 syncsafe 长度字节无效）",
            b"OggS" => "Ogg 容器（Ogg FLAC、Vorbis、Opus 等），不支持",
            b"FORM" => "IFF/AIFF 容器，AIFF 在格式路线 F2 中，尚未实现",
            b"wvpk" => "WavPack，在格式路线 F5 中，尚未实现",
            b"MAC " => "Monkey's Audio（APE），在格式路线 F6 中，尚未实现",
            b"DSD " | b"FRM8" => "DSD（DSF/DFF），尚未实现",
            [_, _, _, _] if m[0] == 0xFF && m[1] & 0xE0 == 0xE0 => {
                "MPEG 音频帧（MP3 等有损格式），不在本程序范围内"
            }
            _ => "不是 RIFF/WAVE 或 FLAC",
        };
        if self.skipped_tag > 0 {
            write!(f, "跳过开头 {} 字节的 ID3v2 标签后，", self.skipped_tag)?;
        }
        write!(
            f,
            "文件头前 4 字节为 {:02X} {:02X} {:02X} {:02X}：{guess}",
            m[0], m[1], m[2], m[3]
        )
    }
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
            magic: [0; 4],
            tag_left: 0,
            tag_bytes: 0,
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

    /// WAV 拒绝明细（当前曲目为 WAV 且有明细时，如未定义的声道布局）。
    #[cfg(feature = "wav")]
    pub fn wav_detail(&self) -> Option<qy_codec_wav::WavDetail> {
        match (self.codec, &self.wav) {
            (Codec::Wav, Some(w)) => w.last_detail(),
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

    /// 当前曲目的文件头无法识别时，给出前 4 字节与推断的类型。
    pub fn unrecognized(&self) -> Option<UnrecognizedHeader> {
        (self.codec == Codec::Unrecognized).then_some(UnrecognizedHeader {
            magic: self.magic,
            skipped_tag: self.tag_bytes,
        })
    }

    /// 当前曲目开头跳过的 ID3v2 标签字节数（没有标签时为 0）。
    pub fn skipped_tag_bytes(&self) -> u64 {
        self.tag_bytes
    }

    /// 识别前的处理：跳过 ID3v2 标签并读取容器标记。`Ok(pos)` 表示已识别，容器从 `input[pos..]`
    /// 开始；`Err` 为本次调用的结果（缺输入或错误）。
    fn identify(
        &mut self,
        input: &[u8],
        eof: bool,
    ) -> Result<usize, Result<Progress, DecodeError>> {
        let mut pos = 0usize;
        loop {
            if self.tag_left > 0 {
                let n = (self.tag_left as usize).min(input.len() - pos);
                pos += n;
                // n ≤ tag_left（u32），转换不丢失。
                self.tag_left -= n as u32;
                self.tag_bytes += n as u64;
                if self.tag_left > 0 {
                    return Err(if eof {
                        // 标签声明的长度超出文件：不是完整的文件。
                        Err(DecodeError::Truncated)
                    } else {
                        Ok(Progress {
                            consumed: pos,
                            status: Status::NeedInput,
                        })
                    });
                }
            }
            let rest = &input[pos..];
            let tag = rest.starts_with(b"ID3") || (rest.len() < 3 && b"ID3".starts_with(rest));
            let need = if tag { ID3_HEADER } else { 4 };
            if rest.len() < need {
                return Err(if eof {
                    // 不足 4 字节的文件（或残缺的标签头）：无法识别，也不是完整的 WAV/FLAC。
                    Err(DecodeError::Truncated)
                } else {
                    Ok(Progress {
                        consumed: pos,
                        status: Status::NeedInput,
                    })
                });
            }
            if tag {
                let mut h = [0u8; ID3_HEADER];
                h.copy_from_slice(&rest[..ID3_HEADER]);
                if let Some(len) = id3v2_len(&h) {
                    self.tag_left = len;
                    continue;
                }
            }
            self.magic = [rest[0], rest[1], rest[2], rest[3]];
            self.codec = self.sniff(&rest[..4]);
            return Ok(pos);
        }
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

impl qy_core::engine::Resources for AutoDecoder {
    fn capacity_rejection(&self) -> Option<qy_core::engine::CapabilityError> {
        dispatch_ref!(self, d => d.capacity_rejection(), None)
    }

    fn demand(&self) -> Option<qy_core::engine::DecoderDemand> {
        dispatch_ref!(self, d => d.demand(), None)
    }

    fn repair_count(&self) -> usize {
        dispatch_ref!(self, d => d.repair_count(), 0)
    }

    #[cfg_attr(not(any(feature = "wav", feature = "flac")), allow(unused_variables))]
    fn repair(&self, index: usize) -> Option<qy_core::engine::Repair> {
        dispatch_ref!(self, d => d.repair(index), None)
    }
}

impl Decoder for AutoDecoder {
    fn reset(&mut self) {
        self.codec = Codec::Unknown;
        self.tag_left = 0;
        self.tag_bytes = 0;
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

    #[cfg_attr(not(any(feature = "wav", feature = "flac")), allow(unused_variables))]
    fn decode(&mut self, input: &[u8], eof: bool) -> Result<Progress, DecodeError> {
        let mut skip = 0;
        if self.codec == Codec::Unknown {
            match self.identify(input, eof) {
                Ok(pos) => skip = pos,
                Err(r) => return r,
            }
        }
        // 识别所在的调用里，标签字节计入消耗量；容器解码器只看到标签之后的输入。
        let input = &input[skip..];
        dispatch!(
            self,
            d => d.decode(input, eof).map(|p| Progress {
                consumed: p.consumed + skip,
                ..p
            }),
            Err(DecodeError::Unsupported)
        )
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
    use std::string::ToString;
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
        // ID3v2 标签头：先等足 10 字节，再按声明长度跳过（完整用例见 tests/id3.rs）。
        assert_eq!(
            d.decode(b"ID3\x04....", false).map(|p| p.status),
            Ok(Status::NeedInput)
        );
        assert_eq!(d.codec(), Codec::Unknown);
        assert_eq!(
            d.decode(b"ID3\x04\0\0\0\0\0\x02..OggS", false),
            Err(DecodeError::Unsupported)
        );
        assert_eq!(d.codec(), Codec::Unrecognized);
        assert_eq!(d.skipped_tag_bytes(), 12);
        let hint = d.unrecognized().map(|u| u.to_string()).unwrap_or_default();
        assert!(
            hint.contains("12 字节的 ID3v2")
                && hint.contains("4F 67 67 53")
                && hint.contains("Ogg"),
            "{hint}"
        );
        d.reset();
        assert_eq!(d.unrecognized(), None);
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
