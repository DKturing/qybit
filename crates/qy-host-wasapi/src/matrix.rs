//! 设备格式矩阵与报告（M0 T6）。
//!
//! 矩阵：采样率 44.1/48/88.2/96/176.4/192 kHz × (C,V) 16/16、24/24、32/24、32/32 ×
//! 单/双声道，描述均为 WAVEFORMATEXTENSIBLE；16/16 另在每个采样率、单/双声道各测一次
//! WAVEFORMATEX。共 60 项。设备不支持是正常的能力结果，照实记录。
//!
//! 每项用 N = V 的测试 WAV（低电平方波）经真实核心、解码与 pack 播放；只对独占
//! 协商并初始化成功的组合播放。报告为确定性的 JSON 与 CSV（不含时间戳，顺序固定），
//! 只记录请求格式、协商结果（HRESULT）与渲染统计，不宣称设备端 bit-perfect。

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use qy_core::DeviceFormat;
use qy_pcm_format::wave::{WaveFormatEx, WaveFormatExtensible};

use qy_player::render::{ErrorCode, PriorityRecord, RenderEnd};
use qy_player::{Backend, HostResources, play};

use crate::attempt::{Attempt, Descriptor, Negotiation, PlaybackReport};

pub const RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
pub const LAYOUTS: [(u8, u8); 4] = [(16, 16), (24, 24), (32, 24), (32, 32)];
/// 后端版本描述（写入报告）。
pub fn backend_version() -> String {
    format!(
        "qy-host-wasapi {} (WASAPI exclusive event-driven, windows-rs 0.62.2)",
        env!("CARGO_PKG_VERSION")
    )
}

/// 每项测试信号时长（毫秒）。
pub const TEST_MS: u32 = 300;

/// 矩阵中的一项：设备格式与描述类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatrixItem {
    pub rate: u32,
    pub channels: u16,
    pub container_bits: u8,
    pub valid_bits: u8,
    pub descriptor: Descriptor,
}

impl MatrixItem {
    pub fn format(&self) -> Option<DeviceFormat> {
        DeviceFormat::wasapi(
            self.rate,
            self.channels,
            self.container_bits,
            self.valid_bits,
        )
        .ok()
    }

    pub fn frames(&self) -> u64 {
        u64::from(self.rate) * u64::from(TEST_MS) / 1000
    }
}

/// 全部矩阵项（固定顺序）。
pub fn matrix_items() -> Vec<MatrixItem> {
    let mut v = Vec::new();
    for rate in RATES {
        for channels in [1u16, 2] {
            for (c, b) in LAYOUTS {
                v.push(MatrixItem {
                    rate,
                    channels,
                    container_bits: c,
                    valid_bits: b,
                    descriptor: Descriptor::Extensible,
                });
            }
            v.push(MatrixItem {
                rate,
                channels,
                container_bits: 16,
                valid_bits: 16,
                descriptor: Descriptor::WaveFormatEx,
            });
        }
    }
    v
}

/// 请求的格式描述字段（由纯描述构造，与 SDK 映射逐字段相同）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requested {
    pub format_tag: u16,
    pub cb_size: u16,
    pub samples_per_sec: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub valid_bits: Option<u16>,
    pub block_align: u16,
    pub avg_bytes_per_sec: u32,
    pub channel_mask: Option<u32>,
    pub sub_format: Option<String>,
}

impl Requested {
    pub fn of(item: &MatrixItem) -> Option<Self> {
        let f = item.format()?;
        match item.descriptor {
            Descriptor::Extensible => {
                let w = WaveFormatExtensible::from_device(&f).ok()?;
                let g = w.sub_format;
                Some(Self {
                    format_tag: w.format.format_tag,
                    cb_size: w.format.cb_size,
                    samples_per_sec: w.format.samples_per_sec,
                    channels: w.format.channels,
                    bits_per_sample: w.format.bits_per_sample,
                    valid_bits: Some(w.valid_bits_per_sample),
                    block_align: w.format.block_align,
                    avg_bytes_per_sec: w.format.avg_bytes_per_sec,
                    channel_mask: Some(w.channel_mask),
                    sub_format: Some(format!(
                        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
                        g.data1,
                        g.data2,
                        g.data3,
                        g.data4[0],
                        g.data4[1],
                        g.data4[2],
                        g.data4[3],
                        g.data4[4],
                        g.data4[5],
                        g.data4[6],
                        g.data4[7]
                    )),
                })
            }
            Descriptor::WaveFormatEx => {
                let w = WaveFormatEx::from_device(&f).ok()?;
                Some(Self {
                    format_tag: w.format_tag,
                    cb_size: w.cb_size,
                    samples_per_sec: w.samples_per_sec,
                    channels: w.channels,
                    bits_per_sample: w.bits_per_sample,
                    valid_bits: None,
                    block_align: w.block_align,
                    avg_bytes_per_sec: w.avg_bytes_per_sec,
                    channel_mask: None,
                    sub_format: None,
                })
            }
        }
    }
}

/// 播放结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Playback {
    /// 未播放（不支持或打开失败）。
    NotRun,
    Played {
        end: String,
        valid_frames: u64,
        expected_frames: u64,
        tail_fill_frames: u64,
        drain_frames: u64,
        underrun_events: u64,
        underrun_frames: u64,
    },
}

/// 一项矩阵结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatrixEntry {
    pub item: MatrixItem,
    pub requested: Option<Requested>,
    pub is_format_supported: Option<i32>,
    pub initialize: Option<i32>,
    pub aligned_retry: Option<i32>,
    pub buffer_frames: Option<u32>,
    pub period_hns: Option<i64>,
    pub mmcss: Option<PriorityRecord>,
    pub playback: Playback,
    pub errors: Vec<String>,
}

impl MatrixEntry {
    pub fn supported(&self) -> bool {
        self.is_format_supported == Some(0)
    }

    /// 播放完整：排空结束、有效帧数等于测试信号帧数、无欠载。
    pub fn played_ok(&self) -> bool {
        matches!(&self.playback, Playback::Played { end, valid_frames, expected_frames, underrun_events, .. }
            if end == "Drained" && valid_frames == expected_frames && *underrun_events == 0)
    }
}

/// 设备与系统信息（读不到的字段为 `None`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceMeta {
    pub id: String,
    pub name: Option<String>,
    pub interface_name: Option<String>,
    pub driver_version: Option<String>,
    pub os: Option<String>,
    pub backend: String,
}

/// 顺序换曲检查结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sequential {
    pub rates: Vec<u32>,
    pub streams: usize,
    pub stream_valid_frames: Vec<u64>,
    pub expected_frames: Vec<u64>,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatrixReport {
    pub device: DeviceMeta,
    pub entries: Vec<MatrixEntry>,
    pub sequential: Option<Sequential>,
}

pub use qy_player::signal::write_test_wav;

fn first_attempt(n: &Negotiation) -> Option<&Attempt> {
    n.chosen
        .and_then(|i| n.attempts.get(i))
        .or(n.attempts.first())
}

/// 把一次单曲播放报告转换为矩阵结果。
pub fn entry_from_playback(
    item: MatrixItem,
    expected_frames: u64,
    r: &PlaybackReport,
) -> MatrixEntry {
    let negotiation = r
        .streams
        .first()
        .map(|s| &s.negotiation)
        .or(r.open_failures.first().map(|f| &f.negotiation));
    let attempt = negotiation.and_then(first_attempt);
    let mut errors: Vec<String> = r.open_failures.iter().map(|f| f.message.clone()).collect();
    errors.extend(r.messages.iter().cloned());
    if let Some((track, fault)) = &r.fault {
        errors.push(format!(
            "曲目 {track} 失败: {}",
            qy_player::fault_text(fault)
        ));
    }
    let render = r.streams.first().and_then(|s| s.render);
    if let Some(rr) = &render
        && let RenderEnd::Error(e) = rr.end
    {
        errors.push(format!("渲染错误 {:?} {}", e.op, e.code));
    }
    let playback = match render {
        Some(rr) => Playback::Played {
            end: match rr.end {
                RenderEnd::Drained => "Drained".into(),
                RenderEnd::Stopped => "Stopped".into(),
                RenderEnd::Error(_) => "Error".into(),
            },
            valid_frames: rr.valid_frames,
            expected_frames,
            tail_fill_frames: rr.tail_fill_frames,
            drain_frames: rr.drain_frames,
            underrun_events: rr.underrun_events,
            underrun_frames: rr.underrun_frames,
        },
        None => Playback::NotRun,
    };
    MatrixEntry {
        item,
        requested: Requested::of(&item),
        is_format_supported: attempt.and_then(|a| a.is_format_supported),
        initialize: attempt.and_then(|a| a.initialize),
        aligned_retry: attempt.and_then(|a| a.aligned_retry),
        buffer_frames: attempt.and_then(|a| a.buffer_frames),
        period_hns: attempt.and_then(|a| a.period_hns),
        mmcss: render.map(|rr| rr.priority),
        playback,
        errors,
    }
}

/// 逐项运行矩阵。`backend_for` 为每项构造只尝试该格式与描述的后端。
pub fn run_matrix<B: Backend<Attempt = Attempt>>(
    items: &[MatrixItem],
    res: &mut HostResources,
    dir: &Path,
    mut backend_for: impl FnMut(&MatrixItem) -> B,
) -> std::io::Result<Vec<MatrixEntry>> {
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let path = dir.join(format!(
            "qybit-matrix-{i:02}-{}-{}ch-n{}.wav",
            item.rate, item.channels, item.valid_bits
        ));
        let frames = write_test_wav(
            &path,
            item.rate,
            item.channels,
            item.valid_bits,
            item.frames(),
        )?;
        let mut backend = backend_for(item);
        let report = play(&mut backend, res, std::slice::from_ref(&path));
        out.push(entry_from_playback(*item, frames, &report));
        let _ = std::fs::remove_file(&path);
    }
    Ok(out)
}

/// 顺序换曲：取两个不同采样率、均已播放成功的 EXTENSIBLE 项，连续播放两首，检查
/// 重开设备（两个流）、各流有效帧完整（无旧 PCM、尾帧不截断）、无欠载。
pub fn run_sequential<B: Backend<Attempt = Attempt>>(
    entries: &[MatrixEntry],
    res: &mut HostResources,
    dir: &Path,
    backend: &mut B,
) -> std::io::Result<Option<Sequential>> {
    let mut picked: Vec<&MatrixEntry> = Vec::new();
    for e in entries {
        if e.item.descriptor == Descriptor::Extensible
            && e.played_ok()
            && picked.iter().all(|p| p.item.rate != e.item.rate)
        {
            picked.push(e);
        }
        if picked.len() == 2 {
            break;
        }
    }
    if picked.len() < 2 {
        return Ok(None);
    }
    let mut files: Vec<PathBuf> = Vec::new();
    let mut expected = Vec::new();
    for (k, e) in picked.iter().enumerate() {
        let p = dir.join(format!("qybit-sequential-{k}.wav"));
        // 第二首稍长，便于区分两段。
        let frames = e.item.frames() + k as u64 * u64::from(e.item.rate) / 10;
        expected.push(write_test_wav(
            &p,
            e.item.rate,
            e.item.channels,
            e.item.valid_bits,
            frames,
        )?);
        files.push(p);
    }
    let r = play(backend, res, &files);
    for f in &files {
        let _ = std::fs::remove_file(f);
    }
    let valid: Vec<u64> = r
        .streams
        .iter()
        .map(|s| s.render.map_or(0, |x| x.valid_frames))
        .collect();
    let drained = r
        .streams
        .iter()
        .all(|s| s.render.is_some_and(|x| x.end == RenderEnd::Drained));
    let ok = r.software_path_intact() && r.streams.len() == 2 && drained && valid == expected;
    Ok(Some(Sequential {
        rates: picked.iter().map(|e| e.item.rate).collect(),
        streams: r.streams.len(),
        stream_valid_frames: valid,
        expected_frames: expected,
        ok,
        detail: if ok {
            "两次独立打开设备，各流有效帧数与源一致，排空结束，无欠载".into()
        } else {
            format!(
                "fault={:?} device_error={} underruns={}",
                r.fault,
                r.device_error,
                r.underrun_events()
            )
        },
    }))
}

// ---- JSON / CSV ----

fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for ch in s.chars() {
        match ch {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn json_opt_str(s: &Option<String>) -> String {
    s.as_deref().map_or_else(|| "null".into(), json_str)
}

fn hex(v: Option<i32>) -> Option<String> {
    v.map(|x| format!("0x{:08X}", x as u32))
}

fn num<T: ToString>(v: Option<T>) -> String {
    v.map_or_else(|| "null".into(), |x| x.to_string())
}

fn descriptor_name(d: Descriptor) -> &'static str {
    match d {
        Descriptor::Extensible => "WAVEFORMATEXTENSIBLE",
        Descriptor::WaveFormatEx => "WAVEFORMATEX",
    }
}

fn priority_code(r: Result<(), ErrorCode>) -> String {
    match r {
        Ok(()) => "ok".into(),
        Err(ErrorCode::Win32(code)) => format!("error {code}"),
        Err(other) => format!("error {other}"),
    }
}

fn mmcss_text(m: Option<PriorityRecord>) -> (Option<String>, Option<String>) {
    match m {
        None => (None, None),
        Some(m) => (
            Some(priority_code(m.registered)),
            m.reverted.map(priority_code),
        ),
    }
}

impl MatrixReport {
    pub fn to_json(&self) -> String {
        let d = &self.device;
        let mut o = String::new();
        o.push_str("{\n  \"report\": \"qybit-device-matrix\",\n");
        let _ = writeln!(o, "  \"backend\": {},", json_str(&d.backend));
        let _ = writeln!(
            o,
            "  \"device\": {{\"id\": {}, \"name\": {}, \"interface\": {}, \"driver_version\": {}}},",
            json_str(&d.id),
            json_opt_str(&d.name),
            json_opt_str(&d.interface_name),
            json_opt_str(&d.driver_version)
        );
        let _ = writeln!(o, "  \"os\": {},", json_opt_str(&d.os));
        o.push_str(
            "  \"note\": \"记录请求格式与独占协商结果；接口成功与播放统计只证明兼容与运行，不证明 DAC 输入样本逐位一致\",\n",
        );
        o.push_str("  \"entries\": [\n");
        for (i, e) in self.entries.iter().enumerate() {
            let it = &e.item;
            let rq = e.requested.as_ref();
            let (reg, rev) = mmcss_text(e.mmcss);
            let _ = write!(
                o,
                "    {{\"rate\": {}, \"channels\": {}, \"container_bits\": {}, \"valid_bits\": {}, \"descriptor\": {}, ",
                it.rate,
                it.channels,
                it.container_bits,
                it.valid_bits,
                json_str(descriptor_name(it.descriptor))
            );
            let _ = write!(
                o,
                "\"requested\": {}, ",
                match rq {
                    None => "null".to_string(),
                    Some(r) => format!(
                        "{{\"wFormatTag\": {}, \"cbSize\": {}, \"nSamplesPerSec\": {}, \"nChannels\": {}, \"wBitsPerSample\": {}, \"wValidBitsPerSample\": {}, \"nBlockAlign\": {}, \"nAvgBytesPerSec\": {}, \"dwChannelMask\": {}, \"SubFormat\": {}}}",
                        r.format_tag,
                        r.cb_size,
                        r.samples_per_sec,
                        r.channels,
                        r.bits_per_sample,
                        num(r.valid_bits),
                        r.block_align,
                        r.avg_bytes_per_sec,
                        num(r.channel_mask),
                        json_opt_str(&r.sub_format)
                    ),
                }
            );
            let _ = write!(
                o,
                "\"is_format_supported\": {}, \"initialize\": {}, \"aligned_retry\": {}, \"buffer_frames\": {}, \"period_hns\": {}, \"mmcss_register\": {}, \"mmcss_revert\": {}, ",
                json_opt_str(&hex(e.is_format_supported)),
                json_opt_str(&hex(e.initialize)),
                json_opt_str(&hex(e.aligned_retry)),
                num(e.buffer_frames),
                num(e.period_hns),
                json_opt_str(&reg),
                json_opt_str(&rev)
            );
            match &e.playback {
                Playback::NotRun => o.push_str("\"playback\": null, "),
                Playback::Played {
                    end,
                    valid_frames,
                    expected_frames,
                    tail_fill_frames,
                    drain_frames,
                    underrun_events,
                    underrun_frames,
                } => {
                    let _ = write!(
                        o,
                        "\"playback\": {{\"end\": {}, \"valid_frames\": {valid_frames}, \"expected_frames\": {expected_frames}, \"tail_fill_frames\": {tail_fill_frames}, \"drain_frames\": {drain_frames}, \"underrun_events\": {underrun_events}, \"underrun_frames\": {underrun_frames}}}, ",
                        json_str(end)
                    );
                }
            }
            let errs: Vec<String> = e.errors.iter().map(|s| json_str(s)).collect();
            let _ = write!(o, "\"errors\": [{}]}}", errs.join(", "));
            o.push_str(if i + 1 < self.entries.len() {
                ",\n"
            } else {
                "\n"
            });
        }
        o.push_str("  ],\n");
        match &self.sequential {
            None => o.push_str("  \"sequential\": null\n"),
            Some(s) => {
                let list = |v: &[u64]| v.iter().map(u64::to_string).collect::<Vec<_>>().join(", ");
                let _ = writeln!(
                    o,
                    "  \"sequential\": {{\"rates\": [{}], \"streams\": {}, \"stream_valid_frames\": [{}], \"expected_frames\": [{}], \"ok\": {}, \"detail\": {}}}",
                    s.rates
                        .iter()
                        .map(u32::to_string)
                        .collect::<Vec<_>>()
                        .join(", "),
                    s.streams,
                    list(&s.stream_valid_frames),
                    list(&s.expected_frames),
                    s.ok,
                    json_str(&s.detail)
                );
            }
        }
        o.push_str("}\n");
        o
    }

    pub fn to_csv(&self) -> String {
        fn cell(s: &str) -> String {
            if s.contains([',', '"', '\n', '\r']) {
                format!("\"{}\"", s.replace('"', "\"\""))
            } else {
                s.to_string()
            }
        }
        let opt = |s: &Option<String>| s.clone().unwrap_or_default();
        let n = |v: Option<String>| v.unwrap_or_default();
        let d = &self.device;
        let mut o = String::from(
            "device_id,device_name,interface,driver_version,os,backend,rate,channels,container_bits,valid_bits,descriptor,wFormatTag,cbSize,nBlockAlign,nAvgBytesPerSec,wValidBitsPerSample,dwChannelMask,SubFormat,is_format_supported,initialize,aligned_retry,buffer_frames,period_hns,mmcss_register,mmcss_revert,played,end,valid_frames,expected_frames,tail_fill_frames,drain_frames,underrun_events,underrun_frames,errors\n",
        );
        for e in &self.entries {
            let it = &e.item;
            let rq = e.requested.as_ref();
            let (reg, rev) = mmcss_text(e.mmcss);
            let (played, end, vf, ef, tf, df, ue, uf) = match &e.playback {
                Playback::NotRun => (
                    "false".to_string(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                ),
                Playback::Played {
                    end,
                    valid_frames,
                    expected_frames,
                    tail_fill_frames,
                    drain_frames,
                    underrun_events,
                    underrun_frames,
                } => (
                    "true".to_string(),
                    end.clone(),
                    valid_frames.to_string(),
                    expected_frames.to_string(),
                    tail_fill_frames.to_string(),
                    drain_frames.to_string(),
                    underrun_events.to_string(),
                    underrun_frames.to_string(),
                ),
            };
            let row = [
                d.id.clone(),
                opt(&d.name),
                opt(&d.interface_name),
                opt(&d.driver_version),
                opt(&d.os),
                d.backend.clone(),
                it.rate.to_string(),
                it.channels.to_string(),
                it.container_bits.to_string(),
                it.valid_bits.to_string(),
                descriptor_name(it.descriptor).to_string(),
                n(rq.map(|r| r.format_tag.to_string())),
                n(rq.map(|r| r.cb_size.to_string())),
                n(rq.map(|r| r.block_align.to_string())),
                n(rq.map(|r| r.avg_bytes_per_sec.to_string())),
                n(rq.and_then(|r| r.valid_bits).map(|v| v.to_string())),
                n(rq.and_then(|r| r.channel_mask).map(|v| v.to_string())),
                n(rq.and_then(|r| r.sub_format.clone())),
                n(hex(e.is_format_supported)),
                n(hex(e.initialize)),
                n(hex(e.aligned_retry)),
                n(e.buffer_frames.map(|v| v.to_string())),
                n(e.period_hns.map(|v| v.to_string())),
                n(reg),
                n(rev),
                played,
                end,
                vf,
                ef,
                tf,
                df,
                ue,
                uf,
                e.errors.join(" | "),
            ];
            let cells: Vec<String> = row.iter().map(|s| cell(s)).collect();
            o.push_str(&cells.join(","));
            o.push('\n');
        }
        o
    }

    /// 写出 `qybit-device-matrix.json` 与 `.csv`，返回两个路径。
    pub fn write(&self, dir: &Path) -> std::io::Result<(PathBuf, PathBuf)> {
        std::fs::create_dir_all(dir)?;
        let json = dir.join("qybit-device-matrix.json");
        let csv = dir.join("qybit-device-matrix.csv");
        std::fs::write(&json, self.to_json())?;
        std::fs::write(&csv, self.to_csv())?;
        Ok((json, csv))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::fake_backend;

    fn work_dir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("qybit-matrix-test-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    #[test]
    fn matrix_has_60_items_in_fixed_order() {
        let items = matrix_items();
        assert_eq!(items.len(), 60);
        assert_eq!(
            items
                .iter()
                .filter(|i| i.descriptor == Descriptor::WaveFormatEx)
                .count(),
            12
        );
        assert!(
            items
                .iter()
                .filter(|i| i.descriptor == Descriptor::WaveFormatEx)
                .all(|i| (i.container_bits, i.valid_bits) == (16, 16))
        );
        for rate in RATES {
            for ch in [1, 2] {
                for (c, v) in LAYOUTS {
                    assert!(items.iter().any(|i| i.rate == rate
                        && i.channels == ch
                        && (i.container_bits, i.valid_bits) == (c, v)
                        && i.descriptor == Descriptor::Extensible));
                }
            }
        }
        assert_eq!(
            items[0],
            MatrixItem {
                rate: 44_100,
                channels: 1,
                container_bits: 16,
                valid_bits: 16,
                descriptor: Descriptor::Extensible
            }
        );
    }

    #[test]
    fn requested_fields_independent_expected() {
        let r = Requested::of(&MatrixItem {
            rate: 96_000,
            channels: 2,
            container_bits: 32,
            valid_bits: 24,
            descriptor: Descriptor::Extensible,
        });
        assert_eq!(
            r,
            Some(Requested {
                format_tag: 0xFFFE,
                cb_size: 22,
                samples_per_sec: 96_000,
                channels: 2,
                bits_per_sample: 32,
                valid_bits: Some(24),
                block_align: 8,
                avg_bytes_per_sec: 768_000,
                channel_mask: Some(3),
                sub_format: Some("00000001-0000-0010-8000-00AA00389B71".into()),
            })
        );
        let r = Requested::of(&MatrixItem {
            rate: 44_100,
            channels: 1,
            container_bits: 16,
            valid_bits: 16,
            descriptor: Descriptor::WaveFormatEx,
        });
        assert_eq!(
            r,
            Some(Requested {
                format_tag: 1,
                cb_size: 0,
                samples_per_sec: 44_100,
                channels: 1,
                bits_per_sample: 16,
                valid_bits: None,
                block_align: 2,
                avg_bytes_per_sec: 88_200,
                channel_mask: None,
                sub_format: None,
            })
        );
    }

    /// 假设备只支持 16/16 与 32/24、只到 96 kHz：其余项记为不支持且不播放；
    /// 支持项经真实核心、解码与 pack 完整播放；顺序换曲两次独立打开。
    #[test]
    fn fake_matrix_records_capabilities_and_playback() {
        let dir = work_dir("fake");
        let Ok(mut res) = HostResources::new() else {
            panic!("res");
        };
        let supported = [(16u8, 16u8), (32, 24)];
        let items = matrix_items();
        let entries = match run_matrix(&items, &mut res, &dir, |item| {
            let mut b = fake_backend(if item.rate <= 96_000 { &supported } else { &[] });
            b.negotiator.forced = Some(((item.container_bits, item.valid_bits), item.descriptor));
            b
        }) {
            Ok(e) => e,
            Err(e) => panic!("{e}"),
        };
        assert_eq!(entries.len(), 60);
        for e in &entries {
            let ok = e.item.rate <= 96_000
                && supported.contains(&(e.item.container_bits, e.item.valid_bits));
            if ok {
                assert!(e.supported() && e.played_ok(), "{e:?}");
                assert_eq!(e.mmcss.map(|m| m.reverted), Some(Some(Ok(()))));
                assert_eq!(e.buffer_frames, Some(e.item.rate / 100));
            } else {
                assert_eq!(
                    e.is_format_supported,
                    Some(crate::fake::AUDCLNT_E_UNSUPPORTED_FORMAT),
                    "{e:?}"
                );
                assert_eq!(e.playback, Playback::NotRun);
                assert!(!e.errors.is_empty());
            }
        }
        let mut b = fake_backend(&supported);
        let seq = match run_sequential(&entries, &mut res, &dir, &mut b) {
            Ok(Some(s)) => s,
            other => panic!("{other:?}"),
        };
        assert!(seq.ok, "{seq:?}");
        assert_eq!(seq.rates, vec![44_100, 48_000]);
        assert_eq!(seq.streams, 2);
        assert_eq!(seq.stream_valid_frames, seq.expected_frames);
        assert_eq!(seq.expected_frames, vec![13_230, 14_400 + 4_800]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn report_is_deterministic_and_well_formed() {
        let item = MatrixItem {
            rate: 48_000,
            channels: 2,
            container_bits: 16,
            valid_bits: 16,
            descriptor: Descriptor::Extensible,
        };
        let report = MatrixReport {
            device: DeviceMeta {
                id: "{0.0.0.00000000}.{abc}".into(),
                name: Some("扬声器 \"USB\", DAC".into()),
                interface_name: None,
                driver_version: None,
                os: Some("Windows 11".into()),
                backend: backend_version(),
            },
            entries: vec![
                MatrixEntry {
                    item,
                    requested: Requested::of(&item),
                    is_format_supported: Some(0),
                    initialize: Some(0),
                    aligned_retry: None,
                    buffer_frames: Some(480),
                    period_hns: Some(100_000),
                    mmcss: Some(PriorityRecord {
                        label: crate::MMCSS_LABEL,
                        registered: Ok(()),
                        reverted: Some(Ok(())),
                    }),
                    playback: Playback::Played {
                        end: "Drained".into(),
                        valid_frames: 14_400,
                        expected_frames: 14_400,
                        tail_fill_frames: 0,
                        drain_frames: 960,
                        underrun_events: 0,
                        underrun_frames: 0,
                    },
                    errors: vec![],
                },
                MatrixEntry {
                    item: MatrixItem {
                        valid_bits: 16,
                        container_bits: 16,
                        descriptor: Descriptor::WaveFormatEx,
                        ..item
                    },
                    requested: None,
                    is_format_supported: Some(0x8889_0008_u32 as i32),
                    initialize: None,
                    aligned_retry: None,
                    buffer_frames: None,
                    period_hns: None,
                    mmcss: None,
                    playback: Playback::NotRun,
                    errors: vec!["设备不支持该格式".into()],
                },
            ],
            sequential: None,
        };
        let json = report.to_json();
        assert_eq!(json, report.to_json());
        // 括号在字符串外配平。
        let (mut depth, mut in_str, mut esc) = (0i32, false, false);
        for ch in json.chars() {
            match (in_str, esc, ch) {
                (true, true, _) => esc = false,
                (true, false, '\\') => esc = true,
                (true, false, '"') => in_str = false,
                (false, _, '"') => in_str = true,
                (false, _, '{' | '[') => depth += 1,
                (false, _, '}' | ']') => {
                    depth -= 1;
                    assert!(depth >= 0);
                }
                _ => {}
            }
        }
        assert_eq!(depth, 0);
        assert!(!in_str);
        assert!(json.contains("\"name\": \"扬声器 \\\"USB\\\", DAC\""));
        assert!(json.contains("\"is_format_supported\": \"0x88890008\""));
        assert!(json.contains("\"SubFormat\": \"00000001-0000-0010-8000-00AA00389B71\""));
        assert!(json.contains("\"playback\": null"));
        assert!(
            !json.to_lowercase().contains("bit-perfect\":"),
            "不得出现 bit-perfect 结论字段"
        );

        let csv = report.to_csv();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 3);
        let header_cols = lines[0].split(',').count();
        assert_eq!(header_cols, 34);
        assert!(
            lines[1]
                .starts_with("{0.0.0.00000000}.{abc},\"扬声器 \"\"USB\"\", DAC\",,,Windows 11,")
        );
        assert!(lines[1].contains(",48000,2,16,16,WAVEFORMATEXTENSIBLE,65534,22,4,192000,16,3,00000001-0000-0010-8000-00AA00389B71,0x00000000,0x00000000,,480,100000,ok,ok,true,Drained,14400,14400,0,960,0,0,"));
        assert!(
            lines[2]
                .contains(",WAVEFORMATEX,,,,,,,,0x88890008,,,,,,,false,,,,,,,,设备不支持该格式")
        );
    }
}
