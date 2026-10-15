//! S1：曲目内定位的样本级精确性。真实 qy-core 输入窗口 + `AutoDecoder`（自有 WAV / AIFF 解码），
//! 宿主按随机短读、零长度读与随机部分取走输出推进；定位目标取 0、1、中点、最后一帧、等于与超过
//! 长度，以及按种子随机的 64 个。定位后的输出须等于独立参考（`qy-gen-fixtures` 的 `.expect`）从
//! 目标帧起的切片；音源可定位时核心发出 `Seek` 且不改为顺序解码，不可定位时（`SeekFailed`）改为
//! 顺序解码，结果相同。

use std::path::{Path, PathBuf};

use qy_codec_auto::AutoDecoder;
use qy_codec_flac::Capacity;
use qy_core::{Action, Core, Decoder, Event, Fault, PCM_WORK_SAMPLES};

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + (self.next() as usize) % (hi - lo + 1)
    }
}

fn leak<T: Copy + Default>(n: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); n].into_boxed_slice())
}

fn new_core() -> Core<AutoDecoder> {
    let cap = Capacity::DESKTOP;
    let Ok(dec) = AutoDecoder::new(
        leak(PCM_WORK_SAMPLES),
        cap,
        leak(cap.workspace_len()),
        leak(cap.pcm_len()),
    ) else {
        panic!("decoder");
    };
    let Ok(core) = Core::with_window(leak(cap.input_window), cap.input_window, dec) else {
        panic!("core");
    };
    core
}

#[derive(Debug, Default)]
struct Run {
    result: Option<Result<(), Fault>>,
    out: Vec<i32>,
    seeks: u32,
    sequential: bool,
    opens: u32,
}

/// 播放一首，从第 `target` 帧开始。`seed` 为 0 时每次读满、取完；否则随机短读与部分取走。
fn play<D: Decoder>(
    core: &mut Core<D>,
    data: &[u8],
    seed: u64,
    target: u64,
    seekable: bool,
) -> Run {
    let mut r = Lcg(seed);
    let mut run = Run::default();
    core.play_at(0, 1, target);
    let mut pos = 0usize;
    for _ in 0..40 * data.len() + 4000 {
        let action = core.poll();
        run.sequential |= core.take_sequential_seek();
        let Some(action) = action else {
            panic!("单线程宿主不应出现无动作等待");
        };
        match action {
            Action::Open { generation, .. } => {
                run.opens += 1;
                pos = 0;
                assert_eq!(core.handle(Event::Opened { generation }), Ok(()));
            }
            Action::Configure { generation, .. } => {
                assert_eq!(core.handle(Event::Configured { generation }), Ok(()));
            }
            Action::Output { generation, frames } => {
                let k = if seed == 0 {
                    frames
                } else {
                    r.range(0, frames)
                };
                let Some(pcm) = core.pcm() else {
                    panic!("Output 时应有 PCM");
                };
                let ch = usize::from(pcm.format().channels());
                run.out.extend_from_slice(&pcm.samples()[..k * ch]);
                assert_eq!(
                    core.handle(Event::Output {
                        generation,
                        frames: k
                    }),
                    Ok(())
                );
            }
            Action::Seek {
                generation,
                request,
                offset,
            } => {
                run.seeks += 1;
                let e = match usize::try_from(offset) {
                    Ok(at) if seekable => {
                        pos = at.min(data.len());
                        Event::SeekDone {
                            generation,
                            request,
                        }
                    }
                    _ => Event::SeekFailed {
                        generation,
                        request,
                    },
                };
                assert_eq!(core.handle(e), Ok(()));
            }
            Action::Read {
                generation,
                request,
                max,
            } => {
                if pos >= data.len() {
                    assert_eq!(
                        core.handle(Event::ReadEof {
                            generation,
                            request
                        }),
                        Ok(())
                    );
                    continue;
                }
                let want = match (seed, r.range(0, 4)) {
                    (0, _) => max,
                    (_, 0) => 0,
                    (_, 1) => 1,
                    (_, 2) => r.range(1, 64),
                    _ => r.range(1, max),
                };
                let n = want.min(max).min(data.len() - pos);
                let Some(dst) = core.read_target(generation, request) else {
                    panic!("read target");
                };
                dst[..n].copy_from_slice(&data[pos..pos + n]);
                pos += n;
                assert_eq!(
                    core.handle(Event::ReadDone {
                        generation,
                        request,
                        len: n
                    }),
                    Ok(())
                );
            }
            Action::Close { .. } => {}
            Action::Finished { .. } => {
                run.result = Some(Ok(()));
                return run;
            }
            Action::Failed { fault, .. } => {
                run.result = Some(Err(fault));
                return run;
            }
        }
    }
    panic!("解码未推进");
}

struct Fx {
    name: String,
    data: Vec<u8>,
    channels: usize,
    samples: Vec<i32>,
}

/// 读取 `.expect`（`result ok` 的夹具）：`format 率 声道 位数` 行与 `samples N` 行之后的样本。
fn load(path: &Path) -> Option<Fx> {
    let text = std::fs::read_to_string(path.with_extension("expect")).ok()?;
    let mut lines = text.lines();
    if lines.next()? != "result ok" {
        return None;
    }
    let channels = text
        .lines()
        .find_map(|l| l.strip_prefix("format "))
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|c| c.parse().ok())?;
    let samples = text
        .lines()
        .skip_while(|l| !l.starts_with("samples "))
        .skip(1)
        .flat_map(|l| {
            l.split(' ')
                .filter(|x| !x.is_empty())
                .map(|x| x.parse::<i32>().unwrap_or(i32::MIN))
                .collect::<Vec<_>>()
        })
        .collect();
    Some(Fx {
        name: path.file_name()?.to_string_lossy().into_owned(),
        data: std::fs::read(path).ok()?,
        channels,
        samples,
    })
}

fn fixtures(dir: &str, ext: &str) -> Vec<Fx> {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(dir);
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&d)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    paths.retain(|p| p.extension().is_some_and(|e| e == ext));
    paths.sort();
    paths.iter().filter_map(|p| load(p)).collect()
}

fn targets(frames: u64, seed: u64) -> Vec<u64> {
    let mut t = vec![
        0,
        1,
        frames / 2,
        frames.saturating_sub(1),
        frames,
        frames + 7,
    ];
    let mut r = Lcg(seed ^ 0x5EE6);
    t.extend((0..64).map(|_| r.next() % (frames + 2)));
    t
}

/// 每个夹具、每个目标：可定位与不可定位两种音源，各一次读满与一次随机短读。
fn check_all(fx: &[Fx], min: usize) {
    assert!(fx.len() >= min, "夹具数 {}", fx.len());
    let mut core = new_core();
    let mut runs = 0usize;
    for (i, f) in fx.iter().enumerate() {
        let frames = (f.samples.len() / f.channels) as u64;
        // 文件头是否给出总帧数（流式占位长度的 WAV 没有）：没有时超出末尾的目标同样先定位，
        // 读到 EOF 后正常结束。
        play(&mut core, &f.data, 0, 0, true);
        let known = core.decoder().declared_frames().is_some();
        for target in targets(frames, i as u64) {
            let want = &f.samples[(target.min(frames) as usize) * f.channels..];
            for seekable in [true, false] {
                for seed in [0, 1 + i as u64 * 131 + target] {
                    let run = play(&mut core, &f.data, seed, target, seekable);
                    let ctx = format!(
                        "{} 目标 {target}/{frames} 可定位 {seekable} 种子 {seed}",
                        f.name
                    );
                    assert_eq!(run.result, Some(Ok(())), "{ctx}");
                    assert!(
                        run.out == want,
                        "{ctx}：输出 {} 个样本，应为 {}",
                        run.out.len(),
                        want.len()
                    );
                    let inside = target > 0 && (target < frames || !known);
                    // 可定位时一次 Seek、只打开一次；不可定位时 Seek 失败，重新打开并顺序解码。
                    assert_eq!(run.seeks, u32::from(inside), "{ctx}");
                    assert_eq!(run.sequential, inside && !seekable, "{ctx}");
                    assert_eq!(run.opens, if inside && !seekable { 2 } else { 1 }, "{ctx}");
                    runs += 1;
                }
            }
        }
    }
    eprintln!("{runs} 次定位播放");
}

#[test]
fn wav_seek_is_sample_exact() {
    check_all(&fixtures("../qy-codec-wav/tests/fixtures", "wav"), 40);
}

#[test]
fn aiff_seek_is_sample_exact() {
    check_all(&fixtures("../qy-codec-aiff/tests/fixtures", "aiff"), 15);
}
