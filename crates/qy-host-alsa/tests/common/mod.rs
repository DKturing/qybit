//! 回环测试共用：独立的信号生成、WAV 写入与期望设备字节公式（不调用被测的解码与 pack）。
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use qy_host_alsa::loopback::LoopCase;
use qy_pcm_format::alsa::AlsaFormat;

pub const RATES: [u32; 4] = [44_100, 48_000, 96_000, 192_000];
pub const SOURCE_BITS: [u8; 4] = [16, 20, 24, 32];
/// 同步曲目帧数与前导静音帧数。
pub const SYNC_FRAMES: usize = 1024;
pub const LEAD_SILENCE: usize = 64;

/// 全部用例：采样率 × 格式 × 声道 × 源有效位（含 N > V 的拒绝用例）。
pub fn cases() -> Vec<LoopCase> {
    let mut v = Vec::new();
    for rate in RATES {
        for format in AlsaFormat::ALL {
            for channels in [1u16, 2] {
                for source_bits in SOURCE_BITS {
                    v.push(LoopCase {
                        rate,
                        channels,
                        format,
                        source_bits,
                    });
                }
            }
        }
    }
    v
}

/// SplitMix64。
pub struct Prng(pub u64);
impl Prng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// N 位有符号满幅范围内的随机样本。
    pub fn sample(&mut self, n: u8) -> i32 {
        ((self.next() >> (64 - u32::from(n))) as i64 - (1i64 << (n - 1))) as i32
    }
}

/// 伪随机同步曲目（无静音帧）。
pub fn sync_samples(n: u8, channels: u16, seed: u64) -> Vec<i32> {
    let mut r = Prng(seed);
    (0..SYNC_FRAMES * usize::from(channels))
        .map(|_| {
            loop {
                let s = r.sample(n);
                if s != 0 {
                    break s;
                }
            }
        })
        .collect()
}

/// 待测曲目：真实的前导静音、各声道极值（最小、最大、−1、1、0），随机段，最后一帧满幅。
pub fn test_samples(n: u8, channels: u16, frames: usize, seed: u64) -> Vec<i32> {
    let ch = usize::from(channels);
    let min = -(1i64 << (n - 1)) as i32;
    let max = ((1i64 << (n - 1)) - 1) as i32;
    let mut v = vec![0i32; LEAD_SILENCE * ch];
    for s in [min, max, -1, 1, 0] {
        v.extend(std::iter::repeat_n(s, ch));
    }
    let mut r = Prng(seed);
    while v.len() < (frames - 1) * ch {
        v.push(r.sample(n));
    }
    v.extend(std::iter::repeat_n(max, ch));
    v
}

/// 独立 WAV 写入：N = 16/24/32 用格式码 1（容器 = N）；N = 20 用 EXTENSIBLE 24/20
/// （有效位左对齐写入 24 位容器，低 4 位为 0）。
pub fn write_wav(path: &Path, rate: u32, channels: u16, n: u8, samples: &[i32]) {
    write_wav_mask(path, rate, channels, n, samples, None);
}

/// 同 [`write_wav`]；给出 `mask` 时一律写 EXTENSIBLE 并写入该声道掩码（F1 多声道）。
pub fn write_wav_mask(
    path: &Path,
    rate: u32,
    channels: u16,
    n: u8,
    samples: &[i32],
    mask: Option<u32>,
) {
    let (container, ext) = match n {
        16 | 24 | 32 => (n, mask.is_some()),
        20 => (24, true),
        _ => panic!("不支持的测试位深 {n}"),
    };
    let bytes = usize::from(container / 8);
    let align = channels * u16::from(container / 8);
    let mut data = Vec::with_capacity(samples.len() * bytes);
    for &s in samples {
        let w = (i64::from(s) << (container - n)) as u32;
        data.extend_from_slice(&w.to_le_bytes()[..bytes]);
    }
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&(if ext { 0xFFFEu16 } else { 1 }).to_le_bytes());
    fmt.extend_from_slice(&channels.to_le_bytes());
    fmt.extend_from_slice(&rate.to_le_bytes());
    fmt.extend_from_slice(&(rate * u32::from(align)).to_le_bytes());
    fmt.extend_from_slice(&align.to_le_bytes());
    fmt.extend_from_slice(&u16::from(container).to_le_bytes());
    if ext {
        fmt.extend_from_slice(&22u16.to_le_bytes());
        fmt.extend_from_slice(&u16::from(n).to_le_bytes());
        let mask: u32 = mask.unwrap_or(if channels == 1 { 0x4 } else { 0x3 });
        fmt.extend_from_slice(&mask.to_le_bytes());
        fmt.extend_from_slice(&[
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38,
            0x9B, 0x71,
        ]);
    }
    let mut w = b"RIFF".to_vec();
    w.extend_from_slice(&((4 + 8 + fmt.len() + 8 + data.len()) as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    w.extend_from_slice(&fmt);
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(data.len() as u32).to_le_bytes());
    w.extend_from_slice(&data);
    if let Err(e) = std::fs::write(path, w) {
        panic!("写 {} 失败: {e}", path.display());
    }
}

/// 独立公式：N 位样本在各 ALSA 格式中的设备字节（小端）。
/// - `S16_LE`/`S24_3LE`/`S32_LE`：左移 C − N 位，取 C/8 字节；
/// - `S24_LE`：左移 24 − N 位放在 32 位字的低 24 位，高字节为符号扩展。
pub fn device_bytes(samples: &[i32], n: u8, format: AlsaFormat) -> Vec<u8> {
    let mut out = Vec::new();
    for &s in samples {
        let s = i64::from(s);
        match format {
            AlsaFormat::S16Le => out.extend_from_slice(&((s << (16 - n)) as u16).to_le_bytes()),
            AlsaFormat::S24_3Le => {
                out.extend_from_slice(&((s << (24 - n)) as u32).to_le_bytes()[..3])
            }
            AlsaFormat::S24Le => out.extend_from_slice(&((s << (24 - n)) as i32).to_le_bytes()),
            AlsaFormat::S32Le => out.extend_from_slice(&((s << (32 - n)) as u32).to_le_bytes()),
        }
    }
    out
}

/// 报告目录：`QYBIT_REPORT_DIR`，默认 `target/qybit-report`。
pub fn report_dir() -> PathBuf {
    std::env::var("QYBIT_REPORT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/qybit-report")
        })
}

/// 一个用例的曲目文件与期望字节。
pub struct Tracks {
    pub sync_path: PathBuf,
    pub test_path: PathBuf,
    pub sync_bytes: Vec<u8>,
    pub test_bytes: Vec<u8>,
}

/// 生成同步与待测曲目（同格式，播放时无缝衔接为一个流）。
pub fn make_tracks(
    dir: &Path,
    tag: &str,
    case: &LoopCase,
    test_frames: usize,
    seed: u64,
) -> Tracks {
    let n = case.source_bits;
    let sync = sync_samples(n, case.channels, seed);
    let test = test_samples(n, case.channels, test_frames, seed ^ 0xABCD);
    let sync_path = dir.join(format!("{tag}-sync.wav"));
    let test_path = dir.join(format!("{tag}-test.wav"));
    write_wav(&sync_path, case.rate, case.channels, n, &sync);
    write_wav(&test_path, case.rate, case.channels, n, &test);
    Tracks {
        sync_path,
        test_path,
        sync_bytes: if n <= case.format.valid_bits() {
            device_bytes(&sync, n, case.format)
        } else {
            Vec::new()
        },
        test_bytes: if n <= case.format.valid_bits() {
            device_bytes(&test, n, case.format)
        } else {
            Vec::new()
        },
    }
}
