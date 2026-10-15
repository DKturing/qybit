//! M2 T5：引擎对宿主公开的状态（曲目序号、设备格式、播放计划、统计）与队列中的曲目边界上限。
//!
//! 用最小的同步宿主循环驱动真实引擎、WAV 解码器、pack 与队列：读取一次给足，设备不消费数据，
//! 直到测试显式从队列取走。期望值按独立公式给出。

use qy_core::engine::{EngineFault, HostAction, HostEvent, Poll, Wait};
use qy_core::{DeviceFormat, SourceFormat};
use qy_host_sim::SimHost;
use qy_pcm_format::{Endian, Placement};

/// 整数 PCM WAV（容器 = 有效位）。
fn wav(rate: u32, channels: u16, bits: u16, frames: usize) -> Vec<u8> {
    let ba = channels * bits / 8;
    let len = frames * usize::from(ba);
    let mut f = Vec::new();
    f.extend_from_slice(b"RIFF");
    f.extend_from_slice(&(36 + len as u32).to_le_bytes());
    f.extend_from_slice(b"WAVEfmt ");
    f.extend_from_slice(&16u32.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&channels.to_le_bytes());
    f.extend_from_slice(&rate.to_le_bytes());
    f.extend_from_slice(&(rate * u32::from(ba)).to_le_bytes());
    f.extend_from_slice(&ba.to_le_bytes());
    f.extend_from_slice(&bits.to_le_bytes());
    f.extend_from_slice(b"data");
    f.extend_from_slice(&(len as u32).to_le_bytes());
    f.extend((0..len).map(|i| (i * 7 + 1) as u8));
    f
}

/// 与源同宽的设备格式（容器 = 有效位 = 源位数）。
fn same_width(s: &SourceFormat) -> DeviceFormat {
    let Ok(d) = DeviceFormat::new(
        s.sample_rate(),
        s.channels(),
        s.bits(),
        s.bits(),
        Endian::Little,
        Placement::MsbAligned,
    ) else {
        panic!("设备格式");
    };
    d
}

struct Mini {
    host: SimHost,
    files: Vec<Vec<u8>>,
    open: usize,
    pos: usize,
    started: bool,
    finished: bool,
    selects: Vec<SourceFormat>,
    /// 为真时打开设备后不重配置帧队列（宿主违约，用于检查输出错误路径）。
    skip_queue: bool,
    failed: Option<(u32, EngineFault)>,
}

impl Mini {
    fn new(files: Vec<Vec<u8>>) -> Self {
        let Ok(host) = SimHost::new() else {
            panic!("宿主");
        };
        Self {
            host,
            files,
            open: 0,
            pos: 0,
            started: false,
            finished: false,
            selects: Vec::new(),
            skip_queue: false,
            failed: None,
        }
    }

    fn handle(&mut self, e: HostEvent) {
        assert_eq!(self.host.engine.handle(e), Ok(()), "{e:?}");
    }

    /// 执行一个宿主动作；`SelectDevice` 交给调用方（返回 `false`）。
    fn act(&mut self, a: HostAction) -> bool {
        match a {
            HostAction::Open { generation, track } => {
                self.open = track as usize;
                self.pos = 0;
                self.handle(HostEvent::Opened { generation });
            }
            HostAction::Read {
                generation,
                request,
                max,
            } => {
                let file = &self.files[self.open];
                let n = max.min(file.len() - self.pos);
                if n == 0 {
                    self.handle(HostEvent::ReadEof {
                        generation,
                        request,
                    });
                } else {
                    let Some(dst) = self.host.engine.read_target(generation, request) else {
                        panic!("读取目标");
                    };
                    dst[..n].copy_from_slice(&file[self.pos..self.pos + n]);
                    self.pos += n;
                    self.handle(HostEvent::ReadDone {
                        generation,
                        request,
                        len: n,
                    });
                }
            }
            HostAction::SelectDevice { source } => {
                self.selects.push(source);
                return false;
            }
            HostAction::OpenDevice { source, format } => {
                let f = format.unwrap_or_else(|| same_width(&source));
                if !self.skip_queue {
                    assert!(self.host.configure_device(&f).is_ok());
                }
                self.handle(HostEvent::DeviceOpened { format: f });
            }
            HostAction::StartDevice => self.started = true,
            HostAction::DrainDevice => {
                self.started = true;
                self.consume_all();
                self.handle(HostEvent::DeviceDrained);
            }
            HostAction::Finished => self.finished = true,
            HostAction::Failed { track, fault } => self.failed = Some((track, fault)),
            HostAction::Close { .. } | HostAction::StopDevice => {}
            other => panic!("意外动作 {other:?}"),
        }
        true
    }

    /// 推进到需要选择设备或等待为止；选择设备时回报同宽格式，除非 `stop_at_select`。
    fn run(&mut self, stop_at_select: bool) -> Poll {
        loop {
            match self.host.engine.poll() {
                Poll::Action(a) => {
                    if !self.act(a) {
                        if stop_at_select {
                            return Poll::Wait(Wait::Device);
                        }
                        let s = *self.selects.last().unwrap_or_else(|| panic!("源"));
                        self.handle(HostEvent::DeviceSelected {
                            format: same_width(&s),
                        });
                    }
                }
                Poll::Yield => {}
                p => return p,
            }
        }
    }

    fn consume_all(&mut self) -> usize {
        if self.skip_queue {
            return 0;
        }
        let fb = self.host.consumer.frame_bytes();
        let n = self.host.consumer.available_frames();
        let mut out = vec![0u8; n * fb];
        assert_eq!(self.host.consumer.pop_frames(&mut out), Ok(n));
        n
    }
}

/// 设备未定时的播放计划按源的最小容器估计帧宽；之后报告设备格式、曲目序号与入队帧数。
#[test]
fn accessors_and_plan_before_and_after_device() {
    let files = vec![wav(8_000, 2, 24, 300), wav(8_000, 2, 24, 200)];
    let mut m = Mini::new(files);
    m.host.engine.play(1, 2);
    assert_eq!((m.host.engine.track(), m.host.engine.track_count()), (1, 2));
    m.host.engine.play(0, 2);
    assert_eq!((m.host.engine.track(), m.host.engine.track_count()), (0, 2));
    assert_eq!(m.host.engine.plan(), None, "源格式未知");
    assert_eq!(m.run(true), Poll::Wait(Wait::Device));
    assert_eq!(m.host.engine.device_format(), None);
    let Some(plan) = m.host.engine.plan() else {
        panic!("计划");
    };
    // 500 ms × 8000 Hz = 4000 帧；双声道 24 位最小容器 3 字节 → 每帧 6 字节。
    assert_eq!(plan.prefill_frames, 4_000);
    assert_eq!(plan.device, None);
    assert_eq!(plan.queue_required, 4_000 * 6);

    let source = m.selects[0];
    m.handle(HostEvent::DeviceSelected {
        format: same_width(&source),
    });
    assert_eq!(m.run(false), Poll::Idle);
    assert!(m.finished && m.started);
    assert_eq!(m.host.engine.device_format(), Some(same_width(&source)));
    assert_eq!(m.host.engine.track(), 1);
    let s = m.host.engine.stats();
    assert_eq!(s.frames_pushed, 500);
    assert_eq!((s.stale, s.protocol_errors, s.skipped), (0, 0, 0));
    let Some(plan) = m.host.engine.plan() else {
        panic!("计划");
    };
    assert_eq!(
        plan.queue_required,
        4_000 * 6,
        "设备 24/24 位帧宽同为 6 字节"
    );
}

/// 队列中最多记录 8 个曲目边界：设备未取数时，第 9 首短曲目等设备播完旧曲目后才入队，
/// 正在播放的曲目因此始终可知。
#[test]
fn queued_track_boundaries_are_bounded() {
    const FRAMES: usize = 10;
    let files: Vec<Vec<u8>> = (0..12).map(|_| wav(8_000, 1, 16, FRAMES)).collect();
    let mut m = Mini::new(files);
    m.host.engine.play(0, 12);
    assert_eq!(m.run(false), Poll::Wait(Wait::QueueFull));
    assert!(m.started, "边界已满时启动设备");
    assert_eq!(m.host.consumer.available_frames(), 8 * FRAMES);
    assert_eq!(m.host.engine.track(), 8, "第 9 首已解码、等待入队");
    assert_eq!(m.host.engine.playing_track(), 0);
    assert_eq!(m.consume_all(), 8 * FRAMES);
    assert_eq!(m.host.engine.playing_track(), 7);
    assert_eq!(m.run(false), Poll::Idle);
    assert!(m.finished);
    assert_eq!(m.host.engine.stats().frames_pushed, 12 * FRAMES as u64);
    assert_eq!(m.host.engine.stats().protocol_errors, 0);
}

/// 设备断开：关闭音源、不再排空，引擎停止；再次播放时重新打开设备。
#[test]
fn device_lost_stops_without_drain_and_reopens_on_play() {
    // 1.2 MB，大于默认队列（768 KB），因此停在队列满。
    let files = vec![wav(48_000, 2, 16, 300_000)];
    let mut m = Mini::new(files);
    m.host.engine.play(0, 1);
    assert_eq!(m.run(false), Poll::Wait(Wait::QueueFull));
    assert!(m.host.engine.is_active() && m.host.engine.device_format().is_some());
    m.host.engine.device_lost();
    assert!(matches!(
        m.host.engine.take_control(),
        Some(HostAction::Close { .. })
    ));
    assert_eq!(m.host.engine.take_control(), None);
    assert!(!m.host.engine.is_active());
    assert_eq!(m.host.engine.device_format(), None);
    assert_eq!(m.host.engine.poll(), Poll::Idle);
    assert!(!m.finished, "断开后不报告正常结束");

    m.host.engine.play(0, 1);
    m.started = false;
    m.consume_all();
    assert_eq!(m.run(false), Poll::Wait(Wait::QueueFull));
    assert!(m.started, "新设备重新预填并启动");
    assert!(m.host.engine.device_format().is_some());
}

/// 控制动作队列（6 项）溢出时不覆盖已排队的动作，计为协议错误：宿主应在每次控制后取走动作。
#[test]
fn control_outbox_overflow_is_counted() {
    let files = vec![wav(48_000, 2, 16, 300_000)];
    let mut m = Mini::new(files);
    m.host.engine.play(0, 1);
    assert_eq!(m.run(false), Poll::Wait(Wait::QueueFull));
    assert!(m.started);
    for _ in 0..4 {
        m.host.engine.pause();
        m.host.engine.resume();
    }
    assert_eq!(m.host.engine.stats().protocol_errors, 2);
    let mut taken = Vec::new();
    while let Some(a) = m.host.engine.take_control() {
        taken.push(a);
    }
    assert_eq!(
        taken,
        [HostAction::PauseDevice, HostAction::ResumeDevice].repeat(3)
    );
}

/// 帧队列入队失败（宿主未按设备格式重配置队列）：关闭音源、排空后报告输出错误并停止。
#[test]
fn queue_push_failure_fails_with_output_fault() {
    let files = vec![wav(8_000, 2, 16, 100), wav(8_000, 2, 16, 100)];
    let mut m = Mini::new(files);
    m.skip_queue = true;
    m.host.engine.play(0, 2);
    assert_eq!(m.run(false), Poll::Idle);
    assert_eq!(m.failed, Some((0, EngineFault::Output)));
    assert!(!m.finished && !m.host.engine.is_active());
    assert_eq!(m.host.engine.stats().frames_pushed, 0);
}

/// 调试输出含状态与统计（宿主日志用）。
#[test]
fn debug_output_shows_state_and_stats() {
    let m = Mini::new(Vec::new());
    let text = format!("{:?}", m.host.engine);
    assert!(text.starts_with("Engine {"), "{text}");
    for field in [
        "mode: Idle",
        "device: None",
        "paused: false",
        "frames_pushed: 0",
    ] {
        assert!(text.contains(field), "{text}");
    }
}
