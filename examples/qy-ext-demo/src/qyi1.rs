//! 示例格式 QYI1 的解码器（E6）：索引在文件末尾的容器，只为说明“解码需要的读取跳转”
//! （`Decoder::take_jump`）而定义，代表 M4A 这类 `moov` 在 `mdat` 之后的文件，不是要支持的生产格式。
//!
//! 文件布局（整数均为小端）：
//!
//! | 位置 | 长度 | 内容 |
//! | --- | --- | --- |
//! | 0 | 4 | 魔数 `QYI1` |
//! | 4 | 8 | 索引位置（距文件开头，不小于 12） |
//! | 12 | … | 数据区：若干个包，每包为交错的 16 位整数样本；包之间可以有填充 |
//! | 索引位置 | 16 | `IDX1`、采样率（u32）、声道数（u16，1–8，默认布局）、保留（u16，为 0）、包数（u32，≥ 1） |
//! | 其后 | 12 × 包数 | 每包：位置（u64）、帧数（u32，≥ 1）；按位置递增、互不重叠，都在文件头之后、索引之前 |
//!
//! 解码过程：读文件头 → 跳到索引（文件头阶段的跳转）→ 读完并核对整个索引后才给出格式 → 跳到第一个包
//! → 顺序解码；下一包不紧接在当前包之后时，在产出当前包最后一批样本的同一次调用中请求跳过去（数据
//! 阶段的跳转）。定位按包直接定位，不需要预滚。索引表存放在宿主提供的静态切片中，包数超过容量时
//! 明确报错（`CapacityExceeded`）。

use qy_core::engine::Resources;
use qy_core::{
    DecodeError, Decoder, Identify, Progress, SeekError, SourceFormat, Status, magic_matches,
};

/// 魔数。
pub const MAGIC: &[u8; 4] = b"QYI1";
/// 文件头字节数。
pub const HEAD: usize = 12;
/// 索引的魔数。
pub const INDEX_MAGIC: &[u8; 4] = b"IDX1";
/// 索引头字节数。
pub const INDEX_HEAD: usize = 16;
/// 每个索引项的字节数。
pub const ENTRY: usize = 12;

/// 索引中的一个包。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Packet {
    pub offset: u64,
    pub frames: u32,
}

/// 解码失败的明细。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Qyi1Detail {
    Magic,
    /// 文件不足 12 字节的文件头。
    HeadTruncated,
    /// 索引位置落在文件头之内（非法偏移）。
    IndexOffset {
        at: u64,
    },
    IndexMagic,
    /// 采样率或声道数不合规定、保留字段不为 0，或包数为 0。
    IndexHeader,
    /// 第 `index` 个包（从 0 起）的位置或长度不合规定：落在文件头内、与前一包重叠、越过索引，或帧数为 0。
    PacketRange {
        index: u32,
    },
    /// 包数超过宿主提供的索引表容量。
    TooManyPackets {
        count: u32,
        capacity: usize,
    },
    /// 跳到索引位置后立即是文件末尾（索引位置在文件末尾或之后）。
    IndexPastEnd {
        at: u64,
    },
    /// 索引在文件末尾被截断。
    IndexTruncated,
    /// 数据在第 `frame` 帧处提前结束。
    DataTruncated {
        frame: u64,
    },
}

impl core::fmt::Display for Qyi1Detail {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Magic => f.write_str("魔数不是 QYI1 / IDX1"),
            Self::HeadTruncated => f.write_str("文件不足 12 字节的文件头"),
            Self::IndexOffset { at } => write!(f, "索引位置 {at} 落在文件头之内"),
            Self::IndexMagic => f.write_str("索引的魔数不是 IDX1"),
            Self::IndexHeader => f.write_str("索引头的采样率、声道数、保留字段或包数不合规定"),
            Self::PacketRange { index } => write!(
                f,
                "第 {index} 个包的位置或长度不合规定（在文件头内、与前一包重叠、越过索引或帧数为 0）"
            ),
            Self::TooManyPackets { count, capacity } => {
                write!(f, "包数 {count} 超过索引表容量 {capacity}")
            }
            Self::IndexPastEnd { at } => write!(f, "索引位置 {at} 在文件末尾或之后"),
            Self::IndexTruncated => f.write_str("索引在文件末尾被截断"),
            Self::DataTruncated { frame } => write!(f, "数据在第 {frame} 帧处提前结束"),
        }
    }
}

type Fail = (Qyi1Detail, DecodeError);

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn u64_at(b: &[u8], i: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[i..i + 8]);
    u64::from_le_bytes(a)
}

/// 文件头（12 字节）→ 索引位置。
fn parse_head(h: &[u8]) -> Result<u64, Fail> {
    if &h[..4] != MAGIC {
        return Err((Qyi1Detail::Magic, DecodeError::Malformed));
    }
    let at = u64_at(h, 4);
    if at < HEAD as u64 {
        return Err((Qyi1Detail::IndexOffset { at }, DecodeError::Malformed));
    }
    Ok(at)
}

/// 索引头（16 字节）→ 格式与包数。
fn parse_index_head(b: &[u8]) -> Result<(SourceFormat, u32), Fail> {
    if &b[..4] != INDEX_MAGIC {
        return Err((Qyi1Detail::IndexMagic, DecodeError::Malformed));
    }
    let rate = u32_at(b, 4);
    let channels = u16::from_le_bytes([b[8], b[9]]);
    let reserved = u16::from_le_bytes([b[10], b[11]]);
    let count = u32_at(b, 12);
    if reserved != 0 || count == 0 || !(1..=8).contains(&channels) {
        return Err((Qyi1Detail::IndexHeader, DecodeError::Malformed));
    }
    let format = SourceFormat::new(rate, channels, 16)
        .map_err(|_| (Qyi1Detail::IndexHeader, DecodeError::Malformed))?;
    Ok((format, count))
}

/// 第 `index` 个索引项（12 字节）：`prev_end` 为前一包的结束位置（第一包为文件头末尾）。返回包与
/// 它的结束位置。
fn parse_entry(
    e: &[u8],
    index: u32,
    prev_end: u64,
    index_at: u64,
    frame_bytes: u64,
) -> Result<(Packet, u64), Fail> {
    let p = Packet {
        offset: u64_at(e, 0),
        frames: u32_at(e, 8),
    };
    let end = u64::from(p.frames)
        .checked_mul(frame_bytes)
        .and_then(|n| p.offset.checked_add(n));
    match end {
        Some(end) if p.frames > 0 && p.offset >= prev_end && end <= index_at => Ok((p, end)),
        _ => Err((Qyi1Detail::PacketRange { index }, DecodeError::Malformed)),
    }
}

/// 识别 QYI1 文件头（E3）。`head` 不含索引时回答 `NeedMore { container: true }`：已认领这个容器，
/// 路由层可以提前转交，不必等到 `total` 字节（E6 §2.5）。
pub fn identify(head: &[u8], eof: bool) -> Identify<Qyi1Detail> {
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
    let reject = |(detail, error): Fail| Identify::Rejected {
        error,
        detail: Some(detail),
    };
    let more = |total: u64, detail: Qyi1Detail| {
        if eof {
            reject((detail, DecodeError::Truncated))
        } else {
            Identify::NeedMore {
                total,
                container: true,
            }
        }
    };
    let Some(h) = head.get(..HEAD) else {
        return more(HEAD as u64, Qyi1Detail::HeadTruncated);
    };
    let at = match parse_head(h) {
        Ok(at) => at,
        Err(e) => return reject(e),
    };
    let index = usize::try_from(at).ok().and_then(|a| head.get(a..));
    let Some(b) = index.and_then(|r| r.get(..INDEX_HEAD)) else {
        let past = index.is_none_or(<[u8]>::is_empty);
        return more(
            at.saturating_add(INDEX_HEAD as u64),
            if past {
                Qyi1Detail::IndexPastEnd { at }
            } else {
                Qyi1Detail::IndexTruncated
            },
        );
    };
    let (format, count) = match parse_index_head(b) {
        Ok(v) => v,
        Err(e) => return reject(e),
    };
    let fb = u64::from(format.channels()) * 2;
    let entries = index.map_or(&[][..], |r| &r[INDEX_HEAD..]);
    let mut prev_end = HEAD as u64;
    let mut first = None;
    for i in 0..count {
        let Some(e) = entries.get(i as usize * ENTRY..(i as usize + 1) * ENTRY) else {
            // 下一项到齐就可能给出结论（它不合规定时），下界取到下一项为止。
            let total = at + INDEX_HEAD as u64 + (u64::from(i) + 1) * ENTRY as u64;
            return more(total, Qyi1Detail::IndexTruncated);
        };
        match parse_entry(e, i, prev_end, at, fb) {
            Ok((p, end)) => {
                first.get_or_insert(p.offset);
                prev_end = end;
            }
            Err(e) => return reject(e),
        }
    }
    Identify::Supported {
        format,
        data_start: first.unwrap_or(HEAD as u64),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Head,
    IndexHead,
    Entries {
        left: u32,
        prev_end: u64,
    },
    /// 第 `i` 个包，还剩 `left` 帧。
    Packet {
        i: usize,
        left: u64,
    },
    Done,
}

/// QYI1 解码器。
pub struct Qyi1Decoder {
    pcm: &'static mut [i32],
    table: &'static mut [Packet],
    len: usize,
    state: State,
    index_at: u64,
    /// 解码器下一次看到的输入在文件中的位置（跳转后为跳到的位置）。
    pos: u64,
    format: Option<SourceFormat>,
    pending_format: Option<SourceFormat>,
    total: u64,
    done: u64,
    start: usize,
    pending: usize,
    jump: Option<u64>,
    failed: Option<Qyi1Detail>,
}

impl Qyi1Decoder {
    /// `pcm` 至少放得下一帧 8 声道；`table` 为索引表（容量即最多包数）。
    pub fn new(pcm: &'static mut [i32], table: &'static mut [Packet]) -> Result<Self, DecodeError> {
        if pcm.len() < 8 || table.is_empty() {
            return Err(DecodeError::CapacityExceeded);
        }
        Ok(Self {
            pcm,
            table,
            len: 0,
            state: State::Head,
            index_at: 0,
            pos: 0,
            format: None,
            pending_format: None,
            total: 0,
            done: 0,
            start: 0,
            pending: 0,
            jump: None,
            failed: None,
        })
    }

    pub fn last_detail(&self) -> Option<Qyi1Detail> {
        self.failed
    }

    /// 索引声明的总帧数（索引读完之后）。
    pub fn declared_frames(&self) -> Option<u64> {
        self.format.map(|_| self.total)
    }

    fn channels(&self) -> usize {
        self.format
            .or(self.pending_format)
            .map_or(1, |f| usize::from(f.channels()))
    }

    fn fail(&mut self, (d, e): Fail) -> Result<Progress, DecodeError> {
        self.failed = Some(d);
        Err(e)
    }

    /// 请求跳到 `to`（返回 `NeedInput`，已消耗 `consumed` 字节）。
    fn go(&mut self, to: u64, consumed: usize) -> Result<Progress, DecodeError> {
        self.jump = Some(to);
        self.pos = to;
        Ok(Progress {
            consumed,
            status: Status::NeedInput,
        })
    }
}

fn need(consumed: usize) -> Result<Progress, DecodeError> {
    Ok(Progress {
        consumed,
        status: Status::NeedInput,
    })
}

impl Decoder for Qyi1Decoder {
    fn reset(&mut self) {
        self.len = 0;
        self.state = State::Head;
        self.index_at = 0;
        self.pos = 0;
        self.format = None;
        self.pending_format = None;
        self.total = 0;
        self.done = 0;
        self.start = 0;
        self.pending = 0;
        self.jump = None;
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
        let mut c = 0usize;
        loop {
            let rest = &input[c..];
            match self.state {
                State::Head => {
                    let Some(h) = rest.get(..HEAD) else {
                        if eof {
                            return self.fail((Qyi1Detail::HeadTruncated, DecodeError::Truncated));
                        }
                        return need(c);
                    };
                    match parse_head(h) {
                        Ok(at) => self.index_at = at,
                        Err(e) => return self.fail(e),
                    }
                    self.state = State::IndexHead;
                    return self.go(self.index_at, c + HEAD);
                }
                State::IndexHead => {
                    let Some(b) = rest.get(..INDEX_HEAD) else {
                        if eof {
                            let d = if rest.is_empty() {
                                Qyi1Detail::IndexPastEnd { at: self.index_at }
                            } else {
                                Qyi1Detail::IndexTruncated
                            };
                            return self.fail((d, DecodeError::Truncated));
                        }
                        return need(c);
                    };
                    let (format, count) = match parse_index_head(b) {
                        Ok(v) => v,
                        Err(e) => return self.fail(e),
                    };
                    if count as usize > self.table.len() {
                        return self.fail((
                            Qyi1Detail::TooManyPackets {
                                count,
                                capacity: self.table.len(),
                            },
                            DecodeError::CapacityExceeded,
                        ));
                    }
                    self.pending_format = Some(format);
                    c += INDEX_HEAD;
                    self.pos += INDEX_HEAD as u64;
                    self.state = State::Entries {
                        left: count,
                        prev_end: HEAD as u64,
                    };
                }
                State::Entries { left, prev_end } => {
                    if left == 0 {
                        // 索引全部核对完：格式此时才确定，跳到第一个包。
                        self.format = self.pending_format;
                        self.total = self.table[..self.len]
                            .iter()
                            .map(|p| u64::from(p.frames))
                            .sum();
                        let first = self.table[0];
                        self.state = State::Packet {
                            i: 0,
                            left: u64::from(first.frames),
                        };
                        return self.go(first.offset, c);
                    }
                    let Some(e) = rest.get(..ENTRY) else {
                        if eof {
                            return self.fail((Qyi1Detail::IndexTruncated, DecodeError::Truncated));
                        }
                        return need(c);
                    };
                    let fb = self.channels() as u64 * 2;
                    let i = self.len as u32;
                    match parse_entry(e, i, prev_end, self.index_at, fb) {
                        Ok((p, end)) => {
                            self.table[self.len] = p;
                            self.len += 1;
                            c += ENTRY;
                            self.pos += ENTRY as u64;
                            self.state = State::Entries {
                                left: left - 1,
                                prev_end: end,
                            };
                        }
                        Err(e) => return self.fail(e),
                    }
                }
                State::Packet { i, left } => {
                    let ch = self.channels();
                    let fb = ch * 2;
                    let cap = self.pcm.len() / ch;
                    let n = (rest.len() / fb)
                        .min(usize::try_from(left).unwrap_or(usize::MAX))
                        .min(cap);
                    if n == 0 {
                        if eof {
                            let frame = self.done;
                            return self.fail((
                                Qyi1Detail::DataTruncated { frame },
                                DecodeError::Truncated,
                            ));
                        }
                        return need(c);
                    }
                    for (o, b) in self.pcm.iter_mut().zip(rest[..n * fb].chunks_exact(2)) {
                        *o = i32::from(i16::from_le_bytes([b[0], b[1]]));
                    }
                    self.start = 0;
                    self.pending = n;
                    self.done += n as u64;
                    self.pos += (n * fb) as u64;
                    c += n * fb;
                    let left = left - n as u64;
                    self.state = match (left, self.table[..self.len].get(i + 1)) {
                        (0, None) => State::Done,
                        (0, Some(next)) => {
                            // 下一包不紧接在这里：在同一次调用中请求跳过去（数据阶段的跳转）。
                            if next.offset != self.pos {
                                self.jump = Some(next.offset);
                                self.pos = next.offset;
                            }
                            State::Packet {
                                i: i + 1,
                                left: u64::from(next.frames),
                            }
                        }
                        (left, _) => State::Packet { i, left },
                    };
                    return Ok(Progress {
                        consumed: c,
                        status: Status::Frames(n),
                    });
                }
                State::Done => {
                    return Ok(Progress {
                        consumed: c,
                        status: Status::End,
                    });
                }
            }
        }
    }

    fn pcm(&self) -> &[i32] {
        let ch = self.channels();
        &self.pcm[self.start * ch..(self.start + self.pending) * ch]
    }

    fn release(&mut self, frames: usize) {
        let n = frames.min(self.pending);
        self.start += n;
        self.pending -= n;
    }

    /// 按包直接定位：找到目标帧所在的包，返回它在包中的字节位置。
    fn seek(&mut self, frame: u64) -> Result<u64, SeekError> {
        if self.format.is_none() || frame >= self.total {
            return Err(SeekError::PastEnd);
        }
        let fb = self.channels() as u64 * 2;
        let mut first = 0u64;
        for (i, p) in self.table[..self.len].iter().enumerate() {
            let frames = u64::from(p.frames);
            if frame < first + frames {
                let within = frame - first;
                let offset = p.offset + within * fb;
                self.state = State::Packet {
                    i,
                    left: frames - within,
                };
                self.done = frame;
                self.start = 0;
                self.pending = 0;
                self.jump = None;
                self.pos = offset;
                return Ok(offset);
            }
            first += frames;
        }
        Err(SeekError::PastEnd)
    }

    fn take_jump(&mut self) -> Option<u64> {
        self.jump.take()
    }
}

impl Resources for Qyi1Decoder {}

impl qy_player::Diagnostics for Qyi1Decoder {
    fn failure_details(&self, out: &mut Vec<String>) {
        if let Some(d) = self.failed {
            out.push(format!("QYI1 错误明细：{d}"));
        }
    }
}

/// 生成一个 QYI1 文件（测试用）：每个包为（之前的填充字节数，交错样本）；索引写在最后。
pub fn encode(rate: u32, channels: u16, packets: &[(usize, &[i16])]) -> Vec<u8> {
    let ch = usize::from(channels.max(1));
    let mut v = Vec::new();
    v.extend_from_slice(MAGIC);
    v.extend_from_slice(&[0; 8]);
    let mut entries = Vec::new();
    for &(gap, samples) in packets {
        v.resize(v.len() + gap, 0xEE);
        entries.push((v.len() as u64, (samples.len() / ch) as u32));
        for s in samples {
            v.extend_from_slice(&s.to_le_bytes());
        }
    }
    let index_at = v.len() as u64;
    v[4..12].copy_from_slice(&index_at.to_le_bytes());
    v.extend_from_slice(INDEX_MAGIC);
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&channels.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (offset, frames) in entries {
        v.extend_from_slice(&offset.to_le_bytes());
        v.extend_from_slice(&frames.to_le_bytes());
    }
    v
}
