//! M1 T1：统一缓冲接口——小栈线程内分配、构造与复用零分配。
//!
//! 本文件只有一个测试，避免并行测试线程的分配干扰全局计数。
//!
//! 在显式 256 KiB 栈的线程内完成桌面容量的 safe 堆分配（`vec!` → boxed slice →
//! `Box::leak`）、核心/解码器构造与多次重置、换曲；计数分配器确认复用阶段零分配；
//! 夹具从文件读入堆。“不崩溃”不等于测得精确最大栈用量。
//!
//! 最大块解码要求真实 FLAC 解码（M1 T2），届时加入本测试；T1 不用空壳凑数。
#![allow(unsafe_code)] // 仅测试用计数分配器，委托给 System。

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use qy_codec_flac::{Capacity, FlacDecoder};
use qy_codec_wav::WavDecoder;
use qy_core::{Action, Core, Event, PCM_WORK_SAMPLES};

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
    // 夹具在主线程从文件读入堆，再移入小栈线程。
    let fixture = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../qy-codec-wav/tests/fixtures/pcm24_stereo_large.wav"),
    )
    .unwrap_or_default();
    assert!(!fixture.is_empty());
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
        // 核心接管 1 MiB 输入窗口（T2 前用 WAV 解码器驱动窗口路径）。
        let Ok(wav) = WavDecoder::new(leak(PCM_WORK_SAMPLES)) else {
            panic!("wav");
        };
        let mut core = match Core::with_window(input, cap.input_window, wav) {
            Ok(c) => c,
            Err(e) => panic!("{e:?}"),
        };
        let before = ALLOCS.load(Ordering::SeqCst);
        // 多次换曲/重置：同一缓冲复用，读取直接进入核心窗口，无新增分配。
        let mut checksum = 0i64;
        for _ in 0..20 {
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
                        let n = max.min(fixture.len() - pos);
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
                            checksum = pcm
                                .samples()
                                .iter()
                                .fold(checksum, |a, &s| a.wrapping_add(i64::from(s)));
                        }
                        let _ = core.handle(Event::Output { generation, frames });
                    }
                    Action::Close { .. } => {}
                    Action::Finished { .. } | Action::Failed { .. } => break,
                }
            }
            let _ = core.stop();
        }
        let after = ALLOCS.load(Ordering::SeqCst);
        assert_eq!(after, before, "重置/换曲期间不得分配");
        assert_ne!(checksum, 0);
        assert_eq!(decoder.capacity(), cap);
    });
}
