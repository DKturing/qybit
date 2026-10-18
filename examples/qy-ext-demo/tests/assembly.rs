//! E2 应用组装：同一个命令行与图形界面主体换上 [`ExtFormats`]（WAV / FLAC / AIFF / QYT1）。
//!
//! - 命令行：组装出的 `qybit-ext` 二进制的帮助列出示例格式，`decode` 把 QYT1 / QYL1 写成 WAV，
//!   样本与测试自己生成的样本逐字节一致；`--device` 后误跟示例格式的文件时按音频文件识别；
//! - 图形界面：`qy_ui::Ui::with_formats` 的播放列表显示示例格式名与有损标记，播放（假设备，
//!   真实核心、组合解码器、pack 与渲染循环）送出的字节与样本一致。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use qy_ext_demo::ExtFormats;
use qy_ext_demo::qyt1::{encode, encode_lossy};
use qy_player::Formats;
use qy_player::signal::write_test_wav;
use qy_player::testing::{FakeBackend, FakeDevice};
use qy_ui::{Probe, Ui, text};

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("qy-ext-e2-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    if let Err(e) = std::fs::create_dir_all(&d) {
        panic!("{e}");
    }
    d
}

fn write(path: &Path, bytes: &[u8]) {
    if let Err(e) = std::fs::write(path, bytes) {
        panic!("{}：{e}", path.display());
    }
}

/// 可辨识的 16 位样本（每个样本不同，避免错位或重复帧仍然相等）。
fn samples(frames: usize, channels: usize, salt: i32) -> Vec<i32> {
    (0..frames * channels)
        .map(|i| ((i as i32).wrapping_mul(7_919).wrapping_add(salt) % 32_768) - 16_384)
        .collect()
}

/// 独立参考：16 位样本的小端字节。
fn le16(samples: &[i32]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|&s| (s as i16).to_le_bytes())
        .collect()
}

/// WAV data 块原始字节（独立提取）。
fn data_chunk(wav: &[u8]) -> Vec<u8> {
    let mut i = 12;
    while i + 8 <= wav.len() {
        let size = u32::from_le_bytes([wav[i + 4], wav[i + 5], wav[i + 6], wav[i + 7]]) as usize;
        if &wav[i..i + 4] == b"data" {
            return wav[i + 8..(i + 8 + size).min(wav.len())].to_vec();
        }
        i += 8 + size + (size & 1);
    }
    Vec::new()
}

fn qybit_ext(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_qybit-ext"))
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{e}"));
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn formats_lists_example_extensions() {
    let f = ExtFormats;
    for ext in ["wav", "rf64", "flac", "aifc", "qyt1", "qyl1"] {
        assert!(f.extensions().contains(&ext), "{ext}");
    }
    assert!(f.is_audio_path(Path::new("a/B.QYT1")));
    assert!(!f.is_audio_path(Path::new("a/b.mp3")));
    assert!(!f.is_audio_path(Path::new("qyt1")));
}

#[test]
fn cli_help_lists_example_format() {
    let (ok, out, _) = qybit_ext(&["--help"]);
    assert!(ok);
    assert!(out.contains("WAV / FLAC / AIFF / QYT1"), "{out}");
}

/// `--device` 的值是示例格式的文件名时，与官方格式一样说明设备 ID 没有传进来。
#[test]
fn cli_device_value_knows_example_extension() {
    let (ok, _, err) = qybit_ext(&["--device", "a.QYT1", "b.qyt1"]);
    assert!(!ok);
    assert!(err.contains("看起来是音频文件"), "{err}");
    let (ok, _, err) = qybit_ext(&["--device", "a.mp3"]);
    assert!(!ok);
    assert!(!err.contains("看起来是音频文件"), "{err}");
}

#[test]
fn cli_decode_writes_qyt1_and_qyl1_samples() {
    let dir = temp("cli");
    for (name, lossy) in [("a.qyt1", false), ("b.qyl1", true)] {
        let s = samples(3_000, 2, if lossy { 11 } else { 3 });
        let bytes = if lossy {
            encode_lossy(48_000, 2, 16, &s)
        } else {
            encode(48_000, 2, 16, &s)
        };
        let input = dir.join(name);
        write(&input, &bytes);
        let out = dir.join(format!("{name}.wav"));
        let (ok, stdout, stderr) = qybit_ext(&[
            "decode",
            input.to_str().unwrap_or_default(),
            out.to_str().unwrap_or_default(),
        ]);
        assert!(ok, "{name}：{stderr}");
        assert!(stdout.contains("源：48000 Hz / 2 声道"), "{stdout}");
        assert!(stdout.contains("写出 3000 帧"), "{stdout}");
        // E1：有损变体在报告中写明解码精度，不说成源位深。
        assert_eq!(stdout.contains("有损"), lossy, "{stdout}");
        let wav = std::fs::read(&out).unwrap_or_else(|e| panic!("{e}"));
        assert!(data_chunk(&wav) == le16(&s), "{name}");
    }
    // 官方格式经同一个组合解码器照常解码。
    let wav = dir.join("c.wav");
    if let Err(e) = write_test_wav(&wav, 44_100, 2, 16, 1_000) {
        panic!("{e}");
    }
    let out = dir.join("c.out.wav");
    let (ok, stdout, stderr) = qybit_ext(&[
        "decode",
        wav.to_str().unwrap_or_default(),
        out.to_str().unwrap_or_default(),
    ]);
    assert!(ok, "{stderr}");
    assert!(stdout.contains("写出 1000 帧"), "{stdout}");
    let src = std::fs::read(&wav).unwrap_or_else(|e| panic!("{e}"));
    let dec = std::fs::read(&out).unwrap_or_else(|e| panic!("{e}"));
    assert!(data_chunk(&src) == data_chunk(&dec));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 处理事件直到 `done` 成立，至多 `secs` 秒。
fn until(ui: &mut Ui, secs: u64, mut done: impl FnMut(&mut Ui) -> bool) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) {
        ui.wait(Duration::from_millis(20));
        if done(ui) {
            return true;
        }
    }
    false
}

#[test]
fn ui_lists_and_plays_example_format() {
    let dir = temp("ui");
    let lossless = samples(4_410, 2, 5);
    let lossy = samples(4_410, 2, 9);
    let a = dir.join("a.qyt1");
    let b = dir.join("b.qyl1");
    let c = dir.join("c.wav");
    write(&a, &encode(44_100, 2, 16, &lossless));
    write(&b, &encode_lossy(44_100, 2, 16, &lossy));
    if let Err(e) = write_test_wav(&c, 44_100, 2, 16, 4_410) {
        panic!("{e}");
    }
    // 扩展名不在格式集合里的文件不加入播放列表。
    let skipped = dir.join("d.mp3");
    write(&skipped, b"ID3");

    let mut backend = FakeBackend::new(&[(16, 16)]);
    backend.device = FakeDevice::Clock;
    let outputs = backend.clone();
    let mut ui = Ui::with_formats(
        ExtFormats,
        move |_device: Option<&str>| Ok(backend.clone()),
        Arc::new(|| {}),
    );
    assert_eq!(ui.format_names(), "WAV / FLAC / AIFF / QYT1");
    ui.add_paths(vec![dir.clone()]);
    assert!(until(&mut ui, 10, |u| {
        u.model.entries.len() == 3 && u.model.entries.iter().all(|e| e.probe != Probe::Pending)
    }));
    let mut columns: Vec<_> = ui
        .model
        .entries
        .iter()
        .map(|e| match &e.probe {
            Probe::Ready(i) => (text::format_column(i), i.frames),
            other => panic!("{other:?}"),
        })
        .collect();
    columns.sort();
    assert_eq!(
        columns,
        [
            (
                "QYL1 44.1 kHz 解码 16 位 2 声道 有损".to_string(),
                Some(4_410)
            ),
            ("QYT1 44.1 kHz 16 位 2 声道".to_string(), Some(4_410)),
            ("WAV 44.1 kHz 16 位 2 声道".to_string(), Some(4_410)),
        ]
    );

    // 播放示例格式的两首：送出的有效字节与样本一致。
    for (name, s) in [("a.qyt1", &lossless), ("b.qyl1", &lossy)] {
        let Some(index) = ui.model.entries.iter().position(|e| e.path.ends_with(name)) else {
            panic!("{name}")
        };
        let before = outputs.outputs().len();
        ui.play_from(index);
        assert!(until(&mut ui, 20, |u| u.model.playing.is_none()
            && u.model.last.is_some()));
        let Some(o) = ui.model.last.clone() else {
            panic!("无结果")
        };
        assert!(o.software_intact, "{name}：{}", o.summary);
        let outs = outputs.outputs();
        assert!(outs.len() > before, "{name}");
        // 同一个播放会话按列表顺序继续播放后面的曲目；找本曲的输出流。
        let out = &outs[before];
        let valid = out.report.valid_frames as usize * 4;
        assert!(out.rendered[..valid].starts_with(&le16(s)), "{name}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
