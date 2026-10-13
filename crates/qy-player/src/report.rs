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

use qy_core::{DeviceFormat, Fault, SourceFormat};

use crate::render::{RenderEnd, RenderReport};

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
}

impl<A> Negotiation<A> {
    pub fn new(source: SourceFormat) -> Self {
        Self {
            source,
            attempts: Vec::new(),
            chosen: None,
        }
    }

    pub fn chosen_attempt(&self) -> Option<&A> {
        self.chosen.and_then(|i| self.attempts.get(i))
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
    pub fault: Option<(u32, Fault)>,
    /// I/O 错误等附加说明。
    pub messages: Vec<String>,
    pub finished: bool,
    /// 渲染线程以错误结束（如设备失效）。
    pub device_error: bool,
    pub protocol_errors: u32,
}

impl<A> Default for PlaybackReport<A> {
    fn default() -> Self {
        Self {
            streams: Vec::new(),
            open_failures: Vec::new(),
            fault: None,
            messages: Vec::new(),
            finished: false,
            device_error: false,
            protocol_errors: 0,
        }
    }
}

impl<A> PlaybackReport<A> {
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
            && self.underrun_events() == 0
            && self.streams.iter().all(|s| {
                s.render
                    .as_ref()
                    .is_some_and(|r| r.end == RenderEnd::Drained)
            })
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

fn negotiation_text<A: AttemptRecord>(out: &mut String, n: &Negotiation<A>) {
    let s = &n.source;
    let _ = writeln!(
        out,
        "  源格式：{} Hz / {} 声道 / {} 位有效",
        s.sample_rate(),
        s.channels(),
        s.bits()
    );
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
    }
}

impl<A: AttemptRecord> PlaybackReport<A> {
    /// 面向用户的中文摘要。只陈述软件路径与协商结果，不宣称设备端 bit-perfect。
    pub fn summary(&self) -> String {
        let mut o = String::new();
        for (i, s) in self.streams.iter().enumerate() {
            let _ = writeln!(o, "输出流 {}：", i + 1);
            negotiation_text(&mut o, &s.negotiation);
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
        if let Some((track, fault)) = self.fault {
            let _ = writeln!(o, "曲目 {} 失败：{fault:?}", track + 1);
        }
        if self.device_error {
            o.push_str("输出设备错误：播放已停止。\n");
        }
        if self.software_path_intact() {
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
        o
    }
}
