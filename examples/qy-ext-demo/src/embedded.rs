//! 嵌入式组装：组合解码器经 `qy_host_embedded::Player::new` 接入，存储与输出由扩展实现。
//!
//! - [`Tracks`]：内存中的曲目（`Storage`），可选不可定位（引擎改为顺序解码）；
//! - [`VirtualI2s`]：虚拟时间下的 I²S 输出（`AudioOutput`）。启动时先填两个缓冲（乒乓），此后每个
//!   缓冲周期到点时，最早的在途缓冲“播完”（记入输出段），再调用一次 `Feeder::fill` 填它（模拟 DMA
//!   完成中断）。停止时在途缓冲丢弃（没有播出）；
//! - [`SharedFeeder`]：主循环与“中断”共享喂数器（`FeederAccess`；MCU 上为临界区）；
//! - [`build`] / [`run`]：按 Embedded Full 配置档组装播放器，并在虚拟时间下运行主循环。
//!
//! 读取不耗虚拟时间，每次 `step` 计 [`STEP_US`]。缓冲全部启动时取得一次（`'static`），与 MCU 上的
//! 静态区相同。

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt;
use std::rc::Rc;

use qy_codec_auto::AutoDecoder;
use qy_codec_auto::profile::{EMBEDDED_FULL, Profile};
use qy_core::engine::{Engine, EngineConfig, ErrorPolicy, ReusePolicy};
use qy_core::{Core, DeviceFormat, PCM_WORK_SAMPLES};
use qy_host_embedded::{
    AudioOutput, BudgetError, Demand, Feeder, FeederAccess, Load, Notice, OutputCaps, Player,
    SlotFormat, Step, Storage,
};
use qy_pcm_format::static_queue::StaticFrameQueue;
use qy_pcm_format::{Endian, Placement};

use crate::{ExtDecoder, Qyt1Decoder, leak};

/// 配置档：Embedded Full（≤ 96 kHz、双声道、≤ 24 位）。
pub const PROFILE: Profile = EMBEDDED_FULL;
/// 静态帧队列字节数。
pub const QN: usize = EMBEDDED_FULL.budget().queue;
/// 主循环每步至多读取的字节数。
pub const READ_CHUNK: u32 = 512;
/// 每次 `step` 的虚拟 CPU 耗时（µs）。
pub const STEP_US: u64 = 20;

const RATES: [u32; 4] = [44_100, 48_000, 88_200, 96_000];
const SLOTS: [SlotFormat; 2] = [
    SlotFormat {
        container_bits: 16,
        valid_bits: 16,
        endian: Endian::Little,
        placement: Placement::MsbAligned,
    },
    SlotFormat {
        container_bits: 32,
        valid_bits: 24,
        endian: Endian::Little,
        placement: Placement::MsbAligned,
    },
];
const CHANNELS: [u16; 2] = [1, 2];

/// 板级输出能力：常见 I²S 时钟能精确产生的采样率，16/16 与 32/24 槽位，乒乓双缓冲。
pub const CAPS: OutputCaps = OutputCaps {
    rates: &RATES,
    slots: &SLOTS,
    channels: &CHANNELS,
    buffer_frames: 256,
    in_flight: 2,
};

// ---- 存储 ----

/// 内存中的曲目（`Storage`）。
pub struct Tracks {
    items: Vec<(String, Vec<u8>)>,
    seekable: bool,
    /// 打开的曲目与读取位置。
    open: Option<(usize, usize)>,
}

/// 曲目不存在或没有打开。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotOpen;

impl Tracks {
    pub fn new(items: Vec<(String, Vec<u8>)>, seekable: bool) -> Self {
        Self {
            items,
            seekable,
            open: None,
        }
    }
}

impl Storage for Tracks {
    type Error = NotOpen;

    fn track_count(&self) -> u32 {
        u32::try_from(self.items.len()).unwrap_or(u32::MAX)
    }

    fn track_name(&self, track: u32, out: &mut dyn fmt::Write) -> fmt::Result {
        match self.items.get(track as usize) {
            Some((name, _)) => out.write_str(name),
            None => Ok(()),
        }
    }

    fn open(&mut self, track: u32) -> Result<(), NotOpen> {
        let t = track as usize;
        self.open = (t < self.items.len()).then_some((t, 0));
        self.open.map(|_| ()).ok_or(NotOpen)
    }

    /// 返回 0 表示文件结束。
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, NotOpen> {
        let (t, pos) = self.open.ok_or(NotOpen)?;
        let data = self
            .items
            .get(t)
            .map(|(_, b)| b.as_slice())
            .ok_or(NotOpen)?;
        let rest = data.get(pos..).unwrap_or(&[]);
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        self.open = Some((t, pos + n));
        Ok(n)
    }

    /// 不可定位或越过文件末尾时返回 `false`：引擎改为顺序解码到目标位置。
    fn seek(&mut self, offset: u64) -> bool {
        let Some((t, _)) = self.open else {
            return false;
        };
        let len = self.items.get(t).map_or(0, |(_, b)| b.len());
        match usize::try_from(offset) {
            Ok(o) if self.seekable && o <= len => {
                self.open = Some((t, o));
                true
            }
            _ => false,
        }
    }

    fn close(&mut self) {
        self.open = None;
    }

    fn rescan(&mut self) -> Result<u32, NotOpen> {
        Ok(self.track_count())
    }
}

// ---- 输出（虚拟时间） ----

/// 一个输出段（一次设备配置）：播出的有效数据与静音帧数。
#[derive(Debug, Clone)]
pub struct Segment {
    pub format: DeviceFormat,
    pub data: Vec<u8>,
    pub silence_frames: u64,
}

struct Dma {
    next: u64,
    period: u64,
    buf_bytes: usize,
    /// 已填好、尚未播完的缓冲：（有效数据，静音帧数）。
    in_flight: VecDeque<(Vec<u8>, u64)>,
}

/// 虚拟世界：时钟、DMA、喂数器与输出段。
pub struct World {
    pub now_us: u64,
    dma: Option<Dma>,
    format: Option<DeviceFormat>,
    feeder: Option<Feeder<QN>>,
    pub segments: Vec<Segment>,
}

impl World {
    fn fill_one(&mut self) {
        let (Some(d), Some(f)) = (self.dma.as_mut(), self.feeder.as_mut()) else {
            return;
        };
        let mut buf = vec![0u8; d.buf_bytes];
        let fill = f.fill(&mut buf);
        let fb = self.format.map_or(1, |f| usize::from(f.block_align()));
        buf.truncate(fill.data_frames * fb);
        d.in_flight.push_back((buf, fill.silence_frames as u64));
    }

    /// DMA 完成中断：最早的在途缓冲播完，再填一个。
    fn isr(&mut self) {
        let played = self.dma.as_mut().and_then(|d| d.in_flight.pop_front());
        if let (Some((data, silence)), Some(seg)) = (played, self.segments.last_mut()) {
            seg.data.extend_from_slice(&data);
            seg.silence_frames += silence;
        }
        self.fill_one();
    }

    /// 推进到 `t`，途中到点的中断依次执行。
    pub fn advance_to(&mut self, t: u64) {
        while let Some(next) = self.dma.as_ref().map(|d| d.next) {
            if next > t {
                break;
            }
            self.now_us = next;
            self.isr();
            if let Some(d) = self.dma.as_mut() {
                d.next += d.period;
            }
        }
        self.now_us = self.now_us.max(t);
    }

    /// 已播出的有效字节数（全部输出段）。
    pub fn played_bytes(&self) -> usize {
        self.segments.iter().map(|s| s.data.len()).sum()
    }
}

pub type Shared = Rc<RefCell<World>>;

/// 主循环钩子看到的快照（钩子里可以下发命令，命令会访问喂数器，因此不借出 [`World`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub now_us: u64,
    /// 已播出的有效字节数（全部输出段）。
    pub played_bytes: usize,
}

/// 虚拟时间下的 I²S 输出。
pub struct VirtualI2s(pub Shared);

impl AudioOutput for VirtualI2s {
    type Error = ();

    fn caps(&self) -> OutputCaps {
        CAPS
    }

    fn configure(&mut self, format: &DeviceFormat) -> Result<(), ()> {
        let mut w = self.0.borrow_mut();
        w.dma = None;
        w.format = Some(*format);
        w.segments.push(Segment {
            format: *format,
            data: Vec::new(),
            silence_frames: 0,
        });
        Ok(())
    }

    fn start(&mut self) -> Result<(), ()> {
        let mut w = self.0.borrow_mut();
        let f = w.format.ok_or(())?;
        let frames = u64::from(CAPS.buffer_frames);
        let period = (frames * 1_000_000).div_ceil(u64::from(f.sample_rate()));
        let next = w.now_us + period;
        w.dma = Some(Dma {
            next,
            period,
            buf_bytes: frames as usize * usize::from(f.block_align()),
            in_flight: VecDeque::new(),
        });
        w.fill_one();
        w.fill_one();
        Ok(())
    }

    fn stop(&mut self) {
        self.0.borrow_mut().dma = None;
    }
}

/// 主循环与“中断”共享的喂数器。
pub struct SharedFeeder(pub Shared);

impl FeederAccess<QN> for SharedFeeder {
    fn with<R>(&self, f: impl FnOnce(&mut Feeder<QN>) -> R) -> Option<R> {
        self.0.borrow_mut().feeder.as_mut().map(f)
    }
}

pub type EmbeddedPlayer = Player<ExtDecoder, Tracks, VirtualI2s, SharedFeeder, QN>;

// ---- 组装 ----

/// 嵌入式组合解码器：WAV、AIFF、FLAC（MCU 容量）与 QYT1，工作区按配置档一次性取得。
pub fn decoder(qyt1_seekable: bool) -> ExtDecoder {
    decoder_with(if qyt1_seekable { Some(0) } else { None })
}

/// 同 [`decoder`]；`qyt1`：`None` 不可定位，`Some(0)` 按帧直接定位，`Some(n)` 包模式（E5，每 `n`
/// 帧一包，定位需要预滚）。
pub fn decoder_with(qyt1: Option<u64>) -> ExtDecoder {
    let budget = PROFILE.budget();
    let Some(flac) = PROFILE.flac_capacity() else {
        panic!("Embedded Full 应启用 FLAC");
    };
    let auto = AutoDecoder::empty()
        .with_wav(leak(PCM_WORK_SAMPLES))
        .and_then(AutoDecoder::with_aiff)
        .and_then(|d| {
            d.with_flac(
                flac,
                leak(budget.flac_workspace / 8),
                leak(budget.flac_pcm / 4),
            )
        });
    let auto = match auto {
        Ok(a) => a,
        Err(e) => panic!("AutoDecoder 工作区：{e:?}"),
    };
    // QYT1 与 WAV 共用同样大小的 PCM 工作区。
    let pcm = leak(PCM_WORK_SAMPLES);
    let qyt1 = match qyt1 {
        None => Qyt1Decoder::without_seek(pcm),
        Some(0) => Qyt1Decoder::new(pcm),
        Some(n) => Qyt1Decoder::with_packets(pcm, n),
    };
    match qyt1 {
        Ok(q) => ExtDecoder::new(q, auto),
        Err(e) => panic!("QYT1 工作区：{e:?}"),
    }
}

/// 板级时序声明（µs）：读取与解码一个最大输入单元的最长耗时 `U` / `D`，打开曲目 `O`。这里读取
/// 不耗虚拟时间，取宽松而仍能通过预算核对的值；真实板子按格式实测（见 `qy_host_embedded::budget`）。
const OPEN_US: u32 = 1_000;

/// 启用的每种格式的需求与时序。QYT1 没有现成的 `Demand` 构造：按公开字段自己声明——每样本至多
/// 4 字节，输入单元为一次读取（`READ_CHUNK` 字节）。
pub fn loads() -> [Load; 3] {
    let caps = PROFILE.caps;
    let Some(flac) = PROFILE.flac_capacity() else {
        panic!("Embedded Full 应启用 FLAC");
    };
    let small = qy_codec_flac::Capacity::for_limits(16, 2, 24).input_window as u32;
    let frame = u64::from(caps.max_channels) * 4;
    let load = |demand, unit_read_us, decode_us| Load {
        demand,
        unit_read_us,
        decode_us,
        open_us: OPEN_US,
    };
    [
        load(Demand::wav(&caps, 16_384), 2_000, 2_000),
        load(
            Demand::flac(
                &caps,
                flac.input_window as u32,
                flac.max_block as u32,
                small,
            ),
            4_000,
            8_000,
        ),
        load(
            Demand {
                bytes_per_sec: u64::from(caps.max_rate) * frame,
                unit_bytes: READ_CHUNK,
                unit_frames: (u64::from(READ_CHUNK) / frame) as u32,
            },
            100,
            100,
        ),
    ]
}

/// 组装播放器：静态帧队列、核心、引擎、存储、输出与喂数器。
pub fn build(
    tracks: Tracks,
    qyt1_seekable: bool,
    policy: ErrorPolicy,
) -> Result<(EmbeddedPlayer, Shared), BudgetError> {
    build_with(tracks, decoder(qyt1_seekable), policy)
}

/// 同 [`build`]，解码器由调用方给出（如 [`decoder_with`] 的包模式）。
pub fn build_with(
    tracks: Tracks,
    decoder: ExtDecoder,
    policy: ErrorPolicy,
) -> Result<(EmbeddedPlayer, Shared), BudgetError> {
    let world = Rc::new(RefCell::new(World {
        now_us: 0,
        dma: None,
        format: None,
        feeder: None,
        segments: Vec::new(),
    }));
    let queue: &'static StaticFrameQueue<QN> = Box::leak(Box::new(StaticFrameQueue::new()));
    let Some((producer, consumer)) = queue.split() else {
        panic!("静态队列只能拆分一次");
    };
    world.borrow_mut().feeder = Some(Feeder::new(consumer, CAPS.in_flight));
    let window = decoder.required_window();
    let core = match Core::with_window(leak(PROFILE.budget().input_window), window, decoder) {
        Ok(c) => c,
        Err(e) => panic!("输入窗口：{e:?}"),
    };
    let engine = Engine::new(
        core,
        producer,
        &mut [],
        EngineConfig {
            caps: PROFILE.caps,
            reuse: ReusePolicy::Accepts,
            on_error: policy,
        },
    );
    let player = Player::new(
        engine,
        tracks,
        VirtualI2s(world.clone()),
        SharedFeeder(world.clone()),
        READ_CHUNK,
        &loads(),
    )?;
    Ok((player, world))
}

/// 运行主循环，直到播放结束（DMA 停止且不在播放）或虚拟时间超过 `limit_us`（返回 `Err`）。
/// `hook` 在每步之前调用，可下发控制命令。返回收到的通知。
pub fn run(
    p: &mut EmbeddedPlayer,
    w: &Shared,
    limit_us: u64,
    mut hook: impl FnMut(&mut EmbeddedPlayer, Snapshot),
) -> Result<Vec<Notice>, Vec<Notice>> {
    let mut notices = Vec::new();
    loop {
        let snap = {
            let w = w.borrow();
            Snapshot {
                now_us: w.now_us,
                played_bytes: w.played_bytes(),
            }
        };
        hook(p, snap);
        let step = p.step();
        while let Some(n) = p.pop_notice() {
            notices.push(n);
        }
        if w.borrow().now_us > limit_us {
            return Err(notices);
        }
        match step {
            Step::Busy => {
                let t = w.borrow().now_us + STEP_US;
                w.borrow_mut().advance_to(t);
            }
            Step::Idle => {
                let next = w.borrow().dma.as_ref().map(|d| d.next);
                match next {
                    Some(t) => w.borrow_mut().advance_to(t),
                    None if !p.status().active => return Ok(notices),
                    None => {
                        let t = w.borrow().now_us + 1_000;
                        w.borrow_mut().advance_to(t);
                    }
                }
            }
        }
    }
}
