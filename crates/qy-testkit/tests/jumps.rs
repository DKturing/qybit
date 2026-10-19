//! E6：契约测试对读取跳转的检查。一个正确的极简“索引在后”解码器通过 `check`、`check_seek` 与
//! `check_identify`；故意违反跳转约定的变体各自被发现。

use qy_core::{DecodeError, Decoder, Identify, Progress, SeekError, SourceFormat, Status};
use qy_testkit::{SeekSupport, check, check_identify, check_seek};

/// 格式：`QJMP` + 索引位置（u32 LE）；数据区为 16 位小端样本；索引为采样率（u32）、声道数（u16）、
/// 数据位置（u32）、帧数（u32），共 14 字节。
const HEAD: usize = 8;
const INDEX: usize = 14;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fault {
    None,
    /// reset 不清除未取走的跳转。
    ResetKeepsJump,
    /// 返回 End 时给出跳转。
    JumpAtEnd,
    /// 跳到索引时同时给出 seek_request。
    BothJumps,
    /// 跳到索引之后、什么都没读就再跳一次。
    JumpWithoutReading,
    /// 读完索引后跳回文件头重读（读到了字节，但来回循环）。
    PingPong,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Head,
    Index,
    Data,
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Detail {
    Magic,
    Truncated,
}

struct Qjmp {
    fault: Fault,
    state: State,
    index_at: u64,
    format: Option<SourceFormat>,
    data_at: u64,
    total: u64,
    done: u64,
    pcm: Vec<i32>,
    start: usize,
    jump: Option<u64>,
    seek_req: Option<u64>,
    /// 刚跳转、还没看到新位置的输入（`JumpWithoutReading` 用）。
    fresh: bool,
    failed: Option<Detail>,
}

impl Qjmp {
    fn new(fault: Fault) -> Self {
        Self {
            fault,
            state: State::Head,
            index_at: 0,
            format: None,
            data_at: 0,
            total: 0,
            done: 0,
            pcm: Vec::new(),
            start: 0,
            jump: None,
            seek_req: None,
            fresh: false,
            failed: None,
        }
    }

    fn channels(&self) -> usize {
        self.format.map_or(1, |f| usize::from(f.channels()))
    }

    fn fail(&mut self, d: Detail, e: DecodeError) -> Result<Progress, DecodeError> {
        self.failed = Some(d);
        Err(e)
    }

    fn go(&mut self, to: u64) {
        self.jump = Some(to);
        self.fresh = true;
    }
}

fn need(consumed: usize) -> Result<Progress, DecodeError> {
    Ok(Progress {
        consumed,
        status: Status::NeedInput,
    })
}

fn parse_index(b: &[u8]) -> Option<(SourceFormat, u64, u64)> {
    let rate = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let ch = u16::from_le_bytes([b[4], b[5]]);
    let data = u32::from_le_bytes([b[6], b[7], b[8], b[9]]);
    let frames = u32::from_le_bytes([b[10], b[11], b[12], b[13]]);
    let f = SourceFormat::new(rate, ch, 16).ok()?;
    Some((f, u64::from(data), u64::from(frames)))
}

impl Decoder for Qjmp {
    fn reset(&mut self) {
        let keep = (self.fault == Fault::ResetKeepsJump)
            .then_some(self.jump)
            .flatten();
        *self = Self::new(self.fault);
        if keep.is_some() {
            self.jump = keep;
        } else if self.fault == Fault::ResetKeepsJump {
            // 第一次 reset 之前没有跳转：人为留一个，模拟“上一首的跳转未清除”。
            self.jump = Some(0);
        }
    }

    fn format(&self) -> Option<SourceFormat> {
        self.format
    }

    fn decode(&mut self, input: &[u8], eof: bool) -> Result<Progress, DecodeError> {
        if self.failed.is_some() {
            return Err(DecodeError::Malformed);
        }
        if self.start < self.pcm.len() {
            return Ok(Progress {
                consumed: 0,
                status: Status::NeedOutput,
            });
        }
        if self.fresh && input.is_empty() && self.fault == Fault::JumpWithoutReading {
            self.go(self.index_at);
            return need(0);
        }
        self.fresh = false;
        match self.state {
            State::Head => {
                let Some(h) = input.get(..HEAD) else {
                    if eof {
                        return self.fail(Detail::Truncated, DecodeError::Truncated);
                    }
                    return need(0);
                };
                if &h[..4] != b"QJMP" {
                    return self.fail(Detail::Magic, DecodeError::Malformed);
                }
                self.index_at = u64::from(u32::from_le_bytes([h[4], h[5], h[6], h[7]]));
                self.state = State::Index;
                self.go(self.index_at);
                if self.fault == Fault::BothJumps {
                    self.seek_req = Some(self.index_at);
                }
                need(HEAD)
            }
            State::Index => {
                let Some(b) = input.get(..INDEX) else {
                    if eof {
                        return self.fail(Detail::Truncated, DecodeError::Truncated);
                    }
                    return need(0);
                };
                if self.fault == Fault::PingPong {
                    self.state = State::Head;
                    self.go(0);
                    return need(INDEX);
                }
                let Some((f, data, frames)) = parse_index(b) else {
                    return self.fail(Detail::Magic, DecodeError::Unsupported);
                };
                self.format = Some(f);
                self.data_at = data;
                self.total = frames;
                self.state = State::Data;
                self.go(data);
                need(INDEX)
            }
            State::Data => {
                if self.done >= self.total {
                    self.state = State::Done;
                    if self.fault == Fault::JumpAtEnd {
                        self.jump = Some(0);
                    }
                    return Ok(Progress {
                        consumed: 0,
                        status: Status::End,
                    });
                }
                let fb = 2 * self.channels();
                let left = usize::try_from(self.total - self.done).unwrap_or(usize::MAX);
                let n = (input.len() / fb).min(left).min(64);
                if n == 0 {
                    if eof {
                        return self.fail(Detail::Truncated, DecodeError::Truncated);
                    }
                    return need(0);
                }
                self.pcm = input[..n * fb]
                    .chunks_exact(2)
                    .map(|b| i32::from(i16::from_le_bytes([b[0], b[1]])))
                    .collect();
                self.start = 0;
                self.done += n as u64;
                Ok(Progress {
                    consumed: n * fb,
                    status: Status::Frames(n),
                })
            }
            State::Done => Ok(Progress {
                consumed: 0,
                status: Status::End,
            }),
        }
    }

    fn pcm(&self) -> &[i32] {
        &self.pcm[self.start..]
    }

    fn release(&mut self, frames: usize) {
        self.start = (self.start + frames * self.channels()).min(self.pcm.len());
    }

    fn seek(&mut self, frame: u64) -> Result<u64, SeekError> {
        if frame >= self.total {
            return Err(SeekError::PastEnd);
        }
        self.pcm.clear();
        self.start = 0;
        self.done = frame;
        self.state = State::Data;
        self.jump = None;
        Ok(self.data_at + frame * 2 * self.channels() as u64)
    }

    fn seek_request(&mut self) -> Option<u64> {
        self.seek_req.take()
    }

    fn take_jump(&mut self) -> Option<u64> {
        self.jump.take()
    }
}

/// 文件头识别：前缀里没有索引时回答 `NeedMore { container: true }`（E6 §2.5）。
fn identify(head: &[u8], eof: bool) -> Identify<Detail> {
    match qy_core::magic_matches(head, &[b"QJMP"], eof) {
        None => {
            return Identify::NeedMore {
                total: head.len() as u64 + 1,
                container: false,
            };
        }
        Some(false) => return Identify::Foreign,
        Some(true) => {}
    }
    let truncated = Identify::Rejected {
        error: DecodeError::Truncated,
        detail: Some(Detail::Truncated),
    };
    let Some(h) = head.get(..HEAD) else {
        return if eof {
            truncated
        } else {
            Identify::NeedMore {
                total: HEAD as u64,
                container: true,
            }
        };
    };
    let at = u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as usize;
    let Some(b) = head.get(at..).and_then(|r| r.get(..INDEX)) else {
        return if eof {
            truncated
        } else {
            Identify::NeedMore {
                total: (at + INDEX) as u64,
                container: true,
            }
        };
    };
    match parse_index(b) {
        Some((format, data, _)) => Identify::Supported {
            format,
            data_start: data,
        },
        None => Identify::Rejected {
            error: DecodeError::Unsupported,
            detail: Some(Detail::Magic),
        },
    }
}

/// 生成文件：文件头、数据（前面有 `gap` 字节填充）、索引。
fn file(frames: u32, ch: u16, gap: usize) -> Vec<u8> {
    let data_at = HEAD + gap;
    let samples: Vec<i16> = (0..frames as usize * usize::from(ch))
        .map(|i| (i as i32 * 37 - 9_000) as i16)
        .collect();
    let index_at = data_at + samples.len() * 2;
    let mut v = Vec::new();
    v.extend_from_slice(b"QJMP");
    v.extend_from_slice(&(index_at as u32).to_le_bytes());
    v.resize(data_at, 0xAB);
    for s in &samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v.extend_from_slice(&48_000u32.to_le_bytes());
    v.extend_from_slice(&ch.to_le_bytes());
    v.extend_from_slice(&(data_at as u32).to_le_bytes());
    v.extend_from_slice(&frames.to_le_bytes());
    v
}

fn run(fault: Fault) -> Result<(), String> {
    let a = file(300, 2, 5);
    let b = file(1, 1, 0);
    let mut truncated = file(10, 1, 3);
    truncated.truncate(truncated.len() - 4); // 索引截断
    let srcs: Vec<&[u8]> = vec![&a, &b, &truncated];
    check(|| Qjmp::new(fault), &srcs).map_err(|v| v.0)?;
    match check_seek(|| Qjmp::new(fault), &a, &[0, 1, 150, 299, 300]) {
        Ok(SeekSupport::Supported) => Ok(()),
        Ok(other) => Err(format!("{other:?}")),
        Err(v) => Err(v.0),
    }
}

#[test]
fn correct_jumping_decoder_passes() {
    assert_eq!(run(Fault::None), Ok(()));
    let mut d = Qjmp::new(Fault::None);
    let mut truncated = file(10, 1, 3);
    truncated.truncate(truncated.len() - 4);
    let mut bad = file(10, 1, 3);
    bad[0] = b'X';
    for src in [file(300, 2, 5), file(1, 1, 0), truncated, bad] {
        if let Err(v) = check_identify(identify, &mut d, |d| d.failed, &src) {
            panic!("{v}");
        }
    }
    // 前缀不含索引：已认领容器。
    assert_eq!(
        identify(&file(300, 2, 5)[..100], false),
        Identify::NeedMore {
            total: 8 + 5 + 1200 + 14,
            container: true
        }
    );
}

#[test]
fn each_jump_violation_is_found() {
    for (fault, needle) in [
        (Fault::ResetKeepsJump, "reset 之后仍有未取走的跳转"),
        (Fault::JumpAtEnd, "Protocol"),
        (Fault::BothJumps, "Protocol"),
        (Fault::JumpWithoutReading, "NoProgress"),
        (Fault::PingPong, "来回跳转"),
    ] {
        match run(fault) {
            Ok(()) => panic!("{fault:?} 未被发现"),
            Err(e) => assert!(e.contains(needle), "{fault:?}：{e}"),
        }
    }
}
