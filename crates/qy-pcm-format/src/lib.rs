//! QyBit PCM 格式描述。
//!
//! 只定义与平台 SDK 无关的格式字段和纯构造函数，不调用任何系统 API，
//! 因此纯逻辑测试可在任何平台运行（M0 T1）。
//!
//! 术语（AGENTS §3）：
//! - N：源有效位数（解码器输出的 `i32` 右对齐，携带 N）。
//! - C：设备容器位宽；V：设备有效位数。
//! - 字节序与有效位在容器中的放置方式（左对齐 / 右对齐）显式区分。
#![no_std]

#[cfg(feature = "ring")]
extern crate alloc;

pub mod alsa;
pub mod pack;
#[cfg(feature = "ring")]
pub mod ring;
pub mod wave;

/// 本阶段支持的最大声道数（M0：单/双声道）。
pub const MAX_CHANNELS: u16 = 2;
/// 本阶段支持的最高采样率。
pub const MAX_SAMPLE_RATE: u32 = 192_000;

/// 格式构造错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    /// 采样率为 0 或超出支持范围。
    SampleRate,
    /// 声道数为 0 或超出支持范围。
    Channels,
    /// 容器位宽不在支持集合内。
    ContainerBits,
    /// 有效位数为 0 或大于容器位宽。
    ValidBits,
    /// 派生字段（blockAlign、字节率等）计算溢出。
    Overflow,
    /// 字节序或有效位放置方式不符合目标描述（如 WASAPI 要求小端、高位对齐）。
    Layout,
}

/// 样本字节序。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
}

/// 有效位在容器内的放置方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// 有效位位于容器高位，低位补零（如 WASAPI 32/24）。
    MsbAligned,
    /// 有效位位于容器低位并符号扩展（如 ALSA `S24_LE`）。
    LsbAligned,
}

/// 源 PCM 格式：解码器输出为交错、右对齐的 `i32`，携带有效位数 N。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceFormat {
    sample_rate: u32,
    channels: u16,
    bits: u8,
}

impl SourceFormat {
    /// 构造源格式；N 必须在 1..=32。
    pub const fn new(sample_rate: u32, channels: u16, bits: u8) -> Result<Self, FormatError> {
        if sample_rate == 0 || sample_rate > MAX_SAMPLE_RATE {
            return Err(FormatError::SampleRate);
        }
        if channels == 0 || channels > MAX_CHANNELS {
            return Err(FormatError::Channels);
        }
        if bits == 0 || bits > 32 {
            return Err(FormatError::ValidBits);
        }
        Ok(Self {
            sample_rate,
            channels,
            bits,
        })
    }

    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub const fn channels(&self) -> u16 {
        self.channels
    }

    /// 源有效位数 N。
    pub const fn bits(&self) -> u8 {
        self.bits
    }
}

/// 设备端 PCM 整数格式字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceFormat {
    sample_rate: u32,
    channels: u16,
    container_bits: u8,
    valid_bits: u8,
    endian: Endian,
    placement: Placement,
    block_align: u16,
    bytes_per_sec: u32,
}

impl DeviceFormat {
    /// 构造设备格式并计算派生字段。
    ///
    /// 容器位宽限定为 16/24/32；V 必须在 1..=C；所有派生量做溢出检查。
    pub const fn new(
        sample_rate: u32,
        channels: u16,
        container_bits: u8,
        valid_bits: u8,
        endian: Endian,
        placement: Placement,
    ) -> Result<Self, FormatError> {
        if sample_rate == 0 || sample_rate > MAX_SAMPLE_RATE {
            return Err(FormatError::SampleRate);
        }
        if channels == 0 || channels > MAX_CHANNELS {
            return Err(FormatError::Channels);
        }
        if !matches!(container_bits, 16 | 24 | 32) {
            return Err(FormatError::ContainerBits);
        }
        if valid_bits == 0 || valid_bits > container_bits {
            return Err(FormatError::ValidBits);
        }
        let bytes_per_sample = (container_bits / 8) as u16;
        let block_align = match channels.checked_mul(bytes_per_sample) {
            Some(v) => v,
            None => return Err(FormatError::Overflow),
        };
        let bytes_per_sec = match sample_rate.checked_mul(block_align as u32) {
            Some(v) => v,
            None => return Err(FormatError::Overflow),
        };
        Ok(Self {
            sample_rate,
            channels,
            container_bits,
            valid_bits,
            endian,
            placement,
            block_align,
            bytes_per_sec,
        })
    }

    /// WASAPI PCM 设备格式：小端、有效位高位对齐（WAVEFORMATEXTENSIBLE 语义）。
    pub const fn wasapi(
        sample_rate: u32,
        channels: u16,
        container_bits: u8,
        valid_bits: u8,
    ) -> Result<Self, FormatError> {
        Self::new(
            sample_rate,
            channels,
            container_bits,
            valid_bits,
            Endian::Little,
            Placement::MsbAligned,
        )
    }

    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub const fn channels(&self) -> u16 {
        self.channels
    }

    /// 容器位宽 C。
    pub const fn container_bits(&self) -> u8 {
        self.container_bits
    }

    /// 有效位数 V。
    pub const fn valid_bits(&self) -> u8 {
        self.valid_bits
    }

    pub const fn endian(&self) -> Endian {
        self.endian
    }

    pub const fn placement(&self) -> Placement {
        self.placement
    }

    /// 一帧（所有声道一个样本）的字节数。
    pub const fn block_align(&self) -> u16 {
        self.block_align
    }

    pub const fn bytes_per_sec(&self) -> u32 {
        self.bytes_per_sec
    }

    /// 源格式能否无损放入本设备格式：采样率、声道一致且 N ≤ V。
    pub const fn accepts(&self, src: &SourceFormat) -> bool {
        src.sample_rate == self.sample_rate
            && src.channels == self.channels
            && src.bits <= self.valid_bits
    }
}

/// 给定采样率下 `millis` 毫秒对应的帧数（向上取整）。
pub const fn frames_for_millis(sample_rate: u32, millis: u32) -> u64 {
    (sample_rate as u64 * millis as u64).div_ceil(1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_format_derived_fields() {
        let f = DeviceFormat::new(44_100, 2, 16, 16, Endian::Little, Placement::MsbAligned);
        assert_eq!(
            f.map(|f| (f.block_align(), f.bytes_per_sec())),
            Ok((4, 176_400))
        );
        let f = DeviceFormat::new(96_000, 2, 32, 24, Endian::Little, Placement::MsbAligned);
        assert_eq!(
            f.map(|f| (f.block_align(), f.bytes_per_sec())),
            Ok((8, 768_000))
        );
    }

    #[test]
    fn device_format_rejects_invalid() {
        let le = Endian::Little;
        let p = Placement::MsbAligned;
        assert_eq!(
            DeviceFormat::new(0, 2, 16, 16, le, p),
            Err(FormatError::SampleRate)
        );
        assert_eq!(
            DeviceFormat::new(384_000, 2, 16, 16, le, p),
            Err(FormatError::SampleRate)
        );
        assert_eq!(
            DeviceFormat::new(48_000, 0, 16, 16, le, p),
            Err(FormatError::Channels)
        );
        assert_eq!(
            DeviceFormat::new(48_000, 3, 16, 16, le, p),
            Err(FormatError::Channels)
        );
        assert_eq!(
            DeviceFormat::new(48_000, 2, 8, 8, le, p),
            Err(FormatError::ContainerBits)
        );
        assert_eq!(
            DeviceFormat::new(48_000, 2, 32, 0, le, p),
            Err(FormatError::ValidBits)
        );
        assert_eq!(
            DeviceFormat::new(48_000, 2, 24, 32, le, p),
            Err(FormatError::ValidBits)
        );
    }

    #[test]
    fn source_format_and_accepts() {
        let dev = DeviceFormat::new(48_000, 2, 32, 24, Endian::Little, Placement::MsbAligned);
        let Ok(dev) = dev else {
            panic!("valid format rejected")
        };
        let src16 = SourceFormat::new(48_000, 2, 16);
        let src24 = SourceFormat::new(48_000, 2, 24);
        let src32 = SourceFormat::new(48_000, 2, 32);
        let mono = SourceFormat::new(48_000, 1, 16);
        let other_rate = SourceFormat::new(44_100, 2, 16);
        assert_eq!(src16.map(|s| dev.accepts(&s)), Ok(true));
        assert_eq!(src24.map(|s| dev.accepts(&s)), Ok(true));
        assert_eq!(src32.map(|s| dev.accepts(&s)), Ok(false));
        assert_eq!(mono.map(|s| dev.accepts(&s)), Ok(false));
        assert_eq!(other_rate.map(|s| dev.accepts(&s)), Ok(false));
        assert_eq!(SourceFormat::new(48_000, 2, 0), Err(FormatError::ValidBits));
        assert_eq!(
            SourceFormat::new(48_000, 2, 33),
            Err(FormatError::ValidBits)
        );
    }

    #[test]
    fn frames_for_half_second() {
        assert_eq!(frames_for_millis(192_000, 500), 96_000);
        assert_eq!(frames_for_millis(44_100, 500), 22_050);
        assert_eq!(frames_for_millis(11_025, 500), 5_513);
    }
}
