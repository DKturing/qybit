//! 通用嵌入式宿主的宿主侧仿真（M3 T2）：运行真实的 `qy-host-embedded`、引擎、自有解码器与 pack，
//! 只把 SD 卡与 I²S DMA 换成虚拟时间下的假设备。
//!
//! - **SD 卡**：内存中的 FAT16 镜像（`qy-fat-image` 用 `fatfs` 生成，与被测的 `embedded-sdmmc`
//!   读取路径相互独立）。每读一个块推进虚拟时钟；可注入固定延迟、周期性停顿、第 k 次块读取失败与拔卡；
//! - **I²S DMA**：启动时连续填两个缓冲（乒乓），此后每个缓冲周期在虚拟时间到点时：最早的在途缓冲
//!   播完（计入输出），再调用一次 `Feeder::fill` 填它（模拟中断）。读卡阻塞期间时钟推进，中断照常
//!   触发，与 MCU 上一致。停止 DMA 时在途缓冲丢弃（没有播出），因此过早停止会少输出数据；
//! - 每次设备配置开始一个新的输出段，只记录播出的有效数据部分（静音另计）。
//!
//! 期望字节由独立参考（FLAC：libFLAC `.s32le`；WAV：ffmpeg `.ffmpeg.s32le` 或测试自己写入的样本）
//! 按 `s << (C − N)` 放置，不调用被测的 pack。
#![allow(dead_code)]

use std::cell::RefCell;
use std::fmt;
use std::path::PathBuf;
use std::rc::Rc;

use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};
use embedded_storage::ReadStorage;
use qy_codec_auto::AutoDecoder;
use qy_codec_auto::profile::{EMBEDDED_FULL, Profile};
use qy_core::engine::{Engine, EngineConfig, ErrorPolicy, ReusePolicy};
use qy_core::{Core, DeviceFormat, PCM_WORK_SAMPLES};
use qy_host_embedded::budget::{Demand, Load};
use qy_host_embedded::output::{AudioOutput, OutputCaps, SlotFormat};
use qy_host_embedded::player::{Notice, Player, Step};
use qy_host_embedded::{
    BudgetError, FatStorage, Feeder, FeederAccess, FlatStorage, FlatTrack, NoClock, Storage,
};
use qy_pcm_format::static_queue::StaticFrameQueue;
use qy_pcm_format::{Endian, Placement};

/// Embedded Full 的静态队列字节数。
pub const QN: usize = EMBEDDED_FULL.budget().queue;
/// 播放列表上限。
pub const MAX_TRACKS: usize = 16;

pub const RATES: [u32; 9] = [
    8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000,
];
pub const SLOTS: [SlotFormat; 2] = [
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
pub const CHANNELS: [u16; 2] = [1, 2];

pub fn caps() -> OutputCaps {
    OutputCaps {
        rates: &RATES,
        slots: &SLOTS,
        channels: &CHANNELS,
        buffer_frames: 256,
        in_flight: 2,
    }
}

/// 一个输出段（一次设备配置）。
#[derive(Debug, Clone)]
pub struct Segment {
    pub format: DeviceFormat,
    pub data: Vec<u8>,
    pub silence_frames: u64,
}

/// 假 SD 卡的故障与时序。
#[derive(Debug, Clone, Default)]
pub struct Disk {
    pub image: Vec<u8>,
    /// 每次块读取的固定耗时（µs）。
    pub block_us: u64,
    /// 每 `stall_every` 次块读取额外停顿 `stall_us`（0 为不停顿）。
    pub stall_every: u64,
    pub stall_us: u64,
    /// 第几次块读取（从 1 起，全局计数）失败一次。
    pub fail_at: Option<u64>,
    /// 拔卡：所有读取失败。
    pub removed: bool,
    pub block_reads: u64,
    pub failures: u64,
}

struct Dma {
    next: u64,
    period: u64,
    buf_bytes: usize,
    /// 已填好、尚未播完的缓冲：（有效数据，静音帧数）。
    in_flight: std::collections::VecDeque<(Vec<u8>, u64)>,
}

/// 虚拟世界：时钟、DMA、喂数器、输出段与磁盘。
pub struct World {
    pub now: u64,
    dma: Option<Dma>,
    format: Option<DeviceFormat>,
    pub feeder: Option<Feeder<QN>>,
    pub segments: Vec<Segment>,
    pub disk: Disk,
    pub isr_calls: u64,
    pub caps: OutputCaps,
}

impl World {
    /// 填一个缓冲并放入在途队列。
    fn fill_one(&mut self) {
        let Some(d) = self.dma.as_ref() else {
            return;
        };
        let mut buf = vec![0u8; d.buf_bytes];
        let Some(f) = self.feeder.as_mut() else {
            return;
        };
        let fill = f.fill(&mut buf);
        self.isr_calls += 1;
        let fb = self.format.map_or(1, |f| usize::from(f.block_align()));
        buf.truncate(fill.data_frames * fb);
        if let Some(d) = self.dma.as_mut() {
            d.in_flight.push_back((buf, fill.silence_frames as u64));
        }
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

    /// 推进到 `t`，途中到点的 DMA 中断依次执行。
    pub fn advance_to(&mut self, t: u64) {
        while let Some(next) = self.dma.as_ref().map(|d| d.next) {
            if next > t {
                break;
            }
            self.now = next;
            self.isr();
            if let Some(d) = self.dma.as_mut() {
                d.next += d.period;
            }
        }
        self.now = self.now.max(t);
    }

    pub fn advance(&mut self, dt: u64) {
        let t = self.now + dt;
        self.advance_to(t);
    }

    pub fn next_dma(&self) -> Option<u64> {
        self.dma.as_ref().map(|d| d.next)
    }
}

pub type Shared = Rc<RefCell<World>>;

/// 假 SD 卡（块设备）。
pub struct SimDisk(pub Shared);

#[derive(Debug)]
pub struct DiskError(&'static str);

impl fmt::Display for DiskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl core::error::Error for DiskError {}

impl BlockDevice for SimDisk {
    type Error = DiskError;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), DiskError> {
        let mut w = self.0.borrow_mut();
        for (i, b) in blocks.iter_mut().enumerate() {
            w.disk.block_reads += 1;
            let n = w.disk.block_reads;
            let mut dt = w.disk.block_us;
            if w.disk.stall_every > 0 && n % w.disk.stall_every == 0 {
                dt += w.disk.stall_us;
            }
            w.advance(dt);
            if w.disk.removed {
                w.disk.failures += 1;
                return Err(DiskError("卡已拔出"));
            }
            if w.disk.fail_at == Some(n) {
                w.disk.failures += 1;
                return Err(DiskError("注入的读取错误"));
            }
            let off = (start.0 as usize + i) * Block::LEN;
            let Some(src) = w.disk.image.get(off..off + Block::LEN) else {
                return Err(DiskError("越界"));
            };
            b.contents.copy_from_slice(src);
        }
        Ok(())
    }

    fn write(&self, _: &[Block], _: BlockIdx) -> Result<(), DiskError> {
        Err(DiskError("只读"))
    }

    fn num_blocks(&self) -> Result<BlockCount, DiskError> {
        Ok(BlockCount(
            (self.0.borrow().disk.image.len() / Block::LEN) as u32,
        ))
    }
}

/// 假的无文件系统闪存（`embedded_storage::ReadStorage`）：与假 SD 卡共用镜像、时序与故障注入，
/// 按读取的 512 字节块数计时与计数。
pub struct SimFlash(pub Shared);

impl ReadStorage for SimFlash {
    type Error = DiskError;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), DiskError> {
        let mut w = self.0.borrow_mut();
        for _ in 0..bytes.len().div_ceil(512).max(1) {
            w.disk.block_reads += 1;
            let n = w.disk.block_reads;
            let mut dt = w.disk.block_us;
            if w.disk.stall_every > 0 && n % w.disk.stall_every == 0 {
                dt += w.disk.stall_us;
            }
            w.advance(dt);
            if w.disk.removed || w.disk.fail_at == Some(n) {
                w.disk.failures += 1;
                return Err(DiskError("注入的闪存读取错误"));
            }
        }
        let start = offset as usize;
        let Some(src) = w.disk.image.get(start..start + bytes.len()) else {
            return Err(DiskError("越界"));
        };
        bytes.copy_from_slice(src);
        Ok(())
    }

    fn capacity(&self) -> usize {
        self.0.borrow().disk.image.len()
    }
}

/// 无文件系统镜像：曲目按顺序紧接存放（每首按 4 字节对齐），返回镜像与曲目表（泄漏为 `'static`）。
pub fn flat_image(tracks: &[Track]) -> (Vec<u8>, &'static [FlatTrack]) {
    let mut img = Vec::new();
    let mut table = Vec::new();
    for t in tracks {
        while img.len() % 4 != 0 {
            img.push(0xA5);
        }
        table.push(FlatTrack {
            name: Box::leak(t.name.clone().into_boxed_str()),
            offset: img.len() as u32,
            len: t.bytes.len() as u32,
        });
        img.extend_from_slice(&t.bytes);
    }
    (img, Box::leak(table.into_boxed_slice()))
}

/// 用无文件系统存储搭建。
pub fn setup_flat(
    tracks: &[Track],
    disk: Disk,
    timing: Timing,
    policy: ErrorPolicy,
) -> Result<(FlatPlayer, Shared), BudgetError> {
    let (img, table) = flat_image(tracks);
    let disk = Disk { image: img, ..disk };
    setup_with(
        |w| match FlatStorage::new(SimFlash(w.clone()), table) {
            Ok(s) => s,
            Err(e) => panic!("曲目表：{e:?}"),
        },
        disk,
        caps(),
        timing,
        policy,
    )
}

/// 假 I²S 输出。
pub struct SimOut(pub Shared);

impl AudioOutput for SimOut {
    type Error = ();

    fn caps(&self) -> OutputCaps {
        self.0.borrow().caps
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
        let Some(f) = w.format else {
            return Err(());
        };
        let frames = u64::from(w.caps.buffer_frames);
        let period = (frames * 1_000_000).div_ceil(u64::from(f.sample_rate()));
        let now = w.now;
        w.dma = Some(Dma {
            next: now + period,
            period,
            buf_bytes: frames as usize * usize::from(f.block_align()),
            in_flight: std::collections::VecDeque::new(),
        });
        // 乒乓双缓冲：启动前先填好两个缓冲。
        w.fill_one();
        w.fill_one();
        Ok(())
    }

    fn stop(&mut self) {
        // 在途缓冲没有播出，丢弃。
        self.0.borrow_mut().dma = None;
    }
}

/// 主循环与“中断”共享的喂数器。
pub struct SimFeeder(pub Shared);

impl FeederAccess<QN> for SimFeeder {
    fn with<R>(&self, f: impl FnOnce(&mut Feeder<QN>) -> R) -> Option<R> {
        self.0.borrow_mut().feeder.as_mut().map(f)
    }
}

pub type SimPlayerWith<S> = Player<AutoDecoder, S, SimOut, SimFeeder, QN>;
pub type SimPlayer = SimPlayerWith<FatStorage<SimDisk, NoClock, MAX_TRACKS>>;
pub type FlatPlayer = SimPlayerWith<FlatStorage<SimFlash>>;

fn leak<T: Default + Clone>(n: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); n].into_boxed_slice())
}

pub const PROFILE: Profile = EMBEDDED_FULL;

/// 板级时序声明：（WAV 读 16 KiB 的 U，WAV 解码 D，FLAC 读最大帧的 U，FLAC 解码 D），µs。
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub wav_read_us: u32,
    pub wav_decode_us: u32,
    pub flac_read_us: u32,
    pub flac_decode_us: u32,
}

/// WAV 统计窗口字节数。
pub const WAV_UNIT: u32 = 16_384;

pub fn loads(t: Timing) -> [Load; 2] {
    let caps = PROFILE.caps;
    let Some(flac) = PROFILE.flac_capacity() else {
        panic!("Full 应启用 FLAC");
    };
    let small = qy_codec_flac::Capacity::for_limits(16, 2, 24).input_window as u32;
    [
        Load {
            demand: Demand::wav(&caps, WAV_UNIT),
            unit_read_us: t.wav_read_us,
            decode_us: t.wav_decode_us,
        },
        Load {
            demand: Demand::flac(
                &caps,
                flac.input_window as u32,
                flac.max_block as u32,
                small,
            ),
            unit_read_us: t.flac_read_us,
            decode_us: t.flac_decode_us,
        },
    ]
}

/// 搭建：磁盘镜像、输出能力与时序。返回播放器、共享世界与扫描结果。
pub fn setup(
    disk: Disk,
    caps: OutputCaps,
    timing: Timing,
    policy: ErrorPolicy,
) -> Result<(SimPlayer, Shared, u32), BudgetError> {
    let (p, w) = setup_with(
        |w| {
            let (storage, scan) = FatStorage::new(SimDisk(w.clone()), NoClock);
            if let Err(e) = scan {
                panic!("扫描失败：{e:?}");
            }
            storage
        },
        disk,
        caps,
        timing,
        policy,
    )?;
    let n = p.storage().track_count();
    Ok((p, w, n))
}

/// 用任意存储搭建（`make` 由共享世界构造存储）。
pub fn setup_with<S: Storage>(
    make: impl FnOnce(&Shared) -> S,
    disk: Disk,
    caps: OutputCaps,
    timing: Timing,
    policy: ErrorPolicy,
) -> Result<(SimPlayerWith<S>, Shared), BudgetError> {
    let world = Rc::new(RefCell::new(World {
        now: 0,
        dma: None,
        format: None,
        feeder: None,
        segments: Vec::new(),
        disk,
        isr_calls: 0,
        caps,
    }));
    let queue: &'static StaticFrameQueue<QN> = Box::leak(Box::new(StaticFrameQueue::new()));
    let Some((producer, consumer)) = queue.split() else {
        panic!("split");
    };
    world.borrow_mut().feeder = Some(Feeder::new(consumer, caps.in_flight));

    let Some(flac) = PROFILE.flac_capacity() else {
        panic!("flac");
    };
    let budget = PROFILE.budget();
    let decoder = match AutoDecoder::empty()
        .with_wav(leak(PCM_WORK_SAMPLES))
        .and_then(AutoDecoder::with_aiff)
        .and_then(|d| {
            d.with_flac(
                flac,
                leak(budget.flac_workspace / 8),
                leak(budget.flac_pcm / 4),
            )
        }) {
        Ok(d) => d,
        Err(e) => panic!("{e:?}"),
    };
    let window = decoder.required_window();
    let core = match Core::with_window(leak(budget.input_window), window, decoder) {
        Ok(c) => c,
        Err(e) => panic!("{e:?}"),
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
    let storage = make(&world);
    let player = Player::new(
        engine,
        storage,
        SimOut(world.clone()),
        SimFeeder(world.clone()),
        512,
        &loads(timing),
    )?;
    Ok((player, world))
}

/// 每次 `step` 的 CPU 耗时（µs，解码与 pack 的虚拟成本）。
pub const STEP_US: u64 = 20;

/// 运行主循环直到播放结束（无 DMA、不在播放）或超时。`hook` 在每步之前调用，可下发命令。
pub fn run<S: Storage>(
    p: &mut SimPlayerWith<S>,
    w: &Shared,
    limit_us: u64,
    notices: &mut Vec<Notice>,
    mut hook: impl FnMut(&mut SimPlayerWith<S>, &Shared, &[Notice]),
) {
    loop {
        hook(p, w, notices);
        let step = p.step();
        while let Some(n) = p.pop_notice() {
            notices.push(n);
        }
        let now = w.borrow().now;
        assert!(now <= limit_us, "仿真超时：{now} µs");
        match step {
            Step::Busy => w.borrow_mut().advance(STEP_US),
            Step::Idle => {
                let next = w.borrow().next_dma();
                match next {
                    Some(t) => w.borrow_mut().advance_to(t),
                    None if !p.status().active => break,
                    // 暂停中且设备未启动等：时间照常流逝。
                    None => w.borrow_mut().advance(1_000),
                }
            }
        }
    }
}

// ---- 参考样本与期望字节（独立公式） ----

pub fn fixtures(dir: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../{dir}/tests/fixtures"))
}

fn read_i32(path: &std::path::Path) -> Vec<i32> {
    match std::fs::read(path) {
        Ok(b) => b
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
        Err(e) => panic!("{}: {e}", path.display()),
    }
}

/// 一首曲目：文件内容与参考样本。
#[derive(Clone)]
pub struct Track {
    pub name: String,
    pub bytes: Vec<u8>,
    pub rate: u32,
    pub channels: u16,
    pub bits: u32,
    /// 参考样本（右对齐 N 位）；被拒绝的曲目为空。
    pub samples: Vec<i32>,
}

/// 夹具曲目：FLAC 取 libFLAC 参考，WAV 取 ffmpeg 参考（左对齐 s32 右移 32 − N）。
pub fn fixture(fat_name: &str, file: &str, rate: u32, channels: u16, bits: u32) -> Track {
    let (dir, samples) = if let Some(stem) = file.strip_suffix(".flac") {
        let d = fixtures("qy-codec-flac");
        (d.clone(), read_i32(&d.join(format!("{stem}.s32le"))))
    } else if let Some(stem) = file.strip_suffix(".wav") {
        let d = fixtures("qy-codec-wav");
        let s = read_i32(&d.join(format!("{stem}.ffmpeg.s32le")))
            .into_iter()
            .map(|s| s >> (32 - bits))
            .collect();
        (d, s)
    } else {
        panic!("{file}")
    };
    let bytes = match std::fs::read(dir.join(file)) {
        Ok(b) => b,
        Err(e) => panic!("{file}: {e}"),
    };
    Track {
        name: fat_name.to_string(),
        bytes,
        rate,
        channels,
        bits,
        samples,
    }
}

/// SplitMix64。
pub struct Prng(pub u64);
impl Prng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// 测试自己写入的 PCM WAV（格式码 1，容器 = N，N ∈ {16, 24}）：伪随机满幅样本。
pub fn synth_wav(
    fat_name: &str,
    rate: u32,
    channels: u16,
    bits: u32,
    frames: usize,
    seed: u64,
) -> Track {
    let mut r = Prng(seed);
    let n = frames * usize::from(channels);
    let samples: Vec<i32> = (0..n)
        .map(|_| ((r.next() >> (64 - bits)) as i64 - (1i64 << (bits - 1))) as i32)
        .collect();
    let bps = (bits / 8) as usize;
    let mut data = Vec::with_capacity(n * bps);
    for &s in &samples {
        data.extend_from_slice(&s.to_le_bytes()[..bps]);
    }
    let align = channels * bps as u16;
    let mut b = b"RIFF".to_vec();
    b.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * u32::from(align)).to_le_bytes());
    b.extend_from_slice(&align.to_le_bytes());
    b.extend_from_slice(&(bits as u16).to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(data.len() as u32).to_le_bytes());
    b.extend_from_slice(&data);
    Track {
        name: fat_name.to_string(),
        bytes: b,
        rate,
        channels,
        bits,
        samples,
    }
}

pub fn image(tracks: &[Track]) -> Vec<u8> {
    let files: Vec<(&str, &[u8])> = tracks
        .iter()
        .map(|t| (t.name.as_str(), t.bytes.as_slice()))
        .collect();
    // 簇 4 KiB（接近真实 SD 卡）：每 8 个数据块才需要一次 FAT 查找。
    match qy_fat_image::build_with(&files, 4096) {
        Ok(i) => i,
        Err(e) => panic!("镜像：{e}"),
    }
}

/// 独立公式：N 位样本放入 C 位容器（左对齐），小端 C/8 字节。
pub fn place(s: &[i32], n: u32, c: u32) -> Vec<u8> {
    s.iter()
        .flat_map(|&x| {
            let w = (i64::from(x) << (c - n)) as u32;
            w.to_le_bytes()[..(c / 8) as usize].to_vec()
        })
        .collect()
}

/// 期望的输出段：(采样率, 声道, C, V, 字节)。规则（独立于被测代码）：设备槽位取第一个能承载源的
/// （N ≤ 16 → 16/16，否则 32/24）；当前设备采样率、声道相同且 N ≤ V 时续用，否则开新段。
/// `playable[i]` 为假的曲目被拒绝（不打开设备、不影响当前设备）；`limit[i]` 截取参考的前若干帧。
pub fn expected(
    tracks: &[Track],
    playable: &[bool],
    limit: &[Option<usize>],
) -> Vec<(u32, u16, u32, u32, Vec<u8>)> {
    let mut out: Vec<(u32, u16, u32, u32, Vec<u8>)> = Vec::new();
    for (i, t) in tracks.iter().enumerate() {
        if !playable[i] {
            continue;
        }
        let ch = usize::from(t.channels);
        let frames = limit[i]
            .unwrap_or(t.samples.len() / ch)
            .min(t.samples.len() / ch);
        let s = &t.samples[..frames * ch];
        let reuse = out
            .last()
            .is_some_and(|l| l.0 == t.rate && l.1 == t.channels && t.bits <= l.3);
        if !reuse {
            let (c, v) = if t.bits <= 16 { (16, 16) } else { (32, 24) };
            out.push((t.rate, t.channels, c, v, Vec::new()));
        }
        if let Some(l) = out.last_mut() {
            let c = l.2;
            l.4.extend_from_slice(&place(s, t.bits, c));
        }
    }
    out
}

/// 实际输出段（去掉没有数据的段）与期望逐段比较。
pub fn check_segments(w: &Shared, want: &[(u32, u16, u32, u32, Vec<u8>)], what: &str) {
    let w = w.borrow();
    let got: Vec<&Segment> = w.segments.iter().filter(|s| !s.data.is_empty()).collect();
    assert_eq!(got.len(), want.len(), "{what}：段数");
    for (i, (g, e)) in got.iter().zip(want).enumerate() {
        let f = g.format;
        assert_eq!(
            (
                f.sample_rate(),
                f.channels(),
                u32::from(f.container_bits()),
                u32::from(f.valid_bits())
            ),
            (e.0, e.1, e.2, e.3),
            "{what}：第 {i} 段格式"
        );
        assert_eq!(g.data.len(), e.4.len(), "{what}：第 {i} 段字节数");
        assert!(g.data == e.4, "{what}：第 {i} 段字节不一致");
    }
}
