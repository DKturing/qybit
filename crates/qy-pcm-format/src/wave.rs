//! 与 Windows SDK 无关的 `WAVEFORMATEX` / `WAVEFORMATEXTENSIBLE` 纯字段描述（M0 T3）。
//!
//! 只做字段构造与校验，不调用 COM，不依赖实际设备，因此在任何平台都可测试。
//! Windows 层只把这些字段原样映射到 SDK 结构。
//!
//! 出处：Microsoft Learn `WAVEFORMATEX`（PCM：`nBlockAlign = nChannels × wBitsPerSample / 8`，
//! `nAvgBytesPerSec = nSamplesPerSec × nBlockAlign`，`cbSize` 对 `WAVE_FORMAT_PCM` 忽略、取 0）；
//! `WAVEFORMATEXTENSIBLE`（`wFormatTag = WAVE_FORMAT_EXTENSIBLE`、`cbSize = 22`、
//! `wBitsPerSample` 为容器位宽且为 8 的倍数、`wValidBitsPerSample ≤ wBitsPerSample`、
//! `dwChannelMask`、`SubFormat`）；ksmedia.h `SPEAKER_*` 与 `KSDATAFORMAT_SUBTYPE_PCM`。

use crate::{DeviceFormat, Endian, FormatError, MAX_CHANNELS, Placement};

/// `WAVE_FORMAT_PCM`。
pub const WAVE_FORMAT_PCM: u16 = 0x0001;
/// `WAVE_FORMAT_EXTENSIBLE`。
pub const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// `WAVEFORMATEXTENSIBLE` 的 `cbSize`：其后附加字段 22 字节。
pub const EXTENSIBLE_CB_SIZE: u16 = 22;

pub const SPEAKER_FRONT_LEFT: u32 = 0x1;
pub const SPEAKER_FRONT_RIGHT: u32 = 0x2;
pub const SPEAKER_FRONT_CENTER: u32 = 0x4;

/// Windows `GUID` 的纯字段表示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl Guid {
    /// Windows 内存布局字节（Data1/2/3 小端，Data4 原序）。
    pub const fn to_bytes(&self) -> [u8; 16] {
        let a = self.data1.to_le_bytes();
        let b = self.data2.to_le_bytes();
        let c = self.data3.to_le_bytes();
        let d = self.data4;
        [
            a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5],
            d[6], d[7],
        ]
    }
}

/// `KSDATAFORMAT_SUBTYPE_PCM` = {00000001-0000-0010-8000-00AA00389B71}。
pub const KSDATAFORMAT_SUBTYPE_PCM: Guid = Guid {
    data1: 0x0000_0001,
    data2: 0x0000,
    data3: 0x0010,
    data4: [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
};

/// 单声道 FrontCenter，双声道 FrontLeft|FrontRight；其他声道数不定义布局。
pub const fn channel_mask(channels: u16) -> Result<u32, FormatError> {
    match channels {
        1 => Ok(SPEAKER_FRONT_CENTER),
        2 => Ok(SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT),
        _ => Err(FormatError::Channels),
    }
}

/// `WAVEFORMATEX` 字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaveFormatEx {
    pub format_tag: u16,
    pub channels: u16,
    pub samples_per_sec: u32,
    pub avg_bytes_per_sec: u32,
    pub block_align: u16,
    pub bits_per_sample: u16,
    pub cb_size: u16,
}

/// `WAVEFORMATEXTENSIBLE` 字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaveFormatExtensible {
    pub format: WaveFormatEx,
    pub valid_bits_per_sample: u16,
    pub channel_mask: u32,
    pub sub_format: Guid,
}

/// 共同字段校验与派生：返回 (block_align, avg_bytes_per_sec)。
const fn derive(
    sample_rate: u32,
    channels: u16,
    container_bits: u16,
    valid_bits: u16,
) -> Result<(u16, u32), FormatError> {
    if sample_rate == 0 {
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
    let block_align = match channels.checked_mul(container_bits / 8) {
        Some(v) => v,
        None => return Err(FormatError::Overflow),
    };
    let avg = match sample_rate.checked_mul(block_align as u32) {
        Some(v) => v,
        None => return Err(FormatError::Overflow),
    };
    Ok((block_align, avg))
}

impl WaveFormatEx {
    /// 纯 `WAVEFORMATEX` PCM 描述，本阶段只用于 16/16 单/双声道。
    pub const fn pcm16(sample_rate: u32, channels: u16) -> Result<Self, FormatError> {
        match derive(sample_rate, channels, 16, 16) {
            Ok((block_align, avg_bytes_per_sec)) => Ok(Self {
                format_tag: WAVE_FORMAT_PCM,
                channels,
                samples_per_sec: sample_rate,
                avg_bytes_per_sec,
                block_align,
                bits_per_sample: 16,
                cb_size: 0,
            }),
            Err(e) => Err(e),
        }
    }

    /// 由设备格式构造；只接受 C16/V16、小端。
    pub const fn from_device(f: &DeviceFormat) -> Result<Self, FormatError> {
        if !matches!(f.endian(), Endian::Little) {
            return Err(FormatError::Layout);
        }
        if f.container_bits() != 16 {
            return Err(FormatError::ContainerBits);
        }
        if f.valid_bits() != 16 {
            return Err(FormatError::ValidBits);
        }
        Self::pcm16(f.sample_rate(), f.channels())
    }

    /// Windows 内存布局字节（`#pragma pack(1)`，18 字节）。
    pub const fn to_bytes(&self) -> [u8; 18] {
        let t = self.format_tag.to_le_bytes();
        let c = self.channels.to_le_bytes();
        let r = self.samples_per_sec.to_le_bytes();
        let a = self.avg_bytes_per_sec.to_le_bytes();
        let b = self.block_align.to_le_bytes();
        let w = self.bits_per_sample.to_le_bytes();
        let s = self.cb_size.to_le_bytes();
        [
            t[0], t[1], c[0], c[1], r[0], r[1], r[2], r[3], a[0], a[1], a[2], a[3], b[0], b[1],
            w[0], w[1], s[0], s[1],
        ]
    }
}

impl WaveFormatExtensible {
    /// 纯 `WAVEFORMATEXTENSIBLE` PCM 描述。
    pub const fn pcm(
        sample_rate: u32,
        channels: u16,
        container_bits: u16,
        valid_bits: u16,
    ) -> Result<Self, FormatError> {
        let (block_align, avg_bytes_per_sec) =
            match derive(sample_rate, channels, container_bits, valid_bits) {
                Ok(v) => v,
                Err(e) => return Err(e),
            };
        let mask = match channel_mask(channels) {
            Ok(m) => m,
            Err(e) => return Err(e),
        };
        Ok(Self {
            format: WaveFormatEx {
                format_tag: WAVE_FORMAT_EXTENSIBLE,
                channels,
                samples_per_sec: sample_rate,
                avg_bytes_per_sec,
                block_align,
                bits_per_sample: container_bits,
                cb_size: EXTENSIBLE_CB_SIZE,
            },
            valid_bits_per_sample: valid_bits,
            channel_mask: mask,
            sub_format: KSDATAFORMAT_SUBTYPE_PCM,
        })
    }

    /// 由设备格式构造；要求小端、有效位高位对齐（WASAPI 语义）。
    pub const fn from_device(f: &DeviceFormat) -> Result<Self, FormatError> {
        if !matches!(f.endian(), Endian::Little) || !matches!(f.placement(), Placement::MsbAligned)
        {
            return Err(FormatError::Layout);
        }
        Self::pcm(
            f.sample_rate(),
            f.channels(),
            f.container_bits() as u16,
            f.valid_bits() as u16,
        )
    }

    /// Windows 内存布局字节（`#pragma pack(1)`，40 字节）。
    pub const fn to_bytes(&self) -> [u8; 40] {
        let mut out = [0u8; 40];
        let head = self.format.to_bytes();
        let mut i = 0;
        while i < 18 {
            out[i] = head[i];
            i += 1;
        }
        let v = self.valid_bits_per_sample.to_le_bytes();
        out[18] = v[0];
        out[19] = v[1];
        let m = self.channel_mask.to_le_bytes();
        out[20] = m[0];
        out[21] = m[1];
        out[22] = m[2];
        out[23] = m[3];
        let g = self.sub_format.to_bytes();
        let mut j = 0;
        while j < 16 {
            out[24 + j] = g[j];
            j += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PCM_GUID_BYTES: [u8; 16] = [
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B,
        0x71,
    ];

    fn ext(rate: u32, ch: u16, c: u16, v: u16) -> WaveFormatExtensible {
        match WaveFormatExtensible::pcm(rate, ch, c, v) {
            Ok(f) => f,
            Err(e) => panic!("{e:?}"),
        }
    }

    /// 独立书写的完整期望字段，逐项比较。
    #[allow(clippy::too_many_arguments)]
    fn expect_ext(
        f: WaveFormatExtensible,
        rate: u32,
        ch: u16,
        c: u16,
        v: u16,
        align: u16,
        avg: u32,
        mask: u32,
    ) {
        assert_eq!(
            f,
            WaveFormatExtensible {
                format: WaveFormatEx {
                    format_tag: 0xFFFE,
                    channels: ch,
                    samples_per_sec: rate,
                    avg_bytes_per_sec: avg,
                    block_align: align,
                    bits_per_sample: c,
                    cb_size: 22,
                },
                valid_bits_per_sample: v,
                channel_mask: mask,
                sub_format: Guid {
                    data1: 1,
                    data2: 0,
                    data3: 0x10,
                    data4: [0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71],
                },
            }
        );
        assert_eq!(f.sub_format.to_bytes(), PCM_GUID_BYTES);
    }

    #[test]
    fn extensible_matrix() {
        expect_ext(ext(44_100, 2, 16, 16), 44_100, 2, 16, 16, 4, 176_400, 0x3);
        expect_ext(ext(44_100, 1, 16, 16), 44_100, 1, 16, 16, 2, 88_200, 0x4);
        expect_ext(ext(48_000, 2, 24, 24), 48_000, 2, 24, 24, 6, 288_000, 0x3);
        expect_ext(ext(96_000, 2, 32, 24), 96_000, 2, 32, 24, 8, 768_000, 0x3);
        expect_ext(ext(192_000, 1, 32, 24), 192_000, 1, 32, 24, 4, 768_000, 0x4);
        expect_ext(
            ext(176_400, 2, 32, 32),
            176_400,
            2,
            32,
            32,
            8,
            1_411_200,
            0x3,
        );
        expect_ext(ext(88_200, 1, 24, 24), 88_200, 1, 24, 24, 3, 264_600, 0x4);
    }

    #[test]
    fn wave_format_ex_pcm16() {
        for (rate, ch, align, avg) in [(44_100u32, 2u16, 4u16, 176_400u32), (48_000, 1, 2, 96_000)]
        {
            let f = WaveFormatEx::pcm16(rate, ch);
            assert_eq!(
                f,
                Ok(WaveFormatEx {
                    format_tag: 1,
                    channels: ch,
                    samples_per_sec: rate,
                    avg_bytes_per_sec: avg,
                    block_align: align,
                    bits_per_sample: 16,
                    cb_size: 0,
                })
            );
            // 与 EXTENSIBLE 的共同字段一致。
            let e = ext(rate, ch, 16, 16).format;
            let Ok(f) = f else { panic!() };
            assert_eq!(
                (
                    f.samples_per_sec,
                    f.channels,
                    f.bits_per_sample,
                    f.block_align,
                    f.avg_bytes_per_sec
                ),
                (
                    e.samples_per_sec,
                    e.channels,
                    e.bits_per_sample,
                    e.block_align,
                    e.avg_bytes_per_sec
                )
            );
        }
    }

    #[test]
    fn byte_layout_independent_expected() {
        // 96k/双声道/C32/V24，按 Windows pack(1) 布局手写。
        let expected: [u8; 40] = [
            0xFE, 0xFF, // wFormatTag
            0x02, 0x00, // nChannels
            0x00, 0x77, 0x01, 0x00, // nSamplesPerSec 96000
            0x00, 0xB8, 0x0B, 0x00, // nAvgBytesPerSec 768000
            0x08, 0x00, // nBlockAlign
            0x20, 0x00, // wBitsPerSample
            0x16, 0x00, // cbSize 22
            0x18, 0x00, // wValidBitsPerSample
            0x03, 0x00, 0x00, 0x00, // dwChannelMask
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38,
            0x9B, 0x71,
        ];
        assert_eq!(ext(96_000, 2, 32, 24).to_bytes(), expected);
        let ex: [u8; 18] = [
            0x01, 0x00, 0x01, 0x00, 0x44, 0xAC, 0x00, 0x00, 0x88, 0x58, 0x01, 0x00, 0x02, 0x00,
            0x10, 0x00, 0x00, 0x00,
        ];
        assert_eq!(WaveFormatEx::pcm16(44_100, 1).map(|f| f.to_bytes()), Ok(ex));
    }

    #[test]
    fn rejects_invalid() {
        use FormatError::*;
        assert_eq!(WaveFormatExtensible::pcm(0, 2, 16, 16), Err(SampleRate));
        assert_eq!(WaveFormatExtensible::pcm(48_000, 0, 16, 16), Err(Channels));
        assert_eq!(WaveFormatExtensible::pcm(48_000, 3, 16, 16), Err(Channels));
        assert_eq!(WaveFormatExtensible::pcm(48_000, 2, 32, 0), Err(ValidBits));
        assert_eq!(WaveFormatExtensible::pcm(48_000, 2, 24, 32), Err(ValidBits));
        assert_eq!(
            WaveFormatExtensible::pcm(48_000, 2, 8, 8),
            Err(ContainerBits)
        );
        assert_eq!(
            WaveFormatExtensible::pcm(48_000, 2, 20, 20),
            Err(ContainerBits)
        );
        assert_eq!(
            WaveFormatExtensible::pcm(u32::MAX, 2, 32, 32),
            Err(Overflow)
        );
        assert_eq!(
            WaveFormatExtensible::pcm(600_000_000, 2, 32, 24),
            Err(Overflow)
        );
        assert_eq!(WaveFormatEx::pcm16(0, 1), Err(SampleRate));
        assert_eq!(WaveFormatEx::pcm16(48_000, 3), Err(Channels));
        assert_eq!(WaveFormatEx::pcm16(u32::MAX, 2), Err(Overflow));
    }

    #[test]
    fn from_device_requires_wasapi_layout() {
        let Ok(d) = DeviceFormat::wasapi(96_000, 2, 32, 24) else {
            panic!()
        };
        assert_eq!(
            WaveFormatExtensible::from_device(&d),
            Ok(ext(96_000, 2, 32, 24))
        );
        assert_eq!(
            WaveFormatEx::from_device(&d),
            Err(FormatError::ContainerBits)
        );
        let Ok(lsb) = DeviceFormat::new(96_000, 2, 32, 24, Endian::Little, Placement::LsbAligned)
        else {
            panic!()
        };
        assert_eq!(
            WaveFormatExtensible::from_device(&lsb),
            Err(FormatError::Layout)
        );
        let Ok(be) = DeviceFormat::new(48_000, 2, 16, 16, Endian::Big, Placement::MsbAligned)
        else {
            panic!()
        };
        assert_eq!(
            WaveFormatExtensible::from_device(&be),
            Err(FormatError::Layout)
        );
        assert_eq!(WaveFormatEx::from_device(&be), Err(FormatError::Layout));
        let Ok(d16) = DeviceFormat::wasapi(48_000, 1, 16, 16) else {
            panic!()
        };
        assert_eq!(
            WaveFormatEx::from_device(&d16),
            WaveFormatEx::pcm16(48_000, 1)
        );
        let Ok(d16v12) = DeviceFormat::wasapi(48_000, 1, 16, 12) else {
            panic!()
        };
        assert_eq!(
            WaveFormatEx::from_device(&d16v12),
            Err(FormatError::ValidBits)
        );
    }
}
