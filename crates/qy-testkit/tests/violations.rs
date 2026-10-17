//! 契约测试本身的测试：一个正确的极简解码器通过全部检查；故意违反某条约定的变体各自被发现。

use qy_core::{DecodeError, Decoder, Progress, SeekError, SourceFormat, Status};
use qy_testkit::{SeekSupport, check, check_seek};

/// 极简格式：`QRAW`、采样率（u32 LE）、声道数（u16 LE），之后是 16 位小端整数样本。
const HEADER: usize = 10;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    /// eof 时仍返回 NeedInput（应返回 End）。
    NeedInputAtEof,
    /// 文件头解析之前就给出格式。
    EarlyFormat,
    /// 输入只有 1 字节时把它丢掉（结果依赖分块）。
    DropsOddBytes,
    /// 出错后再次调用仍产出样本。
    FramesAfterError,
    /// reset 不清除格式。
    ResetKeepsFormat,
    /// 定位后多输出目标前一帧。
    SeekOneEarly,
}

struct Raw {
    fault: Fault,
    format: Option<SourceFormat>,
    pcm: Vec<i32>,
    pending: usize,
    failed: bool,
    seekable: bool,
}

impl Raw {
    fn new(fault: Fault, seekable: bool) -> Self {
        Self {
            fault,
            format: None,
            pcm: Vec::with_capacity(256),
            pending: 0,
            failed: false,
            seekable,
        }
    }

    fn channels(&self) -> usize {
        self.format.map_or(1, |f| usize::from(f.channels()))
    }
}

impl Decoder for Raw {
    fn reset(&mut self) {
        if self.fault != Fault::ResetKeepsFormat {
            self.format = None;
        }
        self.pcm.clear();
        self.pending = 0;
        self.failed = false;
    }

    fn format(&self) -> Option<SourceFormat> {
        if self.fault == Fault::EarlyFormat && self.format.is_none() {
            return SourceFormat::new(1, 1, 16).ok();
        }
        self.format
    }

    fn decode(&mut self, input: &[u8], eof: bool) -> Result<Progress, DecodeError> {
        if self.failed {
            if self.fault == Fault::FramesAfterError {
                self.pcm.clear();
                self.pcm.push(0);
                self.pending = 1;
                return Ok(Progress {
                    consumed: 0,
                    status: Status::Frames(1),
                });
            }
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
            if input.len() < HEADER {
                return if eof {
                    Err(DecodeError::Truncated)
                } else {
                    Ok(Progress {
                        consumed: 0,
                        status: Status::NeedInput,
                    })
                };
            }
            if &input[..4] != b"QRAW" {
                self.failed = true;
                return Err(DecodeError::Malformed);
            }
            let rate = u32::from_le_bytes([input[4], input[5], input[6], input[7]]);
            let ch = u16::from_le_bytes([input[8], input[9]]);
            let f = SourceFormat::new(rate, ch, 16).map_err(|_| DecodeError::Unsupported)?;
            self.format = Some(f);
            consumed = HEADER;
        }
        let rest = &input[consumed..];
        if self.fault == Fault::DropsOddBytes && rest.len() == 1 && !eof {
            return Ok(Progress {
                consumed: consumed + 1,
                status: Status::NeedInput,
            });
        }
        let frame = 2 * self.channels();
        let frames = (rest.len() / frame).min(64);
        if frames == 0 {
            if eof {
                if !rest.is_empty() {
                    self.failed = true;
                    return Err(DecodeError::Truncated);
                }
                if self.fault == Fault::NeedInputAtEof {
                    return Ok(Progress {
                        consumed,
                        status: Status::NeedInput,
                    });
                }
                return Ok(Progress {
                    consumed,
                    status: Status::End,
                });
            }
            return Ok(Progress {
                consumed,
                status: Status::NeedInput,
            });
        }
        self.pcm.clear();
        for s in rest[..frames * frame].chunks_exact(2) {
            self.pcm.push(i32::from(i16::from_le_bytes([s[0], s[1]])));
        }
        self.pending = frames;
        Ok(Progress {
            consumed: consumed + frames * frame,
            status: Status::Frames(frames),
        })
    }

    fn pcm(&self) -> &[i32] {
        &self.pcm[..self.pending * self.channels()]
    }

    fn release(&mut self, frames: usize) {
        let ch = self.channels();
        let n = frames.min(self.pending);
        self.pcm.drain(..n * ch);
        self.pending -= n;
    }

    fn seek(&mut self, frame: u64) -> Result<u64, SeekError> {
        if !self.seekable {
            return Err(SeekError::Unsupported);
        }
        self.pcm.clear();
        self.pending = 0;
        let ch = self.channels() as u64;
        let frame = if self.fault == Fault::SeekOneEarly {
            frame.saturating_sub(1)
        } else {
            frame
        };
        Ok(HEADER as u64 + frame * ch * 2)
    }
}

fn raw_file(frames: usize, ch: u16) -> Vec<u8> {
    let mut v = b"QRAW".to_vec();
    v.extend_from_slice(&44_100u32.to_le_bytes());
    v.extend_from_slice(&ch.to_le_bytes());
    for i in 0..frames * usize::from(ch) {
        v.extend_from_slice(&((i as i16).wrapping_mul(97)).to_le_bytes());
    }
    v
}

fn sources() -> Vec<Vec<u8>> {
    vec![
        raw_file(300, 2),
        raw_file(1, 1),
        raw_file(0, 2),
        // 截断：最后一帧不完整。
        raw_file(10, 2)[..HEADER + 39].to_vec(),
        b"NOPE0123456789".to_vec(),
    ]
}

fn run(fault: Fault) -> Result<(), String> {
    let srcs = sources();
    let refs: Vec<&[u8]> = srcs.iter().map(Vec::as_slice).collect();
    check(|| Raw::new(fault, true), &refs).map_err(|v| v.0)?;
    let s = check_seek(
        || Raw::new(fault, true),
        &srcs[0],
        &[0, 1, 150, 299, 300, 400],
    )
    .map_err(|v| v.0)?;
    assert_eq!(s, SeekSupport::Supported);
    Ok(())
}

#[test]
fn correct_decoder_passes() {
    assert_eq!(run(Fault::None), Ok(()));
    // 不支持定位：核心顺序解码，结果相同。
    let src = raw_file(300, 2);
    assert_eq!(
        check_seek(|| Raw::new(Fault::None, false), &src, &[0, 7, 299, 300]),
        Ok(SeekSupport::Unsupported)
    );
}

#[test]
fn each_violation_is_found() {
    let expect = [
        (Fault::NeedInputAtEof, "eof 为真时返回 NeedInput"),
        (Fault::EarlyFormat, "format() 不是 None"),
        (Fault::DropsOddBytes, "与一次性输入的结果不同"),
        (Fault::FramesAfterError, "出错之后再次调用仍产出样本"),
        (Fault::ResetKeepsFormat, "reset 之后 format() 不是 None"),
        (Fault::SeekOneEarly, "定位到"),
    ];
    for (fault, msg) in expect {
        let Err(e) = run(fault) else {
            panic!("违反约定的解码器通过了检查：{msg}");
        };
        assert!(e.contains(msg), "期望“{msg}”，实际：{e}");
    }
}

#[test]
fn errors_are_part_of_the_result() {
    let srcs = sources();
    let refs: Vec<&[u8]> = srcs.iter().map(Vec::as_slice).collect();
    let Ok(r) = check(|| Raw::new(Fault::None, true), &refs) else {
        panic!("check");
    };
    assert_eq!(r[0].frames(), 300);
    assert_eq!(
        r[3].ending,
        qy_testkit::Ending::Error(DecodeError::Truncated)
    );
    assert_eq!(
        r[4].ending,
        qy_testkit::Ending::Error(DecodeError::Malformed)
    );
}
