//! 诊断子命令（与平台无关）：用来区分“解码 / pack 出错”与“设备输出出错”。
//!
//! - `decode`：用自有解码器解码，经 pack（与播放时同一条表示转换路径）写成标准 WAV
//!   （WAVE_FORMAT_EXTENSIBLE，容器 = 有效位：N ≤ 16 → 16，N ≤ 24 → 24，否则 32）。浮点源（F3）
//!   按位原样写成同一浮点格式的 WAV（IEEE_FLOAT 子格式，带 fact 块）。
//!   这个文件在任何普通播放器里听起来正常，就说明解码与 pack 正确，问题在设备输出一侧。
//! - `tone`：写一个正弦测试音（整数或浮点 WAV、整数 AIFF）。正弦比方波更容易听出失真，可用来
//!   比较不同采样率的输出，也用来生成真机测试用的浮点与 AIFF 文件。
//!
//! 样本值不变：decode 只做 N ≤ V 的左对齐放置；tone 是生成的测试信号，不是音源。

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use qy_codec_auto::AutoDecoder;
use qy_codec_auto::profile::DESKTOP;
use qy_core::engine::Resources;
use qy_core::{ChannelLayout, Decoder, DeviceFormat, PCM_WORK_SAMPLES, SampleEncoding, Status};
use qy_pcm_format::pack::{Pcm, pack};
use qy_pcm_format::wave::WaveFormatExtensible;

fn leak<T: Copy + Default>(len: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); len].into_boxed_slice())
}

/// WAV 文件头（fmt 为 40 字节的 EXTENSIBLE）。浮点格式另写 fact 块（内容为样本帧数）：RIFF 1991
/// 原文只要求压缩格式有 fact；浮点 WAV 的通行做法是写上，便于其他程序读取（本程序的解码器不要求）。
fn wav_header(format: &DeviceFormat, data_len: u32) -> Result<Vec<u8>, String> {
    let fmt = WaveFormatExtensible::from_device(format)
        .map_err(|e| format!("无法描述格式：{e:?}"))?
        .to_bytes();
    let fact = format.encoding().is_float();
    let fact_len = if fact { 12 } else { 0 };
    let mut h = Vec::with_capacity(80);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(
        &(4 + 8 + 40 + 8u32 + fact_len)
            .saturating_add(data_len)
            .to_le_bytes(),
    );
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&40u32.to_le_bytes());
    h.extend_from_slice(&fmt);
    if fact {
        let frames = data_len / u32::from(format.block_align().max(1));
        h.extend_from_slice(b"fact");
        h.extend_from_slice(&4u32.to_le_bytes());
        h.extend_from_slice(&frames.to_le_bytes());
    }
    h.extend_from_slice(b"data");
    h.extend_from_slice(&data_len.to_le_bytes());
    Ok(h)
}

fn write_wav(out: &Path, format: &DeviceFormat, data: &[u8]) -> Result<(), String> {
    let len = u32::try_from(data.len()).map_err(|_| "输出超过 4 GiB，WAV 无法表示".to_string())?;
    let mut f = File::create(out).map_err(|e| format!("创建 {}：{e}", out.display()))?;
    f.write_all(&wav_header(format, len)?)
        .and_then(|()| f.write_all(data))
        .map_err(|e| format!("写入 {}：{e}", out.display()))
}

/// 解码 `input`，经 pack 写成标准 WAV；返回给用户看的说明。
pub fn decode(input: &Path, out: &Path) -> Result<String, String> {
    let mut data = Vec::new();
    File::open(input)
        .and_then(|mut f| f.read_to_end(&mut data))
        .map_err(|e| format!("读取 {}：{e}", input.display()))?;
    let cap = DESKTOP.flac_capacity().ok_or("配置档未启用 FLAC")?;
    let mut d = AutoDecoder::new(
        leak(PCM_WORK_SAMPLES),
        cap,
        leak(cap.workspace_len()),
        leak(cap.pcm_len()),
    )
    .map_err(|e| format!("解码器：{e:?}"))?;
    let window = d.required_window();
    let mut scratch = vec![0u8; cap.pcm_len().max(PCM_WORK_SAMPLES) * 4];
    let mut pcm_bytes = Vec::new();
    let mut device = None;
    let mut frames = 0u64;
    let mut pos = 0usize;
    let ended = loop {
        let end = (pos + window).min(data.len());
        let eof = end == data.len();
        let p = match d.decode(&data[pos..end], eof) {
            Ok(p) => p,
            Err(e) => break Err(e),
        };
        pos += p.consumed;
        match p.status {
            Status::Frames(n) => {
                let src = d.format().ok_or("产出帧时格式未知")?;
                let dev = match device {
                    Some(f) => f,
                    None => {
                        let c = match src.bits() {
                            0..=16 => 16,
                            17..=24 => 24,
                            _ => 32,
                        };
                        let f = if src.encoding().is_float() {
                            DeviceFormat::wasapi_float(
                                src.sample_rate(),
                                src.layout(),
                                src.encoding(),
                            )
                        } else {
                            DeviceFormat::wasapi_layout(src.sample_rate(), src.layout(), c, c)
                        }
                        .map_err(|e| format!("输出格式：{e:?}"))?;
                        device = Some(f);
                        f
                    }
                };
                let words = src.words_per_frame();
                let pcm = Pcm::exact(&d.pcm()[..n * words], src).map_err(|e| format!("{e:?}"))?;
                let k = pack(&pcm, &dev, &mut scratch).map_err(|e| format!("pack：{e:?}"))?;
                pcm_bytes.extend_from_slice(&scratch[..k * usize::from(dev.block_align())]);
                frames += k as u64;
                d.release(k);
            }
            Status::End => break Ok(()),
            Status::NeedInput if p.consumed == 0 && (eof || end - pos >= window) => {
                return Err("解码器缺输入但无法推进".into());
            }
            Status::NeedInput | Status::NeedOutput => {}
        }
    };
    let Some(dev) = device else {
        return Err(match ended {
            Err(e) => format!("解码失败：{e:?}（{}）", detail(&d)),
            Ok(()) => "文件没有音频帧".into(),
        });
    };
    write_wav(out, &dev, &pcm_bytes)?;
    let src = d.format();
    let mut msg = String::new();
    if let Some(s) = src {
        msg.push_str(&format!(
            "源：{} Hz / {} 声道 / {}；",
            s.sample_rate(),
            s.channels(),
            qy_player::source_bits_text(&s)
        ));
    }
    msg.push_str(&format!(
        "写出 {frames} 帧（{:.2} 秒），容器 {} 位：{}",
        frames as f64 / f64::from(dev.sample_rate()),
        dev.container_bits(),
        out.display()
    ));
    // F5：AIFF-C 的非 NONE 未压缩类型写明类型码（twos、sowt、fl32、fl64）。
    if let Some(t) = d.aiff().and_then(|a| a.sample_type())
        && !t.is_plain()
    {
        msg.push_str(&format!("\n样本类型：{}", t.describe()));
    }
    if d.skipped_tag_bytes() > 0 {
        msg.push_str(&format!(
            "\n跳过文件开头的 ID3v2 标签 {} 字节（元数据，不含音频）",
            d.skipped_tag_bytes()
        ));
    }
    for i in 0..d.repair_count() {
        if let Some(r) = d.repair(i) {
            msg.push_str(&format!("\n按规则修正文件结构：{r}（样本未改动）"));
        }
    }
    if let Err(e) = ended {
        msg.push_str(&format!(
            "\n注意：解码在此之后出错：{e:?}（{}），只写出了出错前的完整帧",
            detail(&d)
        ));
    }
    Ok(msg)
}

fn detail(d: &AutoDecoder) -> String {
    if let Some(w) = d.wav_detail() {
        return w.to_string();
    }
    if let Some(a) = d.aiff_detail() {
        return a.to_string();
    }
    if let Some(u) = d.unrecognized() {
        return u.to_string();
    }
    if let Some(f) = d.flac_error() {
        return format!("{f:?}");
    }
    "无明细".into()
}

/// 写一个双声道正弦测试音：约 −12 dBFS，`freq` Hz，`seconds` 秒，左右声道相同。
/// 正弦测试音的样本格式。
enum ToneKind {
    Int(u8),
    Float(SampleEncoding),
}

/// 写一个双声道正弦测试音（约 −12 dBFS）。`float` 时 `bits` 为浮点位宽（32 / 64），写 IEEE 浮点
/// WAV（带 fact 块）；否则 `bits` 为整数位数。输出扩展名为 `.aif` / `.aiff` 时写 AIFF（整数，大端，
/// Apple AIFF 1.3），否则写 WAV。
pub fn tone(
    out: &Path,
    rate: u32,
    bits: u8,
    float: bool,
    seconds: f64,
    freq: f64,
) -> Result<String, String> {
    let kind = match (float, bits) {
        (false, 16 | 24 | 32) => ToneKind::Int(bits),
        (true, 32) => ToneKind::Float(SampleEncoding::Float32),
        (true, 64) => ToneKind::Float(SampleEncoding::Float64),
        _ => return Err(format!("不支持的位数：{bits}")),
    };
    let aiff = out
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("aif") || e.eq_ignore_ascii_case("aiff"));
    let frames = (f64::from(rate) * seconds).round() as u64;
    let sine = |i: u64| (std::f64::consts::TAU * freq * i as f64 / f64::from(rate)).sin() * 0.25;
    let mut data = Vec::new();
    let what = match kind {
        ToneKind::Int(bits) => {
            let amp = (f64::from(1u32 << (bits - 1)) - 1.0) / f64::from(1u32 << (bits - 1));
            let full = f64::from(1u32 << (bits - 1));
            let bytes = usize::from(bits / 8);
            for i in 0..frames {
                let s = (sine(i) * amp * full).round() as i64 as u32;
                for _ in 0..2 {
                    if aiff {
                        data.extend_from_slice(&s.to_be_bytes()[4 - bytes..]);
                    } else {
                        data.extend_from_slice(&s.to_le_bytes()[..bytes]);
                    }
                }
            }
            if aiff {
                write_aiff(out, rate, bits, frames, &data)?;
            } else {
                let format = DeviceFormat::wasapi_layout(rate, ChannelLayout::STEREO, bits, bits)
                    .map_err(|e| format!("格式：{e:?}"))?;
                write_wav(out, &format, &data)?;
            }
            format!("{bits} 位整数")
        }
        ToneKind::Float(encoding) => {
            if aiff {
                return Err("AIFF 不支持浮点（AIFF-C 浮点是规范之外的扩展）；浮点请写 .wav".into());
            }
            for i in 0..frames {
                let x = sine(i);
                for _ in 0..2 {
                    match encoding {
                        SampleEncoding::Float64 => data.extend_from_slice(&x.to_le_bytes()),
                        _ => data.extend_from_slice(&(x as f32).to_le_bytes()),
                    }
                }
            }
            let format = DeviceFormat::wasapi_float(rate, ChannelLayout::STEREO, encoding)
                .map_err(|e| format!("格式：{e:?}"))?;
            write_wav(out, &format, &data)?;
            format!("{} 位浮点", encoding.float_bits().unwrap_or(0))
        }
    };
    Ok(format!(
        "测试音：{freq} Hz 正弦，{rate} Hz / 双声道 / {what}{}，{seconds} 秒，约 −12 dBFS：{}",
        if aiff { "（AIFF）" } else { "" },
        out.display()
    ))
}

/// 80 位扩展精度（AIFF 1.3 COMM 的 sampleRate）：正整数采样率精确表示。
fn extended(rate: u32) -> [u8; 10] {
    let mut b = [0u8; 10];
    if rate == 0 {
        return b;
    }
    let shift = rate.leading_zeros();
    let exp = 16_383 + 31 - shift;
    b[..2].copy_from_slice(&(exp as u16).to_be_bytes());
    b[2..].copy_from_slice(&((u64::from(rate) << shift) << 32).to_be_bytes());
    b
}

/// AIFF（FORM AIFF：COMM + SSND，双声道，整数大端）。
fn write_aiff(out: &Path, rate: u32, bits: u8, frames: u64, data: &[u8]) -> Result<(), String> {
    let frames = u32::try_from(frames).map_err(|_| "帧数超出 AIFF 范围".to_string())?;
    let ssnd = u32::try_from(data.len() + 8).map_err(|_| "输出超过 4 GiB".to_string())?;
    let mut h = Vec::with_capacity(54);
    h.extend_from_slice(b"FORM");
    h.extend_from_slice(&(4 + 8 + 18 + 8u32).saturating_add(ssnd).to_be_bytes());
    h.extend_from_slice(b"AIFFCOMM");
    h.extend_from_slice(&18u32.to_be_bytes());
    h.extend_from_slice(&2u16.to_be_bytes());
    h.extend_from_slice(&frames.to_be_bytes());
    h.extend_from_slice(&u16::from(bits).to_be_bytes());
    h.extend_from_slice(&extended(rate));
    h.extend_from_slice(b"SSND");
    h.extend_from_slice(&ssnd.to_be_bytes());
    h.extend_from_slice(&[0u8; 8]);
    let mut f = File::create(out).map_err(|e| format!("创建 {}：{e}", out.display()))?;
    f.write_all(&h)
        .and_then(|()| f.write_all(data))
        .map_err(|e| format!("写入 {}：{e}", out.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("qybit-diag-{}-{name}", std::process::id()))
    }

    /// decode 写出的 WAV 再用自有解码器读回：样本与 ffmpeg 第二参考一致（经过 pack 往返）。
    #[test]
    fn decode_round_trips_against_ffmpeg_reference() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../qy-codec-wav/tests/fixtures");
        for (name, bits) in [("pcm16_stereo_44k", 16u32), ("pcm24_mono_48k_odd_list", 24)] {
            let out = tmp(&format!("{name}.wav"));
            if let Err(e) = decode(&dir.join(format!("{name}.wav")), &out) {
                panic!("{name}: {e}");
            }
            let reference: Vec<i32> = std::fs::read(dir.join(format!("{name}.ffmpeg.s32le")))
                .unwrap_or_default()
                .chunks_exact(4)
                .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) >> (32 - bits))
                .collect();
            let written = std::fs::read(&out).unwrap_or_default();
            let _ = std::fs::remove_file(&out);
            // 头部 68 字节之后为数据，容器 = 位宽（16 或 24）。
            let b = (bits / 8) as usize;
            let got: Vec<i32> = written[68..]
                .chunks_exact(b)
                .map(|c| {
                    let mut w = [0u8; 4];
                    w[4 - b..].copy_from_slice(c);
                    i32::from_le_bytes(w) >> (32 - bits)
                })
                .collect();
            assert_eq!(got, reference, "{name}");
            assert_eq!(&written[..4], b"RIFF");
            assert_eq!(&written[8..16], b"WAVEfmt ");
        }
    }

    #[test]
    fn tone_has_expected_length_and_peak() {
        let out = tmp("tone.wav");
        assert!(tone(&out, 44_100, 16, false, 0.5, 1000.0).is_ok());
        let w = std::fs::read(&out).unwrap_or_default();
        let _ = std::fs::remove_file(&out);
        assert_eq!(w.len(), 68 + 22_050 * 4);
        let peak = w[68..]
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]).unsigned_abs())
            .max()
            .unwrap_or(0);
        assert!((8000..=8192).contains(&peak), "{peak}");
    }

    /// WAV 的 data 块内容。
    fn wav_data(w: &[u8]) -> &[u8] {
        let mut i = 12;
        while i + 8 <= w.len() {
            let len = u32::from_le_bytes([w[i + 4], w[i + 5], w[i + 6], w[i + 7]]) as usize;
            if &w[i..i + 4] == b"data" {
                return &w[i + 8..(i + 8 + len).min(w.len())];
            }
            i += 8 + len + (len & 1);
        }
        &[]
    }

    /// 浮点测试音：样本等于按定义独立计算的 sin × 0.25（binary32 由 f64 舍入），用自有解码器读回
    /// 按位相同（decode 原样写出同一浮点格式）。
    #[test]
    fn float_tone_round_trips_bitwise() {
        for (bits, width) in [(32u8, 4usize), (64, 8)] {
            let out = tmp(&format!("tone-f{bits}.wav"));
            let back = tmp(&format!("tone-f{bits}-back.wav"));
            assert!(tone(&out, 48_000, bits, true, 0.1, 997.0).is_ok());
            let w = std::fs::read(&out).unwrap_or_default();
            let data = wav_data(&w).to_vec();
            assert_eq!(data.len(), 4_800 * 2 * width);
            for (i, frame) in data.chunks_exact(2 * width).enumerate() {
                let x = (std::f64::consts::TAU * 997.0 * i as f64 / 48_000.0).sin() * 0.25;
                let want = if bits == 32 {
                    (x as f32).to_le_bytes().to_vec()
                } else {
                    x.to_le_bytes().to_vec()
                };
                assert_eq!(&frame[..width], &want[..], "f{bits} 帧 {i}");
                assert_eq!(&frame[width..], &want[..], "f{bits} 帧 {i}");
            }
            let r = decode(&out, &back);
            let b = std::fs::read(&back).unwrap_or_default();
            let _ = (std::fs::remove_file(&out), std::fs::remove_file(&back));
            assert!(r.is_ok(), "{r:?}");
            assert!(wav_data(&b) == &data[..], "f{bits} 读回");
        }
        assert!(tone(&tmp("x.aiff"), 48_000, 32, true, 0.1, 997.0).is_err());
    }

    /// AIFF 测试音：COMM 的 80 位采样率与已知编码相同（44 100 Hz = 0x400E AC44 0000 0000 0000），
    /// 用自有解码器读回的样本与同参数的 WAV 测试音相同（大端 → 小端，样本值不变）。
    #[test]
    fn aiff_tone_matches_wav_tone() {
        assert_eq!(extended(44_100), [0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0]);
        assert_eq!(extended(48_000), [0x40, 0x0E, 0xBB, 0x80, 0, 0, 0, 0, 0, 0]);
        for bits in [16u8, 24, 32] {
            let a = tmp(&format!("tone{bits}.aiff"));
            let wv = tmp(&format!("tone{bits}.wav"));
            let back = tmp(&format!("tone{bits}-back.wav"));
            assert!(tone(&a, 44_100, bits, false, 0.1, 1000.0).is_ok());
            assert!(tone(&wv, 44_100, bits, false, 0.1, 1000.0).is_ok());
            let r = decode(&a, &back);
            let (b, w) = (
                std::fs::read(&back).unwrap_or_default(),
                std::fs::read(&wv).unwrap_or_default(),
            );
            let _ = (
                std::fs::remove_file(&a),
                std::fs::remove_file(&wv),
                std::fs::remove_file(&back),
            );
            assert!(r.is_ok(), "{r:?}");
            assert!(!wav_data(&w).is_empty());
            assert!(wav_data(&b) == wav_data(&w), "{bits} 位 AIFF 读回");
        }
    }
}
