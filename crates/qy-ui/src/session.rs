//! 播放线程：一次性分配宿主资源，按请求依次播放（每次播放是一个“会话”）。
//!
//! 每个会话开始时建立新的命令通道，把发送端随 [`UiEvent::Started`] 交给界面线程；会话结束后
//! 该通道作废，因此旧会话的命令不会落到新会话上。状态、文字与结束报告都带会话编号，界面只采用
//! 当前会话的事件。

use std::fs::File;
use std::io::{self, ErrorKind, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use qy_core::engine::{ChannelPolicy, FloatPolicy};
use qy_player::{
    Backend, CommandSender, HostResources, PlayControl, Status, TrackSource, command_channel,
    play_with,
};

use crate::model::Outcome;
use crate::probe::TrackInfo;

/// 前端提供的通知：后台线程每发送一个事件调用一次（应很快返回，如 `PostMessageW`）。
pub type Notify = Arc<dyn Fn() + Send + Sync>;

/// 后台线程交给界面线程的事件。
#[derive(Debug)]
pub enum UiEvent {
    /// 文件头读取结果（`entry` 为播放列表条目编号）。
    Probed {
        entry: u64,
        result: Result<TrackInfo, String>,
    },
    /// 文件夹扫描结果（按路径排序）。
    Scanned {
        files: Vec<PathBuf>,
    },
    /// 会话开始：此后的控制命令经 `commands` 发送。
    Started {
        session: u64,
        commands: CommandSender,
    },
    Status {
        session: u64,
        status: Status,
    },
    /// 实时进度文字（曲目开始、设备协商结果、跳过、修正、失败等）。
    Line {
        session: u64,
        text: String,
    },
    Ended {
        session: u64,
        outcome: Outcome,
    },
    /// 后台线程无法工作（如资源分配失败）。
    Fatal(String),
}

/// 一次播放请求。
#[derive(Debug, Clone)]
pub(crate) struct PlayRequest {
    pub session: u64,
    pub files: Vec<PathBuf>,
    pub first: u32,
    /// 输出设备（平台端点标识）；`None` 时由后端选择默认设备。
    pub device: Option<String>,
    pub mono_to_stereo: bool,
    /// 浮点转整数（F3，明确开启的非严格选项）。
    pub float_to_int: bool,
    /// 故障注入：读取速率上限（字节/秒），见 [`crate::Model::read_limit`]。
    pub read_limit: Option<u64>,
}

/// 文件路径列表。
struct Paths<'a> {
    files: &'a [PathBuf],
    read_limit: Option<u64>,
}

/// 音频文件读取；有速率上限时每次至多读 50 ms 的量，并按累计读取量等待（模拟慢速存储）。
struct Limited {
    file: File,
    limit: Option<u64>,
    start: Instant,
    done: u64,
}

impl Read for Limited {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(rate) = self.limit.filter(|&r| r > 0) else {
            return self.file.read(buf);
        };
        let cap = usize::try_from(rate / 20).unwrap_or(usize::MAX).max(1);
        let len = buf.len().min(cap);
        let n = self.file.read(&mut buf[..len])?;
        self.done = self.done.saturating_add(n as u64);
        let due = Duration::from_secs_f64(self.done as f64 / rate as f64);
        if let Some(wait) = due.checked_sub(self.start.elapsed()) {
            std::thread::sleep(wait);
        }
        Ok(n)
    }
}

/// 定位后限速重新计时（定位不算读取量）。
impl Seek for Limited {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let at = self.file.seek(pos)?;
        self.start = Instant::now();
        self.done = 0;
        Ok(at)
    }
}

impl TrackSource for Paths<'_> {
    type Reader = Limited;

    fn count(&self) -> u32 {
        u32::try_from(self.files.len()).unwrap_or(u32::MAX)
    }

    fn open(&mut self, track: u32) -> io::Result<Limited> {
        match self.files.get(track as usize) {
            Some(p) => Ok(Limited {
                file: File::open(p)?,
                limit: self.read_limit,
                start: Instant::now(),
                done: 0,
            }),
            None => Err(io::Error::new(ErrorKind::NotFound, "曲目编号越界")),
        }
    }

    fn name(&self, track: u32) -> String {
        self.files.get(track as usize).map_or_else(
            || format!("第 {} 首", u64::from(track) + 1),
            |p| p.display().to_string(),
        )
    }
}

/// 播放线程主循环。`make_backend` 按设备标识创建输出后端（在本线程调用，如 WASAPI 需要的 COM
/// 初始化与之同线程）。
pub(crate) fn run<B, F>(
    requests: Receiver<PlayRequest>,
    events: Sender<UiEvent>,
    notify: Notify,
    mut make_backend: F,
) where
    B: Backend,
    F: FnMut(Option<&str>) -> Result<B, String>,
{
    let send = |e: UiEvent| {
        let ok = events.send(e).is_ok();
        notify();
        ok
    };
    let mut res = match HostResources::new() {
        Ok(r) => r,
        Err(e) => {
            send(UiEvent::Fatal(format!("播放资源分配失败：{e:?}")));
            return;
        }
    };
    while let Ok(req) = requests.recv() {
        let session = req.session;
        res.engine.set_channel_policy(if req.mono_to_stereo {
            ChannelPolicy::DuplicateMono
        } else {
            ChannelPolicy::Strict
        });
        res.engine.set_float_policy(if req.float_to_int {
            FloatPolicy::ToInt
        } else {
            FloatPolicy::Strict
        });
        let mut backend = match make_backend(req.device.as_deref()) {
            Ok(b) => b,
            Err(e) => {
                let outcome = Outcome::failed(format!("无法使用输出设备：{e}"));
                if !send(UiEvent::Ended { session, outcome }) {
                    return;
                }
                continue;
            }
        };
        let (tx, rx) = command_channel();
        if !send(UiEvent::Started {
            session,
            commands: tx,
        }) {
            return;
        }
        send(UiEvent::Line {
            session,
            text: if req.mono_to_stereo {
                "单声道复制到左右两路：开启（单声道 [FC] 曲目按 [FL FR] 播放，样本值不变，\
                 声道已改变，不是严格路径）"
                    .into()
            } else {
                "单声道复制到左右两路：关闭（严格路径；设备不接受单声道时该曲目无法播放）".into()
            },
        });
        send(UiEvent::Line {
            session,
            text: if req.float_to_int {
                "浮点转整数：开启（设备不接受浮点格式时转为整数，样本已改变，不是严格路径）".into()
            } else {
                "浮点转整数：关闭（严格路径；设备不接受浮点格式时该曲目无法播放）".into()
            },
        });
        let report = play_with(
            &mut backend,
            &mut res,
            &mut Paths {
                files: &req.files,
                read_limit: req.read_limit,
            },
            PlayControl {
                on_line: &mut |text: &str| {
                    send(UiEvent::Line {
                        session,
                        text: text.to_string(),
                    });
                },
                stop: None,
                commands: Some(&rx),
                first: req.first,
                on_status: Some(&mut |status: &Status| {
                    send(UiEvent::Status {
                        session,
                        status: *status,
                    });
                }),
            },
        );
        let outcome = Outcome::from_report(&report);
        if !send(UiEvent::Ended { session, outcome }) {
            return;
        }
    }
}
