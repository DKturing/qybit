//! ALSA 输出后端（M2 T3）。
//!
//! 打开流程（每个源格式一次）：
//! 1. 设备名只接受 `hw:`（[`check_device_name`]），其他 PCM 一律拒绝并说明原因；
//! 2. `snd_pcm_open`（阻塞、播放）；`EBUSY` 时报告占用者，不改用共享或混音路径；
//! 3. 查询设备接受的格式，按 [`candidates`]（V ≥ N 中 V 最小，同 V 按设备顺序）逐个尝试：
//!    设置参数并读回，用 [`confirm`] 核对；不一致或失败则释放参数、记录原因、试下一个；
//! 4. 都不行就报告打开失败，不降精度、不转换。
//!
//! 成功后渲染线程运行 `qy_player::render::write` 短写循环，结束时关闭 PCM。

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use qy_core::{DeviceFormat, SourceFormat};
use qy_pcm_format::alsa::{
    AlsaFormat, ChmapPlan, ChmapType, ConfirmError, OpenError, candidates, check_device_name,
    chmap_position, confirm, confirm_chmap, plan_chmap,
};

use crate::chmap::chmap_text;
use qy_pcm_format::ring::FrameConsumer;
use qy_player::render::write::{WriteConfig, render};
use qy_player::render::{RenderControl, RenderReport, ThreadPriority};
use qy_player::{Backend, PendingStream, RunningStream, Submission};

use crate::api::AlsaWriteApi;
use crate::attempt::{Attempt, Negotiation, OpenFailure};
use crate::pcm::{HwPlan, Opener, Pcm, errno};

/// 目标周期（毫秒）与每个缓冲的周期数。
pub const PERIOD_MS: u32 = 10;
pub const PERIODS: u32 = 4;
/// 等待设备可写的超时：远大于周期，超时视为设备停止工作。
const WAIT_TIMEOUT_MS: u32 = 2000;

/// ALSA 后端。
pub struct AlsaBackend<O: Opener, P: ThreadPriority + Clone + Send + 'static> {
    pub device: String,
    pub opener: O,
    pub priority: P,
    /// 只尝试该格式（矩阵逐项测试用）。
    pub forced: Option<AlsaFormat>,
    pub period_ms: u32,
    pub periods: u32,
}

impl<O: Opener, P: ThreadPriority + Clone + Send + 'static> AlsaBackend<O, P> {
    pub fn new(device: impl Into<String>, opener: O, priority: P) -> Self {
        Self {
            device: device.into(),
            opener,
            priority,
            forced: None,
            period_ms: PERIOD_MS,
            periods: PERIODS,
        }
    }
}

pub struct AlsaPending<X: Pcm, P> {
    pcm: X,
    format: DeviceFormat,
    buffer_frames: u32,
    negotiation: Negotiation,
    priority: P,
}

pub struct AlsaRunning(JoinHandle<(FrameConsumer, RenderReport)>);

/// 按计划设置并读回声道映射（F1，在 hw_params 之后）。返回报告说明，失败时返回原因。
fn apply_chmap<X: Pcm>(
    pcm: &mut X,
    source: &SourceFormat,
    plan: &ChmapPlan,
) -> Result<String, String> {
    let read_back = |pcm: &mut X| -> Result<Vec<u32>, String> {
        match pcm.get_chmap() {
            Ok(Some(m)) => Ok(m),
            Ok(None) => Err("设备不支持读回声道映射，无法确认多声道布局".into()),
            Err(e) => Err(format!("读回声道映射失败：{e}")),
        }
    };
    match plan {
        ChmapPlan::Unqueried(l) => Ok(format!("声道映射：设备不报告，按默认布局 {l}")),
        ChmapPlan::Set(l) => {
            let want: Vec<u32> = l.positions().iter().map(|&s| chmap_position(s)).collect();
            pcm.set_chmap(&want)
                .map_err(|e| format!("设置声道映射失败：{e}"))?;
            let got = read_back(pcm)?;
            confirm_chmap(plan, &got).map_err(|e| format!("{e}（读回 {}）", chmap_text(&got)))?;
            Ok(format!("声道映射：可变，已设置并读回 {}", chmap_text(&got)))
        }
        ChmapPlan::Fixed(l) => {
            let got = read_back(pcm)?;
            confirm_chmap(plan, &got).map_err(|e| format!("{e}（读回 {}）", chmap_text(&got)))?;
            let note = if *l == source.layout() {
                ""
            } else {
                "，已按设备映射重排声道（纯置换，样本值不变）"
            };
            Ok(format!("声道映射：固定，读回 {}{note}", chmap_text(&got)))
        }
    }
}

fn failure(source: &SourceFormat, attempts: Vec<Attempt>, message: String) -> OpenFailure {
    OpenFailure {
        negotiation: Negotiation {
            source: *source,
            attempts,
            chosen: None,
        },
        message,
    }
}

impl<O: Opener, P: ThreadPriority + Clone + Send + 'static> Backend for AlsaBackend<O, P> {
    type Attempt = Attempt;
    type Pending = AlsaPending<O::Pcm, P>;

    fn open(&mut self, source: &SourceFormat) -> Result<Self::Pending, OpenFailure> {
        let hw = match check_device_name(&self.device) {
            Ok(hw) => hw,
            Err(e) => {
                return Err(failure(
                    source,
                    Vec::new(),
                    format!("设备名 {:?} 被拒绝：{e}", self.device),
                ));
            }
        };
        let mut pcm = match self.opener.open(&self.device) {
            Ok(p) => p,
            Err(e) => {
                let mut message = format!(
                    "打开 {} 失败：{}（{e}）",
                    self.device,
                    OpenError::from_errno(e.errno)
                );
                if e.errno == errno::EBUSY
                    && let Some((card, device)) = hw.playback_node()
                {
                    let who = self.opener.occupants(card, device);
                    if !who.is_empty() {
                        message.push_str(&format!(
                            "；占用 /dev/snd/pcmC{card}D{device}p 的进程：{}",
                            who.join("、")
                        ));
                    }
                }
                return Err(failure(source, Vec::new(), message));
            }
        };
        let supported = match pcm.supported_formats() {
            Ok(s) => s,
            Err(e) => {
                return Err(failure(
                    source,
                    Vec::new(),
                    format!("查询设备格式失败：{e}"),
                ));
            }
        };
        let list: Vec<AlsaFormat> = match self.forced {
            Some(f) if f.valid_bits() >= source.bits() => vec![f],
            Some(_) => Vec::new(),
            None => candidates(source, &supported).as_slice().collect(),
        };
        // F1：先按设备报告的声道映射确定设备布局（可变则设置为源布局，固定则按设备顺序）。
        let maps = pcm.query_chmaps();
        let map_refs: Vec<(ChmapType, &[u32])> =
            maps.iter().map(|(t, m)| (*t, m.as_slice())).collect();
        let chmap_plan = match plan_chmap(&source.layout(), &map_refs) {
            Ok(p) => p,
            Err(e) => {
                let listed: Vec<String> = maps
                    .iter()
                    .map(|(t, m)| format!("{t:?} {}", chmap_text(m)))
                    .collect();
                return Err(failure(
                    source,
                    Vec::new(),
                    format!(
                        "设备 {} 不能确认源布局 {}：{e}（设备映射：{}）",
                        self.device,
                        source.layout(),
                        if listed.is_empty() {
                            "无".into()
                        } else {
                            listed.join("；")
                        }
                    ),
                ));
            }
        };
        let period = (source.sample_rate() * self.period_ms / 1000).max(1);
        let mut attempts = Vec::new();
        for alsa_format in list {
            let Ok(format) =
                alsa_format.device_format_layout(source.sample_rate(), chmap_plan.layout())
            else {
                continue;
            };
            let mut attempt = Attempt {
                alsa_format,
                format,
                error: None,
                request: None,
                applied: None,
                mismatch: None,
                chmap: None,
            };
            if !supported.contains(&alsa_format) {
                attempt.mismatch = Some("设备不接受该格式（test_format）".into());
                attempts.push(attempt);
                continue;
            }
            let plan = HwPlan {
                format: alsa_format,
                sample_rate: source.sample_rate(),
                channels: source.channels(),
                layout: chmap_plan.layout(),
                period_frames: period,
                periods: self.periods,
            };
            match pcm.configure(&plan) {
                Err(e) => attempt.error = Some(e),
                Ok(c) => {
                    attempt.request = Some(c.request);
                    attempt.applied = Some(c.applied);
                    let confirmed = confirm(source, &c.request, &c.applied);
                    let chmap = match &confirmed {
                        Ok(f) if *f == format => Some(apply_chmap(&mut pcm, source, &chmap_plan)),
                        _ => None,
                    };
                    match (confirmed, chmap) {
                        (Ok(_), Some(Err(why))) => attempt.mismatch = Some(why),
                        (Ok(confirmed), Some(Ok(note))) if confirmed == format => {
                            attempt.chmap = Some(note);
                            attempts.push(attempt);
                            let chosen = attempts.len() - 1;
                            return Ok(AlsaPending {
                                pcm,
                                format,
                                buffer_frames: c.request.buffer_frames,
                                negotiation: Negotiation {
                                    source: *source,
                                    attempts,
                                    chosen: Some(chosen),
                                },
                                priority: self.priority.clone(),
                            });
                        }
                        (Ok(other), _) => {
                            attempt.mismatch = Some(format!("核对得到的格式 {other:?} 与请求不同"));
                        }
                        (Err(ConfirmError::Mismatch(m)), _) => {
                            attempt.mismatch = Some(m.to_string());
                        }
                        (Err(ConfirmError::Request(e)), _) => {
                            attempt.mismatch = Some(format!("请求不能无损承载源：{e:?}"));
                        }
                    }
                }
            }
            attempts.push(attempt);
            let _ = pcm.hw_free();
        }
        let names: Vec<&str> = supported.iter().map(|f| f.name()).collect();
        Err(failure(
            source,
            attempts,
            format!(
                "设备 {} 不能以不改变样本的格式打开（源 {} Hz / {} 声道 / {} 位；设备接受 {}）",
                self.device,
                source.sample_rate(),
                source.channels(),
                source.bits(),
                if names.is_empty() {
                    "无".to_string()
                } else {
                    names.join(", ")
                }
            ),
        ))
    }
}

impl<X: Pcm, P: ThreadPriority + Clone + Send + 'static> PendingStream for AlsaPending<X, P> {
    type Attempt = Attempt;
    type Running = AlsaRunning;

    fn format(&self) -> DeviceFormat {
        self.format
    }

    fn negotiation(&self) -> &Negotiation {
        &self.negotiation
    }

    fn submission(&self) -> Submission {
        Submission::ShortWrite {
            buffer_frames: Some(self.buffer_frames),
        }
    }

    fn start(self, mut consumer: FrameConsumer, ctl: Arc<RenderControl>) -> AlsaRunning {
        let cfg = WriteConfig {
            block_align: usize::from(self.format.block_align()),
            event_timeout_ms: WAIT_TIMEOUT_MS,
            idle_sleep: Duration::from_millis(1),
        };
        let mut priority = self.priority;
        let mut api = AlsaWriteApi { pcm: self.pcm };
        AlsaRunning(std::thread::spawn(move || {
            let report = render(&mut api, &mut priority, &mut consumer, &ctl, cfg);
            // PCM 在线程结束时关闭。
            drop(api);
            (consumer, report)
        }))
    }
}

impl RunningStream for AlsaRunning {
    fn is_finished(&self) -> bool {
        self.0.is_finished()
    }

    fn join(self) -> (Option<FrameConsumer>, Option<RenderReport>) {
        match self.0.join() {
            Ok((c, r)) => (Some(c), Some(r)),
            Err(_) => (None, None),
        }
    }
}
