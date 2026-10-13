//! 纯格式描述到 Windows SDK 结构的字段映射（M0 T3）。
//!
//! 只做逐字段复制，不改变任何值；不调用 COM。字段构造与校验在
//! `qy_pcm_format::wave` 中完成，可在任何平台测试。

use qy_pcm_format::wave::{Guid, WaveFormatEx, WaveFormatExtensible};
use windows::Win32::Media::Audio::{WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0};
use windows::core::GUID;

pub fn guid_to_sdk(g: &Guid) -> GUID {
    GUID {
        data1: g.data1,
        data2: g.data2,
        data3: g.data3,
        data4: g.data4,
    }
}

pub fn wave_format_ex_to_sdk(f: &WaveFormatEx) -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: f.format_tag,
        nChannels: f.channels,
        nSamplesPerSec: f.samples_per_sec,
        nAvgBytesPerSec: f.avg_bytes_per_sec,
        nBlockAlign: f.block_align,
        wBitsPerSample: f.bits_per_sample,
        cbSize: f.cb_size,
    }
}

pub fn extensible_to_sdk(f: &WaveFormatExtensible) -> WAVEFORMATEXTENSIBLE {
    WAVEFORMATEXTENSIBLE {
        Format: wave_format_ex_to_sdk(&f.format),
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: f.valid_bits_per_sample,
        },
        dwChannelMask: f.channel_mask,
        SubFormat: guid_to_sdk(&f.sub_format),
    }
}

#[cfg(test)]
mod tests {
    #![allow(unsafe_code)]
    use super::*;
    use qy_pcm_format::wave::WaveFormatExtensible;
    use windows::Win32::Media::Audio::WAVE_FORMAT_PCM;
    use windows::Win32::Media::KernelStreaming::{
        KSDATAFORMAT_SUBTYPE_PCM, SPEAKER_FRONT_CENTER, SPEAKER_FRONT_LEFT, SPEAKER_FRONT_RIGHT,
        WAVE_FORMAT_EXTENSIBLE,
    };

    fn raw<T>(v: &T) -> &[u8] {
        // SAFETY: T 为 SDK 的 repr(C, packed(1)) 纯数据结构，按其大小读取字节。
        unsafe { core::slice::from_raw_parts((v as *const T).cast::<u8>(), size_of::<T>()) }
    }

    #[test]
    fn layout_sizes() {
        assert_eq!(size_of::<WAVEFORMATEX>(), 18);
        assert_eq!(size_of::<WAVEFORMATEXTENSIBLE>(), 40);
        assert_eq!(size_of::<GUID>(), 16);
    }

    #[test]
    fn extensible_fields_and_bytes_map_exactly() {
        for (rate, ch, c, v) in [
            (44_100u32, 2u16, 16u16, 16u16),
            (48_000, 1, 24, 24),
            (96_000, 2, 32, 24),
            (192_000, 2, 32, 32),
        ] {
            let Ok(pure) = WaveFormatExtensible::pcm(rate, ch, c, v) else {
                panic!("pure");
            };
            let sdk = extensible_to_sdk(&pure);
            let fmt = sdk.Format;
            assert_eq!(u32::from(fmt.wFormatTag), WAVE_FORMAT_EXTENSIBLE);
            assert_eq!({ fmt.nChannels }, ch);
            assert_eq!({ fmt.nSamplesPerSec }, rate);
            assert_eq!({ fmt.nBlockAlign }, ch * c / 8);
            assert_eq!({ fmt.nAvgBytesPerSec }, rate * u32::from(ch * c / 8));
            assert_eq!({ fmt.wBitsPerSample }, c);
            assert_eq!({ fmt.cbSize }, 22);
            // SAFETY: 本模块只写入 wValidBitsPerSample 变体。
            assert_eq!(unsafe { sdk.Samples.wValidBitsPerSample }, v);
            let mask = if ch == 1 {
                SPEAKER_FRONT_CENTER
            } else {
                SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT
            };
            assert_eq!({ sdk.dwChannelMask }, mask);
            assert_eq!({ sdk.SubFormat }, KSDATAFORMAT_SUBTYPE_PCM);
            assert_eq!(raw(&sdk), &pure.to_bytes());
        }
    }

    #[test]
    fn wave_format_ex_maps_exactly() {
        for ch in [1u16, 2] {
            let Ok(pure) = WaveFormatEx::pcm16(44_100, ch) else {
                panic!("pure");
            };
            let sdk = wave_format_ex_to_sdk(&pure);
            assert_eq!(u32::from(sdk.wFormatTag), WAVE_FORMAT_PCM);
            assert_eq!({ sdk.cbSize }, 0);
            assert_eq!({ sdk.nBlockAlign }, ch * 2);
            assert_eq!(raw(&sdk), &pure.to_bytes());
        }
    }
}
