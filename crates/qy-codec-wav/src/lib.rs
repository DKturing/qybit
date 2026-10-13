//! QyFi 自有推送式 WAV 解码器。
//!
//! `#![no_std]`、无 alloc；PCM 工作区由宿主以 `&'static mut [i32]` 提供。
//!
//! M0 T1 只建立工作区契约；流式解析与 [`qy_core::Decoder`] 实现属于 T2，
//! 在此之前不提供解码入口，避免用空壳实现冒充可用解码器。
#![no_std]

use qy_core::{DecodeError, PCM_WORK_SAMPLES};

/// WAV 解码器状态（T2 填充解析状态）。
pub struct WavDecoder {
    pcm: &'static mut [i32],
}

impl WavDecoder {
    /// 用宿主提供的 PCM 工作区构造；长度不足 [`PCM_WORK_SAMPLES`] 返回容量错误。
    pub fn new(pcm: &'static mut [i32]) -> Result<Self, DecodeError> {
        if pcm.len() < PCM_WORK_SAMPLES {
            return Err(DecodeError::CapacityExceeded);
        }
        Ok(Self { pcm })
    }

    /// 工作区容量（样本数）。
    pub fn capacity_samples(&self) -> usize {
        self.pcm.len()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::boxed::Box;
    use std::vec;

    #[test]
    fn workspace_capacity_checked() {
        let small = Box::leak(vec![0i32; PCM_WORK_SAMPLES - 1].into_boxed_slice());
        assert!(matches!(
            WavDecoder::new(small),
            Err(DecodeError::CapacityExceeded)
        ));
        let ok = Box::leak(vec![0i32; PCM_WORK_SAMPLES].into_boxed_slice());
        assert_eq!(
            WavDecoder::new(ok).map(|d| d.capacity_samples()).ok(),
            Some(8192)
        );
    }
}
