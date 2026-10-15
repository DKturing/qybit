//! 设备矩阵用的测试信号（M0 T6 起；M2 T3 移入平台无关层，供 WASAPI 与 ALSA 共用）。

use std::path::Path;

/// 写一个 N 位 PCM（格式码 1）测试 WAV：约 −30 dBFS、周期 96 样本的方波，
/// 各声道相同。返回帧数。
pub fn write_test_wav(
    path: &Path,
    rate: u32,
    channels: u16,
    bits: u8,
    frames: u64,
) -> std::io::Result<u64> {
    write_test_wav_layout(path, rate, channels, bits, frames, None)
}

/// 同 [`write_test_wav`]；给出声道掩码时写 WAVE_FORMAT_EXTENSIBLE（容器 = 有效位，
/// `dwChannelMask` = `mask`），多声道（F1）用。
pub fn write_test_wav_layout(
    path: &Path,
    rate: u32,
    channels: u16,
    bits: u8,
    frames: u64,
    mask: Option<u32>,
) -> std::io::Result<u64> {
    let bytes = usize::from(bits / 8);
    let amp: i64 = (1i64 << (bits - 1)) / 32;
    let mut data = Vec::with_capacity(frames as usize * usize::from(channels) * bytes);
    for i in 0..frames {
        let s = if (i / 48) % 2 == 0 { amp } else { -amp };
        for _ in 0..channels {
            data.extend_from_slice(&(s as u32).to_le_bytes()[..bytes]);
        }
    }
    let align = channels * u16::from(bits / 8);
    let mut fmt = Vec::with_capacity(40);
    fmt.extend_from_slice(&(if mask.is_some() { 0xFFFEu16 } else { 1 }).to_le_bytes());
    fmt.extend_from_slice(&channels.to_le_bytes());
    fmt.extend_from_slice(&rate.to_le_bytes());
    fmt.extend_from_slice(&(rate * u32::from(align)).to_le_bytes());
    fmt.extend_from_slice(&align.to_le_bytes());
    fmt.extend_from_slice(&u16::from(bits).to_le_bytes());
    if let Some(m) = mask {
        fmt.extend_from_slice(&22u16.to_le_bytes());
        fmt.extend_from_slice(&u16::from(bits).to_le_bytes());
        fmt.extend_from_slice(&m.to_le_bytes());
        // KSDATAFORMAT_SUBTYPE_PCM
        fmt.extend_from_slice(&[
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38,
            0x9B, 0x71,
        ]);
    }
    let pad = data.len() as u32 & 1;
    let mut w = Vec::with_capacity(data.len() + 68);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(4 + 8 + fmt.len() as u32 + 8 + data.len() as u32 + pad).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    w.extend_from_slice(&fmt);
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(data.len() as u32).to_le_bytes());
    w.extend_from_slice(&data);
    if data.len() % 2 == 1 {
        w.push(0);
    }
    std::fs::write(path, w)?;
    Ok(frames)
}

/// 32 位浮点测试 WAV（F3）：格式码 3（`WAVE_FORMAT_IEEE_FLOAT`）、带 fact 块；与
/// [`write_test_wav`] 同样的方波，幅度 ±1/32（由 16 位整数生成，转为任何 V ≥ 16 位的整数都精确）。
pub fn write_test_wav_float(
    path: &Path,
    rate: u32,
    channels: u16,
    frames: u64,
) -> std::io::Result<u64> {
    let amp = 1024.0f32 / 32_768.0;
    let mut data = Vec::with_capacity(frames as usize * usize::from(channels) * 4);
    for i in 0..frames {
        let s = if (i / 48) % 2 == 0 { amp } else { -amp };
        for _ in 0..channels {
            data.extend_from_slice(&s.to_bits().to_le_bytes());
        }
    }
    let align = channels * 4;
    let mut fmt = Vec::with_capacity(18);
    fmt.extend_from_slice(&3u16.to_le_bytes());
    fmt.extend_from_slice(&channels.to_le_bytes());
    fmt.extend_from_slice(&rate.to_le_bytes());
    fmt.extend_from_slice(&(rate * u32::from(align)).to_le_bytes());
    fmt.extend_from_slice(&align.to_le_bytes());
    fmt.extend_from_slice(&32u16.to_le_bytes());
    fmt.extend_from_slice(&0u16.to_le_bytes());
    let frames32 = u32::try_from(frames).unwrap_or(u32::MAX);
    let mut w = Vec::with_capacity(data.len() + 80);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(4 + 8 + 18 + 12 + 8 + data.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&18u32.to_le_bytes());
    w.extend_from_slice(&fmt);
    w.extend_from_slice(b"fact");
    w.extend_from_slice(&4u32.to_le_bytes());
    w.extend_from_slice(&frames32.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(data.len() as u32).to_le_bytes());
    w.extend_from_slice(&data);
    std::fs::write(path, w)?;
    Ok(frames)
}
