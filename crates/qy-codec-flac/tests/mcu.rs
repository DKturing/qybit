//! M1 T1：MCU 静态缓冲（特性 static-cell）。
#![cfg(feature = "static-cell")]

use qy_codec_flac::mcu::take_buffers;
use qy_codec_flac::{Buffer, Capacity, FlacDecoder, InitError};

#[test]
fn static_buffers_taken_once_with_declared_capacity() {
    let b = match take_buffers() {
        Ok(b) => b,
        Err(e) => panic!("{e:?}"),
    };
    let cap = Capacity::MCU;
    assert_eq!(b.input.len(), cap.input_window);
    assert_eq!(b.workspace.len(), cap.workspace_len());
    assert_eq!(b.pcm.len(), cap.pcm_len());
    assert!(b.input.iter().all(|&x| x == 0) && b.workspace.iter().all(|&x| x == 0));
    assert!(FlacDecoder::new(cap, b.workspace, b.pcm).is_ok());
    // 再次获取：明确的初始化错误，而不是 panic。
    assert_eq!(
        take_buffers().err(),
        Some(InitError::AlreadyTaken(Buffer::InputWindow))
    );
}
