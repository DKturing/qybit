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

use qy_core::{DecodeError, Decoder, Progress, SourceFormat, Status};

/// 解码器容量配置（宿主声明）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    /// 支持的最大块大小（帧/声道）。
    pub max_block: usize,
    pub max_channels: usize,
    /// 帧输入窗口字节数：本配置承诺支持的最大编码帧长度，超过返回容量错误。
    pub input_window: usize,
}

impl Capacity {
    /// 桌面：块大小最大 65535、双声道、编码帧 ≤ 1 MiB。
    pub const DESKTOP: Capacity = Capacity {
        max_block: 65_535,
        max_channels: 2,
        input_window: 1 << 20,
    };

    /// MCU 参考配置：块大小 ≤ 4608（RFC 9639 可流式子集在 ≤ 48 kHz 时的上限）、
    /// 双声道、编码帧 ≤ 32 KiB（足够 24 位双声道 4608 帧的 verbatim 帧）。
    pub const MCU: Capacity = Capacity {
        max_block: 4_608,
        max_channels: 2,
        input_window: 32 * 1024,
    };

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
    /// 合法但本阶段不支持：声道数、采样率或位深。
    Unsupported,
    /// 源在完整结构前结束（含已解码样本少于 STREAMINFO 总样本数）。
    Truncated,
    /// 已解码样本多于 STREAMINFO 总样本数。
    TotalSamplesMismatch,
}

impl FlacError {
    pub fn to_decode_error(self) -> DecodeError {
        match self {
            FlacError::Unsupported => DecodeError::Unsupported,
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
    StreamInfo { last: bool },
    Skip { remaining: u32, last: bool },
    Frames,
    End,
    Failed(FlacError),
}

/// STREAMINFO 中解码需要的字段（§8.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StreamInfo {
    params: frame::StreamParams,
    format: SourceFormat,
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
        if channels as usize > self.cap.max_channels || channels > 2 {
            return Err(FlacError::Unsupported);
        }
        // 声明的容量需求先检查；实际帧仍逐帧检查。
        if usize::from(max_block) > self.cap.max_block {
            return Err(FlacError::BlockTooLarge);
        }
        if max_frame != 0 && max_frame as usize > self.cap.input_window {
            return Err(FlacError::FrameTooLarge);
        }
        let format = SourceFormat::new(rate, channels as u16, bits as u8)
            .map_err(|_| FlacError::Unsupported)?;
        self.info = Some(StreamInfo {
            params: frame::StreamParams {
                sample_rate: rate,
                channels,
                bits,
            },
            format,
            max_frame,
            total_samples: total,
        });
        Ok(())
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
                    self.state = if last {
                        State::Frames
                    } else {
                        State::BlockHeader
                    };
                }
                State::Skip { remaining, last } => {
                    if remaining == 0 {
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
        self.blocking = None;
        self.retry_at = 0;
        self.decoded_samples = 0;
        self.start = 0;
        self.end = 0;
    }

    fn format(&self) -> Option<SourceFormat> {
        self.info.map(|i| i.format)
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
