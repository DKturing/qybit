//! QyBit 命令行入口（M0 T6）。
//!
//! 后端按目标平台选择：Windows 为 WASAPI 独占；Linux ALSA 在 M2 T3 加入，
//! 在此之前其他平台明确报错，不做任何回退。播放驱动与报告来自 `qy-player`。
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
                for e in list {
                    println!("{}\t{}", e.id, e.name.unwrap_or_default());
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("错误：枚举设备失败：{e}");
                ExitCode::FAILURE
            }
        },
        Command::Play { device, files } => {
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
            let report = qy_player::play(&mut backend, &mut res, &files);
            print!("{}", report.summary());
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

#[cfg(not(windows))]
fn run(_cmd: Command) -> ExitCode {
    eprintln!(
        "错误：当前只支持 Windows WASAPI 独占输出（Linux ALSA 在 M2 T3 实现），本平台不播放。"
    );
    ExitCode::FAILURE
}
