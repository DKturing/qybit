//! 假 ALSA PCM 与假打开器（`cfg(test)` 或 `test-support` 特性）。
//!
//! 在 alsa-lib 调用层模拟：状态机（打开 → 已准备 → 运行 → xrun/停止）、可写空间、随机短写、
//! 按序号注入的 xrun（`EPIPE`）与设备错误（`ENODEV`/`ESTRPIPE`）、读回参数的改写
//! （格式被改、近似采样率、有效位不足）、设置失败与打开失败（`EBUSY` 等）。设备只“播放”
//! 真实写入的字节；“耐心”设备在缓冲为空时不自行进入 xrun，只有脚本注入才 xrun。
//!
//! 渲染期间不分配：设备缓冲、播放记录与调用日志在打开/设置时按容量预留。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use qy_pcm_format::alsa::{Access, AlsaFormat, HwApplied, HwRequest};

use crate::pcm::{Configured, HwPlan, Opener, Pcm, PcmError, errno};

/// 调用日志。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PCall {
    Configure(AlsaFormat),
    HwFree,
    Start,
    Drop,
    Drain,
    Prepare,
    Wait,
    Avail,
    /// 所给帧数与结果（成功帧数或负 errno）。
    Write(u32, i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Open,
    Prepared,
    Running,
    Xrun,
    Setup,
}

/// 假设备行为。
#[derive(Debug, Clone)]
pub struct FakeAlsaConfig {
    /// 设备接受的格式（`test_format`）。
    pub supported: Vec<AlsaFormat>,
    /// 打开失败的 errno。
    pub open_errno: Option<i32>,
    pub occupants: Vec<String>,
    /// 设置该格式时 `set_rate` 失败（`EINVAL`）。
    pub reject_rate_for: Option<AlsaFormat>,
    /// 高于该采样率时 `set_rate` 失败（`EINVAL`）。
    pub max_rate: Option<u32>,
    /// 读回时格式被改写为该格式。
    pub rewrite_format: Option<AlsaFormat>,
    /// 读回的精确采样率 (num, den)。
    pub rate_numden: Option<(u32, u32)>,
    /// 读回的有效位。
    pub significant_bits: Option<u32>,
    /// 设备接受的周期为该值的整数倍（向上取整）。
    pub period_quantum: u32,
    /// 写入随机只接受一部分。
    pub short_writes: bool,
    pub seed: u64,
    /// 设备运行中的第 k 次 `writei`（从 0 起，跨整个 PCM）返回 `EPIPE`。
    pub xrun_at_writes: Vec<u64>,
    /// xrun 时报告的缺失帧。
    pub xrun_missing: u32,
    /// 第 k 次 `writei` 返回该 errno（设备错误）。
    pub fail_write: Option<(u64, i32)>,
    /// 每个 PCM 预留的播放记录字节数。
    pub reserve_bytes: usize,
}

impl Default for FakeAlsaConfig {
    fn default() -> Self {
        Self {
            supported: AlsaFormat::ALL.to_vec(),
            open_errno: None,
            occupants: Vec::new(),
            reject_rate_for: None,
            max_rate: None,
            rewrite_format: None,
            rate_numden: None,
            significant_bits: None,
            period_quantum: 1,
            short_writes: false,
            seed: 1,
            xrun_at_writes: Vec::new(),
            xrun_missing: 37,
            fail_write: None,
            reserve_bytes: 8 << 20,
        }
    }
}

/// 一个假 PCM 的记录（PCM 关闭时写入）。
#[derive(Debug, Clone)]
pub struct FakeAlsaOutput {
    pub name: String,
    pub configured: Vec<Configured>,
    pub rendered: Vec<u8>,
    pub calls: Vec<PCall>,
}

/// 假打开器：每次打开产生一个按 [`FakeAlsaConfig`] 行为的 PCM。
#[derive(Clone, Default)]
pub struct FakeOpener {
    pub config: FakeAlsaConfig,
    pub outputs: Arc<Mutex<Vec<FakeAlsaOutput>>>,
    /// 打开过的设备名。
    pub opened: Arc<Mutex<Vec<String>>>,
}

impl FakeOpener {
    pub fn new(config: FakeAlsaConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    pub fn outputs(&self) -> Vec<FakeAlsaOutput> {
        self.outputs.lock().map(|o| o.clone()).unwrap_or_default()
    }
}

impl Opener for FakeOpener {
    type Pcm = FakePcm;

    fn open(&mut self, name: &str) -> Result<FakePcm, PcmError> {
        if let Ok(mut o) = self.opened.lock() {
            o.push(name.to_string());
        }
        if let Some(e) = self.config.open_errno {
            return Err(PcmError {
                call: "snd_pcm_open",
                errno: e,
            });
        }
        Ok(FakePcm {
            name: name.to_string(),
            cfg: self.config.clone(),
            outputs: Arc::clone(&self.outputs),
            state: State::Open,
            plan: None,
            buffer_frames: 0,
            block_align: 0,
            device: VecDeque::new(),
            rendered: Vec::with_capacity(self.config.reserve_bytes),
            calls: Vec::with_capacity(1 << 16),
            configured: Vec::new(),
            writes: 0,
            running_writes: 0,
            rng: self.config.seed,
        })
    }

    fn occupants(&self, _card: u32, _device: u32) -> Vec<String> {
        self.config.occupants.clone()
    }
}

pub struct FakePcm {
    name: String,
    cfg: FakeAlsaConfig,
    outputs: Arc<Mutex<Vec<FakeAlsaOutput>>>,
    state: State,
    plan: Option<HwPlan>,
    buffer_frames: u32,
    block_align: usize,
    device: VecDeque<u8>,
    rendered: Vec<u8>,
    calls: Vec<PCall>,
    configured: Vec<Configured>,
    writes: u64,
    running_writes: u64,
    rng: u64,
}

fn err(call: &'static str, errno: i32) -> PcmError {
    PcmError { call, errno }
}

impl FakePcm {
    fn log(&mut self, c: PCall) {
        // 预留容量用尽后不再记录（不在渲染期间分配）。
        if self.calls.len() < self.calls.capacity() {
            self.calls.push(c);
        }
    }

    fn next_rand(&mut self, hi: u64) -> u64 {
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) % (hi + 1)
    }

    fn in_device(&self) -> u32 {
        (self.device.len() / self.block_align.max(1)) as u32
    }

    fn play(&mut self, frames: u32) {
        let n = (frames as usize * self.block_align).min(self.device.len());
        let room = self.rendered.capacity() - self.rendered.len();
        let keep = n.min(room);
        self.rendered.extend(self.device.drain(..n).take(keep));
    }
}

impl Pcm for FakePcm {
    fn supported_formats(&mut self) -> Result<Vec<AlsaFormat>, PcmError> {
        Ok(AlsaFormat::ALL
            .into_iter()
            .filter(|f| self.cfg.supported.contains(f))
            .collect())
    }

    fn configure(&mut self, plan: &HwPlan) -> Result<Configured, PcmError> {
        self.log(PCall::Configure(plan.format));
        if self.state != State::Open {
            return Err(err("snd_pcm_hw_params", errno::EBADFD));
        }
        if !self.cfg.supported.contains(&plan.format) {
            return Err(err("snd_pcm_hw_params_set_format", errno::EINVAL));
        }
        if self.cfg.reject_rate_for == Some(plan.format)
            || self.cfg.max_rate.is_some_and(|m| plan.sample_rate > m)
        {
            return Err(err("snd_pcm_hw_params_set_rate", errno::EINVAL));
        }
        let q = self.cfg.period_quantum.max(1);
        let period = plan.period_frames.div_ceil(q) * q;
        let buffer = period * plan.periods;
        let request = HwRequest {
            format: plan.format,
            sample_rate: plan.sample_rate,
            channels: plan.channels,
            layout: plan.layout,
            period_frames: period,
            buffer_frames: buffer,
        };
        let (rate_num, rate_den) = self.cfg.rate_numden.unwrap_or((plan.sample_rate, 1));
        let applied = HwApplied {
            access: Access::RwInterleaved,
            format_code: self.cfg.rewrite_format.unwrap_or(plan.format).code(),
            rate_num,
            rate_den,
            channels: u32::from(plan.channels),
            period_frames: period,
            buffer_frames: buffer,
            resample: false,
            significant_bits: self.cfg.significant_bits,
        };
        self.block_align =
            usize::from(plan.format.container_bits() / 8) * usize::from(plan.channels);
        self.buffer_frames = buffer;
        self.device = VecDeque::with_capacity(buffer as usize * self.block_align);
        self.plan = Some(*plan);
        self.state = State::Prepared;
        let c = Configured { request, applied };
        self.configured.push(c);
        Ok(c)
    }

    fn hw_free(&mut self) -> Result<(), PcmError> {
        self.log(PCall::HwFree);
        self.state = State::Open;
        Ok(())
    }

    fn start(&mut self) -> Result<(), PcmError> {
        self.log(PCall::Start);
        if self.state != State::Prepared || self.device.is_empty() {
            return Err(err("snd_pcm_start", errno::EBADFD));
        }
        self.state = State::Running;
        Ok(())
    }

    fn drop_now(&mut self) -> Result<(), PcmError> {
        self.log(PCall::Drop);
        self.device.clear();
        self.state = State::Setup;
        Ok(())
    }

    fn drain(&mut self) -> Result<(), PcmError> {
        self.log(PCall::Drain);
        if self.state == State::Xrun {
            return Err(err("snd_pcm_drain", errno::EPIPE));
        }
        let n = self.in_device();
        self.play(n);
        self.state = State::Setup;
        Ok(())
    }

    fn prepare(&mut self) -> Result<(), PcmError> {
        self.log(PCall::Prepare);
        self.device.clear();
        self.state = State::Prepared;
        Ok(())
    }

    fn wait(&mut self, _timeout_ms: u32) -> Result<bool, PcmError> {
        self.log(PCall::Wait);
        match self.state {
            State::Xrun => Err(err("snd_pcm_wait", errno::EPIPE)),
            State::Running => {
                let have = self.in_device();
                if have > 0 {
                    let k = 1 + self.next_rand(u64::from(have - 1)) as u32;
                    self.play(k);
                } else {
                    // 耐心设备：缓冲为空时不进入 xrun，稍候即返回。
                    std::thread::sleep(Duration::from_micros(50));
                }
                Ok(true)
            }
            _ => Err(err("snd_pcm_wait", errno::EBADFD)),
        }
    }

    fn avail(&mut self) -> Result<u64, PcmError> {
        self.log(PCall::Avail);
        match self.state {
            State::Xrun => Err(err("snd_pcm_avail", errno::EPIPE)),
            State::Prepared | State::Running => {
                Ok(u64::from(self.buffer_frames - self.in_device()))
            }
            _ => Err(err("snd_pcm_avail", errno::EBADFD)),
        }
    }

    fn writei(&mut self, frames: &[u8]) -> Result<usize, PcmError> {
        let ba = self.block_align.max(1);
        assert_eq!(frames.len() % ba, 0, "写入了不完整的帧");
        let given = (frames.len() / ba) as u32;
        let k = self.writes;
        self.writes += 1;
        if let Some((at, e)) = self.cfg.fail_write
            && at == k
        {
            self.log(PCall::Write(given, -i64::from(e)));
            return Err(err("snd_pcm_writei", e));
        }
        if self.state == State::Xrun {
            self.log(PCall::Write(given, -i64::from(errno::EPIPE)));
            return Err(err("snd_pcm_writei", errno::EPIPE));
        }
        let running_k = self.running_writes;
        if self.state == State::Running {
            self.running_writes += 1;
        }
        if self.state == State::Running && self.cfg.xrun_at_writes.contains(&running_k) {
            // 设备取空：已在缓冲中的数据先播完，再进入 xrun。
            let have = self.in_device();
            self.play(have);
            self.state = State::Xrun;
            self.log(PCall::Write(given, -i64::from(errno::EPIPE)));
            return Err(err("snd_pcm_writei", errno::EPIPE));
        }
        if !matches!(self.state, State::Prepared | State::Running) {
            self.log(PCall::Write(given, -i64::from(errno::EBADFD)));
            return Err(err("snd_pcm_writei", errno::EBADFD));
        }
        let room = self.buffer_frames - self.in_device();
        let mut n = given.min(room);
        if self.cfg.short_writes && n > 0 {
            n = self.next_rand(u64::from(n)) as u32;
        }
        self.device.extend(&frames[..n as usize * ba]);
        self.log(PCall::Write(given, i64::from(n)));
        Ok(n as usize)
    }

    fn xrun_missing_frames(&mut self) -> u32 {
        self.cfg.xrun_missing
    }
}

impl Drop for FakePcm {
    fn drop(&mut self) {
        let out = FakeAlsaOutput {
            name: std::mem::take(&mut self.name),
            configured: std::mem::take(&mut self.configured),
            rendered: std::mem::take(&mut self.rendered),
            calls: std::mem::take(&mut self.calls),
        };
        if let Ok(mut o) = self.outputs.lock() {
            o.push(out);
        }
    }
}
