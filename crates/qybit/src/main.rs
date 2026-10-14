//! QyBit 命令行入口（M0 T6）。
//!
//! 后端按目标平台选择：Windows 为 WASAPI 独占，Linux 为 ALSA hw 设备（M2 T3）；
//! 其他平台明确报错，不做任何回退。播放驱动与报告来自 `qy-player`。
//! 输出只陈述源格式、请求格式、独占协商结果、MMCSS、欠载与错误，
//! 不宣称设备端 bit-perfect。

mod args;

use std::process::ExitCode;

use args::{Command, USAGE, parse};

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match parse(&argv) {
        Err(e) => {
            eprintln!("错误：{e}\n\n{USAGE}");
            ExitCode::FAILURE
        }
        Ok(Command::Help) => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Ok(Command::Version) => {
            println!("qybit {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(cmd) => run(cmd),
    }
}

#[cfg(windows)]
fn run(cmd: Command) -> ExitCode {
    use qy_host_wasapi::endpoints::{endpoint_info, list_render_endpoints};
    match cmd {
        Command::ListDevices => match list_render_endpoints() {
            Ok(list) => {
                if list.is_empty() {
                    eprintln!("没有活动的输出设备");
                }
                let first = list.first().map(|e| e.id.clone());
                for e in list {
                    println!("{}\t{}", e.id, e.name.unwrap_or_default());
                }
                // 端点 ID 含 {…}：PowerShell 中必须加引号，否则被当成脚本块。
                if let Some(id) = first {
                    eprintln!(
                        "\n播放示例（PowerShell 中端点 ID 须加单引号）：\n  qybit --device '{id}' a.wav b.flac"
                    );
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("错误：枚举设备失败：{e}");
                ExitCode::FAILURE
            }
        },
        Command::Play {
            device,
            files,
            container,
        } => {
            let info = match endpoint_info(&device) {
                Ok(i) => i,
                Err(e) => {
                    eprintln!("错误：找不到设备 {device}：{e}");
                    return ExitCode::FAILURE;
                }
            };
            println!(
                "设备：{}（{}）",
                info.id,
                info.name.as_deref().unwrap_or("名称未知")
            );
            let mut res = match qy_player::HostResources::new() {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("错误：资源分配失败：{e:?}");
                    return ExitCode::FAILURE;
                }
            };
            let mut backend = qy_host_wasapi::wasapi::WasapiBackend::new(device);
            backend.container = container;
            if let Some(c) = container {
                println!("只请求容器 {c} 位的设备格式（--container）");
            }
            let stop = interrupt_flag(qy_host_wasapi::interrupt::install());
            let report = play_live(&mut backend, &mut res, &files, stop);
            ExitCode::from(report.exit_code())
        }
        Command::Matrix { device, out } => {
            let work = std::env::temp_dir().join(format!("qybit-matrix-{}", std::process::id()));
            let report = match qy_host_wasapi::wasapi::device_matrix(&device, &work) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("错误：{e}");
                    return ExitCode::FAILURE;
                }
            };
            let _ = std::fs::remove_dir_all(&work);
            let supported = report.entries.iter().filter(|e| e.supported()).count();
            let played = report.entries.iter().filter(|e| e.played_ok()).count();
            println!(
                "设备：{}（{}）\n矩阵 {} 项：独占支持 {supported} 项，播放完整 {played} 项",
                report.device.id,
                report.device.name.as_deref().unwrap_or("名称未知"),
                report.entries.len()
            );
            match &report.sequential {
                Some(s) => println!("顺序换曲 {:?} Hz：{}", s.rates, s.detail),
                None => println!("顺序换曲：未运行（支持并播放成功的采样率少于两个）"),
            }
            match report.write(&out) {
                Ok((json, csv)) => {
                    println!("报告：{}\n      {}", json.display(), csv.display());
                    println!(
                        "说明：接口成功与播放统计只证明兼容与运行，不证明 DAC 输入样本逐位一致。"
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("错误：写报告失败：{e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Help | Command::Version => ExitCode::SUCCESS,
    }
}

#[cfg(target_os = "linux")]
fn run(cmd: Command) -> ExitCode {
    use qy_host_alsa::AlsaBackend;
    use qy_host_alsa::linux::RealOpener;
    use qy_host_alsa::linux::devices::list_playback;
    use qy_host_alsa::linux::priority::RtPriority;
    match cmd {
        Command::ListDevices => match list_playback() {
            Ok(list) => {
                if list.is_empty() {
                    eprintln!("没有可播放的 ALSA hw 设备");
                }
                for e in list {
                    println!("{}\t{} — {}", e.name, e.card_name, e.pcm_name);
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("错误：枚举设备失败：{e}");
                ExitCode::FAILURE
            }
        },
        Command::Play {
            device,
            files,
            container,
        } => {
            if container.is_some() {
                eprintln!("错误：--container 目前只用于 Windows WASAPI");
                return ExitCode::FAILURE;
            }
            if let Err(e) = check_device(&device) {
                eprintln!("错误：{e}");
                return ExitCode::FAILURE;
            }
            println!("设备：{device}（ALSA hw）");
            let mut res = match qy_player::HostResources::new() {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("错误：资源分配失败：{e:?}");
                    return ExitCode::FAILURE;
                }
            };
            let mut backend = AlsaBackend::new(device, RealOpener, RtPriority::default());
            let stop = interrupt_flag(qy_host_alsa::linux::interrupt::install());
            let report = play_live(&mut backend, &mut res, &files, stop);
            ExitCode::from(report.exit_code())
        }
        Command::Matrix { device, out } => {
            let work = std::env::temp_dir().join(format!("qybit-matrix-{}", std::process::id()));
            let report = match qy_host_alsa::device_matrix(&device, &work) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("错误：{e}");
                    return ExitCode::FAILURE;
                }
            };
            let _ = std::fs::remove_dir_all(&work);
            let accepted = report.entries.iter().filter(|e| e.accepted).count();
            let played = report.entries.iter().filter(|e| e.played_ok()).count();
            println!(
                "设备：{}\n矩阵 {} 项：参数读回一致 {accepted} 项，播放完整 {played} 项",
                report.device,
                report.entries.len()
            );
            match &report.sequential {
                Some(s) => println!("顺序换曲 {:?} Hz：{}", s.rates, s.detail),
                None => println!("顺序换曲：未运行（支持并播放成功的采样率少于两个）"),
            }
            match report.write(&out) {
                Ok((json, csv)) => {
                    println!("报告：{}\n      {}", json.display(), csv.display());
                    println!(
                        "说明：参数读回与播放统计只证明兼容与运行，不证明 DAC 输入样本逐位一致。"
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("错误：写报告失败：{e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Help | Command::Version => ExitCode::SUCCESS,
    }
}

/// 设备名检查（只接受 hw:），在分配资源之前给出明确原因。
#[cfg(target_os = "linux")]
fn check_device(device: &str) -> Result<(), String> {
    qy_host_alsa::pcm_name_check(device)
}

/// Ctrl+C 处理安装失败时仍可播放，只是不能中途优雅停止。
#[cfg(any(windows, target_os = "linux"))]
fn interrupt_flag(
    r: Result<&'static std::sync::atomic::AtomicBool, String>,
) -> Option<&'static std::sync::atomic::AtomicBool> {
    match r {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!("警告：{e}；Ctrl+C 将直接结束进程，不输出报告");
            None
        }
    }
}

/// 播放并实时输出进度（曲目、源格式、协商结果、跳过与修正），结束或中断后输出完整报告。
#[cfg(any(windows, target_os = "linux"))]
fn play_live<B: qy_player::Backend>(
    backend: &mut B,
    res: &mut qy_player::HostResources,
    files: &[std::path::PathBuf],
    stop: Option<&std::sync::atomic::AtomicBool>,
) -> qy_player::PlaybackReport<B::Attempt> {
    use std::io::Write;
    println!("（Ctrl+C 停止并输出报告）");
    let report = qy_player::play_with(
        backend,
        res,
        &mut Files(files),
        qy_player::PlayControl {
            on_line: &mut |l: &str| {
                println!("{l}");
                let _ = std::io::stdout().flush();
            },
            stop,
        },
    );
    println!("\n—— 播放报告 ——");
    print!("{}", report.summary());
    report
}

/// 命令行给出的文件列表。
#[cfg(any(windows, target_os = "linux"))]
struct Files<'a>(&'a [std::path::PathBuf]);

#[cfg(any(windows, target_os = "linux"))]
impl qy_player::TrackSource for Files<'_> {
    type Reader = std::fs::File;

    fn count(&self) -> u32 {
        u32::try_from(self.0.len()).unwrap_or(u32::MAX)
    }

    fn open(&mut self, track: u32) -> std::io::Result<std::fs::File> {
        match self.0.get(track as usize) {
            Some(p) => std::fs::File::open(p),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "曲目编号越界",
            )),
        }
    }

    fn name(&self, track: u32) -> String {
        self.0.get(track as usize).map_or_else(
            || format!("第 {} 首", track + 1),
            |p| p.display().to_string(),
        )
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
fn run(_cmd: Command) -> ExitCode {
    eprintln!("错误：当前只支持 Windows WASAPI 独占与 Linux ALSA hw 输出，本平台不播放。");
    ExitCode::FAILURE
}
