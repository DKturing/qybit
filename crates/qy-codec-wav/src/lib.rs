//! QyBit 自有推送式 WAV 解码器。
//!
//! `#![no_std]`、无 alloc；PCM 工作区由宿主以 `&'static mut [i32]` 提供。
//! 解码器不缓存输入：每次调用只看宿主传入的切片，返回实际消耗字节数，
//! 未消耗的字节由宿主（核心输入窗口）保留到下次调用（AGENTS §2）。
//! 任一结构所需的最大连续输入为 40 字节（fmt 前缀）、一帧，或 16 位双声道 data 开头用于识别
//! 压缩码流的 [`BITSTREAM_SCAN`] 字节（小于核心最小输入窗口），因此任意输入分块都能推进。
//!
//! 支持范围：
//! - RIFF/WAVE，小端；格式码 1（PCM）、3（`WAVE_FORMAT_IEEE_FLOAT`）与 0xFFFE（EXTENSIBLE，子格式
//!   PCM 或 `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT`）。
//! - RF64 / BW64（F4，tasks/F4.md）：结构与 RIFF/WAVE 相同，长度由紧接 `WAVE` 的 `ds64` 块给出
//!   （riffSize、dataSize、第三个 64 位值，其后为块大小表），支持超过 4 GiB 的 data。按 ITU-R
//!   BS.2088-2 §2.4：RIFF 与 data 的 32 位长度不是 0xFFFFFFFF 时用 32 位值，是时用 ds64 的 64 位值；
//!   其他块的长度为 0xFFFFFFFF 时按块标识查 `ds64` 表。第三个 64 位值在 RF64（EBU Tech 3306 v1）
//!   中是 `fact` 的帧数，浮点文件的 `fact` 为 0xFFFFFFFF 时取它；在 BW64 中是 dummy，读取时忽略
//!   （§4.2）。流式写入程序（ffmpeg 输出到管道）
//!   留下的 `ds64` 全零按 W1 的 R1/R2 处理。块大小表最多存 [`DS64_TABLE`] 项（不分配内存），超出且
//!   后面确实需要查表时按容量不足拒绝并写明。
//! - 浮点（F3）：32 / 64 位 IEEE 754，小端，样本按位输出（binary32 每样本 1 个 `i32`，binary64 每样本
//!   2 个 `i32`，先低后高），不做数值运算；EXTENSIBLE 的有效位必须等于容器。`fact` 块（样本帧数）
//!   可有可无：RIFF 1991 原文只要求压缩格式有 `fact`，没有时帧数按 data 长度计算（用户 2026-10-16
//!   决定，tasks/F3.md §2）；data 之前的 `fact` 重复、帧数与 data 不符、或 data 长度为占位值而无法
//!   核对时按不合规拒绝并给出 [`WavDetail::Fact`]。data 之后的块不读取（同 PCM）。
//! - 容器 8/16/24/32 位；采样率不设软件上限（M3 T1）；声道布局见 F1。
//! - 8 位 PCM 按 WAVE 规范为无符号、偏移 128：读取时减去 128 得到有符号值（W1），是精确的整数
//!   表示变换。
//! - EXTENSIBLE 有效位 V ≤ C，左对齐于容器高位，读取时算术右移 C−V，
//!   输出右对齐 `i32`，N = V。容器低位填充按规范不属于样本值，被丢弃。
//!
//! 真实世界文件头的容错（W1，tasks/W1.md）：只在意图没有歧义时修正结构，不改、不编造样本；
//! 每条修正记为 [`Repair`]，经 [`qy_core::engine::Resources`] 报告：
//! - R1：RIFF 长度为 0xFFFFFFFF（ffmpeg 输出到管道等流式写入程序的占位值）：不以 RIFF 长度为界，
//!   读到源末尾；
//! - R2：data 长度为 0xFFFFFFFF（同上）：data 延伸到源末尾，按整帧读取，丢弃不足一帧的尾部；
//! - R3：RIFF 长度与源的实际长度不符，而 data 的声明长度在源内完整：以 data 为准。RIFF 偏短时
//!   data 越出 RIFF（以及 data 之后的块越出 RIFF）不再整首拒绝；RIFF 偏长时 data 之后源提前结束
//!   视为正常结束；
//! - R4：data 长度不是整帧：播放全部完整帧，丢弃不完整的尾帧；
//! - R5：data 声明长度超过源的实际长度（录音中断）：播放完整帧后照旧报 `Truncated`，明细
//!   [`WavDetail::DataShort`] 写明缺少的字节数。
//!
//! 伪装成 PCM 的压缩码流：16 位双声道 data 开头若是 DTS 同步字（14 位或 16 位，两种字节序，
//! 常见于“DTS CD”的 DTS-WAV）或 IEC 61937（S/PDIF 封装的 AC-3/DTS 等）前导，按 `Unsupported`
//! 拒绝并给出 [`WavDetail::Bitstream`]，不把码流当成 PCM 播放成噪声。这些是有损编码，不在本解码器
//! 范围内；本解码器也不做数字直通。
//!
//! 意图不明的照旧拒绝：fmt 字段互相矛盾、data 在 fmt 之前、重复的 fmt/data、data 之前的块越出
//! RIFF、RIFF 为占位值而 data 长度为 0（可能是未回填的占位，也可能真的为空）。
//!
//! 错误分类：结构违反 RIFF/WAVE 规范为 [`DecodeError::Malformed`]；
//! 合法但不支持（16/24 位浮点、RIFX 等）为 [`DecodeError::Unsupported`]；`ds64` 表项超过本解码器的
//! 容量为 [`DecodeError::CapacityExceeded`]；
//! 源在完整结构前结束为 [`DecodeError::Truncated`]。
//! 出错后解码器保持该错误，直到 [`Decoder::reset`]。
//!
//! 规范出处：Microsoft “Multimedia Programming Interface and Data
//! Specifications 1.0”（RIFF chunk 与 WORD 对齐填充）；EBU Tech 3306 v2 与 ITU-R BS.2088-2
//! （RF64 / BW64 的 `ds64`；EBU Tech 3306 v2 指定以 BS.2088 为准，按 BS.2088-2 核对，另以
//! FFmpeg n7.1 `wavdec.c` 与 libsndfile 1.2.2 `rf64.c` 交叉核对，见 docs/specs.md）；
//! `WAVEFORMATEX`、`WAVEFORMATEXTENSIBLE`（mmreg.h / ksmedia.h，
//! 有效位、声道掩码、`KSDATAFORMAT_SUBTYPE_PCM`）。
#![no_std]

use qy_core::engine::{MAX_REPAIRS, Repair};
use qy_core::{
    ChannelLayout, DecodeError, Decoder, PCM_WORK_SAMPLES, Progress, SampleEncoding, SeekError,
    SourceFormat, Status,
};

/// 流式写入程序的长度占位值（ffmpeg 输出到不可定位的管道时，RIFF 与 data 都写 0xFFFFFFFF；
/// 见 tasks/W1.md 与 docs/w1-wav-tolerance.md 的样例核对）。
const LENGTH_PLACEHOLDER: u32 = 0xFFFF_FFFF;

/// 16 位双声道 data 开头用于识别压缩码流的字节数。
pub const BITSTREAM_SCAN: usize = 4096;

/// `ds64` 块大小表在解码器内最多存放的表项数（F4）。真实写入程序写的表为空（ffmpeg、libsndfile）；
/// 只有长度超过 4 GiB 的非 data 块才需要表项。
pub const DS64_TABLE: usize = 8;

/// `ds64` 体的固定部分：riffSize、dataSize、sampleCount（各 8 字节）与 tableLength（4 字节）。
const DS64_FIXED: u64 = 28;
/// 一个 `ds64` 表项：块标识（4 字节）+ 64 位长度。
const DS64_ENTRY: u64 = 12;

/// 64 位计数的“无”（源长度不可能达到）。修正状态用哨兵值而不是 `Option`，保持解码器结构紧凑。
const NONE: u64 = u64::MAX;

const FORMAT_PCM: u16 = 0x0001;
/// `WAVE_FORMAT_IEEE_FLOAT`（mmreg.h）。
const FORMAT_IEEE_FLOAT: u16 = 0x0003;
const FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// `KSDATAFORMAT_SUBTYPE_PCM` = {00000001-0000-0010-8000-00AA00389B71} 的磁盘字节序。
const SUBTYPE_PCM: [u8; 16] = [
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

/// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT` = {00000003-0000-0010-8000-00AA00389B71} 的磁盘字节序。
const SUBTYPE_IEEE_FLOAT: [u8; 16] = [
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

pub const SPEAKER_FRONT_LEFT: u32 = 0x1;
pub const SPEAKER_FRONT_RIGHT: u32 = 0x2;
pub const SPEAKER_FRONT_CENTER: u32 = 0x4;

/// 基本 fmt 字段长度（WAVEFORMATEX 去掉 cbSize）。
const FMT_BASE_LEN: u32 = 16;
/// WAVEFORMATEXTENSIBLE 完整长度（18 + cbSize 22）。
const FMT_EXT_LEN: u32 = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    RiffHeader,
    ChunkHeader,
    Fmt {
        size: u64,
        pad: u64,
    },
    /// `fact` 块：读出前 4 字节（dwSampleLength），其余跳过。
    Fact {
        size: u64,
        pad: u64,
    },
    /// `ds64` 块（F4）：先读固定的 28 字节。
    Ds64 {
        size: u64,
        pad: u64,
    },
    /// `ds64` 的块大小表：还有 `entries` 项未读，表之后还有 `rest` 字节（及填充）要跳过。
    Ds64Table {
        entries: u32,
        rest: u64,
        pad: u64,
    },
    Skip {
        remaining: u64,
    },
    /// `remaining` 为尚未读取的完整帧字节；`tail` 为声明长度中不足一帧的尾部（R4）；
    /// `to_eof` 时 data 延伸到源末尾（R2），`remaining` 不用。
    Data {
        declared: u64,
        remaining: u64,
        tail: u64,
        pad: u64,
        to_eof: bool,
    },
    End,
    Failed(DecodeError),
}

/// RIFF 长度的用法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Riff {
    /// 以 RIFF 长度为界：“WAVE” 之后尚未消耗的字节数。
    Bounded(u64),
    /// RIFF 长度不可用（占位值或与实际结构不符）：以各块的声明长度为准，读到源末尾。
    Open,
}

/// 文件头的容器类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Container {
    Riff,
    /// RF64（EBU Tech 3306）或 BW64（ITU-R BS.2088）：长度见 `ds64`。
    Rf64,
}

/// `ds64` 块的三个 64 位长度（F4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ds64 {
    riff: u64,
    data: u64,
    samples: u64,
}

/// `ds64` 块大小表的一项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TableEntry {
    id: [u8; 4],
    size: u64,
    used: bool,
}

const EMPTY_ENTRY: TableEntry = TableEntry {
    id: [0; 4],
    size: 0,
    used: false,
};

/// 解析后的 fmt 信息。
#[derive(Debug, Clone, Copy)]
struct Layout {
    format: SourceFormat,
    /// 容器字节数；为 1 时样本是无符号的（偏移 128）。
    container_bytes: usize,
    shift: u32,
    block_align: usize,
    channels: usize,
    channel_mask: Option<u32>,
    /// 样本编码（F3）；浮点时 `shift` 为 0。
    encoding: SampleEncoding,
}

impl Layout {
    /// 一帧在 PCM 工作区中占的 `i32` 个数。
    fn words_per_frame(&self) -> usize {
        self.channels * self.encoding.words()
    }
}

/// 拒绝的明细（宿主报告用；`DecodeError` 只给类别）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WavDetail {
    /// 多于 2 声道但没有完整声明扬声器布局：格式码 1（`mask` 为 `None`）、EXTENSIBLE 掩码为 0、
    /// 置位数少于声道数，或含无扬声器位置的位（F1）。
    UndefinedLayout { channels: u16, mask: Option<u32> },
    /// data 声明长度超过源的实际长度（W1 R5）：完整帧已全部输出，源比声明短 `missing` 字节。
    DataShort { declared: u64, missing: u64 },
    /// data 不是 PCM，而是伪装成 16 位双声道 PCM 的压缩码流（在 data 中 `offset` 字节处识别）。
    Bitstream { kind: Bitstream, offset: u32 },
    /// 浮点文件 data 之前的 `fact` 块不合规（F3）：`fact` 重复（`duplicate`），或声明的帧数
    /// `declared` 与 data 的完整帧数 `frames` 不符（data 长度为占位值时 `frames` 为 `None`，无法核对）。
    Fact {
        declared: u64,
        frames: Option<u64>,
        duplicate: bool,
    },
    /// RF64 / BW64 的 `ds64` 块有问题（F4）。
    Ds64(Ds64Problem),
}

/// `ds64` 的问题（F4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ds64Problem {
    /// `WAVE` 之后的第一个块不是 `ds64`（`first` 为实际的块标识）。
    Missing { first: [u8; 4] },
    /// `ds64` 的长度不足固定部分的 28 字节。
    TooShort { size: u64 },
    /// 表项总长超出 `ds64` 块。
    TableOverrun { entries: u32, size: u64 },
    /// 第二个 `ds64` 块。
    Duplicate,
    /// `ds64` 声明的 RIFF 长度容纳不下 `ds64` 块本身。
    RiffTooSmall { riff: u64 },
    /// 块长度为 0xFFFFFFFF，`ds64` 表中没有该块的长度。
    NoTableEntry { chunk: [u8; 4] },
    /// `ds64` 表有 `entries` 项，超过本解码器能存放的 `capacity` 项，而后面的块需要查表。
    TableCapacity { entries: u32, capacity: u32 },
}

/// 识别出的压缩码流类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bitstream {
    /// DTS，14 位字（“DTS CD”常用）。
    Dts14,
    /// DTS，16 位字。
    Dts16,
    /// IEC 61937（S/PDIF 封装的 AC-3、DTS、AAC 等）。
    Iec61937,
}

/// 在 `b`（data 开头）中按 2 字节对齐查找压缩码流。普通 PCM（含削波的极值）偶然命中的概率
/// 可以忽略：14 位 DTS 的同步模式有 44 个固定位；16 位 DTS 的同步字 0x7FFE 0x8001 在削波音频中
/// 可能自然出现，因此另核对帧头字段，并在下一帧落在窗口内时要求那里也有同步字；IEC 61937 的前导
/// 之后核对数据类型与长度。
///
/// 出处：ETSI TS 102 114（DTS Coherent Acoustics）帧头：同步字 0x7FFE8001（16 位）/
/// 0x1FFFE800 + 0x07F（14 位），其后 FTYPE(1) SHORT(5) CPF(1) NBLKS(7) FSIZE(14) AMODE(6)
/// SFREQ(4)，正常帧 FTYPE = 1、SHORT = 31，NBLKS ≥ 5，FSIZE ≥ 95；IEC 61937-1 突发前导
/// Pa = 0xF872、Pb = 0x4E1F，Pc 低 5 位为数据类型，Pd 为长度。
fn detect_bitstream(b: &[u8]) -> Option<(Bitstream, usize)> {
    let mut i = 0;
    while i + 6 <= b.len() {
        let w = &b[i..i + 6];
        let kind = match w {
            // 14 位 DTS：字 1FFF E800 07Fx，小端与大端。
            [0xFF, 0x1F, 0x00, 0xE8, x, 0x07] if x & 0xF0 == 0xF0 => Some(Bitstream::Dts14),
            [0x1F, 0xFF, 0xE8, 0x00, 0x07, x] if x & 0xF0 == 0xF0 => Some(Bitstream::Dts14),
            [0xFE, 0x7F, 0x01, 0x80, ..] if dts16_frame(b, i, false) => Some(Bitstream::Dts16),
            [0x7F, 0xFE, 0x80, 0x01, ..] if dts16_frame(b, i, true) => Some(Bitstream::Dts16),
            [0x72, 0xF8, 0x1F, 0x4E, ..] if iec61937_burst(b, i, false) => {
                Some(Bitstream::Iec61937)
            }
            [0xF8, 0x72, 0x4E, 0x1F, ..] if iec61937_burst(b, i, true) => Some(Bitstream::Iec61937),
            _ => None,
        };
        if let Some(k) = kind {
            return Some((k, i));
        }
        i += 2;
    }
    None
}

/// 第 `k` 个 16 位字（从 `at` 起），`be` 为大端。
fn word(b: &[u8], at: usize, k: usize, be: bool) -> Option<u16> {
    let p = at.checked_add(2 * k)?;
    let x = *b.get(p)?;
    let y = *b.get(p + 1)?;
    Some(if be {
        u16::from_be_bytes([x, y])
    } else {
        u16::from_le_bytes([x, y])
    })
}

/// `at` 处的 16 位 DTS 同步字之后是否是合理的正常帧头（见 [`detect_bitstream`]）。
fn dts16_frame(b: &[u8], at: usize, be: bool) -> bool {
    let (Some(w2), Some(w3), Some(w4)) =
        (word(b, at, 2, be), word(b, at, 3, be), word(b, at, 4, be))
    else {
        return false;
    };
    // 同步字之后的 48 位：FTYPE SHORT CPF NBLKS FSIZE AMODE SFREQ …
    let bits = (u64::from(w2) << 32) | (u64::from(w3) << 16) | u64::from(w4);
    let field = |start: u32, len: u32| (bits >> (48 - start - len)) & ((1 << len) - 1);
    let (ftype, short, nblks, fsize, sfreq) = (
        field(0, 1),
        field(1, 5),
        field(7, 7),
        field(14, 14),
        field(34, 4),
    );
    if ftype != 1 || short != 31 || nblks < 5 || fsize < 95 {
        return false;
    }
    if !matches!(sfreq, 1 | 2 | 3 | 6 | 7 | 8 | 11 | 12 | 13) {
        return false;
    }
    // 下一帧在窗口内时必须也以同步字开始。
    let next = at + fsize as usize + 1;
    match (word(b, next, 0, be), word(b, next, 1, be)) {
        (Some(a), Some(c)) => a == 0x7FFE && c == 0x8001,
        _ => true,
    }
}

/// `at` 处的 IEC 61937 前导之后是否是合理的突发头：数据类型为已定义的压缩格式，长度非零。
fn iec61937_burst(b: &[u8], at: usize, be: bool) -> bool {
    let (Some(pc), Some(pd)) = (word(b, at, 2, be), word(b, at, 3, be)) else {
        return false;
    };
    // 数据类型：AC-3、MPEG、AAC、ATRAC、WMA Pro、DTS、E-AC-3、MAT 等（1–23 中已定义的值）。
    let kind = pc & 0x1F;
    matches!(kind, 1 | 4..=13 | 16 | 17 | 21 | 22) && pd != 0
}

impl core::fmt::Display for WavDetail {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Self::UndefinedLayout {
                channels,
                mask: None,
            } => {
                write!(
                    f,
                    "源文件未声明 {channels} 声道的扬声器布局（WAVE_FORMAT_PCM）"
                )
            }
            Self::UndefinedLayout {
                channels,
                mask: Some(m),
            } => write!(
                f,
                "源文件声明的声道掩码 {m:#x} 不能确定 {channels} 声道的扬声器布局"
            ),
            Self::DataShort { declared, missing } => write!(
                f,
                "文件比 data 声明的 {declared} 字节短 {missing} 字节（录音中断或文件被截断），已播放全部完整帧"
            ),
            Self::Fact {
                duplicate: true, ..
            } => write!(f, "浮点 WAV 有重复的 fact 块，帧数不能确定"),
            Self::Fact {
                declared: d,
                frames: Some(n),
                ..
            } => write!(
                f,
                "浮点 WAV 的 fact 块声明 {d} 帧，data 中有 {n} 帧，两者不符"
            ),
            Self::Fact {
                declared: d,
                frames: None,
                ..
            } => write!(
                f,
                "浮点 WAV 的 data 长度是流式写入程序的占位值，无法与 fact 块声明的 {d} 帧核对"
            ),
            Self::Ds64(p) => write!(f, "{p}"),
            Self::Bitstream { kind, offset } => {
                let what = match kind {
                    Bitstream::Dts14 => "DTS 编码流（14 位字，常见于“DTS CD”的 DTS-WAV）",
                    Bitstream::Dts16 => "DTS 编码流（16 位字）",
                    Bitstream::Iec61937 => "IEC 61937 码流（S/PDIF 封装的 AC-3/DTS 等）",
                };
                write!(
                    f,
                    "文件头写的是 16 位双声道 PCM，但 data 中（偏移 {offset} 字节）是{what}，不是 PCM。\
                     这是有损压缩格式，本程序不解码，也不做数字直通；当作 PCM 播放只会是噪声"
                )
            }
        }
    }
}

/// 4 字节标识的显示：可打印 ASCII 原样，其余以 `?` 代替。
struct Id([u8; 4]);

impl core::fmt::Display for Id {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for &b in &self.0 {
            let c = if (0x20..0x7F).contains(&b) {
                b as char
            } else {
                '?'
            };
            core::fmt::Write::write_char(f, c)?;
        }
        Ok(())
    }
}

impl core::fmt::Display for Ds64Problem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Self::Missing { first } => write!(
                f,
                "RF64/BW64 文件 WAVE 之后的第一个块应是 ds64，实际是 '{}'",
                Id(first)
            ),
            Self::TooShort { size } => {
                write!(f, "ds64 块只有 {size} 字节，不足固定部分的 28 字节")
            }
            Self::TableOverrun { entries, size } => {
                write!(f, "ds64 声明 {entries} 个表项，超出 {size} 字节的 ds64 块")
            }
            Self::Duplicate => write!(f, "RF64/BW64 文件有第二个 ds64 块"),
            Self::RiffTooSmall { riff } => {
                write!(f, "ds64 声明的 RIFF 长度 {riff} 字节容纳不下 ds64 块本身")
            }
            Self::NoTableEntry { chunk } => write!(
                f,
                "块 '{}' 的长度为 0xFFFFFFFF，但 ds64 表中没有它的长度",
                Id(chunk)
            ),
            Self::TableCapacity { entries, capacity } => write!(
                f,
                "ds64 表有 {entries} 项，超过本解码器能存放的 {capacity} 项，后面的块需要查表"
            ),
        }
    }
}

/// WAV 解码器。
pub struct WavDecoder {
    pcm: &'static mut [i32],
    state: State,
    riff: Riff,
    /// 文件头的标识（`RIFF`、`RF64`、`BW64`），修正报告用。
    magic: [u8; 4],
    container: Container,
    /// `ds64` 的三个长度（RF64 / BW64，读到之后）。
    ds64: Option<Ds64>,
    /// `ds64` 块大小表（前 `table_len` 项）；`table_declared` 为表声明的总项数。
    table: [TableEntry; DS64_TABLE],
    table_len: usize,
    table_declared: u32,
    /// RIFF 声明的长度（RF64 / BW64 的 32 位字段为 0xFFFFFFFF 时为 `ds64.riffSize`）。
    riff_declared: u64,
    /// RF64 / BW64 的 32 位 RIFF 长度不是 0xFFFFFFFF，按 BS.2088-2 §2.4 使用它（F4）。
    riff_from_32: bool,
    /// data 的声明长度原值（R2 报告用：RIFF 为 0xFFFFFFFF，RF64 为 `ds64.dataSize`）。
    data_declared: u64,
    /// RIFF 长度为占位值（R1）。
    riff_placeholder: bool,
    /// data 越出 RIFF（R3，RIFF 偏短），结束时报告。
    riff_short: bool,
    layout: Option<Layout>,
    detail: Option<WavDetail>,
    data_seen: bool,
    /// data 的完整帧已全部读完（之后源的结构问题不影响样本）。
    data_done: bool,
    /// data 开头已检查过压缩码流。
    scanned: bool,
    /// data 长度为占位值（R2）时读到的字节数；[`NONE`] 表示不是占位值。
    data_placeholder_read: u64,
    /// 源在 data 读完后结束、RIFF 长度需要修正（R1/R3）时，RIFF 体实际的字节数；否则 [`NONE`]。
    riff_actual: u64,
    /// 丢弃的不足一帧的尾部字节数（R4）；0 表示没有丢弃。
    partial_dropped: u32,
    /// data 声明的完整帧数（长度为占位值或尚未读到 data 时为 `None`）。
    declared_frames: Option<u64>,
    /// `fact` 块声明的样本帧数（F3；RF64 的 0xFFFFFFFF 已换成 `ds64.sampleCount`）；尚未读到时为 `None`。
    fact: Option<u64>,
    /// 读到了不止一个 `fact` 块。
    fact_duplicate: bool,
    /// 已消耗的总字节数（用于报告源的实际长度）。
    offset: u64,
    /// data 第一个字节距源开头的位置（S1 定位）；尚未读到 data 时为 [`NONE`]。
    data_start: u64,
    /// 已解码未取走样本：`pcm[start..end]`。
    start: usize,
    end: usize,
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

impl WavDecoder {
    /// 用宿主提供的 PCM 工作区构造；长度不足 [`PCM_WORK_SAMPLES`] 返回容量错误。
    pub fn new(pcm: &'static mut [i32]) -> Result<Self, DecodeError> {
        if pcm.len() < PCM_WORK_SAMPLES {
            return Err(DecodeError::CapacityExceeded);
        }
        Ok(Self {
            pcm,
            state: State::RiffHeader,
            riff: Riff::Bounded(0),
            magic: *b"RIFF",
            container: Container::Riff,
            ds64: None,
            table: [EMPTY_ENTRY; DS64_TABLE],
            table_len: 0,
            table_declared: 0,
            riff_declared: 0,
            riff_from_32: false,
            data_declared: 0,
            riff_placeholder: false,
            riff_short: false,
            layout: None,
            detail: None,
            data_seen: false,
            data_done: false,
            scanned: false,
            data_placeholder_read: NONE,
            riff_actual: NONE,
            partial_dropped: 0,
            declared_frames: None,
            fact: None,
            fact_duplicate: false,
            offset: 0,
            data_start: NONE,
            start: 0,
            end: 0,
        })
    }

    /// 工作区容量（样本数）。
    pub fn capacity_samples(&self) -> usize {
        self.pcm.len()
    }

    /// 交还 PCM 工作区（所有权转移，不复制）。`qy-codec-auto` 用它让 WAV 与 AIFF 解码器共用
    /// 同一块工作区（F2）。
    pub fn into_pcm(self) -> &'static mut [i32] {
        self.pcm
    }

    /// 最近一次拒绝的明细（新曲目 `reset` 后清除）。
    pub fn last_detail(&self) -> Option<WavDetail> {
        self.detail
    }

    /// 当前曲目已做的结构修正（W1），顺序：不足一帧的尾部、RIFF 长度、data 长度。
    pub fn repairs(&self) -> impl Iterator<Item = Repair> {
        self.repair_list().into_iter().flatten()
    }

    /// 修正只记录几个数值（不占缓冲），报告时再组成 [`Repair`]。
    fn repair_list(&self) -> [Option<Repair>; MAX_REPAIRS] {
        let partial = (self.partial_dropped > 0).then_some(Repair::PartialFrameDropped {
            bytes: self.partial_dropped,
        });
        let riff = (self.riff_actual != NONE).then(|| {
            let actual = self.riff_actual;
            if self.riff_placeholder {
                Repair::LengthPlaceholder {
                    chunk: self.magic,
                    declared: self.riff_declared,
                    read: actual,
                }
            } else {
                Repair::LengthMismatch {
                    chunk: self.magic,
                    declared: self.riff_declared,
                    actual,
                }
            }
        });
        let data = (self.data_placeholder_read != NONE && self.data_done).then_some(
            Repair::LengthPlaceholder {
                chunk: *b"data",
                declared: self.data_declared,
                read: self.data_placeholder_read,
            },
        );
        [partial, riff, data, None]
    }

    /// EXTENSIBLE 的声道掩码；格式码 1 或头部未解析时为 `None`。
    /// 文件头声明的总帧数（data 长度 ÷ 块对齐，不足一帧的尾部不计）。长度为占位值或尚未读到
    /// data 时为 `None`。来自不可信的文件头，只用于显示时长；源比声明短时实际帧数更少。
    pub fn declared_frames(&self) -> Option<u64> {
        self.declared_frames
    }

    pub fn channel_mask(&self) -> Option<u32> {
        self.layout.and_then(|l| l.channel_mask)
    }

    /// 刚解析完一个块头（本次调用已消耗 `pos` 字节）：是 data 时记下数据起点。
    fn note_data_start(&mut self, pos: usize) {
        if matches!(self.state, State::Data { .. }) && self.data_start == NONE {
            self.data_start = self.offset + pos as u64;
        }
    }

    /// 定位到第 `frame` 帧（S1）：字节位置 = data 起点 + `frame` × 块对齐，一次精确定位。
    /// 只在 data 中有效；占位长度（R2）的 data 不知道总帧数，超出源末尾时读到 EOF 正常结束。
    fn seek_to(&mut self, frame: u64) -> Result<u64, SeekError> {
        let (
            State::Data {
                declared,
                remaining,
                tail,
                pad,
                to_eof,
            },
            Some(layout),
        ) = (self.state, self.layout)
        else {
            return Err(SeekError::Unsupported);
        };
        if self.data_start == NONE {
            return Err(SeekError::Unsupported);
        }
        let ba = layout.block_align as u64;
        let bytes = frame.checked_mul(ba).ok_or(SeekError::PastEnd)?;
        let offset = self
            .data_start
            .checked_add(bytes)
            .ok_or(SeekError::PastEnd)?;
        // data 中已读过的字节（RIFF 余量按其回退后再扣除目标位置之前的字节）。
        let read = if to_eof {
            self.data_placeholder_read
        } else {
            declared - tail - remaining
        };
        let remaining = if to_eof {
            0
        } else {
            let full = declared - tail;
            if bytes >= full {
                return Err(SeekError::PastEnd);
            }
            full - bytes
        };
        if let Riff::Bounded(r) = self.riff {
            let at_data = r.checked_add(read).ok_or(SeekError::PastEnd)?;
            let left = at_data.checked_sub(bytes).ok_or(SeekError::PastEnd)?;
            self.riff = Riff::Bounded(left);
        }
        if to_eof {
            self.data_placeholder_read = bytes;
        }
        self.state = State::Data {
            declared,
            remaining,
            tail,
            pad,
            to_eof,
        };
        self.offset = offset;
        self.start = 0;
        self.end = 0;
        Ok(offset)
    }

    fn fail(&mut self, e: DecodeError) -> Result<Progress, DecodeError> {
        self.state = State::Failed(e);
        Err(e)
    }

    fn need_input(&mut self, consumed: usize, eof: bool) -> Result<Progress, DecodeError> {
        if eof {
            self.fail(DecodeError::Truncated)
        } else {
            Ok(Progress {
                consumed,
                status: Status::NeedInput,
            })
        }
    }

    /// 从 RIFF 余量中扣除已消耗字节；调用前已保证不越界。RIFF 长度不可用时不计。
    fn take_riff(&mut self, n: u64) -> Result<(), DecodeError> {
        if let Riff::Bounded(r) = self.riff {
            self.riff = Riff::Bounded(r.checked_sub(n).ok_or(DecodeError::Malformed)?);
        }
        Ok(())
    }

    /// 源已到末尾、data 的完整帧已全部读完：消耗剩余字节并正常结束，记下与源实际长度相关的修正。
    /// `rest` 为源中剩余（不再解析）的字节数。
    fn end_at_eof(&mut self, pos: usize, rest: usize) -> Result<Progress, DecodeError> {
        let consumed = pos + rest;
        // 源的实际长度减去 RIFF 头的 8 字节，即 RIFF 体实际的字节数。
        let actual = (self.offset + consumed as u64).saturating_sub(8);
        if self.riff_placeholder
            || self.riff_short
            || matches!(self.riff, Riff::Bounded(r) if r > 0)
        {
            self.riff_actual = actual;
        }
        self.state = State::End;
        Ok(Progress {
            consumed,
            status: Status::End,
        })
    }

    fn parse_riff_header(&mut self, b: &[u8]) -> Result<(), DecodeError> {
        let magic = [b[0], b[1], b[2], b[3]];
        match &magic {
            b"RIFF" => {}
            b"RF64" | b"BW64" => self.container = Container::Rf64,
            b"RIFX" => return Err(DecodeError::Unsupported),
            _ => return Err(DecodeError::Malformed),
        }
        if &b[8..12] != b"WAVE" {
            return Err(DecodeError::Malformed);
        }
        self.magic = magic;
        let size = u32_at(b, 4);
        if self.container == Container::Rf64 {
            // F4（BS.2088-2 §2.4）：32 位长度字段不是 0xFFFFFFFF 时用这个 32 位值；是 0xFFFFFFFF
            // 时用 ds64 的 64 位值（§3.2 要求 BW64 头写 0xFFFFFFFF）。读到 ds64 之前不以 RIFF 长度
            // 为界。
            self.riff = Riff::Open;
            if size != LENGTH_PLACEHOLDER {
                self.riff_from_32 = true;
                self.riff_declared = u64::from(size);
                self.riff = Riff::Bounded(
                    u64::from(size)
                        .checked_sub(4)
                        .ok_or(DecodeError::Malformed)?,
                );
            }
            return Ok(());
        }
        self.riff_declared = u64::from(size);
        if size == LENGTH_PLACEHOLDER {
            // R1：流式写入程序未回填的占位值。
            self.riff_placeholder = true;
            self.riff = Riff::Open;
        } else {
            self.riff = Riff::Bounded(
                u64::from(size)
                    .checked_sub(4)
                    .ok_or(DecodeError::Malformed)?,
            );
        }
        Ok(())
    }

    fn ds64_fail(&mut self, p: Ds64Problem, e: DecodeError) -> DecodeError {
        self.detail = Some(WavDetail::Ds64(p));
        e
    }

    /// RF64 / BW64 中长度字段为 0xFFFFFFFF 的非 data 块：按块标识取 `ds64` 表中第一个未用过的同名表项。
    fn table_size(&mut self, id: [u8; 4]) -> Result<u64, DecodeError> {
        let len = self.table_len;
        if let Some(e) = self.table[..len].iter_mut().find(|e| e.id == id && !e.used) {
            e.used = true;
            return Ok(e.size);
        }
        if self.table_declared as usize > len {
            return Err(self.ds64_fail(
                Ds64Problem::TableCapacity {
                    entries: self.table_declared,
                    capacity: DS64_TABLE as u32,
                },
                DecodeError::CapacityExceeded,
            ));
        }
        Err(self.ds64_fail(
            Ds64Problem::NoTableEntry { chunk: id },
            DecodeError::Malformed,
        ))
    }

    /// 解析 chunk 头并进入对应状态。以 RIFF 长度为界时，`size` 和填充都必须落在 RIFF 范围内，
    /// 仅当 RIFF 恰好在 chunk 体末尾结束时容忍缺失的最后一个填充字节；例外（W1）：data 越出 RIFF
    /// （R3）、data 长度为占位值（R2），以及 data 读完之后的块越出 RIFF（R3），此后不再以 RIFF
    /// 长度为界。RF64 / BW64（F4）：第一个块必须是 ds64；长度字段为 0xFFFFFFFF 时换成 ds64 中的
    /// 64 位长度。
    fn parse_chunk_header(&mut self, b: &[u8]) -> Result<(), DecodeError> {
        self.take_riff(8)?;
        let id = [b[0], b[1], b[2], b[3]];
        let size32 = u32_at(b, 4);
        let is_data = &id == b"data";
        let rf64 = self.container == Container::Rf64;
        if rf64 {
            match (self.ds64.is_some(), &id) {
                (false, b"ds64") => {}
                (false, _) => {
                    return Err(
                        self.ds64_fail(Ds64Problem::Missing { first: id }, DecodeError::Malformed)
                    );
                }
                (true, b"ds64") => {
                    return Err(self.ds64_fail(Ds64Problem::Duplicate, DecodeError::Malformed));
                }
                (true, _) => {}
            }
        }
        // data 长度为占位值（R2）：RIFF 为 0xFFFFFFFF；RF64 为 0xFFFFFFFF 且 ds64 的 RIFF 与 data
        // 长度都是 0（流式写入程序未回填 ds64，F4）。
        let mut placeholder = false;
        // BS.2088-2 §2.4：32 位长度不是 0xFFFFFFFF 时用这个 32 位值（即使与 ds64 不同），是时用
        // ds64 的 64 位值。
        let size: u64 = match self.ds64 {
            Some(ds) if is_data && size32 == LENGTH_PLACEHOLDER => {
                placeholder = ds.data == 0 && self.riff_placeholder;
                ds.data
            }
            Some(_) if size32 == LENGTH_PLACEHOLDER => self.table_size(id)?,
            _ => {
                placeholder = is_data && size32 == LENGTH_PLACEHOLDER;
                u64::from(size32)
            }
        };
        let pad = match self.riff {
            Riff::Open => size & 1,
            Riff::Bounded(r) => match r.checked_sub(size) {
                Some(after_body) => (size & 1).min(after_body),
                None if is_data || self.data_done => {
                    // R2 的占位值本身就说明 RIFF 长度不可用，不另报 RIFF 不符。
                    if !placeholder {
                        self.riff_short = true;
                    }
                    self.riff = Riff::Open;
                    size & 1
                }
                None => return Err(DecodeError::Malformed),
            },
        };
        self.state = match &id {
            b"ds64" if rf64 => {
                if size < DS64_FIXED {
                    return Err(
                        self.ds64_fail(Ds64Problem::TooShort { size }, DecodeError::Malformed)
                    );
                }
                State::Ds64 { size, pad }
            }
            b"fmt " => {
                if self.layout.is_some() {
                    return Err(DecodeError::Malformed);
                }
                if size < u64::from(FMT_BASE_LEN) {
                    return Err(DecodeError::Malformed);
                }
                State::Fmt { size, pad }
            }
            b"fact" if size >= 4 => State::Fact { size, pad },
            b"data" => {
                let Some(layout) = self.layout else {
                    return Err(DecodeError::Malformed);
                };
                if self.data_seen {
                    return Err(DecodeError::Malformed);
                }
                self.data_seen = true;
                if layout.encoding.is_float() {
                    self.check_fact(&layout, size, placeholder)?;
                }
                self.data_declared = if rf64 { size } else { u64::from(size32) };
                if placeholder {
                    // R2：data 延伸到源末尾。
                    self.riff = Riff::Open;
                    self.data_placeholder_read = 0;
                    State::Data {
                        declared: size,
                        remaining: 0,
                        tail: 0,
                        pad: 0,
                        to_eof: true,
                    }
                } else {
                    if size == 0 && self.riff_placeholder {
                        // RIFF 为占位值而 data 为 0：可能是未回填的占位，也可能真的为空，意图不明。
                        return Err(DecodeError::Malformed);
                    }
                    // R4：不足一帧的尾部在完整帧之后丢弃。
                    let tail = size % layout.block_align as u64;
                    self.declared_frames = Some((size - tail) / layout.block_align as u64);
                    State::Data {
                        declared: size,
                        remaining: size - tail,
                        tail,
                        pad,
                        to_eof: false,
                    }
                }
            }
            _ => State::Skip {
                remaining: size.checked_add(pad).ok_or(DecodeError::Malformed)?,
            },
        };
        Ok(())
    }

    /// ds64 的固定部分（28 字节，F4）：记下三个 64 位长度，以 ds64 的 RIFF 长度为界（为 0 时是
    /// 流式写入程序的占位，同 W1 R1）。`size` 为 ds64 体长度。
    fn parse_ds64(&mut self, b: &[u8], size: u64) -> Result<u32, DecodeError> {
        let u64_at = |i: usize| u64::from(u32_at(b, i)) | (u64::from(u32_at(b, i + 4)) << 32);
        let ds = Ds64 {
            riff: u64_at(0),
            data: u64_at(8),
            samples: u64_at(16),
        };
        let entries = u32_at(b, 24);
        if u64::from(entries) * DS64_ENTRY > size - DS64_FIXED {
            return Err(self.ds64_fail(
                Ds64Problem::TableOverrun { entries, size },
                DecodeError::Malformed,
            ));
        }
        if self.riff_from_32 {
            // RIFF 长度来自 32 位字段（§2.4），ds64 的 bw64Size 不用；扣除 ds64 固定部分。
            self.take_riff(DS64_FIXED)?;
        } else if ds.riff == 0 {
            self.riff_declared = 0;
            self.riff_placeholder = true;
            self.riff = Riff::Open;
        } else {
            // RIFF 体从“WAVE”起：已消耗 “WAVE”、ds64 块头与固定部分。
            let left = ds.riff.checked_sub(4 + 8 + DS64_FIXED).ok_or_else(|| {
                self.ds64_fail(
                    Ds64Problem::RiffTooSmall { riff: ds.riff },
                    DecodeError::Malformed,
                )
            })?;
            self.riff_declared = ds.riff;
            self.riff = Riff::Bounded(left);
        }
        self.ds64 = Some(ds);
        self.table_declared = entries;
        Ok(entries)
    }

    /// 浮点文件的 `fact` 核对（F3）：data 之前没有 `fact` 时照常（帧数按 data 长度）；有则只能有一个，
    /// 声明的帧数等于 data 的完整帧数。data 长度为占位值（R2）时无法核对。
    fn check_fact(
        &mut self,
        layout: &Layout,
        size: u64,
        placeholder: bool,
    ) -> Result<(), DecodeError> {
        let Some(declared) = self.fact else {
            return Ok(());
        };
        let frames = (!placeholder).then(|| size / layout.block_align as u64);
        if !self.fact_duplicate && frames == Some(declared) {
            return Ok(());
        }
        self.detail = Some(WavDetail::Fact {
            declared,
            frames,
            duplicate: self.fact_duplicate,
        });
        Err(DecodeError::Malformed)
    }

    fn parse_fmt(&mut self, b: &[u8], size: u64) -> Result<Layout, DecodeError> {
        let tag = u16_at(b, 0);
        let channels = u16_at(b, 2);
        let rate = u32_at(b, 4);
        let block_align = u16_at(b, 12);
        let bits = u16_at(b, 14);

        if channels == 0 || rate == 0 || block_align == 0 || bits == 0 {
            return Err(DecodeError::Malformed);
        }
        let (container_bits, valid_bits, channel_mask, float) = match tag {
            FORMAT_PCM => {
                if !bits.is_multiple_of(8) {
                    return Err(DecodeError::Unsupported);
                }
                (bits, bits, None, false)
            }
            FORMAT_IEEE_FLOAT => (bits, bits, None, true),
            FORMAT_EXTENSIBLE => {
                if size < u64::from(FMT_EXT_LEN) {
                    return Err(DecodeError::Malformed);
                }
                let cb_size = u16_at(b, 16);
                if u32::from(cb_size) < FMT_EXT_LEN - 18 || u64::from(cb_size) + 18 > size {
                    return Err(DecodeError::Malformed);
                }
                if !bits.is_multiple_of(8) {
                    return Err(DecodeError::Malformed);
                }
                let valid = u16_at(b, 18);
                if valid == 0 || valid > bits {
                    return Err(DecodeError::Malformed);
                }
                let mask = u32_at(b, 20);
                let float = if b[24..40] == SUBTYPE_PCM {
                    false
                } else if b[24..40] == SUBTYPE_IEEE_FLOAT {
                    // 浮点没有“容器内的有效位”：有效位必须等于容器。
                    if valid != bits {
                        return Err(DecodeError::Malformed);
                    }
                    true
                } else {
                    return Err(DecodeError::Unsupported);
                };
                (bits, valid, Some(mask), float)
            }
            _ => return Err(DecodeError::Unsupported),
        };
        let encoding = match (float, container_bits) {
            (false, 8 | 16 | 24 | 32) => SampleEncoding::Int,
            (true, 32) => SampleEncoding::Float32,
            (true, 64) => SampleEncoding::Float64,
            _ => return Err(DecodeError::Unsupported),
        };
        let container_bytes = container_bits / 8;
        let expected_align = channels
            .checked_mul(container_bytes)
            .ok_or(DecodeError::Malformed)?;
        if block_align != expected_align {
            return Err(DecodeError::Malformed);
        }
        // 声道布局（F1，Microsoft WAVEFORMATEXTENSIBLE：样本按 dwChannelMask 位从低到高排列）。
        // 单/双声道：格式码 1 或掩码为 0 时按 [FC] / [FL FR]。多于 2 声道必须由掩码完整声明
        // 每个声道的位置（置位数等于声道数、不含第 18 位及以上），否则布局未定义，拒绝。
        // 采样率不设软件上限。
        if let Some(mask) = channel_mask
            && mask.count_ones() > u32::from(channels)
        {
            return Err(DecodeError::Malformed);
        }
        let layout = match channel_mask {
            None | Some(0) => ChannelLayout::default_for(channels),
            Some(m) => ChannelLayout::from_mask(m).filter(|l| l.channels() == channels),
        };
        let Some(layout) = layout else {
            self.detail = Some(WavDetail::UndefinedLayout {
                channels,
                mask: channel_mask,
            });
            return Err(DecodeError::Unsupported);
        };
        let format = match encoding {
            SampleEncoding::Int => SourceFormat::with_layout(rate, layout, valid_bits as u8),
            _ => SourceFormat::float(rate, layout, encoding),
        }
        .map_err(|_| DecodeError::Unsupported)?;
        Ok(Layout {
            format,
            container_bytes: usize::from(container_bytes),
            shift: u32::from(container_bits - valid_bits),
            block_align: usize::from(block_align),
            channels: usize::from(channels),
            channel_mask,
            encoding,
        })
    }

    /// 将完整帧转换为右对齐 `i32` 写入工作区（浮点按位存放），返回写入的 `i32` 个数。
    fn convert(&mut self, layout: &Layout, src: &[u8]) -> usize {
        if layout.encoding.is_float() {
            // binary32：每样本一个字；binary64：每样本两个字（先低 32 位、后高 32 位，即小端
            // 字节序的前 4 字节与后 4 字节）。都是按位重解释，不做数值运算。
            let words = src.len() / 4;
            for (o, c) in self.pcm[..words].iter_mut().zip(src.chunks_exact(4)) {
                *o = i32::from_le_bytes([c[0], c[1], c[2], c[3]]);
            }
            return words;
        }
        let samples = src.len() / layout.container_bytes;
        let out = &mut self.pcm[..samples];
        match layout.container_bytes {
            // 8 位为无符号、偏移 128（WAVE 规范）：减去 128 得到有符号值，再去掉低位填充。
            1 => {
                for (o, &c) in out.iter_mut().zip(src) {
                    *o = (i32::from(c) - 128) >> layout.shift;
                }
            }
            2 => {
                for (o, c) in out.iter_mut().zip(src.chunks_exact(2)) {
                    *o = i32::from(i16::from_le_bytes([c[0], c[1]])) >> layout.shift;
                }
            }
            3 => {
                for (o, c) in out.iter_mut().zip(src.chunks_exact(3)) {
                    *o = i32::from_le_bytes([0, c[0], c[1], c[2]]) >> (8 + layout.shift);
                }
            }
            _ => {
                for (o, c) in out.iter_mut().zip(src.chunks_exact(4)) {
                    *o = i32::from_le_bytes([c[0], c[1], c[2], c[3]]) >> layout.shift;
                }
            }
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
                State::RiffHeader => {
                    if avail.len() < 12 {
                        return self.need_input(pos, eof);
                    }
                    self.parse_riff_header(&avail[..12])?;
                    pos += 12;
                    self.state = State::ChunkHeader;
                }
                State::ChunkHeader => match self.riff {
                    Riff::Bounded(0) => {
                        if !self.data_seen {
                            return Err(DecodeError::Malformed);
                        }
                        self.state = State::End;
                    }
                    Riff::Bounded(r) => {
                        // R3：RIFF 声明的比源实际的长，data 已读完：源在此结束即为正常结束。
                        // RIFF 余下不足一个块头时，要等到余下的字节到齐或源结束才能区分
                        // “RIFF 内的残余字节”（拒绝）与“RIFF 声明过长”（R3），结果与输入分块无关。
                        if self.data_done && (avail.len() as u64) < r.min(8) {
                            if eof {
                                return self.end_at_eof(pos, avail.len());
                            }
                            return self.need_input(pos, eof);
                        }
                        if r < 8 {
                            return Err(DecodeError::Malformed);
                        }
                        if avail.len() < 8 {
                            return self.need_input(pos, eof);
                        }
                        self.parse_chunk_header(&avail[..8])?;
                        pos += 8;
                        self.note_data_start(pos);
                    }
                    Riff::Open => {
                        if avail.len() < 8 {
                            if !eof {
                                return self.need_input(pos, eof);
                            }
                            if self.data_done {
                                // data 之后不足一个块头的尾部字节：不影响样本。
                                return self.end_at_eof(pos, avail.len());
                            }
                            return Err(if !self.data_seen && avail.is_empty() {
                                DecodeError::Malformed
                            } else {
                                DecodeError::Truncated
                            });
                        }
                        self.parse_chunk_header(&avail[..8])?;
                        pos += 8;
                        self.note_data_start(pos);
                    }
                },
                State::Fmt { size, pad } => {
                    let want = size.min(u64::from(FMT_EXT_LEN));
                    if (avail.len() as u64) < want {
                        return self.need_input(pos, eof);
                    }
                    let layout = self.parse_fmt(&avail[..want as usize], size)?;
                    self.layout = Some(layout);
                    self.take_riff(want)?;
                    pos += want as usize;
                    self.state = State::Skip {
                        remaining: (size - want)
                            .checked_add(pad)
                            .ok_or(DecodeError::Malformed)?,
                    };
                }
                State::Fact { size, pad } => {
                    if avail.len() < 4 {
                        return self.need_input(pos, eof);
                    }
                    if self.fact.is_some() {
                        self.fact_duplicate = true;
                    } else {
                        let v = u32_at(avail, 0);
                        // F4：RF64 的 fact 为 0xFFFFFFFF 时帧数见 ds64 的第三个 64 位值（EBU Tech 3306
                        // v1 的 sampleCount）。BW64 中这个值是 dummy，“读取时必须忽略”（BS.2088-2
                        // §4.2），fact 按 32 位原值核对。
                        self.fact = Some(match self.ds64 {
                            Some(ds) if v == LENGTH_PLACEHOLDER && &self.magic == b"RF64" => {
                                ds.samples
                            }
                            _ => u64::from(v),
                        });
                    }
                    self.take_riff(4)?;
                    pos += 4;
                    self.state = State::Skip {
                        remaining: (size - 4).checked_add(pad).ok_or(DecodeError::Malformed)?,
                    };
                }
                State::Ds64 { size, pad } => {
                    if avail.len() < DS64_FIXED as usize {
                        return self.need_input(pos, eof);
                    }
                    let entries = self.parse_ds64(&avail[..DS64_FIXED as usize], size)?;
                    pos += DS64_FIXED as usize;
                    self.state = State::Ds64Table {
                        entries,
                        rest: size - DS64_FIXED - u64::from(entries) * DS64_ENTRY,
                        pad,
                    };
                }
                State::Ds64Table { entries, rest, pad } => {
                    if entries == 0 {
                        self.state = State::Skip {
                            remaining: rest.checked_add(pad).ok_or(DecodeError::Malformed)?,
                        };
                        continue;
                    }
                    if avail.len() < DS64_ENTRY as usize {
                        return self.need_input(pos, eof);
                    }
                    if self.table_len < DS64_TABLE {
                        let lo = u64::from(u32_at(avail, 4));
                        let hi = u64::from(u32_at(avail, 8));
                        self.table[self.table_len] = TableEntry {
                            id: [avail[0], avail[1], avail[2], avail[3]],
                            size: lo | (hi << 32),
                            used: false,
                        };
                        self.table_len += 1;
                    }
                    self.take_riff(DS64_ENTRY)?;
                    pos += DS64_ENTRY as usize;
                    self.state = State::Ds64Table {
                        entries: entries - 1,
                        rest,
                        pad,
                    };
                }
                State::Skip { remaining } => {
                    if remaining == 0 {
                        self.state = State::ChunkHeader;
                        continue;
                    }
                    if avail.is_empty() {
                        if eof && self.data_done {
                            // R3：data 之后的块（或其填充）被源截断，不影响样本。
                            return self.end_at_eof(pos, 0);
                        }
                        return self.need_input(pos, eof);
                    }
                    let n = remaining.min(avail.len() as u64);
                    self.take_riff(n)?;
                    pos += n as usize;
                    self.state = State::Skip {
                        remaining: remaining - n,
                    };
                }
                State::Data {
                    declared,
                    remaining,
                    tail,
                    pad,
                    to_eof,
                } => {
                    if remaining == 0 && !to_eof {
                        self.data_done = true;
                        if tail > 0 {
                            // 不足一帧，小于块对齐（16 位），放得进 u32。
                            self.partial_dropped = u32::try_from(tail).unwrap_or(u32::MAX);
                        }
                        self.state = State::Skip {
                            remaining: tail.checked_add(pad).ok_or(DecodeError::Malformed)?,
                        };
                        continue;
                    }
                    let Some(layout) = self.layout else {
                        return Err(DecodeError::Malformed);
                    };
                    // 16 位双声道：输出任何样本之前，先确认 data 开头不是伪装成 PCM 的压缩码流。
                    // 等到 BITSTREAM_SCAN 字节（或 data 结束、源结束）到齐再判断，结果与输入分块无关。
                    if !self.scanned {
                        if layout.container_bytes == 2 && layout.channels == 2 && layout.shift == 0
                        {
                            let need = if to_eof {
                                BITSTREAM_SCAN
                            } else {
                                usize::try_from(remaining)
                                    .map_or(BITSTREAM_SCAN, |r| BITSTREAM_SCAN.min(r))
                            };
                            if avail.len() < need && !eof {
                                return self.need_input(pos, eof);
                            }
                            let window = &avail[..avail.len().min(need)];
                            if let Some((kind, at)) = detect_bitstream(window) {
                                self.detail = Some(WavDetail::Bitstream {
                                    kind,
                                    offset: u32::try_from(at).unwrap_or(u32::MAX),
                                });
                                return Err(DecodeError::Unsupported);
                            }
                        }
                        self.scanned = true;
                    }
                    let ba = layout.block_align;
                    let limit = if to_eof {
                        usize::MAX
                    } else {
                        // 32 位目标上超过 usize 的帧数按 usize 上限取（本次调用只取到工作区为止）。
                        usize::try_from(remaining / ba as u64).unwrap_or(usize::MAX)
                    };
                    let frames = (avail.len() / ba)
                        .min(limit)
                        .min(self.pcm.len() / layout.words_per_frame());
                    if frames == 0 {
                        if !eof {
                            return self.need_input(pos, eof);
                        }
                        if to_eof {
                            // R2：源末尾不足一帧的字节丢弃（R4）。
                            self.data_done = true;
                            if let Ok(rest) = u32::try_from(avail.len())
                                && rest > 0
                            {
                                self.partial_dropped = rest;
                            }
                            return self.end_at_eof(pos, avail.len());
                        }
                        // R5：源比 data 声明的短。完整帧已全部输出。
                        let present = avail.len() as u64;
                        let missing = remaining.saturating_add(tail).saturating_sub(present);
                        self.detail = Some(WavDetail::DataShort { declared, missing });
                        return Err(DecodeError::Truncated);
                    }
                    let bytes = frames * ba;
                    let samples = self.convert(&layout, &avail[..bytes]);
                    let bytes64 = bytes as u64;
                    self.take_riff(bytes64)?;
                    pos += bytes;
                    self.start = 0;
                    self.end = samples;
                    if to_eof {
                        self.data_placeholder_read += bytes as u64;
                    }
                    self.state = State::Data {
                        declared,
                        remaining: if to_eof { 0 } else { remaining - bytes64 },
                        tail,
                        pad,
                        to_eof,
                    };
                    return Ok(Progress {
                        consumed: pos,
                        status: Status::Frames(frames),
                    });
                }
            }
        }
    }
}

/// WAV 是流式解码，资源需求固定（PCM 工作区与核心最小窗口），没有文件头阶段的容量拒绝；
/// 结构修正（W1）经本接口报告。
impl qy_core::engine::Resources for WavDecoder {
    fn repair_count(&self) -> usize {
        self.repairs().count()
    }

    fn repair(&self, index: usize) -> Option<Repair> {
        self.repairs().nth(index)
    }
}

impl Decoder for WavDecoder {
    fn reset(&mut self) {
        self.state = State::RiffHeader;
        self.riff = Riff::Bounded(0);
        self.magic = *b"RIFF";
        self.container = Container::Riff;
        self.ds64 = None;
        self.table = [EMPTY_ENTRY; DS64_TABLE];
        self.table_len = 0;
        self.table_declared = 0;
        self.riff_declared = 0;
        self.riff_from_32 = false;
        self.data_declared = 0;
        self.riff_placeholder = false;
        self.riff_short = false;
        self.layout = None;
        self.detail = None;
        self.data_seen = false;
        self.data_done = false;
        self.scanned = false;
        self.data_placeholder_read = NONE;
        self.riff_actual = NONE;
        self.partial_dropped = 0;
        self.declared_frames = None;
        self.fact = None;
        self.fact_duplicate = false;
        self.offset = 0;
        self.data_start = NONE;
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

    fn pcm(&self) -> &[i32] {
        &self.pcm[self.start..self.end]
    }

    fn seek(&mut self, frame: u64) -> Result<u64, SeekError> {
        self.seek_to(frame)
    }

    fn release(&mut self, frames: usize) {
        let words = self.layout.map_or(1, |l| l.words_per_frame());
        let samples = frames.saturating_mul(words);
        self.start = self.start.saturating_add(samples).min(self.end);
        if self.start == self.end {
            self.start = 0;
            self.end = 0;
        }
    }
}

/// M2 T5：Kani 有界证明（`cargo kani -p qy-codec-wav`）。文件头的解析函数在任意字节、任意长度
/// 字段下不 panic：没有整数溢出、越界索引或除零（Kani 默认检查这些）；成功时结果自洽。
#[cfg(kani)]
mod proofs {
    extern crate alloc;
    use super::*;
    use alloc::boxed::Box;
    use alloc::vec;

    fn decoder() -> WavDecoder {
        let pcm = Box::leak(vec![0i32; PCM_WORK_SAMPLES].into_boxed_slice());
        match WavDecoder::new(pcm) {
            Ok(d) => d,
            Err(_) => panic!(),
        }
    }

    /// RIFF 头 12 字节为任意值。
    #[kani::proof]
    fn riff_header_any_bytes() {
        let mut d = decoder();
        let b: [u8; 12] = kani::any();
        let _ = d.parse_riff_header(&b);
    }

    /// RIFF 头之后的块头 8 字节为任意值（RIFF 剩余长度、是否已有 fmt/data 也任意）。
    #[kani::proof]
    fn chunk_header_any_bytes() {
        let mut d = decoder();
        let riff: [u8; 12] = kani::any();
        if d.parse_riff_header(&riff).is_err() {
            return;
        }
        if kani::any() {
            d.data_done = true;
        }
        let b: [u8; 8] = kani::any();
        let _ = d.parse_chunk_header(&b);
    }

    /// F4：RF64 文件头之后的块头与 ds64 固定部分为任意值（ds64 声明长度也任意）；成功时 RIFF
    /// 余量与表项数自洽。之后一个任意块头（长度可能为 0xFFFFFFFF，需要查表或取 ds64）也不 panic。
    #[kani::proof]
    fn ds64_any_bytes() {
        let mut d = decoder();
        if d.parse_riff_header(b"RF64\xff\xff\xff\xffWAVE").is_err() {
            panic!();
        }
        let head: [u8; 8] = kani::any();
        if d.parse_chunk_header(&head).is_err() {
            return;
        }
        let State::Ds64 { size, .. } = d.state else {
            return;
        };
        let b: [u8; 28] = kani::any();
        if let Ok(entries) = d.parse_ds64(&b, size) {
            assert!(u64::from(entries) * DS64_ENTRY <= size - DS64_FIXED);
            let next: [u8; 8] = kani::any();
            let _ = d.parse_chunk_header(&next);
        }
    }

    /// fmt 体 40 字节与声明长度（16–40 之外也任意）为任意值；成功时块对齐、容器字节数与声道数
    /// 一致，右移量小于 32。
    #[kani::proof]
    fn fmt_any_fields() {
        let mut d = decoder();
        let b: [u8; 40] = kani::any();
        let size: u64 = kani::any();
        kani::assume(size >= u64::from(FMT_BASE_LEN));
        let want = size.min(u64::from(FMT_EXT_LEN)) as usize;
        if let Ok(l) = d.parse_fmt(&b[..want], size) {
            assert!(l.block_align == l.container_bytes * l.channels);
            assert!(l.shift < 32);
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::boxed::Box;
    use std::vec;

    #[test]
    fn workspace_capacity_checked() {
        let small = Box::leak(vec![0i32; PCM_WORK_SAMPLES - 1].into_boxed_slice());
        assert!(matches!(
            WavDecoder::new(small),
            Err(DecodeError::CapacityExceeded)
        ));
        let ok = Box::leak(vec![0i32; PCM_WORK_SAMPLES].into_boxed_slice());
        assert_eq!(
            WavDecoder::new(ok).map(|d| d.capacity_samples()).ok(),
            Some(8192)
        );
    }

    #[test]
    fn subtype_pcm_bytes_match_guid() {
        // {00000001-0000-0010-8000-00AA00389B71}：Data1/2/3 小端，Data4 原序。
        assert_eq!(&SUBTYPE_PCM[0..4], &1u32.to_le_bytes());
        assert_eq!(&SUBTYPE_PCM[4..6], &0u16.to_le_bytes());
        assert_eq!(&SUBTYPE_PCM[6..8], &0x0010u16.to_le_bytes());
        assert_eq!(
            &SUBTYPE_PCM[8..],
            &[0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71]
        );
    }
}
