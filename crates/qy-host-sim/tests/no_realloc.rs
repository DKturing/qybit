//! 验证宿主资源只在启动时分配：设备重配置/换曲不产生堆分配。
#![allow(unsafe_code)] // 仅测试用计数分配器，委托给 System。

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use qy_core::{INPUT_WINDOW_BYTES, PCM_WORK_SAMPLES};
use qy_host_sim::SimHost;
use qy_pcm_format::ring::DEFAULT_CAPACITY_BYTES;
use qy_pcm_format::{DeviceFormat, Endian, Placement};

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

#[test]
fn reconfigure_reuses_buffers_without_allocation() {
    let Ok(mut host) = SimHost::new() else {
        panic!("host init");
    };
    assert_eq!(
        host.input.as_ref().map(|s| s.len()),
        Some(INPUT_WINDOW_BYTES)
    );
    assert_eq!(host.decoder.capacity_samples(), PCM_WORK_SAMPLES);
    assert_eq!(host.producer.free_frames(), 0); // 未配置帧宽

    let formats = [
        (44_100, 2, 16, 16),
        (192_000, 2, 32, 32),
        (96_000, 1, 32, 24),
        (48_000, 2, 24, 24),
    ];
    let mut parsed = Vec::new();
    for (r, c, cb, vb) in formats {
        match DeviceFormat::new(r, c, cb, vb, Endian::Little, Placement::MsbAligned) {
            Ok(f) => parsed.push(f),
            Err(e) => panic!("{e:?}"),
        }
    }

    let before = ALLOCS.load(Ordering::SeqCst);
    for f in &parsed {
        let Ok(_) = host.configure_device(f) else {
            panic!("configure");
        };
        let frame = usize::from(f.block_align());
        assert_eq!(host.consumer.frame_bytes(), frame);
        assert_eq!(host.producer.free_frames(), DEFAULT_CAPACITY_BYTES / frame);
    }
    assert_eq!(ALLOCS.load(Ordering::SeqCst), before, "重配置不应分配");
}
