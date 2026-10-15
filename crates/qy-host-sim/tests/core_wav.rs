//! 真实 qy-core 输入窗口（8192 字节）+ qy-codec-wav 的流式解码：
//! 随机短读、零长度读取与随机部分取走输出，结果必须与一次性解码一致（M0 T2）。

use std::fs;
use std::path::{Path, PathBuf};

use qy_codec_wav::WavDecoder;
use qy_core::{Action, Core, Event, Fault, INPUT_WINDOW_BYTES, PCM_WORK_SAMPLES};

struct Lcg(u64);
impl Lcg {
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        lo + ((self.0 >> 33) as usize) % (hi - lo + 1)
    }
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../qy-codec-wav/tests/fixtures")
}

fn new_core() -> Core<WavDecoder> {
    let input = Box::leak(vec![0u8; INPUT_WINDOW_BYTES].into_boxed_slice());
    let pcm = Box::leak(vec![0i32; PCM_WORK_SAMPLES].into_boxed_slice());
    let Ok(dec) = WavDecoder::new(pcm) else {
        panic!("decoder");
    };
    let Ok(core) = Core::new(input, dec) else {
        panic!("core");
    };
    core
}

fn play(core: &mut Core<WavDecoder>, data: &[u8], seed: u64) -> (Result<(), Fault>, Vec<i32>) {
    let mut r = Lcg(seed);
    core.play(0, 1);
    let mut pos = 0usize;
    let mut out = Vec::new();
    for _ in 0..10 * data.len() + 1000 {
        let Some(action) = core.poll() else {
            panic!("单线程宿主不应出现无动作等待");
        };
        match action {
            Action::Open { generation, .. } => {
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
                out.extend_from_slice(&pcm.samples()[..k * ch]);
                assert_eq!(
                    core.handle(Event::Output {
                        generation,
                        frames: k
                    }),
                    Ok(())
                );
            }
            Action::Read {
                generation,
                request,
                max,
            } => {
                if pos == data.len() {
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
                    (_, 0) => 0, // 零长度短读不是 EOF
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
            Action::Seek { .. } => panic!("未定位时不应有 Seek"),
            Action::Finished { .. } => return (Ok(()), out),
            Action::Failed { fault, .. } => return (Err(fault), out),
        }
    }
    panic!("解码未推进");
}

#[test]
fn core_window_streaming_matches_one_shot() {
    let mut names: Vec<_> = fs::read_dir(fixtures())
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_else(|_| Vec::new());
    names.retain(|p| p.extension().is_some_and(|e| e == "wav"));
    names.sort();
    assert!(names.len() >= 50);
    let mut core = new_core();
    for path in &names {
        let data = fs::read(path).unwrap_or_default();
        let reference = play(&mut core, &data, 0);
        for seed in 1..=50u64 {
            let got = play(&mut core, &data, seed);
            assert_eq!(got, reference, "{} seed={seed}", path.display());
        }
    }
}
