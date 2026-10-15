//! QyBit 自有推送式 AIFF / AIFF-C（未压缩）解码器（F2，tasks/F2.md）。
//!
//! `#![no_std]`、无 alloc；PCM 工作区由宿主以 `&'static mut [i32]` 提供。解码器不缓存输入：每次
//! 调用只看宿主传入的切片，返回实际消耗字节数，未消耗的字节由宿主保留到下次调用（AGENTS §2）。
//! 任一结构所需的最大连续输入为 278 字节（AIFF-C 的 COMM：22 + 最长 256 字节的 pstring）或一帧，
//! 小于核心最小输入窗口，因此任意输入分块都能推进。
//!
//! 规范（版本与校验值见 docs/specs.md）：
//! - Apple《Audio Interchange File Format: "AIFF"》1.3（下称 AIFF）；
//! - Apple《AIFF-C》草案 1991-08-26（下称 AIFF-C）。
//!
//! 原则：Core 最严格，只接受规范定义的内容；拒绝分三类（[`Category`]），各有独立的明细项
//! （[`AiffDetail`]），以后加支持时只改对应的一项：
//! - **不合规**：违反规范的“必须”或结构越界（[`DecodeError::Malformed`]），源在完整结构前结束
//!   （[`DecodeError::Truncated`]）；
//! - **合规但尚不支持**（[`DecodeError::Unsupported`]）：例如 SSND 在 COMM 之前、非整数采样率、
//!   规范定义的压缩类型、规范中有歧义的多声道约定；
//! - **规范之外的扩展**（[`DecodeError::Unsupported`]）：AIFF-C 原文未定义的 `compressionType`，
//!   如 `sowt`、`twos`、`raw `、`fl32`。
//!
//! 输出：交错右对齐 `i32`，N = `sampleSize`；不经浮点，不改变样本。
#![no_std]

use qy_core::{
    ChannelLayout, DecodeError, Decoder, PCM_WORK_SAMPLES, Progress, SeekError, SourceFormat,
    Status,
};

/// AIFF-C FVER 的 `timestamp`：AIFCVersion1（AIFF-C §Format Version Chunk）。
pub const AIFC_VERSION_1: u32 = 0xA280_5140;
/// AIFF 的 COMM `ckSize` 恒为 18（AIFF §Common Chunk）。
const COMM_AIFF_LEN: u32 = 18;
/// AIFF-C 的 COMM `ckDataSize` 为 22 + pstring 的字节数（AIFF-C §Common Chunk）。
const COMM_AIFC_BASE: u32 = 22;
/// pstring 最长 1 字节计数 + 255 字节文本（总长为偶数）。
const PSTRING_MAX: u32 = 256;
/// IFF 的长度字段是 `long`（有符号 32 位）：超过此值即为负数。
const LONG_MAX: u32 = 0x7FFF_FFFF;

/// 形式类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    Aiff,
    Aifc,
}

/// 拒绝的类别（报告用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// 不符合规范（结构违反规范的“必须”、越界或文件不完整）。
    NotCompliant,
    /// 符合规范，但本程序尚不支持。
    NotYetSupported,
    /// 规范之外的扩展。
    Extension,
}

/// 不合规的具体情形。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Violation {
    /// FORM 的长度小于 4 或超出 `long` 的范围。
    FormSize { size: u32 },
    /// chunk 长度超出 `long` 的范围（为负数）。
    ChunkSize { id: [u8; 4], size: u32 },
    /// chunk（含奇数长度后的填充字节）越出 FORM 声明的范围。
    ChunkBeyondForm { id: [u8; 4] },
    /// FORM 末尾剩下不足一个 chunk 头的字节。
    StrayBytes { bytes: u32 },
    /// COMM 的长度与规范规定的不符（AIFF 为 18；AIFF-C 为 22 + pstring）。
    CommSize { size: u32, expected: u32 },
    /// FVER 的长度不是 4。
    FverSize { size: u32 },
    /// 规定“有且只有一个”或“最多一个”的 chunk 重复出现。
    DuplicateChunk { id: [u8; 4] },
    /// 必需的 chunk 缺失。
    MissingChunk { id: [u8; 4] },
    /// `numChannels` 小于 1。
    Channels { value: i16 },
    /// `sampleSize` 不在 1–32 之内。
    SampleSize { value: i16 },
    /// SSND 的长度装不下 offset、blockSize 与 COMM 声明的全部样本帧。
    SsndTooShort { size: u32, needed: u64 },
}

/// 拒绝的明细（宿主报告用；`DecodeError` 只给大类）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiffDetail {
    /// 不合规。
    NotCompliant(Violation),
    /// 源在 SSND 的样本帧结束前结束：已输出的完整帧之外还缺 `missing` 字节。
    DataShort { frames: u32, missing: u64 },
    /// 源在 FORM 声明的范围结束前结束（样本已全部输出时同样报告）。
    FormShort { missing: u64 },
    /// SSND 在 COMM 之前：规范允许（chunk 顺序没有限制），按顺序读取时还不知道格式，尚不支持。
    SsndBeforeComm,
    /// 采样率不是正整数或超出 32 位范围（80 位扩展精度原值）。
    SampleRate { raw: [u8; 10] },
    /// AIFF-C 的 FVER 版本不是 AIFCVersion1。
    FormatVersion { timestamp: u32 },
    /// AIFF-C 规范定义的压缩类型（`ACE2`、`ACE8`、`MAC3`、`MAC6`）。
    Compressed { tag: [u8; 4] },
    /// AIFF-C 规范之外的 `compressionType`。
    NonStandardType { tag: [u8; 4] },
    /// 规范对该声道数给出的约定有歧义（4 声道有两种约定；6 声道的“环绕”位置未定义）。
    AmbiguousLayout { channels: u16 },
    /// 规范没有给出该声道数的扬声器约定。
    UndefinedLayout { channels: u16 },
}

impl AiffDetail {
    pub const fn category(&self) -> Category {
        match self {
            Self::NotCompliant(_) | Self::DataShort { .. } | Self::FormShort { .. } => {
                Category::NotCompliant
            }
            Self::NonStandardType { .. } => Category::Extension,
            _ => Category::NotYetSupported,
        }
    }
}

/// 4 字节标识的显示：可打印 ASCII 原样，其余为十六进制。
struct Id([u8; 4]);

impl core::fmt::Display for Id {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0.iter().all(|b| (0x20..0x7F).contains(b)) {
            f.write_str("'")?;
            for &b in &self.0 {
                write!(f, "{}", char::from(b))?;
            }
            f.write_str("'")
        } else {
            let [a, b, c, d] = self.0;
            write!(f, "{a:02X} {b:02X} {c:02X} {d:02X}")
        }
    }
}

impl core::fmt::Display for Violation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Self::FormSize { size } => write!(f, "FORM 长度 {size} 无效"),
            Self::ChunkSize { id, size } => {
                write!(f, "chunk {} 的长度 {size} 超出 long 的范围", Id(id))
            }
            Self::ChunkBeyondForm { id } => write!(f, "chunk {} 越出 FORM 的范围", Id(id)),
            Self::StrayBytes { bytes } => {
                write!(f, "FORM 末尾有 {bytes} 字节不足一个 chunk 头")
            }
            Self::CommSize { size, expected } => {
                write!(f, "COMM 长度为 {size}，规范规定为 {expected}")
            }
            Self::FverSize { size } => write!(f, "FVER 长度为 {size}，规范规定为 4"),
            Self::DuplicateChunk { id } => write!(f, "chunk {} 重复出现", Id(id)),
            Self::MissingChunk { id } => write!(f, "缺少必需的 chunk {}", Id(id)),
            Self::Channels { value } => write!(f, "numChannels 为 {value}，至少为 1"),
            Self::SampleSize { value } => write!(f, "sampleSize 为 {value}，应在 1–32 之内"),
            Self::SsndTooShort { size, needed } => write!(
                f,
                "SSND 长度 {size} 装不下 COMM 声明的全部样本帧（需要 {needed} 字节）"
            ),
        }
    }
}

impl core::fmt::Display for AiffDetail {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let prefix = match self.category() {
            Category::NotCompliant => "不符合 AIFF 规范",
            Category::NotYetSupported => "符合规范但本程序尚不支持",
            Category::Extension => "AIFF-C 规范之外的扩展，本程序不支持",
        };
        f.write_str(prefix)?;
        f.write_str("：")?;
        match *self {
            Self::NotCompliant(v) => write!(f, "{v}"),
            Self::DataShort { frames, missing } => write!(
                f,
                "文件在 COMM 声明的 {frames} 帧结束前结束，缺少 {missing} 字节（已播放全部完整帧）"
            ),
            Self::FormShort { missing } => write!(f, "文件比 FORM 声明的短 {missing} 字节"),
            Self::SsndBeforeComm => f.write_str(
                "SSND（样本数据）在 COMM（格式）之前；规范允许这种顺序，按顺序读取时尚无法得知格式",
            ),
            Self::SampleRate { raw } => {
                f.write_str("采样率不是正整数或超出 32 位范围（80 位扩展精度原值")?;
                for b in raw {
                    write!(f, " {b:02X}")?;
                }
                f.write_str("）")
            }
            Self::FormatVersion { timestamp } => write!(
                f,
                "AIFF-C 版本 {timestamp:#010X}，只支持 AIFCVersion1（{AIFC_VERSION_1:#010X}）"
            ),
            Self::Compressed { tag } => write!(f, "压缩类型 {}（不在本程序范围内）", Id(tag)),
            Self::NonStandardType { tag } => write!(f, "compressionType {}", Id(tag)),
            Self::AmbiguousLayout { channels } => write!(
                f,
                "规范对 {channels} 声道的扬声器约定有歧义，无法确定声道位置"
            ),
            Self::UndefinedLayout { channels } => {
                write!(f, "规范没有 {channels} 声道的扬声器约定")
            }
        }
    }
}

/// 80 位 IEEE 754 扩展精度（SANE Extended）精确换算为正整数；不是正整数或超出 `u32` 时为 `None`。
/// 不四舍五入。格式：1 位符号、15 位指数（偏置 16383）、64 位含显式整数位的尾数。
pub fn extended_to_u32(b: [u8; 10]) -> Option<u32> {
    let se = u16::from_be_bytes([b[0], b[1]]);
    if se & 0x8000 != 0 {
        return None;
    }
    let exp = i32::from(se & 0x7FFF);
    if exp == 0x7FFF {
        return None;
    }
    let m = u64::from_be_bytes([b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9]]);
    if m == 0 {
        return None;
    }
    // 值 = m × 2^(exp − 16383 − 63)。
    let shift = exp - 16383 - 63;
    if shift >= 0 {
        if shift >= 32 {
            return None;
        }
        u32::try_from(u128::from(m) << shift).ok()
    } else {
        let s = shift.unsigned_abs();
        if s >= 64 || m & ((1u64 << s) - 1) != 0 {
            return None;
        }
        u32::try_from(m >> s).ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    FormHeader,
    ChunkHeader,
    Comm {
        size: u32,
        pad: u32,
    },
    Fver {
        pad: u32,
    },
    SsndHeader {
        size: u32,
        pad: u32,
    },
    /// 跳过 `remaining` 字节；`data` 为 `Some((帧数, 帧之后要跳过的字节))` 时随后进入样本数据。
    Skip {
        remaining: u64,
        data: Option<(u32, u64)>,
    },
    /// 样本数据：还有 `frames` 帧，之后跳过 `after` 字节（剩余字节与填充）。
    Data {
        frames: u32,
        after: u64,
    },
    End,
    Failed(DecodeError),
}

#[derive(Debug, Clone, Copy)]
struct Layout {
    format: SourceFormat,
    /// 每个样本点的字节数（1–4）。
    bytes: usize,
    channels: usize,
}

/// AIFF / AIFF-C 解码器。
pub struct AiffDecoder {
    pcm: &'static mut [i32],
    state: State,
    form: Form,
    /// FORM 体中尚未消耗的字节数。
    form_remaining: u32,
    layout: Option<Layout>,
    /// COMM 的 `numSampleFrames`。
    frames: u32,
    comm_seen: bool,
    ssnd_seen: bool,
    fver_seen: bool,
    detail: Option<AiffDetail>,
    /// 已消耗的总字节数（S1 定位用）。
    offset: u64,
    /// 第一个样本帧距源开头的位置（S1）；尚未到样本数据时为 `None`。
    data_start: Option<u64>,
    start: usize,
    end: usize,
}

fn be16(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b[i], b[i + 1]])
}

fn be32(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn id_at(b: &[u8], i: usize) -> [u8; 4] {
    [b[i], b[i + 1], b[i + 2], b[i + 3]]
}

impl AiffDecoder {
    /// 用宿主提供的 PCM 工作区构造；长度不足 [`PCM_WORK_SAMPLES`] 返回容量错误。
    pub fn new(pcm: &'static mut [i32]) -> Result<Self, DecodeError> {
        if pcm.len() < PCM_WORK_SAMPLES {
            return Err(DecodeError::CapacityExceeded);
        }
        Ok(Self {
            pcm,
            state: State::FormHeader,
            form: Form::Aiff,
            form_remaining: 0,
            layout: None,
            frames: 0,
            comm_seen: false,
            ssnd_seen: false,
            fver_seen: false,
            detail: None,
            offset: 0,
            data_start: None,
            start: 0,
            end: 0,
        })
    }

    /// 工作区容量（样本数）。
    pub fn capacity_samples(&self) -> usize {
        self.pcm.len()
    }

    /// 交还 PCM 工作区（所有权转移，不复制）。`qy-codec-auto` 用它让 WAV 与 AIFF 解码器共用
    /// 同一块工作区。
    pub fn into_pcm(self) -> &'static mut [i32] {
        self.pcm
    }

    /// 最近一次拒绝的明细（新曲目 `reset` 后清除）。
    pub fn last_detail(&self) -> Option<AiffDetail> {
        self.detail
    }

    /// 形式类型（FORM 头解析之后有效）。
    /// COMM 声明的总帧数（`numSampleFrames`）；尚未读到 COMM 时为 `None`。来自不可信的文件头，
    /// 只用于显示时长。
    pub fn declared_frames(&self) -> Option<u64> {
        self.comm_seen.then_some(u64::from(self.frames))
    }

    pub fn form(&self) -> Form {
        self.form
    }

    fn reject(&mut self, e: DecodeError, d: AiffDetail) -> DecodeError {
        self.detail = Some(d);
        e
    }

    fn violation(&mut self, v: Violation) -> DecodeError {
        self.reject(DecodeError::Malformed, AiffDetail::NotCompliant(v))
    }

    fn fail(&mut self, e: DecodeError) -> Result<Progress, DecodeError> {
        self.state = State::Failed(e);
        Err(e)
    }

    /// 缺输入：源已结束则为截断（FORM 未读完）。
    fn need_input(
        &mut self,
        consumed: usize,
        avail: usize,
        eof: bool,
    ) -> Result<Progress, DecodeError> {
        if eof {
            let missing = u64::from(self.form_remaining).saturating_sub(avail as u64);
            return Err(self.reject(DecodeError::Truncated, AiffDetail::FormShort { missing }));
        }
        Ok(Progress {
            consumed,
            status: Status::NeedInput,
        })
    }

    fn take_form(&mut self, n: u64) -> Result<(), DecodeError> {
        let n = u32::try_from(n).map_err(|_| DecodeError::Malformed)?;
        self.form_remaining = self
            .form_remaining
            .checked_sub(n)
            .ok_or(DecodeError::Malformed)?;
        Ok(())
    }

    fn parse_form_header(&mut self, b: &[u8]) -> Result<(), DecodeError> {
        if &b[0..4] != b"FORM" {
            return Err(DecodeError::Malformed);
        }
        self.form = match &b[8..12] {
            b"AIFF" => Form::Aiff,
            b"AIFC" => Form::Aifc,
            _ => return Err(DecodeError::Unsupported),
        };
        let size = be32(b, 4);
        if !(4..=LONG_MAX).contains(&size) {
            return Err(self.violation(Violation::FormSize { size }));
        }
        self.form_remaining = size - 4;
        Ok(())
    }

    /// FORM 读完：核对必需的 chunk（AIFF §Chunks；AIFF-C §Format Version Chunk）。
    fn finish(&mut self) -> Result<(), DecodeError> {
        if !self.comm_seen {
            return Err(self.violation(Violation::MissingChunk { id: *b"COMM" }));
        }
        if self.form == Form::Aifc && !self.fver_seen {
            return Err(self.violation(Violation::MissingChunk { id: *b"FVER" }));
        }
        if self.frames > 0 && !self.ssnd_seen {
            return Err(self.violation(Violation::MissingChunk { id: *b"SSND" }));
        }
        Ok(())
    }

    /// 解析 chunk 头（已有 8 字节）并进入对应状态。
    fn parse_chunk_header(&mut self, b: &[u8]) -> Result<(), DecodeError> {
        let id = id_at(b, 0);
        let size = be32(b, 4);
        if size > LONG_MAX {
            return Err(self.violation(Violation::ChunkSize { id, size }));
        }
        let pad = size & 1;
        // 规范：奇数长度后必须有 1 字节 0 填充（不计入长度），填充也在 FORM 之内。
        if 8 + u64::from(size) + u64::from(pad) > u64::from(self.form_remaining) {
            return Err(self.violation(Violation::ChunkBeyondForm { id }));
        }
        self.take_form(8)?;
        self.state = match &id {
            b"COMM" => {
                if self.comm_seen {
                    return Err(self.violation(Violation::DuplicateChunk { id }));
                }
                match self.form {
                    Form::Aiff if size != COMM_AIFF_LEN => {
                        return Err(self.violation(Violation::CommSize {
                            size,
                            expected: COMM_AIFF_LEN,
                        }));
                    }
                    // AIFF-C：至少要有 pstring 的计数字节；最长 22 + 256。确切长度读到计数后核对。
                    Form::Aifc
                        if !(COMM_AIFC_BASE + 2..=COMM_AIFC_BASE + PSTRING_MAX).contains(&size) =>
                    {
                        return Err(self.violation(Violation::CommSize {
                            size,
                            expected: COMM_AIFC_BASE + 2,
                        }));
                    }
                    _ => {}
                }
                State::Comm { size, pad }
            }
            b"SSND" => {
                if !self.comm_seen {
                    return Err(self.reject(DecodeError::Unsupported, AiffDetail::SsndBeforeComm));
                }
                if self.ssnd_seen {
                    return Err(self.violation(Violation::DuplicateChunk { id }));
                }
                State::SsndHeader { size, pad }
            }
            b"FVER" if self.form == Form::Aifc => {
                if self.fver_seen {
                    return Err(self.violation(Violation::DuplicateChunk { id }));
                }
                if size != 4 {
                    return Err(self.violation(Violation::FverSize { size }));
                }
                State::Fver { pad }
            }
            // 规范定义的可选 chunk 与未知 chunk：跳过，不影响样本。
            _ => State::Skip {
                remaining: u64::from(size) + u64::from(pad),
                data: None,
            },
        };
        Ok(())
    }

    /// 解析 COMM 体（`b` 为完整的 chunk 数据）。
    fn parse_comm(&mut self, b: &[u8]) -> Result<Layout, DecodeError> {
        let channels = be16(b, 0) as i16;
        let frames = be32(b, 2);
        let sample_size = be16(b, 6) as i16;
        let mut raw = [0u8; 10];
        raw.copy_from_slice(&b[8..18]);
        if self.form == Form::Aifc {
            // pstring：1 字节计数 + 文本，总长为偶数（AIFF-C §Data Types）。
            let n = u32::from(b[22]);
            let plen = (1 + n) + ((1 + n) & 1);
            let expected = COMM_AIFC_BASE + plen;
            let size = u32::try_from(b.len()).unwrap_or(u32::MAX);
            if size != expected {
                return Err(self.violation(Violation::CommSize { size, expected }));
            }
        }
        if channels < 1 {
            return Err(self.violation(Violation::Channels { value: channels }));
        }
        if !(1..=32).contains(&sample_size) {
            return Err(self.violation(Violation::SampleSize { value: sample_size }));
        }
        if self.form == Form::Aifc {
            // AIFF-C 原文只定义以下五种 compressionType（AIFF-C §Common Chunk）。
            let tag = id_at(b, 18);
            match &tag {
                b"NONE" => {}
                b"ACE2" | b"ACE8" | b"MAC3" | b"MAC6" => {
                    return Err(
                        self.reject(DecodeError::Unsupported, AiffDetail::Compressed { tag })
                    );
                }
                _ => {
                    return Err(self.reject(
                        DecodeError::Unsupported,
                        AiffDetail::NonStandardType { tag },
                    ));
                }
            }
        }
        let Some(rate) = extended_to_u32(raw) else {
            return Err(self.reject(DecodeError::Unsupported, AiffDetail::SampleRate { raw }));
        };
        // 多声道约定（AIFF §Sound Data Chunk，“应遵守”）：1 单声道；2 左、右；3 左、右、中；
        // 4 声道有两种互相冲突的约定（quad 与 “4 channel”）；6 声道的单个“环绕”位置未定义。
        let channels = channels as u16;
        let layout = match channels {
            1 => Some(ChannelLayout::MONO),
            2 => Some(ChannelLayout::STEREO),
            // [FL FR FC] 正好是掩码 0x7 的顺序。
            3 => ChannelLayout::from_mask(0x7),
            4 | 6 => {
                return Err(self.reject(
                    DecodeError::Unsupported,
                    AiffDetail::AmbiguousLayout { channels },
                ));
            }
            _ => None,
        };
        let Some(layout) = layout else {
            return Err(self.reject(
                DecodeError::Unsupported,
                AiffDetail::UndefinedLayout { channels },
            ));
        };
        let bits = sample_size as u8;
        let format =
            SourceFormat::with_layout(rate, layout, bits).map_err(|_| DecodeError::Unsupported)?;
        self.frames = frames;
        Ok(Layout {
            format,
            bytes: usize::from(bits.div_ceil(8)),
            channels: usize::from(channels),
        })
    }

    /// 完整帧转换为右对齐 `i32`：大端、左对齐、补码（AIFF §Sample Points）。低位填充不属于样本值，
    /// 算术右移时丢弃。
    fn convert(&mut self, layout: &Layout, src: &[u8]) -> usize {
        let bytes = layout.bytes;
        let shift = 32 - u32::from(layout.format.bits());
        let samples = src.len() / bytes;
        for (o, c) in self.pcm[..samples].iter_mut().zip(src.chunks_exact(bytes)) {
            let mut w = 0u32;
            for (k, &x) in c.iter().enumerate() {
                w |= u32::from(x) << (24 - 8 * k);
            }
            *o = (w as i32) >> shift;
        }
        samples
    }

    fn step(&mut self, input: &[u8], eof: bool) -> Result<Progress, DecodeError> {
        let mut pos = 0usize;
        loop {
            let avail = &input[pos..];
            match self.state {
                State::Failed(e) => return Err(e),
                State::End => {
                    return Ok(Progress {
                        consumed: pos,
                        status: Status::End,
                    });
                }
                State::FormHeader => {
                    if avail.len() < 12 {
                        if eof {
                            return Err(DecodeError::Truncated);
                        }
                        return self.need_input(pos, avail.len(), false);
                    }
                    self.parse_form_header(&avail[..12])?;
                    pos += 12;
                    self.state = State::ChunkHeader;
                }
                State::ChunkHeader => {
                    if self.form_remaining == 0 {
                        self.finish()?;
                        self.state = State::End;
                        continue;
                    }
                    if self.form_remaining < 8 {
                        return Err(self.violation(Violation::StrayBytes {
                            bytes: self.form_remaining,
                        }));
                    }
                    if avail.len() < 8 {
                        return self.need_input(pos, avail.len(), eof);
                    }
                    self.parse_chunk_header(&avail[..8])?;
                    pos += 8;
                }
                State::Comm { size, pad } => {
                    let want = size as usize;
                    if avail.len() < want {
                        return self.need_input(pos, avail.len(), eof);
                    }
                    let layout = self.parse_comm(&avail[..want])?;
                    self.layout = Some(layout);
                    self.comm_seen = true;
                    self.take_form(u64::from(size))?;
                    pos += want;
                    self.state = State::Skip {
                        remaining: u64::from(pad),
                        data: None,
                    };
                }
                State::Fver { pad } => {
                    if avail.len() < 4 {
                        return self.need_input(pos, avail.len(), eof);
                    }
                    let timestamp = be32(avail, 0);
                    if timestamp != AIFC_VERSION_1 {
                        return Err(self.reject(
                            DecodeError::Unsupported,
                            AiffDetail::FormatVersion { timestamp },
                        ));
                    }
                    self.fver_seen = true;
                    self.take_form(4)?;
                    pos += 4;
                    self.state = State::Skip {
                        remaining: u64::from(pad),
                        data: None,
                    };
                }
                State::SsndHeader { size, pad } => {
                    let Some(layout) = self.layout else {
                        return Err(DecodeError::Malformed);
                    };
                    if size < 8 {
                        return Err(self.violation(Violation::SsndTooShort { size, needed: 8 }));
                    }
                    if avail.len() < 8 {
                        return self.need_input(pos, avail.len(), eof);
                    }
                    let offset = u64::from(be32(avail, 0));
                    // blockSize（avail[4..8]）只是对齐提示，不影响解码。
                    let frame_bytes = (layout.bytes * layout.channels) as u64;
                    let data = u64::from(self.frames) * frame_bytes;
                    let needed = 8 + offset + data;
                    if u64::from(size) < needed {
                        return Err(self.violation(Violation::SsndTooShort { size, needed }));
                    }
                    self.ssnd_seen = true;
                    self.take_form(8)?;
                    pos += 8;
                    // 帧数以 COMM 为准（AIFF-C §Number of Sample Frames）；帧之后剩余的字节不是样本。
                    let after = u64::from(size) - needed + u64::from(pad);
                    self.state = State::Skip {
                        remaining: offset,
                        data: Some((self.frames, after)),
                    };
                }
                State::Skip { remaining, data } => {
                    if remaining == 0 {
                        self.state = match data {
                            Some((frames, after)) => {
                                self.data_start = Some(self.offset + pos as u64);
                                State::Data { frames, after }
                            }
                            None => State::ChunkHeader,
                        };
                        continue;
                    }
                    if avail.is_empty() {
                        return self.need_input(pos, 0, eof);
                    }
                    let n = remaining.min(avail.len() as u64);
                    self.take_form(n)?;
                    pos += n as usize;
                    self.state = State::Skip {
                        remaining: remaining - n,
                        data,
                    };
                }
                State::Data { frames, after } => {
                    if frames == 0 {
                        self.state = State::Skip {
                            remaining: after,
                            data: None,
                        };
                        continue;
                    }
                    let Some(layout) = self.layout else {
                        return Err(DecodeError::Malformed);
                    };
                    let fb = layout.bytes * layout.channels;
                    let n = (avail.len() / fb)
                        .min(frames as usize)
                        .min(self.pcm.len() / layout.channels);
                    if n == 0 {
                        if !eof {
                            return self.need_input(pos, avail.len(), false);
                        }
                        let missing =
                            (u64::from(frames) * fb as u64).saturating_sub(avail.len() as u64);
                        return Err(self.reject(
                            DecodeError::Truncated,
                            AiffDetail::DataShort {
                                frames: self.frames,
                                missing,
                            },
                        ));
                    }
                    let bytes = n * fb;
                    let samples = self.convert(&layout, &avail[..bytes]);
                    self.take_form(bytes as u64)?;
                    pos += bytes;
                    self.start = 0;
                    self.end = samples;
                    self.state = State::Data {
                        frames: frames - n as u32,
                        after,
                    };
                    return Ok(Progress {
                        consumed: pos,
                        status: Status::Frames(n),
                    });
                }
            }
        }
    }
}

/// AIFF 是流式解码，资源需求固定（PCM 工作区与核心最小窗口），没有文件头阶段的容量拒绝。
impl qy_core::engine::Resources for AiffDecoder {}

impl Decoder for AiffDecoder {
    fn reset(&mut self) {
        self.state = State::FormHeader;
        self.form = Form::Aiff;
        self.form_remaining = 0;
        self.layout = None;
        self.frames = 0;
        self.comm_seen = false;
        self.ssnd_seen = false;
        self.fver_seen = false;
        self.detail = None;
        self.offset = 0;
        self.data_start = None;
        self.start = 0;
        self.end = 0;
    }

    fn format(&self) -> Option<SourceFormat> {
        self.layout.map(|l| l.format)
    }

    /// 输入分块任意；工作区仍有未取走样本时返回 `NeedOutput` 且不消耗输入。
    fn decode(&mut self, input: &[u8], eof: bool) -> Result<Progress, DecodeError> {
        if let State::Failed(e) = self.state {
            return Err(e);
        }
        if self.start < self.end {
            return Ok(Progress {
                consumed: 0,
                status: Status::NeedOutput,
            });
        }
        match self.step(input, eof) {
            Ok(p) => {
                self.offset += p.consumed as u64;
                Ok(p)
            }
            Err(e) => self.fail(e),
        }
    }

    /// 定位（S1）：字节位置 = 第一个样本帧的位置 + `frame` × 帧字节数，一次精确定位。
    /// 帧数以 COMM 为准。
    fn seek(&mut self, frame: u64) -> Result<u64, SeekError> {
        let (
            State::Data {
                frames: left,
                after,
            },
            Some(layout),
            Some(data_start),
        ) = (self.state, self.layout, self.data_start)
        else {
            return Err(SeekError::Unsupported);
        };
        let total = u64::from(self.frames);
        if frame >= total {
            return Err(SeekError::PastEnd);
        }
        let fb = (layout.bytes * layout.channels) as u64;
        let read = (total - u64::from(left)) * fb;
        let skip = frame * fb;
        let form = (u64::from(self.form_remaining) + read)
            .checked_sub(skip)
            .ok_or(SeekError::PastEnd)?;
        self.form_remaining = u32::try_from(form).map_err(|_| SeekError::PastEnd)?;
        self.state = State::Data {
            frames: u32::try_from(total - frame).map_err(|_| SeekError::PastEnd)?,
            after,
        };
        let offset = data_start + skip;
        self.offset = offset;
        self.start = 0;
        self.end = 0;
        Ok(offset)
    }

    fn pcm(&self) -> &[i32] {
        &self.pcm[self.start..self.end]
    }

    fn release(&mut self, frames: usize) {
        let channels = self.layout.map_or(1, |l| l.channels);
        let samples = frames.saturating_mul(channels);
        self.start = self.start.saturating_add(samples).min(self.end);
        if self.start == self.end {
            self.start = 0;
            self.end = 0;
        }
    }
}

/// M2 T5：Kani 有界证明（`cargo kani -p qy-codec-aiff`）。80 位扩展精度采样率的换算对任意 10 字节
/// 不 panic（移位量受检查），返回值精确：`Some(r)` 时 r ≥ 1 且 r 与 m × 2^(e − 16383 − 63) 精确
/// 相等（不四舍五入）；正的有限整数值且不超过 `u32::MAX` 时必然返回 `Some`。
#[cfg(kani)]
mod proofs {
    use super::*;

    #[kani::proof]
    fn extended_to_u32_is_exact() {
        let b: [u8; 10] = kani::any();
        let r = extended_to_u32(b);
        let se = u16::from_be_bytes([b[0], b[1]]);
        let exp = i32::from(se & 0x7FFF);
        let m = u64::from_be_bytes([b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9]]);
        let shift = exp - 16383 - 63;
        // 独立判断：值是否为不超过 u32::MAX 的正整数，是则求出它。
        let positive = se & 0x8000 == 0 && exp != 0x7FFF && m != 0;
        let expect: Option<u128> = if !positive {
            None
        } else if shift >= 0 {
            if shift >= 32 {
                None
            } else {
                Some(u128::from(m) << shift)
            }
        } else {
            let s = shift.unsigned_abs();
            if s >= 64 {
                None
            } else if (u128::from(m) >> s) << s == u128::from(m) {
                Some(u128::from(m) >> s)
            } else {
                None
            }
        };
        let expect = expect.filter(|&v| v >= 1 && v <= u128::from(u32::MAX));
        assert!(r.map(u128::from) == expect);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 44100 的 80 位扩展精度：指数 0x400E，尾数 0xAC44 << 48。
    #[test]
    fn extended_sample_rates() {
        let ext = |v: u64| {
            // 规范化：最高位为整数位。
            let lz = v.leading_zeros();
            let m = v << lz;
            let e = 16383 + 63 - lz as u16;
            let mut b = [0u8; 10];
            b[..2].copy_from_slice(&e.to_be_bytes());
            b[2..].copy_from_slice(&m.to_be_bytes());
            b
        };
        assert_eq!(ext(44_100), [0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0]);
        for r in [
            1u64,
            8_000,
            44_100,
            48_000,
            96_000,
            192_000,
            2_822_400,
            u64::from(u32::MAX),
        ] {
            assert_eq!(extended_to_u32(ext(r)), u32::try_from(r).ok(), "{r}");
        }
        assert_eq!(extended_to_u32(ext(1 << 32)), None);
        // 44100.5：尾数低位非 0。
        let mut half = ext(88_201);
        half[1] -= 1;
        assert_eq!(extended_to_u32(half), None);
        // 0、负数、无穷。
        assert_eq!(extended_to_u32([0; 10]), None);
        let mut neg = ext(44_100);
        neg[0] |= 0x80;
        assert_eq!(extended_to_u32(neg), None);
        assert_eq!(
            extended_to_u32([0x7F, 0xFF, 0x80, 0, 0, 0, 0, 0, 0, 0]),
            None
        );
        // 未规范化的尾数（整数位为 0）按数值换算。
        assert_eq!(
            extended_to_u32([0x40, 0x0F, 0x56, 0x22, 0, 0, 0, 0, 0, 0]),
            Some(44_100)
        );
        // 极小值。
        assert_eq!(extended_to_u32([0, 1, 0x80, 0, 0, 0, 0, 0, 0, 0]), None);
    }
}
