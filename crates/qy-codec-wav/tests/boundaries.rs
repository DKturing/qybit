//! M2 T5：cargo-mutants 发现的 WAV 解码器测试缺口（每个用例注明它区分的边界）。
//!
//! 文件由本测试内的小型写入器构造，期望按规则独立写出：可播放的文件逐样本比较；拒绝的文件比较
//! 错误类型与明细。

use qy_codec_wav::{Bitstream, WavDecoder, WavDetail};
use qy_core::{DecodeError, Decoder, PCM_WORK_SAMPLES, Status};

fn decoder() -> WavDecoder {
    let pcm = Box::leak(vec![0i32; PCM_WORK_SAMPLES].into_boxed_slice());
    match WavDecoder::new(pcm) {
        Ok(d) => d,
        Err(e) => panic!("{e:?}"),
    }
}

/// 一次喂入全部字节（EOF），取走全部输出。
fn decode(data: &[u8]) -> (Result<(), DecodeError>, Vec<i32>, Option<WavDetail>) {
    let mut d = decoder();
    let mut pos = 0;
    let mut out = Vec::new();
    for _ in 0..10_000 {
        match d.decode(&data[pos..], true) {
            Err(e) => return (Err(e), out, d.last_detail()),
            Ok(p) => {
                pos += p.consumed;
                match p.status {
                    Status::Frames(_) | Status::NeedOutput => {
                        let ch = d.format().map_or(1, |f| usize::from(f.channels()));
                        let k = d.pcm().len() / ch;
                        out.extend_from_slice(d.pcm());
                        d.release(k);
                    }
                    Status::End => return (Ok(()), out, d.last_detail()),
                    Status::NeedInput => panic!("EOF 后不得返回 NeedInput"),
                }
            }
        }
    }
    panic!("解码未结束")
}

fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut c = id.to_vec();
    c.extend_from_slice(&(body.len() as u32).to_le_bytes());
    c.extend_from_slice(body);
    if body.len() % 2 == 1 {
        c.push(0);
    }
    c
}

fn riff(len: Option<u32>, chunks: &[Vec<u8>]) -> Vec<u8> {
    let body: Vec<u8> = chunks.concat();
    let mut r = b"RIFF".to_vec();
    r.extend_from_slice(&len.unwrap_or(4 + body.len() as u32).to_le_bytes());
    r.extend_from_slice(b"WAVE");
    r.extend_from_slice(&body);
    r
}

/// 格式码 1 的 fmt 体（16 字节），字段可单独改写。
fn fmt_pcm(channels: u16, rate: u32, block_align: u16, bits: u16) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&channels.to_le_bytes());
    f.extend_from_slice(&rate.to_le_bytes());
    f.extend_from_slice(&(rate * u32::from(block_align)).to_le_bytes());
    f.extend_from_slice(&block_align.to_le_bytes());
    f.extend_from_slice(&bits.to_le_bytes());
    f
}

const SUBTYPE_PCM: [u8; 16] = [
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

/// EXTENSIBLE 的 fmt 体（40 字节）。
fn fmt_ext(channels: u16, container: u16, valid: u16, cb_size: u16) -> Vec<u8> {
    let ba = channels * container / 8;
    let mut f = fmt_pcm(channels, 48_000, ba, container);
    f[0..2].copy_from_slice(&0xFFFEu16.to_le_bytes());
    f.extend_from_slice(&cb_size.to_le_bytes());
    f.extend_from_slice(&valid.to_le_bytes());
    let mask: u32 = if channels == 1 { 0x4 } else { 0x3 };
    f.extend_from_slice(&mask.to_le_bytes());
    f.extend_from_slice(&SUBTYPE_PCM);
    f
}

/// 16 位双声道 48 kHz 文件，data 为 `head` 后补 0 到 `BITSTREAM_SCAN` 字节。
fn stereo16(head: &[u8]) -> Vec<u8> {
    let mut data = head.to_vec();
    data.resize(qy_codec_wav::BITSTREAM_SCAN, 0);
    riff(
        None,
        &[
            chunk(b"fmt ", &fmt_pcm(2, 48_000, 4, 16)),
            chunk(b"data", &data),
        ],
    )
}

/// 小端 16 位样本。
fn le16(data: &[u8]) -> Vec<i32> {
    data.chunks_exact(2)
        .map(|c| i32::from(i16::from_le_bytes([c[0], c[1]])))
        .collect()
}

fn detected(file: &[u8]) -> Option<Bitstream> {
    match decode(file) {
        (Err(DecodeError::Unsupported), _, Some(WavDetail::Bitstream { kind, offset: 0 })) => {
            Some(kind)
        }
        (Ok(()), samples, _) => {
            // 未识别为码流时按 PCM 播放：样本就是 data 的小端 16 位值。
            let data = &file[44..];
            assert_eq!(samples, le16(data));
            None
        }
        other => panic!("{:?}", (other.0, other.2)),
    }
}

/// 14 位 DTS：同步模式第 5 字节（小端）/第 6 字节（大端）的高 4 位必须全为 1。
#[test]
fn dts14_sync_nibble_is_checked() {
    assert_eq!(
        detected(&stereo16(&[0xFF, 0x1F, 0x00, 0xE8, 0xF7, 0x07])),
        Some(Bitstream::Dts14)
    );
    assert_eq!(
        detected(&stereo16(&[0xFF, 0x1F, 0x00, 0xE8, 0x07, 0x07])),
        None
    );
    assert_eq!(
        detected(&stereo16(&[0x1F, 0xFF, 0xE8, 0x00, 0x07, 0xF7])),
        Some(Bitstream::Dts14)
    );
    assert_eq!(
        detected(&stereo16(&[0x1F, 0xFF, 0xE8, 0x00, 0x07, 0x07])),
        None
    );
}

/// 16 位 DTS 帧头：48 位字段 FTYPE(1) SHORT(5) CPF(1) NBLKS(7) FSIZE(14) AMODE(6) SFREQ(4)。
fn dts16(
    be: bool,
    ftype: u64,
    nblks: u64,
    fsize: u64,
    sfreq: u64,
    next: Option<[u16; 2]>,
) -> Vec<u8> {
    let bits = (ftype << 47) | (31 << 42) | (nblks << 34) | (fsize << 20) | (sfreq << 10);
    let words = [
        0x7FFE,
        0x8001,
        (bits >> 32) as u16,
        (bits >> 16) as u16,
        bits as u16,
    ];
    let put = |w: u16| if be { w.to_be_bytes() } else { w.to_le_bytes() };
    let mut d: Vec<u8> = words.iter().flat_map(|&w| put(w)).collect();
    if let Some([a, c]) = next {
        // 下一帧从同步字起 FSIZE + 1 字节处开始。
        d.resize(fsize as usize + 1, 0);
        d.extend_from_slice(&put(a));
        d.extend_from_slice(&put(c));
    }
    d
}

#[test]
fn dts16_header_fields_and_next_frame() {
    const SYNC: [u16; 2] = [0x7FFE, 0x8001];
    for be in [false, true] {
        let ok = Some(Bitstream::Dts16);
        assert_eq!(
            detected(&stereo16(&dts16(be, 1, 5, 95, 1, Some(SYNC)))),
            ok,
            "边界值"
        );
        // 下一帧不在窗口内：只核对帧头。
        assert_eq!(detected(&stereo16(&dts16(be, 1, 5, 5000, 13, None))), ok);
        assert_eq!(
            detected(&stereo16(&dts16(be, 0, 5, 95, 1, Some(SYNC)))),
            None,
            "FTYPE"
        );
        assert_eq!(
            detected(&stereo16(&dts16(be, 1, 4, 95, 1, Some(SYNC)))),
            None,
            "NBLKS"
        );
        assert_eq!(
            detected(&stereo16(&dts16(be, 1, 5, 94, 1, Some(SYNC)))),
            None,
            "FSIZE"
        );
        assert_eq!(
            detected(&stereo16(&dts16(be, 1, 5, 95, 4, Some(SYNC)))),
            None,
            "SFREQ"
        );
        // 下一帧在窗口内：两个字都必须是同步字。
        assert_eq!(
            detected(&stereo16(&dts16(be, 1, 5, 95, 1, Some([0, 0])))),
            None,
            "下一帧"
        );
        assert_eq!(
            detected(&stereo16(&dts16(be, 1, 5, 95, 1, Some([0x7FFE, 0])))),
            None,
            "下一帧第二个字"
        );
    }
}

/// IEC 61937：Pa Pb 之后 Pc 低 5 位为已定义的数据类型，且 Pd（长度）不为 0。
#[test]
fn iec61937_burst_info_is_checked() {
    for be in [false, true] {
        let pre = if be {
            [0xF8, 0x72, 0x4E, 0x1F]
        } else {
            [0x72, 0xF8, 0x1F, 0x4E]
        };
        let w = |x: u16| if be { x.to_be_bytes() } else { x.to_le_bytes() };
        let burst = |pc: u16, pd: u16| {
            let mut d = pre.to_vec();
            d.extend_from_slice(&w(pc));
            d.extend_from_slice(&w(pd));
            d
        };
        assert_eq!(
            detected(&stereo16(&burst(1, 0x1800))),
            Some(Bitstream::Iec61937)
        );
        assert_eq!(detected(&stereo16(&burst(1, 0))), None, "Pd 为 0");
        assert_eq!(
            detected(&stereo16(&burst(2, 0x1800))),
            None,
            "未定义的数据类型"
        );
    }
}

/// 码流识别只针对 16 位双声道整容器 PCM：单声道、24 位、有效位少于容器的文件即使 data 开头
/// 恰好是同步模式，也按 PCM 播放。
#[test]
fn bitstream_scan_only_for_16bit_stereo() {
    let pattern = [0xFF, 0x1F, 0x00, 0xE8, 0xF7, 0x07];
    let mut data = pattern.to_vec();
    data.resize(qy_codec_wav::BITSTREAM_SCAN, 0);
    let mono = riff(
        None,
        &[
            chunk(b"fmt ", &fmt_pcm(1, 48_000, 2, 16)),
            chunk(b"data", &data),
        ],
    );
    let (r, s, _) = decode(&mono);
    assert_eq!((r, s), (Ok(()), le16(&data)));
    // 16 位容器、12 位有效（低 4 位为填充）：样本右移 4 位。
    let ext = riff(
        None,
        &[
            chunk(b"fmt ", &fmt_ext(2, 16, 12, 22)),
            chunk(b"data", &data),
        ],
    );
    let (r, s, _) = decode(&ext);
    let want: Vec<i32> = le16(&data).into_iter().map(|x| x >> 4).collect();
    assert_eq!((r, s), (Ok(()), want));
    // 24 位双声道。
    let d24 = riff(
        None,
        &[
            chunk(b"fmt ", &fmt_pcm(2, 48_000, 6, 24)),
            chunk(b"data", &data[..4092]),
        ],
    );
    let (r, _, d) = decode(&d24);
    assert_eq!((r, d), (Ok(()), None));
}

/// fmt 字段：声道、采样率、块对齐、位深任一为 0 都不合规（Malformed），而不是“不支持”。
#[test]
fn zero_fmt_fields_are_malformed() {
    let data = chunk(b"data", &[0; 8]);
    for (ch, rate, ba, bits) in [
        (0, 48_000, 4, 16),
        (2, 0, 4, 16),
        (2, 48_000, 0, 16),
        (2, 48_000, 4, 0),
    ] {
        let f = riff(
            None,
            &[chunk(b"fmt ", &fmt_pcm(ch, rate, ba, bits)), data.clone()],
        );
        assert_eq!(
            decode(&f).0,
            Err(DecodeError::Malformed),
            "{ch} {rate} {ba} {bits}"
        );
    }
}

/// EXTENSIBLE 的 cbSize 声明的扩展长度超出 fmt 块时不合规。
#[test]
fn extensible_cb_size_must_fit_fmt_chunk() {
    let data = chunk(b"data", &[1, 0, 2, 0, 3, 0, 4, 0]);
    let ok = riff(
        None,
        &[chunk(b"fmt ", &fmt_ext(2, 16, 16, 22)), data.clone()],
    );
    assert_eq!(decode(&ok).0, Ok(()));
    let big = riff(None, &[chunk(b"fmt ", &fmt_ext(2, 16, 16, 30)), data]);
    assert_eq!(decode(&big).0, Err(DecodeError::Malformed));
}

/// 奇数长度的 fmt 块（扩展字段后多 1 字节）：按长度跳过并跳过 1 字节填充，样本不受影响。
#[test]
fn odd_fmt_chunk_is_padded() {
    let samples = [1i16, -2, 3, -4];
    let raw: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    for mut f in [fmt_pcm(2, 48_000, 4, 16), fmt_ext(2, 16, 16, 22)] {
        f.push(0xAB);
        let file = riff(None, &[chunk(b"fmt ", &f), chunk(b"data", &raw)]);
        let (r, s, _) = decode(&file);
        assert_eq!((r, s), (Ok(()), vec![1, -2, 3, -4]), "fmt 长度 {}", f.len());
    }
}

/// RIFF 长度为占位值（W1 R2）时，data 之前的其他块仍按长度与奇偶填充跳过。
#[test]
fn chunks_skipped_with_placeholder_riff() {
    let raw: Vec<u8> = [5i16, -6].iter().flat_map(|s| s.to_le_bytes()).collect();
    for list in [vec![0u8; 4], vec![0u8; 3]] {
        let file = riff(
            Some(0xFFFF_FFFF),
            &[
                chunk(b"fmt ", &fmt_pcm(2, 48_000, 4, 16)),
                chunk(b"LIST", &list),
                chunk(b"data", &raw),
            ],
        );
        let (r, s, _) = decode(&file);
        assert_eq!((r, s), (Ok(()), vec![5, -6]), "LIST 长度 {}", list.len());
    }
}

/// RIFF 长度为占位值时，fmt 之后源结束：没有 data 为不合规；剩下不足一个块头的字节为截断；
/// 最后恰好是一个长度为 0 的 data 块头：RIFF 与 data 都可能是未回填的占位，意图不明，按 W1 的
/// 规则为不合规（不是截断：块头是完整的 8 字节）。
#[test]
fn placeholder_riff_end_cases() {
    let head = riff(
        Some(0xFFFF_FFFF),
        &[chunk(b"fmt ", &fmt_pcm(2, 48_000, 4, 16))],
    );
    assert_eq!(decode(&head).0, Err(DecodeError::Malformed));
    let mut partial = head.clone();
    partial.extend_from_slice(b"dat");
    assert_eq!(decode(&partial).0, Err(DecodeError::Truncated));
    let mut empty = head;
    empty.extend_from_slice(b"data\0\0\0\0");
    assert_eq!(decode(&empty).0, Err(DecodeError::Malformed));
}

/// 明细的说明文字（报告与实时输出使用）。
#[test]
fn detail_texts() {
    let t = |d: WavDetail| format!("{d}");
    assert!(
        t(WavDetail::UndefinedLayout {
            channels: 4,
            mask: None
        })
        .contains("4 声道")
    );
    assert!(
        t(WavDetail::UndefinedLayout {
            channels: 3,
            mask: Some(0x7)
        })
        .contains("0x7")
    );
    assert!(
        t(WavDetail::DataShort {
            declared: 100,
            missing: 7
        })
        .contains("短 7 字节")
    );
    let b = t(WavDetail::Bitstream {
        kind: Bitstream::Dts14,
        offset: 12,
    });
    assert!(b.contains("DTS") && b.contains("12"), "{b}");
    assert!(
        t(WavDetail::Bitstream {
            kind: Bitstream::Iec61937,
            offset: 0
        })
        .contains("IEC 61937")
    );
}
