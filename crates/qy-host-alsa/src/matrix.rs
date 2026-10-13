//! ALSA 设备格式矩阵与报告（M2 T3）。
//!
//! 矩阵：采样率 44.1/48/88.2/96/176.4/192 kHz × `S16_LE`/`S24_3LE`/`S24_LE`/`S32_LE` ×
//! 单/双声道，共 48 项。每项强制只尝试该格式，用 N = V 的测试 WAV 经真实核心、解码与 pack
//! 播放；设备不接受、设置失败或读回不一致都是能力结果，照实记录。报告为确定性的 JSON
//! 与 CSV（不含时间戳，顺序固定），只记录请求、读回与播放统计，不宣称设备端 bit-perfect。

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use qy_pcm_format::alsa::{AlsaFormat, HwApplied, HwRequest};
use qy_player::render::{ErrorCode, PriorityRecord, RenderEnd};
use qy_player::signal::write_test_wav;
use qy_player::{Backend, HostResources, play};

use crate::attempt::{Attempt, PlaybackReport};

pub const RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
/// 每项测试信号时长（毫秒）。
pub const TEST_MS: u32 = 300;

/// 后端版本描述（写入报告）。
pub fn backend_version() -> String {
    format!(
        "qy-host-alsa {} (ALSA hw RW_INTERLEAVED, alsa-rs 0.12.1)",
        env!("CARGO_PKG_VERSION")
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatrixItem {
    pub rate: u32,
    pub channels: u16,
    pub format: AlsaFormat,
}

impl MatrixItem {
    pub fn frames(&self) -> u64 {
        u64::from(self.rate) * u64::from(TEST_MS) / 1000
    }
}

/// 全部矩阵项（固定顺序）。
pub fn matrix_items() -> Vec<MatrixItem> {
    let mut v = Vec::new();
    for rate in RATES {
        for channels in [1u16, 2] {
            for format in AlsaFormat::ALL {
                v.push(MatrixItem {
                    rate,
                    channels,
                    format,
                });
            }
        }
    }
    v
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Playback {
    NotRun,
    Played {
        end: String,
        valid_frames: u64,
        expected_frames: u64,
        underrun_events: u64,
        underrun_frames: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatrixEntry {
    pub item: MatrixItem,
    /// 设置成功且读回一致。
    pub accepted: bool,
    pub request: Option<HwRequest>,
    pub applied: Option<HwApplied>,
    /// 设置失败的 alsa-lib 调用或读回不一致说明。
    pub detail: Option<String>,
    pub priority: Option<PriorityRecord>,
    pub playback: Playback,
    pub errors: Vec<String>,
}

impl MatrixEntry {
    /// 播放完整：排空结束、有效帧数等于测试信号帧数、无欠载。
    pub fn played_ok(&self) -> bool {
        matches!(&self.playback, Playback::Played { end, valid_frames, expected_frames, underrun_events, .. }
            if end == "Drained" && valid_frames == expected_frames && *underrun_events == 0)
    }
}

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
    pub device: String,
    pub backend: String,
    pub entries: Vec<MatrixEntry>,
    pub sequential: Option<Sequential>,
}

/// 把一次单曲播放报告转换为矩阵结果。
pub fn entry_from_playback(item: MatrixItem, expected: u64, r: &PlaybackReport) -> MatrixEntry {
    let attempt = r
        .streams
        .first()
        .and_then(|s| s.negotiation.chosen_attempt())
        .or_else(|| {
            r.open_failures
                .first()
                .and_then(|f| f.negotiation.attempts.first())
        });
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
            expected_frames: expected,
            underrun_events: rr.underrun_events,
            underrun_frames: rr.underrun_frames,
        },
        None => Playback::NotRun,
    };
    MatrixEntry {
        item,
        accepted: attempt.is_some_and(Attempt::accepted),
        request: attempt.and_then(|a| a.request),
        applied: attempt.and_then(|a| a.applied),
        detail: attempt.and_then(|a| {
            a.error
                .map(|e| e.to_string())
                .or_else(|| a.mismatch.clone())
        }),
        priority: render.map(|rr| rr.priority),
        playback,
        errors,
    }
}

/// 逐项运行矩阵。`backend_for` 为每项构造只尝试该格式的后端。
pub fn run_matrix<B: Backend<Attempt = Attempt>>(
    items: &[MatrixItem],
    res: &mut HostResources,
    dir: &Path,
    mut backend_for: impl FnMut(&MatrixItem) -> B,
) -> std::io::Result<Vec<MatrixEntry>> {
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let path = dir.join(format!(
            "qybit-alsa-matrix-{i:02}-{}-{}ch-{}.wav",
            item.rate,
            item.channels,
            item.format.name()
        ));
        let frames = write_test_wav(
            &path,
            item.rate,
            item.channels,
            item.format.valid_bits(),
            item.frames(),
        )?;
        let mut backend = backend_for(item);
        let report = play(&mut backend, res, std::slice::from_ref(&path));
        out.push(entry_from_playback(*item, frames, &report));
        let _ = std::fs::remove_file(&path);
    }
    Ok(out)
}

/// 顺序换曲：取两个不同采样率、均已播放成功的项，连续播放两首，检查重开设备
/// （两个流）、各流有效帧完整、无欠载。
pub fn run_sequential<B: Backend<Attempt = Attempt>>(
    entries: &[MatrixEntry],
    res: &mut HostResources,
    dir: &Path,
    backend: &mut B,
) -> std::io::Result<Option<Sequential>> {
    let mut picked: Vec<&MatrixEntry> = Vec::new();
    for e in entries {
        if e.played_ok() && picked.iter().all(|p| p.item.rate != e.item.rate) {
            picked.push(e);
        }
        if picked.len() == 2 {
            break;
        }
    }
    if picked.len() < 2 {
        return Ok(None);
    }
    let mut files = Vec::new();
    let mut expected = Vec::new();
    for (k, e) in picked.iter().enumerate() {
        let path = dir.join(format!("qybit-alsa-seq-{k}.wav"));
        let frames = write_test_wav(
            &path,
            e.item.rate,
            e.item.channels,
            e.item.format.valid_bits(),
            e.item.frames(),
        )?;
        files.push(path);
        expected.push(frames);
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
    let ok = r.software_path_intact() && r.streams.len() == 2 && valid == expected;
    Ok(Some(Sequential {
        rates: picked.iter().map(|e| e.item.rate).collect(),
        streams: r.streams.len(),
        stream_valid_frames: valid,
        expected_frames: expected,
        ok,
        detail: if ok {
            "两次独立打开，各流有效帧完整、无欠载".into()
        } else {
            format!("未通过：{}", r.summary().replace('\n', " "))
        },
    }))
}

fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn json_opt<T: ToString>(v: Option<T>) -> String {
    v.map_or_else(|| "null".into(), |x| x.to_string())
}

fn code_text(r: Result<(), ErrorCode>) -> String {
    match r {
        Ok(()) => "ok".into(),
        Err(e) => format!("error {e}"),
    }
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

impl MatrixReport {
    pub fn to_json(&self) -> String {
        let mut o = String::new();
        o.push_str("{\n  \"report\": \"qybit-alsa-device-matrix\",\n");
        let _ = writeln!(o, "  \"backend\": {},", json_str(&self.backend));
        let _ = writeln!(o, "  \"device\": {},", json_str(&self.device));
        o.push_str("  \"note\": \"记录请求格式、alsa-lib 返回与 hw_params_current 读回；接口成功与播放统计只证明兼容与运行，不证明 DAC 输入样本逐位一致\",\n");
        o.push_str("  \"entries\": [\n");
        for (i, e) in self.entries.iter().enumerate() {
            let it = &e.item;
            let _ = write!(
                o,
                "    {{\"rate\": {}, \"channels\": {}, \"format\": {}, \"accepted\": {}, ",
                it.rate,
                it.channels,
                json_str(it.format.name()),
                e.accepted
            );
            match e.request {
                Some(r) => {
                    let _ = write!(
                        o,
                        "\"period_frames\": {}, \"buffer_frames\": {}, ",
                        r.period_frames, r.buffer_frames
                    );
                }
                None => o.push_str("\"period_frames\": null, \"buffer_frames\": null, "),
            }
            match e.applied {
                Some(a) => {
                    let _ = write!(
                        o,
                        "\"applied\": {{\"format_code\": {}, \"rate\": \"{}/{}\", \"channels\": {}, \"period_frames\": {}, \"buffer_frames\": {}, \"resample\": {}, \"significant_bits\": {}}}, ",
                        a.format_code,
                        a.rate_num,
                        a.rate_den,
                        a.channels,
                        a.period_frames,
                        a.buffer_frames,
                        a.resample,
                        json_opt(a.significant_bits)
                    );
                }
                None => o.push_str("\"applied\": null, "),
            }
            let _ = write!(
                o,
                "\"detail\": {}, ",
                e.detail.as_deref().map_or("null".into(), json_str)
            );
            match e.priority {
                Some(p) => {
                    let _ = write!(
                        o,
                        "\"priority\": {{\"label\": {}, \"register\": {}, \"revert\": {}}}, ",
                        json_str(p.label),
                        json_str(&code_text(p.registered)),
                        p.reverted
                            .map_or("null".into(), |r| json_str(&code_text(r)))
                    );
                }
                None => o.push_str("\"priority\": null, "),
            }
            match &e.playback {
                Playback::NotRun => o.push_str("\"played\": false, "),
                Playback::Played {
                    end,
                    valid_frames,
                    expected_frames,
                    underrun_events,
                    underrun_frames,
                } => {
                    let _ = write!(
                        o,
                        "\"played\": true, \"end\": {}, \"valid_frames\": {valid_frames}, \"expected_frames\": {expected_frames}, \"underrun_events\": {underrun_events}, \"underrun_frames\": {underrun_frames}, ",
                        json_str(end)
                    );
                }
            }
            let errs: Vec<String> = e.errors.iter().map(|s| json_str(s)).collect();
            let _ = write!(o, "\"errors\": [{}]}}", errs.join(", "));
            o.push_str(if i + 1 == self.entries.len() {
                "\n"
            } else {
                ",\n"
            });
        }
        o.push_str("  ],\n");
        match &self.sequential {
            None => o.push_str("  \"sequential\": null\n"),
            Some(s) => {
                let _ = writeln!(
                    o,
                    "  \"sequential\": {{\"rates\": {:?}, \"streams\": {}, \"stream_valid_frames\": {:?}, \"expected_frames\": {:?}, \"ok\": {}, \"detail\": {}}}",
                    s.rates,
                    s.streams,
                    s.stream_valid_frames,
                    s.expected_frames,
                    s.ok,
                    json_str(&s.detail)
                );
            }
        }
        o.push_str("}\n");
        o
    }

    pub fn to_csv(&self) -> String {
        let mut o = String::from(
            "device,backend,rate,channels,format,accepted,period_frames,buffer_frames,applied_format_code,applied_rate,applied_channels,applied_period,applied_buffer,resample,significant_bits,detail,priority_register,priority_revert,played,end,valid_frames,expected_frames,underrun_events,underrun_frames,errors\n",
        );
        for e in &self.entries {
            let it = &e.item;
            let a = e.applied;
            let (end, valid, exp, ue, uf, played) = match &e.playback {
                Playback::NotRun => (
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    "false",
                ),
                Playback::Played {
                    end,
                    valid_frames,
                    expected_frames,
                    underrun_events,
                    underrun_frames,
                } => (
                    end.clone(),
                    valid_frames.to_string(),
                    expected_frames.to_string(),
                    underrun_events.to_string(),
                    underrun_frames.to_string(),
                    "true",
                ),
            };
            let row = [
                csv_field(&self.device),
                csv_field(&self.backend),
                it.rate.to_string(),
                it.channels.to_string(),
                it.format.name().to_string(),
                e.accepted.to_string(),
                e.request
                    .map_or(String::new(), |r| r.period_frames.to_string()),
                e.request
                    .map_or(String::new(), |r| r.buffer_frames.to_string()),
                a.map_or(String::new(), |a| a.format_code.to_string()),
                a.map_or(String::new(), |a| format!("{}/{}", a.rate_num, a.rate_den)),
                a.map_or(String::new(), |a| a.channels.to_string()),
                a.map_or(String::new(), |a| a.period_frames.to_string()),
                a.map_or(String::new(), |a| a.buffer_frames.to_string()),
                a.map_or(String::new(), |a| a.resample.to_string()),
                a.and_then(|a| a.significant_bits)
                    .map_or(String::new(), |b| b.to_string()),
                csv_field(e.detail.as_deref().unwrap_or("")),
                e.priority
                    .map_or(String::new(), |p| csv_field(&code_text(p.registered))),
                e.priority
                    .and_then(|p| p.reverted)
                    .map_or(String::new(), |r| csv_field(&code_text(r))),
                played.to_string(),
                end,
                valid,
                exp,
                ue,
                uf,
                csv_field(&e.errors.join(" | ")),
            ];
            o.push_str(&row.join(","));
            o.push('\n');
        }
        o
    }

    /// 写出 `qybit-alsa-device-matrix.json` 与 `.csv`。
    pub fn write(&self, dir: &Path) -> std::io::Result<(PathBuf, PathBuf)> {
        std::fs::create_dir_all(dir)?;
        let json = dir.join("qybit-alsa-device-matrix.json");
        let csv = dir.join("qybit-alsa-device-matrix.csv");
        std::fs::write(&json, self.to_json())?;
        std::fs::write(&csv, self.to_csv())?;
        Ok((json, csv))
    }
}
