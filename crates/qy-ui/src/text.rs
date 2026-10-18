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

/// 源格式：`44.1 kHz 16 位 2 声道`；有损编码（E1）为 `44.1 kHz 解码 24 位 2 声道 有损`（位数是
/// 解码输出的精度，不是音源位深）。
pub fn source_text(f: &SourceFormat) -> String {
    let bits = if f.encoding().is_float() {
        format!("{} 位浮点", f.bits())
    } else {
        format!("{} 位", f.bits())
    };
    if f.is_lossy() {
        return format!(
            "{} kHz 解码 {bits} {} 声道 有损",
            khz(f.sample_rate()),
            f.channels()
        );
    }
    format!("{} kHz {bits} {} 声道", khz(f.sample_rate()), f.channels())
}

/// 设备格式：`44.1 kHz 2 声道 容器 32 位 / 有效 24 位`。
pub fn device_text(f: &DeviceFormat) -> String {
    if f.encoding().is_float() {
        return format!(
            "{} kHz {} 声道 {} 位浮点",
            khz(f.sample_rate()),
            f.channels(),
            f.container_bits()
        );
    }
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
    let mark = if s.paused { "‖" } else { "▶" };
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
    out.push_str(&format!("  ·  欠载 {}", s.underrun_events));
    if s.underrun_frames > 0 {
        out.push_str(&format!("（缺 {} 帧）", s.underrun_frames));
    }
    if s.skipped_tracks > 0 {
        out.push_str(&format!("  ·  跳过 {} 首", s.skipped_tracks));
    }
    out
}

/// 正在播放的曲目的源格式与设备流格式（独占协商结果）；未播放时为空。完整的请求、协商记录与
/// 已应用参数在日志中。
pub fn format_line(m: &Model) -> String {
    let Some(s) = m.playing.as_ref().and_then(|p| p.status) else {
        return String::new();
    };
    match (&s.source, &s.device) {
        (Some(src), Some(d)) => {
            format!("源 {} → 设备 {}（独占）", source_text(src), device_text(d))
        }
        (Some(src), None) => format!("源 {} → 设备未打开", source_text(src)),
        (None, Some(d)) => format!("设备 {}（独占）", device_text(d)),
        (None, None) => String::new(),
    }
}

/// 保真状态（播放中按实时计数）；未播放时为空，结束结论见 [`status_line`]。
pub fn fidelity_line(m: &Model) -> String {
    if let Some(p) = &m.playing {
        let underruns = p.status.map_or(0, |s| s.underrun_events);
        if underruns > 0 {
            return format!("有欠载 {underruns} 次：本次播放不是完整保真");
        }
        let skipped = p.status.map_or(0, |s| s.skipped_tracks);
        if skipped > 0 {
            return format!("有 {skipped} 首因错误跳过：本次播放未完整完成");
        }
        let samples = p.status.map_or(0, |s| s.samples_altered);
        if samples > 0 {
            return format!(
                "样本已改变：本次播放有 {samples} 首浮点转为整数（明确开启的选项），不是严格路径"
            );
        }
        let altered = p.status.map_or(0, |s| s.channels_altered);
        if altered > 0 {
            return format!(
                "声道已改变：本次播放有 {altered} 首单声道复制到左右两路（明确开启的选项，样本值未改变）"
            );
        }
        if p.status
            .and_then(|s| s.source)
            .is_some_and(|f| f.is_lossy())
        {
            // E1：有损编码时不能说成音源的无损数据。
            return "软件路径：未改动解码输出、无欠载；当前曲目是有损编码，播放的是解码重建的信号"
                .into();
        }
        return "软件路径：未改动样本、无欠载（不证明设备端 bit-perfect）".into();
    }
    String::new()
}

/// 一次播放结束后的结论（一行）。
pub fn outcome_line(o: &Outcome) -> String {
    if let Some(e) = &o.error {
        return format!("错误：{e}");
    }
    if o.strict_intact {
        "播放完成：严格路径，无欠载、无错误（不证明设备端 bit-perfect）".into()
    } else if o.software_intact {
        "播放完成：无欠载、无错误；部分曲目已按明确开启的选项改变（声道布局或样本），不是严格路径"
            .into()
    } else if o.device_error {
        "输出设备错误：播放已停止".into()
    } else if o.finished && o.underrun_events > 0 {
        format!("播放完成但发生 {} 次欠载：不是完整保真", o.underrun_events)
    } else if o.interrupted {
        "已停止".into()
    } else if o.finished && o.user_skips > 0 {
        "播放完成（有曲目被切走，没有放完）".into()
    } else if let Some(reason) = &o.stopped {
        format!("播放已停止：{reason}")
    } else {
        "播放未完整完成，详见日志".into()
    }
}

/// 采样率指示灯的状态（GUI T6）。只依据独占协商的结果，不宣称 DAC 实际运行在该采样率。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLamp {
    /// 未播放（灰）。
    Off,
    /// 正在打开设备（琥珀）。
    Opening,
    /// 设备流已按源采样率独占打开，没有重采样（绿）。
    Native,
    /// 设备未能按曲目格式打开；原因在状态栏与日志，不断言是采样率本身的问题（红）。
    Rejected,
    /// 设备流采样率与源不同（严格路径不应出现，出现即报告；红）。
    Mismatch,
}

/// 指示灯状态与旁边的文字（文字本身写明状态，不只靠颜色区分）。
pub fn rate_lamp(m: &Model) -> (RateLamp, String) {
    if let Some(p) = &m.playing {
        let Some(s) = p.status.filter(|_| !p.stopping) else {
            return (RateLamp::Opening, "正在打开…".into());
        };
        return match (&s.source, &s.device) {
            (Some(src), Some(d)) if src.sample_rate() != d.sample_rate() => (
                RateLamp::Mismatch,
                format!(
                    "采样率不一致：源 {} kHz，设备 {} kHz",
                    khz(src.sample_rate()),
                    khz(d.sample_rate())
                ),
            ),
            (_, Some(d)) => (
                RateLamp::Native,
                format!("{} kHz 原生", khz(d.sample_rate())),
            ),
            (Some(src), None) => (
                RateLamp::Opening,
                format!("{} kHz 打开中", khz(src.sample_rate())),
            ),
            (None, None) => (RateLamp::Opening, "正在打开…".into()),
        };
    }
    match &m.last {
        Some(o) if o.device_rejected => match &m.last_source {
            Some(src) => (
                RateLamp::Rejected,
                format!("{} kHz 未打开", khz(src.sample_rate())),
            ),
            None => (RateLamp::Rejected, "设备未打开".into()),
        },
        _ => (RateLamp::Off, "采样率 —".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// E1：有损编码的源写明“解码 N 位 … 有损”，无损源不变。
    #[test]
    fn lossy_source_text() {
        let Ok(f) = SourceFormat::new(44_100, 2, 24) else {
            panic!("格式");
        };
        assert_eq!(source_text(&f), "44.1 kHz 24 位 2 声道");
        assert_eq!(source_text(&f.lossy()), "44.1 kHz 解码 24 位 2 声道 有损");
        let Ok(x) = f
            .lossy()
            .with_encoding(qy_core::SampleEncoding::Float32, 32)
        else {
            panic!("浮点");
        };
        assert_eq!(source_text(&x), "44.1 kHz 解码 32 位浮点 2 声道 有损");
    }
}
