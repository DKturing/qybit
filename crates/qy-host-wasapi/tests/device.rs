//! 真实设备测试（M0 T5，仅 Windows，默认忽略）。
//!
//! 运行：设置 `QYBIT_DEVICE_ID=<端点 ID>`（`qybit list-devices` 可列出），然后
//! `cargo test -p qy-host-wasapi --test device -- --ignored --test-threads=1 --nocapture`。
//! 未设置设备 ID 或设备不存在时明确失败，不跳过。
//! 本测试只证明独占协商与播放运行，不证明 DAC 输入样本逐位一致。
#![cfg(windows)]

use std::path::PathBuf;

use qy_host_wasapi::wasapi::WasapiBackend;
use qy_player::HostResources;
use qy_player::play;
use qy_player::render::RenderEnd;

fn device_id() -> String {
    match std::env::var("QYBIT_DEVICE_ID") {
        Ok(id) if !id.is_empty() => id,
        _ => panic!("未设置 QYBIT_DEVICE_ID：真机测试需要显式设备 ID"),
    }
}

/// 写一个 48 kHz/16 位/双声道、1 秒的低电平方波 WAV（-30 dBFS 左右）。
fn write_test_wav() -> PathBuf {
    let rate = 48_000u32;
    let frames = rate as usize;
    let mut data = Vec::with_capacity(frames * 4);
    for i in 0..frames {
        let s: i16 = if (i / 48) % 2 == 0 { 1000 } else { -1000 };
        data.extend_from_slice(&s.to_le_bytes());
        data.extend_from_slice(&s.to_le_bytes());
    }
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * 4).to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    let path = std::env::temp_dir().join("qybit-device-test-48k16.wav");
    if let Err(e) = std::fs::write(&path, wav) {
        panic!("写测试文件失败: {e}");
    }
    path
}

#[test]
#[ignore = "需要真实音频设备与 QYBIT_DEVICE_ID"]
fn exclusive_playback_48k_16bit() {
    let id = device_id();
    let file = write_test_wav();
    let Ok(mut res) = HostResources::new() else {
        panic!("资源分配失败");
    };
    let mut backend = WasapiBackend::new(id.clone());
    let report = play(&mut backend, &mut res, &[file]);
    println!("设备 {id}\n{report:#?}");
    assert!(
        report.open_failures.is_empty(),
        "独占打开失败: {:#?}",
        report.open_failures
    );
    assert_eq!(report.streams.len(), 1);
    let stream = &report.streams[0];
    assert!(stream.negotiation.chosen.is_some());
    let Some(render) = stream.render else {
        panic!("渲染线程未返回报告");
    };
    println!("MMCSS: {:?}", render.priority);
    assert_eq!(render.end, RenderEnd::Drained);
    assert_eq!(render.valid_frames, 48_000);
    assert_eq!(render.underrun_events, 0, "出现欠载: {render:?}");
    assert!(report.finished && report.software_path_intact());
}

/// M1 T3：自有 FLAC 解码经独占流播放（libFLAC 生成的 44.1 kHz/16 位/双声道夹具，20000 帧）。
#[test]
#[ignore = "需要真实音频设备与 QYBIT_DEVICE_ID"]
fn exclusive_playback_flac_44k_16bit() {
    let id = device_id();
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../qy-codec-flac/tests/fixtures/cli_16_stereo_44k_default.flac");
    let Ok(mut res) = HostResources::new() else {
        panic!("资源分配失败");
    };
    let mut backend = WasapiBackend::new(id.clone());
    let report = play(&mut backend, &mut res, &[file]);
    println!("设备 {id}\n{report:#?}");
    assert!(
        report.open_failures.is_empty(),
        "独占打开失败: {:#?}",
        report.open_failures
    );
    assert_eq!(report.streams.len(), 1);
    let Some(render) = report.streams[0].render else {
        panic!("渲染线程未返回报告");
    };
    assert_eq!(render.end, RenderEnd::Drained);
    assert_eq!(render.valid_frames, 20_000);
    assert_eq!(render.underrun_events, 0, "出现欠载: {render:?}");
    assert!(report.finished && report.software_path_intact());
}

/// 设备格式矩阵（70 项，含 F1 多声道 10 项）与顺序换曲。报告写入 `QYBIT_REPORT_DIR`（默认 `target/qybit-report`）。
/// 设备不支持某格式是正常能力结果；凡独占初始化成功的项都必须完整播放（排空、帧数一致、无欠载）。
#[test]
#[ignore = "需要真实音频设备与 QYBIT_DEVICE_ID"]
fn format_matrix_and_sequential() {
    let id = device_id();
    let work = std::env::temp_dir().join("qybit-device-matrix");
    let report = match qy_host_wasapi::wasapi::device_matrix(&id, &work) {
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
    assert_eq!(report.entries.len(), 60);
    let bad: Vec<_> = report
        .entries
        .iter()
        .filter(|e| e.initialize == Some(0) || e.aligned_retry == Some(0))
        .filter(|e| !e.played_ok())
        .collect();
    assert!(bad.is_empty(), "独占初始化成功但未完整播放: {bad:#?}");
    match &report.sequential {
        Some(s) => assert!(s.ok, "顺序换曲失败: {s:?}"),
        None => panic!("支持并播放成功的采样率少于两个，无法验证顺序换曲"),
    }
}
