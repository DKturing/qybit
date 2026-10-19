//! E6：解码需要的读取跳转（tasks/E6.md §2）。脚本解码器按步骤读取、跳转与产出，并核对每次看到的
//! 输入都等于它认为所在的文件位置上的字节；测试宿主按动作执行真实的读取与定位。

use qy_core::{
    Action, Core, DecodeError, Decoder, Event, Fault, Generation, INPUT_WINDOW_BYTES, JumpFault,
    JumpGuard, ParseJump, Progress, SeekError, SourceFormat, Status,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// 读取并消耗 n 字节（文件头）。
    Read(usize),
    /// 此后格式已知。
    Format,
    /// 请求跳到该位置（返回 `NeedInput`）。
    Jump(u64),
    /// 读取 n 字节，每字节产出一帧（样本值即字节值）。
    Out(usize),
    /// 同 `Out`，并在同一次调用中请求跳转（数据阶段）。
    OutJump(usize, u64),
    /// 同一次调用同时给出两种跳转（违约）。
    Both(u64),
    /// 返回 `End` 的同时给出跳转（违约）。
    EndJump(u64),
    /// 不读取、反复请求跳到同一位置（违约）。
    Spin(u64),
    End,
}

struct Script {
    steps: &'static [Step],
    file: &'static [u8],
    idx: usize,
    pos: u64,
    format: bool,
    pcm: Vec<i32>,
    start: usize,
    jump: Option<u64>,
    seek_req: Option<u64>,
    /// 输入与所在位置的字节不符（应永远为假）。
    mismatch: bool,
    /// 用户定位：目标帧 → (字节位置, 继续执行的步骤)。
    seek_to: Option<(u64, usize)>,
}

impl Script {
    fn new(file: &'static [u8], steps: &'static [Step]) -> Self {
        Self {
            steps,
            file,
            idx: 0,
            pos: 0,
            format: false,
            pcm: Vec::new(),
            start: 0,
            jump: None,
            seek_req: None,
            mismatch: false,
            seek_to: None,
        }
    }

    fn check(&mut self, input: &[u8]) {
        let at = usize::try_from(self.pos).unwrap_or(usize::MAX);
        let want = self.file.get(at..).unwrap_or(&[]);
        if !want.starts_with(input) {
            self.mismatch = true;
        }
    }

    fn take_bytes(&mut self, input: &[u8], n: usize, consumed: &mut usize) -> Option<Vec<u8>> {
        let rest = &input[*consumed..];
        if rest.len() < n {
            return None;
        }
        *consumed += n;
        self.pos += n as u64;
        Some(rest[..n].to_vec())
    }
}

fn need(consumed: usize) -> Result<Progress, DecodeError> {
    Ok(Progress {
        consumed,
        status: Status::NeedInput,
    })
}

impl Decoder for Script {
    fn reset(&mut self) {
        self.idx = 0;
        self.pos = 0;
        self.format = false;
        self.pcm.clear();
        self.start = 0;
        self.jump = None;
        self.seek_req = None;
    }

    fn format(&self) -> Option<SourceFormat> {
        self.format.then(|| SourceFormat::new(8_000, 1, 16).ok())?
    }

    fn decode(&mut self, input: &[u8], eof: bool) -> Result<Progress, DecodeError> {
        if self.start < self.pcm.len() {
            return Ok(Progress {
                consumed: 0,
                status: Status::NeedOutput,
            });
        }
        self.check(input);
        let mut consumed = 0;
        loop {
            let Some(&step) = self.steps.get(self.idx) else {
                return Ok(Progress {
                    consumed,
                    status: Status::End,
                });
            };
            match step {
                Step::Read(n) => match self.take_bytes(input, n, &mut consumed) {
                    Some(_) => self.idx += 1,
                    None if eof => return Err(DecodeError::Truncated),
                    None => return need(consumed),
                },
                Step::Format => {
                    self.format = true;
                    self.idx += 1;
                }
                Step::Jump(o) => {
                    self.idx += 1;
                    self.jump = Some(o);
                    self.pos = o;
                    return need(consumed);
                }
                Step::Out(n) | Step::OutJump(n, _) => {
                    let Some(bytes) = self.take_bytes(input, n, &mut consumed) else {
                        if eof {
                            return Err(DecodeError::Truncated);
                        }
                        return need(consumed);
                    };
                    self.idx += 1;
                    self.pcm = bytes.into_iter().map(i32::from).collect();
                    self.start = 0;
                    if let Step::OutJump(_, o) = step {
                        self.jump = Some(o);
                        self.pos = o;
                    }
                    return Ok(Progress {
                        consumed,
                        status: Status::Frames(n),
                    });
                }
                Step::Both(o) => {
                    self.idx += 1;
                    self.jump = Some(o);
                    self.seek_req = Some(o);
                    return need(consumed);
                }
                Step::EndJump(o) => {
                    self.idx += 1;
                    self.jump = Some(o);
                    return Ok(Progress {
                        consumed,
                        status: Status::End,
                    });
                }
                Step::Spin(o) => {
                    self.jump = Some(o);
                    self.pos = o;
                    return need(consumed);
                }
                Step::End => {
                    return Ok(Progress {
                        consumed,
                        status: Status::End,
                    });
                }
            }
        }
    }

    fn pcm(&self) -> &[i32] {
        &self.pcm[self.start..]
    }

    fn release(&mut self, frames: usize) {
        self.start = (self.start + frames).min(self.pcm.len());
    }

    fn seek(&mut self, frame: u64) -> Result<u64, SeekError> {
        let _ = frame;
        let (offset, idx) = self.seek_to.ok_or(SeekError::Unsupported)?;
        self.pcm.clear();
        self.start = 0;
        self.idx = idx;
        self.pos = offset;
        Ok(offset)
    }

    fn seek_request(&mut self) -> Option<u64> {
        self.seek_req.take()
    }

    fn take_jump(&mut self) -> Option<u64> {
        self.jump.take()
    }
}

#[derive(Debug, Default)]
struct Outcome {
    out: Vec<i32>,
    opens: u32,
    seeks: Vec<u64>,
    parse: Vec<ParseJump>,
    failed: Option<Fault>,
    finished: bool,
    reads_after_eof: u32,
    sequential: bool,
}

/// 执行动作的测试宿主：`chunk` 为每次读取的上限（短读），`seekable` 为假时定位失败。
fn run(core: &mut Core<Script>, file: &[u8], seekable: bool, chunk: usize) -> Outcome {
    let mut o = Outcome::default();
    let mut pos = 0usize;
    let mut at_eof = false;
    for _ in 0..100_000 {
        let action = core.poll();
        if let Some(j) = core.take_parse_jump() {
            o.parse.push(j);
        }
        o.sequential |= core.take_sequential_seek();
        let Some(action) = action else {
            assert!(core.is_idle(), "测试宿主不应等待");
            return o;
        };
        let r = match action {
            Action::Open { generation, .. } => {
                o.opens += 1;
                pos = 0;
                at_eof = false;
                core.handle(Event::Opened { generation })
            }
            Action::Read {
                generation,
                request,
                max,
            } => {
                let rest = file.get(pos..).unwrap_or(&[]);
                let n = rest.len().min(max).min(chunk);
                if n == 0 {
                    if at_eof {
                        o.reads_after_eof += 1;
                    }
                    at_eof = true;
                    core.handle(Event::ReadEof {
                        generation,
                        request,
                    })
                } else {
                    let Some(t) = core.read_target(generation, request) else {
                        panic!("读取目标");
                    };
                    t[..n].copy_from_slice(&rest[..n]);
                    pos += n;
                    core.handle(Event::ReadDone {
                        generation,
                        request,
                        len: n,
                    })
                }
            }
            Action::Seek {
                generation,
                request,
                offset,
            } => {
                o.seeks.push(offset);
                if seekable {
                    pos = usize::try_from(offset).unwrap_or(usize::MAX);
                    at_eof = false;
                    core.handle(Event::SeekDone {
                        generation,
                        request,
                    })
                } else {
                    core.handle(Event::SeekFailed {
                        generation,
                        request,
                    })
                }
            }
            Action::Configure { generation, .. } => core.handle(Event::Configured { generation }),
            Action::Output { generation, frames } => {
                let Some(pcm) = core.pcm() else {
                    panic!("输出样本");
                };
                o.out.extend_from_slice(&pcm.samples()[..frames]);
                core.handle(Event::Output { generation, frames })
            }
            Action::Close { .. } => Ok(()),
            Action::Finished { .. } => {
                o.finished = true;
                Ok(())
            }
            Action::Failed { fault, .. } => {
                o.failed = Some(fault);
                Ok(())
            }
        };
        assert_eq!(r, Ok(()));
    }
    panic!("测试宿主步数超限（核心没有终止）");
}

fn leak(n: usize) -> &'static mut [u8] {
    Box::leak(vec![0u8; n].into_boxed_slice())
}

fn file(n: usize) -> &'static [u8] {
    Box::leak(
        (0..n)
            .map(|i| (i * 7 + 3) as u8)
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    )
}

fn core(d: Script) -> Core<Script> {
    match Core::new(leak(INPUT_WINDOW_BYTES), d) {
        Ok(c) => c,
        Err(e) => panic!("{e:?}"),
    }
}

/// 尾置索引的典型流程：读文件头 → 跳到末尾的索引 → 读索引 → 跳回数据 → 数据中跳过填充。
const TAIL: &[Step] = &[
    Step::Read(12),
    Step::Jump(9_000),
    Step::Read(100),
    Step::Format,
    Step::Jump(12),
    Step::Out(500),
    Step::OutJump(300, 1_000), // 跳过 812..1000 的填充
    Step::Out(400),
    Step::End,
];

fn tail_reference(f: &[u8]) -> Vec<i32> {
    f[12..812]
        .iter()
        .chain(&f[1_000..1_400])
        .map(|&b| i32::from(b))
        .collect()
}

#[test]
fn tail_index_reads_the_right_bytes_under_any_chunking() {
    let f = file(9_100);
    for chunk in [1, 7, 64, 4096, usize::MAX] {
        let mut c = core(Script::new(f, TAIL));
        c.play(0, 1);
        let o = run(&mut c, f, true, chunk);
        assert!(o.finished && o.failed.is_none(), "chunk {chunk}：{o:?}");
        assert!(!c.decoder().mismatch, "chunk {chunk}：输入与文件位置不符");
        assert_eq!(o.out, tail_reference(f), "chunk {chunk}");
        assert_eq!(o.seeks, vec![9_000, 12, 1_000]);
        assert_eq!(
            o.parse,
            vec![
                ParseJump {
                    offset: 9_000,
                    header: true
                },
                ParseJump {
                    offset: 12,
                    header: true
                },
                ParseJump {
                    offset: 1_000,
                    header: false
                },
            ]
        );
        assert_eq!(o.opens, 1);
    }
}

/// 跳转后旧位置的 EOF 清除：先读到文件末尾（EOF），再跳回前面继续读。
#[test]
fn jump_clears_eof_of_old_position() {
    let f = file(200);
    static STEPS: &[Step] = &[
        Step::Read(10),
        Step::Jump(190),
        Step::Read(10), // 读到末尾
        Step::Format,
        Step::Jump(10),
        Step::Out(50),
        Step::End,
    ];
    let mut c = core(Script::new(f, STEPS));
    c.play(0, 1);
    let o = run(&mut c, f, true, 64);
    assert!(o.finished, "{o:?}");
    assert!(!c.decoder().mismatch);
    assert_eq!(
        o.out,
        f[10..60].iter().map(|&b| i32::from(b)).collect::<Vec<_>>()
    );
}

/// 跳到文件末尾之后：读到 EOF，交给解码器（它按截断报错），不再读取、不重开。
#[test]
fn jump_past_end_delivers_eof() {
    let f = file(100);
    static STEPS: &[Step] = &[Step::Read(12), Step::Jump(5_000), Step::Read(8), Step::End];
    let mut c = core(Script::new(f, STEPS));
    c.play(0, 1);
    let o = run(&mut c, f, true, usize::MAX);
    assert_eq!(
        o.failed,
        Some(Fault::Decode(DecodeError::Truncated)),
        "{o:?}"
    );
    assert_eq!(o.opens, 1);
    assert_eq!(o.reads_after_eof, 0, "EOF 之后不再读取");
}

/// 不可定位的音源：明确失败，不重新打开；两首的列表在第一首失败后停止（核心层不跳过）。
#[test]
fn unseekable_source_fails_without_reopening() {
    let f = file(9_100);
    let mut c = core(Script::new(f, TAIL));
    c.play(0, 2);
    let o = run(&mut c, f, false, 4096);
    assert_eq!(o.failed, Some(Fault::Jump(JumpFault::Unseekable)));
    assert_eq!(o.opens, 1, "不重新打开");
    assert_eq!(o.seeks, vec![9_000]);
    assert!(o.out.is_empty());
}

/// 无推进：反复请求同一位置、没有读到任何字节。
#[test]
fn spinning_jumps_are_no_progress() {
    let f = file(100);
    static SPIN: &[Step] = &[Step::Read(4), Step::Spin(50)];
    let mut c = core(Script::new(f, SPIN));
    c.play(0, 1);
    let o = run(&mut c, f, true, 4096);
    assert_eq!(o.failed, Some(Fault::Jump(JumpFault::NoProgress)), "{o:?}");
    // 第一次跳转之前读过字节：允许；跳转之后什么都没读就再跳：失败。
    assert_eq!(o.seeks, vec![50]);

    // 打开后还没读到任何字节就跳转。
    static FIRST: &[Step] = &[Step::Jump(10), Step::End];
    let mut c = core(Script::new(f, FIRST));
    c.play(0, 1);
    let o = run(&mut c, f, true, 4096);
    assert_eq!(o.failed, Some(Fault::Jump(JumpFault::NoProgress)));
    assert!(o.seeks.is_empty());

    // 跳到文件末尾之后再跳（读到的只有 EOF）。
    static PAST: &[Step] = &[Step::Read(4), Step::Jump(1_000), Step::Jump(0), Step::End];
    let mut c = core(Script::new(f, PAST));
    c.play(0, 1);
    let o = run(&mut c, f, true, 4096);
    assert_eq!(o.failed, Some(Fault::Jump(JumpFault::NoProgress)));
    assert_eq!(o.seeks, vec![1_000]);
}

#[test]
fn protocol_violations() {
    let f = file(100);
    static BOTH: &[Step] = &[Step::Read(4), Step::Both(20)];
    let mut c = core(Script::new(f, BOTH));
    c.play(0, 1);
    assert_eq!(
        run(&mut c, f, true, 4096).failed,
        Some(Fault::Jump(JumpFault::Protocol))
    );
    static END: &[Step] = &[Step::Read(4), Step::Format, Step::EndJump(20)];
    let mut c = core(Script::new(f, END));
    c.play(0, 1);
    let o = run(&mut c, f, true, 4096);
    assert_eq!(o.failed, Some(Fault::Jump(JumpFault::Protocol)));
    assert!(o.seeks.is_empty());
}

/// 用户定位：定位目标在文件头跳转期间保持待交，首次产出样本时交给解码器；同一次调用中的数据
/// 跳转随之作废，输出从目标处开始。
#[test]
fn user_seek_waits_for_header_jumps_and_takes_priority() {
    let f = file(9_100);
    let mut d = Script::new(f, TAIL);
    // 定位后从第 6 步（Out(400) @1000）继续：输出 1000..1400。
    d.seek_to = Some((1_000, 7));
    let mut c = core(d);
    c.play_at(0, 1, 800);
    let o = run(&mut c, f, true, 4096);
    assert!(o.finished && o.failed.is_none(), "{o:?}");
    assert!(!c.decoder().mismatch);
    assert_eq!(
        o.out,
        f[1_000..1_400]
            .iter()
            .map(|&b| i32::from(b))
            .collect::<Vec<_>>()
    );
    // 两次文件头跳转，然后是 seek 返回的位置（不是解析跳转）。
    assert_eq!(o.seeks, vec![9_000, 12, 1_000]);
    assert_eq!(o.parse.len(), 2);
    assert!(o.parse.iter().all(|j| j.header));
}

/// 解码器不支持定位：顺序丢弃到目标，解析跳转照常执行。
#[test]
fn user_seek_unsupported_keeps_parse_jumps() {
    let f = file(9_100);
    let mut c = core(Script::new(f, TAIL));
    c.play_at(0, 1, 600);
    let o = run(&mut c, f, true, 4096);
    assert!(o.finished && o.sequential, "{o:?}");
    assert_eq!(o.out, tail_reference(f)[600..].to_vec());
    assert_eq!(o.seeks, vec![9_000, 12, 1_000]);
}

/// 停止与换曲：旧代次的定位回报被拒绝；reset 后同一解码器复用。
#[test]
fn stale_seek_after_stop_and_reuse() {
    let f = file(9_100);
    let mut c = core(Script::new(f, TAIL));
    c.play(0, 1);
    let Some(Action::Open { generation, .. }) = c.poll() else {
        panic!("open");
    };
    assert_eq!(c.handle(Event::Opened { generation }), Ok(()));
    let Some(Action::Read {
        generation,
        request,
        ..
    }) = c.poll()
    else {
        panic!("read");
    };
    let Some(t) = c.read_target(generation, request) else {
        panic!("target");
    };
    t[..12].copy_from_slice(&f[..12]);
    assert_eq!(
        c.handle(Event::ReadDone {
            generation,
            request,
            len: 12
        }),
        Ok(())
    );
    let Some(Action::Seek {
        generation: g,
        request: rq,
        offset: 9_000,
    }) = c.poll()
    else {
        panic!("seek");
    };
    assert!(c.stop().is_some());
    assert_eq!(
        c.handle(Event::SeekDone {
            generation: g,
            request: rq
        }),
        Err(qy_core::CoreError::Stale)
    );
    assert_eq!(c.generation(), Generation(g.0 + 1));
    // 复用：重新播放，完整且正确。
    c.play(0, 1);
    let o = run(&mut c, f, true, 333);
    assert!(o.finished);
    assert_eq!(o.out, tail_reference(f));
    assert!(c.take_parse_jump().is_none());
}

/// `JumpGuard` 的规则本身（同步驱动共用）。
#[test]
fn guard_rules() {
    let f = file(10);
    let mut g = JumpGuard::new();
    let mut d = Script::new(f, &[]);
    d.jump = Some(5);
    assert_eq!(
        g.take(&mut d, Status::NeedInput),
        Err(JumpFault::NoProgress)
    );
    assert_eq!(d.jump, None, "取走即清空");
    g.on_read(1);
    d.jump = Some(5);
    assert_eq!(
        g.take(&mut d, Status::NeedInput),
        Ok(qy_core::Jump::Parse(5))
    );
    g.restart();
    d.seek_req = Some(3);
    assert_eq!(
        g.take(&mut d, Status::NeedInput),
        Ok(qy_core::Jump::Seek(3))
    );
    assert_eq!(g.take(&mut d, Status::NeedInput), Ok(qy_core::Jump::None));
}
