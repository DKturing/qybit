//! 协商与播放报告（M2 T1 统一）。
//!
//! 记录明确区分四项，互不冒充：
//! 1. **请求格式**：宿主向后端请求的设备格式（[`AttemptRecord::requested`]）；
//! 2. **协商结果**：后端 API 对该请求的回答（如 WASAPI 独占 `IsFormatSupported`/`Initialize`
//!    的 HRESULT，ALSA `hw_params` 的返回值），由后端的 [`AttemptRecord::describe`] 给出；
//! 3. **可查询的已应用参数**：后端能从设备读回的实际参数（[`AppliedParams`]）。读不到的
//!    项为 `None`，不用其他来源代替（WASAPI 的共享混音格式 `GetMixFormat` 不得冒充独占流格式）；
//! 4. **回录证据**：数字回环录回并逐字节比对的结果（[`Evidence`]，M2 T4 填写）。
//!
//! 软件检查与虚拟回环都不证明真实 DAC 输入逐位一致。

use std::fmt::{Debug, Write as _};

use qy_core::engine::{EngineFault, FloatReport};
use qy_core::{DeviceFormat, SampleEncoding, SourceFormat};

use crate::render::{RenderEnd, RenderReport};

/// 故障说明：核心故障按原名（如 `Configure`、`Decode(Malformed)`），能力不足给出具体原因。
pub fn fault_text(fault: &EngineFault) -> String {
    match fault {
        EngineFault::Core(f) => format!("{f:?}"),
        EngineFault::Capability(e) => format!("能力不足：{e}"),
        EngineFault::Output => "输出队列错误".into(),
        EngineFault::NonFinite { frame } => {
            format!("第 {frame} 帧有 NaN 或无穷样本，无法转换为整数（不猜测数值），该曲目在此停止")
        }
    }
}

/// 浮点编码的文字（报告用）。
pub fn encoding_text(e: SampleEncoding) -> &'static str {
    match e {
        SampleEncoding::Int => "整数",
        SampleEncoding::Float32 => "32 位浮点",
        SampleEncoding::Float64 => "64 位浮点",
    }
}

/// 后端的一次格式尝试记录。
pub trait AttemptRecord: Clone + Debug + Send + 'static {
    /// 请求的设备格式。
    fn requested(&self) -> DeviceFormat;
    /// 描述类型与协商结果（不含请求格式本身），如
    /// `（WAVEFORMATEXTENSIBLE）IsFormatSupported(独占) 0x00000000，Initialize 0x00000000`。
    fn describe(&self) -> String;
    /// 协商成功后从设备读回的已应用参数；后端无法查询时为 `None`。
    fn applied(&self) -> Option<AppliedParams>;
}

/// 从设备读回的已应用参数。读不到的项为 `None`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedParams {
    /// 设备报告的实际格式。
    pub format: Option<DeviceFormat>,
    /// 设备缓冲帧数。
    pub buffer_frames: Option<u32>,
    /// 周期帧数。
    pub period_frames: Option<u32>,
    /// 说明（如哪些项无法查询及原因）。
    pub note: Option<&'static str>,
}

/// 回录证据。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Evidence {
    /// 未回录：只有软件路径与协商记录。
    #[default]
    NotCaptured,
}

/// 后端的提交语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submission {
    /// 每次请求与释放固定帧数的整包（WASAPI 独占事件驱动）。
    Packet { frames: u32 },
    /// 允许短写：只提交实际写出的帧，其余保留（ALSA `writei`）。
    ShortWrite { buffer_frames: Option<u32> },
}

/// 协商记录：源格式与各次尝试。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiation<A> {
    pub source: SourceFormat,
    pub attempts: Vec<A>,
    /// 成功的尝试序号。
    pub chosen: Option<usize>,
    /// 端点报告的物理扬声器配置（O1 §2：只作提示）。
    pub speakers: PhysicalSpeakers,
    /// 按单声道复制（O1）以立体声请求设备：源文件实际为同采样率、同位数的单声道 `[FC]`，
    /// `source` 为请求用的立体声格式。
    pub duplicated_mono: bool,
    /// 全部请求都因格式不受支持被拒绝时，对其他采样率的只读探测（后端支持时填写）。
    pub rate_probe: Option<Box<RateProbe>>,
}

/// 采样率探测：同一声道布局与位深候选下，逐个采样率做只读的格式查询（不打开设备）。
/// 用来说明失败是因为采样率不受支持，不代表会改用其他采样率：本程序不重采样。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateProbe {
    /// 查询过的采样率（不含源采样率）。
    pub checked: Vec<u32>,
    /// 其中端点接受的采样率。
    pub accepted: Vec<u32>,
    /// 源为单声道 `[FC]` 且所有采样率都被拒时，按立体声 `[FL FR]`（同样的位深候选）查询被接受的
    /// 采样率（含源采样率）。用来说明单声道复制到左右两路（`--mono-to-stereo` / 图形界面选项）能否让该文件播放；不会自动改用立体声。
    pub stereo_accepted: Option<Vec<u32>>,
}

impl<A> Negotiation<A> {
    pub fn new(source: SourceFormat) -> Self {
        Self {
            source,
            attempts: Vec::new(),
            chosen: None,
            speakers: PhysicalSpeakers::NotQueryable,
            duplicated_mono: false,
            rate_probe: None,
        }
    }

    pub fn chosen_attempt(&self) -> Option<&A> {
        self.chosen.and_then(|i| self.attempts.get(i))
    }
}

/// 端点报告的物理扬声器配置（tasks/O1.md §2）。
///
/// 这是驱动填写的信息，可能缺失或不准，因此**只作提示**：协商与播放照常进行，严格规则不变。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PhysicalSpeakers {
    /// 后端没有对应的属性（ALSA），不推测。
    #[default]
    NotQueryable,
    /// 端点没有该属性、类型不符或读取失败（WASAPI）。
    NotReported,
    /// 声道配置掩码（WASAPI `PKEY_AudioEndpoint_PhysicalSpeakers`，`VT_UI4`）。位定义见
    /// `KSAUDIO_CHANNEL_CONFIG`，与 `WAVEFORMATEXTENSIBLE` 声道掩码的 `SPEAKER_*` 相同；
    /// 0 为 `KSAUDIO_SPEAKER_DIRECTOUT`（声道不对应扬声器位置）。
    Mask(u32),
}

impl PhysicalSpeakers {
    /// 设备流中不在端点物理扬声器里的位置（WAVE 掩码）。只在端点报告了非零掩码、且有缺少的位置
    /// 时返回；其余情形无从判断或无需提示，返回 `None`。
    pub fn missing(&self, layout: &qy_core::ChannelLayout) -> Option<u32> {
        match *self {
            Self::Mask(m) if m != 0 => Some(layout.mask() & !m).filter(|&x| x != 0),
            _ => None,
        }
    }
}

/// 掩码按 `SPEAKER_*` 位从低到高写成 `[FL FR …]`；本程序未定义的位写成十六进制。
fn speaker_mask_text(mask: u32) -> String {
    let mut parts = Vec::new();
    let mut unknown = 0u32;
    for bit in 0..32 {
        if mask & (1 << bit) == 0 {
            continue;
        }
        match qy_core::Speaker::from_bit(bit) {
            Some(s) => parts.push(s.abbrev().to_string()),
            None => unknown |= 1 << bit,
        }
    }
    if unknown != 0 {
        parts.push(format!("0x{unknown:X}"));
    }
    format!("[{}]", parts.join(" "))
}

/// 物理扬声器的说明行（打开设备后给出；设备流布局取协商结果）。
pub(crate) fn speakers_text(
    out: &mut String,
    speakers: PhysicalSpeakers,
    stream: &qy_core::ChannelLayout,
) {
    match speakers {
        PhysicalSpeakers::NotQueryable => {
            out.push_str("  物理扬声器配置：不可查询（本后端没有对应属性）\n");
        }
        PhysicalSpeakers::NotReported => {
            out.push_str("  物理扬声器配置：端点未报告\n");
        }
        PhysicalSpeakers::Mask(0) => {
            out.push_str(
                "  物理扬声器配置：端点报告掩码 0x0（KSAUDIO_SPEAKER_DIRECTOUT，声道不对应扬声器位置）\n",
            );
        }
        PhysicalSpeakers::Mask(m) => {
            let _ = writeln!(
                out,
                "  物理扬声器配置（驱动报告）：0x{m:X} {}",
                speaker_mask_text(m)
            );
            if let Some(missing) = speakers.missing(stream) {
                let _ = writeln!(
                    out,
                    "  提示：端点报告的物理扬声器为 {}，设备流的 {} 不在其中，可能听不到。\
                     该信息由驱动填写，可能不准；协商与播放照常进行。",
                    speaker_mask_text(m),
                    speaker_mask_text(missing)
                );
            }
        }
    }
}

/// 打开失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenFailure<A> {
    pub negotiation: Negotiation<A>,
    pub message: String,
}

/// 一个输出流的记录。
#[derive(Debug, Clone)]
pub struct StreamReport<A> {
    pub negotiation: Negotiation<A>,
    /// 打开本流时的播放计划（R1）。
    pub plan: Option<qy_core::engine::PlaybackPlan>,
    /// 实际使用的设备格式（即被接受的请求格式）。
    pub format: DeviceFormat,
    pub submission: Submission,
    pub pushed_frames: u64,
    pub render: Option<RenderReport>,
    pub evidence: Evidence,
}

/// 播放报告。
#[derive(Debug, Clone)]
pub struct PlaybackReport<A> {
    pub streams: Vec<StreamReport<A>>,
    pub open_failures: Vec<OpenFailure<A>>,
    pub fault: Option<(u32, EngineFault)>,
    /// 按出错策略跳过的曲目（[`qy_core::engine::ErrorPolicy::Skip`]）。
    pub skipped: Vec<(u32, EngineFault)>,
    /// 曲目的结构修正（W1：只修正结构，不改样本）。
    pub repairs: Vec<(u32, qy_core::engine::Repair)>,
    /// 按明确开启的选项把单声道复制到左右两路的曲目（O1：声道布局已改变，样本值未改变）。
    pub channels_altered: Vec<u32>,
    /// 按明确开启的选项把浮点转为整数的曲目与位数（F3：样本已改变）。
    pub samples_altered: Vec<(u32, u8)>,
    /// 浮点曲目的统计（F3；同一曲目定位后的多段已累加）。
    pub float_stats: Vec<(u32, FloatReport)>,
    /// I/O 错误等附加说明。
    pub messages: Vec<String>,
    pub finished: bool,
    /// 渲染线程以错误结束（如设备失效）。
    pub device_error: bool,
    /// 按用户要求中断（如 Ctrl+C）。
    pub interrupted: bool,
    /// 按用户操作切换曲目（上一首、下一首、跳曲）的次数；被切走的曲目没有放完。
    pub user_skips: u32,
    /// 按操作在曲目内定位的次数（S1；定位过的曲目没有完整播放）。
    pub user_seeks: u32,
    /// 定位改为顺序解码的曲目（解码器不支持定位或音源不可定位；结果同样精确）。
    pub sequential_seeks: Vec<u32>,
    pub protocol_errors: u32,
}

impl<A> Default for PlaybackReport<A> {
    fn default() -> Self {
        Self {
            streams: Vec::new(),
            open_failures: Vec::new(),
            fault: None,
            skipped: Vec::new(),
            repairs: Vec::new(),
            channels_altered: Vec::new(),
            samples_altered: Vec::new(),
            float_stats: Vec::new(),
            messages: Vec::new(),
            finished: false,
            device_error: false,
            interrupted: false,
            user_skips: 0,
            user_seeks: 0,
            sequential_seeks: Vec::new(),
            protocol_errors: 0,
        }
    }
}

impl<A> PlaybackReport<A> {
    /// 记录一首浮点曲目的统计（同一曲目按段累加）。
    pub fn add_float_stats(&mut self, track: u32, r: FloatReport) {
        match self.float_stats.iter_mut().find(|(t, _)| *t == track) {
            Some((_, acc)) => {
                acc.nonfinite += r.nonfinite;
                acc.convert.add(&r.convert);
                if r.converted_to.is_some() {
                    acc.converted_to = r.converted_to;
                }
            }
            None => self.float_stats.push((track, r)),
        }
    }

    pub fn underrun_events(&self) -> u64 {
        self.streams
            .iter()
            .filter_map(|s| s.render.as_ref())
            .map(|r| r.underrun_events)
            .sum()
    }

    /// 能否把本次播放记录描述为软件路径完整：无欠载、无失败、无设备错误。
    /// 这不证明设备端 bit-perfect。
    pub fn software_path_intact(&self) -> bool {
        self.finished
            && self.fault.is_none()
            && !self.device_error
            && self.protocol_errors == 0
            && self.user_seeks == 0
            && self.underrun_events() == 0
            && self.streams.iter().all(|s| {
                s.render
                    .as_ref()
                    .is_some_and(|r| r.end == RenderEnd::Drained)
            })
    }

    /// 严格路径完整：[`software_path_intact`](Self::software_path_intact)，且没有曲目按明确开启的
    /// 选项改变声道布局（O1）或样本（F3 浮点转整数）。
    pub fn strict_path_intact(&self) -> bool {
        self.software_path_intact()
            && self.channels_altered.is_empty()
            && self.samples_altered.is_empty()
    }

    /// 进程退出码：0 完整，2 完成但有欠载，1 失败或设备错误。
    pub fn exit_code(&self) -> u8 {
        if self.software_path_intact() {
            0
        } else if self.finished
            && self.fault.is_none()
            && !self.device_error
            && self.protocol_errors == 0
        {
            2
        } else {
            1
        }
    }
}

fn format_text(f: &DeviceFormat) -> String {
    format!(
        "{} Hz / {} 声道 / 容器 {} 位 / 有效 {} 位",
        f.sample_rate(),
        f.channels(),
        f.container_bits(),
        f.valid_bits()
    )
}

fn applied_text(out: &mut String, a: Option<AppliedParams>) {
    let Some(a) = a else {
        out.push_str("  已应用参数：后端无法查询\n");
        return;
    };
    out.push_str("  已应用参数：");
    let mut parts = Vec::new();
    parts.push(match a.format {
        Some(f) => format!("格式 {}", format_text(&f)),
        None => "格式不可查询".to_string(),
    });
    if let Some(b) = a.buffer_frames {
        parts.push(format!("缓冲 {b} 帧"));
    }
    if let Some(p) = a.period_frames {
        parts.push(format!("周期 {p} 帧"));
    }
    out.push_str(&parts.join("，"));
    if let Some(n) = a.note {
        let _ = write!(out, "（{n}）");
    }
    out.push('\n');
}

pub(crate) fn negotiation_text<A: AttemptRecord>(out: &mut String, n: &Negotiation<A>) {
    let s = &n.source;
    let channels = if n.duplicated_mono { 1 } else { s.channels() };
    let _ = write!(
        out,
        "  源格式：{} Hz / {channels} 声道 / {} 位有效",
        s.sample_rate(),
        s.bits()
    );
    if n.duplicated_mono {
        let _ = write!(
            out,
            "（单声道复制到左右两路：按 {} 声道 {} 请求设备）",
            n.source.channels(),
            n.source.layout()
        );
    }
    out.push('\n');
    for (i, a) in n.attempts.iter().enumerate() {
        let _ = write!(
            out,
            "  请求 #{i}：{}{}",
            format_text(&a.requested()),
            a.describe()
        );
        if n.chosen == Some(i) {
            out.push_str(" ← 已协商");
        }
        out.push('\n');
    }
    if let Some(a) = n.chosen_attempt() {
        applied_text(out, a.applied());
        speakers_text(out, n.speakers, &a.requested().layout());
    }
    if let Some(p) = &n.rate_probe {
        rate_probe_text(out, p, n.source.sample_rate());
    }
}

fn rate_list(rates: &[u32]) -> String {
    rates
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join("、")
}

/// 采样率探测的说明行。
fn rate_probe_text(out: &mut String, p: &RateProbe, source_rate: u32) {
    let rejected: Vec<u32> = p
        .checked
        .iter()
        .copied()
        .filter(|r| !p.accepted.contains(r))
        .collect();
    if p.accepted.is_empty() {
        let _ = writeln!(
            out,
            "  采样率探测（只读查询，未打开设备）：同一声道布局与位深候选下，查询的 {} Hz 也都被拒绝，\
             问题不只在采样率{}。",
            rate_list(&p.checked),
            if p.stereo_accepted.is_some() {
                "，见下面的声道探测"
            } else {
                ""
            }
        );
        match &p.stereo_accepted {
            Some(st) if st.contains(&source_rate) => {
                let _ = writeln!(
                    out,
                    "  声道探测：端点在所有查询的采样率下都不接受单声道 [FC]；按立体声 [FL FR] 查询，\
                     端点接受 {} Hz（含源的 {source_rate} Hz）。可明确开启“单声道复制到左右两路”\
                     （命令行 --mono-to-stereo，图形界面同名复选框；样本值不变，报告标为“声道已改变”，\
                     不是严格路径）。",
                    rate_list(st)
                );
            }
            Some(st) if !st.is_empty() => {
                let _ = writeln!(
                    out,
                    "  声道探测：端点在所有查询的采样率下都不接受单声道 [FC]；按立体声 [FL FR] 查询，\
                     端点接受 {} Hz，不含源的 {source_rate} Hz。即使开启单声道复制到左右两路也不能播放\
                     （本程序不重采样）。",
                    rate_list(st)
                );
            }
            Some(_) => {
                out.push_str("  声道探测：按立体声 [FL FR] 查询也都被拒绝（位深候选不受支持）。\n");
            }
            None => {}
        }
        return;
    }
    let _ = writeln!(
        out,
        "  采样率探测（只读查询，未打开设备）：同一声道布局与位深候选下，端点接受 {} Hz{}。\
         源的 {source_rate} Hz 不受支持；本程序不重采样，因此不能播放。",
        rate_list(&p.accepted),
        if rejected.is_empty() {
            String::new()
        } else {
            format!("，拒绝 {} Hz", rate_list(&rejected))
        }
    );
}

impl<A: AttemptRecord> PlaybackReport<A> {
    /// 面向用户的中文摘要。只陈述软件路径与协商结果，不宣称设备端 bit-perfect。
    pub fn summary(&self) -> String {
        let mut o = String::new();
        for (i, s) in self.streams.iter().enumerate() {
            let _ = writeln!(o, "输出流 {}：", i + 1);
            negotiation_text(&mut o, &s.negotiation);
            if let Some(p) = &s.plan {
                let _ = writeln!(o, "  播放计划：{p}");
            }
            match s.submission {
                Submission::Packet { frames } => {
                    let _ = writeln!(o, "  提交方式：整包，每包 {frames} 帧");
                }
                Submission::ShortWrite { .. } => {
                    o.push_str("  提交方式：短写（只提交实际写出的帧，其余保留）\n");
                }
            }
            match s.evidence {
                Evidence::NotCaptured => o.push_str("  回录证据：无（未回录）\n"),
            }
            let _ = writeln!(o, "  已入队帧：{}", s.pushed_frames);
            if let Some(r) = &s.render {
                let w = &r.watermark;
                match (w.min_frames, w.min_ms(s.format.sample_rate())) {
                    (Some(f), Some(ms)) => {
                        let _ = writeln!(
                            o,
                            "  队列水位：最低 {f} 帧（{ms} ms），低水位 {} 次，临界 {} 次（不含设备缓冲）",
                            w.low, w.critical
                        );
                    }
                    _ => o.push_str(
                        "  队列水位：未观测（流未进入稳定播放）
",
                    ),
                }
            }
            match &s.render {
                None => o.push_str("  渲染：无报告（渲染线程异常结束）\n"),
                Some(r) => {
                    let p = &r.priority;
                    let reg = match p.registered {
                        Ok(()) => "成功".to_string(),
                        Err(c) => format!("失败（{c}）"),
                    };
                    let rev = match p.reverted {
                        None => "未注册无需撤销".to_string(),
                        Some(Ok(())) => "已撤销".to_string(),
                        Some(Err(c)) => format!("撤销失败（{c}）"),
                    };
                    let end = match r.end {
                        RenderEnd::Drained => "已排空".to_string(),
                        RenderEnd::Stopped => "已停止（未排空）".to_string(),
                        RenderEnd::Error(e) => format!("错误：{:?} {}", e.op, e.code),
                    };
                    let _ = writeln!(o, "  {}：注册{reg}，{rev}", p.label);
                    let _ = writeln!(
                        o,
                        "  渲染：有效帧 {}，尾部填充 {} 帧，排空静音 {} 帧，欠载 {} 次 / {} 帧，{end}",
                        r.valid_frames,
                        r.tail_fill_frames,
                        r.drain_frames,
                        r.underrun_events,
                        r.underrun_frames
                    );
                    if r.paused_frames > 0 {
                        let _ = writeln!(
                            o,
                            "  暂停：送出静音 {} 帧（按操作暂停，不是欠载）",
                            r.paused_frames
                        );
                    }
                    for u in r.underrun_records() {
                        let _ = writeln!(
                            o,
                            "    欠载：第 {} 次提交（设备帧 {}）缺 {} 帧",
                            u.packet, u.device_frame, u.missing_frames
                        );
                    }
                }
            }
        }
        for f in &self.open_failures {
            let _ = writeln!(o, "打开失败：{}", f.message);
            negotiation_text(&mut o, &f.negotiation);
        }
        for m in &self.messages {
            let _ = writeln!(o, "错误：{m}");
        }
        for (track, fault) in &self.skipped {
            let _ = writeln!(o, "曲目 {} 已跳过：{}", track + 1, fault_text(fault));
        }
        for (track, r) in &self.repairs {
            let _ = writeln!(
                o,
                "曲目 {} 按规则修正文件结构：{r}（样本未改动）",
                track + 1
            );
        }
        for track in &self.channels_altered {
            let _ = writeln!(
                o,
                "曲目 {} 声道已改变：单声道 [FC] 复制为 [FL FR]（明确开启；样本值未改变，不混音）",
                track + 1
            );
        }
        for (track, r) in &self.float_stats {
            match r.converted_to {
                Some(bits) => {
                    let _ = write!(
                        o,
                        "曲目 {} 样本已改变：{}转为 {bits} 位整数（明确开启；不加抖动，round-half-even）。",
                        track + 1,
                        encoding_text(r.encoding)
                    );
                    if r.convert.inexact == 0 {
                        let _ = writeln!(
                            o,
                            "共 {} 个样本，每个都能被 {bits} 位整数精确表示（例如由整数 PCM 生成的浮点文件），转换未损失数值，但表示已改变。",
                            r.convert.samples
                        );
                    } else {
                        let _ = writeln!(
                            o,
                            "共 {} 个样本，其中 {} 个不能精确表示（舍入），{} 个超出 ±1.0 被限幅。",
                            r.convert.samples, r.convert.inexact, r.convert.clipped
                        );
                    }
                }
                None if r.nonfinite > 0 => {
                    let _ = writeln!(
                        o,
                        "曲目 {} 含 {} 个 NaN 或无穷样本（{}），已按位原样送到设备。",
                        track + 1,
                        r.nonfinite,
                        encoding_text(r.encoding)
                    );
                }
                None => {}
            }
        }
        if let Some((track, fault)) = self.fault {
            let _ = writeln!(o, "曲目 {} 失败：{}", track + 1, fault_text(&fault));
        }
        if self.device_error {
            o.push_str("输出设备错误：播放已停止。\n");
        }
        if self.interrupted {
            o.push_str("已按用户要求中断（未排空，正在播放的曲目没有放完）。\n");
        }
        if self.user_skips > 0 {
            let _ = writeln!(
                o,
                "按操作切换曲目 {} 次（被切走的曲目没有放完）。",
                self.user_skips
            );
        }
        if self.user_seeks > 0 {
            let _ = writeln!(
                o,
                "按操作定位 {} 次（定位过的曲目没有完整播放）。",
                self.user_seeks
            );
        }
        for track in &self.sequential_seeks {
            let _ = writeln!(
                o,
                "曲目 {} 的音源不支持定位，已按顺序解码到目标位置（结果相同，只是更慢）。",
                track + 1
            );
        }
        if self.software_path_intact() && !self.samples_altered.is_empty() {
            o.push_str(
                "结论：全部曲目已播放，无欠载、无错误；部分曲目的浮点样本已按明确开启的选项转换为整数（样本已改变），不是严格路径。\n",
            );
        } else if self.software_path_intact() && !self.channels_altered.is_empty() {
            o.push_str(
                "结论：全部曲目已播放，无欠载、无错误；样本值未改变，但部分曲目的声道布局已按明确开启的选项改变（单声道复制到左右两路），不是严格路径。\n",
            );
        } else if self.software_path_intact() {
            o.push_str(
                "结论：全部曲目已播放，无欠载、无错误。这只说明软件路径与协商正常，不证明设备端 bit-perfect。\n",
            );
        } else if self.finished && self.underrun_events() > 0 {
            let _ = writeln!(
                o,
                "结论：播放完成但发生 {} 次欠载，本次播放不是完整保真。",
                self.underrun_events()
            );
        } else {
            o.push_str("结论：播放未完整完成。\n");
        }
        if self.software_path_intact() && !self.repairs.is_empty() {
            // 声道已改变时不能再说“软件路径无损”，只说结构修正没有改动样本。
            o.push_str(if self.channels_altered.is_empty() && self.samples_altered.is_empty() {
                "部分曲目的文件结构不规范，已按规则修正后播放：软件路径无损，但这不说明文件本身完好。\n"
            } else {
                "部分曲目的文件结构不规范，已按规则修正后播放：修正没有改动样本，但这不说明文件本身完好。\n"
            });
        }
        o
    }
}
