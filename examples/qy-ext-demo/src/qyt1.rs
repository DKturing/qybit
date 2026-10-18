//! 示例格式 QYT1 的解码器：只为说明扩展接口而定义，不是要支持的生产格式。
//!
//! 文件布局（全部小端）：
//!
//! | 偏移 | 长度 | 内容 |
//! | --- | --- | --- |
//! | 0 | 4 | 魔数 `QYT1`，或 `QYL1`（有损变体，见下） |
//! | 4 | 4 | 采样率（Hz，非 0） |
//! | 8 | 2 | 声道数（1 或 2） |
//! | 10 | 2 | 有效位数 N（1..=32） |
//! | 12 | 8 | 帧数 |
//! | 20 | … | 交错样本：每个样本占 ⌈N/8⌉ 字节，小端补码，值必须在 N 位有符号范围内 |
//!
//! `QYL1` 与 `QYT1` 布局相同，只是把解码出的格式标为有损（E1，[`SourceFormat::lossy`]）：它代表
//! 有损格式的插件解码器——样本是解码重建的信号，N 是解码输出的精度，不是音源位深；报告与界面据此
//! 写明“有损”。
//!
//! 写法与仓库里的自有解码器相同，满足扩展契约（`docs/x1-extensions.md`）：
//! - 不用 `alloc`、不写 `unsafe`；PCM 工作区由宿主以 `&'static mut [i32]` 提供，`reset` 后复用；
//! - 推送式：缺输入返回 `NeedInput`，任意分块结果相同；文件头解析完成前 `format()` 为 `None`；
//! - 输入不可信：长度与整数计算都检查，不 `panic`；出错后在 `reset` 之前不再产出样本；
//! - 定位可选：[`Qyt1Decoder::new`] 支持按字节直接定位，[`Qyt1Decoder::without_seek`] 不支持，
//!   由核心改为顺序解码（结果同样精确）。

use qy_core::engine::Resources;
use qy_core::{DecodeError, Decoder, Progress, SeekError, SourceFormat, Status};

/// 魔数。
pub const MAGIC: &[u8; 4] = b"QYT1";
/// 有损变体的魔数（E1）。
pub const MAGIC_LOSSY: &[u8; 4] = b"QYL1";
/// 文件头字节数。
pub const HEADER: usize = 20;

/// 解码失败的明细（报告用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Qyt1Detail {
    /// 魔数不是 `QYT1`。
    Magic,
    /// 采样率为 0、声道数不是 1 或 2，或位数不在 1..=32。
    Header,
    /// 第 `frame` 帧有样本超出声明位数的范围。
    SampleRange { frame: u64 },
    /// 数据比声明的帧数短。
    Truncated { frames: u64 },
}

/// QYT1 解码器。
pub struct Qyt1Decoder {
    pcm: &'static mut [i32],
    seekable: bool,
    format: Option<SourceFormat>,
    channels: usize,
    bytes: usize,
    bits: u32,
    total: u64,
    done: u64,
    /// 工作区中尚未取走的帧：从 `start` 起 `pending` 帧。
    start: usize,
    pending: usize,
    failed: Option<Qyt1Detail>,
    /// 坏样本之前的完整帧已输出，下一次调用报告的错误（结果与分块无关）。
    deferred: Option<Qyt1Detail>,
}

impl Qyt1Decoder {
    /// 支持定位的解码器；`pcm` 至少能放一帧双声道样本。
    pub fn new(pcm: &'static mut [i32]) -> Result<Self, DecodeError> {
        if pcm.len() < 2 {
            return Err(DecodeError::CapacityExceeded);
        }
        Ok(Self {
            pcm,
            seekable: true,
            format: None,
            channels: 0,
            bytes: 0,
            bits: 0,
            total: 0,
            done: 0,
            start: 0,
            pending: 0,
            failed: None,
            deferred: None,
        })
    }

    /// 不支持定位的解码器（核心改为顺序解码）。
    pub fn without_seek(pcm: &'static mut [i32]) -> Result<Self, DecodeError> {
        let mut d = Self::new(pcm)?;
        d.seekable = false;
        Ok(d)
    }

    /// 当前曲目的失败明细。
    pub fn last_detail(&self) -> Option<Qyt1Detail> {
        self.failed
    }

    fn fail(&mut self, detail: Qyt1Detail, e: DecodeError) -> Result<Progress, DecodeError> {
        self.failed = Some(detail);
        Err(e)
    }

    fn header(&mut self, h: &[u8]) -> Result<(), (Qyt1Detail, DecodeError)> {
        let lossy = &h[..4] == MAGIC_LOSSY;
        if &h[..4] != MAGIC && !lossy {
            return Err((Qyt1Detail::Magic, DecodeError::Malformed));
        }
        let rate = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
        let channels = u16::from_le_bytes([h[8], h[9]]);
        let bits = u16::from_le_bytes([h[10], h[11]]);
        let total = u64::from_le_bytes([h[12], h[13], h[14], h[15], h[16], h[17], h[18], h[19]]);
        if !(1..=32).contains(&bits) || !(1..=2).contains(&channels) {
            return Err((Qyt1Detail::Header, DecodeError::Unsupported));
        }
        let format = SourceFormat::new(rate, channels, bits as u8)
            .map_err(|_| (Qyt1Detail::Header, DecodeError::Malformed))?;
        let format = if lossy { format.lossy() } else { format };
        self.format = Some(format);
        self.channels = usize::from(channels);
        self.bits = u32::from(bits);
        self.bytes = usize::from(bits).div_ceil(8);
        self.total = total;
        Ok(())
    }

    fn frame_bytes(&self) -> usize {
        self.channels * self.bytes
    }

    /// 读一个样本：`b` 为 ⌈N/8⌉ 字节小端补码；超出 N 位有符号范围时为 `None`。
    fn sample(&self, b: &[u8]) -> Option<i32> {
        let mut raw = 0u32;
        for (i, &x) in b.iter().enumerate() {
            raw |= u32::from(x) << (8 * i);
        }
        let width = 8 * b.len() as u32;
        // 按容器宽度做符号扩展，再检查是否落在 N 位范围内。
        let v = if width == 32 {
            raw as i32
        } else {
            ((raw << (32 - width)) as i32) >> (32 - width)
        };
        let lo = -(1i64 << (self.bits - 1));
        let hi = (1i64 << (self.bits - 1)) - 1;
        (lo..=hi).contains(&i64::from(v)).then_some(v)
    }
}

impl Decoder for Qyt1Decoder {
    fn reset(&mut self) {
        self.format = None;
        self.channels = 0;
        self.bytes = 0;
        self.bits = 0;
        self.total = 0;
        self.done = 0;
        self.start = 0;
        self.pending = 0;
        self.failed = None;
        self.deferred = None;
    }

    fn format(&self) -> Option<SourceFormat> {
        self.format
    }

    fn decode(&mut self, input: &[u8], eof: bool) -> Result<Progress, DecodeError> {
        if self.failed.is_some() {
            return Err(DecodeError::Malformed);
        }
        if self.pending > 0 {
            return Ok(Progress {
                consumed: 0,
                status: Status::NeedOutput,
            });
        }
        if let Some(detail) = self.deferred.take() {
            return self.fail(detail, DecodeError::Malformed);
        }
        let mut consumed = 0;
        if self.format.is_none() {
            let Some(h) = input.get(..HEADER) else {
                if eof {
                    return self.fail(Qyt1Detail::Truncated { frames: 0 }, DecodeError::Truncated);
                }
                return Ok(Progress {
                    consumed: 0,
                    status: Status::NeedInput,
                });
            };
            if let Err((detail, e)) = self.header(h) {
                return self.fail(detail, e);
            }
            consumed = HEADER;
        }
        if self.done >= self.total {
            return Ok(Progress {
                consumed,
                status: Status::End,
            });
        }
        let rest = input.get(consumed..).unwrap_or(&[]);
        let fb = self.frame_bytes();
        let left = usize::try_from(self.total - self.done).unwrap_or(usize::MAX);
        let room = self.pcm.len() / self.channels;
        let n = (rest.len() / fb).min(left).min(room);
        if n == 0 {
            if eof {
                return self.fail(
                    Qyt1Detail::Truncated { frames: self.done },
                    DecodeError::Truncated,
                );
            }
            return Ok(Progress {
                consumed,
                status: Status::NeedInput,
            });
        }
        let mut n = n;
        for (i, b) in rest[..n * fb].chunks_exact(self.bytes).enumerate() {
            let Some(v) = self.sample(b) else {
                // 先输出坏样本之前的完整帧，下一次调用再报错：不论输入怎样分块，出错前输出的
                // 帧都相同。
                let good = i / self.channels;
                let detail = Qyt1Detail::SampleRange {
                    frame: self.done + good as u64,
                };
                if good == 0 {
                    return self.fail(detail, DecodeError::Malformed);
                }
                self.deferred = Some(detail);
                n = good;
                break;
            };
            self.pcm[i] = v;
        }
        self.start = 0;
        self.pending = n;
        self.done += n as u64;
        Ok(Progress {
            consumed: consumed + n * fb,
            status: Status::Frames(n),
        })
    }

    fn pcm(&self) -> &[i32] {
        let ch = self.channels;
        &self.pcm[self.start * ch..(self.start + self.pending) * ch]
    }

    fn release(&mut self, frames: usize) {
        let n = frames.min(self.pending);
        self.start += n;
        self.pending -= n;
    }

    fn seek(&mut self, frame: u64) -> Result<u64, SeekError> {
        if !self.seekable {
            return Err(SeekError::Unsupported);
        }
        if frame >= self.total {
            return Err(SeekError::PastEnd);
        }
        let offset = frame
            .checked_mul(self.frame_bytes() as u64)
            .and_then(|b| b.checked_add(HEADER as u64))
            .ok_or(SeekError::PastEnd)?;
        self.start = 0;
        self.pending = 0;
        self.done = frame;
        Ok(offset)
    }
}

/// 资源需求：QYT1 没有大于一帧的输入单元，也不需要额外工作区，用默认实现（不额外说明）。
impl Resources for Qyt1Decoder {}

impl core::fmt::Display for Qyt1Detail {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Magic => f.write_str("魔数不是 QYT1 或 QYL1"),
            Self::Header => f.write_str("文件头的采样率、声道数或位数不合规定"),
            Self::SampleRange { frame } => write!(f, "第 {frame} 帧的样本超出声明位数"),
            Self::Truncated { frames } => write!(f, "数据在第 {frames} 帧处提前结束"),
        }
    }
}

/// 生成一个 QYT1 文件（示例与测试用）：`samples` 为交错样本，必须在 `bits` 位有符号范围内。
pub fn encode(rate: u32, channels: u16, bits: u16, samples: &[i32]) -> Vec<u8> {
    encode_with(MAGIC, rate, channels, bits, samples)
}

/// 生成一个 QYL1 文件（有损变体，E1）：布局与 [`encode`] 相同。
pub fn encode_lossy(rate: u32, channels: u16, bits: u16, samples: &[i32]) -> Vec<u8> {
    encode_with(MAGIC_LOSSY, rate, channels, bits, samples)
}

fn encode_with(magic: &[u8; 4], rate: u32, channels: u16, bits: u16, samples: &[i32]) -> Vec<u8> {
    let bytes = usize::from(bits).div_ceil(8);
    let frames = samples.len() / usize::from(channels.max(1));
    let mut v = Vec::with_capacity(HEADER + samples.len() * bytes);
    v.extend_from_slice(magic);
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&channels.to_le_bytes());
    v.extend_from_slice(&bits.to_le_bytes());
    v.extend_from_slice(&(frames as u64).to_le_bytes());
    for &s in samples {
        v.extend_from_slice(&s.to_le_bytes()[..bytes]);
    }
    v
}
