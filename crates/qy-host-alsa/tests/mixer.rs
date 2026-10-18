//! A2：真实内核上的混音器读写（仅 Linux，默认忽略）。
//!
//! 设备为内核的 snd-dummy 虚拟声卡（`QYBIT_MIXER_DEVICE`，如 `hw:CARD=Dummy`；
//! `scripts/aloop-vm.sh` 在虚拟机中加载它）。用到它的这几个混音器元素（`sound/drivers/dummy.c`）：
//! - `Master Volume`：立体声，原始值 −50..100，DB_SCALE −45 dB 起每档 0.30 dB，初始 0（−30 dB）；
//! - `Master Capture Switch`：立体声开关；
//! - `External I/O Box`：枚举 None / CD Player，选 None 时 `CD Volume` 与 `CD Capture Switch`
//!   变为未启用（INACTIVE）。
//!
//! 期望值都按 dummy.c 与 alsa-lib 的换算规则独立算出。
#![cfg(target_os = "linux")]

use qy_host_alsa::linux::mixer::{playback_hint, set, show};

fn device() -> String {
    match std::env::var("QYBIT_MIXER_DEVICE") {
        Ok(d) if !d.is_empty() => d,
        _ => panic!("未设置 QYBIT_MIXER_DEVICE：需要已加载的 snd-dummy（如 hw:CARD=Dummy）"),
    }
}

fn set_ok(dev: &str, control: &str, values: &[&str]) -> String {
    let v: Vec<String> = values.iter().map(|s| s.to_string()).collect();
    match set(dev, control, &v) {
        Ok(lines) => {
            let s = lines.join("\n");
            println!("{s}");
            s
        }
        Err(e) => panic!("设置 {control} {values:?} 失败：{e}"),
    }
}

fn set_err(dev: &str, control: &str, values: &[&str]) -> String {
    let v: Vec<String> = values.iter().map(|s| s.to_string()).collect();
    match set(dev, control, &v) {
        Ok(lines) => panic!("应当失败：{control} {values:?} → {lines:?}"),
        Err(e) => {
            println!("（预期的错误）{e}");
            e
        }
    }
}

#[test]
#[ignore = "需要 snd-dummy 与 QYBIT_MIXER_DEVICE"]
fn dummy_mixer_read_and_write() {
    let dev = device();
    let shown = match show(&dev) {
        Ok(l) => l.join("\n"),
        Err(e) => panic!("{e}"),
    };
    println!("{shown}");
    // 初始：原始值 0 → 50/150 = 33%，−45 + 0.30·50 = −30 dB。
    assert!(
        shown.contains("Master：音量 0（范围 -50..100），33%，-30.00 dB"),
        "{shown}"
    );
    assert!(shown.contains("Master（录音）：开关 关"), "{shown}");
    assert!(
        shown.contains("External I/O Box：CD Player（可选：None、CD Player）"),
        "{shown}"
    );

    // 百分比：−50 + 150·40% = 10 → −45 + 0.30·60 = −27 dB。
    let r = set_ok(&dev, "Master", &["40%"]);
    assert!(
        r.contains("音量 10（范围 -50..100），40%，-27.00 dB"),
        "{r}"
    );
    // dB：不超过 −20 dB 的最大档：0.30·k ≤ 25 → k = 83 → 原始值 33，−20.10 dB。
    let r = set_ok(&dev, "master", &["-20dB"]);
    assert!(
        r.contains("音量 33（范围 -50..100），55%，-20.10 dB"),
        "{r}"
    );
    // 原始值：100 → 0 dB；越界拒绝且不改变。
    let r = set_ok(&dev, "Master Volume", &["100"]);
    assert!(
        r.contains("音量 100（范围 -50..100），100%，0.00 dB"),
        "{r}"
    );
    set_err(&dev, "Master", &["101"]);
    // 没有开关的组不接受 on/off；录音开关按完整元素名或录音方向设置。
    set_err(&dev, "Master", &["off"]);
    let r = set_ok(&dev, "Master Capture Switch", &["on"]);
    assert!(r.contains("Master（录音）：开关 开"), "{r}");
    // 枚举：选 None 后 CD 的两个控制变为未启用；再恢复。
    set_ok(&dev, "External I/O Box", &["None"]);
    let shown = show(&dev).map(|l| l.join("\n")).unwrap_or_default();
    assert!(
        shown.contains("CD：音量 0（范围 -50..100），33%，-30.00 dB（未启用）"),
        "{shown}"
    );
    set_err(&dev, "External I/O Box", &["Tape"]);
    set_ok(&dev, "External I/O Box", &["CD Player"]);
    // 未知控制：列出可用的名称。
    let e = set_err(&dev, "Headphone", &["40%"]);
    assert!(e.contains("可用：") && e.contains("Master"), "{e}");

    // 播放前提示：Master 在最小档时列出（−50 → 0%，−45 dB）；打开后不再列出 Master。
    set_ok(&dev, "Master", &["0%"]);
    let hint = playback_hint(&dev).unwrap_or_default();
    println!("{hint}");
    assert!(hint.contains("Master（0%，-45.00 dB）"), "{hint}");
    set_ok(&dev, "Master", &["0"]);
    let hint = playback_hint(&dev).unwrap_or_default();
    assert!(!hint.contains("Master（"), "{hint}");
    // 恢复初始状态（同一虚拟机中下一个目标架构的程序再跑一遍）。
    set_ok(&dev, "Master Capture Switch", &["off"]);
}
