//! 命令行参数解析（与平台无关）。

use std::path::PathBuf;

pub const USAGE: &str = "\
用法:
  qybit list-devices                         列出可用输出设备（Windows：端点 ID 与名称；Linux：hw 设备名）
  qybit --device <设备> [--container 16|24|32] [--mono-to-stereo] [--float-to-int]
        [--volume <N>%|off] [--volume-control <控制>] <文件>...
                                             按顺序播放 WAV/FLAC/AIFF（按文件头识别）；Ctrl+C 停止并输出报告
  qybit matrix --device <设备> [--out <目录>]
                                             运行设备格式矩阵，写出 JSON/CSV 报告
  qybit mixer --device <设备>                Linux：列出声卡的硬件混音器（静音开关、音量、dB），只读
  qybit decode <输入> <输出.wav>              诊断：自有解码 + pack 写成标准 WAV，可用任何播放器试听
  qybit tone <输出.wav|.aiff> [--rate 44100] [--bits 16|24|32 | --float 32|64] [--seconds 5] [--freq 1000]
                                             诊断：写一个双声道正弦测试音（约 −12 dBFS）；输出文件扩展名为
                                             .aif/.aiff 时写 AIFF（整数），否则写 WAV；--float 写 IEEE 浮点 WAV
  qybit --version | --help

设备：Windows 为 WASAPI 端点 ID（独占模式）；Linux 为 ALSA hw:卡,设备 或 hw:CARD=名称,DEV=n
（只接受 hw 设备，拒绝 plughw/default/dmix/pipewire/pulse 等会转换或混音的设备）。
PowerShell 会把未加引号的 {…} 当成脚本块，端点 ID 要加单引号：
  qybit --device '{0.0.0.00000000}.{…}' a.wav

--container：只请求该容器位宽的设备格式（目前只用于 Windows）。驱动对某种格式回报支持、实际播放
却是杂音时用来排查，例如 24 位源用 --container 32 避开 3 字节紧凑格式；样本值不变。

--mono-to-stereo：非严格选项，默认关闭。单声道 [FC] 文件按立体声 [FL FR] 请求设备格式，每个样本
原样复制到左右两路（不混音、不改增益、样本值不变）。用于只接受立体声的设备；报告标为“声道已改变”，
不再记为严格路径。

--float-to-int：非严格选项，默认关闭。浮点 WAV 先按原样（同一浮点格式）请求设备；设备不接受浮点时，
依次按 32、24、16 位整数请求，把样本按 round(x × 2^(V−1)) 转为设备有效位 V 的整数（不加抖动，
超出 ±1.0 的样本限幅）。报告标为“样本已改变”，并给出不精确与限幅的样本数；遇到 NaN 或无穷时该曲目
在该处停止。

--volume（Linux）：播放期间把设备自身的硬件音量设为 N%（默认 40%，百分比与 alsamixer 相同），并打开
该组的静音开关，播放结束或 Ctrl+C 后恢复原值；off 为不调整。只调设备的硬件控制，送出的数据不变；
设备没有可用的硬件音量时照常播放并说明原因，不以软件缩放代替。控制按标准名称 Master、PCM 选择，
--volume-control 可指定（如 Headphone）。进程被强制结束（如连按两次 Ctrl+C）时来不及恢复。

退出码：0 完整播放；2 播放完成但有欠载；1 失败（含设备不支持、打开失败、设备错误、中断）。";

/// 播放期间的设备硬件音量（Linux，tasks/A4.md）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Volume {
    /// 未给出：Linux 用 [`DEFAULT_VOLUME_PERCENT`]。
    Default,
    Percent(u32),
    Off,
}

/// 未给出 `--volume` 时的音量（2026-10-18 用户决定；只用于 Linux）。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const DEFAULT_VOLUME_PERCENT: u32 = 40;

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    ListDevices,
    Play {
        device: String,
        files: Vec<PathBuf>,
        container: Option<u8>,
        /// 明确开启的单声道复制到左右两路（tasks/O1.md）。
        mono_to_stereo: bool,
        /// 明确开启的浮点转整数（tasks/F3.md）。
        float_to_int: bool,
        /// 播放期间的设备硬件音量（tasks/A4.md）。
        volume: Volume,
        /// 调整音量所用的控制名（缺省按标准名称选择）。
        volume_control: Option<String>,
    },
    Matrix {
        device: String,
        out: PathBuf,
    },
    /// 列出声卡硬件混音器（Linux，tasks/A2.md，只读）。
    Mixer {
        device: String,
    },
    Decode {
        input: PathBuf,
        out: PathBuf,
    },
    Tone {
        out: PathBuf,
        rate: u32,
        /// 整数位数 16 / 24 / 32；`float` 时为浮点位宽 32 / 64。
        bits: u8,
        /// 写 IEEE 浮点 WAV（F3）。
        float: bool,
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
            let (mut float, mut int_bits) = (false, false);
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
                        };
                        int_bits = true;
                    }
                    "--float" => {
                        bits = match v.as_str() {
                            "32" => 32,
                            "64" => 64,
                            _ => return Err(bad()),
                        };
                        float = true;
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
            if float && int_bits {
                return Err("--bits 与 --float 只能选一个".into());
            }
            Ok(Command::Tone {
                out,
                rate,
                bits,
                float,
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
        Some("mixer") => {
            let mut device = None;
            while let Some(a) = it.next() {
                match a {
                    "--device" => device = Some(device_value(&mut it)?),
                    x => {
                        return Err(format!(
                            "mixer 只用于查看，不接受参数 {x}；播放音量用 --volume 指定"
                        ));
                    }
                }
            }
            Ok(Command::Mixer {
                device: device.ok_or("mixer 需要 --device <hw 设备>")?,
            })
        }
        Some("--device") => {
            let device = device_value(&mut it)?;
            let mut files = Vec::new();
            let mut container = None;
            let mut mono_to_stereo = false;
            let mut float_to_int = false;
            let mut volume = Volume::Default;
            let mut volume_control = None;
            while let Some(a) = it.next() {
                if a == "--volume" {
                    let v = value(&mut it, "--volume")?;
                    volume = match v.as_str() {
                        "off" => Volume::Off,
                        x => match x.strip_suffix('%').and_then(|n| n.parse::<u32>().ok()) {
                            Some(n) if n <= 100 => Volume::Percent(n),
                            _ => {
                                return Err(format!(
                                    "--volume 只接受 0%..100% 的整数百分比或 off，收到 {x}"
                                ));
                            }
                        },
                    };
                } else if a == "--volume-control" {
                    volume_control = Some(value(&mut it, "--volume-control")?);
                } else if a == "--mono-to-stereo" {
                    mono_to_stereo = true;
                } else if a == "--float-to-int" {
                    float_to_int = true;
                } else if a == "--container" {
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
                mono_to_stereo,
                float_to_int,
                volume,
                volume_control,
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
    if [
        ".wav", ".rf64", ".bw64", ".flac", ".fla", ".aif", ".aiff", ".aifc",
    ]
    .iter()
    .any(|e| lower.ends_with(e))
    {
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
                mono_to_stereo: false,
                float_to_int: false,
                volume: Volume::Default,
                volume_control: None,
            })
        );
        assert_eq!(
            p(&["--device", "id", "a.wav", "--container", "32", "b.flac"]),
            Ok(Command::Play {
                device: "id".into(),
                files: vec!["a.wav".into(), "b.flac".into()],
                container: Some(32),
                mono_to_stereo: false,
                float_to_int: false,
                volume: Volume::Default,
                volume_control: None,
            })
        );
        assert_eq!(
            p(&["--device", "id", "--mono-to-stereo", "m.wav"]),
            Ok(Command::Play {
                device: "id".into(),
                files: vec!["m.wav".into()],
                container: None,
                mono_to_stereo: true,
                float_to_int: false,
                volume: Volume::Default,
                volume_control: None,
            })
        );
        assert_eq!(
            p(&["--device", "id", "f.wav", "--float-to-int"]),
            Ok(Command::Play {
                device: "id".into(),
                files: vec!["f.wav".into()],
                container: None,
                mono_to_stereo: false,
                float_to_int: true,
                volume: Volume::Default,
                volume_control: None,
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

    #[test]
    fn parses_mixer_and_volume() {
        assert_eq!(
            p(&["mixer", "--device", "hw:CARD=X"]),
            Ok(Command::Mixer {
                device: "hw:CARD=X".into()
            })
        );
        assert!(p(&["mixer", "--device", "hw:0", "Master", "40%"]).is_err());
        assert!(p(&["mixer", "--device", "hw:0", "--save"]).is_err());
        assert!(p(&["mixer"]).is_err());
        let play = |extra: &[&str]| {
            let mut a = vec!["--device", "hw:0"];
            a.extend_from_slice(extra);
            a.push("a.wav");
            p(&a)
        };
        let vol = |r: Result<Command, String>| match r {
            Ok(Command::Play {
                volume,
                volume_control,
                ..
            }) => Ok((volume, volume_control)),
            Ok(other) => Err(format!("{other:?}")),
            Err(e) => Err(e),
        };
        assert_eq!(vol(play(&[])), Ok((Volume::Default, None)));
        assert_eq!(
            vol(play(&["--volume", "60%"])),
            Ok((Volume::Percent(60), None))
        );
        assert_eq!(vol(play(&["--volume", "off"])), Ok((Volume::Off, None)));
        assert_eq!(
            vol(play(&["--volume", "0%", "--volume-control", "Headphone"])),
            Ok((Volume::Percent(0), Some("Headphone".into())))
        );
        assert!(play(&["--volume", "101%"]).is_err());
        assert!(play(&["--volume", "40"]).is_err());
        assert!(play(&["--volume"]).is_err());
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
                float: false,
                seconds: 5.0,
                freq: 1000.0
            })
        );
        assert_eq!(
            p(&["tone", "f.wav", "--float", "64"]),
            Ok(Command::Tone {
                out: "f.wav".into(),
                rate: 44_100,
                bits: 64,
                float: true,
                seconds: 5.0,
                freq: 1000.0
            })
        );
        assert!(p(&["tone", "f.wav", "--float", "16"]).is_err());
        assert!(p(&["tone", "f.wav", "--float", "32", "--bits", "16"]).is_err());
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
