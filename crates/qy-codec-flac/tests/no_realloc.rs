//! M1 T1：统一缓冲接口——小栈线程内分配、构造与复用零分配。
//!
//! 本文件只有一个测试，避免并行测试线程的分配干扰全局计数。
//!
//! 在显式 256 KiB 栈的线程内完成桌面容量的 safe 堆分配（`vec!` → boxed slice →
//! `Box::leak`）、核心/解码器构造与多次重置、换曲；计数分配器确认复用阶段零分配；
//! 夹具从文件读入堆。“不崩溃”不等于测得精确最大栈用量。
//!
//! M1 T2 起：核心为 `Core<FlacDecoder>`（1 MiB 窗口），每次播放解码约 512 KiB 的最大块
//! verbatim 帧（65535 × 2 × 32 位），读取以 4093 字节短读进入核心窗口，结果与独立参考比对。
#![allow(unsafe_code)] // 仅测试用计数分配器，委托给 System。

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use qy_codec_flac::{Capacity, FlacDecoder};
use qy_core::{Action, Core, Event};

struct Counting;
static ALLOCS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: 原样委托给 System，只增加计数。
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::SeqCst);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const STACK: usize = 256 * 1024;

fn leak<T: Copy + Default>(n: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); n].into_boxed_slice())
}

fn on_small_stack(f: impl FnOnce() + Send + 'static) {
    let h = std::thread::Builder::new().stack_size(STACK).spawn(f);
    match h.map(|h| h.join()) {
        Ok(Ok(())) => {}
        Ok(Err(p)) => std::panic::resume_unwind(p),
        Err(e) => panic!("创建线程失败: {e}"),
    }
}

#[test]
fn desktop_buffers_on_256k_stack_reuse_without_allocation() {
    // 夹具与独立参考在主线程从文件读入堆，再移入小栈线程。
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let fixture = std::fs::read(dir.join("max_block_verbatim_32_stereo.flac")).unwrap_or_default();
    let reference: i64 = std::fs::read(dir.join("max_block_verbatim_32_stereo.s32le"))
        .unwrap_or_default()
        .chunks_exact(4)
        .map(|c| i64::from(i32::from_le_bytes([c[0], c[1], c[2], c[3]])))
        .fold(0i64, i64::wrapping_add);
    assert!(fixture.len() > 500_000);
    on_small_stack(move || {
        let cap = Capacity::DESKTOP;
        // 启动时一次性分配：全部为 safe 的零初始化堆分配。
        let input = leak::<u8>(cap.input_window);
        let decoder = match FlacDecoder::new(cap, leak(cap.workspace_len()), leak(cap.pcm_len())) {
            Ok(d) => d,
            Err(e) => panic!("{e:?}"),
        };
        assert_eq!(
            (decoder.workspace_len(), decoder.pcm_len()),
            (131_070, 131_070)
        );
        let mut core = match Core::with_window(input, cap.input_window, decoder) {
            Ok(c) => c,
            Err(e) => panic!("{e:?}"),
        };
        let before = ALLOCS.load(Ordering::SeqCst);
        // 多次换曲/重置：同一缓冲复用，读取直接进入核心窗口，无新增分配。
        for round in 0..5 {
            let mut checksum = 0i64;
            let mut samples = 0usize;
            let mut finished = false;
            core.play(0, 1);
            let mut pos = 0usize;
            while let Some(action) = core.poll() {
                match action {
                    Action::Open { generation, .. } => {
                        let _ = core.handle(Event::Opened { generation });
                    }
                    Action::Read {
                        generation,
                        request,
                        max,
                    } => {
                        let n = max.min(4093).min(fixture.len() - pos);
                        if n == 0 {
                            let _ = core.handle(Event::ReadEof {
                                generation,
                                request,
                            });
                            continue;
                        }
                        if let Some(dst) = core.read_target(generation, request) {
                            dst[..n].copy_from_slice(&fixture[pos..pos + n]);
                        }
                        pos += n;
                        let _ = core.handle(Event::ReadDone {
                            generation,
                            request,
                            len: n,
                        });
                    }
                    Action::Configure { generation, .. } => {
                        let _ = core.handle(Event::Configured { generation });
                    }
                    Action::Output { generation, frames } => {
                        if let Some(pcm) = core.pcm() {
                            samples += pcm.samples().len();
                            checksum = pcm
                                .samples()
                                .iter()
                                .fold(checksum, |a, &s| a.wrapping_add(i64::from(s)));
                        }
                        let _ = core.handle(Event::Output { generation, frames });
                    }
                    Action::Close { .. } => {}
                    Action::Finished { .. } => {
                        finished = true;
                        break;
                    }
                    Action::Failed { fault, .. } => panic!("第 {round} 次播放失败: {fault:?}"),
                }
            }
            assert!(finished);
            assert_eq!(samples, 2 * 65_535, "第 {round} 次");
            assert_eq!(checksum, reference, "第 {round} 次样本与参考不一致");
            let _ = core.stop();
        }
        let after = ALLOCS.load(Ordering::SeqCst);
        assert_eq!(after, before, "重置/换曲期间不得分配");
        assert_eq!(core.decoder().capacity(), cap);
    });
}
