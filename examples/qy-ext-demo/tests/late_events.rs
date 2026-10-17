//! 停止后迟到的 I/O 回报：由核心按代次拒绝，扩展（解码器、帧队列、宿主）不需要处理。
//!
//! 这里用一个假宿主直接驱动引擎（同步完成每个动作），并用示例自己实现的 `FrameSink`（把 pack 后的
//! 字节记在内存里）接收输出：第 1 首的第一次读取先不回报，停止并改播第 2 首之后，再把第 1 首的
//! 打开、读取与结束回报注入引擎。

use qy_codec_auto::profile::DESKTOP;
use qy_core::engine::{
    Engine, EngineConfig, ErrorPolicy, HostAction, HostEvent, Poll, ReusePolicy,
};
use qy_core::{Core, CoreError, DeviceFormat, Generation, Pcm};
use qy_ext_demo::qyt1::encode;
use qy_ext_demo::{ExtDecoder, leak};
use qy_pcm_format::pack::{ChannelMode, Fidelity, PackError, pack_with};
use qy_pcm_format::queue::FrameSink;
use qy_pcm_format::{Endian, Placement};

/// 示例帧队列：pack 到内存，不限容量（从不返回“队列满”）。`StopDevice` 时丢弃。
#[derive(Default)]
struct Recording {
    bytes: Vec<u8>,
    frames: usize,
}

impl FrameSink for Recording {
    type Error = PackError;

    fn push<M: Fidelity>(
        &mut self,
        pcm: &Pcm<'_, M>,
        device: &DeviceFormat,
        mode: ChannelMode,
        _scratch: &mut [u8],
    ) -> Result<usize, PackError> {
        let mut out = vec![0u8; pcm.frames() * usize::from(device.block_align())];
        let n = pack_with(pcm, device, mode, &mut out)?;
        out.truncate(n * usize::from(device.block_align()));
        self.bytes.extend_from_slice(&out);
        self.frames += n;
        Ok(n)
    }

    fn queued_frames(&self) -> usize {
        self.frames
    }
}

/// 独立参考：16 位样本放进 16/16 小端容器。
fn place16(samples: &[i32]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|&s| (s as i16).to_le_bytes())
        .collect()
}

fn samples(frames: usize, salt: i32) -> Vec<i32> {
    (0..frames * 2)
        .map(|i| (i as i32).wrapping_mul(7_919).wrapping_add(salt) % 32_768)
        .collect()
}

struct Host {
    files: Vec<Vec<u8>>,
    /// 打开的曲目与读取位置。
    open: Option<(usize, usize)>,
}

impl Host {
    /// 执行一个动作并回报；`hold` 为真时第一次读取不回报，返回该读取的代次与请求号。
    fn execute(
        &mut self,
        e: &mut Engine<ExtDecoder, Recording>,
        a: HostAction,
        hold: bool,
    ) -> Option<(Generation, u32)> {
        let ok = |r: Result<(), CoreError>| assert_eq!(r, Ok(()), "{a:?}");
        match a {
            HostAction::Open { generation, track } => {
                self.open = Some((track as usize, 0));
                ok(e.handle(HostEvent::Opened { generation }));
            }
            HostAction::Read {
                generation,
                request,
                max,
            } => {
                if hold {
                    return Some((generation, request));
                }
                let Some((t, pos)) = self.open else {
                    panic!("没有打开的曲目");
                };
                let rest = &self.files[t][pos..];
                let n = rest.len().min(max);
                let Some(dst) = e.read_target(generation, request) else {
                    panic!("读取目标");
                };
                dst[..n].copy_from_slice(&rest[..n]);
                self.open = Some((t, pos + n));
                ok(e.handle(if n == 0 {
                    HostEvent::ReadEof {
                        generation,
                        request,
                    }
                } else {
                    HostEvent::ReadDone {
                        generation,
                        request,
                        len: n,
                    }
                }));
            }
            HostAction::Close { .. } => self.open = None,
            HostAction::OpenDevice { source, .. } => {
                let Ok(format) = DeviceFormat::new(
                    source.sample_rate(),
                    source.channels(),
                    16,
                    16,
                    Endian::Little,
                    Placement::MsbAligned,
                ) else {
                    panic!("设备格式");
                };
                ok(e.handle(HostEvent::DeviceOpened { format }));
            }
            HostAction::DrainDevice => ok(e.handle(HostEvent::DeviceDrained)),
            HostAction::StopDevice => *e.sink_mut() = Recording::default(),
            _ => {}
        }
        None
    }

    /// 运行到引擎空闲；`hold` 时在第一次读取处返回。
    fn run(
        &mut self,
        e: &mut Engine<ExtDecoder, Recording>,
        hold: bool,
    ) -> Option<(Generation, u32)> {
        for _ in 0..1_000_000 {
            match e.poll() {
                Poll::Action(a) => {
                    if let Some(held) = self.execute(e, a, hold) {
                        return Some(held);
                    }
                }
                Poll::Yield => {}
                Poll::Idle => return None,
                Poll::Wait(w) => panic!("假宿主同步完成所有动作，不应等待 {w:?}"),
            }
        }
        panic!("没有结束");
    }
}

#[test]
fn late_reports_after_stop_are_rejected_by_generation() {
    let first = samples(3_000, 1);
    let second = samples(2_000, 2);
    let mut host = Host {
        files: vec![
            encode(44_100, 2, 16, &first),
            encode(44_100, 2, 16, &second),
        ],
        open: None,
    };
    let decoder = ExtDecoder::desktop(true);
    let window = decoder.required_window();
    let core = match Core::with_window(leak(DESKTOP.budget().input_window), window, decoder) {
        Ok(c) => c,
        Err(e) => panic!("{e:?}"),
    };
    let mut e = Engine::new(
        core,
        Recording::default(),
        &mut [],
        EngineConfig {
            caps: DESKTOP.caps,
            reuse: ReusePolicy::Accepts,
            on_error: ErrorPolicy::Stop,
        },
    );

    e.play(0, 2);
    let Some((old, request)) = host.run(&mut e, true) else {
        panic!("应在第 1 首的第一次读取处停下");
    };
    // 停止（关闭第 1 首）并改播第 2 首。
    e.stop();
    while let Some(a) = e.take_control() {
        host.execute(&mut e, a, false);
    }
    e.skip_to(1);

    // 注入第 1 首迟到的回报：全部按旧代次拒绝，宿主拿不到写入目标。
    assert!(e.read_target(old, request).is_none());
    let late = [
        HostEvent::Opened { generation: old },
        HostEvent::ReadDone {
            generation: old,
            request,
            len: 64,
        },
        HostEvent::ReadEof {
            generation: old,
            request,
        },
    ];
    for ev in late {
        assert_eq!(e.handle(ev), Err(CoreError::Stale), "{ev:?}");
    }

    // 第 2 首照常播放，输出与参考逐字节一致，没有混入第 1 首的数据。
    assert!(host.run(&mut e, false).is_none());
    assert_eq!(e.sink().bytes, place16(&second));
    assert_eq!(e.stats().protocol_errors, 0);
}
