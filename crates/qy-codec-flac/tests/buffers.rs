//! M1 T1：容量配置、结构体大小与容量不足错误。零分配复用见 `no_realloc.rs`。

use qy_codec_flac::{Buffer, Capacity, FlacDecoder, InitError};
use qy_codec_wav::WavDecoder;
use qy_core::{Core, CoreError, PCM_WORK_SAMPLES};

fn leak<T: Copy + Default>(n: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); n].into_boxed_slice())
}

#[test]
fn desktop_capacity_matches_task_card() {
    let c = Capacity::DESKTOP;
    assert_eq!(c.input_window, 1_048_576);
    assert_eq!(c.workspace_len(), 2 * 65_535);
    assert_eq!(c.workspace_len() * 8, 1_048_560);
    assert_eq!(c.pcm_len() * 4, 524_280);
    let m = Capacity::MCU;
    assert_eq!(
        (m.max_block, m.max_channels, m.input_window),
        (4_608, 2, 32_768)
    );
}

#[test]
fn structs_hold_no_block_sized_storage() {
    // 结构体大小与容量无关：只含切片与少量状态。
    let d = std::mem::size_of::<FlacDecoder>();
    let c = std::mem::size_of::<Core<WavDecoder>>();
    assert!(d <= 128, "FlacDecoder {d} 字节");
    assert!(c <= 256, "Core {c} 字节");
}

#[test]
fn capacity_errors_are_explicit() {
    let c = Capacity::DESKTOP;
    let r = FlacDecoder::new(c, leak(c.workspace_len() - 1), leak(c.pcm_len()));
    assert_eq!(
        r.err(),
        Some(InitError::CapacityExceeded {
            buffer: Buffer::Workspace,
            required: 131_070,
            provided: 131_069
        })
    );
    let r = FlacDecoder::new(c, leak(c.workspace_len()), leak(c.pcm_len() - 2));
    assert_eq!(
        r.err(),
        Some(InitError::CapacityExceeded {
            buffer: Buffer::Pcm,
            required: 131_070,
            provided: 131_068
        })
    );
    let Ok(wav) = WavDecoder::new(leak(PCM_WORK_SAMPLES)) else {
        panic!("wav");
    };
    let r = Core::with_window(leak(c.input_window - 1), c.input_window, wav);
    assert!(matches!(
        r,
        Err(CoreError::CapacityExceeded {
            required: 1_048_576,
            provided: 1_048_575
        })
    ));
}
