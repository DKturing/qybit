//! QyFi sans-IO 播放核心。
//!
//! `#![no_std]`、不用 `alloc`，不访问文件、设备、线程或系统时钟（AGENTS §2）。
//! 核心通过 [`Action`] 请求宿主执行 I/O，通过 [`Event`] 接收结果；
//! 输入窗口由宿主启动时一次性提供 `&'static mut [u8]`，停止/换曲时复用。
//!
//! M0 T1 只建立骨架：缓冲契约、代次/请求 ID 过滤与解码器接口。
//! 解码循环、背压、EOF 排空在 T2/T4 实现；`Action`、`Event`、`Decoder`
//! 在 T4 验收前仍可调整。
#![no_std]

pub use qy_pcm_format::{DeviceFormat, MAX_CHANNELS, MAX_SAMPLE_RATE, SourceFormat};

/// M0 默认输入窗口字节数。
pub const INPUT_WINDOW_BYTES: usize = 8192;
/// M0 默认 PCM 工作区帧数。
pub const PCM_WORK_FRAMES: usize = 4096;
/// M0 默认 PCM 工作区样本数（4096 帧 × 最多 2 声道）。
pub const PCM_WORK_SAMPLES: usize = PCM_WORK_FRAMES * qy_pcm_format::MAX_CHANNELS as usize;

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
}

/// 播放代次：每次开始/停止/换曲递增，旧代次的回报一律拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Generation(pub u32);

/// 核心请求宿主执行的操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 打开第 `track` 首音源。
    Open { generation: Generation, track: u32 },
    /// 读取至多 `max` 字节，写入 [`Core::read_target`] 返回的切片。
    Read {
        generation: Generation,
        request: u32,
        max: usize,
    },
    /// 关闭当前音源。
    Close { generation: Generation },
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
}

/// 核心错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreError {
    /// 宿主提供的缓冲小于声明容量。
    CapacityExceeded { required: usize, provided: usize },
    /// 回报属于旧代次或非当前请求，已丢弃。
    Stale,
    /// 回报与当前状态不符（例如读取长度超出请求）。
    Protocol,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Opening,
    Reading { request: u32, max: usize },
    Ready,
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
        })
    }

    pub fn generation(&self) -> Generation {
        self.generation
    }

    pub fn decoder(&self) -> &D {
        &self.decoder
    }

    /// 输入窗口中已缓冲的字节。
    pub fn buffered(&self) -> &[u8] {
        &self.input[..self.filled]
    }

    pub fn source_eof(&self) -> bool {
        self.source_eof
    }

    pub fn decoder_mut(&mut self) -> &mut D {
        &mut self.decoder
    }

    /// 用已缓冲输入驱动解码器一次，并从窗口头部移除已消耗字节。
    /// 源 EOF 已确认时以 `eof = true` 调用，解码器据此区分截断与缺输入。
    pub fn decode(&mut self) -> Result<Progress, DecodeError> {
        let progress = self
            .decoder
            .decode(&self.input[..self.filled], self.source_eof)?;
        debug_assert!(progress.consumed <= self.filled, "解码器消耗超出输入");
        let consumed = progress.consumed.min(self.filled);
        self.input.copy_within(consumed..self.filled, 0);
        self.filled -= consumed;
        Ok(progress)
    }

    /// 开始播放第 `track` 首：进入新代次，复用输入窗口和解码器。
    pub fn start(&mut self, track: u32) -> Action {
        self.reset_for_new_generation();
        self.state = State::Opening;
        Action::Open {
            generation: self.generation,
            track,
        }
    }

    /// 停止：进入新代次，此后旧代次的回报全部拒绝。
    pub fn stop(&mut self) -> Action {
        let closing = self.generation;
        self.reset_for_new_generation();
        Action::Close {
            generation: closing,
        }
    }

    fn reset_for_new_generation(&mut self) {
        self.generation = Generation(self.generation.0.wrapping_add(1));
        self.filled = 0;
        self.source_eof = false;
        self.state = State::Idle;
        self.decoder.reset();
    }

    /// 发起一次读取；仅在已打开、无未完成读取、未 EOF 且窗口有空间时返回。
    pub fn request_read(&mut self) -> Option<Action> {
        if self.state != State::Ready || self.source_eof {
            return None;
        }
        let max = self.input.len() - self.filled;
        if max == 0 {
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
            | Event::ReadFailed { generation, .. } => generation,
        };
        if generation != self.generation {
            return Err(CoreError::Stale);
        }
        match (event, self.state) {
            (Event::Opened { .. }, State::Opening) => {
                self.state = State::Ready;
                Ok(())
            }
            (Event::OpenFailed { .. }, State::Opening) => {
                self.state = State::Idle;
                Ok(())
            }
            (Event::ReadDone { request, len, .. }, State::Reading { request: r, max }) => {
                if request != r {
                    return Err(CoreError::Stale);
                }
                if len > max {
                    return Err(CoreError::Protocol);
                }
                self.filled += len;
                self.state = State::Ready;
                Ok(())
            }
            (Event::ReadEof { request, .. }, State::Reading { request: r, .. }) => {
                if request != r {
                    return Err(CoreError::Stale);
                }
                self.source_eof = true;
                self.state = State::Ready;
                Ok(())
            }
            (Event::ReadFailed { request, .. }, State::Reading { request: r, .. }) => {
                if request != r {
                    return Err(CoreError::Stale);
                }
                self.state = State::Idle;
                Ok(())
            }
            (Event::ReadDone { .. } | Event::ReadEof { .. } | Event::ReadFailed { .. }, _) => {
                Err(CoreError::Stale)
            }
            _ => Err(CoreError::Protocol),
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::boxed::Box;
    use std::vec;

    struct NullDecoder {
        resets: u32,
    }

    impl Decoder for NullDecoder {
        fn reset(&mut self) {
            self.resets += 1;
        }
        fn format(&self) -> Option<SourceFormat> {
            None
        }
        fn decode(&mut self, _input: &[u8], _eof: bool) -> Result<Progress, DecodeError> {
            Ok(Progress {
                consumed: 0,
                status: Status::NeedInput,
            })
        }
        fn pcm(&self) -> &[i32] {
            &[]
        }
        fn release(&mut self, _frames: usize) {}
    }

    fn leak(n: usize) -> &'static mut [u8] {
        Box::leak(vec![0u8; n].into_boxed_slice())
    }

    #[test]
    fn rejects_small_input_window() {
        let r = Core::new(leak(INPUT_WINDOW_BYTES - 1), NullDecoder { resets: 0 });
        assert!(matches!(
            r,
            Err(CoreError::CapacityExceeded {
                required: INPUT_WINDOW_BYTES,
                provided: 8191
            })
        ));
    }

    #[test]
    fn read_cycle_and_stale_rejection() {
        let Ok(mut core) = Core::new(leak(INPUT_WINDOW_BYTES), NullDecoder { resets: 0 }) else {
            panic!("construct");
        };
        let Action::Open { generation: g1, .. } = core.start(0) else {
            panic!("expected open");
        };
        assert_eq!(core.handle(Event::Opened { generation: g1 }), Ok(()));
        let Some(Action::Read { request, max, .. }) = core.request_read() else {
            panic!("expected read");
        };
        assert_eq!(max, INPUT_WINDOW_BYTES);
        let Some(dst) = core.read_target(g1, request) else {
            panic!("target");
        };
        dst[..3].copy_from_slice(&[1, 2, 3]);
        assert_eq!(
            core.handle(Event::ReadDone {
                generation: g1,
                request,
                len: 3
            }),
            Ok(())
        );
        assert_eq!(core.buffered(), &[1, 2, 3]);

        // 换曲后旧代次回报被拒绝，窗口复用且清空。
        let Some(Action::Read { request: r2, .. }) = core.request_read() else {
            panic!("expected read");
        };
        let Action::Open { generation: g2, .. } = core.start(1) else {
            panic!("expected open");
        };
        assert!(g2 > g1);
        assert_eq!(core.decoder().resets, 2);
        assert!(core.read_target(g1, r2).is_none());
        assert_eq!(
            core.handle(Event::ReadDone {
                generation: g1,
                request: r2,
                len: 1
            }),
            Err(CoreError::Stale)
        );
        assert!(core.buffered().is_empty());
    }

    #[test]
    fn eof_is_distinct_from_zero_length_read() {
        let Ok(mut core) = Core::new(leak(INPUT_WINDOW_BYTES), NullDecoder { resets: 0 }) else {
            panic!("construct");
        };
        let Action::Open { generation: g, .. } = core.start(0) else {
            panic!("expected open");
        };
        assert_eq!(core.handle(Event::Opened { generation: g }), Ok(()));
        let Some(Action::Read { request, .. }) = core.request_read() else {
            panic!("read");
        };
        assert_eq!(
            core.handle(Event::ReadDone {
                generation: g,
                request,
                len: 0
            }),
            Ok(())
        );
        assert!(!core.source_eof());
        let Some(Action::Read { request, max, .. }) = core.request_read() else {
            panic!("read");
        };
        assert_eq!(
            core.handle(Event::ReadDone {
                generation: g,
                request,
                len: max + 1
            }),
            Err(CoreError::Protocol)
        );
        assert_eq!(
            core.handle(Event::ReadEof {
                generation: g,
                request
            }),
            Ok(())
        );
        assert!(core.source_eof());
        assert!(core.request_read().is_none());
    }
}
