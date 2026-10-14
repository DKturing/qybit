//! QyBit MCU 演示（M3 T1/T2）。
//!
//! thumbv7em（Cortex-M4F/M7）上运行通用嵌入式宿主 `qy-host-embedded`，与桌面/模拟宿主共用同一个
//! 播放引擎（`qy_core::engine`）、自有解码器与 pack。本程序就是一个“板级适配”，只是外设是软件模拟的：
//! - **SD 卡**：构建时生成的 FAT16 镜像（MBR + 分区，`build.rs`），编进闪存，实现
//!   `embedded_sdmmc::BlockDevice`；播放列表由 `embedded-sdmmc` 扫描根目录得到；
//! - **I²S DMA**：主循环空闲时推进一个 DMA 缓冲周期，在临界区里调用 `Feeder::fill`（与真实板子
//!   在 DMA 完成中断里调用的是同一个函数），对每个缓冲的有效数据累加校验和；
//!   设备格式为 I²S 32 位槽位：N ≤ 16 用 16/16，否则 32/24（左对齐）；
//! - **串口**：按时钟周期把命令脚本（`ls`、`play`、`pause`、`resume`、`status`）逐字节送入行缓冲，
//!   回应经半主机打印。
//!
//! 所有大缓冲都是静态分配（`ConstStaticCell` 与静态队列，位于 `.bss`），容量由配置档按公式算出：
//! 默认 `full` 特性为 Embedded Full（WAV + FLAC，≤ 96 kHz / 24 位），关闭后为 Embedded Lite
//! （只链接 WAV，≤ 48 kHz / 16 位）。
//!
//! 结果经半主机打印：每个设备段的格式、帧数与 FNV-1a 64 校验和，跳过的曲目及原因，统计与预算。
//! 宿主测试 `qy-host-sim/tests/mcu_demo.rs` 在 QEMU 中运行本程序，按独立参考核对校验和，
//! 并读取内存统计栈用量。这是指令集仿真，不是实机运行；读卡与解码时序是名义值，不是实测。
#![no_std]
#![no_main]

use core::cell::{Cell, RefCell};
use core::fmt;

use cortex_m::interrupt::{self, Mutex};
use cortex_m_rt::entry;
use cortex_m_semihosting::{debug, hprint, hprintln};
use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};
use static_cell::ConstStaticCell;

use qy_codec_auto::AutoDecoder;
use qy_codec_auto::profile::{Budget, Profile};
use qy_core::engine::{Engine, EngineConfig, ErrorPolicy, ReusePolicy};
use qy_core::{Core, DeviceFormat, PCM_WORK_SAMPLES};
use qy_host_embedded::budget::{Demand, Load};
use qy_host_embedded::output::{AudioOutput, OutputCaps, SlotFormat};
use qy_host_embedded::player::{Notice, Player, Step, write_parse_error, write_status};
use qy_host_embedded::{FatStorage, Feeder, FeederAccess, LineBuffer, NoClock};
use qy_pcm_format::static_queue::StaticFrameQueue;
use qy_pcm_format::{Endian, Placement};

#[cfg(feature = "full")]
const PROFILE: Profile = qy_codec_auto::profile::EMBEDDED_FULL;
#[cfg(not(feature = "full"))]
const PROFILE: Profile = qy_codec_auto::profile::EMBEDDED_LITE;
const BUDGET: Budget = PROFILE.budget();

const QUEUE_BYTES: usize = BUDGET.queue;
const WINDOW: usize = BUDGET.input_window;
/// 主循环每步至多读取的字节数（一个扇区）。
const READ_CHUNK: u32 = 512;
/// 每个 DMA 缓冲的帧数。
const DMA_FRAMES: usize = 256;
/// 本机最大设备帧字节数：配置档最多声道 × C32。
const FRAME_BYTES: usize = PROFILE.caps.max_channels as usize * 4;
/// 播放列表上限。
const MAX_TRACKS: usize = 16;

/// 串口命令脚本：（时钟周期，命令行）。时钟为 DMA 缓冲周期（含设备空闲时的等待）。
#[cfg(feature = "full")]
const SCRIPT: [(u32, &[u8]); 4] = [
    (0, b"ls\r\n"),
    (0, b"play\r\n"),
    (100, b"pause\r\n"),
    (150, b"resume\r\n"),
];
#[cfg(not(feature = "full"))]
const SCRIPT: [(u32, &[u8]); 4] = [
    (0, b"ls\r\n"),
    (0, b"play\r\n"),
    (2, b"pause\r\n"),
    (4, b"resume\r\n"),
];

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

/// “SD 卡”镜像（构建时生成，见 `build.rs`）。
static CARD: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/card.img"));

// ---- 板级：块设备 ----

/// 闪存中的“SD 卡”。
struct FlashCard {
    reads: Cell<u32>,
}

#[derive(Debug)]
struct CardError;

impl fmt::Display for CardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("块越界")
    }
}

impl core::error::Error for CardError {}

impl BlockDevice for FlashCard {
    type Error = CardError;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), CardError> {
        for (i, b) in blocks.iter_mut().enumerate() {
            let off = (start.0 as usize + i) * Block::LEN;
            let src = CARD.get(off..off + Block::LEN).ok_or(CardError)?;
            b.contents.copy_from_slice(src);
            self.reads.set(self.reads.get().wrapping_add(1));
        }
        Ok(())
    }

    fn write(&self, _: &[Block], _: BlockIdx) -> Result<(), CardError> {
        Err(CardError)
    }

    fn num_blocks(&self) -> Result<BlockCount, CardError> {
        Ok(BlockCount((CARD.len() / Block::LEN) as u32))
    }
}

// ---- 板级：I²S 输出（软件模拟 DMA）与共享喂数器 ----

static RATES: [u32; 9] = [
    8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000,
];
static SLOTS: [SlotFormat; 2] = [
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
static CHANNELS: [u16; 2] = [1, 2];
const CAPS: OutputCaps = OutputCaps {
    rates: &RATES,
    slots: &SLOTS,
    channels: &CHANNELS,
    buffer_frames: DMA_FRAMES as u32,
    in_flight: 2,
};

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

/// 一次设备配置期间的输出。
#[derive(Clone, Copy)]
struct Segment {
    format: DeviceFormat,
    frames: u64,
    fnv: Fnv,
}

const MAX_SEGMENTS: usize = 8;

/// 板级输出状态（主循环与“中断”共享）。
struct Board {
    format: Option<DeviceFormat>,
    running: bool,
    segments: [Option<Segment>; MAX_SEGMENTS],
    seg_len: usize,
    overflow: bool,
}

static BOARD: Mutex<RefCell<Board>> = Mutex::new(RefCell::new(Board {
    format: None,
    running: false,
    segments: [None; MAX_SEGMENTS],
    seg_len: 0,
    overflow: false,
}));
static FEEDER: Mutex<RefCell<Option<Feeder<QUEUE_BYTES>>>> = Mutex::new(RefCell::new(None));

/// 在临界区内访问板级状态。
fn board<R>(f: impl FnOnce(&mut Board) -> R) -> Option<R> {
    interrupt::free(|cs| {
        BOARD
            .borrow(cs)
            .try_borrow_mut()
            .ok()
            .map(|mut b| f(&mut b))
    })
}

struct SharedFeeder;

impl FeederAccess<QUEUE_BYTES> for SharedFeeder {
    fn with<R>(&self, f: impl FnOnce(&mut Feeder<QUEUE_BYTES>) -> R) -> Option<R> {
        interrupt::free(|cs| {
            let mut cell = FEEDER.borrow(cs).try_borrow_mut().ok()?;
            cell.as_mut().map(f)
        })
    }
}

struct DemoOut;

impl AudioOutput for DemoOut {
    type Error = ();

    fn caps(&self) -> OutputCaps {
        CAPS
    }

    fn configure(&mut self, format: &DeviceFormat) -> Result<(), ()> {
        board(|b| {
            b.running = false;
            b.format = Some(*format);
            match b.segments.get_mut(b.seg_len) {
                Some(slot) => {
                    *slot = Some(Segment {
                        format: *format,
                        frames: 0,
                        fnv: Fnv::new(),
                    });
                    b.seg_len += 1;
                    Ok(())
                }
                None => {
                    b.overflow = true;
                    Err(())
                }
            }
        })
        .unwrap_or(Err(()))
    }

    fn start(&mut self) -> Result<(), ()> {
        board(|b| b.running = b.format.is_some()).ok_or(())
    }

    fn stop(&mut self) {
        let _ = board(|b| b.running = false);
    }
}

/// 一个 DMA 缓冲周期（真实板子上由 DMA 完成中断触发）：填缓冲，有效数据计入当前段。
fn dma_period(dma: &mut [u8]) -> bool {
    interrupt::free(|cs| {
        let Ok(mut b) = BOARD.borrow(cs).try_borrow_mut() else {
            return false;
        };
        let (Some(format), true) = (b.format, b.running) else {
            return false;
        };
        let Ok(mut cell) = FEEDER.borrow(cs).try_borrow_mut() else {
            return false;
        };
        let Some(feeder) = cell.as_mut() else {
            return false;
        };
        let fb = usize::from(format.block_align());
        let buf = &mut dma[..DMA_FRAMES * fb];
        let fill = feeder.fill(buf);
        let n = b.seg_len;
        if let Some(Some(seg)) = n.checked_sub(1).and_then(|i| b.segments.get_mut(i)) {
            seg.frames += fill.data_frames as u64;
            seg.fnv.feed(&buf[..fill.data_frames * fb]);
        }
        true
    })
}

// ---- 串口（半主机） ----

struct Serial;

impl fmt::Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        hprint!("{}", s);
        Ok(())
    }
}

// ---- 组装与运行 ----

type DemoPlayer = Player<
    AutoDecoder,
    FatStorage<FlashCard, NoClock, MAX_TRACKS>,
    DemoOut,
    SharedFeeder,
    QUEUE_BYTES,
>;

/// 名义时序（QEMU 不测时序；实物板须按 `qy_host_embedded::budget` 实测后填写）。
fn loads() -> ([Load; 2], usize) {
    let caps = PROFILE.caps;
    let wav = Load {
        demand: Demand::wav(&caps, 16_384),
        unit_read_us: 10_000,
        decode_us: 2_000,
    };
    #[cfg(feature = "full")]
    if let Some(c) = PROFILE.flac_capacity() {
        let small = qy_codec_flac::Capacity::for_limits(
            16,
            caps.max_channels as usize,
            caps.max_bits as usize,
        );
        let flac = Load {
            demand: Demand::flac(
                &caps,
                c.input_window as u32,
                c.max_block as u32,
                small.input_window as u32,
            ),
            unit_read_us: 10_000,
            decode_us: 10_000,
        };
        return ([wav, flac], 2);
    }
    ([wav, wav], 1)
}

enum SetupError {
    Buffers,
    Budget(qy_host_embedded::BudgetError),
}

fn setup() -> Result<DemoPlayer, SetupError> {
    // WAV 与 AIFF 共用流式工作区（F2）。
    let wav = AutoDecoder::empty()
        .with_wav(WAV_PCM.try_take().ok_or(SetupError::Buffers)?)
        .and_then(AutoDecoder::with_aiff)
        .map_err(|_| SetupError::Buffers)?;
    #[cfg(feature = "full")]
    let wav = wav
        .with_flac(
            PROFILE.flac_capacity().ok_or(SetupError::Buffers)?,
            flac_buffers::WORKSPACE
                .try_take()
                .ok_or(SetupError::Buffers)?,
            flac_buffers::PCM_BUF
                .try_take()
                .ok_or(SetupError::Buffers)?,
        )
        .map_err(|_| SetupError::Buffers)?;
    let decoder = wav;
    let input = INPUT.try_take().ok_or(SetupError::Buffers)?;
    let window = decoder.required_window();
    let core = Core::with_window(input, window, decoder).map_err(|_| SetupError::Buffers)?;
    let (producer, consumer) = QUEUE.split().ok_or(SetupError::Buffers)?;
    interrupt::free(|cs| {
        if let Ok(mut f) = FEEDER.borrow(cs).try_borrow_mut() {
            *f = Some(Feeder::new(consumer, CAPS.in_flight));
        }
    });
    let config = EngineConfig {
        caps: PROFILE.caps,
        reuse: ReusePolicy::Accepts,
        on_error: ErrorPolicy::Skip,
    };
    // 静态队列直接 pack 进队列存储，不需要 pack 暂存区。
    let engine = Engine::new(core, producer, &mut [], config);
    let (storage, _scan) = FatStorage::new(
        FlashCard {
            reads: Cell::new(0),
        },
        NoClock,
    );
    let (loads, n) = loads();
    Player::new(
        engine,
        storage,
        DemoOut,
        SharedFeeder,
        READ_CHUNK,
        &loads[..n],
    )
    .map_err(SetupError::Budget)
}

struct Run {
    finished: bool,
    failed: bool,
    clock: u32,
    dma_periods: u32,
    host_errors: u32,
}

fn run(p: &mut DemoPlayer, dma: &mut [u8]) -> Run {
    let mut serial = Serial;
    let mut lines = LineBuffer::<32>::new();
    let mut r = Run {
        finished: false,
        failed: false,
        clock: 0,
        dma_periods: 0,
        host_errors: 0,
    };
    let mut next_cmd = 0;
    // 有界：每次循环要么推进宿主，要么推进一个时钟周期；超出即视为错误。
    for _ in 0..50_000_000u32 {
        while let Some(&(at, cmd)) = SCRIPT.get(next_cmd) {
            if at > r.clock {
                break;
            }
            next_cmd += 1;
            for &b in cmd {
                match lines.push(b) {
                    Some(Ok(c)) => {
                        let _ = p.command(c, &mut serial);
                    }
                    Some(Err(e)) => {
                        let _ = write_parse_error(&mut serial, e);
                        r.host_errors += 1;
                    }
                    None => {}
                }
            }
        }
        let step = p.step();
        while let Some(n) = p.pop_notice() {
            match n {
                Notice::Skipped { track, fault } => {
                    hprintln!("QYBIT-SKIP track={} fault={:?}", track, fault)
                }
                Notice::Failed { track, fault } => {
                    r.failed = true;
                    hprintln!("QYBIT-FAIL track={} fault={:?}", track, fault)
                }
                Notice::Finished => r.finished = true,
                Notice::OutputError => r.host_errors += 1,
                _ => {}
            }
        }
        if step == Step::Busy {
            continue;
        }
        // 空闲：推进一个时钟周期（设备运行时即一个 DMA 缓冲周期）。
        r.clock += 1;
        if dma_period(dma) {
            r.dma_periods += 1;
        } else if !p.status().active && next_cmd >= SCRIPT.len() {
            return r;
        }
    }
    r.host_errors += 1;
    r
}

fn report(p: &DemoPlayer, r: &Run) -> u8 {
    let _ = board(|b| {
        for (i, s) in b.segments.iter().flatten().enumerate() {
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
    });
    let s = p.status();
    let _ = write_status(&mut Serial, &s);
    let reads = p.storage().device_reads();
    let errors = r.host_errors
        + s.counters.output_errors
        + s.counters.protocol_errors
        + s.counters.notices_dropped
        + s.output.errors
        + u32::from(board(|b| b.overflow).unwrap_or(true));
    hprintln!(
        "QYBIT-STAT finished={} underruns={} pause_periods={} dma_periods={} reads={} errors={} stale={} protocol={} pushed={}",
        u8::from(r.finished),
        s.output.underruns,
        s.output.paused_fills,
        r.dma_periods,
        reads,
        errors,
        s.engine.stale,
        s.engine.protocol_errors,
        s.engine.frames_pushed
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
    let m = p.margins();
    hprintln!(
        "QYBIT-TIMING nominal load_permille={} required_prefill_us={} prefill_us={} card_bytes={}",
        m.load_permille,
        m.required_prefill_us,
        m.prefill_us,
        CARD.len()
    );
    let ok = r.finished && !r.failed && errors == 0 && s.output.underruns == 0;
    u8::from(!ok)
}

/// 块读取计数（只用于报告）。
trait DeviceReads {
    fn device_reads(&self) -> u32;
}

impl DeviceReads for FatStorage<FlashCard, NoClock, MAX_TRACKS> {
    fn device_reads(&self) -> u32 {
        self.device(|d| d.reads.get()).unwrap_or(0)
    }
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
    let code = match (setup(), DMA_BUF.try_take()) {
        (Ok(mut p), Some(dma)) => {
            let r = run(&mut p, dma);
            report(&p, &r)
        }
        (Err(SetupError::Budget(e)), _) => {
            hprintln!("QYBIT-ERROR 时序不足：{}", e);
            2
        }
        _ => {
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
