//! QyBit sans-IO 播放核心。
//!
//! `#![no_std]`、不用 `alloc`，不访问文件、设备、线程或系统时钟（AGENTS §2）。
//! 宿主循环调用 [`Core::poll`] 取得下一个 [`Action`] 并执行，结果以 [`Event`]
//! 经 [`Core::handle`] 回报。输入窗口由宿主启动时一次性提供 `&'static mut [u8]`，
//! 解码器持有宿主提供的 PCM 工作区；停止/换曲复用二者，不分配。
//!
//! 播放流程（每首曲目一个新代次）：
//! `Open` → (`Read`)* → 首批 PCM 时若格式与已配置输出不同则 `Configure` →
//! (`Output` | `Read`)* → 码流结束 `Close` → 下一首 `Open` … → `Finished`。
//! 任一失败：若音源已打开先 `Close`，再 `Failed`，播放列表停止。
//! 相同格式的相邻曲目不重新配置，PCM 无缝衔接；格式变化时宿主须先排空旧格式
//! 的队列与设备，再回报 `Configured`。
//!
//! 旧代次或非当前请求的回报返回 [`CoreError::Stale`] 且不改变状态；宿主对
//! 旧读取不得写入（[`Core::read_target`] 对其返回 `None`）。
//!
//! M0 T4 验收后 `Action`、`Event`、`Decoder` 为冻结接口，破坏性变更须先征得用户同意。
//! S1（曲目内定位，2026-10-15 用户同意）新增 `Action::Seek`、`Event::SeekDone` /
//! `SeekFailed`，`Decoder` 只新增带默认实现的方法。
//!
//! 定位（[`Core::play_at`]）：从目标帧重新开始一首曲目。打开音源、解析文件头后，核心向解码器
//! 要目标帧的字节位置（[`Decoder::seek`]）并发出 `Seek`，宿主移动读取位置后继续读取；解码器
//! 只输出目标帧及之后的样本。解码器不支持定位或宿主回报 `SeekFailed` 时，核心重新打开该曲目，
//! 顺序解码并丢弃目标帧之前的样本——结果同样精确，只是更慢。
#![no_std]

pub mod engine;

pub use qy_pcm_format::pack::{Exact, Pcm};
pub use qy_pcm_format::{
    ChannelLayout, DeviceFormat, MAX_CHANNELS, SampleEncoding, SourceFormat, Speaker,
};

/// M0 默认输入窗口字节数。
pub const INPUT_WINDOW_BYTES: usize = 8192;
/// M0 默认 PCM 工作区帧数。
pub const PCM_WORK_FRAMES: usize = 4096;
/// WAV PCM 工作区样本数（4096 帧双声道）。声道更多时每次解码的帧数相应减少（F1）。
pub const PCM_WORK_SAMPLES: usize = PCM_WORK_FRAMES * 2;

/// 解码器一次调用的结果状态。各状态互斥，零产出不兼表缺输入与 EOF。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// 产出若干完整帧，可从 [`Decoder::pcm`] 读取。
    Frames(usize),
    /// 需要更多输入才能继续；下次调用从暂停处继续。
    NeedInput,
    /// PCM 工作区已满，需先 [`Decoder::release`]。
    NeedOutput,
    /// 码流正常结束。
    End,
}

/// 解码器一次调用的进度：消耗的输入字节数与状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub consumed: usize,
    pub status: Status,
}

/// 解码错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// 码流结构非法。
    Malformed,
    /// 合法但本阶段不支持的格式。
    Unsupported,
    /// 源在完整结构前结束。
    Truncated,
    /// 宿主提供的缓冲不足。
    CapacityExceeded,
}

/// 定位失败的原因（[`Decoder::seek`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekError {
    /// 解码器不支持定位，或当前码流无法定位（核心改为顺序解码并丢弃）。
    Unsupported,
    /// 目标帧等于或超过曲目长度（核心按播完该曲处理）。
    PastEnd,
}

/// 推送式解码器接口（静态分发）。
///
/// 实现必须 `no_std`、无 alloc，不调用 `Read/Seek`，不等待输入。
pub trait Decoder {
    /// 回到初始状态（换曲/停止），复用已有工作区。
    fn reset(&mut self);
    /// 头部解析完成前返回 `None`，不得伪造格式。
    fn format(&self) -> Option<SourceFormat>;
    /// 喂入当前可用输入；`eof` 表示源已结束且 `input` 之后不再有数据。
    fn decode(&mut self, input: &[u8], eof: bool) -> Result<Progress, DecodeError>;
    /// 已解码、尚未取走的交错右对齐样本。
    fn pcm(&self) -> &[i32];
    /// 声明已取走 `frames` 帧。
    fn release(&mut self, frames: usize);

    /// 定位到第 `frame` 帧（从 0 起），返回宿主应移动到的字节位置（距音源开头）。只在文件头
    /// 解析完成、首次产出 PCM 之后由核心调用；调用后解码器丢弃未取走的样本，从该位置的数据
    /// 继续解码，并且只输出 `frame` 及之后的样本。默认不支持。
    fn seek(&mut self, frame: u64) -> Result<u64, SeekError> {
        let _ = frame;
        Err(SeekError::Unsupported)
    }

    /// 定位过程中解码器需要再跳一次（如 FLAC 无定位表时的二分查找）：返回并清除新的字节位置。
    /// 核心在每次 `decode` 之后取一次。默认没有。
    fn seek_request(&mut self) -> Option<u64> {
        None
    }
}

/// 播放代次：每首曲目打开时与停止时递增，旧代次的回报一律拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Generation(pub u32);

/// 播放失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// 宿主打开音源失败。
    Open,
    /// 宿主读取失败。
    Read,
    /// 解码失败（含截断）。
    Decode(DecodeError),
    /// 输出无法按源格式配置（设备不支持、独占打开失败等）。
    Configure,
    /// 输入窗口已满仍缺输入（解码器单一结构超出窗口容量）。
    InputWindow,
}

/// 核心请求宿主执行的操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 打开第 `track` 首音源，完成后回报 `Opened` / `OpenFailed`。
    Open { generation: Generation, track: u32 },
    /// 读取至多 `max` 字节写入 [`Core::read_target`]，完成后回报
    /// `ReadDone` / `ReadEof` / `ReadFailed`。
    Read {
        generation: Generation,
        request: u32,
        max: usize,
    },
    /// 按源格式配置输出；若当前输出为其他格式，宿主须先排空旧格式数据。
    /// 完成后回报 `Configured` / `ConfigureFailed`。
    Configure {
        generation: Generation,
        format: SourceFormat,
    },
    /// 有 `frames` 帧 PCM 可取：宿主经 [`Core::pcm`] 取得并 pack 入队至多该帧数，
    /// 回报 `Output` 实际取走帧数（队列满时可为 0，宿主应等待空间后再 poll）。
    Output {
        generation: Generation,
        frames: usize,
    },
    /// 把音源读取位置移到距开头 `offset` 字节处，此后的 `Read` 从该处读取（S1）。
    /// 完成后回报 `SeekDone` / `SeekFailed`（音源不可定位，如管道）。
    Seek {
        generation: Generation,
        request: u32,
        offset: u64,
    },
    /// 关闭当前音源。
    Close { generation: Generation },
    /// 播放列表全部完成；宿主排空队列与设备中剩余音频。
    Finished { generation: Generation },
    /// 播放失败，播放列表停止；已入队的有效音频仍可排空播放。
    Failed {
        generation: Generation,
        track: u32,
        fault: Fault,
    },
}

/// 宿主回报给核心的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Opened {
        generation: Generation,
    },
    OpenFailed {
        generation: Generation,
    },
    /// 读取完成，`len` 字节已写入读取目标；`len == 0` 不表示 EOF。
    ReadDone {
        generation: Generation,
        request: u32,
        len: usize,
    },
    /// 源已到结尾（明确的 EOF，区别于零长度读取）。
    ReadEof {
        generation: Generation,
        request: u32,
    },
    ReadFailed {
        generation: Generation,
        request: u32,
    },
    /// 读取位置已移动（S1）。
    SeekDone {
        generation: Generation,
        request: u32,
    },
    /// 音源不可定位；核心改为重新打开并顺序解码到目标帧。
    SeekFailed {
        generation: Generation,
        request: u32,
    },
    Configured {
        generation: Generation,
    },
    ConfigureFailed {
        generation: Generation,
    },
    /// 宿主已从 `Output` 取走 `frames` 帧。
    Output {
        generation: Generation,
        frames: usize,
    },
}

/// 核心错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreError {
    /// 宿主提供的缓冲小于声明容量。
    CapacityExceeded { required: usize, provided: usize },
    /// 回报属于旧代次或非当前请求，已丢弃。
    Stale,
    /// 回报与当前状态不符（例如读取长度超出请求、取走帧数超出可用）。
    Protocol,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Open,
    Opening,
    Run,
    Reading {
        request: u32,
        max: usize,
    },
    Seek {
        offset: u64,
    },
    Seeking {
        request: u32,
    },
    /// 定位失败后重新打开当前曲目（先关闭已打开的音源）。
    Reopen,
    Configure(SourceFormat),
    Configuring(SourceFormat),
    Output {
        frames: usize,
    },
    EndOfTrack,
    Finish,
    Fail(Fault),
}

/// sans-IO 播放核心。
pub struct Core<D: Decoder> {
    input: &'static mut [u8],
    filled: usize,
    decoder: D,
    generation: Generation,
    next_request: u32,
    state: State,
    source_eof: bool,
    source_open: bool,
    configured: Option<SourceFormat>,
    track: u32,
    track_count: u32,
    /// 尚未交给解码器的定位目标帧（S1）。
    seek: Option<u64>,
    /// 定位进行中的目标帧（`SeekFailed` 时据此改为顺序解码）。
    seek_target: Option<u64>,
    /// 顺序解码时还要丢弃的帧数。
    discard: u64,
    /// 本曲目改为顺序解码到目标帧（宿主据此报告），取走后清除。
    sequential: bool,
}

impl<D: Decoder> Core<D> {
    /// 用宿主提供的输入窗口构造核心；长度不足 [`INPUT_WINDOW_BYTES`] 返回错误。
    pub fn new(input: &'static mut [u8], decoder: D) -> Result<Self, CoreError> {
        if input.len() < INPUT_WINDOW_BYTES {
            return Err(CoreError::CapacityExceeded {
                required: INPUT_WINDOW_BYTES,
                provided: input.len(),
            });
        }
        Ok(Self {
            input,
            filled: 0,
            decoder,
            generation: Generation(0),
            next_request: 0,
            state: State::Idle,
            source_eof: false,
            source_open: false,
            configured: None,
            track: 0,
            track_count: 0,
            seek: None,
            seek_target: None,
            discard: 0,
            sequential: false,
        })
    }

    /// 用宿主声明的输入窗口容量构造核心（如 FLAC 桌面配置 1 MiB）。
    /// 切片短于 `window` 或短于 [`INPUT_WINDOW_BYTES`] 时返回容量错误。
    pub fn with_window(
        input: &'static mut [u8],
        window: usize,
        decoder: D,
    ) -> Result<Self, CoreError> {
        if input.len() < window {
            return Err(CoreError::CapacityExceeded {
                required: window,
                provided: input.len(),
            });
        }
        Self::new(input, decoder)
    }

    pub fn generation(&self) -> Generation {
        self.generation
    }

    /// 当前曲目序号。
    pub fn track(&self) -> u32 {
        self.track
    }

    pub fn decoder(&self) -> &D {
        &self.decoder
    }

    /// 是否处于空闲（未播放或已完成/失败/停止）。
    pub fn is_idle(&self) -> bool {
        self.state == State::Idle
    }

    /// 从第 `first` 首开始播放共 `count` 首的列表。正在播放时先隐式停止
    /// （调用方应先用 [`Core::stop`] 关闭音源）。
    pub fn play(&mut self, first: u32, count: u32) {
        self.reset_generation();
        self.clear_seek();
        self.configured = None;
        self.track = first;
        self.track_count = count;
        self.state = if first < count {
            State::Open
        } else {
            State::Finish
        };
    }

    /// 同 [`Core::play`]，第 `first` 首从第 `frame` 帧开始（S1 定位）。`frame` 为 0 时与
    /// `play` 相同；等于或超过曲目长度时按播完该曲处理。
    pub fn play_at(&mut self, first: u32, count: u32, frame: u64) {
        self.play(first, count);
        if frame > 0 {
            self.seek = Some(frame);
        }
    }

    /// 本曲目的定位改为顺序解码到目标帧（解码器不支持定位或音源不可定位）时返回 `true`，
    /// 每次定位至多一次；取走后清除。
    pub fn take_sequential_seek(&mut self) -> bool {
        core::mem::take(&mut self.sequential)
    }

    fn clear_seek(&mut self) {
        self.seek = None;
        self.seek_target = None;
        self.discard = 0;
        self.sequential = false;
    }

    /// 改为顺序解码：从头重新打开当前曲目，丢弃目标帧之前的样本。
    fn seek_sequentially(&mut self, target: u64) {
        self.seek = None;
        self.seek_target = None;
        self.discard = target;
        self.sequential = true;
        self.state = State::Reopen;
    }

    /// 停止：进入新代次，此后旧代次的回报全部拒绝；已配置输出视为失效。
    /// 音源已打开时返回 `Close`（旧代次）。
    pub fn stop(&mut self) -> Option<Action> {
        let old = self.generation;
        let was_open = self.source_open;
        self.reset_generation();
        self.clear_seek();
        self.configured = None;
        self.state = State::Idle;
        was_open.then_some(Action::Close { generation: old })
    }

    fn reset_generation(&mut self) {
        self.generation = Generation(self.generation.0.wrapping_add(1));
        self.filled = 0;
        self.source_eof = false;
        self.source_open = false;
        self.decoder.reset();
    }

    /// 取得下一个宿主操作；`None` 表示正在等待回报或已空闲。
    pub fn poll(&mut self) -> Option<Action> {
        // 每次最多推进有限步（解析头部、跳过 chunk 等），不会无限循环：
        // 每一步要么返回动作，要么切换到返回动作的状态。顺序定位（S1）丢弃样本时，每一步都在
        // 消耗输入或样本，直到需要读取（返回 `Read`）或曲目结束，这些步不计入上限。
        let mut budget = 8u32;
        while budget > 0 {
            budget -= 1;
            match self.state {
                State::Idle
                | State::Opening
                | State::Reading { .. }
                | State::Seeking { .. }
                | State::Configuring(_)
                | State::Output { .. } => return None,
                State::Seek { offset } => {
                    let request = self.next_request;
                    self.next_request = self.next_request.wrapping_add(1);
                    self.state = State::Seeking { request };
                    return Some(Action::Seek {
                        generation: self.generation,
                        request,
                        offset,
                    });
                }
                State::Reopen => {
                    self.state = State::Open;
                    if self.source_open {
                        self.source_open = false;
                        return Some(Action::Close {
                            generation: self.generation,
                        });
                    }
                }
                State::Open => {
                    self.reset_generation();
                    self.state = State::Opening;
                    return Some(Action::Open {
                        generation: self.generation,
                        track: self.track,
                    });
                }
                State::Configure(format) => {
                    self.state = State::Configuring(format);
                    return Some(Action::Configure {
                        generation: self.generation,
                        format,
                    });
                }
                State::EndOfTrack => {
                    let generation = self.generation;
                    self.source_open = false;
                    self.clear_seek();
                    let next = self.track.saturating_add(1);
                    if next < self.track_count {
                        self.track = next;
                        self.state = State::Open;
                    } else {
                        self.state = State::Finish;
                    }
                    return Some(Action::Close { generation });
                }
                State::Finish => {
                    self.state = State::Idle;
                    return Some(Action::Finished {
                        generation: self.generation,
                    });
                }
                State::Fail(fault) => {
                    if self.source_open {
                        self.source_open = false;
                        return Some(Action::Close {
                            generation: self.generation,
                        });
                    }
                    self.state = State::Idle;
                    return Some(Action::Failed {
                        generation: self.generation,
                        track: self.track,
                        fault,
                    });
                }
                State::Run => {
                    let before = (self.discard, self.filled, self.decoder.pcm().len());
                    if let Some(action) = self.step() {
                        return Some(action);
                    }
                    let after = (self.discard, self.filled, self.decoder.pcm().len());
                    if before.0 > 0 && after != before && self.state == State::Run {
                        budget += 1;
                    }
                }
            }
        }
        None
    }

    /// 解码推进一步；返回 `None` 时状态已切换，由 `poll` 继续。
    fn step(&mut self) -> Option<Action> {
        if !self.decoder.pcm().is_empty() {
            return self.output_or_configure();
        }
        let progress = match self
            .decoder
            .decode(&self.input[..self.filled], self.source_eof)
        {
            Ok(p) => p,
            Err(e) => {
                self.state = State::Fail(Fault::Decode(e));
                return None;
            }
        };
        let consumed = progress.consumed.min(self.filled);
        self.input.copy_within(consumed..self.filled, 0);
        self.filled -= consumed;
        // 解码器在定位过程中要求再跳一次（FLAC 二分）。
        if let Some(offset) = self.decoder.seek_request() {
            self.jump(offset);
            return None;
        }
        // 文件头已解析、首次产出 PCM：把定位目标交给解码器。
        if let (Some(target), Status::Frames(_) | Status::NeedOutput) = (self.seek, progress.status)
        {
            self.seek = None;
            match self.decoder.seek(target) {
                Ok(offset) => {
                    self.seek_target = Some(target);
                    self.jump(offset);
                    return None;
                }
                Err(SeekError::PastEnd) => {
                    self.state = State::EndOfTrack;
                    return None;
                }
                // 已在数据开头：就地顺序解码，丢弃目标帧之前的样本。
                Err(SeekError::Unsupported) => {
                    self.discard = target;
                    self.sequential = true;
                }
            }
        }
        match progress.status {
            Status::Frames(_) | Status::NeedOutput => self.output_or_configure(),
            Status::NeedInput => {
                let max = self.input.len() - self.filled;
                if self.source_eof || max == 0 {
                    // 解码器契约：EOF 时不返回 NeedInput；窗口满仍缺输入为容量不足。
                    self.state = State::Fail(Fault::InputWindow);
                    return None;
                }
                let request = self.next_request;
                self.next_request = self.next_request.wrapping_add(1);
                self.state = State::Reading { request, max };
                Some(Action::Read {
                    generation: self.generation,
                    request,
                    max,
                })
            }
            Status::End => {
                self.state = State::EndOfTrack;
                None
            }
        }
    }

    /// 丢弃窗口中旧位置的数据，请宿主把读取位置移到 `offset`。
    fn jump(&mut self, offset: u64) {
        self.filled = 0;
        self.source_eof = false;
        self.state = State::Seek { offset };
    }

    fn output_or_configure(&mut self) -> Option<Action> {
        let Some(format) = self.decoder.format() else {
            self.state = State::Fail(Fault::Decode(DecodeError::Malformed));
            return None;
        };
        if self.discard > 0 {
            // 顺序定位：丢弃目标帧之前的样本（不配置输出、不送出）。
            let frames = (self.decoder.pcm().len() / usize::from(format.channels())) as u64;
            let n = self.discard.min(frames);
            self.decoder
                .release(usize::try_from(n).unwrap_or(usize::MAX));
            self.discard -= n;
            if self.decoder.pcm().is_empty() {
                self.state = State::Run;
                return None;
            }
        }
        if self.configured != Some(format) {
            self.state = State::Configure(format);
            return None;
        }
        let frames = self.decoder.pcm().len() / usize::from(format.channels());
        self.state = State::Output { frames };
        Some(Action::Output {
            generation: self.generation,
            frames,
        })
    }

    /// 当前 `Output` 动作对应的精确 PCM；其他状态返回 `None`。
    pub fn pcm(&self) -> Option<Pcm<'_, Exact>> {
        match self.state {
            State::Output { .. } => Pcm::exact(self.decoder.pcm(), self.decoder.format()?).ok(),
            _ => None,
        }
    }

    /// 宿主为当前读取请求写入数据的目标切片；代次或请求不符时返回 `None`。
    pub fn read_target(&mut self, generation: Generation, request: u32) -> Option<&mut [u8]> {
        match self.state {
            State::Reading { request: r, max } if r == request && generation == self.generation => {
                Some(&mut self.input[self.filled..self.filled + max])
            }
            _ => None,
        }
    }

    /// 处理宿主回报。旧代次或非当前请求返回 [`CoreError::Stale`] 且不改变状态。
    pub fn handle(&mut self, event: Event) -> Result<(), CoreError> {
        let generation = match event {
            Event::Opened { generation }
            | Event::OpenFailed { generation }
            | Event::ReadDone { generation, .. }
            | Event::ReadEof { generation, .. }
            | Event::ReadFailed { generation, .. }
            | Event::SeekDone { generation, .. }
            | Event::SeekFailed { generation, .. }
            | Event::Configured { generation }
            | Event::ConfigureFailed { generation }
            | Event::Output { generation, .. } => generation,
        };
        if generation != self.generation {
            return Err(CoreError::Stale);
        }
        let request_matches = |request: u32, state: State| -> Result<usize, CoreError> {
            match state {
                State::Reading { request: r, max } if r == request => Ok(max),
                _ => Err(CoreError::Stale),
            }
        };
        match event {
            Event::Opened { .. } | Event::OpenFailed { .. } => {
                if self.state != State::Opening {
                    return Err(CoreError::Protocol);
                }
                if matches!(event, Event::Opened { .. }) {
                    self.source_open = true;
                    self.state = State::Run;
                } else {
                    self.state = State::Fail(Fault::Open);
                }
            }
            Event::ReadDone { request, len, .. } => {
                let max = request_matches(request, self.state)?;
                if len > max {
                    return Err(CoreError::Protocol);
                }
                self.filled += len;
                self.state = State::Run;
            }
            Event::ReadEof { request, .. } => {
                request_matches(request, self.state)?;
                self.source_eof = true;
                self.state = State::Run;
            }
            Event::ReadFailed { request, .. } => {
                request_matches(request, self.state)?;
                self.state = State::Fail(Fault::Read);
            }
            Event::SeekDone { request, .. } | Event::SeekFailed { request, .. } => {
                match self.state {
                    State::Seeking { request: r } if r == request => {}
                    _ => return Err(CoreError::Stale),
                }
                if matches!(event, Event::SeekDone { .. }) {
                    self.state = State::Run;
                } else {
                    let target = self.seek_target.unwrap_or(0);
                    self.seek_sequentially(target);
                }
            }
            Event::Configured { .. } | Event::ConfigureFailed { .. } => {
                let State::Configuring(format) = self.state else {
                    return Err(CoreError::Protocol);
                };
                if matches!(event, Event::Configured { .. }) {
                    self.configured = Some(format);
                    self.state = State::Run;
                } else {
                    self.state = State::Fail(Fault::Configure);
                }
            }
            Event::Output { frames, .. } => {
                let State::Output { frames: available } = self.state else {
                    return Err(CoreError::Protocol);
                };
                if frames > available {
                    return Err(CoreError::Protocol);
                }
                self.decoder.release(frames);
                self.state = State::Run;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::boxed::Box;
    use std::vec;

    /// 可编排的测试解码器：先要求一次输入，然后产出固定帧，最后结束。
    struct ScriptDecoder {
        resets: u32,
        stage: u8,
        pcm: [i32; 4],
        pending: usize,
    }

    impl ScriptDecoder {
        fn new() -> Self {
            Self {
                resets: 0,
                stage: 0,
                pcm: [1, 2, 3, 4],
                pending: 0,
            }
        }
    }

    impl Decoder for ScriptDecoder {
        fn reset(&mut self) {
            self.resets += 1;
            self.stage = 0;
            self.pending = 0;
        }
        fn format(&self) -> Option<SourceFormat> {
            (self.stage > 0).then(|| SourceFormat::new(48_000, 2, 16).ok())?
        }
        fn decode(&mut self, input: &[u8], eof: bool) -> Result<Progress, DecodeError> {
            match self.stage {
                0 if input.is_empty() => {
                    if eof {
                        return Err(DecodeError::Truncated);
                    }
                    Ok(Progress {
                        consumed: 0,
                        status: Status::NeedInput,
                    })
                }
                0 => {
                    self.stage = 1;
                    self.pending = 4;
                    Ok(Progress {
                        consumed: input.len(),
                        status: Status::Frames(2),
                    })
                }
                _ => Ok(Progress {
                    consumed: 0,
                    status: Status::End,
                }),
            }
        }
        fn pcm(&self) -> &[i32] {
            &self.pcm[..self.pending]
        }
        fn release(&mut self, frames: usize) {
            self.pending = self.pending.saturating_sub(frames * 2);
        }
    }

    fn leak(n: usize) -> &'static mut [u8] {
        Box::leak(vec![0u8; n].into_boxed_slice())
    }

    fn core() -> Core<ScriptDecoder> {
        match Core::new(leak(INPUT_WINDOW_BYTES), ScriptDecoder::new()) {
            Ok(c) => c,
            Err(e) => panic!("{e:?}"),
        }
    }

    fn read(core: &mut Core<ScriptDecoder>) -> (Generation, u32) {
        let Some(Action::Read {
            generation,
            request,
            max,
        }) = core.poll()
        else {
            panic!("expected read");
        };
        assert_eq!(max, INPUT_WINDOW_BYTES);
        (generation, request)
    }

    #[test]
    fn rejects_small_input_window() {
        let r = Core::new(leak(INPUT_WINDOW_BYTES - 1), ScriptDecoder::new());
        assert!(matches!(
            r,
            Err(CoreError::CapacityExceeded {
                required: INPUT_WINDOW_BYTES,
                provided: 8191
            })
        ));
    }

    #[test]
    fn two_track_playlist_same_format_configures_once() {
        let mut c = core();
        c.play(0, 2);
        for track in 0..2u32 {
            let Some(Action::Open {
                generation: g,
                track: t,
            }) = c.poll()
            else {
                panic!("open");
            };
            assert_eq!(t, track);
            assert_eq!(c.poll(), None, "等待 Opened");
            assert_eq!(c.handle(Event::Opened { generation: g }), Ok(()));
            let (g2, req) = read(&mut c);
            assert_eq!(g2, g);
            let Some(dst) = c.read_target(g, req) else {
                panic!("target");
            };
            dst[0] = 9;
            assert_eq!(
                c.handle(Event::ReadDone {
                    generation: g,
                    request: req,
                    len: 1
                }),
                Ok(())
            );
            if track == 0 {
                let Some(Action::Configure { format, .. }) = c.poll() else {
                    panic!("configure");
                };
                assert_eq!(format.channels(), 2);
                assert_eq!(c.handle(Event::Configured { generation: g }), Ok(()));
            }
            assert_eq!(
                c.poll(),
                Some(Action::Output {
                    generation: g,
                    frames: 2
                })
            );
            assert_eq!(c.pcm().map(|p| p.samples().len()), Some(4));
            assert_eq!(
                c.handle(Event::Output {
                    generation: g,
                    frames: 3
                }),
                Err(CoreError::Protocol)
            );
            assert_eq!(
                c.handle(Event::Output {
                    generation: g,
                    frames: 1
                }),
                Ok(())
            );
            assert_eq!(
                c.poll(),
                Some(Action::Output {
                    generation: g,
                    frames: 1
                })
            );
            assert_eq!(
                c.handle(Event::Output {
                    generation: g,
                    frames: 1
                }),
                Ok(())
            );
            assert_eq!(c.poll(), Some(Action::Close { generation: g }));
        }
        assert!(matches!(c.poll(), Some(Action::Finished { .. })));
        assert!(c.is_idle());
        assert_eq!(c.poll(), None);
    }

    #[test]
    fn stale_events_rejected_after_stop_and_new_track() {
        let mut c = core();
        c.play(0, 1);
        let Some(Action::Open { generation: g1, .. }) = c.poll() else {
            panic!("open");
        };
        assert_eq!(c.handle(Event::Opened { generation: g1 }), Ok(()));
        let (_, req) = read(&mut c);
        assert_eq!(c.stop(), Some(Action::Close { generation: g1 }));
        assert!(c.read_target(g1, req).is_none());
        assert_eq!(
            c.handle(Event::ReadDone {
                generation: g1,
                request: req,
                len: 1
            }),
            Err(CoreError::Stale)
        );
        c.play(0, 1);
        let Some(Action::Open { generation: g2, .. }) = c.poll() else {
            panic!("open");
        };
        assert!(g2 > g1);
        assert_eq!(
            c.handle(Event::Opened { generation: g1 }),
            Err(CoreError::Stale)
        );
        assert_eq!(c.handle(Event::Opened { generation: g2 }), Ok(()));
        let (_, req2) = read(&mut c);
        assert_ne!(req, req2);
        // 当前代次但旧请求 ID 也被拒绝。
        assert_eq!(
            c.handle(Event::ReadDone {
                generation: g2,
                request: req,
                len: 1
            }),
            Err(CoreError::Stale)
        );
        assert_eq!(
            c.handle(Event::ReadDone {
                generation: g2,
                request: req2,
                len: INPUT_WINDOW_BYTES + 1
            }),
            Err(CoreError::Protocol)
        );
    }

    #[test]
    fn faults_close_then_fail() {
        // 打开失败：没有打开的音源，直接 Failed。
        let mut c = core();
        c.play(0, 3);
        let Some(Action::Open { generation: g, .. }) = c.poll() else {
            panic!("open");
        };
        assert_eq!(c.handle(Event::OpenFailed { generation: g }), Ok(()));
        assert_eq!(
            c.poll(),
            Some(Action::Failed {
                generation: g,
                track: 0,
                fault: Fault::Open
            })
        );
        assert_eq!(c.poll(), None);

        // 零长度读取不是 EOF；EOF 后解码器报截断 → Close 再 Failed。
        c.play(0, 1);
        let Some(Action::Open { generation: g, .. }) = c.poll() else {
            panic!("open");
        };
        assert_eq!(c.handle(Event::Opened { generation: g }), Ok(()));
        let (_, req) = read(&mut c);
        assert_eq!(
            c.handle(Event::ReadDone {
                generation: g,
                request: req,
                len: 0
            }),
            Ok(())
        );
        let (_, req) = read(&mut c);
        assert_eq!(
            c.handle(Event::ReadEof {
                generation: g,
                request: req
            }),
            Ok(())
        );
        assert_eq!(c.poll(), Some(Action::Close { generation: g }));
        assert_eq!(
            c.poll(),
            Some(Action::Failed {
                generation: g,
                track: 0,
                fault: Fault::Decode(DecodeError::Truncated)
            })
        );

        // 读取失败与配置失败。
        c.play(0, 1);
        let Some(Action::Open { generation: g, .. }) = c.poll() else {
            panic!("open");
        };
        assert_eq!(c.handle(Event::Opened { generation: g }), Ok(()));
        let (_, req) = read(&mut c);
        assert_eq!(
            c.handle(Event::ReadFailed {
                generation: g,
                request: req
            }),
            Ok(())
        );
        assert_eq!(c.poll(), Some(Action::Close { generation: g }));
        assert!(matches!(
            c.poll(),
            Some(Action::Failed {
                fault: Fault::Read,
                ..
            })
        ));

        c.play(0, 1);
        let Some(Action::Open { generation: g, .. }) = c.poll() else {
            panic!("open");
        };
        assert_eq!(c.handle(Event::Opened { generation: g }), Ok(()));
        let (_, req) = read(&mut c);
        assert_eq!(
            c.handle(Event::ReadDone {
                generation: g,
                request: req,
                len: 1
            }),
            Ok(())
        );
        assert!(matches!(c.poll(), Some(Action::Configure { .. })));
        assert_eq!(
            c.handle(Event::Output {
                generation: g,
                frames: 0
            }),
            Err(CoreError::Protocol)
        );
        assert_eq!(c.handle(Event::ConfigureFailed { generation: g }), Ok(()));
        assert_eq!(c.poll(), Some(Action::Close { generation: g }));
        assert!(matches!(
            c.poll(),
            Some(Action::Failed {
                fault: Fault::Configure,
                ..
            })
        ));
    }

    #[test]
    fn stop_invalidates_configuration() {
        let mut c = core();
        c.play(0, 1);
        let Some(Action::Open { generation: g, .. }) = c.poll() else {
            panic!("open");
        };
        assert_eq!(c.handle(Event::Opened { generation: g }), Ok(()));
        let (_, req) = read(&mut c);
        assert_eq!(
            c.handle(Event::ReadDone {
                generation: g,
                request: req,
                len: 1
            }),
            Ok(())
        );
        assert!(matches!(c.poll(), Some(Action::Configure { .. })));
        assert_eq!(c.handle(Event::Configured { generation: g }), Ok(()));
        assert!(matches!(c.poll(), Some(Action::Output { .. })));
        assert_eq!(c.stop(), Some(Action::Close { generation: g }));
        assert!(c.pcm().is_none());
        c.play(0, 1);
        let Some(Action::Open { generation: g, .. }) = c.poll() else {
            panic!("open");
        };
        assert_eq!(c.handle(Event::Opened { generation: g }), Ok(()));
        let (_, req) = read(&mut c);
        assert_eq!(
            c.handle(Event::ReadDone {
                generation: g,
                request: req,
                len: 1
            }),
            Ok(())
        );
        assert!(
            matches!(c.poll(), Some(Action::Configure { .. })),
            "停止后须重新配置"
        );
    }

    /// 始终缺输入、不消耗的解码器（违反“EOF 时不返回 NeedInput”的契约）。
    struct Hungry;

    impl Decoder for Hungry {
        fn reset(&mut self) {}
        fn format(&self) -> Option<SourceFormat> {
            None
        }
        fn decode(&mut self, _: &[u8], _: bool) -> Result<Progress, DecodeError> {
            Ok(Progress {
                consumed: 0,
                status: Status::NeedInput,
            })
        }
        fn pcm(&self) -> &[i32] {
            &[]
        }
        fn release(&mut self, _: usize) {}
    }

    fn open_hungry() -> (Core<Hungry>, Generation, u32) {
        let Ok(mut c) = Core::new(leak(INPUT_WINDOW_BYTES), Hungry) else {
            panic!("core");
        };
        c.play(0, 1);
        let Some(Action::Open { generation: g, .. }) = c.poll() else {
            panic!("open");
        };
        assert_eq!(c.handle(Event::Opened { generation: g }), Ok(()));
        let Some(Action::Read { request, max, .. }) = c.poll() else {
            panic!("read");
        };
        assert_eq!(max, INPUT_WINDOW_BYTES);
        (c, g, request)
    }

    /// M2 T5：EOF 后仍缺输入、或输入窗口已满仍缺输入，都是输入窗口错误（不再发出读取）。
    #[test]
    fn need_input_at_eof_or_full_window_is_input_window_fault() {
        let (mut c, g, request) = open_hungry();
        assert_eq!(
            c.handle(Event::ReadEof {
                generation: g,
                request
            }),
            Ok(())
        );
        assert_eq!(c.poll(), Some(Action::Close { generation: g }));
        assert_eq!(
            c.poll(),
            Some(Action::Failed {
                generation: g,
                track: 0,
                fault: Fault::InputWindow
            })
        );

        let (mut c, g, request) = open_hungry();
        assert_eq!(
            c.handle(Event::ReadDone {
                generation: g,
                request,
                len: INPUT_WINDOW_BYTES
            }),
            Ok(())
        );
        assert_eq!(c.poll(), Some(Action::Close { generation: g }));
        assert!(matches!(
            c.poll(),
            Some(Action::Failed {
                fault: Fault::InputWindow,
                ..
            })
        ));

        // 未满时照常继续读取剩余空间。
        let (mut c, g, request) = open_hungry();
        assert_eq!(
            c.handle(Event::ReadDone {
                generation: g,
                request,
                len: 100
            }),
            Ok(())
        );
        assert!(matches!(
            c.poll(),
            Some(Action::Read { max, .. }) if max == INPUT_WINDOW_BYTES - 100
        ));
    }

    /// 读取目标只交给当前代次的当前请求。
    #[test]
    fn read_target_requires_current_generation_and_request() {
        let (mut c, g, request) = open_hungry();
        assert!(c.read_target(Generation(g.0 + 1), request).is_none());
        assert!(c.read_target(g, request.wrapping_add(1)).is_none());
        assert_eq!(
            c.read_target(g, request).map(|t| t.len()),
            Some(INPUT_WINDOW_BYTES)
        );
    }

    /// 起始曲目不在列表内时直接结束，不打开音源。
    #[test]
    fn play_past_end_finishes_without_open() {
        let mut c = core();
        c.play(2, 2);
        assert!(matches!(c.poll(), Some(Action::Finished { .. })));
        c.play(1, 2);
        assert!(matches!(c.poll(), Some(Action::Open { track: 1, .. })));
    }

    /// 输入窗口切片短于声明的窗口时拒绝；相等时接受。
    #[test]
    fn with_window_checks_declared_window() {
        let r = Core::with_window(leak(20_000), 20_001, ScriptDecoder::new());
        assert!(matches!(
            r,
            Err(CoreError::CapacityExceeded {
                required: 20_001,
                provided: 20_000
            })
        ));
        assert!(Core::with_window(leak(20_000), 20_000, ScriptDecoder::new()).is_ok());
    }

    /// 默认工作区：4096 帧双声道。
    #[test]
    fn default_workspace_sizes() {
        assert_eq!(core::hint::black_box(PCM_WORK_SAMPLES), 8_192);
        assert_eq!(core::hint::black_box(PCM_WORK_FRAMES), 4_096);
    }
}
