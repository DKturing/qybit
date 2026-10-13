//! M2 T4 A 层：alsa-lib `file` 插件数字回环（仅 Linux，不需要内核模块，CI 运行）。
//!
//! 播放走真实路径：`qy_player::play` → 核心 → 自有解码 → pack → 队列 → 短写渲染循环 →
//! alsa crate → alsa-lib。打开器把 `hw:` 设备名映射到 alsa-lib 内置定义
//! `file:FILE=<路径>,FORMAT=raw`（`type file`、`slave.pcm null`，见 `/usr/share/alsa/alsa.conf`），
//! 提交给 alsa-lib 的字节被原样写入文件，再按同步序列对齐、逐字节比对。
//!
//! 说明：任务卡写的是“生成临时 asoundrc”。alsa crate 0.12.1 没有 `snd_pcm_open_lconf`，
//! 进程内改 `HOME`/`ALSA_CONFIG_PATH` 又需要 `unsafe` 的 `set_var`；alsa-lib 内置的
//! `pcm.file` 定义与之等价（同为 type file、slave null），并能按每次打开指定文件路径。
//!
//! 这一层不经过任何驱动，只证明软件路径提交正确，不证明设备端 bit-perfect。
#![cfg(target_os = "linux")]

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use common::{cases, device_bytes, make_tracks, report_dir, sync_samples, test_samples, write_wav};
use qy_host_alsa::AlsaBackend;
use qy_host_alsa::attempt::PlaybackReport;
use qy_host_alsa::linux::{RealOpener, RealPcm};
use qy_host_alsa::loopback::{Layer, LoopEntry, LoopReport, Verdict, align_and_compare};
use qy_host_alsa::pcm::{Opener, PcmError};
use qy_pcm_format::alsa::AlsaFormat;
use qy_player::testing::FakePriority;
use qy_player::{HostResources, play};

/// 把每次打开映射到 `file` 插件的新文件。
#[derive(Clone, Default)]
struct FileOpener {
    dir: PathBuf,
    files: Arc<Mutex<Vec<PathBuf>>>,
}

impl Opener for FileOpener {
    type Pcm = RealPcm;

    fn open(&mut self, _name: &str) -> Result<RealPcm, PcmError> {
        let mut files = self.files.lock().map_err(|_| PcmError {
            call: "FileOpener",
            errno: 0,
        })?;
        let path = self.dir.join(format!("capture-{}.raw", files.len()));
        files.push(path.clone());
        RealOpener.open(&format!("file:FILE={},FORMAT=raw", path.display()))
    }
}

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("qybit-loop-file-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    if let Err(e) = std::fs::create_dir_all(&d) {
        panic!("{e}");
    }
    d
}

fn backend(dir: &Path, forced: Option<AlsaFormat>) -> AlsaBackend<FileOpener, FakePriority> {
    let mut b = AlsaBackend::new(
        "hw:CARD=FileLoop,DEV=0",
        FileOpener {
            dir: dir.to_path_buf(),
            ..FileOpener::default()
        },
        FakePriority::default(),
    );
    b.forced = forced;
    b
}

fn started_files(b: &AlsaBackend<FileOpener, FakePriority>, r: &PlaybackReport) -> Vec<Vec<u8>> {
    // 每个成功打开的流对应一次打开；失败的协商不会产生流。
    let files = b.opener.files.lock().map(|f| f.clone()).unwrap_or_default();
    let n = r.streams.len();
    files[files.len() - n..]
        .iter()
        .map(|p| std::fs::read(p).unwrap_or_default())
        .collect()
}

fn alsa_null_available() -> bool {
    match RealOpener.open("null") {
        Ok(_) => true,
        Err(e) => {
            eprintln!("未运行：alsa-lib 无法打开 null 插件（{e}）");
            false
        }
    }
}

#[test]
fn file_plugin_loopback_matrix() {
    if !alsa_null_available() {
        return;
    }
    let dir = tmpdir("matrix");
    let Ok(mut res) = HostResources::new() else {
        panic!("res");
    };
    let mut entries = Vec::new();
    for (i, case) in cases().into_iter().enumerate() {
        let tracks = make_tracks(&dir, &format!("c{i}"), &case, 2048, 1000 + i as u64);
        let mut b = backend(&dir, Some(case.format));
        let r = play(
            &mut b,
            &mut res,
            &[tracks.sync_path.clone(), tracks.test_path.clone()],
        );
        let attempt = r
            .streams
            .first()
            .and_then(|s| s.negotiation.chosen_attempt())
            .cloned();
        let mut entry = LoopEntry {
            case,
            request: attempt.as_ref().and_then(|a| a.request),
            applied: attempt.as_ref().and_then(|a| a.applied),
            captured_bytes: 0,
            sync_offset: None,
            compared_bytes: 0,
            verdict: Verdict::Failed(String::new()),
        };
        if case.source_bits > case.format.valid_bits() {
            entry.verdict = if r.streams.is_empty() && !r.open_failures.is_empty() {
                Verdict::Rejected(r.open_failures[0].message.clone())
            } else {
                Verdict::Failed(format!("N > V 未被拒绝：{}", r.summary()))
            };
        } else if !r.software_path_intact() || r.streams.len() != 1 {
            entry.verdict = Verdict::Failed(r.summary());
        } else {
            let captured = started_files(&b, &r).pop().unwrap_or_default();
            let (off, v) = align_and_compare(&captured, &tracks.sync_bytes, &tracks.test_bytes);
            entry.captured_bytes = captured.len();
            entry.sync_offset = off;
            entry.compared_bytes = tracks.test_bytes.len();
            entry.verdict = v;
        }
        entries.push(entry);
    }

    let mut extra = Vec::new();
    // 同格式多曲无缝衔接：同步 + 两首待测在一个流内连续。
    {
        let case = qy_host_alsa::loopback::LoopCase {
            rate: 48_000,
            channels: 2,
            format: AlsaFormat::S24_3Le,
            source_bits: 24,
        };
        let t = make_tracks(&dir, "gapless", &case, 3000, 7);
        let second = test_samples(24, 2, 1500, 99);
        let second_path = dir.join("gapless-second.wav");
        write_wav(&second_path, 48_000, 2, 24, &second);
        let mut b = backend(&dir, None);
        let r = play(&mut b, &mut res, &[t.sync_path, t.test_path, second_path]);
        let captured = started_files(&b, &r).pop().unwrap_or_default();
        let want = [t.test_bytes, device_bytes(&second, 24, AlsaFormat::S24_3Le)].concat();
        let (_, v) = align_and_compare(&captured, &t.sync_bytes, &want);
        let ok = r.software_path_intact() && r.streams.len() == 1 && v == Verdict::Match;
        extra.push((
            "同格式多曲无缝衔接（48 kHz/2ch/S24_3LE，三曲一个流）".to_string(),
            ok,
            format!("{v:?}，流数 {}", r.streams.len()),
        ));
    }
    // 不同格式换曲后重新打开：16 位 → 24 位，两个流各自对齐比对。
    {
        let s16 = sync_samples(16, 2, 11);
        let t16 = test_samples(16, 2, 1200, 12);
        let s24 = sync_samples(24, 1, 13);
        let t24 = test_samples(24, 1, 1200, 14);
        let paths: Vec<PathBuf> = ["fc-s16", "fc-t16", "fc-s24", "fc-t24"]
            .iter()
            .map(|n| dir.join(format!("{n}.wav")))
            .collect();
        write_wav(&paths[0], 44_100, 2, 16, &s16);
        write_wav(&paths[1], 44_100, 2, 16, &t16);
        write_wav(&paths[2], 96_000, 1, 24, &s24);
        write_wav(&paths[3], 96_000, 1, 24, &t24);
        let mut b = backend(&dir, None);
        let r = play(&mut b, &mut res, &paths);
        let caps = started_files(&b, &r);
        let v1 = caps.first().map(|c| {
            align_and_compare(
                c,
                &device_bytes(&s16, 16, AlsaFormat::S16Le),
                &device_bytes(&t16, 16, AlsaFormat::S16Le),
            )
            .1
        });
        let v2 = caps.get(1).map(|c| {
            align_and_compare(
                c,
                &device_bytes(&s24, 24, AlsaFormat::S24_3Le),
                &device_bytes(&t24, 24, AlsaFormat::S24_3Le),
            )
            .1
        });
        let ok = r.software_path_intact()
            && r.streams.len() == 2
            && v1 == Some(Verdict::Match)
            && v2 == Some(Verdict::Match);
        extra.push((
            "不同格式换曲后重新打开（44.1k/2ch/S16_LE → 96k/1ch/S24_3LE）".to_string(),
            ok,
            format!("{v1:?} / {v2:?}，流数 {}", r.streams.len()),
        ));
    }

    let report = LoopReport {
        layer: Layer::File,
        device: "file:FILE=…,FORMAT=raw（slave null）".into(),
        entries,
        extra,
    };
    match report.write(&report_dir()) {
        Ok(p) => eprintln!("A 层报告：{}", p.display()),
        Err(e) => panic!("写报告失败: {e}"),
    }
    let total = report.entries.len();
    let matched = report
        .entries
        .iter()
        .filter(|e| e.verdict == Verdict::Match)
        .count();
    let rejected = report
        .entries
        .iter()
        .filter(|e| matches!(e.verdict, Verdict::Rejected(_)))
        .count();
    eprintln!("A 层：{total} 项，逐字节一致 {matched}，N > V 拒绝 {rejected}");
    let bad: Vec<_> = report
        .entries
        .iter()
        .filter(|e| !e.verdict.ok_for(&e.case))
        .collect();
    assert!(bad.is_empty(), "不通过的用例：{bad:#?}");
    assert!(report.extra.iter().all(|x| x.1), "{:#?}", report.extra);
    let _ = std::fs::remove_dir_all(&dir);
}
