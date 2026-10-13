//! 数字回环的对齐、比对与报告（M2 T4）。
//!
//! 两层回环共用：
//! - A 层（`file`）：alsa-lib `file` 插件把提交给 alsa-lib 的字节写入文件（slave 为 `null`），
//!   只证明软件路径提交正确，不经过任何驱动；
//! - B 层（`snd-aloop`）：内核虚拟声卡，由 `hw:Loopback,0,N` 播放、`hw:Loopback,1,N` 回录，
//!   经过真实 ALSA 内核路径，但仍是虚拟回环，不证明真实 DAC 输入逐位一致。
//!
//! 对齐：测试先播放一段同格式的伪随机同步曲目，再无缝衔接待测曲目；在回录中找到同步序列
//! 的期望设备字节，其后字节必须与待测曲目的期望设备字节逐字节一致——包括曲目真实的前导静音
//! 与尾帧，不删除任何静音来凑对齐。

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use qy_pcm_format::alsa::{AlsaFormat, HwApplied, HwRequest};

/// 回环层。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// alsa-lib `file` 插件（软件路径）。
    File,
    /// snd-aloop 内核虚拟声卡。
    Aloop,
}

impl Layer {
    pub fn name(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Aloop => "snd-aloop",
        }
    }

    pub fn note(self) -> &'static str {
        match self {
            Self::File => {
                "A 层：alsa-lib file 插件（slave null）记录提交给 alsa-lib 的字节，只证明软件路径提交正确，不经过驱动，不证明设备端 bit-perfect"
            }
            Self::Aloop => {
                "B 层：snd-aloop 内核虚拟声卡回录，经过 ALSA 内核路径；虚拟回环不证明真实 DAC 输入样本逐位一致"
            }
        }
    }
}

/// 一个回环用例。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopCase {
    pub rate: u32,
    pub channels: u16,
    pub format: AlsaFormat,
    /// 源有效位 N。
    pub source_bits: u8,
}

/// 比对结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 同步后的字节与期望逐字节一致。
    Match,
    /// 第 `at` 字节（相对待测曲目起点）起不一致，或回录不足。
    Mismatch { at: usize, detail: String },
    /// 回录中找不到同步序列。
    SyncNotFound,
    /// N > V：设备选择阶段按要求拒绝（期望结果）。
    Rejected(String),
    /// 打开或播放失败（非期望）。
    Failed(String),
}

impl Verdict {
    /// 是否为该用例的期望结论（N ≤ V 须逐字节一致，N > V 须被拒绝）。
    pub fn ok_for(&self, case: &LoopCase) -> bool {
        if case.source_bits > case.format.valid_bits() {
            matches!(self, Self::Rejected(_))
        } else {
            matches!(self, Self::Match)
        }
    }
}

/// 一项回环记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopEntry {
    pub case: LoopCase,
    pub request: Option<HwRequest>,
    pub applied: Option<HwApplied>,
    pub captured_bytes: usize,
    pub sync_offset: Option<usize>,
    pub compared_bytes: usize,
    pub verdict: Verdict,
}

/// 在回录中查找同步序列的期望字节，返回起始偏移。
pub fn find_sync(captured: &[u8], sync: &[u8]) -> Option<usize> {
    if sync.is_empty() || captured.len() < sync.len() {
        return None;
    }
    let first = sync[0];
    (0..=captured.len() - sync.len())
        .filter(|&i| captured[i] == first)
        .find(|&i| &captured[i..i + sync.len()] == sync)
}

/// 对齐并比对：同步序列之后必须紧接 `expected`。
pub fn align_and_compare(
    captured: &[u8],
    sync: &[u8],
    expected: &[u8],
) -> (Option<usize>, Verdict) {
    let Some(off) = find_sync(captured, sync) else {
        return (None, Verdict::SyncNotFound);
    };
    let start = off + sync.len();
    let got = &captured[start..];
    if let Some(at) = got.iter().zip(expected).position(|(a, b)| a != b) {
        return (
            Some(off),
            Verdict::Mismatch {
                at,
                detail: format!("期望 0x{:02X}，回录 0x{:02X}", expected[at], got[at]),
            },
        );
    }
    if got.len() < expected.len() {
        return (
            Some(off),
            Verdict::Mismatch {
                at: got.len(),
                detail: format!("回录不足：{} / {} 字节", got.len(), expected.len()),
            },
        );
    }
    (Some(off), Verdict::Match)
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

/// 回环报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopReport {
    pub layer: Layer,
    pub device: String,
    pub entries: Vec<LoopEntry>,
    /// 附加检查（无缝衔接、换格式重开等）：名称与是否通过、说明。
    pub extra: Vec<(String, bool, String)>,
}

impl LoopReport {
    pub fn passed(&self) -> bool {
        self.entries.iter().all(|e| e.verdict.ok_for(&e.case)) && self.extra.iter().all(|x| x.1)
    }

    pub fn to_json(&self) -> String {
        let mut o = String::from("{\n");
        let _ = writeln!(o, "  \"report\": \"qybit-alsa-loopback\",");
        let _ = writeln!(o, "  \"layer\": {},", json_str(self.layer.name()));
        let _ = writeln!(o, "  \"device\": {},", json_str(&self.device));
        let _ = writeln!(o, "  \"note\": {},", json_str(self.layer.note()));
        let _ = writeln!(o, "  \"passed\": {},", self.passed());
        o.push_str("  \"entries\": [\n");
        for (i, e) in self.entries.iter().enumerate() {
            let c = &e.case;
            let (verdict, detail) = match &e.verdict {
                Verdict::Match => ("match", String::new()),
                Verdict::Mismatch { at, detail } => ("mismatch", format!("字节 {at}：{detail}")),
                Verdict::SyncNotFound => ("sync_not_found", String::new()),
                Verdict::Rejected(m) => ("rejected", m.clone()),
                Verdict::Failed(m) => ("failed", m.clone()),
            };
            let _ = write!(
                o,
                "    {{\"rate\": {}, \"channels\": {}, \"format\": {}, \"source_bits\": {}, ",
                c.rate,
                c.channels,
                json_str(c.format.name()),
                c.source_bits
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
                        "\"applied\": {{\"format_code\": {}, \"rate\": \"{}/{}\", \"channels\": {}, \"significant_bits\": {}}}, ",
                        a.format_code,
                        a.rate_num,
                        a.rate_den,
                        a.channels,
                        a.significant_bits
                            .map_or("null".to_string(), |b| b.to_string())
                    );
                }
                None => o.push_str("\"applied\": null, "),
            }
            let _ = write!(
                o,
                "\"captured_bytes\": {}, \"sync_offset\": {}, \"compared_bytes\": {}, \"verdict\": {}, \"detail\": {}, \"ok\": {}}}",
                e.captured_bytes,
                e.sync_offset.map_or("null".to_string(), |x| x.to_string()),
                e.compared_bytes,
                json_str(verdict),
                json_str(&detail),
                e.verdict.ok_for(c)
            );
            o.push_str(if i + 1 == self.entries.len() {
                "\n"
            } else {
                ",\n"
            });
        }
        o.push_str("  ],\n  \"extra\": [\n");
        for (i, (name, ok, detail)) in self.extra.iter().enumerate() {
            let _ = write!(
                o,
                "    {{\"name\": {}, \"ok\": {ok}, \"detail\": {}}}",
                json_str(name),
                json_str(detail)
            );
            o.push_str(if i + 1 == self.extra.len() {
                "\n"
            } else {
                ",\n"
            });
        }
        o.push_str("  ]\n}\n");
        o
    }

    /// 写出 `qybit-alsa-loopback-<层>.json`。
    pub fn write(&self, dir: &Path) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("qybit-alsa-loopback-{}.json", self.layer.name()));
        std::fs::write(&path, self.to_json())?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_alignment_keeps_leading_silence() {
        let sync = [7u8, 1, 9, 9, 3];
        // 回录：前置静音 + 同步 + 待测（含 3 字节真实前导静音）+ 尾部静音。
        let expected = [0u8, 0, 0, 5, 6, 0x80];
        let mut cap = vec![0u8; 11];
        cap.extend_from_slice(&sync);
        cap.extend_from_slice(&expected);
        cap.extend_from_slice(&[0; 4]);
        assert_eq!(
            align_and_compare(&cap, &sync, &expected),
            (Some(11), Verdict::Match)
        );
        // 丢掉一个前导静音字节：必须报不一致，不得“对齐到第一个非零字节”。
        let mut bad = vec![0u8; 11];
        bad.extend_from_slice(&sync);
        bad.extend_from_slice(&expected[1..]);
        assert!(matches!(
            align_and_compare(&bad, &sync, &expected).1,
            Verdict::Mismatch { .. }
        ));
        // 回录不足与找不到同步。
        let short = [&sync[..], &expected[..4]].concat();
        assert!(matches!(
            align_and_compare(&short, &sync, &expected).1,
            Verdict::Mismatch { at: 4, .. }
        ));
        assert_eq!(
            align_and_compare(&expected, &sync, &expected),
            (None, Verdict::SyncNotFound)
        );
        assert_eq!(find_sync(&[1, 2], &[]), None);
    }

    #[test]
    fn verdict_expectation_depends_on_n_vs_v() {
        let case = |format, source_bits| LoopCase {
            rate: 48_000,
            channels: 2,
            format,
            source_bits,
        };
        assert!(Verdict::Match.ok_for(&case(AlsaFormat::S24Le, 24)));
        assert!(!Verdict::Rejected(String::new()).ok_for(&case(AlsaFormat::S24Le, 24)));
        assert!(Verdict::Rejected(String::new()).ok_for(&case(AlsaFormat::S16Le, 24)));
        assert!(!Verdict::Match.ok_for(&case(AlsaFormat::S16Le, 24)));
    }
}
