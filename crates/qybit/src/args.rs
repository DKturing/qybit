//! 命令行参数解析（与平台无关）。

use std::path::PathBuf;

pub const USAGE: &str = "\
用法:
  qybit list-devices                         列出可用输出设备（Windows：端点 ID 与名称；Linux：hw 设备名）
  qybit --device <设备> [--container 16|24|32] <文件>...
                                             按顺序播放 WAV/FLAC（按文件头识别）；Ctrl+C 停止并输出报告
  qybit matrix --device <设备> [--out <目录>]
                                             运行设备格式矩阵，写出 JSON/CSV 报告
  qybit decode <输入> <输出.wav>              诊断：自有解码 + pack 写成标准 WAV，可用任何播放器试听
  qybit tone <输出.wav> [--rate 44100] [--bits 16|24|32] [--seconds 5] [--freq 1000]
                                             诊断：写一个正弦测试音 WAV
  qybit --version | --help

设备：Windows 为 WASAPI 端点 ID（独占模式）；Linux 为 ALSA hw:卡,设备 或 hw:CARD=名称,DEV=n
（只接受 hw 设备，拒绝 plughw/default/dmix/pipewire/pulse 等会转换或混音的设备）。
PowerShell 会把未加引号的 {…} 当成脚本块，端点 ID 要加单引号：
  qybit --device '{0.0.0.00000000}.{…}' a.wav

--container：只请求该容器位宽的设备格式（目前只用于 Windows）。驱动对某种格式回报支持、实际播放
却是杂音时用来排查，例如 24 位源用 --container 32 避开 3 字节紧凑格式；样本值不变。

退出码：0 完整播放；2 播放完成但有欠载；1 失败（含设备不支持、打开失败、设备错误、中断）。";

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    ListDevices,
    Play {
        device: String,
        files: Vec<PathBuf>,
        container: Option<u8>,
    },
    Matrix {
        device: String,
        out: PathBuf,
    },
    Decode {
        input: PathBuf,
        out: PathBuf,
    },
    Tone {
        out: PathBuf,
        rate: u32,
        bits: u8,
        seconds: f64,
        freq: f64,
    },
    Version,
    Help,
}

/// 解析参数（不含程序名）。
pub fn parse(args: &[String]) -> Result<Command, String> {
    let mut it = args.iter().map(String::as_str);
    match it.next() {
        None | Some("-h" | "--help" | "help") => Ok(Command::Help),
        Some("--version" | "-V") => Ok(Command::Version),
        Some("list-devices") => match it.next() {
            None => Ok(Command::ListDevices),
            Some(x) => Err(format!("list-devices 不接受参数：{x}")),
        },
        Some("decode") => match (it.next(), it.next(), it.next()) {
            (Some(i), Some(o), None) if !i.starts_with('-') && !o.starts_with('-') => {
                Ok(Command::Decode {
                    input: PathBuf::from(i),
                    out: PathBuf::from(o),
                })
            }
            _ => Err("decode 需要两个参数：<输入文件> <输出.wav>".into()),
        },
        Some("tone") => {
            let out = match it.next() {
                Some(o) if !o.starts_with('-') => PathBuf::from(o),
                _ => return Err("tone 需要输出文件：qybit tone <输出.wav>".into()),
            };
            let (mut rate, mut bits, mut seconds, mut freq) = (44_100u32, 16u8, 5.0f64, 1000.0f64);
            while let Some(a) = it.next() {
                let v = value(&mut it, a)?;
                let bad = || format!("{a} 的值无效：{v}");
                match a {
                    "--rate" => rate = v.parse().ok().filter(|&r| r > 0).ok_or_else(bad)?,
                    "--bits" => {
                        bits = match v.as_str() {
                            "16" => 16,
                            "24" => 24,
                            "32" => 32,
                            _ => return Err(bad()),
                        }
                    }
                    "--seconds" => {
                        seconds = v
                            .parse()
                            .ok()
                            .filter(|&x: &f64| x > 0.0 && x <= 600.0)
                            .ok_or_else(bad)?
                    }
                    "--freq" => {
                        freq = v
                            .parse()
                            .ok()
                            .filter(|&x: &f64| x > 0.0 && x < f64::from(rate) / 2.0)
                            .ok_or_else(bad)?
                    }
                    x => return Err(format!("未知参数：{x}")),
                }
            }
            Ok(Command::Tone {
                out,
                rate,
                bits,
                seconds,
                freq,
            })
        }
        Some("matrix") => {
            let mut device = None;
            let mut out = None;
            while let Some(a) = it.next() {
                match a {
                    "--device" => device = Some(device_value(&mut it)?),
                    "--out" => out = Some(PathBuf::from(value(&mut it, "--out")?)),
                    x => return Err(format!("未知参数：{x}")),
                }
            }
            Ok(Command::Matrix {
                device: device.ok_or("matrix 需要 --device <端点ID>")?,
                out: out.unwrap_or_else(|| PathBuf::from("qybit-report")),
            })
        }
        Some("--device") => {
            let device = device_value(&mut it)?;
            let mut files = Vec::new();
            let mut container = None;
            while let Some(a) = it.next() {
                if a == "--container" {
                    container = Some(match value(&mut it, "--container")?.as_str() {
                        "16" => 16,
                        "24" => 24,
                        "32" => 32,
                        x => return Err(format!("--container 只接受 16、24 或 32，收到 {x}")),
                    });
                } else if a.starts_with('-') {
                    return Err(format!("未知参数：{a}"));
                } else {
                    files.push(PathBuf::from(a));
                }
            }
            if files.is_empty() {
                return Err("至少需要一个音频文件（WAV 或 FLAC）".into());
            }
            Ok(Command::Play {
                device,
                files,
                container,
            })
        }
        Some(x) if !x.starts_with('-') => Err(format!(
            "未指定设备：播放需要 --device <端点ID>（可用 qybit list-devices 查看），收到 {x}"
        )),
        Some(x) => Err(format!("未知参数：{x}")),
    }
}

/// `--device` 的值。值看起来是音频文件时说明设备 ID 没有传进来：最常见的原因是 PowerShell 把
/// 未加引号的 `{…}`（WASAPI 端点 ID 的写法）当成脚本块吞掉了，直接给出原因而不是“找不到设备”。
fn device_value<'a>(it: &mut impl Iterator<Item = &'a str>) -> Result<String, String> {
    let v = value(it, "--device")?;
    let lower = v.to_ascii_lowercase();
    if [".wav", ".flac", ".fla"].iter().any(|e| lower.ends_with(e)) {
        return Err(format!(
            "--device 的值 {v} 看起来是音频文件，设备 ID 没有传进来。\
             在 PowerShell 中，未加引号的 {{…}} 会被当成脚本块，请给端点 ID 加单引号，例如：\
             qybit --device '{{0.0.0.00000000}}.{{…}}' a.wav"
        ));
    }
    Ok(v)
}

fn value<'a>(it: &mut impl Iterator<Item = &'a str>, flag: &str) -> Result<String, String> {
    match it.next() {
        Some(v) if !v.is_empty() && !v.starts_with("--") => Ok(v.to_string()),
        _ => Err(format!("{flag} 需要一个值")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(a: &[&str]) -> Result<Command, String> {
        parse(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn parses_commands() {
        assert_eq!(p(&[]), Ok(Command::Help));
        assert_eq!(p(&["--version"]), Ok(Command::Version));
        assert_eq!(p(&["list-devices"]), Ok(Command::ListDevices));
        assert_eq!(
            p(&["--device", "{0.0.0.00000000}.{x}", "a.wav", "b.wav"]),
            Ok(Command::Play {
                device: "{0.0.0.00000000}.{x}".into(),
                files: vec!["a.wav".into(), "b.wav".into()],
                container: None,
            })
        );
        assert_eq!(
            p(&["--device", "id", "a.wav", "--container", "32", "b.flac"]),
            Ok(Command::Play {
                device: "id".into(),
                files: vec!["a.wav".into(), "b.flac".into()],
                container: Some(32),
            })
        );
        assert!(p(&["--device", "id", "--container", "20", "a.wav"]).is_err());
        assert_eq!(
            p(&["matrix", "--device", "id"]),
            Ok(Command::Matrix {
                device: "id".into(),
                out: "qybit-report".into()
            })
        );
        assert_eq!(
            p(&["matrix", "--out", "r", "--device", "id"]),
            Ok(Command::Matrix {
                device: "id".into(),
                out: "r".into()
            })
        );
    }

    /// PowerShell 吞掉未加引号的 `{…}` 后，文件名落到 `--device` 的位置：给出原因。
    #[test]
    fn device_swallowed_by_powershell_is_explained() {
        for a in [
            &["--device", ".\\a.wav", ".\\b.flac"][..],
            &["matrix", "--device", "C:\\music\\B.FLAC"][..],
        ] {
            let r = p(a);
            assert!(
                r.as_ref()
                    .is_err_and(|e| e.contains("单引号") && e.contains("脚本块")),
                "{r:?}"
            );
        }
    }

    #[test]
    fn parses_diagnostics() {
        assert_eq!(
            p(&["decode", "a.wav", "out.wav"]),
            Ok(Command::Decode {
                input: "a.wav".into(),
                out: "out.wav".into()
            })
        );
        assert!(p(&["decode", "a.wav"]).is_err());
        assert_eq!(
            p(&["tone", "t.wav", "--rate", "48000", "--bits", "24"]),
            Ok(Command::Tone {
                out: "t.wav".into(),
                rate: 48_000,
                bits: 24,
                seconds: 5.0,
                freq: 1000.0
            })
        );
        assert!(p(&["tone", "t.wav", "--bits", "20"]).is_err());
        assert!(p(&["tone"]).is_err());
    }

    #[test]
    fn rejects_missing_device_and_bad_args() {
        assert!(p(&["a.wav"]).is_err_and(|e| e.contains("未指定设备")));
        assert!(p(&["--device"]).is_err());
        assert!(p(&["--device", "id"]).is_err_and(|e| e.contains("至少需要一个")));
        assert!(p(&["--device", "id", "--shared", "a.wav"]).is_err());
        assert!(p(&["matrix"]).is_err_and(|e| e.contains("--device")));
        assert!(p(&["matrix", "--device", "--out", "x"]).is_err());
        assert!(p(&["list-devices", "x"]).is_err());
        assert!(p(&["--bogus"]).is_err());
    }
}
