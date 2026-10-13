//! QyBit MCU 资源演示（M3 T1）。
//!
//! thumbv7em（Cortex-M4F/M7）上的 `no_std` 纯软件宿主，与桌面/模拟宿主共用同一个播放引擎
//! （`qy_core::engine`）、自有解码器与 pack，只把 I/O 换成：
//! - **“SD 卡”**：编译进闪存的测试夹具，每次读取至多 512 字节（一个扇区）；
//! - **“I²S DMA”**：每个 DMA 半缓冲周期从静态帧队列（`bbqueue`）取出至多 256 帧并累加校验和，
//!   只做有界搬运；设备格式为 I²S 32 位槽位：N ≤ 16 用 16/16，否则 32/24（左对齐）。
//!
//! 所有大缓冲都是静态分配（`ConstStaticCell` 与静态队列，位于 `.bss`），容量由配置档按公式算出：
//! 默认 `full` 特性为 Embedded Full（WAV + FLAC，≤ 96 kHz / 24 位），关闭后为 Embedded Lite
//! （只链接 WAV，≤ 48 kHz / 16 位）。播放中按脚本暂停/恢复一次，出错的曲目按策略跳过。
//!
//! 结果经半主机打印：每个设备段的格式、帧数与 FNV-1a 64 校验和，跳过的曲目及原因，统计与预算。
//! 宿主测试 `qy-host-sim/tests/mcu_demo.rs` 在 QEMU 中运行本程序，按独立参考核对校验和，
//! 并读取内存统计栈用量。这是指令集仿真，不是实机运行。
#![no_std]
#![no_main]

use cortex_m_rt::entry;
use cortex_m_semihosting::{debug, hprintln};
use static_cell::ConstStaticCell;

use qy_codec_auto::AutoDecoder;
use qy_codec_auto::profile::{Budget, Profile};
use qy_core::engine::{
    Engine, EngineConfig, EngineFault, ErrorPolicy, FrameSource, HostAction, HostEvent, Poll,
    ReusePolicy, Wait,
};
use qy_core::{Core, DeviceFormat, PCM_WORK_SAMPLES, SourceFormat};
use qy_pcm_format::static_queue::{StaticConsumer, StaticFrameQueue, StaticProducer, reconfigure};
use qy_pcm_format::{Endian, Placement, frames_for_millis};

#[cfg(feature = "full")]
const PROFILE: Profile = qy_codec_auto::profile::EMBEDDED_FULL;
#[cfg(not(feature = "full"))]
const PROFILE: Profile = qy_codec_auto::profile::EMBEDDED_LITE;
const BUDGET: Budget = PROFILE.budget();

const QUEUE_BYTES: usize = BUDGET.queue;
const WINDOW: usize = BUDGET.input_window;
/// “SD 卡”每次读取的最大字节数。
const SECTOR: usize = 512;
/// 每个 DMA 半缓冲的帧数。
const DMA_FRAMES: usize = 256;
/// 本机最大设备帧字节数：配置档最多声道 × C32（F1：缓冲按配置档推出，不按布局上限 18 声道）。
const FRAME_BYTES: usize = PROFILE.caps.max_channels as usize * 4;
/// 串口命令脚本：在第几个时钟周期（一个 DMA 半缓冲时长）暂停、恢复（Lite 的播放列表很短，
/// 提前暂停）。
#[cfg(feature = "full")]
const PAUSE_AT: u32 = 100;
#[cfg(feature = "full")]
const RESUME_AT: u32 = 150;
#[cfg(not(feature = "full"))]
const PAUSE_AT: u32 = 2;
#[cfg(not(feature = "full"))]
const RESUME_AT: u32 = 4;

static QUEUE: StaticFrameQueue<QUEUE_BYTES> = StaticFrameQueue::new();
static INPUT: ConstStaticCell<[u8; WINDOW]> = ConstStaticCell::new([0; WINDOW]);
static WAV_PCM: ConstStaticCell<[i32; PCM_WORK_SAMPLES]> =
    ConstStaticCell::new([0; PCM_WORK_SAMPLES]);
static DMA_BUF: ConstStaticCell<[u8; DMA_FRAMES * FRAME_BYTES]> =
    ConstStaticCell::new([0; DMA_FRAMES * FRAME_BYTES]);

#[cfg(feature = "full")]
mod flac_buffers {
    use super::{BUDGET, ConstStaticCell};
    pub const WS: usize = BUDGET.flac_workspace / 8;
    pub const PCM: usize = BUDGET.flac_pcm / 4;
    pub static WORKSPACE: ConstStaticCell<[i64; WS]> = ConstStaticCell::new([0; WS]);
    pub static PCM_BUF: ConstStaticCell<[i32; PCM]> = ConstStaticCell::new([0; PCM]);
}

macro_rules! card {
    ($($dir:literal / $name:literal),* $(,)?) => {
        &[$(($name, include_bytes!(concat!("../../", $dir, "/tests/fixtures/", $name)))),*]
    };
}

/// “SD 卡”上的播放列表（文件名，内容）。
#[cfg(feature = "full")]
static CARD: &[(&str, &[u8])] = card![
    "qy-codec-wav" / "pcm16_stereo_44k.wav",
    "qy-codec-flac" / "cli_16_stereo_44k_default.flac",
    "qy-codec-flac" / "err_frame_crc.flac",
    "qy-codec-wav" / "ext24in32_6ch_51.wav",
    "qy-codec-flac" / "cli_16_6ch_48k_51_side.flac",
    "qy-codec-flac" / "cli_32_stereo_192k.flac",
    "qy-codec-flac" / "cli_24_mono_96k_lpc12.flac",
    "qy-codec-flac" / "fixed_orders_and_lpc32.flac",
    "qy-codec-wav" / "pcm24_mono_48k_odd_list.wav",
];
#[cfg(not(feature = "full"))]
static CARD: &[(&str, &[u8])] = card![
    "qy-codec-wav" / "pcm16_stereo_44k.wav",
    "qy-codec-flac" / "cli_16_stereo_44k_default.flac",
    "qy-codec-wav" / "ext16_6ch_51_side.wav",
    "qy-codec-wav" / "pcm24_mono_48k_odd_list.wav",
    "qy-codec-wav" / "pcm32_stereo_96k.wav",
    "qy-codec-wav" / "pcm16_mono_8k_fmt18.wav",
];

/// FNV-1a 64。
#[derive(Clone, Copy)]
struct Fnv(u64);

impl Fnv {
    const fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

/// 一次设备打开期间的输出。
#[derive(Clone, Copy)]
struct Segment {
    format: DeviceFormat,
    frames: u64,
    fnv: Fnv,
}

const MAX_SEGMENTS: usize = 8;
const MAX_SKIPPED: usize = 8;

struct Host {
    engine: Engine<AutoDecoder, StaticProducer<QUEUE_BYTES>>,
    consumer: StaticConsumer<QUEUE_BYTES>,
    dma: &'static mut [u8; DMA_FRAMES * FRAME_BYTES],
    open: Option<usize>,
    cursor: usize,
    ring_generation: u64,
    /// 设备：格式、是否在取数、是否暂停。
    device: Option<(DeviceFormat, bool, bool)>,
    segments: [Option<Segment>; MAX_SEGMENTS],
    seg_len: usize,
    skipped: [Option<(u32, EngineFault)>; MAX_SKIPPED],
    skip_len: usize,
    failed: Option<(u32, EngineFault)>,
    finished: bool,
    /// 时钟：每个 DMA 半缓冲周期（含设备空闲等待）加一。
    clock: u32,
    dma_periods: u32,
    pause_periods: u32,
    underruns: u32,
    reads: u32,
    errors: u32,
}

/// I²S 设备格式：N ≤ 16 用 16/16，否则 32 位槽位、24 位有效（左对齐）。
fn device_format(src: &SourceFormat) -> Option<DeviceFormat> {
    let (c, v) = if src.bits() <= 16 { (16, 16) } else { (32, 24) };
    DeviceFormat::new(
        src.sample_rate(),
        src.channels(),
        c,
        v,
        Endian::Little,
        Placement::MsbAligned,
    )
    .ok()
    .filter(|d| d.accepts(src))
}

impl Host {
    fn handle(&mut self, e: HostEvent) {
        if self.engine.handle(e).is_err() {
            self.errors += 1;
        }
    }

    fn execute(&mut self, a: HostAction) {
        match a {
            HostAction::Open { generation, track } => {
                if (track as usize) < CARD.len() {
                    self.open = Some(track as usize);
                    self.cursor = 0;
                    self.handle(HostEvent::Opened { generation });
                } else {
                    self.handle(HostEvent::OpenFailed { generation });
                }
            }
            HostAction::Read {
                generation,
                request,
                max,
            } => self.read(generation, request, max),
            HostAction::Close { .. } => self.open = None,
            HostAction::SelectDevice { .. } => {
                self.errors += 1;
                self.handle(HostEvent::DeviceUnsupported);
            }
            HostAction::OpenDevice { source, .. } => {
                let event = match self.open_device(&source) {
                    Some(format) => HostEvent::DeviceOpened { format },
                    None => HostEvent::DeviceOpenFailed,
                };
                self.handle(event);
            }
            HostAction::StartDevice => {
                if let Some(d) = self.device.as_mut() {
                    d.1 = true;
                }
            }
            HostAction::DrainDevice => {
                if let Some(d) = self.device.as_mut() {
                    d.1 = true;
                }
                // 有界：队列中的帧数有限，每个周期至少取走一帧（暂停时按脚本恢复）。
                for _ in 0..QUEUE_BYTES {
                    if self.consumer.available_frames() == 0 {
                        break;
                    }
                    self.dma_period(true);
                }
                self.handle(HostEvent::DeviceDrained);
            }
            HostAction::StopDevice => self.device = None,
            HostAction::PauseDevice => {
                if let Some(d) = self.device.as_mut() {
                    d.2 = true;
                }
            }
            HostAction::ResumeDevice => {
                if let Some(d) = self.device.as_mut() {
                    d.2 = false;
                }
            }
            HostAction::TrackSkipped { track, fault } => {
                if let Some(slot) = self.skipped.get_mut(self.skip_len) {
                    *slot = Some((track, fault));
                    self.skip_len += 1;
                }
            }
            HostAction::Finished => self.finished = true,
            HostAction::Failed { track, fault } => self.failed = Some((track, fault)),
        }
    }

    fn read(&mut self, generation: qy_core::Generation, request: u32, max: usize) {
        self.reads += 1;
        let file = self.open.and_then(|i| CARD.get(i)).map(|f| f.1);
        let (Some(file), Some(dst)) = (file, self.engine.read_target(generation, request)) else {
            self.errors += 1;
            self.handle(HostEvent::ReadFailed {
                generation,
                request,
            });
            return;
        };
        let rest = file.get(self.cursor..).unwrap_or(&[]);
        let n = rest.len().min(max).min(SECTOR).min(dst.len());
        if n == 0 {
            self.handle(HostEvent::ReadEof {
                generation,
                request,
            });
            return;
        }
        dst[..n].copy_from_slice(&rest[..n]);
        self.cursor += n;
        self.handle(HostEvent::ReadDone {
            generation,
            request,
            len: n,
        });
    }

    fn open_device(&mut self, source: &SourceFormat) -> Option<DeviceFormat> {
        let format = device_format(source)?;
        let frames = usize::try_from(frames_for_millis(
            format.sample_rate(),
            PROFILE.caps.prefill_ms,
        ))
        .ok()?;
        let required = frames.checked_mul(usize::from(format.block_align()))?;
        self.ring_generation += 1;
        reconfigure(
            self.engine.sink_mut(),
            &mut self.consumer,
            &format,
            required,
            self.ring_generation,
        )
        .ok()?;
        let slot = self.segments.get_mut(self.seg_len)?;
        *slot = Some(Segment {
            format,
            frames: 0,
            fnv: Fnv::new(),
        });
        self.seg_len += 1;
        self.device = Some((format, false, false));
        Some(format)
    }

    /// 推进一个时钟周期并执行到期的串口命令。
    fn tick(&mut self) {
        self.clock += 1;
        if self.clock == PAUSE_AT {
            self.engine.pause();
        } else if self.clock == RESUME_AT {
            self.engine.resume();
        }
        while let Some(a) = self.engine.take_control() {
            self.execute(a);
        }
    }

    /// 一个 DMA 半缓冲周期：至多取出 `DMA_FRAMES` 帧（有界拷贝）并计入当前段。
    fn dma_period(&mut self, draining: bool) {
        self.tick();
        let Some((format, running, paused)) = self.device else {
            return;
        };
        if !running {
            return;
        }
        self.dma_periods += 1;
        if paused {
            // 暂停：输出静音，单独计数，不算欠载。
            self.pause_periods += 1;
            return;
        }
        let fb = usize::from(format.block_align());
        let n = match self.consumer.pop_frames(&mut self.dma[..DMA_FRAMES * fb]) {
            Ok(n) => n,
            Err(_) => {
                self.errors += 1;
                0
            }
        };
        if n < DMA_FRAMES && !draining {
            self.underruns += 1;
        }
        if let Some(Some(seg)) = self
            .seg_len
            .checked_sub(1)
            .and_then(|i| self.segments.get_mut(i))
        {
            seg.frames += n as u64;
            seg.fnv.feed(&self.dma[..n * fb]);
        }
    }

    fn run(&mut self) {
        // 有界：每次循环要么推进核心，要么让设备取走数据；超出即视为错误。
        for _ in 0..50_000_000u32 {
            match self.engine.poll() {
                Poll::Action(a) => self.execute(a),
                Poll::Yield => {}
                Poll::Wait(Wait::QueueFull) => self.dma_period(false),
                // 暂停中、设备尚未启动（等待恢复后排空）：时间照常流逝。
                Poll::Wait(Wait::Device) if self.engine.is_paused() => self.tick(),
                Poll::Wait(_) => {
                    self.errors += 1;
                    return;
                }
                Poll::Idle => return,
            }
        }
        self.errors += 1;
    }
}

fn setup() -> Option<Host> {
    let wav = AutoDecoder::empty().with_wav(WAV_PCM.try_take()?).ok()?;
    #[cfg(feature = "full")]
    let wav = wav
        .with_flac(
            PROFILE.flac_capacity()?,
            flac_buffers::WORKSPACE.try_take()?,
            flac_buffers::PCM_BUF.try_take()?,
        )
        .ok()?;
    let decoder = wav;
    let input = INPUT.try_take()?;
    let window = decoder.required_window();
    let core = Core::with_window(input, window, decoder).ok()?;
    let (producer, consumer) = QUEUE.split()?;
    let config = EngineConfig {
        caps: PROFILE.caps,
        reuse: ReusePolicy::Accepts,
        on_error: ErrorPolicy::Skip,
    };
    // 静态队列直接 pack 进队列存储，不需要 pack 暂存区。
    let engine = Engine::new(core, producer, &mut [], config);
    Some(Host {
        engine,
        consumer,
        dma: DMA_BUF.try_take()?,
        open: None,
        cursor: 0,
        ring_generation: 0,
        device: None,
        segments: [None; MAX_SEGMENTS],
        seg_len: 0,
        skipped: [None; MAX_SKIPPED],
        skip_len: 0,
        failed: None,
        finished: false,
        clock: 0,
        dma_periods: 0,
        pause_periods: 0,
        underruns: 0,
        reads: 0,
        errors: 0,
    })
}

fn report(h: &Host) -> u8 {
    for (i, (name, data)) in CARD.iter().enumerate() {
        hprintln!("QYBIT-TRACK i={} bytes={} name={}", i, data.len(), name);
    }
    for (i, s) in h.segments.iter().flatten().enumerate() {
        let f = s.format;
        hprintln!(
            "QYBIT-SEG i={} rate={} ch={} c={} v={} frames={} fnv={:016x}",
            i,
            f.sample_rate(),
            f.channels(),
            f.container_bits(),
            f.valid_bits(),
            s.frames,
            s.fnv.0
        );
    }
    for (track, fault) in h.skipped.iter().flatten() {
        hprintln!("QYBIT-SKIP track={} fault={:?}", track, fault);
    }
    if let Some((track, fault)) = h.failed {
        hprintln!("QYBIT-FAIL track={} fault={:?}", track, fault);
    }
    let st = h.engine.stats();
    hprintln!(
        "QYBIT-STAT finished={} underruns={} pause_periods={} dma_periods={} reads={} errors={} stale={} protocol={} pushed={}",
        u8::from(h.finished),
        h.underruns,
        h.pause_periods,
        h.dma_periods,
        h.reads,
        h.errors,
        st.stale,
        st.protocol_errors,
        st.frames_pushed
    );
    let b = BUDGET;
    hprintln!(
        "QYBIT-BUDGET queue={} input={} wav_pcm={} flac_ws={} flac_pcm={} pack={} total={}",
        b.queue,
        b.input_window,
        b.wav_pcm,
        b.flac_workspace,
        b.flac_pcm,
        b.pack_scratch,
        b.total
    );
    let ok = h.finished && h.errors == 0 && h.underruns == 0 && st.protocol_errors == 0;
    u8::from(!ok)
}

#[entry]
fn main() -> ! {
    let stack_top = cortex_m::register::msp::read();
    let heap_start = cortex_m_rt::heap_start() as usize;
    hprintln!(
        "QYBIT-MCU profile={} stack_top=0x{:08x} heap_start=0x{:08x}",
        PROFILE.name,
        stack_top,
        heap_start
    );
    let code = match setup() {
        Some(mut h) => {
            h.engine.play(0, CARD.len() as u32);
            h.run();
            report(&h)
        }
        None => {
            hprintln!("QYBIT-ERROR 初始化失败");
            2
        }
    };
    hprintln!("QYBIT-DONE code={}", code);
    if cfg!(feature = "hold") {
        loop {
            cortex_m::asm::wfi();
        }
    }
    debug::exit(if code == 0 {
        debug::EXIT_SUCCESS
    } else {
        debug::EXIT_FAILURE
    });
    loop {
        cortex_m::asm::wfi();
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    hprintln!("QYBIT-PANIC {}", info);
    debug::exit(debug::EXIT_FAILURE);
    loop {
        cortex_m::asm::wfi();
    }
}
