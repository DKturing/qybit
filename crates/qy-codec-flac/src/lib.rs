//! QyBit 自有推送式 FLAC 解码器（M1）。
//!
//! `#![no_std]`、无 alloc。解码器只保存宿主提供的切片与小状态，不内嵌随块大小增长的
//! 数组（AGENTS §2，M1 T1）。三类大缓冲统一为 `&'static mut [T]`：
//!
//! | 缓冲 | 类型 | 归属 |
//! | --- | --- | --- |
//! | 帧输入窗口 | `[u8]` | 核心（[`qy_core::Core::with_window`]） |
//! | 解码/预测/解相关工作区 | `[i64]` | 解码器 |
//! | 最终交错 PCM | `[i32]` | 解码器 |
//!
//! 容量只来自宿主配置（[`Capacity`]），不依据码流头部动态决定；切片不足时构造返回
//! [`InitError::CapacityExceeded`]。桌面宿主启动时 `vec![0; n]` → boxed slice → `Box::leak`
//! 一次；MCU 用 `static_cell::ConstStaticCell` 的 `try_take`（特性 `static-cell`，见 [`mcu`]）。
//! 停止、换曲与重置复用同一套缓冲。
//!
//! 帧解析见 `frame` 模块（M1 T2）。
#![no_std]

mod bits;
mod crc;
mod frame;

use qy_core::{ChannelLayout, DecodeError, Decoder, Progress, SourceFormat, Status};

/// 解码器容量配置（宿主声明）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    /// 支持的最大块大小（帧/声道）。
    pub max_block: usize,
    pub max_channels: usize,
    /// 帧输入窗口字节数：本配置承诺支持的最大编码帧长度，超过返回容量错误。
    pub input_window: usize,
}

/// 帧头最大字节数（RFC 9639 §9.1）：同步码与标志 2、块大小/采样率码 1、声道/位深码 1、
/// 编号（UTF-8 式编码，可变块时最多 7 字节）7、非常用块大小 2、非常用采样率 2、CRC-8 1。
pub const MAX_FRAME_HEADER_BYTES: usize = 2 + 1 + 1 + 7 + 2 + 2 + 1;

/// 给定块大小、声道与位深时，不压缩（verbatim）编码帧的字节上界：
/// 帧头 + Σ 各声道（子帧头 8 位 + 块大小 × 样本位宽）按字节向上取整 + CRC-16 2 字节。
/// 双声道时其中一个声道可能是 side 声道，样本位宽多 1 位（RFC 9639 §9.2.3）；wasted bits
/// 只会让子帧变短，不影响上界。
///
/// 正常编码器（如 libFLAC）在压缩反而更大时改用 verbatim，故这是实际可依赖的上界；
/// 超出窗口的帧由解码器返回容量错误，不会越界。
pub const fn verbatim_frame_bound(max_block: usize, channels: usize, bits: usize) -> usize {
    let side = if channels == 2 { 1 } else { 0 };
    let sub_bits = channels * 8 + max_block * (channels * bits + side);
    MAX_FRAME_HEADER_BYTES + sub_bits.div_ceil(8) + 2
}

impl Capacity {
    /// 按目标能力计算容量：输入窗口取不压缩帧上界（[`verbatim_frame_bound`]）。
    pub const fn for_limits(max_block: usize, max_channels: usize, max_bits: usize) -> Capacity {
        Capacity {
            max_block,
            max_channels,
            input_window: verbatim_frame_bound(max_block, max_channels, max_bits),
        }
    }

    /// 输入窗口至少为 `bytes`（为非 libFLAC 编码器可能产生的更大帧留余量）。
    pub const fn with_min_window(self, bytes: usize) -> Capacity {
        Capacity {
            input_window: if self.input_window > bytes {
                self.input_window
            } else {
                bytes
            },
            ..self
        }
    }

    /// 桌面：块大小最大 65535、双声道、32 位。不压缩帧上界约 520 KiB，窗口保留 M1 承诺的
    /// 1 MiB（编码帧 ≤ 1 MiB）。
    pub const DESKTOP: Capacity = Capacity::for_limits(65_535, 2, 32).with_min_window(1 << 20);

    /// 桌面多声道（F1）：块大小最大 65535、最多 8 声道（FLAC 声道上限）、32 位。不压缩帧上界
    /// 2 097 146 字节，即输入窗口；工作区 4 MiB、PCM 2 MiB。桌面配置档使用此容量。
    pub const DESKTOP_MULTICHANNEL: Capacity =
        Capacity::for_limits(65_535, 8, 32).with_min_window(1 << 20);

    /// MCU 参考配置：块大小 ≤ 4608（RFC 9639 可流式子集在 ≤ 48 kHz 时的上限）、双声道、
    /// **覆盖到 24 位**（不压缩帧上界 28 244 字节）。32 位需要 [`Capacity::for_limits`]`(4608, 2, 32)`
    /// （37 460 字节）。实际可用性以 M3 T3 在指定 MCU 上的实测为准。
    pub const MCU: Capacity = Capacity::for_limits(4_608, 2, 24);

    /// 三块缓冲的 RAM 字节数（输入窗口 + `i64` 工作区 + `i32` PCM）。
    pub const fn ram_bytes(&self) -> usize {
        self.input_window + self.workspace_len() * 8 + self.pcm_len() * 4
    }

    /// `i64` 工作区元素数。
    pub const fn workspace_len(&self) -> usize {
        self.max_block * self.max_channels
    }

    /// `i32` 交错 PCM 元素数。
    pub const fn pcm_len(&self) -> usize {
        self.max_block * self.max_channels
    }
}

/// 缓冲种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Buffer {
    InputWindow,
    Workspace,
    Pcm,
}

/// 初始化错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitError {
    /// 宿主提供的切片短于声明容量。
    CapacityExceeded {
        buffer: Buffer,
        required: usize,
        provided: usize,
    },
    /// 静态缓冲已被取走（每个 `ConstStaticCell` 只能取一次）。
    AlreadyTaken(Buffer),
}

/// 一套宿主缓冲。
pub struct FlacBuffers {
    pub input: &'static mut [u8],
    pub workspace: &'static mut [i64],
    pub pcm: &'static mut [i32],
}

fn check(buffer: Buffer, required: usize, provided: usize) -> Result<(), InitError> {
    if provided < required {
        Err(InitError::CapacityExceeded {
            buffer,
            required,
            provided,
        })
    } else {
        Ok(())
    }
}

impl FlacBuffers {
    /// 按容量检查全部切片长度。
    pub fn check(&self, cap: &Capacity) -> Result<(), InitError> {
        check(Buffer::InputWindow, cap.input_window, self.input.len())?;
        check(Buffer::Workspace, cap.workspace_len(), self.workspace.len())?;
        check(Buffer::Pcm, cap.pcm_len(), self.pcm.len())
    }
}

/// FLAC 解码错误明细（[`FlacDecoder::last_error`]）；对外的 [`DecodeError`] 由此映射。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlacError {
    /// 缺少 "fLaC" 标记。
    BadMarker,
    /// 第一个元数据块不是 STREAMINFO、长度不为 34 或重复出现。
    BadStreamInfo,
    /// 元数据块类型 127（禁用）。
    BadMetadataType,
    /// 帧同步码不符。
    LostSync,
    /// 帧头保留码或保留位。
    ReservedCode,
    BadFrameNumber,
    HeaderCrc,
    FrameCrc,
    /// 帧参数与 STREAMINFO 不一致，或阻塞策略中途改变。
    StreamMismatch,
    BadSubframe,
    BadWastedBits,
    BadLpcPrecision,
    NegativeShift,
    BadResidualCoding,
    BadPartitionOrder,
    /// 残差超出 32 位有符号范围。
    ResidualOverflow,
    /// 重建样本超出位深范围。
    SampleOverflow,
    NonZeroPadding,
    /// 块大小超出宿主容量。
    BlockTooLarge,
    /// 编码帧超出输入窗口容量（含 STREAMINFO 声明的最大帧长度）。
    FrameTooLarge,
    /// 合法但不支持：位深或采样率超出格式描述可表示的范围。
    Unsupported,
    /// VORBIS_COMMENT 块结构错误（长度越过块边界）。
    BadVorbisComment,
    /// `WAVEFORMATEXTENSIBLE_CHANNEL_MASK` 标签格式错误、多次出现且不一致，或置位数与声道数
    /// 不符 / 含无扬声器位置的位（F1：布局无法确认，拒绝）。
    BadChannelMask,
    /// 源在完整结构前结束（含已解码样本少于 STREAMINFO 总样本数）。
    Truncated,
    /// 已解码样本多于 STREAMINFO 总样本数。
    TotalSamplesMismatch,
}

impl FlacError {
    pub fn to_decode_error(self) -> DecodeError {
        match self {
            FlacError::Unsupported | FlacError::BadChannelMask => DecodeError::Unsupported,
            FlacError::Truncated => DecodeError::Truncated,
            FlacError::BlockTooLarge | FlacError::FrameTooLarge => DecodeError::CapacityExceeded,
            _ => DecodeError::Malformed,
        }
    }
}

/// 测试统计（仅 `cfg(test)` 或特性 `test-metrics`；正式构建不含）。
#[cfg(any(test, feature = "test-metrics"))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Metrics {
    /// 整帧解析尝试次数（含失败）。
    pub parse_attempts: u64,
    /// 因输入不足失败的尝试次数。
    pub incomplete_attempts: u64,
    /// 所有尝试累计读取的位数（输入重扫描量）。
    pub scanned_bits: u64,
    /// 样本重建运算次数（每个子帧样本与解相关样本各计 1）。
    pub sample_ops: u64,
    /// 成功解码的帧数。
    pub frames: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Marker,
    BlockHeader,
    StreamInfo {
        last: bool,
    },
    Skip {
        remaining: u32,
        last: bool,
    },
    /// VORBIS_COMMENT 块（§8.6），只提取 `WAVEFORMATEXTENSIBLE_CHANNEL_MASK`。
    Comment {
        remaining: u32,
        last: bool,
        phase: Vc,
    },
    Frames,
    End,
    Failed(FlacError),
}

/// VORBIS_COMMENT 解析阶段：长度字段均为 32 位小端（§8.6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Vc {
    VendorLen,
    SkipVendor(u32),
    Count,
    FieldLen {
        left: u32,
    },
    Field {
        len: u32,
        left: u32,
    },
    SkipField {
        rem: u32,
        left: u32,
    },
    /// 注释之后块内的剩余字节。
    Tail,
}

/// 声道掩码标签名（含 `=`，大小写不敏感）。
const MASK_TAG: &[u8] = b"WAVEFORMATEXTENSIBLE_CHANNEL_MASK=";
/// 解析声道掩码标签时最多读入的字段长度；更长的字段不是该标签的合法形式，按普通注释跳过。
const MAX_MASK_FIELD: u32 = 64;

/// STREAMINFO 中解码需要的字段（§8.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StreamInfo {
    params: frame::StreamParams,
    /// 元数据全部解析完（得到声道布局）之后才有值，之前不对外报告格式。
    format: Option<SourceFormat>,
    max_frame: u32,
    total_samples: u64,
}

/// 自有推送式 FLAC 解码器。
///
/// 整帧重试（M1 T2）：每次尝试都从帧头重新解析整帧。输入不足时记录下一次尝试门槛
/// `retry_at = min(max(2n, n+1), 窗口容量)`；输入未达门槛且未 EOF 时直接返回
/// `NeedInput`，不重新解析。窗口已满仍不完整返回容量错误；EOF 时绕过门槛做至多一次
/// 最终解析。成功后按实际帧长度消耗，门槛归零；已解码待输出时不重解该帧。
pub struct FlacDecoder {
    cap: Capacity,
    workspace: &'static mut [i64],
    pcm: &'static mut [i32],
    state: State,
    info: Option<StreamInfo>,
    /// 0 为固定块、1 为可变块；首帧确定后不得改变。
    blocking: Option<bool>,
    retry_at: usize,
    decoded_samples: u64,
    /// VORBIS_COMMENT 声明的声道掩码。
    channel_mask: Option<u32>,
    /// 已发布、未取走的交错样本 `pcm[start..end]`。
    start: usize,
    end: usize,
    #[cfg(any(test, feature = "test-metrics"))]
    metrics: Metrics,
}

impl FlacDecoder {
    /// 用宿主提供的工作区与 PCM 缓冲构造；长度按 `cap` 检查。
    pub fn new(
        cap: Capacity,
        workspace: &'static mut [i64],
        pcm: &'static mut [i32],
    ) -> Result<Self, InitError> {
        check(Buffer::Workspace, cap.workspace_len(), workspace.len())?;
        check(Buffer::Pcm, cap.pcm_len(), pcm.len())?;
        Ok(Self {
            cap,
            workspace,
            pcm,
            channel_mask: None,
            state: State::Marker,
            info: None,
            blocking: None,
            retry_at: 0,
            decoded_samples: 0,
            start: 0,
            end: 0,
            #[cfg(any(test, feature = "test-metrics"))]
            metrics: Metrics::default(),
        })
    }

    pub fn capacity(&self) -> Capacity {
        self.cap
    }

    pub fn workspace_len(&self) -> usize {
        self.workspace.len()
    }

    pub fn pcm_len(&self) -> usize {
        self.pcm.len()
    }

    /// 最近一次错误的明细。
    pub fn last_error(&self) -> Option<FlacError> {
        match self.state {
            State::Failed(e) => Some(e),
            _ => None,
        }
    }

    /// 当前整帧重试门槛（0 表示无）。
    pub fn retry_at(&self) -> usize {
        self.retry_at
    }

    #[cfg(any(test, feature = "test-metrics"))]
    pub fn metrics(&self) -> Metrics {
        self.metrics
    }

    #[cfg(any(test, feature = "test-metrics"))]
    pub fn reset_metrics(&mut self) {
        self.metrics = Metrics::default();
    }

    fn fail(&mut self, e: FlacError) -> Result<Progress, DecodeError> {
        self.state = State::Failed(e);
        Err(e.to_decode_error())
    }

    fn need(&mut self, consumed: usize, eof: bool) -> Result<Progress, DecodeError> {
        if eof {
            self.fail(FlacError::Truncated)
        } else {
            Ok(Progress {
                consumed,
                status: Status::NeedInput,
            })
        }
    }

    fn parse_stream_info(&mut self, b: &[u8]) -> Result<(), FlacError> {
        // §8.2：min/max 块（16/16）、min/max 帧（24/24）、采样率 20、声道−1 3、
        // 位深−1 5、总样本 36、MD5 128。
        let max_block = u16::from_be_bytes([b[2], b[3]]);
        let max_frame = u32::from_be_bytes([0, b[7], b[8], b[9]]);
        let rate = (u32::from(b[10]) << 12) | (u32::from(b[11]) << 4) | (u32::from(b[12]) >> 4);
        let channels = u32::from((b[12] >> 1) & 0x07) + 1;
        let bits = ((u32::from(b[12] & 1) << 4) | (u32::from(b[13]) >> 4)) + 1;
        let total = (u64::from(b[13] & 0x0F) << 32)
            | u64::from(u32::from_be_bytes([b[14], b[15], b[16], b[17]]));
        if rate == 0 || bits < 4 || max_block < 16 {
            return Err(FlacError::BadStreamInfo);
        }
        if channels as usize > self.cap.max_channels {
            return Err(FlacError::Unsupported);
        }
        // 声明的容量需求先检查；实际帧仍逐帧检查。
        if usize::from(max_block) > self.cap.max_block {
            return Err(FlacError::BlockTooLarge);
        }
        if max_frame != 0 && max_frame as usize > self.cap.input_window {
            return Err(FlacError::FrameTooLarge);
        }
        // 采样率与位深可由格式描述表示（声道布局在元数据结束时确定，见 end_metadata）。
        let layout = ChannelLayout::flac_default(channels as u16).ok_or(FlacError::Unsupported)?;
        SourceFormat::with_layout(rate, layout, bits as u8).map_err(|_| FlacError::Unsupported)?;
        self.info = Some(StreamInfo {
            params: frame::StreamParams {
                sample_rate: rate,
                channels,
                bits,
            },
            format: None,
            max_frame,
            total_samples: total,
        });
        Ok(())
    }

    /// 元数据结束：确定声道布局（§9.1.3 默认顺序，或 `WAVEFORMATEXTENSIBLE_CHANNEL_MASK`
    /// 声明的掩码），此后才对外报告格式。
    fn end_metadata(&mut self) -> Result<(), FlacError> {
        let Some(info) = self.info.as_mut() else {
            return Err(FlacError::BadStreamInfo);
        };
        let channels = info.params.channels as u16;
        let layout = match self.channel_mask {
            Some(m) => ChannelLayout::from_mask(m)
                .filter(|l| l.channels() == channels)
                .ok_or(FlacError::BadChannelMask)?,
            None => ChannelLayout::flac_default(channels).ok_or(FlacError::Unsupported)?,
        };
        let format =
            SourceFormat::with_layout(info.params.sample_rate, layout, info.params.bits as u8)
                .map_err(|_| FlacError::Unsupported)?;
        info.format = Some(format);
        Ok(())
    }

    /// 解析声道掩码标签字段（不含长度前缀）；不是该标签返回 `Ok(None)`。
    fn parse_mask_field(field: &[u8]) -> Result<Option<u32>, FlacError> {
        let n = MASK_TAG.len();
        if field.len() < n || !field[..n].eq_ignore_ascii_case(MASK_TAG) {
            return Ok(None);
        }
        let v = &field[n..];
        let hex = match v {
            [b'0', b'x' | b'X', rest @ ..] => rest,
            _ => return Err(FlacError::BadChannelMask),
        };
        if hex.is_empty() || hex.len() > 8 {
            return Err(FlacError::BadChannelMask);
        }
        let mut m = 0u32;
        for &c in hex {
            let d = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => return Err(FlacError::BadChannelMask),
            };
            m = (m << 4) | u32::from(d);
        }
        Ok(Some(m))
    }

    /// VORBIS_COMMENT 的一步：返回本步消耗的字节数（0 表示需要更多输入）。
    fn comment_step(
        &mut self,
        avail: &[u8],
        remaining: u32,
        phase: Vc,
    ) -> Result<(usize, u32, Option<Vc>), FlacError> {
        let take_u32 = |avail: &[u8]| -> Option<u32> {
            avail
                .get(..4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        // (消耗字节, 新的块剩余, 下一阶段；None 表示块结束)
        let need = |n: u32| -> Result<(), FlacError> {
            if n > remaining {
                Err(FlacError::BadVorbisComment)
            } else {
                Ok(())
            }
        };
        match phase {
            Vc::VendorLen | Vc::Count | Vc::FieldLen { .. } => {
                need(4)?;
                let Some(v) = take_u32(avail) else {
                    return Ok((0, remaining, Some(phase)));
                };
                let next = match phase {
                    Vc::VendorLen => Vc::SkipVendor(v),
                    Vc::Count if v == 0 => Vc::Tail,
                    Vc::Count => Vc::FieldLen { left: v },
                    Vc::FieldLen { left } if v <= MAX_MASK_FIELD => Vc::Field { len: v, left },
                    Vc::FieldLen { left } => Vc::SkipField { rem: v, left },
                    _ => Vc::Tail,
                };
                Ok((4, remaining - 4, Some(next)))
            }
            Vc::SkipVendor(rem) | Vc::SkipField { rem, .. } => {
                need(rem)?;
                if rem == 0 {
                    let next = match phase {
                        Vc::SkipVendor(_) => Vc::Count,
                        Vc::SkipField { left, .. } if left > 1 => Vc::FieldLen { left: left - 1 },
                        _ => Vc::Tail,
                    };
                    return Ok((0, remaining, Some(next)));
                }
                if avail.is_empty() {
                    return Ok((0, remaining, Some(phase)));
                }
                let n = rem.min(u32::try_from(avail.len()).unwrap_or(u32::MAX));
                let next = match phase {
                    Vc::SkipVendor(r) => Vc::SkipVendor(r - n),
                    Vc::SkipField { rem, left } => Vc::SkipField { rem: rem - n, left },
                    p => p,
                };
                Ok((n as usize, remaining - n, Some(next)))
            }
            Vc::Field { len, left } => {
                need(len)?;
                let Some(field) = avail.get(..len as usize) else {
                    return Ok((0, remaining, Some(phase)));
                };
                if let Some(m) = Self::parse_mask_field(field)? {
                    if self.channel_mask.is_some_and(|old| old != m) {
                        return Err(FlacError::BadChannelMask);
                    }
                    self.channel_mask = Some(m);
                }
                let next = if left > 1 {
                    Vc::FieldLen { left: left - 1 }
                } else {
                    Vc::Tail
                };
                Ok((len as usize, remaining - len, Some(next)))
            }
            Vc::Tail => {
                // 注释之后的剩余字节按 §8.6 不属于任何字段，流式跳过。
                if remaining == 0 {
                    return Ok((0, 0, None));
                }
                if avail.is_empty() {
                    return Ok((0, remaining, Some(phase)));
                }
                let n = remaining.min(u32::try_from(avail.len()).unwrap_or(u32::MAX));
                Ok((n as usize, remaining - n, Some(Vc::Tail)))
            }
        }
    }

    fn step(&mut self, input: &[u8], eof: bool) -> Result<Progress, DecodeError> {
        let mut pos = 0usize;
        loop {
            let avail = &input[pos..];
            match self.state {
                State::Failed(e) => return Err(e.to_decode_error()),
                State::End => {
                    return Ok(Progress {
                        consumed: pos,
                        status: Status::End,
                    });
                }
                State::Marker => {
                    if avail.len() < 4 {
                        return self.need(pos, eof);
                    }
                    if &avail[..4] != b"fLaC" {
                        return self.fail(FlacError::BadMarker);
                    }
                    pos += 4;
                    self.state = State::BlockHeader;
                }
                State::BlockHeader => {
                    if avail.len() < 4 {
                        return self.need(pos, eof);
                    }
                    let last = avail[0] & 0x80 != 0;
                    let ty = avail[0] & 0x7F;
                    let len = u32::from_be_bytes([0, avail[1], avail[2], avail[3]]);
                    pos += 4;
                    self.state = match (ty, self.info.is_some()) {
                        (0, false) if len == 34 => State::StreamInfo { last },
                        (0, _) | (_, false) => return self.fail(FlacError::BadStreamInfo),
                        (127, _) => return self.fail(FlacError::BadMetadataType),
                        (4, true) => State::Comment {
                            remaining: len,
                            last,
                            phase: Vc::VendorLen,
                        },
                        // 其余块（填充、应用、寻道表、注释、CUE、图片及保留类型）流式跳过，不缓存。
                        _ => State::Skip {
                            remaining: len,
                            last,
                        },
                    };
                }
                State::StreamInfo { last } => {
                    if avail.len() < 34 {
                        return self.need(pos, eof);
                    }
                    if let Err(e) = self.parse_stream_info(&avail[..34]) {
                        return self.fail(e);
                    }
                    pos += 34;
                    if last && let Err(e) = self.end_metadata() {
                        return self.fail(e);
                    }
                    self.state = if last {
                        State::Frames
                    } else {
                        State::BlockHeader
                    };
                }
                State::Comment {
                    remaining,
                    last,
                    phase,
                } => match self.comment_step(avail, remaining, phase) {
                    Err(e) => return self.fail(e),
                    Ok((_, _, None)) => {
                        if last && let Err(e) = self.end_metadata() {
                            return self.fail(e);
                        }
                        self.state = if last {
                            State::Frames
                        } else {
                            State::BlockHeader
                        };
                    }
                    Ok((0, r, Some(p))) if p == phase && r == remaining => {
                        return self.need(pos, eof);
                    }
                    Ok((n, r, Some(p))) => {
                        pos += n;
                        self.state = State::Comment {
                            remaining: r,
                            last,
                            phase: p,
                        };
                    }
                },
                State::Skip { remaining, last } => {
                    if remaining == 0 {
                        if last && let Err(e) = self.end_metadata() {
                            return self.fail(e);
                        }
                        self.state = if last {
                            State::Frames
                        } else {
                            State::BlockHeader
                        };
                        continue;
                    }
                    if avail.is_empty() {
                        return self.need(pos, eof);
                    }
                    let n = remaining.min(u32::try_from(avail.len()).unwrap_or(u32::MAX));
                    pos += n as usize;
                    self.state = State::Skip {
                        remaining: remaining - n,
                        last,
                    };
                }
                State::Frames => return self.frame(pos, avail, eof),
            }
        }
    }

    fn frame(&mut self, pos: usize, avail: &[u8], eof: bool) -> Result<Progress, DecodeError> {
        let Some(info) = self.info else {
            return self.fail(FlacError::BadStreamInfo);
        };
        if avail.is_empty() {
            if !eof {
                return Ok(Progress {
                    consumed: pos,
                    status: Status::NeedInput,
                });
            }
            // 码流结束：已知总样本数时核对，帧边界处的截断也能发现。
            if info.total_samples != 0 {
                if self.decoded_samples < info.total_samples {
                    return self.fail(FlacError::Truncated);
                }
                if self.decoded_samples > info.total_samples {
                    return self.fail(FlacError::TotalSamplesMismatch);
                }
            }
            self.state = State::End;
            return Ok(Progress {
                consumed: pos,
                status: Status::End,
            });
        }
        // 未达门槛且未 EOF：不重新解析。
        if !eof && avail.len() < self.retry_at {
            return Ok(Progress {
                consumed: pos,
                status: Status::NeedInput,
            });
        }
        let window = self.cap.input_window;
        let slice = &avail[..avail.len().min(window)];
        let attempt = frame::decode(
            slice,
            &info.params,
            self.cap.max_block,
            self.workspace,
            self.pcm,
        );
        #[cfg(any(test, feature = "test-metrics"))]
        {
            self.metrics.parse_attempts += 1;
            self.metrics.scanned_bits += attempt.bits;
            self.metrics.sample_ops += attempt.sample_ops;
        }
        #[cfg(not(any(test, feature = "test-metrics")))]
        let _ = (attempt.bits, attempt.sample_ops);
        match attempt.result {
            Ok(d) => {
                if *self.blocking.get_or_insert(d.variable) != d.variable {
                    return self.fail(FlacError::StreamMismatch);
                }
                #[cfg(any(test, feature = "test-metrics"))]
                {
                    self.metrics.frames += 1;
                }
                self.retry_at = 0;
                self.decoded_samples += d.block_size as u64;
                self.start = 0;
                self.end = d.block_size * info.params.channels as usize;
                Ok(Progress {
                    consumed: pos + d.len,
                    status: Status::Frames(d.block_size),
                })
            }
            Err(frame::FrameError::Bad(e)) => self.fail(e),
            Err(frame::FrameError::Incomplete) => {
                #[cfg(any(test, feature = "test-metrics"))]
                {
                    self.metrics.incomplete_attempts += 1;
                }
                let n = slice.len();
                // 窗口之外还有数据：帧超出窗口容量，不是截断（即使已到 EOF）。
                if avail.len() > window {
                    return self.fail(FlacError::FrameTooLarge);
                }
                if eof {
                    return self.fail(FlacError::Truncated);
                }
                if n >= window {
                    return self.fail(FlacError::FrameTooLarge);
                }
                if info.max_frame != 0 && n > info.max_frame as usize {
                    // STREAMINFO 声明的最大帧长度不可信：只作提示，实际以窗口容量为界。
                }
                self.retry_at = n.saturating_mul(2).max(n + 1).min(window);
                Ok(Progress {
                    consumed: pos,
                    status: Status::NeedInput,
                })
            }
        }
    }
}

impl Decoder for FlacDecoder {
    fn reset(&mut self) {
        self.state = State::Marker;
        self.info = None;
        self.channel_mask = None;
        self.blocking = None;
        self.retry_at = 0;
        self.decoded_samples = 0;
        self.start = 0;
        self.end = 0;
    }

    fn format(&self) -> Option<SourceFormat> {
        self.info.and_then(|i| i.format)
    }

    /// 工作区仍有未取走样本时返回 `NeedOutput`，不消耗输入、不重解帧。
    fn decode(&mut self, input: &[u8], eof: bool) -> Result<Progress, DecodeError> {
        if let State::Failed(e) = self.state {
            return Err(e.to_decode_error());
        }
        if self.start < self.end {
            return Ok(Progress {
                consumed: 0,
                status: Status::NeedOutput,
            });
        }
        self.step(input, eof)
    }

    fn pcm(&self) -> &[i32] {
        &self.pcm[self.start..self.end]
    }

    fn release(&mut self, frames: usize) {
        let ch = self.info.map_or(1, |i| i.params.channels as usize);
        self.start = self
            .start
            .saturating_add(frames.saturating_mul(ch))
            .min(self.end);
        if self.start == self.end {
            self.start = 0;
            self.end = 0;
        }
    }
}

#[cfg(feature = "static-cell")]
pub mod mcu;
