//! 真实设备测试（M0 T5，仅 Windows，默认忽略）。
//!
//! 运行：设置 `QYBIT_DEVICE_ID=<端点 ID>`（`qybit list-devices` 可列出），然后
//! `cargo test -p qy-host-wasapi --test device -- --ignored --test-threads=1 --nocapture`。
//! 未设置设备 ID 或设备不存在时明确失败，不跳过。
//! 本测试只证明独占协商与播放运行，不证明 DAC 输入样本逐位一致。
#![cfg(windows)]

use std::path::PathBuf;

use qy_host_wasapi::HostResources;
use qy_host_wasapi::player::play;
use qy_host_wasapi::render::RenderEnd;
use qy_host_wasapi::wasapi::WasapiBackend;

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
    println!("MMCSS: {:?}", render.mmcss);
    assert_eq!(render.end, RenderEnd::Drained);
    assert_eq!(render.valid_frames, 48_000);
    assert_eq!(render.underrun_events, 0, "出现欠载: {render:?}");
    assert!(report.finished && report.software_path_intact());
}
