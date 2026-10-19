//! M2 T3：ALSA 短写渲染循环（`AlsaWriteApi` + `qy_player::render::write`）在渲染线程上
//! 零分配。计数分配器只统计打开了计数开关的线程，其他测试线程不受影响。
//! 覆盖随机短写、一次 xrun 恢复与排空。
#![allow(unsafe_code)] // 仅测试用计数分配器，委托给 System。

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use qy_host_alsa::api::AlsaWriteApi;
use qy_host_alsa::pcm::{HwPlan, Opener, Pcm};
use qy_host_alsa::testing::{FakeAlsaConfig, FakeOpener};
use qy_pcm_format::alsa::AlsaFormat;
use qy_pcm_format::ring::{frame_ring, reconfigure};
use qy_player::render::write::{WriteConfig, render};
use qy_player::render::{RenderControl, RenderEnd};
use qy_player::testing::{FakePriority, Feeder, data_chunk, fixture};

struct Counting;
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
}

fn note() {
    if TRACK.try_with(Cell::get).unwrap_or(false) {
        ALLOCS.fetch_add(1, Ordering::SeqCst);
    }
}

// SAFETY: 原样委托给 System，只在开启计数的线程上增加计数。
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn alsa_render_loop_does_not_allocate() {
    let mut opener = FakeOpener::new(FakeAlsaConfig {
        short_writes: true,
        seed: 9,
        xrun_at_writes: vec![4],
        ..FakeAlsaConfig::default()
    });
    let mut pcm = match opener.open("hw:0,0") {
        Ok(p) => p,
        Err(e) => panic!("{e}"),
    };
    let plan = HwPlan {
        format: AlsaFormat::S16Le,
        sample_rate: 44_100,
        channels: 2,
        layout: qy_pcm_format::ChannelLayout::STEREO,
        period_frames: 441,
        periods: 4,
    };
    if let Err(e) = pcm.configure(&plan) {
        panic!("{e}");
    }
    let Ok(format) = AlsaFormat::S16Le.device_format(44_100, 2) else {
        panic!("fmt");
    };
    let (mut p, mut c) = frame_ring(768_000);
    if let Err(e) = reconfigure(&mut p, &mut c, &format, 1) {
        panic!("{e:?}");
    }
    let repeat = 30;
    Feeder::new("pcm16_stereo_44k.wav", format, repeat).feed(&mut p, usize::MAX);
    let ctl = Arc::new(RenderControl::default());
    ctl.eof.store(true, Ordering::Release);
    let mut prio = FakePriority::default();
    let cfg = WriteConfig {
        block_align: 4,
        period_frames: 441,
        event_timeout_ms: 2000,
        idle_sleep: Duration::from_micros(50),
        silence: format.silence_frame(),
    };
    let mut api = AlsaWriteApi::new(pcm, &format);
    let h = std::thread::spawn(move || {
        TRACK.with(|t| t.set(true));
        let r = render(&mut api, &mut prio, &mut c, &ctl, cfg);
        TRACK.with(|t| t.set(false));
        drop(api);
        r
    });
    let r = match h.join() {
        Ok(r) => r,
        Err(p) => std::panic::resume_unwind(p),
    };
    assert_eq!(ALLOCS.load(Ordering::SeqCst), 0, "渲染循环不得分配");
    assert_eq!(r.end, RenderEnd::Drained);
    assert_eq!(r.underrun_events, 1, "须覆盖 xrun 路径");
    let raw = data_chunk(&fixture("pcm16_stereo_44k.wav"));
    let outs = opener.outputs();
    assert!(outs[0].rendered == raw.repeat(repeat), "有效字节须完整");
}
