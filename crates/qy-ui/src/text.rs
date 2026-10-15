//! 各前端共用的显示文字。只描述软件路径与协商结果，不宣称设备端 bit-perfect。

use qy_core::{DeviceFormat, SourceFormat};

use crate::model::{Model, Outcome};
use crate::probe::{Probe, TrackInfo};

/// 采样率的千赫兹写法：48000 → `48`，44100 → `44.1`，22050 → `22.05`。
pub fn khz(rate: u32) -> String {
    let whole = rate / 1000;
    let frac = rate % 1000;
    if frac == 0 {
        return whole.to_string();
    }
    let digits = format!("{frac:03}");
    format!("{whole}.{}", digits.trim_end_matches('0'))
}

/// `m:ss`，满一小时为 `h:mm:ss`。
pub fn clock(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// 帧数按采样率换成 `m:ss`；采样率为 0 时为空。
pub fn duration(frames: u64, rate: u32) -> String {
    if rate == 0 {
        return String::new();
    }
    clock(frames / u64::from(rate))
}

/// 源格式：`44.1 kHz 16 位 2 声道`。
pub fn source_text(f: &SourceFormat) -> String {
    format!(
        "{} kHz {} 位 {} 声道",
        khz(f.sample_rate()),
        f.bits(),
        f.channels()
    )
}

/// 设备格式：`44.1 kHz 2 声道 容器 32 位 / 有效 24 位`。
pub fn device_text(f: &DeviceFormat) -> String {
    format!(
        "{} kHz {} 声道 容器 {} 位 / 有效 {} 位",
        khz(f.sample_rate()),
        f.channels(),
        f.container_bits(),
        f.valid_bits()
    )
}

/// 播放列表“格式”列：`FLAC 44.1 kHz 16 位 2 声道`。
pub fn format_column(info: &TrackInfo) -> String {
    format!("{} {}", info.kind.label(), source_text(&info.format))
}

/// 播放列表的三列：文件名、时长、格式（读取中、无法播放时给出说明）。
pub fn columns(probe: &Probe, name: &str) -> [String; 3] {
    match probe {
        Probe::Pending => [name.to_string(), String::new(), "读取中…".into()],
        Probe::Ready(info) => [
            name.to_string(),
            info.frames
                .map_or_else(String::new, |n| duration(n, info.format.sample_rate())),
            format_column(info),
        ],
        Probe::Failed(r) => [name.to_string(), String::new(), format!("无法播放：{r}")],
    }
}

/// 状态栏文字。
pub fn status_line(m: &Model) -> String {
    if let Some(f) = &m.fatal {
        return format!("错误：{f}");
    }
    let Some(p) = &m.playing else {
        return match &m.last {
            Some(o) => outcome_line(o),
            None => "就绪".into(),
        };
    };
    let Some(s) = p.status else {
        return if p.stopping {
            "正在停止…".into()
        } else {
            "正在打开…".into()
        };
    };
    let mark = if s.paused { "⏸" } else { "▶" };
    let n = p.entries.len();
    let mut out = format!("{mark} {}/{n}", u64::from(s.track) + 1);
    if let Some(d) = &s.device {
        let pos = duration(s.position_frames, d.sample_rate());
        let total = m
            .playing_info()
            .and_then(|e| match &e.probe {
                Probe::Ready(i) => i.frames.map(|f| duration(f, i.format.sample_rate())),
                _ => None,
            })
            .map_or(String::new(), |t| format!(" / {t}"));
        out.push_str(&format!("  {pos}{total}"));
    }
    if let Some(src) = &s.source {
        out.push_str(&format!("  ·  源 {}", source_text(src)));
    }
    if let Some(d) = &s.device {
        out.push_str(&format!(" → 设备 {}（独占）", device_text(d)));
    }
    out.push_str(&format!("  ·  欠载 {}", s.underrun_events));
    out
}

/// 保真状态（播放中按实时计数，结束后按报告）。
pub fn fidelity_line(m: &Model) -> String {
    if let Some(p) = &m.playing {
        let underruns = p.status.map_or(0, |s| s.underrun_events);
        if underruns > 0 {
            return format!("有欠载 {underruns} 次：本次播放不是完整保真");
        }
        let altered = p.status.map_or(0, |s| s.channels_altered);
        if altered > 0 {
            return format!(
                "声道已改变：本次播放有 {altered} 首单声道复制到左右两路（明确开启的选项，样本值未改变）"
            );
        }
        return "软件路径：未改动样本、无欠载（不证明设备端 bit-perfect）".into();
    }
    match &m.last {
        Some(o) => outcome_line(o),
        None => String::new(),
    }
}

/// 一次播放结束后的结论（一行）。
pub fn outcome_line(o: &Outcome) -> String {
    if let Some(e) = &o.error {
        return format!("错误：{e}");
    }
    if o.strict_intact {
        "播放完成：严格路径，无欠载、无错误（不证明设备端 bit-perfect）".into()
    } else if o.software_intact {
        "播放完成：无欠载、无错误；部分曲目的声道布局已按选项改变，不是严格路径".into()
    } else if o.device_error {
        "输出设备错误：播放已停止".into()
    } else if o.finished && o.underrun_events > 0 {
        format!("播放完成但发生 {} 次欠载：不是完整保真", o.underrun_events)
    } else if o.interrupted {
        "已停止".into()
    } else if o.finished && o.user_skips > 0 {
        "播放完成（有曲目被切走，没有放完）".into()
    } else {
        "播放未完整完成，详见日志".into()
    }
}
