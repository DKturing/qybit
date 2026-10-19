//! 示例 DSD 格式 QYD1 的解码器（D1）：只为说明 DSD 输出接口而定义，代表 Full 插件的 DSF / DFF
//! 解码器，不是要支持的生产格式。
//!
//! 文件布局：
//!
//! | 偏移 | 长度 | 内容 |
//! | --- | --- | --- |
//! | 0 | 4 | 魔数 `QYD1` |
//! | 4 | 4 | 每声道 DSD 位率（Hz，小端，32 的正整数倍，如 2 822 400） |
//! | 8 | 2 | 声道数（小端，1–18，默认布局） |
//! | 10 | 2 | 保留，为 0 |
//! | 12 | 8 | DSD 字数（每声道，小端） |
//! | 20 | … | 交错的 DSD 字：每声道 4 字节，时间上最早的字节在前、每字节最早的位在最高位（DFF 的位序） |
//!
//! 解码输出即 [`SourceFormat::dsd`] 的约定：每帧每声道一个 32 位字，最早的位在 bit 31。不做任何
//! 换算——DSD 码流原样交给核心，由 pack 按设备的传输方式（原生或 DoP）写出。

use qy_core::engine::Resources;
use qy_core::{
    ChannelLayout, DecodeError, Decoder, Identify, Progress, SeekError, SourceFormat, Status,
    magic_matches,
};

/// 魔数。
pub const MAGIC: &[u8; 4] = b"QYD1";
/// 文件头字节数。
pub const HEADER: usize = 20;

/// 解码失败的明细。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Qyd1Detail {
    Magic,
    /// 位率不是 32 的正整数倍、声道数不在 1–18，或保留字段不为 0。
    Header,
    /// 数据在第 `words` 个字处提前结束。
    Truncated {
        words: u64,
    },
}

impl core::fmt::Display for Qyd1Detail {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Magic => f.write_str("魔数不是 QYD1"),
            Self::Header => f.write_str("文件头的位率、声道数或保留字段不合规定"),
            Self::Truncated { words } => write!(f, "数据在第 {words} 个 DSD 字处提前结束"),
        }
    }
}

fn parse_header(h: &[u8]) -> Result<(SourceFormat, u64), (Qyd1Detail, DecodeError)> {
    if &h[..4] != MAGIC {
        return Err((Qyd1Detail::Magic, DecodeError::Malformed));
    }
    let rate = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
    let channels = u16::from_le_bytes([h[8], h[9]]);
    let reserved = u16::from_le_bytes([h[10], h[11]]);
    let total = u64::from_le_bytes([h[12], h[13], h[14], h[15], h[16], h[17], h[18], h[19]]);
    let layout = ChannelLayout::default_for(channels)
        .or_else(|| ChannelLayout::flac_default(channels))
        .ok_or((Qyd1Detail::Header, DecodeError::Unsupported))?;
    if reserved != 0 {
        return Err((Qyd1Detail::Header, DecodeError::Malformed));
    }
    let format = SourceFormat::dsd(rate, layout)
        .map_err(|_| (Qyd1Detail::Header, DecodeError::Malformed))?;
    Ok((format, total))
}

/// 识别 QYD1 文件头（E3 的写法）。
pub fn identify(head: &[u8], eof: bool) -> Identify<Qyd1Detail> {
    match magic_matches(head, &[MAGIC], eof) {
        None => {
            return Identify::NeedMore {
                total: head.len() as u64 + 1,
                container: false,
            };
        }
        Some(false) => return Identify::Foreign,
        Some(true) => {}
    }
    let Some(h) = head.get(..HEADER) else {
        if eof {
            return Identify::Rejected {
                error: DecodeError::Truncated,
                detail: Some(Qyd1Detail::Truncated { words: 0 }),
            };
        }
        return Identify::NeedMore {
            total: HEADER as u64,
            container: true,
        };
    };
    match parse_header(h) {
        Ok((format, _)) => Identify::Supported {
            format,
            data_start: HEADER as u64,
        },
        Err((detail, error)) => Identify::Rejected {
            error,
            detail: Some(detail),
        },
    }
}

/// QYD1 解码器。
pub struct Qyd1Decoder {
    pcm: &'static mut [i32],
    format: Option<SourceFormat>,
    channels: usize,
    total: u64,
    done: u64,
    start: usize,
    pending: usize,
    failed: Option<Qyd1Detail>,
}

impl Qyd1Decoder {
    /// `pcm` 至少能放一帧 18 声道的字。
    pub fn new(pcm: &'static mut [i32]) -> Result<Self, DecodeError> {
        if pcm.len() < qy_core::MAX_CHANNELS as usize {
            return Err(DecodeError::CapacityExceeded);
        }
        Ok(Self {
            pcm,
            format: None,
            channels: 0,
            total: 0,
            done: 0,
            start: 0,
            pending: 0,
            failed: None,
        })
    }

    pub fn last_detail(&self) -> Option<Qyd1Detail> {
        self.failed
    }

    /// 文件头声明的每声道 DSD 字数。
    pub fn declared_frames(&self) -> Option<u64> {
        self.format.map(|_| self.total)
    }

    fn fail(&mut self, d: Qyd1Detail, e: DecodeError) -> Result<Progress, DecodeError> {
        self.failed = Some(d);
        Err(e)
    }
}

impl Decoder for Qyd1Decoder {
    fn reset(&mut self) {
        self.format = None;
        self.channels = 0;
        self.total = 0;
        self.done = 0;
        self.start = 0;
        self.pending = 0;
        self.failed = None;
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
        let mut consumed = 0;
        if self.format.is_none() {
            let Some(h) = input.get(..HEADER) else {
                if eof {
                    return self.fail(Qyd1Detail::Truncated { words: 0 }, DecodeError::Truncated);
                }
                return Ok(Progress {
                    consumed: 0,
                    status: Status::NeedInput,
                });
            };
            match parse_header(h) {
                Ok((f, total)) => {
                    self.format = Some(f);
                    self.channels = usize::from(f.channels());
                    self.total = total;
                }
                Err((d, e)) => return self.fail(d, e),
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
        let fb = self.channels * 4;
        let left = usize::try_from(self.total - self.done).unwrap_or(usize::MAX);
        let n = (rest.len() / fb)
            .min(left)
            .min(self.pcm.len() / self.channels);
        if n == 0 {
            if eof {
                return self.fail(
                    Qyd1Detail::Truncated { words: self.done },
                    DecodeError::Truncated,
                );
            }
            return Ok(Progress {
                consumed,
                status: Status::NeedInput,
            });
        }
        for (o, b) in self.pcm.iter_mut().zip(rest[..n * fb].chunks_exact(4)) {
            *o = i32::from_be_bytes([b[0], b[1], b[2], b[3]]);
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

    /// 按字直接定位（DSD 没有帧间依赖）。
    fn seek(&mut self, frame: u64) -> Result<u64, SeekError> {
        if frame >= self.total {
            return Err(SeekError::PastEnd);
        }
        let offset = frame
            .checked_mul(self.channels as u64 * 4)
            .and_then(|b| b.checked_add(HEADER as u64))
            .ok_or(SeekError::PastEnd)?;
        self.start = 0;
        self.pending = 0;
        self.done = frame;
        Ok(offset)
    }
}

impl Resources for Qyd1Decoder {}

impl qy_player::Diagnostics for Qyd1Decoder {
    fn failure_details(&self, out: &mut Vec<String>) {
        if let Some(d) = self.failed {
            out.push(format!("QYD1 错误明细：{d}"));
        }
    }
}

/// 生成一个 QYD1 文件（测试用）：`words` 为交错的 DSD 字（最早的位在 bit 31）。
pub fn encode(bit_rate: u32, channels: u16, words: &[i32]) -> Vec<u8> {
    let frames = words.len() / usize::from(channels.max(1));
    let mut v = Vec::with_capacity(HEADER + words.len() * 4);
    v.extend_from_slice(MAGIC);
    v.extend_from_slice(&bit_rate.to_le_bytes());
    v.extend_from_slice(&channels.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&(frames as u64).to_le_bytes());
    for &w in words {
        v.extend_from_slice(&w.to_be_bytes());
    }
    v
}
