//! 测试辅助：夹具路径与独立参考，以及用真实 WAV 解码与 pack 向队列供数。

use std::path::{Path, PathBuf};

use qy_codec_wav::WavDecoder;
use qy_core::{Decoder, DeviceFormat, PCM_WORK_SAMPLES, Pcm, Status};
use qy_pcm_format::ring::{FrameProducer, PACK_SCRATCH_BYTES};

pub fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../qy-codec-wav/tests/fixtures")
        .join(name)
}

pub fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(fixture_path(name)).unwrap_or_default()
}

/// 独立提取 data chunk 原始字节（只用于有效夹具）。
pub fn data_chunk(wav: &[u8]) -> Vec<u8> {
    let mut i = 12;
    while i + 8 <= wav.len() {
        let size = u32::from_le_bytes([wav[i + 4], wav[i + 5], wav[i + 6], wav[i + 7]]) as usize;
        if &wav[i..i + 4] == b"data" {
            return wav[i + 8..(i + 8 + size).min(wav.len())].to_vec();
        }
        i += 8 + size + (size & 1);
    }
    Vec::new()
}

/// 把同一 WAV 解码 `repeat` 遍，按需 pack 入队。
pub struct Feeder {
    decoder: WavDecoder,
    wav: Vec<u8>,
    pos: usize,
    repeat_left: usize,
    scratch: Vec<u8>,
    pub format: DeviceFormat,
    pub pushed: u64,
}

impl Feeder {
    pub fn new(name: &str, format: DeviceFormat, repeat: usize) -> Self {
        let pcm = Box::leak(vec![0i32; PCM_WORK_SAMPLES].into_boxed_slice());
        let Ok(decoder) = WavDecoder::new(pcm) else {
            panic!("decoder");
        };
        Self {
            decoder,
            wav: fixture(name),
            pos: 0,
            repeat_left: repeat,
            scratch: vec![0u8; PACK_SCRATCH_BYTES],
            format,
            pushed: 0,
        }
    }

    pub fn exhausted(&self) -> bool {
        self.repeat_left == 0
    }

    /// 最多入队 `max_frames` 帧，返回实际入队帧数。
    pub fn feed(&mut self, producer: &mut FrameProducer, max_frames: usize) -> usize {
        let mut done = 0;
        while done < max_frames && self.repeat_left > 0 {
            if self.decoder.pcm().is_empty() {
                let Ok(p) = self.decoder.decode(&self.wav[self.pos..], true) else {
                    panic!("decode");
                };
                self.pos += p.consumed;
                if p.status == Status::End {
                    self.repeat_left -= 1;
                    self.decoder.reset();
                    self.pos = 0;
                }
                continue;
            }
            let Some(fmt) = self.decoder.format() else {
                panic!("format");
            };
            let ch = usize::from(fmt.channels());
            let avail = self.decoder.pcm().len() / ch;
            let k = avail.min(max_frames - done);
            let Ok(pcm) = Pcm::exact(&self.decoder.pcm()[..k * ch], fmt) else {
                panic!("pcm");
            };
            let Ok(n) = producer.pack_push(&pcm, &self.format, &mut self.scratch) else {
                panic!("pack_push");
            };
            self.decoder.release(n);
            done += n;
            if n == 0 {
                break;
            }
        }
        self.pushed += done as u64;
        done
    }
}

pub fn flac_fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../qy-codec-flac/tests/fixtures")
        .join(name)
}

/// libFLAC 参考样本（`.s32le`，交错右对齐 i32），见 qy-codec-flac 夹具 MANIFEST。
pub fn flac_reference(stem: &str) -> Vec<i32> {
    std::fs::read(flac_fixture_path(&format!("{stem}.s32le")))
        .unwrap_or_default()
        .chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// ffmpeg 第二参考（`.ffmpeg.s32le`，左对齐到 32 位）还原为 N 位样本。
pub fn ffmpeg_reference(wav_name: &str, n: u8) -> Vec<i32> {
    let stem = wav_name.trim_end_matches(".wav");
    std::fs::read(fixture_path(&format!("{stem}.ffmpeg.s32le")))
        .unwrap_or_default()
        .chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) >> (32 - u32::from(n)))
        .collect()
}

/// 独立公式：N 位样本放入容器 C 的高位（WASAPI 左对齐），小端取 C/8 字节。
pub fn place(samples: &[i32], n: u8, c: u8) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|&s| {
            let word = (i64::from(s) << (c - n)) as u32;
            word.to_le_bytes()[..usize::from(c / 8)].to_vec()
        })
        .collect()
}

/// `qy-codec-aiff` 的夹具路径（F2）。
pub fn aiff_fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../qy-codec-aiff/tests/fixtures")
        .join(name)
}
