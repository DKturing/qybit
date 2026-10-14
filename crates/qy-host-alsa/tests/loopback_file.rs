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

use common::{
    cases, device_bytes, make_tracks, report_dir, sync_samples, test_samples, write_wav,
    write_wav_mask,
};
use qy_host_alsa::AlsaBackend;
use qy_host_alsa::attempt::PlaybackReport;
use qy_host_alsa::linux::{RealOpener, RealPcm};
use qy_host_alsa::loopback::{Layer, LoopEntry, LoopReport, Verdict, align_and_compare};
use qy_host_alsa::pcm::{Configured, HwPlan, Opener, Pcm, PcmError};
use qy_pcm_format::alsa::{AlsaFormat, ChmapType};
use qy_player::testing::FakePriority;
use qy_player::{HostResources, play};

/// 把每次打开映射到 `file` 插件的新文件。
#[derive(Clone, Default)]
struct FileOpener {
    dir: PathBuf,
    files: Arc<Mutex<Vec<PathBuf>>>,
    /// 测试声明的固定声道映射（ALSA 位置值）。`file` 插件本身不报告映射，多声道用例由测试
    /// 声明一个固定映射，报告中明确标注；`None` 时原样使用 alsa-lib 的结果。
    declared: Option<Vec<u32>>,
}

/// 真实 `file` 插件 PCM；声道映射查询/读回可由测试声明（见 [`FileOpener::declared`]），
/// 其余调用全部原样转给 alsa-lib。
struct FilePcm {
    inner: RealPcm,
    declared: Option<Vec<u32>>,
}

impl Pcm for FilePcm {
    fn supported_formats(&mut self) -> Result<Vec<AlsaFormat>, PcmError> {
        self.inner.supported_formats()
    }
    fn configure(&mut self, plan: &HwPlan) -> Result<Configured, PcmError> {
        self.inner.configure(plan)
    }
    fn hw_free(&mut self) -> Result<(), PcmError> {
        self.inner.hw_free()
    }
    fn start(&mut self) -> Result<(), PcmError> {
        self.inner.start()
    }
    fn drop_now(&mut self) -> Result<(), PcmError> {
        self.inner.drop_now()
    }
    fn drain(&mut self) -> Result<(), PcmError> {
        self.inner.drain()
    }
    fn prepare(&mut self) -> Result<(), PcmError> {
        self.inner.prepare()
    }
    fn wait(&mut self, timeout_ms: u32) -> Result<bool, PcmError> {
        self.inner.wait(timeout_ms)
    }
    fn avail(&mut self) -> Result<u64, PcmError> {
        self.inner.avail()
    }
    fn writei(&mut self, frames: &[u8]) -> Result<usize, PcmError> {
        self.inner.writei(frames)
    }
    fn xrun_missing_frames(&mut self) -> u32 {
        self.inner.xrun_missing_frames()
    }
    fn query_chmaps(&mut self) -> Vec<(ChmapType, Vec<u32>)> {
        match &self.declared {
            Some(m) => vec![(ChmapType::Fixed, m.clone())],
            None => self.inner.query_chmaps(),
        }
    }
    fn set_chmap(&mut self, positions: &[u32]) -> Result<(), PcmError> {
        match &self.declared {
            Some(_) => Err(PcmError {
                call: "snd_pcm_set_chmap",
                errno: qy_host_alsa::pcm::errno::EINVAL,
            }),
            None => self.inner.set_chmap(positions),
        }
    }
    fn get_chmap(&mut self) -> Result<Option<Vec<u32>>, PcmError> {
        match &self.declared {
            Some(m) => Ok(Some(m.clone())),
            None => self.inner.get_chmap(),
        }
    }
}

impl Opener for FileOpener {
    type Pcm = FilePcm;

    fn open(&mut self, _name: &str) -> Result<FilePcm, PcmError> {
        let mut files = self.files.lock().map_err(|_| PcmError {
            call: "FileOpener",
            errno: 0,
        })?;
        let path = self.dir.join(format!("capture-{}.raw", files.len()));
        files.push(path.clone());
        Ok(FilePcm {
            inner: RealOpener.open(&format!("file:FILE={},FORMAT=raw", path.display()))?,
            declared: self.declared.clone(),
        })
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

/// alsa-lib 能否打开内置 `null` 插件。CI 中（设置了 `CI`）打不开即失败，确保 A 层确实运行；
/// 本地缺少 alsa-lib 配置时明确打印“未运行”。
fn alsa_null_available() -> bool {
    match RealOpener.open("null") {
        Ok(_) => true,
        Err(e) => {
            assert!(
                std::env::var_os("CI").is_none(),
                "CI 中 A 层必须运行，但 alsa-lib 无法打开 null 插件（{e}）"
            );
            eprintln!("未运行：alsa-lib 无法打开 null 插件（{e}）");
            false
        }
    }
}

/// 按扬声器位置独立重排每帧样本：`order[i]` 为设备第 i 个声道对应的源声道下标。
fn permute(samples: &[i32], order: &[usize]) -> Vec<i32> {
    samples
        .chunks_exact(order.len())
        .flat_map(|f| order.iter().map(move |&i| f[i]))
        .collect()
}

/// F1 多声道：`file` 插件不报告声道映射，测试声明固定映射后走真实路径回环。
/// 设备顺序与源一致时应原样提交；不一致（HDA 常见 5.1 顺序）时应按位置纯置换。
fn multichannel_extra(dir: &Path, res: &mut HostResources) -> Vec<(String, bool, String)> {
    // ALSA 位置值：FL=3 FR=4 RL=5 RR=6 FC=7 LFE=8 SL=9 SR=10（`asound.h`）。
    struct Mc {
        label: &'static str,
        mask: u32,
        bits: u8,
        format: AlsaFormat,
        map: [u32; 6],
        /// 设备第 i 声道对应的源（WAVE 升序）声道下标。
        order: [usize; 6],
    }
    let cases = [
        Mc {
            label: "5.1 后环绕 0x3F，设备映射与源同序（FL FR FC LFE RL RR），48k/S24_3LE",
            mask: 0x3F,
            bits: 24,
            format: AlsaFormat::S24_3Le,
            map: [3, 4, 7, 8, 5, 6],
            order: [0, 1, 2, 3, 4, 5],
        },
        Mc {
            label: "5.1 后环绕 0x3F，设备映射 HDA 序（FL FR RL RR FC LFE），48k/S16_LE",
            mask: 0x3F,
            bits: 16,
            format: AlsaFormat::S16Le,
            map: [3, 4, 5, 6, 7, 8],
            order: [0, 1, 4, 5, 2, 3],
        },
        Mc {
            label: "5.1 侧环绕 0x60F，设备映射（FL FR SL SR FC LFE），48k/S32_LE",
            mask: 0x60F,
            bits: 24,
            format: AlsaFormat::S32Le,
            map: [3, 4, 9, 10, 7, 8],
            order: [0, 1, 4, 5, 2, 3],
        },
    ];
    let mut out = Vec::new();
    for (i, c) in cases.iter().enumerate() {
        let sync = sync_samples(c.bits, 6, 300 + i as u64);
        let test = test_samples(c.bits, 6, 1500, 400 + i as u64);
        let sync_path = dir.join(format!("mc{i}-sync.wav"));
        let test_path = dir.join(format!("mc{i}-test.wav"));
        write_wav_mask(&sync_path, 48_000, 6, c.bits, &sync, Some(c.mask));
        write_wav_mask(&test_path, 48_000, 6, c.bits, &test, Some(c.mask));
        let mut b = backend(dir, Some(c.format));
        b.opener.declared = Some(c.map.to_vec());
        let r = play(&mut b, res, &[sync_path, test_path]);
        let captured = started_files(&b, &r).pop().unwrap_or_default();
        let (_, v) = align_and_compare(
            &captured,
            &device_bytes(&permute(&sync, &c.order), c.bits, c.format),
            &device_bytes(&permute(&test, &c.order), c.bits, c.format),
        );
        let note = r
            .streams
            .first()
            .and_then(|s| s.negotiation.chosen_attempt())
            .and_then(|a| a.chmap.clone())
            .unwrap_or_default();
        let ok = r.software_path_intact() && r.streams.len() == 1 && v == Verdict::Match;
        out.push((
            format!(
                "多声道（测试声明的固定声道映射，file 插件本身不报告映射）：{}",
                c.label
            ),
            ok,
            format!("{v:?}，流数 {}；{note}", r.streams.len()),
        ));
    }
    // 设备没有任何映射时，多于 2 声道必须拒绝（不假定默认布局）。
    {
        let test = test_samples(16, 6, 600, 500);
        let path = dir.join("mc-nomap.wav");
        write_wav_mask(&path, 48_000, 6, 16, &test, Some(0x3F));
        let mut b = backend(dir, Some(AlsaFormat::S16Le));
        let r = play(&mut b, res, &[path]);
        let ok = r.streams.is_empty() && !r.open_failures.is_empty();
        out.push((
            "多声道、设备不报告声道映射（file 插件原样）：明确拒绝".to_string(),
            ok,
            r.open_failures
                .first()
                .map(|f| f.message.clone())
                .unwrap_or_else(|| r.summary()),
        ));
    }
    out
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
            watermark: None,
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
            entry.watermark = r
                .streams
                .first()
                .and_then(|s| s.render.as_ref())
                .map(|x| x.watermark);
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

    extra.extend(multichannel_extra(&dir, &mut res));

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
