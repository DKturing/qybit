//! GUI T2：界面模型在假后端上的完整会话（真实核心、自有解码、pack 与渲染循环，只模拟设备），
//! 以及文件头读取与文件夹扫描。期望值来自夹具的独立参考（.meta / .expect）与测试自己写入的 WAV。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use qy_player::signal::write_test_wav;
use qy_player::testing::{FakeBackend, FakeDevice, NoMonoNegotiator};
use qy_ui::probe::{Prober, scan_folder};
use qy_ui::{Changes, Deferred, Kind, Probe, Ui, text};

const ALL: [(u8, u8); 4] = [(16, 16), (24, 24), (32, 24), (32, 32)];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("qybit-ui-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    if let Err(e) = std::fs::create_dir_all(&d) {
        panic!("{e}");
    }
    d
}

/// 写入 `n` 首 44.1 kHz 双声道 16 位、各 `frames` 帧的 WAV。
fn tracks(dir: &Path, n: usize, frames: u64) -> Vec<PathBuf> {
    (0..n)
        .map(|k| {
            let p = dir.join(format!("{k:02}.wav"));
            if let Err(e) = write_test_wav(&p, 44_100, 2, 16, frames) {
                panic!("{e}");
            }
            p
        })
        .collect()
}

fn ui_with(backend: FakeBackend) -> Ui {
    Ui::new(
        move |_device: Option<&str>| Ok(backend.clone()),
        Arc::new(|| {}),
    )
}

fn clock_backend() -> FakeBackend {
    let mut b = FakeBackend::new(&ALL);
    b.device = FakeDevice::Clock;
    b
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

fn all_probed(ui: &mut Ui) -> bool {
    ui.model.entries.iter().all(|e| e.probe != Probe::Pending)
}

const TENTH: u64 = 4_410;

/// 文件头读取：格式与声明的帧数与独立参考一致；无法识别的文件给出原因。
#[test]
fn probe_reads_format_and_declared_frames() {
    let mut p = match Prober::new() {
        Ok(p) => p,
        Err(e) => panic!("{e}"),
    };
    // FLAC：参考来自 .meta（flac 命令行生成时记录）。
    let flac = p.probe(&root().join("qy-codec-flac/tests/fixtures/cli_16_3ch_48k.flac"));
    let Ok(info) = flac else { panic!("{flac:?}") };
    assert_eq!(info.kind, Kind::Flac);
    assert_eq!(
        (
            info.format.sample_rate(),
            info.format.channels(),
            info.format.bits()
        ),
        (48_000, 3, 16)
    );
    assert_eq!(info.frames, Some(6000));
    assert_eq!(text::format_column(&info), "FLAC 48 kHz 16 位 3 声道");

    // AIFF-C：.expect 的样本总数 = 帧数 × 声道数。
    let aiff = root().join("qy-codec-aiff/tests/fixtures/aifc_none_16_stereo");
    let expect = std::fs::read_to_string(aiff.with_extension("expect")).unwrap_or_default();
    let samples: u64 = expect
        .lines()
        .find_map(|l| l.strip_prefix("samples "))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let r = p.probe(&aiff.with_extension("aiff"));
    let Ok(info) = r else { panic!("{r:?}") };
    assert_eq!(info.kind, Kind::Aiff);
    assert_eq!(info.frames.map(|f| f * 2), Some(samples));

    // WAV：测试自己写入的文件。
    let dir = temp("probe");
    let w = tracks(&dir, 1, 88_200);
    let r = p.probe(&w[0]);
    let Ok(info) = r else { panic!("{r:?}") };
    assert_eq!((info.kind, info.frames), (Kind::Wav, Some(88_200)));
    assert_eq!(text::columns(&Probe::Ready(info), "00.wav")[1], "0:02");

    // 无法识别与不存在的文件。
    let bad = dir.join("bad.wav");
    let _ = std::fs::write(&bad, b"not audio at all");
    let r = p.probe(&bad);
    assert!(r.as_ref().is_err_and(|e| e.contains("无法识别")), "{r:?}");
    assert!(p.probe(&dir.join("missing.wav")).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

/// 文件夹扫描：递归、只收音频扩展名（不区分大小写）、按路径排序。
#[test]
fn folder_scan_is_recursive_filtered_and_sorted() {
    let dir = temp("scan");
    let sub = dir.join("b");
    let _ = std::fs::create_dir_all(&sub);
    for f in ["z.flac", "a.WAV", "notes.txt", "b/c.aiff", "b/d.mp3"] {
        let _ = std::fs::write(dir.join(f), b"x");
    }
    let found = scan_folder(&dir);
    let names: Vec<String> = found
        .iter()
        .filter_map(|p| p.strip_prefix(&dir).ok())
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    assert_eq!(names, ["a.WAV", "b/c.aiff", "z.flac"]);

    // 经界面加入文件夹：在读取线程扫描，结果加入列表并读取文件头（内容无效时为“无法播放”）。
    let mut ui = ui_with(clock_backend());
    ui.add_paths(vec![dir.clone()]);
    assert!(until(&mut ui, 10, |u| u.model.entries.len() == 3 && all_probed(u)));
    assert!(
        ui.model
            .entries
            .iter()
            .all(|e| matches!(e.probe, Probe::Failed(_)))
    );
    drop(ui);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 完整会话与控制：播放、暂停、恢复、下一首、按列表跳曲，结束后报告无欠载；暂停期间状态
/// 显示暂停；正在播放的标记跟随曲目。
#[test]
fn session_controls_follow_engine_rules() {
    let dir = temp("ctl");
    let files = tracks(&dir, 4, 13_230);
    let mut ui = ui_with(clock_backend());
    ui.add_paths(files);
    assert!(until(&mut ui, 10, all_probed));
    ui.play_from(0);
    assert_eq!(text::status_line(&ui.model), "正在打开…");
    let mut stage = 0;
    let mut saw_paused = false;
    let ok = until(&mut ui, 30, |u| {
        let Some(s) = u.model.playing.as_ref().and_then(|p| p.status) else {
            return u.model.playing.is_none();
        };
        saw_paused |= s.paused && text::status_line(&u.model).starts_with('‖');
        match stage {
            0 if s.track == 0 && s.position_frames >= TENTH => {
                u.pause();
                stage = 1;
            }
            1 if s.paused && s.paused_frames >= TENTH => {
                u.resume();
                stage = 2;
            }
            2 if !s.paused && s.position_frames >= 2 * TENTH => {
                u.next();
                stage = 3;
            }
            3 if s.track == 1 && s.position_frames >= TENTH => {
                assert_eq!(u.model.playing_entry(), Some(u.model.entries[1].id));
                u.play_from(3);
                stage = 4;
            }
            _ => {}
        }
        false
    });
    assert!(ok, "会话未结束");
    assert_eq!(stage, 4);
    assert!(saw_paused);
    let Some(o) = ui.model.last.clone() else {
        panic!("无结果")
    };
    assert!(
        o.finished && o.underrun_events == 0 && o.user_skips == 2,
        "{}",
        o.summary
    );
    assert!(o.summary.contains("暂停：送出静音"), "{}", o.summary);
    assert_eq!(text::outcome_line(&o), "播放完成（有曲目被切走，没有放完）");
    assert!(ui.model.log.iter().any(|l| l.contains("▶ 曲目 1/4")));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 界面线程卡顿（两次各 1 秒不处理事件）不影响播放：无欠载，严格路径完整。
#[test]
fn ui_stall_does_not_cause_underrun() {
    let dir = temp("stall");
    let files = tracks(&dir, 2, 66_150);
    let mut ui = ui_with(clock_backend());
    ui.add_paths(files);
    assert!(until(&mut ui, 10, all_probed));
    ui.play_from(0);
    std::thread::sleep(Duration::from_secs(1));
    ui.pump();
    std::thread::sleep(Duration::from_secs(1));
    assert!(until(&mut ui, 30, |u| u.model.playing.is_none()));
    let Some(o) = ui.model.last.clone() else {
        panic!("无结果")
    };
    assert!(o.strict_intact && o.underrun_events == 0, "{}", o.summary);
    assert!(text::status_line(&ui.model).starts_with("播放完成：严格路径"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 停止后立即开始新会话：旧会话的状态与结束报告不显示为当前状态；停止的会话标记为中断。
#[test]
fn stop_then_new_session_ignores_stale_events() {
    let dir = temp("stale");
    let files = tracks(&dir, 2, 22_050);
    let mut ui = ui_with(clock_backend());
    ui.add_paths(files);
    assert!(until(&mut ui, 10, all_probed));
    ui.play_from(0);
    assert!(until(&mut ui, 10, |u| u
        .model
        .playing
        .as_ref()
        .and_then(|p| p.status)
        .is_some_and(|s| s.position_frames >= TENTH)));
    ui.stop();
    ui.model.entries.remove(0);
    ui.play_from(0);
    let session = ui.model.playing.as_ref().map(|p| p.session);
    assert!(until(&mut ui, 30, |u| u.model.playing.is_none()));
    let Some(o) = ui.model.last.clone() else {
        panic!("无结果")
    };
    assert!(o.strict_intact, "新会话应完整：{}", o.summary);
    assert_eq!(session, Some(1));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 输出设备失效与设备不可用：结束报告给出原因，界面回到非播放状态。
#[test]
fn device_failures_are_reported() {
    let dir = temp("dev");
    let files = tracks(&dir, 1, 22_050);
    let mut b = FakeBackend::new(&ALL);
    b.invalidate = Some((0, 5));
    let mut ui = ui_with(b);
    ui.add_paths(files.clone());
    ui.play_from(0);
    assert!(until(&mut ui, 30, |u| u.model.playing.is_none()));
    let o = ui.model.last.clone();
    assert!(o.as_ref().is_some_and(|o| o.device_error), "{o:?}");
    assert_eq!(text::status_line(&ui.model), "输出设备错误：播放已停止");

    let mut ui = Ui::new(
        |_d: Option<&str>| -> Result<FakeBackend, String> { Err("找不到设备".into()) },
        Arc::new(|| {}),
    );
    ui.add_paths(files);
    ui.play_from(0);
    assert!(until(&mut ui, 10, |u| u.model.playing.is_none()));
    assert_eq!(
        text::status_line(&ui.model),
        "错误：无法使用输出设备：找不到设备"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 播放中关闭界面：后台线程停止并退出，不挂起。
#[test]
fn dropping_ui_while_playing_stops_threads() {
    let dir = temp("drop");
    let files = tracks(&dir, 1, 441_000);
    let mut ui = ui_with(clock_backend());
    ui.add_paths(files);
    ui.play_from(0);
    let t0 = Instant::now();
    drop(ui);
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    let _ = std::fs::remove_dir_all(&dir);
}

/// 单声道复制选项（O1）在界面中的行为，设备不接受单声道时：
/// - 默认关闭：单声道曲目打开失败，日志开头写明选项为关闭（严格路径）；
/// - 勾选后重新播放：按 [FL FR] 播放，播放中保真行写明声道已改变，结果不是严格路径；
/// - 播放中勾选：选项发给进行中的会话，跳到单声道曲目即按 [FL FR] 播放。
#[test]
fn mono_to_stereo_option_reaches_sessions() {
    let dir = temp("mono");
    let mono = dir.join("mono.wav");
    if let Err(e) = write_test_wav(&mono, 44_100, 1, 16, 44_100) {
        panic!("{e}");
    }
    let stereo = tracks(&dir, 1, 44_100);
    let mut b = FakeBackend::with_negotiator(&ALL, NoMonoNegotiator);
    b.device = FakeDevice::Clock;
    let mut ui = Ui::new(move |_device: Option<&str>| Ok(b.clone()), Arc::new(|| {}));
    let log = |ui: &Ui, text: &str| ui.model.log.iter().any(|l| l.contains(text));

    ui.add_paths(vec![mono.clone()]);
    assert!(until(&mut ui, 10, all_probed));
    ui.play_from(0);
    assert!(until(&mut ui, 10, |u| u.model.playing.is_none()));
    let o = ui.model.last.clone();
    assert!(o.as_ref().is_some_and(|o| !o.finished), "{o:?}");
    assert!(log(&ui, "单声道复制到左右两路：关闭（严格路径"));
    assert!(log(&ui, "设备不接受单声道"));
    assert_eq!(
        text::status_line(&ui.model),
        "播放已停止：曲目 1 无法打开输出设备：设备不接受单声道"
    );

    let c = ui.set_mono_to_stereo(true);
    assert!(c.status && ui.model.mono_to_stereo);
    ui.play_from(0);
    let mut saw_altered = false;
    assert!(until(&mut ui, 10, |u| {
        saw_altered |= text::fidelity_line(&u.model).starts_with("声道已改变：本次播放有 1 首");
        u.model.playing.is_none()
    }));
    let Some(o) = ui.model.last.clone() else {
        panic!("无结果")
    };
    assert!(
        o.finished && o.software_intact && !o.strict_intact && o.error.is_none(),
        "{}",
        o.summary
    );
    assert!(saw_altered);
    assert!(log(&ui, "单声道复制到左右两路：开启"));
    assert!(log(&ui, "曲目 1 声道已改变"));

    // 播放中勾选：先关闭，播放立体声曲目时开启并跳到单声道曲目。
    ui.set_mono_to_stereo(false);
    ui.clear();
    ui.add_paths(vec![stereo[0].clone(), mono]);
    assert!(until(&mut ui, 10, all_probed));
    ui.play_from(0);
    let mut stage = 0;
    assert!(until(&mut ui, 20, |u| {
        if stage == 0
            && let Some(s) = u.model.playing.as_ref().and_then(|p| p.status)
            && s.track == 0
            && s.position_frames >= TENTH
        {
            u.set_mono_to_stereo(true);
            u.play_from(1);
            stage = 1;
        }
        u.model.playing.is_none()
    }));
    assert_eq!(stage, 1);
    let Some(o) = ui.model.last.clone() else {
        panic!("无结果")
    };
    assert!(
        o.finished && o.user_skips == 1 && !o.strict_intact,
        "{}",
        o.summary
    );
    assert!(log(&ui, "单声道复制到左右两路：已开启"));
    assert!(log(&ui, "曲目 2 声道已改变"), "{}", o.summary);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 读取限速（故障注入，低于曲目码率）产生真实欠载：播放中状态栏写明欠载次数与缺失帧，保真行
/// 不再是完整保真；结束结果与报告一致。
#[test]
fn slow_reads_show_underruns() {
    let dir = temp("slow");
    let files = tracks(&dir, 1, 44_100);
    let mut ui = ui_with(clock_backend());
    // 44.1 kHz 16 位双声道为 176 400 字节/秒，限速为其 1/4（预填 500 ms 之后必然欠载）。
    ui.model.read_limit = Some(44_100);
    ui.add_paths(files);
    assert!(until(&mut ui, 10, all_probed));
    ui.play_from(0);
    let (mut status, mut fidelity) = (String::new(), String::new());
    assert!(until(&mut ui, 20, |u| {
        if u.model
            .playing
            .as_ref()
            .and_then(|p| p.status)
            .is_some_and(|s| s.underrun_events > 0)
        {
            status = text::status_line(&u.model);
            fidelity = text::fidelity_line(&u.model);
        }
        u.model.playing.is_none()
    }));
    assert!(
        status.contains("欠载 ") && status.contains("帧）"),
        "{status} {:?}",
        ui.model.last.as_ref().map(|o| &o.summary)
    );
    assert!(fidelity.starts_with("有欠载"), "{fidelity}");
    let Some(o) = ui.model.last.clone() else {
        panic!("无结果")
    };
    assert!(
        o.underrun_events > 0 && !o.software_intact && !o.strict_intact,
        "{}",
        o.summary
    );
    assert!(o.summary.contains("欠载"), "{}", o.summary);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 44.1 kHz 16 位双声道、各样本不同（不周期重复）的 WAV；返回 data 块。
fn distinct_wav(path: &Path, frames: usize) -> Vec<u8> {
    let data: Vec<u8> = (0..frames as u32 * 2)
        .flat_map(|i| ((i.wrapping_mul(2_654_435_761) >> 16) as u16).to_le_bytes())
        .collect();
    let mut f = Vec::new();
    f.extend_from_slice(b"RIFF");
    f.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    f.extend_from_slice(b"WAVEfmt ");
    f.extend_from_slice(&16u32.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&2u16.to_le_bytes());
    f.extend_from_slice(&44_100u32.to_le_bytes());
    f.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
    f.extend_from_slice(&4u16.to_le_bytes());
    f.extend_from_slice(&16u16.to_le_bytes());
    f.extend_from_slice(b"data");
    f.extend_from_slice(&(data.len() as u32).to_le_bytes());
    f.extend_from_slice(&data);
    if let Err(e) = std::fs::write(path, f) {
        panic!("{e}");
    }
    data
}

/// S1：界面定位。按帧、按进度条千分比与按秒（←/→）定位：新的输出流从目标帧起与文件数据逐字节
/// 一致，状态位置从目标帧算起，结束报告写明按操作定位；向前越过开头时从头开始。
#[test]
fn seek_from_ui_restarts_at_target() {
    let dir = temp("seek");
    let path = dir.join("distinct.wav");
    let data = distinct_wav(&path, 44_100);
    let b = clock_backend();
    let outputs = b.clone();
    let mut ui = ui_with(b);
    ui.add_paths(vec![path]);
    assert!(until(&mut ui, 10, all_probed));
    type Act = fn(&mut Ui) -> Option<u64>;
    let cases: [(&str, Act, u64); 3] = [
        ("帧", |u| u.seek(20_000).then_some(20_000), 20_000),
        ("千分比", |u| u.seek_permille(500), 22_050),
        ("向前越过开头", |u| u.seek_by(-10), 0),
    ];
    for (what, act, target) in cases {
        let before = outputs.outputs().len();
        ui.play_from(0);
        let mut done = false;
        let mut seen_after = 0u64;
        assert!(until(&mut ui, 20, |u| {
            if let Some((pos, total, rate)) = u.model.playing_position() {
                assert_eq!((total, rate), (44_100, 44_100));
                if !done && pos >= TENTH {
                    assert_eq!(act(u), Some(target), "{what}");
                    // 播放线程执行定位之前，位置已按目标帧显示（进度条不被旧快照拉回原位置）。
                    assert_eq!(
                        u.model.playing_position().map(|p| p.0),
                        Some(target),
                        "{what}"
                    );
                    done = true;
                } else if done && pos >= target {
                    seen_after = seen_after.max(pos);
                }
            }
            u.model.playing.is_none()
        }));
        assert!(done, "{what}");
        let Some(o) = ui.model.last.clone() else {
            panic!("无结果")
        };
        assert!(
            o.summary.contains("按操作定位 1 次"),
            "{what}：{}",
            o.summary
        );
        assert!(!o.software_intact, "{what}");
        let outs = outputs.outputs();
        assert_eq!(outs.len(), before + 2, "{what}");
        let last = &outs[outs.len() - 1];
        let valid = last.report.valid_frames as usize * 4;
        assert!(
            last.rendered[..valid] == data[target as usize * 4..],
            "{what}：定位后输出 {valid} 字节"
        );
        assert!(seen_after >= target, "{what}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn text_helpers() {
    assert_eq!(text::khz(44_100), "44.1");
    assert_eq!(text::khz(48_000), "48");
    assert_eq!(text::khz(22_050), "22.05");
    assert_eq!(text::khz(352_800), "352.8");
    assert_eq!(text::clock(59), "0:59");
    assert_eq!(text::clock(3_725), "1:02:05");
    assert_eq!(text::duration(441_000, 44_100), "0:10");
    assert_eq!(text::duration(1, 0), "");
}

/// L1：窗口最小化期间不重绘，但模型照常更新（状态、日志与结束报告都不丢）；恢复显示时把期间的
/// 变化一次重绘。
#[test]
fn hidden_window_defers_redraw_without_losing_events() {
    let dir = temp("hidden");
    let files = tracks(&dir, 2, 4_410);
    let mut ui = ui_with(clock_backend());
    let mut view = Deferred::default();
    ui.add_paths(files);
    assert!(until(&mut ui, 10, all_probed));
    assert_eq!(view.set_visible(false), Changes::default());
    ui.play_from(0);
    let mut redrawn = Changes::default();
    let t0 = Instant::now();
    let mut ended = false;
    while !ended && t0.elapsed() < Duration::from_secs(30) {
        redrawn.merge(view.filter(ui.wait(Duration::from_millis(20))));
        ended = ui.model.playing.is_none() && ui.model.last.is_some();
    }
    assert!(ended, "会话未结束");
    assert!(!redrawn.any(), "最小化期间不应重绘：{redrawn:?}");
    let Some(o) = ui.model.last.clone() else {
        panic!("无结果")
    };
    assert!(o.finished, "{}", o.summary);
    assert!(ui.model.log.iter().any(|l| l.contains("▶ 曲目 2/2")));
    let shown = view.set_visible(true);
    assert!(shown.status && shown.log && shown.session, "{shown:?}");
    // 可见后照常重绘，暂存已清空。
    assert_eq!(view.filter(Changes::default()), Changes::default());
    assert_eq!(view.set_visible(true), Changes::default());
    let _ = std::fs::remove_dir_all(&dir);
}
