//! QyBit 扩展接口契约测试（X1）：对任意 [`Decoder`] 检查核心依赖的约定。只作 dev-dependency。
//!
//! 驱动方式与核心一致：输入窗口按需增长，解码器返回 `NeedInput` 时再送一块；`Frames` 后立即取走
//! 全部样本并 `release`；定位按核心的顺序（首次产出样本后 `seek`，之后每次 `decode` 查看
//! `seek_request`）。
//!
//! 检查的约定（违反时返回 [`Violation`]，说明哪一条、在哪一步）：
//! - **状态语义**：`consumed` 不超过输入；`Frames(n)` 的 `n > 0`，且 [`Decoder::pcm`] 至少有
//!   `n` 帧；`eof` 为真时不返回 `NeedInput`；`NeedOutput` 时工作区不为空；`End` 之后再调用
//!   不返回 `NeedInput` 或样本；出错后在 `reset` 之前不再产出样本。
//! - **格式**：新建与 `reset` 之后 `format()` 为 `None`；产出样本时必须有格式；同一个源的格式
//!   不变。
//! - **分块无关**：一次性、逐字节与随机分块输入的格式、样本与结束方式（正常结束或哪一种错误）
//!   完全相同。
//! - **复位复用**：同一个解码器依次解码多个源（中间 `reset`），结果与各用新解码器相同。
//! - **定位**（[`check_seek`]）：支持时只输出目标帧及之后的样本，与完整解码的对应部分相同；
//!   目标等于或超过长度时返回 `PastEnd`；不支持时返回 `Unsupported`，核心改为顺序解码。
//!
//! 停止后迟到的 I/O 回报由核心按代次拒绝，解码器不需要处理（见 `qy-core` 的测试）。

use qy_core::{DecodeError, Decoder, Progress, SeekError, SourceFormat, Status};

/// 违反的约定与发生的位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation(pub String);

impl core::fmt::Display for Violation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

fn violation<T>(what: impl Into<String>) -> Result<T, Violation> {
    Err(Violation(what.into()))
}

/// 输入的分块方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chunking {
    /// 一次送入全部数据。
    Whole,
    /// 每次一个字节。
    Bytewise,
    /// 每次 1..=`max` 字节，长度由 `seed` 决定（xorshift）。
    Random { seed: u64, max: usize },
}

/// 一个源解码到底的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    pub format: Option<SourceFormat>,
    /// 全部样本字（交错；binary64 浮点每样本两个字）。
    pub words: Vec<i32>,
    pub ending: Ending,
}

/// 结束方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    End,
    Error(DecodeError),
}

impl Decoded {
    /// 帧数（按每帧字数）。
    pub fn frames(&self) -> usize {
        self.format
            .map_or(0, |f| self.words.len() / f.words_per_frame().max(1))
    }
}

struct Chunks {
    mode: Chunking,
    state: u64,
}

impl Chunks {
    fn new(mode: Chunking) -> Self {
        let state = match mode {
            Chunking::Random { seed, .. } => seed | 1,
            _ => 1,
        };
        Self { mode, state }
    }

    fn next(&mut self, left: usize) -> usize {
        let n = match self.mode {
            Chunking::Whole => left,
            Chunking::Bytewise => 1,
            Chunking::Random { max, .. } => {
                self.state ^= self.state << 13;
                self.state ^= self.state >> 7;
                self.state ^= self.state << 17;
                1 + (self.state % max.max(1) as u64) as usize
            }
        };
        n.clamp(1, left.max(1))
    }
}

/// 驱动状态：已消耗的绝对位置与已送达的字节数。
struct Feed<'a> {
    bytes: &'a [u8],
    pos: usize,
    delivered: usize,
    chunks: Chunks,
}

impl<'a> Feed<'a> {
    fn new(bytes: &'a [u8], mode: Chunking) -> Self {
        let mut f = Self {
            bytes,
            pos: 0,
            delivered: 0,
            chunks: Chunks::new(mode),
        };
        f.more();
        f
    }

    fn input(&self) -> &'a [u8] {
        &self.bytes[self.pos..self.delivered]
    }

    fn eof(&self) -> bool {
        self.delivered == self.bytes.len()
    }

    fn more(&mut self) {
        let left = self.bytes.len() - self.delivered;
        if left > 0 {
            self.delivered += self.chunks.next(left);
        }
    }

    /// 移到绝对位置 `at`（定位），之后从那里按分块方式继续送达。
    fn jump(&mut self, at: usize) {
        let at = at.min(self.bytes.len());
        self.pos = at;
        self.delivered = at;
        self.more();
    }
}

/// 解码一步并检查通用约定；返回进度。
/// `hold` 为真时 `Frames` 的样本留在工作区不取走（定位检查用：核心在首次产出样本之后、取走之前
/// 交出定位目标）。
fn step<D: Decoder>(
    d: &mut D,
    feed: &mut Feed<'_>,
    format: &mut Option<SourceFormat>,
    words: &mut Vec<i32>,
    discard: &mut u64,
    hold: bool,
) -> Result<Step, Violation> {
    let input = feed.input();
    let eof = feed.eof();
    let at = feed.pos;
    let p = match d.decode(input, eof) {
        Ok(p) => p,
        Err(e) => return Ok(Step::Error(e)),
    };
    if p.consumed > input.len() {
        return violation(format!(
            "位置 {at}：消耗 {} 字节，超过输入 {} 字节",
            p.consumed,
            input.len()
        ));
    }
    feed.pos += p.consumed;
    // 与核心相同：每次 decode 之后先查看跳转请求（定位中的二分查找），再看状态。
    if let Some(to) = d.seek_request() {
        return Ok(Step::Jump(to));
    }
    match p.status {
        Status::Frames(n) => {
            if n == 0 {
                return violation(format!("位置 {at}：Frames(0)，零产出须用其他状态表示"));
            }
            let Some(f) = d.format() else {
                return violation(format!("位置 {at}：产出样本时 format() 为 None"));
            };
            if format.is_some_and(|old| old != f) {
                return violation(format!("位置 {at}：同一个源的格式改变了"));
            }
            *format = Some(f);
            let w = n * f.words_per_frame();
            let pcm = d.pcm();
            if pcm.len() < w {
                return violation(format!(
                    "位置 {at}：Frames({n}) 但 pcm() 只有 {} 个字（需要 {w}）",
                    pcm.len()
                ));
            }
            if !hold {
                take(d, n, f.words_per_frame(), words, discard);
            }
        }
        Status::NeedOutput => {
            if d.pcm().is_empty() {
                return violation(format!("位置 {at}：工作区已取空仍返回 NeedOutput"));
            }
        }
        Status::NeedInput => {
            if eof {
                return violation(format!("位置 {at}：eof 为真时返回 NeedInput"));
            }
            feed.more();
        }
        Status::End => {}
    }
    Ok(Step::Progress(p))
}

/// 一步的结果。
enum Step {
    Progress(Progress),
    /// 解码器要求跳到该字节位置（[`Decoder::seek_request`]）。
    Jump(u64),
    Error(DecodeError),
}

/// 取走 `n` 帧；顺序定位时先丢弃目标帧之前的样本（与核心相同）。
fn take<D: Decoder>(d: &mut D, n: usize, wpf: usize, words: &mut Vec<i32>, discard: &mut u64) {
    let skip = (*discard).min(n as u64) as usize;
    *discard -= skip as u64;
    words.extend_from_slice(&d.pcm()[skip * wpf..n * wpf]);
    d.release(n);
}

/// 结束后的再次调用：`End` 之后不得返回 `NeedInput` 或样本；出错之后不得产出样本。
fn after_end<D: Decoder>(d: &mut D, feed: &Feed<'_>, ending: Ending) -> Result<(), Violation> {
    let r = d.decode(feed.input(), feed.eof());
    match (ending, r) {
        (
            _,
            Ok(Progress {
                status: Status::Frames(_),
                ..
            }),
        ) => violation(format!(
            "{}之后再次调用仍产出样本",
            match ending {
                Ending::End => "End ",
                Ending::Error(_) => "出错",
            }
        )),
        (
            Ending::End,
            Ok(Progress {
                status: Status::NeedInput,
                ..
            }),
        ) => violation("End 之后再次调用返回 NeedInput"),
        _ => Ok(()),
    }
}

/// 循环上限：每个字节与每个输出帧都允许若干次调用，超出视为不前进。
fn budget(bytes: &[u8]) -> usize {
    bytes.len().saturating_mul(4).saturating_add(1 << 20)
}

/// 按 `chunking` 把 `bytes` 解码到底（解码器先 `reset`），检查通用约定。
pub fn decode<D: Decoder>(
    d: &mut D,
    bytes: &[u8],
    chunking: Chunking,
) -> Result<Decoded, Violation> {
    d.reset();
    if d.format().is_some() {
        return violation("reset 之后 format() 不是 None");
    }
    let mut feed = Feed::new(bytes, chunking);
    let (mut format, mut words, mut discard) = (None, Vec::new(), 0u64);
    for _ in 0..budget(bytes) {
        match step(d, &mut feed, &mut format, &mut words, &mut discard, false)? {
            Step::Jump(_) => return violation("未定位时 seek_request 要求跳转"),
            Step::Error(e) => {
                let ending = Ending::Error(e);
                after_end(d, &feed, ending)?;
                return Ok(Decoded {
                    format,
                    words,
                    ending,
                });
            }
            Step::Progress(p) if p.status == Status::End => {
                after_end(d, &feed, Ending::End)?;
                return Ok(Decoded {
                    format: format.or(d.format()),
                    words,
                    ending: Ending::End,
                });
            }
            Step::Progress(_) => {}
        }
    }
    violation("解码不前进（调用次数超过上限）")
}

/// 对一组源检查解码器契约：`make` 每次返回一个新解码器（缓冲由宿主提供，可以是同一组切片的
/// 新实例；需要时自行复用）。返回各源的一次性解码结果，供调用方再与独立参考比对。
pub fn check<D: Decoder>(
    mut make: impl FnMut() -> D,
    sources: &[&[u8]],
) -> Result<Vec<Decoded>, Violation> {
    let mut results = Vec::new();
    for (i, src) in sources.iter().enumerate() {
        let mut d = make();
        if d.format().is_some() {
            return violation(format!("源 {i}：新建的解码器 format() 不是 None"));
        }
        let whole = decode(&mut d, src, Chunking::Whole)
            .map_err(|v| Violation(format!("源 {i}（一次性）：{v}")))?;
        // 逐字节对大文件很慢：超过 256 KiB 时改用 1..=7 字节的随机分块。
        let fine = if src.len() <= 256 * 1024 {
            Chunking::Bytewise
        } else {
            Chunking::Random { seed: 7, max: 7 }
        };
        let mut modes = vec![fine];
        for seed in 1..=3u64 {
            modes.push(Chunking::Random {
                seed: seed * 0x9E37_79B9,
                max: 4096,
            });
        }
        for mode in modes {
            let got = decode(&mut d, src, mode)
                .map_err(|v| Violation(format!("源 {i}（{mode:?}）：{v}")))?;
            if got != whole {
                return violation(format!(
                    "源 {i}：{mode:?} 与一次性输入的结果不同（帧 {} 对 {}，结束 {:?} 对 {:?}）",
                    got.frames(),
                    whole.frames(),
                    got.ending,
                    whole.ending
                ));
            }
        }
        results.push(whole);
    }
    // 复位复用：同一个解码器依次解码全部源。
    let mut d = make();
    for (i, src) in sources.iter().enumerate() {
        let got = decode(&mut d, src, Chunking::Random { seed: 99, max: 512 })
            .map_err(|v| Violation(format!("复用解码器，源 {i}：{v}")))?;
        if got != results[i] {
            return violation(format!("复用解码器解码源 {i} 的结果与新解码器不同"));
        }
    }
    Ok(results)
}

/// 定位检查的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekSupport {
    /// 解码器支持定位：各目标的输出与完整解码的对应部分相同。
    Supported,
    /// 解码器不支持定位（核心改为顺序解码，结果同样精确）。
    Unsupported,
}

/// 按核心的顺序定位到每个目标帧（首次产出样本后 `seek`，之后每次 `decode` 查看
/// `seek_request`），检查只输出目标帧及之后的样本。`source` 必须能完整解码到 `End`。
pub fn check_seek<D: Decoder>(
    mut make: impl FnMut() -> D,
    source: &[u8],
    targets: &[u64],
) -> Result<SeekSupport, Violation> {
    let mut d = make();
    let full = decode(&mut d, source, Chunking::Whole)?;
    if full.ending != Ending::End {
        return violation("定位检查的源不能完整解码");
    }
    let Some(f) = full.format else {
        return violation("定位检查的源没有格式");
    };
    let wpf = f.words_per_frame();
    let total = full.frames() as u64;
    let mut support = None;
    for &target in targets {
        for chunking in [Chunking::Whole, Chunking::Random { seed: 5, max: 1024 }] {
            d.reset();
            let mut feed = Feed::new(source, chunking);
            let (mut format, mut words, mut discard) = (None, Vec::new(), 0u64);
            let (mut asked, mut sequential, mut ended) = (false, false, false);
            for _ in 0..budget(source) {
                let r = step(
                    &mut d,
                    &mut feed,
                    &mut format,
                    &mut words,
                    &mut discard,
                    !asked,
                )?;
                let p = match r {
                    Step::Progress(p) => p,
                    Step::Jump(at) => {
                        feed.jump(usize::try_from(at).unwrap_or(usize::MAX));
                        continue;
                    }
                    Step::Error(e) => {
                        return violation(format!("定位到 {target}：解码出错 {e:?}"));
                    }
                };
                if !asked && let Status::Frames(n) = p.status {
                    asked = true;
                    match d.seek(target) {
                        Ok(at) => {
                            if support == Some(SeekSupport::Unsupported) {
                                return violation("定位有时支持、有时不支持");
                            }
                            support = Some(SeekSupport::Supported);
                            if !d.pcm().is_empty() {
                                return violation("seek 返回后未丢弃尚未取走的样本");
                            }
                            feed.jump(usize::try_from(at).unwrap_or(usize::MAX));
                        }
                        Err(SeekError::PastEnd) => {
                            if target < total {
                                return violation(format!(
                                    "目标 {target} 小于长度 {total} 却返回 PastEnd"
                                ));
                            }
                            ended = true;
                            break;
                        }
                        Err(SeekError::Unsupported) => {
                            if support == Some(SeekSupport::Supported) {
                                return violation("定位有时支持、有时不支持");
                            }
                            support = Some(SeekSupport::Unsupported);
                            // 就地顺序解码：丢弃目标帧之前的样本，包括工作区中尚未取走的。
                            sequential = true;
                            discard = target;
                            take(&mut d, n, wpf, &mut words, &mut discard);
                        }
                    }
                    continue;
                }
                if p.status == Status::End {
                    ended = true;
                    break;
                }
            }
            if !ended {
                return violation(format!("定位到 {target}：解码不前进"));
            }
            if target >= total {
                if !words.is_empty() {
                    return violation(format!("定位到长度之后（{target}）仍输出样本"));
                }
                continue;
            }
            let want = &full.words[(target as usize) * wpf..];
            if words != want {
                return violation(format!(
                    "定位到 {target}（{}{chunking:?}）：输出 {} 字，应为完整解码第 {target} 帧起的 {} 字",
                    if sequential { "顺序解码，" } else { "" },
                    words.len(),
                    want.len()
                ));
            }
        }
    }
    Ok(support.unwrap_or(SeekSupport::Unsupported))
}
