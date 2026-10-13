//! QyBit 自有推送式 WAV 解码器。
//!
//! `#![no_std]`、无 alloc；PCM 工作区由宿主以 `&'static mut [i32]` 提供。
//! 解码器不缓存输入：每次调用只看宿主传入的切片，返回实际消耗字节数，
//! 未消耗的字节由宿主（核心输入窗口）保留到下次调用（AGENTS §2）。
//! 任一结构所需的最大连续输入为 40 字节（fmt 前缀）或一帧（≤ 8 字节），
//! 因此任意输入分块都能推进。
//!
//! 支持范围（M0 T2）：
//! - RIFF/WAVE，小端；格式码 1（PCM）与 0xFFFE（EXTENSIBLE，子格式 PCM）。
//! - 容器 16/24/32 位，单/双声道，采样率 1..=192000。
//! - EXTENSIBLE 有效位 V ≤ C，左对齐于容器高位，读取时算术右移 C−V，
//!   输出右对齐 `i32`，N = V。容器低位填充按规范不属于样本值，被丢弃。
//! - 声道掩码：单声道 0 或 FrontCenter，双声道 0 或 FrontLeft|FrontRight；
//!   其他布局明确不支持，不做重映射。
//!
//! 错误分类：结构违反 RIFF/WAVE 规范为 [`DecodeError::Malformed`]；
//! 合法但本阶段不支持（8 位、浮点、>2 声道、RIFX/RF64 等）为
//! [`DecodeError::Unsupported`]；源在完整结构前结束为 [`DecodeError::Truncated`]。
//! 出错后解码器保持该错误，直到 [`Decoder::reset`]。
//!
//! 规范出处：Microsoft “Multimedia Programming Interface and Data
//! Specifications 1.0”（RIFF chunk 与 WORD 对齐填充）；
//! `WAVEFORMATEX`、`WAVEFORMATEXTENSIBLE`（mmreg.h / ksmedia.h，
//! 有效位、声道掩码、`KSDATAFORMAT_SUBTYPE_PCM`）。
#![no_std]

use qy_core::{DecodeError, Decoder, PCM_WORK_SAMPLES, Progress, SourceFormat, Status};

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
    Fmt { size: u32, pad: u32 },
    Skip { remaining: u32 },
    Data { remaining: u32, pad: u32 },
    End,
    Failed(DecodeError),
}

/// 解析后的 fmt 信息。
#[derive(Debug, Clone, Copy)]
struct Layout {
    format: SourceFormat,
    container_bytes: usize,
    shift: u32,
    block_align: usize,
    channels: usize,
    channel_mask: Option<u32>,
}

/// WAV 解码器。
pub struct WavDecoder {
    pcm: &'static mut [i32],
    state: State,
    /// RIFF 中 "WAVE" 之后尚未消耗的字节数。
    riff_remaining: u32,
    layout: Option<Layout>,
    data_seen: bool,
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
            riff_remaining: 0,
            layout: None,
            data_seen: false,
            start: 0,
            end: 0,
        })
    }

    /// 工作区容量（样本数）。
    pub fn capacity_samples(&self) -> usize {
        self.pcm.len()
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

    /// 从 RIFF 余量中扣除已消耗字节；调用前已保证不越界。
    fn take_riff(&mut self, n: u32) -> Result<(), DecodeError> {
        self.riff_remaining = self
            .riff_remaining
            .checked_sub(n)
            .ok_or(DecodeError::Malformed)?;
        Ok(())
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
        self.riff_remaining = size.checked_sub(4).ok_or(DecodeError::Malformed)?;
        Ok(())
    }

    /// 解析 chunk 头并进入对应状态。`size` 和填充都必须落在 RIFF 范围内；
    /// 仅当 RIFF 恰好在 chunk 体末尾结束时容忍缺失的最后一个填充字节。
    fn parse_chunk_header(&mut self, b: &[u8]) -> Result<(), DecodeError> {
        self.take_riff(8)?;
        let id = [b[0], b[1], b[2], b[3]];
        let size = u32_at(b, 4);
        let after_body = self
            .riff_remaining
            .checked_sub(size)
            .ok_or(DecodeError::Malformed)?;
        let pad = (size & 1).min(after_body);
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
                if size as usize % layout.block_align != 0 {
                    return Err(DecodeError::Malformed);
                }
                self.data_seen = true;
                State::Data {
                    remaining: size,
                    pad,
                }
            }
            _ => State::Skip {
                remaining: size + pad,
            },
        };
        Ok(())
    }

    fn parse_fmt(b: &[u8], size: u32) -> Result<Layout, DecodeError> {
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
        if !matches!(container_bits, 16 | 24 | 32) {
            return Err(DecodeError::Unsupported);
        }
        let container_bytes = container_bits / 8;
        let expected_align = channels
            .checked_mul(container_bytes)
            .ok_or(DecodeError::Malformed)?;
        if block_align != expected_align {
            return Err(DecodeError::Malformed);
        }
        // 多于 2 声道：多声道（声道映射）尚未实现，见 tasks/FORMATS.md F1。采样率不设软件上限。
        if channels > 2 || rate == 0 {
            return Err(DecodeError::Unsupported);
        }
        if let Some(mask) = channel_mask {
            if mask.count_ones() > u32::from(channels) {
                return Err(DecodeError::Malformed);
            }
            let ok = match channels {
                1 => mask == 0 || mask == SPEAKER_FRONT_CENTER,
                _ => mask == 0 || mask == SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
            };
            if !ok {
                return Err(DecodeError::Unsupported);
            }
        }
        let format = SourceFormat::new(rate, channels, valid_bits as u8)
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
                State::ChunkHeader => {
                    if self.riff_remaining == 0 {
                        if !self.data_seen {
                            return Err(DecodeError::Malformed);
                        }
                        self.state = State::End;
                        continue;
                    }
                    if self.riff_remaining < 8 {
                        return Err(DecodeError::Malformed);
                    }
                    if avail.len() < 8 {
                        return self.need_input(pos, eof);
                    }
                    self.parse_chunk_header(&avail[..8])?;
                    pos += 8;
                }
                State::Fmt { size, pad } => {
                    let want = size.min(FMT_EXT_LEN);
                    if avail.len() < want as usize {
                        return self.need_input(pos, eof);
                    }
                    let layout = Self::parse_fmt(&avail[..want as usize], size)?;
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
                        return self.need_input(pos, eof);
                    }
                    let n = remaining.min(u32::try_from(avail.len()).unwrap_or(u32::MAX));
                    self.take_riff(n)?;
                    pos += n as usize;
                    self.state = State::Skip {
                        remaining: remaining - n,
                    };
                }
                State::Data { remaining, pad } => {
                    if remaining == 0 {
                        self.state = State::Skip { remaining: pad };
                        continue;
                    }
                    let Some(layout) = self.layout else {
                        return Err(DecodeError::Malformed);
                    };
                    let ba = layout.block_align;
                    let frames = (avail.len() / ba)
                        .min(remaining as usize / ba)
                        .min(self.pcm.len() / layout.channels);
                    if frames == 0 {
                        return self.need_input(pos, eof);
                    }
                    let bytes = frames * ba;
                    let samples = self.convert(&layout, &avail[..bytes]);
                    let bytes32 = u32::try_from(bytes).map_err(|_| DecodeError::Malformed)?;
                    self.take_riff(bytes32)?;
                    pos += bytes;
                    self.start = 0;
                    self.end = samples;
                    self.state = State::Data {
                        remaining: remaining - bytes32,
                        pad,
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

impl Decoder for WavDecoder {
    fn reset(&mut self) {
        self.state = State::RiffHeader;
        self.riff_remaining = 0;
        self.layout = None;
        self.data_seen = false;
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
            Ok(p) => Ok(p),
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
