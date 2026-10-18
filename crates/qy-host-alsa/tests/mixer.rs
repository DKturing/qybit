//! A2 查看、A4 播放期间的硬件音量：真实内核上的混音器读写（仅 Linux，默认忽略）。
//!
//! 设备为内核的 snd-dummy 虚拟声卡（`QYBIT_MIXER_DEVICE`，如 `hw:CARD=Dummy`；
//! `scripts/aloop-vm.sh` 在虚拟机中加载它）。用到它的这几个混音器元素（`sound/drivers/dummy.c`）：
//! - `Master Volume`、`Synth Volume`：立体声，原始值 −50..100，DB_SCALE −45 dB 起每档 0.30 dB，
//!   初始 0（−30 dB）；
//! - `Master Capture Switch`：立体声开关；
//! - `External I/O Box`：枚举 None / CD Player。
//!
//! 百分比按 alsamixer（alsa-utils `volume_mapping.c`）独立算出：dB 范围 45 dB > 24 dB，按
//! 10^(dB/60) 映射，min_norm = 10^(−45/60) = 0.177828。
//! - 原始值 0（−30 dB）：(0.316228 − 0.177828) / 0.822172 = 0.1683 → 17%；
//! - 40%：0.4 · 0.822172 + 0.177828 = 0.506697 → 60 · log10 = −17.72 dB → 最接近的档为 91 档
//!   （−17.70 dB），即原始值 41；
//! - 0% → −45 dB（原始值 −50）；100% → 0 dB（原始值 100）。
#![cfg(target_os = "linux")]

use qy_host_alsa::linux::mixer::{PlaybackVolume, playback_hint, show};

fn device() -> String {
    match std::env::var("QYBIT_MIXER_DEVICE") {
        Ok(d) if !d.is_empty() => d,
        _ => panic!("未设置 QYBIT_MIXER_DEVICE：需要已加载的 snd-dummy（如 hw:CARD=Dummy）"),
    }
}

fn list(dev: &str) -> String {
    match show(dev) {
        Ok(l) => l.join("\n"),
        Err(e) => panic!("{e}"),
    }
}

fn apply(dev: &str, pct: u32, control: Option<&str>) -> PlaybackVolume {
    match PlaybackVolume::apply(dev, pct, control) {
        Ok((g, line)) => {
            println!("{line}");
            g
        }
        Err(e) => panic!("设置 {pct}% {control:?} 失败：{e}"),
    }
}

#[test]
#[ignore = "需要 snd-dummy 与 QYBIT_MIXER_DEVICE"]
fn dummy_mixer_list_and_playback_volume() {
    let dev = device();
    let before = list(&dev);
    println!("{before}");
    assert!(
        before.contains("Master：音量 0（范围 -50..100），17%，-30.00 dB"),
        "{before}"
    );
    assert!(before.contains("Master（录音）：开关 关"), "{before}");
    assert!(
        before.contains("External I/O Box：CD Player（可选：None、CD Player）"),
        "{before}"
    );

    // 缺省按标准名称选 Master；40% → 原始值 41（−17.70 dB）；恢复后与播放前完全相同。
    let mut g = apply(&dev, 40, None);
    let during = list(&dev);
    assert!(
        during.contains("Master：音量 41（范围 -50..100），40%，-17.70 dB"),
        "{during}"
    );
    // 播放中改为 60%（界面拖动音量滑块）：原始值 65（−10.50 dB，与命令行 --volume 60% 相同），
    // 恢复的仍是播放前的值。
    match g.set_percent(60) {
        Ok(l) => assert!(
            l.contains("音量 65（范围 -50..100），60%，-10.50 dB"),
            "{l}"
        ),
        Err(e) => panic!("{e}"),
    }
    match g.restore() {
        Ok(l) => println!("{l}"),
        Err(e) => panic!("{e}"),
    }
    assert_eq!(list(&dev), before);

    // 0%：最小档，播放前提示列出 Master；守卫析构时也恢复。
    {
        let _g = apply(&dev, 0, None);
        let hint = playback_hint(&dev).unwrap_or_default();
        println!("{hint}");
        assert!(hint.contains("Master（0%，-45.00 dB）"), "{hint}");
    }
    assert_eq!(list(&dev), before);

    // 指定控制：Synth 100% → 0 dB。
    let g = apply(&dev, 100, Some("Synth"));
    assert!(
        list(&dev).contains("Synth：音量 100（范围 -50..100），100%，0.00 dB"),
        "{}",
        list(&dev)
    );
    let _ = g.restore();
    assert_eq!(list(&dev), before);

    // 与当前相同（17% → 仍为原始值 0）：不写入，也无需恢复。
    let g = apply(&dev, 17, None);
    assert_eq!(list(&dev), before);
    let r = g.restore().unwrap_or_default();
    assert!(r.contains("无需恢复"), "{r}");

    // 不存在的控制与没有音量的控制：报错，设备不变。
    for (c, want) in [("Nope", "可用："), ("External I/O Box", "没有音量")] {
        match PlaybackVolume::apply(&dev, 40, Some(c)) {
            Ok(_) => panic!("{c} 应当失败"),
            Err(e) => assert!(e.contains(want), "{e}"),
        }
    }
    assert_eq!(list(&dev), before);
}
