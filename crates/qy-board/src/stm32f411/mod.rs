//! 板：STM32F411（512 KiB 闪存、128 KiB RAM），特性 `stm32f411`。
//!
//! **只是示例**：说明一块板的适配层有多薄（外设初始化 + 三个接口的实现），不代表推荐型号；
//! 本仓库只验证它能编译，**未在实物上运行**。播放规则、FAT 读取、喂数与命令都在
//! `qy-host-embedded` / `qy-core`，这里不写任何播放逻辑。
//!
//! 外设（以 Nucleo-F411RE 为参照：HSE 8 MHz 来自 ST-LINK 的 MCO）：
//! - **SD 卡**：SDIO 4 位（PC8–PC11 = D0–D3，PC12 = CK，PD2 = CMD），HAL 先低速初始化再切到
//!   24 MHz；实现 `embedded_sdmmc::BlockDevice`，读错误后下次访问重新初始化（换卡）；
//! - **I²S**：SPI2/I²S2 主发送（PB12 = WS，PB13 = CK，PB15 = SD，不输出 MCK），飞利浦标准；
//!   DMA1 Stream4 通道 0 双缓冲，传输完成中断里调用 `Feeder::fill` 填空闲缓冲后换入；
//! - **串口**：USART2（PA2 = TX，PA3 = RX），115200 8N1，命令见 `qy_host_embedded::command`。
//!
//! 输出格式：
//! - 16 位源用 16 位帧（`Data16Channel16`），样本小端放入 16 位数据寄存器；
//! - 17–24 位源用 32 位帧中的 24 位数据（`Data24Channel32`）。SPI/I²S 数据寄存器是 16 位，DMA 按
//!   半字先发高半字，因此槽位格式为 32/24 左对齐、[`Endian::HalfWordSwapped`]，由 pack 直接排好；
//! - 采样率 = I2SCLK / (帧位数 × (2 × I2SDIV + ODD))（RM0383 §20.4.4，不输出 MCK 时）。I2SCLK 取
//!   61.44 MHz 时 8/16/32/48 kHz 都能整除，44.1 kHz 系列不能，因此不列出；`configure` 按实际的
//!   I2SCLK 重新核对，除不尽即报错，不近似。只能输出立体声（SPI/I²S 外设没有单声道模式）。
//!
//! 配置档：Embedded Full 需要约 360 KiB RAM，本芯片放不下；这里声明一个适合 128 KiB 的配置档
//! （只链接 WAV，≤ 48 kHz、≤ 24 位、双声道、预填 150 ms，约 98 KiB），代码与其他板相同。
//!
//! 时序：`budget` 要求的读取最大输入单元耗时 `U`、解码耗时 `D` 与打开曲目耗时 `O` 必须在实物上实测。本示例的值是
//! **未实测的占位值**，启动时打印警告；实测后替换 [`UNMEASURED_WAV_READ_US`] 等常量。
//!
//! `unsafe`：只有一处，开启 DMA 中断的 `NVIC::unmask`（cortex-m 规定为 unsafe：解除屏蔽可能打破
//! 依赖屏蔽的临界区；这里在中断处理所需的共享状态都已就绪之后才解除，处理函数只经临界区访问状态）。

use core::cell::{Cell, RefCell};
use core::fmt::{self, Write as _};

use cortex_m::interrupt::{Mutex, free};
use cortex_m_rt::entry;
use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};
use static_cell::ConstStaticCell;
use stm32f4xx_hal::dma::config::DmaConfig;
use stm32f4xx_hal::dma::{DMAError, MemoryToPeripheral, Stream4, StreamsTuple, Transfer};
use stm32f4xx_hal::i2s::I2s;
use stm32f4xx_hal::i2s::stm32_i2s_v12x::driver::{DataFormat, I2sDriver, I2sDriverConfig};
use stm32f4xx_hal::i2s::stm32_i2s_v12x::marker::{Master, Philips, Transmit};
use stm32f4xx_hal::pac::{self, DMA1, SPI2, interrupt};
use stm32f4xx_hal::prelude::*;
use stm32f4xx_hal::rcc::Config;
use stm32f4xx_hal::sdio::{ClockFreq, SdCard, Sdio};

use qy_codec_auto::AutoDecoder;
use qy_codec_auto::profile::{Budget, Profile, QueueKind};
use qy_core::engine::{Engine, EngineConfig, ErrorPolicy, ReusePolicy};
use qy_core::{Core, DeviceFormat, PCM_WORK_SAMPLES};
use qy_host_embedded::budget::{Demand, Load};
use qy_host_embedded::output::{AudioOutput, OutputCaps, SlotFormat};
use qy_host_embedded::player::{Player, write_notice, write_parse_error};
use qy_host_embedded::{FatStorage, Feeder, FeederAccess, LineBuffer, NoClock};
use qy_pcm_format::static_queue::StaticFrameQueue;
use qy_pcm_format::{Endian, Placement};

/// 适合 128 KiB RAM 的配置档：WAV、≤ 48 kHz、双声道、≤ 24 位（32 位槽位）、预填 150 ms。
const PROFILE: Profile = Profile::new(
    "STM32F411 示例",
    48_000,
    48_000,
    2,
    24,
    4,
    150,
    QueueKind::Static,
);
const BUDGET: Budget = PROFILE.budget();
const QUEUE_BYTES: usize = BUDGET.queue;
const WINDOW: usize = BUDGET.input_window;

/// 未实测的占位值（µs）：WAV 读取 16 KiB 的最长耗时与解码耗时。实测后替换。
const UNMEASURED_WAV_READ_US: u32 = 20_000;
const UNMEASURED_WAV_DECODE_US: u32 = 5_000;
/// 未实测的占位值（µs）：打开一首曲目（根目录查找与读取目录项）的最长耗时。实测后替换。
const UNMEASURED_OPEN_US: u32 = 20_000;
/// WAV 统计窗口字节数。
const WAV_UNIT: u32 = 16_384;
/// 主循环每步至多读取的字节数。
const READ_CHUNK: u32 = 512;

/// I2SCLK 目标值（PLLI2S）。
const I2S_CLK_HZ: u32 = 61_440_000;
/// 每个 DMA 缓冲的半字数（2 KiB）：16 位帧 512 帧，32 位帧 256 帧。
const DMA_WORDS: usize = 1024;
const DMA_BYTES: usize = DMA_WORDS * 2;

static RATES: [u32; 4] = [8_000, 16_000, 32_000, 48_000];
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
        endian: Endian::HalfWordSwapped,
        placement: Placement::MsbAligned,
    },
];
static CHANNELS: [u16; 1] = [2];
const CAPS: OutputCaps = OutputCaps {
    rates: &RATES,
    slots: &SLOTS,
    channels: &CHANNELS,
    // 最长缓冲：16 位帧时 512 帧。
    buffer_frames: (DMA_WORDS / 2) as u32,
    // 双缓冲：填某个缓冲时，前两个缓冲中较早的那个已播完。
    in_flight: 2,
};

static QUEUE: StaticFrameQueue<QUEUE_BYTES> = StaticFrameQueue::new();
static INPUT: ConstStaticCell<[u8; WINDOW]> = ConstStaticCell::new([0; WINDOW]);
static WAV_PCM: ConstStaticCell<[i32; PCM_WORK_SAMPLES]> =
    ConstStaticCell::new([0; PCM_WORK_SAMPLES]);
static DMA_A: ConstStaticCell<[u16; DMA_WORDS]> = ConstStaticCell::new([0; DMA_WORDS]);
static DMA_B: ConstStaticCell<[u16; DMA_WORDS]> = ConstStaticCell::new([0; DMA_WORDS]);
static DMA_C: ConstStaticCell<[u16; DMA_WORDS]> = ConstStaticCell::new([0; DMA_WORDS]);

// ---- SD 卡（SDIO） ----

#[derive(Debug)]
struct CardError;

impl fmt::Display for CardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SD 卡读取失败")
    }
}

impl core::error::Error for CardError {}

struct SdioCard {
    sdio: RefCell<Sdio<SdCard>>,
    ready: Cell<bool>,
}

impl SdioCard {
    fn ensure_ready(&self) -> Result<(), CardError> {
        if self.ready.get() {
            return Ok(());
        }
        let mut s = self.sdio.try_borrow_mut().map_err(|_| CardError)?;
        s.init(ClockFreq::F24Mhz).map_err(|_| CardError)?;
        self.ready.set(true);
        Ok(())
    }
}

impl BlockDevice for SdioCard {
    type Error = CardError;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), CardError> {
        self.ensure_ready()?;
        let mut s = self.sdio.try_borrow_mut().map_err(|_| CardError)?;
        for (i, b) in blocks.iter_mut().enumerate() {
            let addr = start.0.checked_add(i as u32).ok_or(CardError)?;
            if s.read_block(addr, &mut b.contents).is_err() {
                // 读错误（含拔卡）：下次访问重新初始化。
                self.ready.set(false);
                return Err(CardError);
            }
        }
        Ok(())
    }

    fn write(&self, _: &[Block], _: BlockIdx) -> Result<(), CardError> {
        Err(CardError)
    }

    fn num_blocks(&self) -> Result<BlockCount, CardError> {
        self.ensure_ready()?;
        let s = self.sdio.try_borrow().map_err(|_| CardError)?;
        let n = s.card().map_err(|_| CardError)?.block_count();
        Ok(BlockCount(u32::try_from(n).unwrap_or(u32::MAX)))
    }
}

// ---- I²S + DMA ----

type Driver = I2sDriver<I2s<SPI2>, Master, Transmit, Philips>;
type Buf = &'static mut [u16; DMA_WORDS];
type Tx = Transfer<Stream4<DMA1>, 0, Driver, MemoryToPeripheral, Buf>;

/// DMA 状态：停止时持有外设与三块缓冲；运行时持有传输与一块空闲缓冲。
enum Dma {
    Stopped {
        stream: Stream4<DMA1>,
        driver: Driver,
        bufs: [Buf; 3],
    },
    Running {
        transfer: Tx,
        /// 空闲缓冲；中断里取出填好后换入，换回的旧缓冲放回。
        spare: Option<Buf>,
    },
    /// 状态转换的中间态（不应长期存在）。
    Gone,
}

static DMA: Mutex<RefCell<Dma>> = Mutex::new(RefCell::new(Dma::Gone));
static FEEDER: Mutex<RefCell<Option<Feeder<QUEUE_BYTES>>>> = Mutex::new(RefCell::new(None));
/// 中断里 `next_transfer` 失败（DMA 未就绪或溢出）的次数。
static DMA_ERRORS: Mutex<Cell<u32>> = Mutex::new(Cell::new(0));

/// 喂数器填一个缓冲（字节按 pack 的内存顺序），再按小端半字写入 DMA 缓冲。
fn fill_words(feeder: &mut Feeder<QUEUE_BYTES>, words: &mut [u16; DMA_WORDS]) {
    let mut bytes = [0u8; DMA_BYTES];
    let _ = feeder.fill(&mut bytes);
    for (w, b) in words.iter_mut().zip(bytes.chunks_exact(2)) {
        *w = u16::from_le_bytes([b[0], b[1]]);
    }
}

struct SharedFeeder;

impl FeederAccess<QUEUE_BYTES> for SharedFeeder {
    fn with<R>(&self, f: impl FnOnce(&mut Feeder<QUEUE_BYTES>) -> R) -> Option<R> {
        free(|cs| {
            let mut cell = FEEDER.borrow(cs).try_borrow_mut().ok()?;
            cell.as_mut().map(f)
        })
    }
}

#[derive(Debug)]
enum OutError {
    /// DMA 状态不对（不应发生）。
    State,
    /// I2SCLK 不能整除出该采样率，或不是本适配支持的槽位。
    Clock,
}

struct I2sOut {
    i2s_clk: u32,
}

impl I2sOut {
    /// 由设备格式得出 I²S 数据格式与分频（必须整除）。
    fn plan(&self, f: &DeviceFormat) -> Result<(DataFormat, bool, u8), OutError> {
        let (data, frame_bits) = match (f.container_bits(), f.valid_bits(), f.endian()) {
            (16, 16, Endian::Little) => (DataFormat::Data16Channel16, 32u32),
            (32, 24, Endian::HalfWordSwapped) => (DataFormat::Data24Channel32, 64),
            _ => return Err(OutError::Clock),
        };
        let per_div = f
            .sample_rate()
            .checked_mul(frame_bits)
            .ok_or(OutError::Clock)?;
        if per_div == 0 || !self.i2s_clk.is_multiple_of(per_div) {
            return Err(OutError::Clock);
        }
        let d = self.i2s_clk / per_div; // 2 × I2SDIV + ODD
        let div = d / 2;
        if !(2..=255).contains(&div) {
            return Err(OutError::Clock);
        }
        Ok((data, d % 2 == 1, div as u8))
    }
}

impl AudioOutput for I2sOut {
    type Error = OutError;

    fn caps(&self) -> OutputCaps {
        CAPS
    }

    fn configure(&mut self, format: &DeviceFormat) -> Result<(), OutError> {
        let (data, odd, div) = self.plan(format)?;
        free(|cs| {
            let mut dma = DMA
                .borrow(cs)
                .try_borrow_mut()
                .map_err(|_| OutError::State)?;
            match core::mem::replace(&mut *dma, Dma::Gone) {
                Dma::Stopped {
                    stream,
                    driver,
                    bufs,
                } => {
                    let config = I2sDriverConfig::new_master()
                        .transmit()
                        .standard(Philips)
                        .data_format(data)
                        .master_clock(false)
                        .prescaler(odd, div);
                    let driver = driver.reconfigure(config);
                    *dma = Dma::Stopped {
                        stream,
                        driver,
                        bufs,
                    };
                    Ok(())
                }
                other => {
                    *dma = other;
                    Err(OutError::State)
                }
            }
        })
    }

    fn start(&mut self) -> Result<(), OutError> {
        free(|cs| {
            let mut dma = DMA
                .borrow(cs)
                .try_borrow_mut()
                .map_err(|_| OutError::State)?;
            let mut feeder = FEEDER
                .borrow(cs)
                .try_borrow_mut()
                .map_err(|_| OutError::State)?;
            let feeder = feeder.as_mut().ok_or(OutError::State)?;
            match core::mem::replace(&mut *dma, Dma::Gone) {
                Dma::Stopped {
                    stream,
                    mut driver,
                    bufs: [a, b, spare],
                } => {
                    // 启动前先填好两个缓冲。
                    fill_words(feeder, a);
                    fill_words(feeder, b);
                    driver.set_tx_dma(true);
                    let config = DmaConfig::default()
                        .memory_increment(true)
                        .double_buffer(true)
                        .transfer_complete_interrupt(true);
                    let mut transfer =
                        Transfer::init_memory_to_peripheral(stream, driver, a, Some(b), config);
                    transfer.start(|d| d.enable());
                    *dma = Dma::Running {
                        transfer,
                        spare: Some(spare),
                    };
                    Ok(())
                }
                other => {
                    *dma = other;
                    Err(OutError::State)
                }
            }
        })
    }

    fn stop(&mut self) {
        free(|cs| {
            let Ok(mut dma) = DMA.borrow(cs).try_borrow_mut() else {
                return;
            };
            match core::mem::replace(&mut *dma, Dma::Gone) {
                Dma::Running {
                    mut transfer,
                    spare,
                } => {
                    transfer.pause(|d| d.disable());
                    let (stream, mut driver, a, b) = transfer.release();
                    driver.set_tx_dma(false);
                    // 双缓冲模式下总有第二块缓冲，空闲缓冲只在中断执行期间短暂取出；缺失时无法再启动，
                    // 保持 Gone（之后启动返回错误，引擎按设备错误处理）。
                    if let (Some(b), Some(spare)) = (b, spare) {
                        *dma = Dma::Stopped {
                            stream,
                            driver,
                            bufs: [a, b, spare],
                        };
                    }
                }
                other => *dma = other,
            }
        });
    }
}

/// DMA 传输完成：填空闲缓冲并换入（双缓冲模式下硬件已切到另一块）。
#[interrupt]
fn DMA1_STREAM4() {
    free(|cs| {
        let (Ok(mut dma), Ok(mut feeder)) = (
            DMA.borrow(cs).try_borrow_mut(),
            FEEDER.borrow(cs).try_borrow_mut(),
        ) else {
            return;
        };
        let (Dma::Running { transfer, spare }, Some(feeder)) = (&mut *dma, feeder.as_mut()) else {
            return;
        };
        let Some(next) = spare.take() else {
            return;
        };
        fill_words(feeder, next);
        match transfer.next_transfer(next) {
            Ok((old, _)) => *spare = Some(old),
            Err(e) => {
                let errors = DMA_ERRORS.borrow(cs);
                errors.set(errors.get().wrapping_add(1));
                *spare = Some(match e {
                    DMAError::NotReady(b) | DMAError::SmallBuffer(b) | DMAError::Overrun(b) => b,
                });
            }
        }
    });
}

// ---- 串口 ----

struct Console<W: fmt::Write>(W);

impl<W: fmt::Write> fmt::Write for Console<W> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.write_str(s)
    }
}

// ---- 组装 ----

#[entry]
fn main() -> ! {
    // 外设只取一次；失败时停在这里（没有可报告的通道）。
    let (Some(dp), Some(_cp)) = (pac::Peripherals::take(), cortex_m::Peripherals::take()) else {
        loop {
            cortex_m::asm::wfi();
        }
    };
    let mut rcc = dp.RCC.freeze(
        Config::hse(8.MHz())
            .sysclk(96.MHz())
            .require_pll48clk()
            .i2s_clk(I2S_CLK_HZ.Hz()),
    );
    let i2s_clk = rcc.clocks.i2s_clk().map_or(0, |f| f.raw());
    let gpioa = dp.GPIOA.split(&mut rcc);
    let gpiob = dp.GPIOB.split(&mut rcc);
    let gpioc = dp.GPIOC.split(&mut rcc);
    let gpiod = dp.GPIOD.split(&mut rcc);

    let Ok(serial) = dp
        .USART2
        .serial((gpioa.pa2, gpioa.pa3), 115_200.bps(), &mut rcc)
    else {
        loop {
            cortex_m::asm::wfi();
        }
    };
    let (tx, mut rx) = serial.split();
    let mut out = Console(tx);
    let _ = writeln!(out, "QYBIT board=stm32f411 i2s_clk={i2s_clk}");
    let _ = writeln!(
        out,
        "WARN timing=unmeasured wav_read_us={UNMEASURED_WAV_READ_US} wav_decode_us={UNMEASURED_WAV_DECODE_US} open_us={UNMEASURED_OPEN_US}"
    );

    // SD 卡。
    let sdio: Sdio<SdCard> = Sdio::new(
        dp.SDIO,
        (
            gpioc.pc12,
            gpiod.pd2.internal_pull_up(true),
            gpioc.pc8.internal_pull_up(true),
            gpioc.pc9.internal_pull_up(true),
            gpioc.pc10.internal_pull_up(true),
            gpioc.pc11.internal_pull_up(true),
        ),
        &mut rcc,
    );
    let card = SdioCard {
        sdio: RefCell::new(sdio),
        ready: Cell::new(false),
    };
    let (storage, scan) = FatStorage::<_, _, 64>::new(card, NoClock);
    match scan {
        Ok(n) => {
            let _ = writeln!(out, "EVENT scan tracks={n}");
        }
        Err(_) => {
            let _ = writeln!(out, "EVENT scan error=no-card-or-fs");
        }
    }

    // I²S2 + DMA1 Stream4。
    let i2s = I2s::new(
        dp.SPI2,
        (gpiob.pb12, gpiob.pb13, SPI2::NoMck, gpiob.pb15),
        &mut rcc,
    );
    let driver = I2sDriverConfig::new_master()
        .transmit()
        .standard(Philips)
        .data_format(DataFormat::Data16Channel16)
        .master_clock(false)
        .prescaler(false, 2)
        .i2s_driver(i2s);
    let streams = StreamsTuple::new(dp.DMA1, &mut rcc);
    let (Some(a), Some(b), Some(c)) = (DMA_A.try_take(), DMA_B.try_take(), DMA_C.try_take()) else {
        loop {
            cortex_m::asm::wfi();
        }
    };

    // 解码器、核心、引擎。
    let Some(wav_pcm) = WAV_PCM.try_take() else {
        loop {
            cortex_m::asm::wfi();
        }
    };
    let Some(input) = INPUT.try_take() else {
        loop {
            cortex_m::asm::wfi();
        }
    };
    let Some((producer, consumer)) = QUEUE.split() else {
        loop {
            cortex_m::asm::wfi();
        }
    };
    // WAV 与 AIFF 共用流式工作区（F2）。
    let Ok(decoder) = AutoDecoder::empty()
        .with_wav(wav_pcm)
        .and_then(AutoDecoder::with_aiff)
    else {
        loop {
            cortex_m::asm::wfi();
        }
    };
    let window = decoder.required_window();
    let Ok(core) = Core::with_window(input, window, decoder) else {
        loop {
            cortex_m::asm::wfi();
        }
    };
    let engine = Engine::new(
        core,
        producer,
        &mut [],
        EngineConfig {
            caps: PROFILE.caps,
            reuse: ReusePolicy::Accepts,
            on_error: ErrorPolicy::Skip,
        },
    );
    free(|cs| {
        if let Ok(mut f) = FEEDER.borrow(cs).try_borrow_mut() {
            *f = Some(Feeder::new(consumer, CAPS.in_flight));
        }
        if let Ok(mut d) = DMA.borrow(cs).try_borrow_mut() {
            *d = Dma::Stopped {
                stream: streams.4,
                driver,
                bufs: [a, b, c],
            };
        }
    });
    // 共享状态都已就绪，才解除 DMA 中断屏蔽（见模块说明中的 unsafe 审计）。
    #[allow(unsafe_code)]
    unsafe {
        cortex_m::peripheral::NVIC::unmask(pac::Interrupt::DMA1_STREAM4);
    }

    let loads = [Load {
        demand: Demand::wav(&PROFILE.caps, WAV_UNIT),
        unit_read_us: UNMEASURED_WAV_READ_US,
        decode_us: UNMEASURED_WAV_DECODE_US,
        open_us: UNMEASURED_OPEN_US,
    }];
    let mut player = match Player::new(
        engine,
        storage,
        I2sOut { i2s_clk },
        SharedFeeder,
        READ_CHUNK,
        &loads,
    ) {
        Ok(p) => p,
        Err(e) => {
            let _ = writeln!(out, "ERR timing {e}");
            loop {
                cortex_m::asm::wfi();
            }
        }
    };

    let mut lines = LineBuffer::<32>::new();
    loop {
        while let Ok(byte) = rx.read() {
            match lines.push(byte) {
                Some(Ok(c)) => {
                    let _ = player.command(c, &mut out);
                }
                Some(Err(e)) => {
                    let _ = write_parse_error(&mut out, e);
                }
                None => {}
            }
        }
        while let Some(n) = player.pop_notice() {
            let _ = write_notice(&mut out, &n);
        }
        // 示例不进入低功耗等待：串口接收没有用中断或 DMA，WFI 期间到达的多个字符会因 1 字节
        // 接收寄存器溢出而丢失，因此空闲时也轮询。
        let _ = player.step();
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        cortex_m::asm::wfi();
    }
}
