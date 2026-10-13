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
