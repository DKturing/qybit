//! 生产端流水线（M0 T3）：真实 WAV 解码 → Pcm<Exact> → pack → SPSC 队列 → 消费端。
//!
//! 设备格式与源容器布局相同时，消费端得到的字节必须与 WAV data chunk 原始字节逐字节一致
//! （夹具生成器写入零填充位，独立于本实现）。同一曲目重复多遍以跨越队列回绕点；
//! 消费端用不规则块大小取数，只按完整帧出队。

use std::fs;
use std::path::Path;

use qy_core::{Decoder, Status};
use qy_host_sim::SimHost;
use qy_pcm_format::DeviceFormat;
use qy_pcm_format::pack::{PackError, Pcm};
use qy_pcm_format::ring::{DEFAULT_CAPACITY_BYTES, RingError};

fn fixture(name: &str) -> Vec<u8> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../qy-codec-wav/tests/fixtures")
        .join(name);
    fs::read(p).unwrap_or_default()
}

/// 测试内独立的 data chunk 提取（只用于有效夹具）。
fn data_chunk(wav: &[u8]) -> &[u8] {
    let mut i = 12;
    while i + 8 <= wav.len() {
        let size = u32::from_le_bytes([wav[i + 4], wav[i + 5], wav[i + 6], wav[i + 7]]) as usize;
        if &wav[i..i + 4] == b"data" {
            return &wav[i + 8..i + 8 + size];
        }
        i += 8 + size + (size & 1);
    }
    panic!("无 data chunk");
}

/// 解码 `wav` 共 `repeat` 遍，经 pack 与队列输出全部字节。
fn pipeline(host: &mut SimHost, wav: &[u8], dev: &DeviceFormat, repeat: usize) -> Vec<u8> {
    let Ok(_) = host.configure_device(dev) else {
        panic!("configure");
    };
    let fb = usize::from(dev.block_align());
    let mut out = Vec::new();
    let mut pop_buf = vec![0u8; 1000 * fb];
    let mut block = 1usize;
    let mut pop = |host: &mut SimHost, out: &mut Vec<u8>, all: bool| loop {
        block = block * 7 % 997 + 1; // 不规则消费块
        let want = if all { pop_buf.len() } else { block * fb };
        let Ok(n) = host.consumer.pop_frames(&mut pop_buf[..want]) else {
            panic!("pop");
        };
        out.extend_from_slice(&pop_buf[..n * fb]);
        if n == 0 || !all {
            break;
        }
    };
    for _ in 0..repeat {
        host.decoder.reset();
        let mut pos = 0;
        loop {
            let Ok(p) = host.decoder.decode(&wav[pos..], true) else {
                panic!("decode");
            };
            pos += p.consumed;
            match p.status {
                Status::Frames(_) | Status::NeedOutput => {
                    let Some(fmt) = host.decoder.format() else {
                        panic!("format");
                    };
                    while !host.decoder.pcm().is_empty() {
                        let Ok(pcm) = Pcm::exact(host.decoder.pcm(), fmt) else {
                            panic!("pcm");
                        };
                        let Ok(n) = host.producer.pack_push(&pcm, dev, host.pack_scratch) else {
                            panic!("pack_push");
                        };
                        host.decoder.release(n);
                        // 保持队列较满，消费端不规则取数。
                        pop(host, &mut out, false);
                    }
                }
                Status::End => break,
                Status::NeedInput => panic!("eof 下不应缺输入"),
            }
        }
    }
    pop(host, &mut out, true);
    out
}

#[test]
fn identity_layouts_match_raw_data_bytes_across_wraparound() {
    let Ok(mut host) = SimHost::new() else {
        panic!("host");
    };
    let cases = [
        (
            "pcm16_stereo_44k.wav",
            DeviceFormat::wasapi(44_100, 2, 16, 16),
        ),
        (
            "pcm24_stereo_large.wav",
            DeviceFormat::wasapi(88_200, 2, 24, 24),
        ),
        (
            "ext24in32_stereo_192k.wav",
            DeviceFormat::wasapi(192_000, 2, 32, 24),
        ),
        (
            "pcm32_stereo_96k.wav",
            DeviceFormat::wasapi(96_000, 2, 32, 32),
        ),
        (
            "ext20in24_mono_center.wav",
            DeviceFormat::wasapi(96_000, 1, 24, 20),
        ),
        (
            "ext12in16_stereo.wav",
            DeviceFormat::wasapi(48_000, 2, 16, 12),
        ),
    ];
    for (name, dev) in cases {
        let Ok(dev) = dev else { panic!("{name}") };
        let wav = fixture(name);
        let raw = data_chunk(&wav);
        // 重复次数保证总字节数超过队列容量，覆盖回绕。
        let repeat = DEFAULT_CAPACITY_BYTES / raw.len().max(1) + 2;
        let got = pipeline(&mut host, &wav, &dev, repeat);
        assert_eq!(got.len(), raw.len() * repeat, "{name}");
        for (k, chunk) in got.chunks_exact(raw.len()).enumerate() {
            assert!(chunk == raw, "{name}: 第 {k} 遍字节不一致");
        }
    }
}

#[test]
fn widening_matches_independent_formula() {
    // 16 位源 → WASAPI 32/24：每个样本应为 s << 16 的小端 4 字节。
    let Ok(mut host) = SimHost::new() else {
        panic!("host");
    };
    let wav = fixture("pcm16_stereo_44k.wav");
    let raw = data_chunk(&wav);
    let Ok(dev) = DeviceFormat::wasapi(44_100, 2, 32, 24) else {
        panic!("dev");
    };
    let got = pipeline(&mut host, &wav, &dev, 1);
    let want: Vec<u8> = raw
        .chunks_exact(2)
        .flat_map(|b| [0, 0, b[0], b[1]])
        .collect();
    assert_eq!(got, want);
}

#[test]
fn producer_rejects_precision_loss_and_wrong_width() {
    let Ok(mut host) = SimHost::new() else {
        panic!("host");
    };
    let wav = fixture("pcm32_stereo_96k.wav");
    let Ok(d24) = DeviceFormat::wasapi(96_000, 2, 32, 24) else {
        panic!()
    };
    assert!(host.configure_device(&d24).is_ok());
    let Ok(p) = host.decoder.decode(&wav, true) else {
        panic!("decode");
    };
    assert!(matches!(p.status, Status::Frames(_)));
    let Some(fmt) = host.decoder.format() else {
        panic!("format");
    };
    let Ok(pcm) = Pcm::exact(host.decoder.pcm(), fmt) else {
        panic!("pcm");
    };
    // N=32 > V=24：拒绝，不降精度。
    assert_eq!(
        host.producer.pack_push(&pcm, &d24, host.pack_scratch),
        Err(RingError::Pack(PackError::Precision {
            source_bits: 32,
            valid_bits: 24
        }))
    );
    assert_eq!(host.consumer.available_frames(), 0);
    // 设备帧宽与队列配置不符。
    let Ok(d16) = DeviceFormat::wasapi(96_000, 2, 16, 16) else {
        panic!()
    };
    assert_eq!(
        host.producer.pack_push(&pcm, &d16, host.pack_scratch),
        Err(RingError::FrameWidth)
    );
}
