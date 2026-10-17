//! 串口命令（与具体串口外设无关）：逐字节输入，按行解析。
//!
//! 命令（ASCII，不区分大小写，参数以空格分隔）：
//!
//! | 命令 | 作用 |
//! | --- | --- |
//! | `ls` | 列出曲目 |
//! | `play [n]` | 从第 n 首（默认 0）开始播放整个列表 |
//! | `pause` / `resume` | 暂停 / 恢复 |
//! | `stop` | 停止 |
//! | `next` / `prev` | 下一首 / 上一首（相对正在播放的曲目） |
//! | `seek <毫秒>` | 在正在播放的曲目内定位（S1；样本级精确，等于或超过长度时进入下一首） |
//! | `status` | 状态与计数 |
//! | `stats` | 主循环活跃比例、单次 `step` 最长耗时与 DMA 截止余量（L1；时间由板级测量） |
//! | `scan` | 重新扫描存储（换卡后；只在停止时有效） |
//! | `mono-to-stereo on\|off` | 单声道 `[FC]` 复制到左右两路（非严格选项，默认关闭；对之后开始的曲目生效，见 tasks/O1.md） |
//! | `float-to-int on\|off` | 浮点转整数（非严格选项，默认关闭；I²S 只输出整数，浮点文件只在开启时播放，样本已改变；见 tasks/F3.md） |
//!
//! 回应为一行或多行 `key=value` 文本（见 [`crate::player::Player::command`]），便于脚本解析。

/// 一条命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    List,
    Play(u32),
    Pause,
    Resume,
    Stop,
    Next,
    Prev,
    Status,
    Scan,
    /// 开启（`true`）或关闭单声道复制到左右两路（O1）。
    MonoToStereo(bool),
    /// 开启（`true`）或关闭浮点转整数（F3）。
    FloatToInt(bool),
    /// 在正在播放的曲目内定位到第 n 毫秒（S1）。
    Seek(u32),
    /// 主循环的活跃与休眠统计（L1）。
    Stats,
}

/// 解析错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// 空行。
    Empty,
    /// 未知命令。
    Unknown,
    /// 参数缺失、多余或不是十进制数。
    BadArgument,
    /// 行超过缓冲长度（整行丢弃）。
    TooLong,
}

fn eq_ignore_case(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

fn parse_u32(s: &[u8]) -> Option<u32> {
    if s.is_empty() || s.len() > 10 {
        return None;
    }
    let mut v: u32 = 0;
    for &c in s {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add(u32::from(c - b'0'))?;
    }
    Some(v)
}

/// 解析一行（不含换行符）。
pub fn parse(line: &[u8]) -> Result<Command, ParseError> {
    let mut words = line
        .split(|c| *c == b' ' || *c == b'\t')
        .filter(|w| !w.is_empty());
    let Some(cmd) = words.next() else {
        return Err(ParseError::Empty);
    };
    let arg = words.next();
    if words.next().is_some() {
        return Err(ParseError::BadArgument);
    }
    let simple = |c: Command| match arg {
        None => Ok(c),
        Some(_) => Err(ParseError::BadArgument),
    };
    const TABLE: [(&[u8], Command); 9] = [
        (b"ls", Command::List),
        (b"pause", Command::Pause),
        (b"resume", Command::Resume),
        (b"stop", Command::Stop),
        (b"next", Command::Next),
        (b"prev", Command::Prev),
        (b"status", Command::Status),
        (b"scan", Command::Scan),
        (b"stats", Command::Stats),
    ];
    if eq_ignore_case(cmd, b"mono-to-stereo") {
        return match arg {
            Some(a) if eq_ignore_case(a, b"on") => Ok(Command::MonoToStereo(true)),
            Some(a) if eq_ignore_case(a, b"off") => Ok(Command::MonoToStereo(false)),
            _ => Err(ParseError::BadArgument),
        };
    }
    if eq_ignore_case(cmd, b"float-to-int") {
        return match arg {
            Some(a) if eq_ignore_case(a, b"on") => Ok(Command::FloatToInt(true)),
            Some(a) if eq_ignore_case(a, b"off") => Ok(Command::FloatToInt(false)),
            _ => Err(ParseError::BadArgument),
        };
    }
    if eq_ignore_case(cmd, b"seek") {
        return arg
            .and_then(parse_u32)
            .map(Command::Seek)
            .ok_or(ParseError::BadArgument);
    }
    if eq_ignore_case(cmd, b"play") {
        return match arg {
            None => Ok(Command::Play(0)),
            Some(a) => parse_u32(a)
                .map(Command::Play)
                .ok_or(ParseError::BadArgument),
        };
    }
    for (name, c) in TABLE {
        if eq_ignore_case(cmd, name) {
            return simple(c);
        }
    }
    Err(ParseError::Unknown)
}

/// 行缓冲：`L` 字节。`\r`、`\n` 结束一行（`\r\n` 只算一次）；超长的行整行丢弃并报
/// [`ParseError::TooLong`]。
pub struct LineBuffer<const L: usize> {
    buf: [u8; L],
    len: usize,
    overflow: bool,
    last_cr: bool,
}

impl<const L: usize> Default for LineBuffer<L> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const L: usize> LineBuffer<L> {
    pub const fn new() -> Self {
        Self {
            buf: [0; L],
            len: 0,
            overflow: false,
            last_cr: false,
        }
    }

    /// 输入一个字节；行结束时返回解析结果。空行（含 `\r\n` 的 `\n`）不返回。
    pub fn push(&mut self, b: u8) -> Option<Result<Command, ParseError>> {
        let was_cr = core::mem::replace(&mut self.last_cr, b == b'\r');
        if b == b'\n' && was_cr {
            return None;
        }
        if b == b'\r' || b == b'\n' {
            let r = if self.overflow {
                Some(Err(ParseError::TooLong))
            } else {
                match parse(&self.buf[..self.len]) {
                    Err(ParseError::Empty) => None,
                    other => Some(other),
                }
            };
            self.len = 0;
            self.overflow = false;
            return r;
        }
        if self.len < L {
            self.buf[self.len] = b;
            self.len += 1;
        } else {
            self.overflow = true;
        }
        None
    }
}

/// 串口接收队列（L1）：接收中断里 [`RxQueue::push`] 收到的字节，主循环 [`RxQueue::pop`] 后交给
/// [`LineBuffer`]。有界（`N` 字节）；满时丢弃新到的字节并计数，之后的行可能因此不完整，按解析错误
/// 回应。本身不做同步：中断与主循环共享时，板级在临界区内访问。
pub struct RxQueue<const N: usize> {
    buf: [u8; N],
    head: usize,
    len: usize,
    /// 因队列满丢弃的字节数（饱和）。
    pub dropped: u32,
}

impl<const N: usize> Default for RxQueue<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> RxQueue<N> {
    pub const fn new() -> Self {
        Self {
            buf: [0; N],
            head: 0,
            len: 0,
            dropped: 0,
        }
    }

    /// 放入一个字节；队列满时丢弃并返回 false。
    pub fn push(&mut self, b: u8) -> bool {
        if self.len >= N {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        let tail = (self.head + self.len) % N;
        self.buf[tail] = b;
        self.len += 1;
        true
    }

    /// 取出最早的字节。
    pub fn pop(&mut self) -> Option<u8> {
        if self.len == 0 {
            return None;
        }
        let b = self.buf[self.head];
        self.head = (self.head + 1) % N;
        self.len -= 1;
        Some(b)
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;
    use std::vec::Vec;

    #[test]
    fn parses_commands_and_arguments() {
        assert_eq!(parse(b"ls"), Ok(Command::List));
        assert_eq!(parse(b"  PLAY "), Ok(Command::Play(0)));
        assert_eq!(parse(b"play 12"), Ok(Command::Play(12)));
        assert_eq!(parse(b"play 4294967295"), Ok(Command::Play(u32::MAX)));
        assert_eq!(parse(b"play 4294967296"), Err(ParseError::BadArgument));
        assert_eq!(parse(b"play -1"), Err(ParseError::BadArgument));
        assert_eq!(parse(b"play 1 2"), Err(ParseError::BadArgument));
        assert_eq!(parse(b"pause now"), Err(ParseError::BadArgument));
        assert_eq!(parse(b"Next"), Ok(Command::Next));
        assert_eq!(parse(b"rewind"), Err(ParseError::Unknown));
        assert_eq!(parse(b""), Err(ParseError::Empty));
        assert_eq!(parse(b"mono-to-stereo on"), Ok(Command::MonoToStereo(true)));
        assert_eq!(parse(b"SEEK 90500"), Ok(Command::Seek(90_500)));
        assert_eq!(parse(b"seek"), Err(ParseError::BadArgument));
        assert_eq!(parse(b"seek -1"), Err(ParseError::BadArgument));
        assert_eq!(parse(b"seek 1 2"), Err(ParseError::BadArgument));
        assert_eq!(
            parse(b"Mono-To-Stereo OFF"),
            Ok(Command::MonoToStereo(false))
        );
        assert_eq!(parse(b"mono-to-stereo"), Err(ParseError::BadArgument));
        assert_eq!(parse(b"mono-to-stereo 1"), Err(ParseError::BadArgument));
        assert_eq!(parse(b"mono-to-stereo on x"), Err(ParseError::BadArgument));
        assert_eq!(parse(b"float-to-int on"), Ok(Command::FloatToInt(true)));
        assert_eq!(parse(b"FLOAT-TO-INT Off"), Ok(Command::FloatToInt(false)));
        assert_eq!(parse(b"float-to-int"), Err(ParseError::BadArgument));
        assert_eq!(parse(b"float-to-int yes"), Err(ParseError::BadArgument));
        assert_eq!(parse(b"STATS"), Ok(Command::Stats));
        assert_eq!(parse(b"stats now"), Err(ParseError::BadArgument));
    }

    #[test]
    fn rx_queue_is_bounded_fifo() {
        let mut q = RxQueue::<4>::new();
        assert!(q.is_empty() && q.pop().is_none());
        for b in b"abcd" {
            assert!(q.push(*b));
        }
        // 满：丢弃并计数，已有字节不变。
        assert!(!q.push(b'e') && !q.push(b'f'));
        assert_eq!(q.dropped, 2);
        assert_eq!((q.pop(), q.pop()), (Some(b'a'), Some(b'b')));
        // 环绕。
        assert!(q.push(b'g') && q.push(b'h') && !q.push(b'i'));
        let rest: Vec<u8> = core::iter::from_fn(|| q.pop()).collect();
        assert_eq!(rest, b"cdgh");
        assert!(q.is_empty());
        assert_eq!(q.dropped, 3);
        let mut z = RxQueue::<0>::new();
        assert!(!z.push(1) && z.pop().is_none() && z.dropped == 1);
    }

    #[test]
    fn line_buffer_handles_crlf_and_overflow() {
        let mut lb = LineBuffer::<8>::new();
        let mut got = Vec::new();
        for &b in b"ls\r\nnext\n\r\nplay 123456789\nstop\r" {
            if let Some(r) = lb.push(b) {
                got.push(r);
            }
        }
        assert_eq!(
            got,
            vec![
                Ok(Command::List),
                Ok(Command::Next),
                Err(ParseError::TooLong),
                Ok(Command::Stop)
            ]
        );
    }
}
