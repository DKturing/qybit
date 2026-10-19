//! 按文件头选择自有解码器（M1 T3；M3 T1 起各格式可按需链接）。
//!
//! `#![no_std]`、无 alloc、静态分发：持有 WAV、AIFF（F2）与/或 FLAC 解码器；每首曲目开始时
//! （`reset` 之后）读取文件头：`RIFF`/`RIFX`/`RF64`/`BW64` 交给 WAV 解码器（后者再区分支持与否），
//! `FORM` + 形式类型 `AIFF`/`AIFC`（共 12 字节）交给 AIFF 解码器，`fLaC` 交给 FLAC 解码器，其他返回
//! `Unsupported`。不依据文件扩展名。
//!
//! 工作区：FLAC 使用独立的工作区；WAV 与 AIFF 都是流式 PCM 容器，共用一块 PCM 工作区，同一时刻只由
//! 其中一个解码器持有。识别出另一种容器时把工作区移交给它（所有权转移，不复制、不重新分配，
//! 不使用 `unsafe`），因此启用 AIFF 不增加 RAM。
//!
//! 文件开头的 ID3v2 标签（常见于被标签软件写过的 FLAC）先按标签头声明的长度跳过，再识别其后的
//! 容器。依据：ID3v2.4.0 结构规范 §3.1（10 字节标签头：`ID3`、主/次版本各 1 字节且不为 `FF`、
//! 标志 1 字节、4 字节 syncsafe 长度，每字节最高位为 0；长度不含标签头，v2.4 标志位 4 置位时
//! 另有 10 字节标签尾）；参考解码器 libFLAC（`stream_decoder.c` 的 `skip_id3v2_tag_`）与
//! ffmpeg 的格式探测同样在识别前跳过 ID3v2。可连续多个标签；标签头不合规则时按无法识别拒绝。
//! 跳过的是元数据，不是音频，样本不受影响；跳过的字节数见 [`AutoDecoder::skipped_tag_bytes`]。
//!
//! 特性 `wav`、`flac`、`aiff`（默认都开）决定链接哪些解码器；只开 `wav` 时不链接 FLAC 与 AIFF 代码。
//! 已链接但宿主未提供工作区的格式，与未链接的格式一样按 `Unsupported` 拒绝（[`Codec::Disabled`]）。
//! 各配置档的资源预算见 [`profile`]。
#![no_std]

pub mod profile;

#[cfg(feature = "aiff")]
use qy_codec_aiff::{AiffDecoder, AiffDetail};
#[cfg(feature = "flac")]
use qy_codec_flac::{Capacity, FlacDecoder, FlacError, InitError};
#[cfg(feature = "wav")]
use qy_codec_wav::WavDecoder;
#[cfg(feature = "wav")]
use qy_codec_wav::WavDetail;
use qy_core::{
    DecodeError, Decoder, INPUT_WINDOW_BYTES, Identify, PCM_WORK_SAMPLES, Progress, SeekError,
    SourceFormat, Status, magic_matches,
};

/// 当前曲目使用的解码器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// 尚未读到足够的文件头。
    Unknown,
    Wav,
    Flac,
    /// AIFF / AIFF-C（F2）。
    Aiff,
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
    /// 流式 PCM 工作区不足 [`PCM_WORK_SAMPLES`]。
    Stream(DecodeError),
    /// 启用 AIFF 之前没有提供流式 PCM 工作区（[`AutoDecoder::with_wav`] 或
    /// [`AutoDecoder::with_stream_pcm`]）。
    NoStreamBuffer,
}

/// 流式 PCM 容器（WAV、AIFF）共用的工作区，同一时刻只由一个解码器持有。
///
/// F4 之后 WAV 解码器（含 ds64 块大小表）比 AIFF 大约 300 字节。`no_std`、无 alloc，不能装箱；
/// 本枚举在 `AutoDecoder` 中只有一份，随宿主一次性取得的存储存放，大小差异只是固定的几百字节。
#[allow(clippy::large_enum_variant)]
enum Stream {
    /// 未提供工作区。
    None,
    /// 工作区空闲。
    Idle(&'static mut [i32]),
    #[cfg(feature = "wav")]
    Wav(WavDecoder),
    #[cfg(feature = "aiff")]
    Aiff(AiffDecoder),
}

impl Stream {
    /// 取回工作区（持有它的解码器随之释放）。
    #[cfg(any(feature = "wav", feature = "aiff"))]
    fn take_pcm(&mut self) -> Option<&'static mut [i32]> {
        match core::mem::replace(self, Stream::None) {
            Stream::None => None,
            Stream::Idle(p) => Some(p),
            #[cfg(feature = "wav")]
            Stream::Wav(d) => Some(d.into_pcm()),
            #[cfg(feature = "aiff")]
            Stream::Aiff(d) => Some(d.into_pcm()),
        }
    }

    fn capacity(&self) -> usize {
        match self {
            Stream::None => 0,
            Stream::Idle(p) => p.len(),
            #[cfg(feature = "wav")]
            Stream::Wav(d) => d.capacity_samples(),
            #[cfg(feature = "aiff")]
            Stream::Aiff(d) => d.capacity_samples(),
        }
    }

    fn reset(&mut self) {
        match self {
            #[cfg(feature = "wav")]
            Stream::Wav(d) => d.reset(),
            #[cfg(feature = "aiff")]
            Stream::Aiff(d) => d.reset(),
            _ => {}
        }
    }
}

pub struct AutoDecoder {
    stream: Stream,
    /// 启用了 WAV（需要 `wav` 特性与流式工作区）。
    wav_on: bool,
    /// 启用了 AIFF（需要 `aiff` 特性与流式工作区）。
    aiff_on: bool,
    #[cfg(feature = "flac")]
    flac: Option<FlacDecoder>,
    codec: Codec,
    /// `FORM` 文件头的形式类型（无法识别时报告用）。
    form_type: Option<[u8; 4]>,
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

/// 组合识别（E3，[`AutoDecoder::identify`]）的拒绝明细。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoDetail {
    #[cfg(feature = "wav")]
    Wav(WavDetail),
    #[cfg(feature = "aiff")]
    Aiff(AiffDetail),
    #[cfg(feature = "flac")]
    Flac(FlacError),
    /// 可识别的容器（`"WAV"`、`"AIFF"`、`"FLAC"`），但本构建未链接或宿主未启用其解码器。
    Disabled(&'static str),
}

/// 各容器的文件头标记（与 [`AutoDecoder`] 解码时的选择相同；未链接的格式也要能说出名字）。
const WAV_MAGICS: [&[u8]; 4] = [b"RIFF", b"RIFX", b"RF64", b"BW64"];
const FLAC_MAGIC: [&[u8]; 1] = [b"fLaC"];

/// 无法识别的文件头（报告用）：前 4 字节与按常见格式标记推断的类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnrecognizedHeader {
    /// 文件头前 4 字节；跳过了 ID3v2 标签时为标签之后的 4 字节。
    pub magic: [u8; 4],
    /// 之前跳过的 ID3v2 标签字节数。
    pub skipped_tag: u64,
    /// `FORM` / `RIFF` 文件头的形式类型（不是 `AIFF`/`AIFC`、`WAVE` 的其他 IFF / RIFF 格式）。
    pub form_type: Option<[u8; 4]>,
}

impl core::fmt::Display for UnrecognizedHeader {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let m = self.magic;
        let guess = match &m {
            [b'I', b'D', b'3', _] => "ID3v2 标签头不合规则（版本或 syncsafe 长度字节无效）",
            b"OggS" => "Ogg 容器（Ogg FLAC、Vorbis、Opus 等），不支持",
            b"FORM" => "IFF 容器，但形式类型不是 AIFF / AIFF-C",
            b"RIFF" | b"RIFX" | b"RF64" | b"BW64" => "RIFF 容器，但形式类型不是 WAVE",
            b"wvpk" => "WavPack，在格式路线 F5 中，尚未实现",
            b"MAC " => "Monkey's Audio（APE），在格式路线 F6 中，尚未实现",
            b"DSD " | b"FRM8" => "DSD（DSF/DFF），尚未实现",
            [_, _, _, _] if m[0] == 0xFF && m[1] & 0xE0 == 0xE0 => {
                "MPEG 音频帧（MP3 等有损格式），不在本程序范围内"
            }
            _ => "不是 RIFF/WAVE、AIFF 或 FLAC",
        };
        if self.skipped_tag > 0 {
            write!(f, "跳过开头 {} 字节的 ID3v2 标签后，", self.skipped_tag)?;
        }
        write!(
            f,
            "文件头前 4 字节为 {:02X} {:02X} {:02X} {:02X}：{guess}",
            m[0], m[1], m[2], m[3]
        )?;
        if let Some(t) = self.form_type {
            write!(
                f,
                "（形式类型 {:02X} {:02X} {:02X} {:02X}）",
                t[0], t[1], t[2], t[3]
            )?;
        }
        Ok(())
    }
}

/// 未启用 AIFF 解码器时按 `FORM` + 形式类型认出名字。
fn aiff_by_magic(head: &[u8], eof: bool, disabled: Identify<AutoDetail>) -> Identify<AutoDetail> {
    match magic_matches(head, &[b"FORM"], eof) {
        None => Identify::NeedMore {
            total: head.len() as u64 + 1,
            container: false,
        },
        Some(false) => Identify::Foreign,
        Some(true) if head.len() < 12 && !eof => Identify::NeedMore {
            total: 12,
            container: false,
        },
        Some(true) if head.len() < 12 => Identify::Rejected {
            error: DecodeError::Truncated,
            detail: None,
        },
        Some(true) if head.len() >= 12 && (&head[8..12] == b"AIFF" || &head[8..12] == b"AIFC") => {
            disabled
        }
        Some(true) => Identify::Foreign,
    }
}

impl Default for AutoDecoder {
    fn default() -> Self {
        Self::empty()
    }
}

impl AutoDecoder {
    /// 不含任何解码器；用 [`AutoDecoder::with_wav`] / [`AutoDecoder::with_aiff`] /
    /// [`AutoDecoder::with_flac`] 加入。
    pub const fn empty() -> Self {
        Self {
            stream: Stream::None,
            wav_on: false,
            aiff_on: false,
            #[cfg(feature = "flac")]
            flac: None,
            codec: Codec::Unknown,
            form_type: None,
            magic: [0; 4],
            tag_left: 0,
            tag_bytes: 0,
        }
    }

    /// 启用全部已链接的解码器：`wav_pcm` 为 WAV 与 AIFF 共用的流式 PCM 工作区；FLAC 工作区与 PCM
    /// 按 `flac_cap` 检查。
    #[cfg(all(feature = "wav", feature = "flac"))]
    pub fn new(
        wav_pcm: &'static mut [i32],
        flac_cap: Capacity,
        flac_workspace: &'static mut [i64],
        flac_pcm: &'static mut [i32],
    ) -> Result<Self, AutoInitError> {
        let d = Self::empty().with_wav(wav_pcm)?;
        #[cfg(feature = "aiff")]
        let d = d.with_aiff()?;
        d.with_flac(flac_cap, flac_workspace, flac_pcm)
    }

    /// 提供流式 PCM 工作区（不少于 [`PCM_WORK_SAMPLES`]），由 WAV 与 AIFF 共用。
    pub fn with_stream_pcm(mut self, pcm: &'static mut [i32]) -> Result<Self, AutoInitError> {
        if pcm.len() < PCM_WORK_SAMPLES {
            return Err(AutoInitError::Stream(DecodeError::CapacityExceeded));
        }
        self.stream = Stream::Idle(pcm);
        Ok(self)
    }

    /// 启用 WAV 解码器；`pcm` 为流式 PCM 工作区（不少于 `PCM_WORK_SAMPLES`，与 AIFF 共用）。
    #[cfg(feature = "wav")]
    pub fn with_wav(mut self, pcm: &'static mut [i32]) -> Result<Self, AutoInitError> {
        self.stream = Stream::Wav(WavDecoder::new(pcm).map_err(AutoInitError::Wav)?);
        self.wav_on = true;
        Ok(self)
    }

    /// 启用 AIFF 解码器，使用已提供的流式 PCM 工作区（与 WAV 共用，不另占 RAM）。
    #[cfg(feature = "aiff")]
    pub fn with_aiff(mut self) -> Result<Self, AutoInitError> {
        if matches!(self.stream, Stream::None) {
            return Err(AutoInitError::NoStreamBuffer);
        }
        self.aiff_on = true;
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
        match (self.codec, &self.stream) {
            (Codec::Wav, Stream::Wav(w)) => w.last_detail(),
            _ => None,
        }
    }

    /// AIFF 拒绝明细（当前曲目为 AIFF 且有明细时）。
    #[cfg(feature = "aiff")]
    pub fn aiff_detail(&self) -> Option<AiffDetail> {
        match (self.codec, &self.stream) {
            (Codec::Aiff, Stream::Aiff(a)) => a.last_detail(),
            _ => None,
        }
    }

    /// 当前曲目文件头声明的总帧数（不可信，只用于显示时长）；未知时为 `None`。
    pub fn declared_frames(&self) -> Option<u64> {
        match (self.codec, &self.stream) {
            #[cfg(feature = "wav")]
            (Codec::Wav, Stream::Wav(w)) => w.declared_frames(),
            #[cfg(feature = "aiff")]
            (Codec::Aiff, Stream::Aiff(a)) => a.declared_frames(),
            #[cfg(feature = "flac")]
            (Codec::Flac, _) => self.flac.as_ref().and_then(FlacDecoder::declared_frames),
            _ => None,
        }
    }

    /// 流式 PCM 工作区（WAV / AIFF 共用）的容量，样本数；未提供时为 0。
    pub fn stream_capacity(&self) -> usize {
        self.stream.capacity()
    }

    #[cfg(feature = "flac")]
    pub fn flac(&self) -> Option<&FlacDecoder> {
        self.flac.as_ref()
    }

    /// 当前持有流式工作区的 WAV 解码器（最近一首流式曲目为 AIFF 时为 `None`）。
    #[cfg(feature = "wav")]
    pub fn wav(&self) -> Option<&WavDecoder> {
        match &self.stream {
            Stream::Wav(d) => Some(d),
            _ => None,
        }
    }

    /// 当前持有流式工作区的 AIFF 解码器。
    #[cfg(feature = "aiff")]
    pub fn aiff(&self) -> Option<&AiffDecoder> {
        match &self.stream {
            Stream::Aiff(d) => Some(d),
            _ => None,
        }
    }

    /// 当前曲目的文件头无法识别时，给出前 4 字节与推断的类型。
    pub fn unrecognized(&self) -> Option<UnrecognizedHeader> {
        (self.codec == Codec::Unrecognized).then_some(UnrecognizedHeader {
            magic: self.magic,
            skipped_tag: self.tag_bytes,
            form_type: self.form_type,
        })
    }

    /// 当前曲目是可识别的容器，但本构建未链接或宿主未启用其解码器时，给出格式名
    /// （`"WAV"`、`"AIFF"`、`"FLAC"`），供宿主说明拒绝原因（B1）。
    pub fn disabled(&self) -> Option<&'static str> {
        if self.codec != Codec::Disabled {
            return None;
        }
        Some(match &self.magic {
            b"fLaC" => "FLAC",
            b"FORM" => "AIFF",
            _ => "WAV",
        })
    }

    /// 当前曲目开头跳过的 ID3v2 标签字节数（没有标签时为 0）。
    pub fn skipped_tag_bytes(&self) -> u64 {
        self.tag_bytes
    }

    /// 识别前的处理：跳过 ID3v2 标签并读取容器标记。`Ok(pos)` 表示已识别，容器从 `input[pos..]`
    /// 开始；`Err` 为本次调用的结果（缺输入或错误）。
    fn locate(&mut self, input: &[u8], eof: bool) -> Result<usize, Result<Progress, DecodeError>> {
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
            // IFF：形式类型在第 8–11 字节，读到 12 字节才能区分 AIFF / AIFF-C 与其他 IFF 格式。
            // RIFF 家族的形式类型同样在第 8–11 字节（`WAVE` 之外的 RIFF 格式如 AVI 不是 WAV）；
            // 与 [`AutoDecoder::identify`] 的判断相同。
            let need = if tag {
                ID3_HEADER
            } else if rest.starts_with(b"FORM") || WAV_MAGICS.iter().any(|m| rest.starts_with(m)) {
                12
            } else {
                4
            };
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
            self.codec = self.sniff(&rest[..need]);
            return Ok(pos);
        }
    }

    fn sniff(&mut self, head: &[u8]) -> Codec {
        match &head[..4] {
            b"RIFF" | b"RIFX" | b"RF64" | b"BW64" => {
                if &head[8..12] != b"WAVE" {
                    self.form_type = Some([head[8], head[9], head[10], head[11]]);
                    return Codec::Unrecognized;
                }
                if cfg!(feature = "wav") && self.wav_on {
                    return Codec::Wav;
                }
                Codec::Disabled
            }
            b"FORM" => match &head[8..12] {
                b"AIFF" | b"AIFC" if cfg!(feature = "aiff") && self.aiff_on => Codec::Aiff,
                b"AIFF" | b"AIFC" => Codec::Disabled,
                t => {
                    self.form_type = Some([t[0], t[1], t[2], t[3]]);
                    Codec::Unrecognized
                }
            },
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

    /// 识别文件头（E3）：不消耗输入、不改变解码器状态，回答哪个官方解码器会接手、它是否支持。
    /// 与 [`Decoder::decode`] 的选择规则相同（不依据扩展名）；各格式的判断就是各解码器自己的
    /// `identify`（与完整解码同一个状态机）。
    ///
    /// 文件开头的 ID3v2 标签给出 [`Identify::Skip`]：插件的路由层跳过标签后，用其后的字节再识别
    /// （交给本解码器时，本解码器从标签之后开始也能解码；从文件开头开始则自己跳过标签）。
    /// 不是任何已知容器时为 [`Identify::Foreign`]，`Codec::Unrecognized`；识别出容器但本构建未
    /// 启用其解码器时为 `Rejected`（`Unsupported`，[`AutoDetail::Disabled`]），`Codec::Disabled`。
    pub fn identify(&self, head: &[u8], eof: bool) -> (Codec, Identify<AutoDetail>) {
        // ID3v2 标签（ID3v2.4.0 结构规范 §3.1），与 `decode` 中的跳过规则相同。
        if head.starts_with(b"ID3") {
            if head.len() < ID3_HEADER {
                if !eof {
                    return (
                        Codec::Unknown,
                        Identify::NeedMore {
                            total: ID3_HEADER as u64,
                            container: false,
                        },
                    );
                }
                return (
                    Codec::Unknown,
                    Identify::Rejected {
                        error: DecodeError::Truncated,
                        detail: None,
                    },
                );
            }
            let mut h = [0u8; ID3_HEADER];
            h.copy_from_slice(&head[..ID3_HEADER]);
            if let Some(len) = id3v2_len(&h) {
                if eof && len as usize > head.len() {
                    // 标签声明的长度超出文件（与 `decode` 相同：不是完整的文件）。
                    return (
                        Codec::Unknown,
                        Identify::Rejected {
                            error: DecodeError::Truncated,
                            detail: None,
                        },
                    );
                }
                return (
                    Codec::Unknown,
                    Identify::Skip {
                        bytes: u64::from(len),
                    },
                );
            }
            // 标签头不合规则：按普通文件头继续（与 `decode` 相同，结果为无法识别）。
        }
        let mut need: Option<u64> = None;
        let mut take =
            |codec: Codec, r: Identify<AutoDetail>| -> Option<(Codec, Identify<AutoDetail>)> {
                match r {
                    Identify::Foreign => None,
                    Identify::NeedMore {
                        total,
                        container: false,
                    } => {
                        need = Some(need.map_or(total, |n| n.min(total)));
                        None
                    }
                    r => Some((codec, r)),
                }
            };
        let disabled = |name: &'static str| Identify::Rejected {
            error: DecodeError::Unsupported,
            detail: Some(AutoDetail::Disabled(name)),
        };
        // 未启用的格式只按标记认出名字。
        let by_magic = |magics: &[&[u8]], name: &'static str| match magic_matches(head, magics, eof)
        {
            None => Identify::NeedMore {
                total: 4,
                container: false,
            },
            Some(true) => disabled(name),
            Some(false) => Identify::Foreign,
        };
        let wav = {
            #[cfg(feature = "wav")]
            if self.wav_on {
                qy_codec_wav::identify(head, eof).map_detail(AutoDetail::Wav)
            } else {
                by_magic(&WAV_MAGICS, "WAV")
            }
            #[cfg(not(feature = "wav"))]
            by_magic(&WAV_MAGICS, "WAV")
        };
        let wav_codec = if matches!(
            wav,
            Identify::Rejected {
                detail: Some(AutoDetail::Disabled(_)),
                ..
            }
        ) {
            Codec::Disabled
        } else {
            Codec::Wav
        };
        if let Some(r) = take(wav_codec, wav) {
            return r;
        }
        let aiff = {
            #[cfg(feature = "aiff")]
            if self.aiff_on {
                qy_codec_aiff::identify(head, eof).map_detail(AutoDetail::Aiff)
            } else {
                aiff_by_magic(head, eof, disabled("AIFF"))
            }
            #[cfg(not(feature = "aiff"))]
            aiff_by_magic(head, eof, disabled("AIFF"))
        };
        let aiff_codec = if matches!(
            aiff,
            Identify::Rejected {
                detail: Some(AutoDetail::Disabled(_)),
                ..
            }
        ) {
            Codec::Disabled
        } else {
            Codec::Aiff
        };
        if let Some(r) = take(aiff_codec, aiff) {
            return r;
        }
        let flac = {
            #[cfg(feature = "flac")]
            if self.flac.is_some() {
                qy_codec_flac::identify(head, eof).map_detail(AutoDetail::Flac)
            } else {
                by_magic(&FLAC_MAGIC, "FLAC")
            }
            #[cfg(not(feature = "flac"))]
            by_magic(&FLAC_MAGIC, "FLAC")
        };
        let flac_codec = if matches!(
            flac,
            Identify::Rejected {
                detail: Some(AutoDetail::Disabled(_)),
                ..
            }
        ) {
            Codec::Disabled
        } else {
            Codec::Flac
        };
        if let Some(r) = take(flac_codec, flac) {
            return r;
        }
        // `ID3` 标签头的前缀也要等。
        if !eof && head.len() < 3 && b"ID3".starts_with(head) {
            let t = head.len() as u64 + 1;
            need = Some(need.map_or(t, |n| n.min(t)));
        }
        match need {
            Some(total) => (
                Codec::Unknown,
                Identify::NeedMore {
                    total,
                    container: false,
                },
            ),
            None => (Codec::Unrecognized, Identify::Foreign),
        }
    }

    /// 让当前曲目的流式解码器持有共用工作区：识别出的容器与上一首不同时移交工作区。
    fn activate(&mut self) -> Result<(), DecodeError> {
        match self.codec {
            #[cfg(feature = "wav")]
            Codec::Wav if !matches!(self.stream, Stream::Wav(_)) => {
                let pcm = self.stream.take_pcm().ok_or(DecodeError::Unsupported)?;
                // 工作区已在启用时核对过容量，这里不会失败。
                self.stream = Stream::Wav(WavDecoder::new(pcm)?);
            }
            #[cfg(feature = "aiff")]
            Codec::Aiff if !matches!(self.stream, Stream::Aiff(_)) => {
                let pcm = self.stream.take_pcm().ok_or(DecodeError::Unsupported)?;
                self.stream = Stream::Aiff(AiffDecoder::new(pcm)?);
            }
            _ => {}
        }
        Ok(())
    }
}

/// 按当前曲目的解码器静态分发；未选中或未启用时求值 `$none`。
macro_rules! dispatch {
    ($self:ident, $d:ident => $e:expr, $none:expr) => {
        match $self.codec {
            #[cfg(feature = "wav")]
            Codec::Wav => match &mut $self.stream {
                Stream::Wav($d) => $e,
                _ => $none,
            },
            #[cfg(feature = "aiff")]
            Codec::Aiff => match &mut $self.stream {
                Stream::Aiff($d) => $e,
                _ => $none,
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
            Codec::Wav => match &$self.stream {
                Stream::Wav($d) => $e,
                _ => $none,
            },
            #[cfg(feature = "aiff")]
            Codec::Aiff => match &$self.stream {
                Stream::Aiff($d) => $e,
                _ => $none,
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

    #[cfg_attr(
        not(any(feature = "wav", feature = "flac", feature = "aiff")),
        allow(unused_variables)
    )]
    fn repair(&self, index: usize) -> Option<qy_core::engine::Repair> {
        dispatch_ref!(self, d => d.repair(index), None)
    }
}

impl Decoder for AutoDecoder {
    fn reset(&mut self) {
        self.codec = Codec::Unknown;
        self.form_type = None;
        self.tag_left = 0;
        self.tag_bytes = 0;
        self.stream.reset();
        #[cfg(feature = "flac")]
        if let Some(d) = &mut self.flac {
            d.reset();
        }
    }

    fn format(&self) -> Option<SourceFormat> {
        dispatch_ref!(self, d => d.format(), None)
    }

    #[cfg_attr(
        not(any(feature = "wav", feature = "flac", feature = "aiff")),
        allow(unused_variables)
    )]
    fn decode(&mut self, input: &[u8], eof: bool) -> Result<Progress, DecodeError> {
        let mut skip = 0;
        if self.codec == Codec::Unknown {
            match self.locate(input, eof) {
                Ok(pos) => skip = pos,
                Err(r) => return r,
            }
            self.activate()?;
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

    #[cfg_attr(
        not(any(feature = "wav", feature = "flac", feature = "aiff")),
        allow(unused_variables)
    )]
    fn release(&mut self, frames: usize) {
        dispatch!(self, d => d.release(frames), ())
    }

    /// 容器解码器给出的位置从容器开头算起；文件开头跳过的 ID3v2 标签在此加回。
    #[cfg_attr(
        not(any(feature = "wav", feature = "flac", feature = "aiff")),
        allow(unused_variables)
    )]
    fn seek(&mut self, frame: u64) -> Result<u64, SeekError> {
        let tag = self.tag_bytes;
        dispatch!(
            self,
            d => d.seek(frame).and_then(|o| o.checked_add(tag).ok_or(SeekError::PastEnd)),
            Err(SeekError::Unsupported)
        )
    }

    #[cfg_attr(
        not(any(feature = "wav", feature = "flac", feature = "aiff")),
        allow(unused_variables)
    )]
    fn seek_request(&mut self) -> Option<u64> {
        let tag = self.tag_bytes;
        dispatch!(self, d => d.seek_request().and_then(|o| o.checked_add(tag)), None)
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
