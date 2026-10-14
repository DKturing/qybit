//! QyBit 自有推送式 WAV 解码器。
//!
//! `#![no_std]`、无 alloc；PCM 工作区由宿主以 `&'static mut [i32]` 提供。
//! 解码器不缓存输入：每次调用只看宿主传入的切片，返回实际消耗字节数，
//! 未消耗的字节由宿主（核心输入窗口）保留到下次调用（AGENTS §2）。
//! 任一结构所需的最大连续输入为 40 字节（fmt 前缀）或一帧（≤ 8 字节），
//! 因此任意输入分块都能推进。
//!
//! 支持范围：
//! - RIFF/WAVE，小端；格式码 1（PCM）与 0xFFFE（EXTENSIBLE，子格式 PCM）。
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
//! 意图不明的照旧拒绝：fmt 字段互相矛盾、data 在 fmt 之前、重复的 fmt/data、data 之前的块越出
//! RIFF、RIFF 为占位值而 data 长度为 0（可能是未回填的占位，也可能真的为空）。
//!
//! 错误分类：结构违反 RIFF/WAVE 规范为 [`DecodeError::Malformed`]；
//! 合法但不支持（浮点、RIFX/RF64 等）为 [`DecodeError::Unsupported`]；
//! 源在完整结构前结束为 [`DecodeError::Truncated`]。
//! 出错后解码器保持该错误，直到 [`Decoder::reset`]。
//!
//! 规范出处：Microsoft “Multimedia Programming Interface and Data
//! Specifications 1.0”（RIFF chunk 与 WORD 对齐填充）；
//! `WAVEFORMATEX`、`WAVEFORMATEXTENSIBLE`（mmreg.h / ksmedia.h，
//! 有效位、声道掩码、`KSDATAFORMAT_SUBTYPE_PCM`）。
#![no_std]

use qy_core::engine::{MAX_REPAIRS, Repair};
use qy_core::{
    ChannelLayout, DecodeError, Decoder, PCM_WORK_SAMPLES, Progress, SourceFormat, Status,
};

/// 流式写入程序的长度占位值（ffmpeg 输出到不可定位的管道时，RIFF 与 data 都写 0xFFFFFFFF；
/// 见 tasks/W1.md 与 docs/w1-wav-tolerance.md 的样例核对）。
const LENGTH_PLACEHOLDER: u32 = 0xFFFF_FFFF;

/// 64 位计数的“无”（源长度不可能达到）。修正状态用哨兵值而不是 `Option`，保持解码器结构紧凑。
const NONE: u64 = u64::MAX;

const FORMAT_PCM: u16 = 0x0001;
const FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// `KSDATAFORMAT_SUBTYPE_PCM` = {00000001-0000-0010-8000-00AA00389B71} 的磁盘字节序。
const SUBTYPE_PCM: [u8; 16] = [
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
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
        size: u32,
        pad: u32,
    },
    Skip {
        remaining: u32,
    },
    /// `remaining` 为尚未读取的完整帧字节；`tail` 为声明长度中不足一帧的尾部（R4）；
    /// `to_eof` 时 data 延伸到源末尾（R2），`remaining` 不用。
    Data {
        declared: u32,
        remaining: u32,
        tail: u32,
        pad: u32,
        to_eof: bool,
    },
    End,
    Failed(DecodeError),
}

/// RIFF 长度的用法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Riff {
    /// 以 RIFF 长度为界：“WAVE” 之后尚未消耗的字节数。
    Bounded(u32),
    /// RIFF 长度不可用（占位值或与实际结构不符）：以各块的声明长度为准，读到源末尾。
    Open,
}

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
}

/// 拒绝的明细（宿主报告用；`DecodeError` 只给类别）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WavDetail {
    /// 多于 2 声道但没有完整声明扬声器布局：格式码 1（`mask` 为 `None`）、EXTENSIBLE 掩码为 0、
    /// 置位数少于声道数，或含无扬声器位置的位（F1）。
    UndefinedLayout { channels: u16, mask: Option<u32> },
    /// data 声明长度超过源的实际长度（W1 R5）：完整帧已全部输出，源比声明短 `missing` 字节。
    DataShort { declared: u32, missing: u64 },
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
        }
    }
}

/// WAV 解码器。
pub struct WavDecoder {
    pcm: &'static mut [i32],
    state: State,
    riff: Riff,
    /// RIFF 头中声明的长度。
    riff_declared: u32,
    /// RIFF 长度为占位值（R1）。
    riff_placeholder: bool,
    /// data 越出 RIFF（R3，RIFF 偏短），结束时报告。
    riff_short: bool,
    layout: Option<Layout>,
    detail: Option<WavDetail>,
    data_seen: bool,
    /// data 的完整帧已全部读完（之后源的结构问题不影响样本）。
    data_done: bool,
    /// data 长度为占位值（R2）时读到的字节数；[`NONE`] 表示不是占位值。
    data_placeholder_read: u64,
    /// 源在 data 读完后结束、RIFF 长度需要修正（R1/R3）时，RIFF 体实际的字节数；否则 [`NONE`]。
    riff_actual: u64,
    /// 丢弃的不足一帧的尾部字节数（R4）；0 表示没有丢弃。
    partial_dropped: u32,
    /// 已消耗的总字节数（用于报告源的实际长度）。
    offset: u64,
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
            riff_declared: 0,
            riff_placeholder: false,
            riff_short: false,
            layout: None,
            detail: None,
            data_seen: false,
            data_done: false,
            data_placeholder_read: NONE,
            riff_actual: NONE,
            partial_dropped: 0,
            offset: 0,
            start: 0,
            end: 0,
        })
    }

    /// 工作区容量（样本数）。
    pub fn capacity_samples(&self) -> usize {
        self.pcm.len()
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
                    chunk: *b"RIFF",
                    declared: self.riff_declared,
                    read: actual,
                }
            } else {
                Repair::LengthMismatch {
                    chunk: *b"RIFF",
                    declared: self.riff_declared,
                    actual,
                }
            }
        });
        let data = (self.data_placeholder_read != NONE && self.data_done).then_some(
            Repair::LengthPlaceholder {
                chunk: *b"data",
                declared: LENGTH_PLACEHOLDER,
                read: self.data_placeholder_read,
            },
        );
        [partial, riff, data, None]
    }

    /// EXTENSIBLE 的声道掩码；格式码 1 或头部未解析时为 `None`。
    pub fn channel_mask(&self) -> Option<u32> {
        self.layout.and_then(|l| l.channel_mask)
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
    fn take_riff(&mut self, n: u32) -> Result<(), DecodeError> {
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
        match &b[0..4] {
            b"RIFF" => {}
            b"RIFX" | b"RF64" | b"BW64" => return Err(DecodeError::Unsupported),
            _ => return Err(DecodeError::Malformed),
        }
        if &b[8..12] != b"WAVE" {
            return Err(DecodeError::Malformed);
        }
        let size = u32_at(b, 4);
        self.riff_declared = size;
        if size == LENGTH_PLACEHOLDER {
            // R1：流式写入程序未回填的占位值。
            self.riff_placeholder = true;
            self.riff = Riff::Open;
        } else {
            self.riff = Riff::Bounded(size.checked_sub(4).ok_or(DecodeError::Malformed)?);
        }
        Ok(())
    }

    /// 解析 chunk 头并进入对应状态。以 RIFF 长度为界时，`size` 和填充都必须落在 RIFF 范围内，
    /// 仅当 RIFF 恰好在 chunk 体末尾结束时容忍缺失的最后一个填充字节；例外（W1）：data 越出 RIFF
    /// （R3）、data 长度为占位值（R2），以及 data 读完之后的块越出 RIFF（R3），此后不再以 RIFF
    /// 长度为界。
    fn parse_chunk_header(&mut self, b: &[u8]) -> Result<(), DecodeError> {
        self.take_riff(8)?;
        let id = [b[0], b[1], b[2], b[3]];
        let size = u32_at(b, 4);
        let is_data = &id == b"data";
        let pad = match self.riff {
            Riff::Open => size & 1,
            Riff::Bounded(r) => match r.checked_sub(size) {
                Some(after_body) => (size & 1).min(after_body),
                None if is_data || self.data_done => {
                    // R2 的占位值本身就说明 RIFF 长度不可用，不另报 RIFF 不符。
                    if !(is_data && size == LENGTH_PLACEHOLDER) {
                        self.riff_short = true;
                    }
                    self.riff = Riff::Open;
                    size & 1
                }
                None => return Err(DecodeError::Malformed),
            },
        };
        self.state = match &id {
            b"fmt " => {
                if self.layout.is_some() {
                    return Err(DecodeError::Malformed);
                }
                if size < FMT_BASE_LEN {
                    return Err(DecodeError::Malformed);
                }
                State::Fmt { size, pad }
            }
            b"data" => {
                let Some(layout) = self.layout else {
                    return Err(DecodeError::Malformed);
                };
                if self.data_seen {
                    return Err(DecodeError::Malformed);
                }
                self.data_seen = true;
                if size == LENGTH_PLACEHOLDER {
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
                    let tail = u32::try_from(size as usize % layout.block_align)
                        .map_err(|_| DecodeError::Malformed)?;
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

    fn parse_fmt(&mut self, b: &[u8], size: u32) -> Result<Layout, DecodeError> {
        let tag = u16_at(b, 0);
        let channels = u16_at(b, 2);
        let rate = u32_at(b, 4);
        let block_align = u16_at(b, 12);
        let bits = u16_at(b, 14);

        if channels == 0 || rate == 0 || block_align == 0 || bits == 0 {
            return Err(DecodeError::Malformed);
        }
        let (container_bits, valid_bits, channel_mask) = match tag {
            FORMAT_PCM => {
                if bits % 8 != 0 {
                    return Err(DecodeError::Unsupported);
                }
                (bits, bits, None)
            }
            FORMAT_EXTENSIBLE => {
                if size < FMT_EXT_LEN {
                    return Err(DecodeError::Malformed);
                }
                let cb_size = u16_at(b, 16);
                if u32::from(cb_size) < FMT_EXT_LEN - 18 || u32::from(cb_size) + 18 > size {
                    return Err(DecodeError::Malformed);
                }
                if bits % 8 != 0 {
                    return Err(DecodeError::Malformed);
                }
                let valid = u16_at(b, 18);
                if valid == 0 || valid > bits {
                    return Err(DecodeError::Malformed);
                }
                let mask = u32_at(b, 20);
                if b[24..40] != SUBTYPE_PCM {
                    return Err(DecodeError::Unsupported);
                }
                (bits, valid, Some(mask))
            }
            _ => return Err(DecodeError::Unsupported),
        };
        if !matches!(container_bits, 8 | 16 | 24 | 32) {
            return Err(DecodeError::Unsupported);
        }
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
        let format = SourceFormat::with_layout(rate, layout, valid_bits as u8)
            .map_err(|_| DecodeError::Unsupported)?;
        Ok(Layout {
            format,
            container_bytes: usize::from(container_bytes),
            shift: u32::from(container_bits - valid_bits),
            block_align: usize::from(block_align),
            channels: usize::from(channels),
            channel_mask,
        })
    }

    /// 将完整帧转换为右对齐 `i32` 写入工作区。
    fn convert(&mut self, layout: &Layout, src: &[u8]) -> usize {
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
                        if self.data_done && avail.len() < (r as usize).min(8) {
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
                    }
                },
                State::Fmt { size, pad } => {
                    let want = size.min(FMT_EXT_LEN);
                    if avail.len() < want as usize {
                        return self.need_input(pos, eof);
                    }
                    let layout = self.parse_fmt(&avail[..want as usize], size)?;
                    self.layout = Some(layout);
                    self.take_riff(want)?;
                    pos += want as usize;
                    self.state = State::Skip {
                        remaining: size - want + pad,
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
                    let n = remaining.min(u32::try_from(avail.len()).unwrap_or(u32::MAX));
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
                            self.partial_dropped = tail;
                        }
                        self.state = State::Skip {
                            remaining: tail.checked_add(pad).ok_or(DecodeError::Malformed)?,
                        };
                        continue;
                    }
                    let Some(layout) = self.layout else {
                        return Err(DecodeError::Malformed);
                    };
                    let ba = layout.block_align;
                    let limit = if to_eof {
                        usize::MAX
                    } else {
                        remaining as usize / ba
                    };
                    let frames = (avail.len() / ba)
                        .min(limit)
                        .min(self.pcm.len() / layout.channels);
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
                        let missing =
                            (u64::from(remaining) + u64::from(tail)).saturating_sub(present);
                        self.detail = Some(WavDetail::DataShort { declared, missing });
                        return Err(DecodeError::Truncated);
                    }
                    let bytes = frames * ba;
                    let samples = self.convert(&layout, &avail[..bytes]);
                    let bytes32 = u32::try_from(bytes).map_err(|_| DecodeError::Malformed)?;
                    self.take_riff(bytes32)?;
                    pos += bytes;
                    self.start = 0;
                    self.end = samples;
                    if to_eof {
                        self.data_placeholder_read += bytes as u64;
                    }
                    self.state = State::Data {
                        declared,
                        remaining: if to_eof { 0 } else { remaining - bytes32 },
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
        self.riff_declared = 0;
        self.riff_placeholder = false;
        self.riff_short = false;
        self.layout = None;
        self.detail = None;
        self.data_seen = false;
        self.data_done = false;
        self.data_placeholder_read = NONE;
        self.riff_actual = NONE;
        self.partial_dropped = 0;
        self.offset = 0;
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
