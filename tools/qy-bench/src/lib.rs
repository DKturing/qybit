//! QyBit 桌面吞吐基准（R1 §6）。
//!
//! - [`decode_pack`]：只计时“解码 + pack”（自有解码器经 `qy-codec-auto` 按文件头选择，桌面配置档
//!   容量；输入在内存中，按核心输入窗口大小分段送入；pack 到 WASAPI 独占常用的设备格式），
//!   不含磁盘读取与设备输出。结果记为实时倍数（每秒处理的音频秒数）。
//! - [`end_to_end`]：真实播放器层（`qy-player`）、引擎、解码、pack、队列与整包渲染循环，接到按
//!   真实时间消费的假设备（每 10 ms 取一包），可在曲目读取层注入延迟与停顿；记录最低水位、
//!   临界次数与欠载，并把设备收到的有效字节与独立 pack 的参考逐字节比对。
//!
//! 测试文件由 `qy-gen-fixtures` 的基准生成器按固定种子生成，内容确定。结论只对运行它的机器成立。

use std::io::{self, Read};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use qy_codec_auto::AutoDecoder;
use qy_codec_auto::profile::DESKTOP;
use qy_core::{Decoder, DeviceFormat, PCM_WORK_SAMPLES, SourceFormat, Status};
use qy_gen_fixtures::bench::{self, Predictor};
use qy_pcm_format::pack::{Pcm, pack};
use qy_player::testing::{FakeBackend, FakeDevice};
use qy_player::{HostResources, PlaybackReport, TrackSource};

/// 编码方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Wav,
    Flac { block: usize, predictor: Predictor },
}

/// 一项基准规格。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    pub codec: Codec,
    pub rate: u32,
    pub channels: u16,
    pub bits: u32,
}

impl Spec {
    pub fn label(&self) -> String {
        let codec = match self.codec {
            Codec::Wav => "WAV".to_string(),
            Codec::Flac { block, predictor } => {
                let p = match predictor {
                    Predictor::Fixed(o) => format!("固定{o}阶"),
                    Predictor::Lpc(o) => format!("LPC{o}"),
                };
                format!("FLAC 块{block} {p}")
            }
        };
        format!(
            "{codec} {} kHz / {} 位 / {} 声道",
            fmt_rate(self.rate),
            self.bits,
            self.channels
        )
    }

    /// 生成 `seconds` 秒的文件与源样本（种子由规格导出，内容确定）。
    pub fn file(&self, seconds: f64) -> (Vec<u8>, Vec<i32>) {
        let frames = (f64::from(self.rate) * seconds).round() as usize;
        let seed =
            u64::from(self.rate) ^ (u64::from(self.channels) << 40) ^ (u64::from(self.bits) << 48);
        let s = bench::samples(seed, frames, self.channels, self.bits);
        let data = match self.codec {
            Codec::Wav => bench::wav(&s, self.rate, self.channels, self.bits),
            Codec::Flac { block, predictor } => {
                bench::flac(&s, self.rate, self.channels, self.bits, block, predictor)
            }
        };
        (data, s)
    }
}

fn fmt_rate(rate: u32) -> String {
    if rate % 1000 == 0 {
        format!("{}", rate / 1000)
    } else {
        format!("{:.1}", f64::from(rate) / 1000.0)
    }
}

/// 解码基准的全部规格（R1 §6）：WAV 与 FLAC 各 44.1/96/192/384 kHz、16/24/32 位、双声道与
/// 8 声道；FLAC 另含块 4096 与 16384、固定预测与 LPC 32 阶。
pub fn decode_specs() -> Vec<Spec> {
    let mut out = Vec::new();
    let rates = [44_100, 96_000, 192_000, 384_000];
    let bits = [16, 24, 32];
    let chans = [2u16, 8];
    for &rate in &rates {
        for &b in &bits {
            for &ch in &chans {
                out.push(Spec {
                    codec: Codec::Wav,
                    rate,
                    channels: ch,
                    bits: b,
                });
                for block in [4096, 16384] {
                    for predictor in [Predictor::Fixed(2), Predictor::Lpc(32)] {
                        out.push(Spec {
                            codec: Codec::Flac { block, predictor },
                            rate,
                            channels: ch,
                            bits: b,
                        });
                    }
                }
            }
        }
    }
    out
}

/// WASAPI 独占常用的设备格式：16 位源 → C16/V16，24 位源 → C32/V24（左对齐），32 位源 → C32/V32。
pub fn device_format(src: &SourceFormat) -> Result<DeviceFormat, String> {
    let (c, v) = match src.bits() {
        0..=16 => (16, 16),
        17..=24 => (32, 24),
        _ => (32, 32),
    };
    DeviceFormat::wasapi_layout(src.sample_rate(), src.layout(), c, v)
        .map_err(|e| format!("设备格式：{e:?}"))
}

fn leak<T: Copy + Default>(len: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); len].into_boxed_slice())
}

/// 解码器与 pack 缓冲：进程内只分配一次（与宿主相同的契约），各次运行复用。
pub struct DecodeRig {
    decoder: AutoDecoder,
    window: usize,
    out: Vec<u8>,
}

impl DecodeRig {
    pub fn new() -> Result<Self, String> {
        let cap = DESKTOP.flac_capacity().ok_or("桌面配置档未启用 FLAC")?;
        let decoder = AutoDecoder::new(
            leak(PCM_WORK_SAMPLES),
            cap,
            leak(cap.workspace_len()),
            leak(cap.pcm_len()),
        )
        .map_err(|e| format!("解码器：{e:?}"))?;
        let window = decoder.required_window();
        // pack 输出：一次解码产出的最多样本数（FLAC 一块 × 8 声道 × 4 字节）足够。
        let out = vec![0u8; cap.pcm_len().max(PCM_WORK_SAMPLES) * 4];
        Ok(Self {
            decoder,
            window,
            out,
        })
    }
}

/// 一次“解码 + pack”的结果。
#[derive(Debug, Clone, Copy)]
pub struct DecodeRun {
    pub frames: u64,
    pub elapsed: Duration,
    pub format: SourceFormat,
}

/// 解码并 pack 整个文件；`verify` 给出时逐样本比对解码结果（比对不计入计时，因此计时运行
/// 与比对运行分开调用）。
pub fn decode_pack(
    rig: &mut DecodeRig,
    data: &[u8],
    verify: Option<&[i32]>,
) -> Result<DecodeRun, String> {
    let d = &mut rig.decoder;
    d.reset();
    let mut pos = 0usize;
    let mut frames = 0u64;
    let mut format = None;
    let mut dev = None;
    let mut checked = 0usize;
    let start = Instant::now();
    loop {
        let end = (pos + rig.window).min(data.len());
        let eof = end == data.len();
        let p = d
            .decode(&data[pos..end], eof)
            .map_err(|e| format!("解码错误 {e:?}（{:?}）", d.flac_error()))?;
        pos += p.consumed;
        match p.status {
            Status::Frames(n) => {
                let f = match format {
                    Some(f) => f,
                    None => {
                        let f = d.format().ok_or("产出帧时格式未知")?;
                        format = Some(f);
                        dev = Some(device_format(&f)?);
                        f
                    }
                };
                let ch = usize::from(f.channels());
                let pcm = &d.pcm()[..n * ch];
                if let Some(v) = verify {
                    let want = v
                        .get(checked..checked + pcm.len())
                        .ok_or("解码样本多于源")?;
                    if pcm != want {
                        let i = pcm.iter().zip(want).position(|(a, b)| a != b).unwrap_or(0);
                        return Err(format!("第 {} 个样本不符", checked + i));
                    }
                    checked += pcm.len();
                }
                let src = Pcm::exact(pcm, f).map_err(|e| format!("{e:?}"))?;
                let dev = dev.as_ref().ok_or("设备格式未定")?;
                let packed = pack(&src, dev, &mut rig.out).map_err(|e| format!("pack：{e:?}"))?;
                if packed != n {
                    return Err(format!("pack 只放下 {packed}/{n} 帧"));
                }
                frames += n as u64;
                d.release(n);
            }
            Status::End => break,
            Status::NeedInput => {
                if p.consumed == 0 && end - pos >= rig.window {
                    return Err("输入窗口已满仍缺输入".into());
                }
                if p.consumed == 0 && eof {
                    return Err("EOF 后仍缺输入".into());
                }
            }
            Status::NeedOutput => {}
        }
    }
    let elapsed = start.elapsed();
    if let Some(v) = verify
        && checked != v.len()
    {
        return Err(format!("解码样本 {checked} 少于源 {}", v.len()));
    }
    Ok(DecodeRun {
        frames,
        elapsed,
        format: format.ok_or("没有产出")?,
    })
}

/// 一项解码基准的汇总。
#[derive(Debug, Clone)]
pub struct DecodeResult {
    pub spec: Spec,
    pub seconds: f64,
    pub file_bytes: usize,
    /// 各次实时倍数（升序）。
    pub rtf: Vec<f64>,
}

impl DecodeResult {
    /// 最慢一次。
    pub fn worst(&self) -> f64 {
        self.rtf.first().copied().unwrap_or(0.0)
    }

    pub fn median(&self) -> f64 {
        self.rtf.get(self.rtf.len() / 2).copied().unwrap_or(0.0)
    }
}

/// 运行一项解码基准：先比对一次（不计时），再计时 `reps` 次。
pub fn bench_decode(
    rig: &mut DecodeRig,
    spec: Spec,
    seconds: f64,
    reps: usize,
) -> Result<DecodeResult, String> {
    let (data, samples) = spec.file(seconds);
    decode_pack(rig, &data, Some(&samples)).map_err(|e| format!("{}：{e}", spec.label()))?;
    let mut rtf = Vec::with_capacity(reps);
    for _ in 0..reps {
        let r = decode_pack(rig, &data, None)?;
        let audio = r.frames as f64 / f64::from(spec.rate);
        rtf.push(audio / r.elapsed.as_secs_f64().max(1e-9));
    }
    rtf.sort_by(f64::total_cmp);
    Ok(DecodeResult {
        spec,
        seconds,
        file_bytes: data.len(),
        rtf,
    })
}

// ---------------------------------------------------------------- 端到端

/// 曲目读取层注入的延迟。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadDelay {
    /// 每次读取调用的固定延迟（模拟存储访问延迟）。
    pub per_read: Duration,
    /// 每读过这么多字节停顿一次（0 为不停顿）。
    pub stall_every_bytes: u64,
    pub stall: Duration,
}

/// 内存中的曲目，读取时按 [`ReadDelay`] 休眠。
pub struct MemSource {
    tracks: Vec<std::sync::Arc<Vec<u8>>>,
    delay: ReadDelay,
}

impl MemSource {
    pub fn new(tracks: Vec<Vec<u8>>, delay: ReadDelay) -> Self {
        Self {
            tracks: tracks.into_iter().map(std::sync::Arc::new).collect(),
            delay,
        }
    }
}

pub struct MemReader {
    data: std::sync::Arc<Vec<u8>>,
    pos: usize,
    delay: ReadDelay,
    since_stall: u64,
}

impl Read for MemReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.delay.per_read.is_zero() {
            std::thread::sleep(self.delay.per_read);
        }
        let n = buf.len().min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        if self.delay.stall_every_bytes > 0 {
            self.since_stall += n as u64;
            if self.since_stall >= self.delay.stall_every_bytes {
                self.since_stall = 0;
                std::thread::sleep(self.delay.stall);
            }
        }
        Ok(n)
    }
}

impl TrackSource for MemSource {
    type Reader = MemReader;

    fn count(&self) -> u32 {
        self.tracks.len() as u32
    }

    fn open(&mut self, track: u32) -> io::Result<MemReader> {
        let data = self
            .tracks
            .get(track as usize)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "曲目编号越界"))?;
        Ok(MemReader {
            data,
            pos: 0,
            delay: self.delay,
            since_stall: 0,
        })
    }
}

/// 端到端结果。
#[derive(Debug, Clone)]
pub struct E2eResult {
    pub spec: Spec,
    pub seconds: f64,
    pub delay: ReadDelay,
    pub wall: Duration,
    pub report: PlaybackReport<qy_player::testing::TestAttempt>,
    /// 设备收到的有效数据与独立参考逐字节一致；`None` 为未比对（欠载明细超出保留条数，或
    /// 播放未完成）。
    pub bytes_equal: Option<bool>,
    /// 计数自洽（设备有效帧 = 入队帧 = 源帧数，且播放完成、无协议错误）。
    pub counters_ok: bool,
    pub source_frames: u64,
}

impl E2eResult {
    pub fn render(&self) -> Option<&qy_player::render::RenderReport> {
        self.report.streams.first().and_then(|s| s.render.as_ref())
    }
}

/// 端到端用的宿主资源：进程内只分配一次（桌面配置档），各次运行复用。
pub fn host() -> &'static std::sync::Mutex<HostResources> {
    static HOST: OnceLock<std::sync::Mutex<HostResources>> = OnceLock::new();
    HOST.get_or_init(|| match HostResources::new() {
        Ok(h) => std::sync::Mutex::new(h),
        Err(e) => panic!("宿主资源：{e:?}"),
    })
}

/// 播放一首 `seconds` 秒的文件到按真实时间消费的假设备。
pub fn end_to_end(spec: Spec, seconds: f64, delay: ReadDelay) -> Result<E2eResult, String> {
    let (data, samples) = spec.file(seconds);
    let source_frames = (samples.len() / usize::from(spec.channels)) as u64;
    let mut backend = FakeBackend::new(&[(16, 16), (24, 24), (32, 24), (32, 32)]);
    backend.device = FakeDevice::Clock;
    let mut source = MemSource::new(vec![data], delay);
    let mut guard = host().lock().map_err(|_| "宿主资源锁中毒")?;
    let start = Instant::now();
    let report = qy_player::play_source(&mut backend, &mut guard, &mut source);
    let wall = start.elapsed();
    drop(guard);
    let outputs = backend.outputs();
    let mut bytes_equal = None;
    let mut counters_ok = false;
    if let (Some(o), true) = (outputs.first(), report.finished) {
        let r = &o.report;
        let pushed = report.streams.first().map_or(0, |s| s.pushed_frames);
        counters_ok = outputs.len() == 1
            && report.protocol_errors == 0
            && r.valid_frames == source_frames
            && pushed == source_frames;
        if r.underrun_events as usize <= qy_player::render::MAX_UNDERRUN_RECORDS {
            bytes_equal = Some(compare(o, &samples, spec)?);
        }
    }
    Ok(E2eResult {
        spec,
        seconds,
        delay,
        wall,
        report,
        bytes_equal,
        counters_ok,
        source_frames,
    })
}

/// 去掉欠载补入的静音后，设备收到的有效字节是否等于独立 pack 的参考；有效数据之后只允许静音。
fn compare(
    o: &qy_player::testing::FakeOutput,
    samples: &[i32],
    spec: Spec,
) -> Result<bool, String> {
    // 设备声道布局与源相同（协商按源布局请求），pack 为恒等置换。
    let src = SourceFormat::with_layout(spec.rate, o.format.layout(), spec.bits as u8)
        .map_err(|e| format!("源格式：{e:?}"))?;
    let pcm = Pcm::exact(samples, src).map_err(|e| format!("{e:?}"))?;
    let ba = usize::from(o.format.block_align());
    let mut want = vec![0u8; samples.len() / usize::from(spec.channels) * ba];
    pack(&pcm, &o.format, &mut want).map_err(|e| format!("参考 pack：{e:?}"))?;
    let frames = (o.format.sample_rate() / 100) as usize;
    let packet = frames * ba;
    let mut missing = std::collections::HashMap::new();
    for u in o.report.underrun_records() {
        missing.insert(u.packet as usize, u.missing_frames as usize);
    }
    let mut got = Vec::with_capacity(want.len());
    for (i, p) in o.rendered.chunks(packet).enumerate() {
        let keep = (frames - missing.get(&i).copied().unwrap_or(0)) * ba;
        got.extend_from_slice(&p[..keep.min(p.len())]);
    }
    if got.len() < want.len() {
        return Ok(false);
    }
    Ok(got[..want.len()] == want[..] && got[want.len()..].iter().all(|&b| b == 0))
}
