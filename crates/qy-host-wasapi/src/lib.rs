//! QyFi WASAPI 宿主。
//!
//! 大型缓冲在 [`HostResources::new`] 中启动时一次性分配（与模拟宿主同一契约）；
//! Windows SDK 绑定只在 Windows 目标编译。
//!
//! M0 T1 只建立资源骨架与最小端点枚举以验证绑定可构建；
//! 独占初始化、事件驱动渲染与 MMCSS 在 T5 实现。

use qy_codec_wav::WavDecoder;
use qy_core::{DecodeError, INPUT_WINDOW_BYTES, PCM_WORK_SAMPLES};
use qy_pcm_format::ring::{
    DEFAULT_CAPACITY_BYTES, FrameConsumer, FrameProducer, PACK_SCRATCH_BYTES, frame_ring,
};

#[cfg(windows)]
pub mod endpoints;
#[cfg(windows)]
pub mod sdk_format;

/// 一次性分配零初始化切片并泄漏为 `'static`。只在宿主启动时调用。
fn leak_zeroed<T: Copy + Default>(len: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); len].into_boxed_slice())
}

/// 宿主长期资源：整程序生命期复用。
pub struct HostResources {
    pub input: Option<&'static mut [u8]>,
    pub decoder: WavDecoder,
    /// 生产端 pack 暂存区（启动时分配一次）。
    pub pack_scratch: &'static mut [u8],
    pub producer: FrameProducer,
    pub consumer: FrameConsumer,
}

impl HostResources {
    pub fn new() -> Result<Self, DecodeError> {
        let input = leak_zeroed::<u8>(INPUT_WINDOW_BYTES);
        let decoder = WavDecoder::new(leak_zeroed::<i32>(PCM_WORK_SAMPLES))?;
        let (producer, consumer) = frame_ring(DEFAULT_CAPACITY_BYTES);
        Ok(Self {
            input: Some(input),
            decoder,
            pack_scratch: leak_zeroed::<u8>(PACK_SCRATCH_BYTES),
            producer,
            consumer,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resources_have_declared_capacity() {
        let Ok(r) = HostResources::new() else {
            panic!("init");
        };
        assert_eq!(r.input.as_ref().map(|s| s.len()), Some(INPUT_WINDOW_BYTES));
        assert_eq!(r.decoder.capacity_samples(), PCM_WORK_SAMPLES);
    }
}
