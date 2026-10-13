//! 命令行参数解析（与平台无关）。

use std::path::PathBuf;

pub const USAGE: &str = "\
用法:
  qybit list-devices                         列出可用输出设备（Windows：端点 ID 与名称；Linux：hw 设备名）
  qybit --device <设备> <文件>...            按顺序播放 WAV/FLAC（按文件头识别）
  qybit matrix --device <设备> [--out <目录>]
                                             运行设备格式矩阵，写出 JSON/CSV 报告
  qybit --version | --help

设备：Windows 为 WASAPI 端点 ID（独占模式）；Linux 为 ALSA hw:卡,设备 或 hw:CARD=名称,DEV=n
（只接受 hw 设备，拒绝 plughw/default/dmix/pipewire/pulse 等会转换或混音的设备）。

退出码：0 完整播放；2 播放完成但有欠载；1 失败（含设备不支持、打开失败、设备错误）。";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    ListDevices,
    Play { device: String, files: Vec<PathBuf> },
    Matrix { device: String, out: PathBuf },
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
        Some("matrix") => {
            let mut device = None;
            let mut out = None;
            while let Some(a) = it.next() {
                match a {
                    "--device" => device = Some(value(&mut it, "--device")?),
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
            let device = value(&mut it, "--device")?;
            let mut files = Vec::new();
            for a in it {
                if a.starts_with('-') {
                    return Err(format!("未知参数：{a}"));
                }
                files.push(PathBuf::from(a));
            }
            if files.is_empty() {
                return Err("至少需要一个音频文件（WAV 或 FLAC）".into());
            }
            Ok(Command::Play { device, files })
        }
        Some(x) if !x.starts_with('-') => Err(format!(
            "未指定设备：播放需要 --device <端点ID>（可用 qybit list-devices 查看），收到 {x}"
        )),
        Some(x) => Err(format!("未知参数：{x}")),
    }
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
                files: vec!["a.wav".into(), "b.wav".into()]
            })
        );
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
