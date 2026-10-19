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

use qy_core::engine::{CapabilityError, DecoderDemand, Resources, Unit};
use qy_core::{
    ChannelLayout, DecodeError, Decoder, Identify, Progress, SeekError, SourceFormat, Status,
    magic_matches,
};

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
    /// 声道数超出宿主容量（F1：如双声道配置档上的多声道流）。
    TooManyChannels,
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
    /// 帧头的编号与前面已解码的内容不符（RFC 9639 §9.1.5：固定块策略下为此前的帧数，可变块策略下
    /// 为此前的样本数）：帧被重排、重复或缺失，单帧 CRC 发现不了。
    FrameNumberMismatch,
}

impl FlacError {
    pub fn to_decode_error(self) -> DecodeError {
        match self {
            FlacError::Unsupported | FlacError::BadChannelMask => DecodeError::Unsupported,
            FlacError::Truncated => DecodeError::Truncated,
            FlacError::BlockTooLarge | FlacError::FrameTooLarge | FlacError::TooManyChannels => {
                DecodeError::CapacityExceeded
            }
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

/// 定位（S1）的跳转次数上限：超过时从已确认的、目标之前最近的帧起顺序解码。
const MAX_SEEK_JUMPS: u32 = 32;

/// 定位过程（S1）。位置均为距源开头的字节数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Seeking {
    target: u64,
    /// 已确认的、不晚于目标的帧起点（字节位置、首样本号）。
    lo: (u64, u64),
    /// 已确认的、晚于目标的帧起点。
    hi: Option<(u64, u64)>,
    jumps: u32,
    /// 上一次试探落在源末尾之后（一个字节也没读到）：不知道文件实际多长，下一次取上下界中点
    /// 而不按样本号插值（定位表的偏移越界时，插值每次只能缩小一小部分范围）。
    beyond_end: bool,
    phase: Phase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// 重新读取定位表（§8.5）：还剩 `left` 个点；`best` 为目前不晚于目标的最大点
    /// （样本号、相对第一帧帧头的字节偏移）。
    Table { left: u32, best: Option<(u64, u64)> },
    /// 在 `start` 之后找帧：当前输入从 `at` 开始。
    Probe { start: u64, at: u64 },
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
    /// STREAMINFO 声明的最大块（不可信，只用于播放计划）。
    max_block: u32,
    /// 最小块等于最大块：固定块大小的码流（§8.2），帧号可换算为样本号（S1 定位）。
    fixed_size: bool,
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
    /// 下一帧帧头应有的编号（固定块：帧号；可变块：首样本号）。定位后为 `None`，由落点之后的第一帧
    /// 重新确定。
    expected_number: Option<u64>,
    /// VORBIS_COMMENT 声明的声道掩码。
    channel_mask: Option<u32>,
    /// STREAMINFO 阶段因本机容量不足拒绝的具体原因（R1）。
    rejection: Option<CapabilityError>,
    /// 已发布、未取走的交错样本 `pcm[start..end]`。
    start: usize,
    end: usize,
    /// 已消耗的总字节数（S1 定位用：当前输入第一个字节的位置）。
    offset: u64,
    /// 第一帧帧头的位置（S1）。
    first_frame: Option<u64>,
    /// SEEKTABLE 内容的位置与长度（S1）：定位时重新读取，不缓存表。
    table: Option<(u64, u32)>,
    /// 第一帧的字节长度与块大小（S1）：固定块策略下由帧号换算样本号，并估计每样本字节数。
    nominal: Option<(usize, usize)>,
    /// 定位进行中（S1）。
    seeking: Option<Seeking>,
    /// 顺序解码到目标时还要丢弃的样本帧数（S1）。
    skip: u64,
    /// 等待核心取走的跳转位置（S1）。
    request: Option<u64>,
    #[cfg(any(test, feature = "test-metrics"))]
    metrics: Metrics,
    /// 识别模式（E3，[`identify`]）：没有工作区、容量不设限，元数据读完即停。
    probe: bool,
}

/// 识别 FLAC 文件头（E3，[`qy_core::Identify`]）：不消耗输入、不需要工作区，与完整解码按同一个
/// 状态机判断，读到元数据结束、第一帧开始处为止（元数据块可以任意大，如嵌入的图片，需要的字节数
/// 由文件决定，不够时以 [`Identify::NeedMore`] 说明至少还要多少）。
///
/// 本机容量（块大小、声道数、帧长度超出宿主配置）不在识别范围内：识别按不设限判断，完整解码时
/// 仍按宿主容量报告（[`Resources::capacity_rejection`]）。拒绝明细为 [`FlacError`]。
pub fn identify(head: &[u8], eof: bool) -> Identify<FlacError> {
    match magic_matches(head, &[b"fLaC"], eof) {
        // 标记的前缀：下一个字节就可能排除本格式。
        None => {
            return Identify::NeedMore {
                total: head.len() as u64 + 1,
                container: false,
            };
        }
        Some(false) => return Identify::Foreign,
        Some(true) => {}
    }
    let unlimited = Capacity {
        max_block: usize::MAX,
        max_channels: usize::MAX,
        input_window: usize::MAX,
    };
    let mut d =
        FlacDecoder::with_buffers(unlimited, <&mut [i64]>::default(), <&mut [i32]>::default());
    d.probe = true;
    let result = d.decode(head, eof);
    match (result, d.state, d.format(), d.first_frame) {
        (Err(error), ..) => Identify::Rejected {
            error,
            detail: d.last_error(),
        },
        (Ok(p), State::Frames, Some(format), Some(data_start))
            if p.status == Status::NeedOutput =>
        {
            Identify::Supported { format, data_start }
        }
        (Ok(_), ..) => Identify::NeedMore {
            total: d
                .offset
                .saturating_add(d.wanted())
                .max(head.len() as u64 + 1),
            container: true,
        },
    }
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
        Ok(Self::with_buffers(cap, workspace, pcm))
    }

    fn with_buffers(cap: Capacity, workspace: &'static mut [i64], pcm: &'static mut [i32]) -> Self {
        Self {
            cap,
            workspace,
            pcm,
            channel_mask: None,
            rejection: None,
            state: State::Marker,
            info: None,
            blocking: None,
            retry_at: 0,
            decoded_samples: 0,
            expected_number: Some(0),
            start: 0,
            end: 0,
            offset: 0,
            first_frame: None,
            table: None,
            nominal: None,
            seeking: None,
            skip: 0,
            request: None,
            #[cfg(any(test, feature = "test-metrics"))]
            metrics: Metrics::default(),
            probe: false,
        }
    }

    /// 识别模式下，当前状态还要读的字节数（从当前结构开头算；只用于 [`Identify::NeedMore`] 的
    /// 下界）。
    fn wanted(&self) -> u64 {
        match self.state {
            State::Marker | State::BlockHeader => 4,
            State::StreamInfo { .. } => 34,
            // 最后一个元数据块跳过之后就是第一帧（结论随即确定）。
            State::Skip { remaining, .. } => u64::from(remaining),
            State::Comment { .. } | State::Frames | State::End | State::Failed(_) => 1,
        }
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

    /// STREAMINFO 声明的总帧数（RFC 9639 §8.2 “total samples in stream”，每声道样本数）；
    /// 未声明（0）或尚未读到 STREAMINFO 时为 `None`。来自不可信的文件头，只用于显示时长。
    pub fn declared_frames(&self) -> Option<u64> {
        self.info
            .and_then(|i| (i.total_samples != 0).then_some(i.total_samples))
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
        let min_block = u16::from_be_bytes([b[0], b[1]]);
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
        // 声明的容量需求先检查（R1：记录具体原因，引擎按能力不足报告）；实际帧仍逐帧检查。
        let sat = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
        if channels as usize > self.cap.max_channels {
            self.rejection = Some(CapabilityError::Channels {
                channels: channels as u16,
                max: u16::try_from(self.cap.max_channels).unwrap_or(u16::MAX),
            });
            return Err(FlacError::TooManyChannels);
        }
        if usize::from(max_block) > self.cap.max_block {
            self.rejection = Some(CapabilityError::DecodeUnit {
                frames: u32::from(max_block),
                max: sat(self.cap.max_block),
                unit: &BLOCK,
            });
            return Err(FlacError::BlockTooLarge);
        }
        if max_frame != 0 && max_frame as usize > self.cap.input_window {
            self.rejection = Some(CapabilityError::InputUnit {
                bytes: max_frame,
                window: sat(self.cap.input_window),
                unit: &FRAME,
            });
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
            max_block: u32::from(max_block),
            fixed_size: min_block == max_block,
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
                        // 寻道表（§8.5）：记下位置，定位时再读；照常跳过。
                        (3, true) => {
                            if self.table.is_none() {
                                self.table = Some((self.offset + pos as u64, len));
                            }
                            State::Skip {
                                remaining: len,
                                last,
                            }
                        }
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
                State::Frames if self.probe => {
                    // E3：元数据都已接受，第一帧从这里开始。
                    self.first_frame = Some(self.offset + pos as u64);
                    return Ok(Progress {
                        consumed: pos,
                        status: Status::NeedOutput,
                    });
                }
                State::Frames => return self.frame(pos, avail, eof),
            }
        }
    }

    fn frame(&mut self, pos: usize, avail: &[u8], eof: bool) -> Result<Progress, DecodeError> {
        let Some(info) = self.info else {
            return self.fail(FlacError::BadStreamInfo);
        };
        if self.first_frame.is_none() {
            self.first_frame = Some(self.offset + pos as u64);
        }
        // 顺序解码到定位目标时整帧丢弃的帧在本次调用内继续解码（循环，不递归）。
        let mut used = 0usize;
        loop {
            let avail = &avail[used..];
            let pos = pos + used;
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
                    if self.expected_number.is_some_and(|n| n != d.number) {
                        return self.fail(FlacError::FrameNumberMismatch);
                    }
                    self.expected_number = next_number(&d);
                    #[cfg(any(test, feature = "test-metrics"))]
                    {
                        self.metrics.frames += 1;
                    }
                    if self.nominal.is_none() {
                        self.nominal = Some((d.len, d.block_size));
                    }
                    self.retry_at = 0;
                    self.decoded_samples += d.block_size as u64;
                    let ch = info.params.channels as usize;
                    let drop = usize::try_from(self.skip)
                        .unwrap_or(usize::MAX)
                        .min(d.block_size);
                    self.skip -= drop as u64;
                    if drop == d.block_size {
                        self.start = 0;
                        self.end = 0;
                        used += d.len;
                        continue;
                    }
                    self.start = drop * ch;
                    self.end = d.block_size * ch;
                    return Ok(Progress {
                        consumed: pos + d.len,
                        status: Status::Frames(d.block_size - drop),
                    });
                }
                Err(frame::FrameError::Bad(e)) => return self.fail(e),
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
                    // STREAMINFO 声明的最大帧长度不可信，只作提示：实际以窗口容量为界。
                    // 这里 n ≥ 1（输入非空），翻倍后必然比 n 大。
                    self.retry_at = n.saturating_mul(2).min(window);
                    return Ok(Progress {
                        consumed: pos,
                        status: Status::NeedInput,
                    });
                }
            }
        }
    }

    // ---- S1 定位 ----

    /// 开始定位（核心在首次产出 PCM 之后调用）：有定位表时先跳到表重新读取；没有表时按比例
    /// 估计位置开始二分；总样本数未知又没有表时不支持（核心就地顺序解码）。
    fn seek_to(&mut self, target: u64) -> Result<u64, SeekError> {
        let (Some(info), Some(first), Some(_), State::Frames) =
            (self.info, self.first_frame, self.nominal, self.state)
        else {
            return Err(SeekError::Unsupported);
        };
        let total = info.total_samples;
        if total != 0 && target >= total {
            return Err(SeekError::PastEnd);
        }
        // 固定块策略下帧号 × 块大小才是样本号，只在固定块大小的码流中成立（§9.1.5）；块大小
        // 不一的固定块码流不试探，就地顺序解码。
        if self.blocking == Some(false) && !info.fixed_size {
            return Err(SeekError::Unsupported);
        }
        let mut s = Seeking {
            target,
            lo: (first, 0),
            hi: None,
            jumps: 1,
            beyond_end: false,
            phase: Phase::Probe {
                start: first,
                at: first,
            },
        };
        let to = match self.table {
            Some((at, len)) => {
                s.phase = Phase::Table {
                    left: len / 18,
                    best: None,
                };
                at
            }
            None if total == 0 => return Err(SeekError::Unsupported),
            None => {
                let g = self.guess(&s);
                s.phase = Phase::Probe { start: g, at: g };
                g
            }
        };
        self.seeking = Some(s);
        self.start = 0;
        self.end = 0;
        self.retry_at = 0;
        self.skip = 0;
        self.expected_number = None;
        self.offset = to;
        Ok(to)
    }

    /// 下一次试探的位置：已知上界时在上下界之间按样本号插值，否则按第一帧的每样本字节数外推；
    /// 再向前退两个第一帧长度，使落点之后找到的帧更可能不晚于目标。结果在 [下界, 上界) 内。
    fn guess(&self, s: &Seeking) -> u64 {
        let (lo_off, lo_s) = s.lo;
        let (len, block) = self.nominal.unwrap_or((1, 1));
        let ahead = u128::from(s.target.saturating_sub(lo_s));
        let est = match s.hi {
            Some((ho, _)) if s.beyond_end && ho > lo_off => {
                (u128::from(lo_off) + u128::from(ho)) / 2 + 2 * len as u128
            }
            Some((ho, hs)) if hs > lo_s && ho > lo_off => {
                u128::from(lo_off) + ahead * u128::from(ho - lo_off) / u128::from(hs - lo_s)
            }
            _ => u128::from(lo_off) + ahead * len as u128 / block.max(1) as u128,
        };
        let g = u64::try_from(est.saturating_sub(2 * len as u128))
            .unwrap_or(u64::MAX)
            .max(lo_off);
        match s.hi {
            Some((ho, _)) => g.min(ho.saturating_sub(1)).max(lo_off),
            None => g,
        }
    }

    /// 请核心把读取位置移到 `to`，丢弃本次输入。
    fn jump(&mut self, to: u64, input_len: usize) -> Result<Progress, DecodeError> {
        self.request = Some(to);
        self.offset = to;
        self.retry_at = 0;
        Ok(Progress {
            consumed: input_len,
            status: Status::NeedInput,
        })
    }

    /// 已离目标足够近（不超过 1 秒的样本）、范围已窄、跳转次数用完或不能二分（总样本数未知）时，
    /// 从已确认的下界帧顺序解码并丢弃目标之前的样本；否则继续试探。
    fn next_jump(&mut self, mut s: Seeking, input_len: usize) -> Result<Progress, DecodeError> {
        let Some(info) = self.info else {
            return self.fail(FlacError::BadStreamInfo);
        };
        let (len, _) = self.nominal.unwrap_or((1, 1));
        let near = s.target - s.lo.1 <= u64::from(info.params.sample_rate);
        let narrow =
            s.hi.is_some_and(|(ho, _)| ho.saturating_sub(s.lo.0) <= 4 * len as u64);
        if near || narrow || info.total_samples == 0 || s.jumps >= MAX_SEEK_JUMPS {
            self.seeking = None;
            self.skip = s.target - s.lo.1;
            self.decoded_samples = s.lo.1;
            return self.jump(s.lo.0, input_len);
        }
        let g = self.guess(&s);
        s.jumps += 1;
        s.phase = Phase::Probe { start: g, at: g };
        self.seeking = Some(s);
        self.jump(g, input_len)
    }

    fn seek_step(&mut self, s: Seeking, input: &[u8], eof: bool) -> Result<Progress, DecodeError> {
        match s.phase {
            Phase::Table { left, best } => self.table_step(s, left, best, input, eof),
            Phase::Probe { start, at } => self.probe_step(s, start, at, input, eof),
        }
    }

    /// 读取定位表的点（每点 18 字节，§8.5），跳过占位点（样本号全 1）。表读完（或源结束）后
    /// 到选中的点试探；没有可用的点时按无表处理。表的内容不可信：选中的点仍要经试探确认。
    fn table_step(
        &mut self,
        mut s: Seeking,
        mut left: u32,
        mut best: Option<(u64, u64)>,
        input: &[u8],
        eof: bool,
    ) -> Result<Progress, DecodeError> {
        let mut pos = 0usize;
        while left > 0 && input.len() - pos >= 18 {
            let p = &input[pos..pos + 18];
            let sample = u64::from_be_bytes([p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7]]);
            let off = u64::from_be_bytes([p[8], p[9], p[10], p[11], p[12], p[13], p[14], p[15]]);
            if sample != u64::MAX && sample <= s.target && best.is_none_or(|(b, _)| sample > b) {
                best = Some((sample, off));
            }
            pos += 18;
            left -= 1;
        }
        if left > 0 && !eof {
            s.phase = Phase::Table { left, best };
            self.seeking = Some(s);
            return Ok(Progress {
                consumed: pos,
                status: Status::NeedInput,
            });
        }
        let point = best.and_then(|(_, off)| s.lo.0.checked_add(off));
        match point {
            Some(at) if at > s.lo.0 => {
                s.jumps += 1;
                s.phase = Phase::Probe { start: at, at };
                self.seeking = Some(s);
                self.jump(at, input.len())
            }
            _ => self.next_jump(s, input.len()),
        }
    }

    /// 从 `at` 起找帧同步：同步码 `0xFFF8`/`0xFFF9`（阻塞策略位须与码流一致），再整帧解码——
    /// 帧头 CRC-8、与 STREAMINFO 一致、帧尾 CRC-16 都通过才算找到；否则是音频数据里的误同步，
    /// 继续向后找。
    fn probe_step(
        &mut self,
        mut s: Seeking,
        start: u64,
        at: u64,
        input: &[u8],
        eof: bool,
    ) -> Result<Progress, DecodeError> {
        let Some(info) = self.info else {
            return self.fail(FlacError::BadStreamInfo);
        };
        let window = self.cap.input_window;
        let bit = self.blocking.map(u8::from);
        let mut i = 0usize;
        while i + 1 < input.len() {
            if input[i] == 0xFF
                && input[i + 1] & 0xFE == 0xF8
                && bit.is_none_or(|b| input[i + 1] & 1 == b)
            {
                let slice = &input[i..input.len().min(i + window)];
                let attempt = frame::decode(
                    slice,
                    &info.params,
                    self.cap.max_block,
                    self.workspace,
                    self.pcm,
                );
                match attempt.result {
                    Ok(d) => return self.probed(s, at + i as u64, d, i, input.len()),
                    // 帧可能还没读全：保留从候选同步码起的输入，等更多数据。
                    Err(frame::FrameError::Incomplete) if !eof && slice.len() < window => {
                        s.phase = Phase::Probe {
                            start,
                            at: at + i as u64,
                        };
                        self.seeking = Some(s);
                        return Ok(Progress {
                            consumed: i,
                            status: Status::NeedInput,
                        });
                    }
                    _ => {}
                }
            }
            i += 1;
        }
        if !eof {
            // 没有候选：丢弃已扫描的字节，保留最后一个（可能是同步码的前半）。
            let consumed = input.len().saturating_sub(1);
            s.phase = Phase::Probe {
                start,
                at: at + consumed as u64,
            };
            self.seeking = Some(s);
            return Ok(Progress {
                consumed,
                status: Status::NeedInput,
            });
        }
        // 到源末尾也没有帧：`start` 之后没有帧起点，上界前移到这里。
        s.beyond_end = at == start && input.is_empty();
        let end = info.total_samples;
        s.hi = Some(match s.hi {
            Some((ho, hs)) if ho <= start => (ho, hs),
            _ => (start, end),
        });
        self.next_jump(s, input.len())
    }

    /// 试探找到一帧（位于 `f`，首样本号 `first`）。含目标时落在此帧：丢弃目标之前的样本，其后的
    /// 输入照常解码；否则更新上界或下界再试。
    fn probed(
        &mut self,
        mut s: Seeking,
        f: u64,
        d: frame::Decoded,
        i: usize,
        input_len: usize,
    ) -> Result<Progress, DecodeError> {
        let Some(info) = self.info else {
            return self.fail(FlacError::BadStreamInfo);
        };
        let (_, nominal_block) = self.nominal.unwrap_or((0, d.block_size));
        let first = if d.variable {
            d.number
        } else {
            d.number.saturating_mul(nominal_block as u64)
        };
        let block = d.block_size as u64;
        if first <= s.target && s.target < first.saturating_add(block) {
            self.seeking = None;
            self.retry_at = 0;
            self.decoded_samples = first + block;
            self.expected_number = next_number(&d);
            let drop = (s.target - first) as usize;
            let ch = info.params.channels as usize;
            self.start = drop * ch;
            self.end = d.block_size * ch;
            return Ok(Progress {
                consumed: i + d.len,
                status: Status::Frames(d.block_size - drop),
            });
        }
        let Phase::Probe { start, .. } = s.phase else {
            return self.fail(FlacError::BadStreamInfo);
        };
        if first > s.target {
            s.hi = Some(match s.hi {
                // 又找到已知的上界帧：[start, 上界) 内没有帧起点，上界前移到 start。
                Some((ho, hs)) if ho <= f => (start.min(ho), hs),
                _ => (f, first),
            });
        } else if f >= s.lo.0 {
            s.lo = (f + d.len as u64, first + block);
        }
        s.beyond_end = false;
        self.start = 0;
        self.end = 0;
        self.next_jump(s, input_len)
    }
}

impl Resources for FlacDecoder {
    fn capacity_rejection(&self) -> Option<CapabilityError> {
        self.rejection
    }

    fn demand(&self) -> Option<DecoderDemand> {
        let info = self.info.as_ref()?;
        let sat = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
        Some(DecoderDemand {
            max_unit_frames: info.max_block,
            unit_frames_capacity: sat(self.cap.max_block),
            max_unit_bytes: info.max_frame,
            window: sat(self.cap.input_window),
            decode_unit: &BLOCK,
            input_unit: &FRAME,
        })
    }
}

/// FLAC 的解码单元是“块”（一个块的样本帧数），输入单元是“帧”（编码帧的字节数）；报告中的叫法
/// （B1：核心只按资源表达）。
pub const BLOCK: Unit = Unit {
    codec: "FLAC",
    name: "块",
};
pub const FRAME: Unit = Unit {
    codec: "FLAC",
    name: "帧",
};

/// 下一帧帧头应有的编号：固定块策略为帧号加 1，可变块策略为首样本号加本帧块大小（RFC 9639
/// §9.1.5）。溢出时为 `None`（不再核对）。
fn next_number(d: &frame::Decoded) -> Option<u64> {
    if d.variable {
        d.number.checked_add(d.block_size as u64)
    } else {
        d.number.checked_add(1)
    }
}

impl Decoder for FlacDecoder {
    fn reset(&mut self) {
        self.state = State::Marker;
        self.info = None;
        self.channel_mask = None;
        self.rejection = None;
        self.blocking = None;
        self.retry_at = 0;
        self.decoded_samples = 0;
        self.expected_number = Some(0);
        self.start = 0;
        self.end = 0;
        self.offset = 0;
        self.first_frame = None;
        self.table = None;
        self.nominal = None;
        self.seeking = None;
        self.skip = 0;
        self.request = None;
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
        let r = match self.seeking.take() {
            Some(s) => self.seek_step(s, input, eof),
            None => self.step(input, eof),
        };
        // 跳转时位置已设为目标，本次输入整体丢弃，不计入。
        if let Ok(p) = r
            && self.request.is_none()
        {
            self.offset += p.consumed as u64;
        }
        r
    }

    fn seek(&mut self, frame: u64) -> Result<u64, SeekError> {
        self.seek_to(frame)
    }

    fn seek_request(&mut self) -> Option<u64> {
        self.request.take()
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

/// M2 T5：Kani 有界证明（`cargo kani -p qy-codec-flac`）。STREAMINFO 的 34 字节全为任意值时，解析与
/// 能力检查不 panic（无整数溢出、越界或除零）；成功时字段在规范范围内。
#[cfg(kani)]
mod proofs {
    extern crate alloc;
    use super::*;
    use alloc::boxed::Box;
    use alloc::vec;

    #[kani::proof]
    fn stream_info_any_fields() {
        let cap = Capacity::for_limits(16, 2, 32);
        let ws = Box::leak(vec![0i64; cap.workspace_len()].into_boxed_slice());
        let pcm = Box::leak(vec![0i32; cap.pcm_len()].into_boxed_slice());
        let Ok(mut d) = FlacDecoder::new(cap, ws, pcm) else {
            panic!()
        };
        let b: [u8; 34] = kani::any();
        if d.parse_stream_info(&b).is_ok() {
            let Some(info) = d.info else { panic!() };
            let p = info.params;
            assert!((1..=8).contains(&p.channels) && (4..=32).contains(&p.bits));
            assert!(p.sample_rate > 0 && info.max_block >= 16);
        }
    }
}
