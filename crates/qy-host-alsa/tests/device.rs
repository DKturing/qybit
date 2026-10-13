//! 真实 ALSA 设备测试（M2 T3，仅 Linux，默认忽略）。
//!
//! 运行：设置 `QYBIT_ALSA_DEVICE=hw:CARD=…,DEV=…`（`qybit list-devices` 可列出），然后
//! `cargo test -p qy-host-alsa --test device -- --ignored --test-threads=1 --nocapture`。
//! 未设置设备或设备不存在时明确失败，不跳过。设备须未被 PipeWire/PulseAudio 等占用。
//! 本测试只证明 hw 设备上的参数读回一致与播放运行，不证明 DAC 输入样本逐位一致。
#![cfg(target_os = "linux")]

use std::path::PathBuf;

use qy_host_alsa::AlsaBackend;
use qy_host_alsa::linux::RealOpener;
use qy_host_alsa::linux::priority::RtPriority;
use qy_player::render::RenderEnd;
use qy_player::signal::write_test_wav;
use qy_player::{HostResources, play};

fn device() -> String {
    match std::env::var("QYBIT_ALSA_DEVICE") {
        Ok(d) if !d.is_empty() => d,
        _ => panic!("未设置 QYBIT_ALSA_DEVICE：真机测试需要显式 hw 设备名"),
    }
}

fn play_one(file: PathBuf, frames: u64) {
    let dev = device();
    let Ok(mut res) = HostResources::new() else {
        panic!("资源分配失败");
    };
    let mut backend = AlsaBackend::new(dev.clone(), RealOpener, RtPriority::default());
    let report = play(&mut backend, &mut res, &[file]);
    println!("设备 {dev}\n{}", report.summary());
    assert!(
        report.open_failures.is_empty(),
        "打开失败: {:#?}",
        report.open_failures
    );
    assert_eq!(report.streams.len(), 1);
    let Some(render) = report.streams[0].render else {
        panic!("渲染线程未返回报告");
    };
    assert_eq!(render.end, RenderEnd::Drained);
    assert_eq!(render.valid_frames, frames);
    assert_eq!(render.underrun_events, 0, "出现欠载: {render:?}");
    assert!(report.finished && report.software_path_intact());
}

#[test]
#[ignore = "需要真实 ALSA hw 设备与 QYBIT_ALSA_DEVICE"]
fn hw_playback_48k_16bit() {
    let path = std::env::temp_dir().join("qybit-alsa-device-test-48k16.wav");
    let frames = match write_test_wav(&path, 48_000, 2, 16, 48_000) {
        Ok(f) => f,
        Err(e) => panic!("写测试文件失败: {e}"),
    };
    play_one(path, frames);
}

#[test]
#[ignore = "需要真实 ALSA hw 设备与 QYBIT_ALSA_DEVICE"]
fn hw_playback_flac_44k_16bit() {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../qy-codec-flac/tests/fixtures/cli_16_stereo_44k_default.flac");
    play_one(file, 20_000);
}

/// 格式矩阵（48 项）与顺序换曲。报告写入 `QYBIT_REPORT_DIR`（默认 `target/qybit-report`）。
#[test]
#[ignore = "需要真实 ALSA hw 设备与 QYBIT_ALSA_DEVICE"]
fn hw_format_matrix_and_sequential() {
    let dev = device();
    let work = std::env::temp_dir().join("qybit-alsa-device-matrix");
    let report = match qy_host_alsa::device_matrix(&dev, &work) {
        Ok(r) => r,
        Err(e) => panic!("{e}"),
    };
    let out = std::env::var("QYBIT_REPORT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/qybit-report")
        });
    match report.write(&out) {
        Ok((json, csv)) => println!("报告：{}\n      {}", json.display(), csv.display()),
        Err(e) => panic!("写报告失败: {e}"),
    }
    assert_eq!(report.entries.len(), 48);
    let bad: Vec<_> = report
        .entries
        .iter()
        .filter(|e| e.accepted && !e.played_ok())
        .collect();
    assert!(bad.is_empty(), "参数读回一致但未完整播放: {bad:#?}");
    match &report.sequential {
        Some(s) => assert!(s.ok, "顺序换曲失败: {s:?}"),
        None => panic!("支持并播放成功的采样率少于两个，无法验证顺序换曲"),
    }
}
