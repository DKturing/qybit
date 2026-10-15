//! M2 T5：cargo-mutants 发现的 FLAC 元数据与流级测试缺口（每个用例注明它区分的边界）。
//!
//! 文件由真实夹具的音频帧加上本测试构造的元数据组成；期望按 RFC 9639 与本解码器文档写出。

use std::path::Path;

use qy_codec_flac::{Capacity, FlacDecoder, FlacError, MAX_FRAME_HEADER_BYTES};
use qy_core::engine::{CapabilityError, Resources};
use qy_core::{DecodeError, Decoder, Status};

fn leak<T: Copy + Default>(n: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); n].into_boxed_slice())
}

fn fixture(name: &str) -> Vec<u8> {
    match std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    ) {
        Ok(b) => b,
        Err(e) => panic!("{name}: {e}"),
    }
}

fn dec(cap: Capacity) -> FlacDecoder {
    match FlacDecoder::new(cap, leak(cap.workspace_len()), leak(cap.pcm_len())) {
        Ok(d) => d,
        Err(e) => panic!("{e:?}"),
    }
}

/// 夹具的 STREAMINFO（34 字节）与音频帧。
fn split(data: &[u8]) -> ([u8; 34], Vec<u8>) {
    let mut info = [0u8; 34];
    info.copy_from_slice(&data[8..42]);
    let mut i = 4;
    loop {
        let last = data[i] & 0x80 != 0;
        let len = u32::from_be_bytes([0, data[i + 1], data[i + 2], data[i + 3]]) as usize;
        i += 4 + len;
        if last {
            return (info, data[i..].to_vec());
        }
    }
}

/// `fLaC` + 元数据块（类型, 内容；最后一块自动置末块标志）+ 帧。
fn build(blocks: &[(u8, Vec<u8>)], frames: &[u8]) -> Vec<u8> {
    let mut f = b"fLaC".to_vec();
    for (k, (ty, body)) in blocks.iter().enumerate() {
        let last = if k + 1 == blocks.len() { 0x80 } else { 0 };
        f.push(last | ty);
        f.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        f.extend_from_slice(body);
    }
    f.extend_from_slice(frames);
    f
}

/// 一次喂入全部数据（EOF）。
fn run(d: &mut FlacDecoder, data: &[u8]) -> (Result<(), DecodeError>, usize) {
    let mut pos = 0;
    let mut n = 0;
    loop {
        match d.decode(&data[pos..], true) {
            Err(e) => return (Err(e), n),
            Ok(p) => {
                pos += p.consumed;
                match p.status {
                    Status::End => return (Ok(()), n),
                    Status::NeedInput => panic!("EOF 后 NeedInput"),
                    _ => {
                        n += d.pcm().len();
                        let ch = d.format().map_or(1, |f| usize::from(f.channels()));
                        d.release(d.pcm().len() / ch);
                    }
                }
            }
        }
    }
}

fn result(blocks: &[(u8, Vec<u8>)], frames: &[u8]) -> (Result<(), DecodeError>, Option<FlacError>) {
    let mut d = dec(Capacity::DESKTOP);
    let r = run(&mut d, &build(blocks, frames)).0;
    (r, d.last_error())
}

const BASE: &str = "cli_16_stereo_44k_default.flac";

/// STREAMINFO 字段写入（§8.2）。
fn set_bits(info: &mut [u8; 34], bits: u32) {
    let v = bits - 1;
    info[12] = (info[12] & 0xFE) | ((v >> 4) as u8 & 1);
    info[13] = (info[13] & 0x0F) | (((v & 0xF) as u8) << 4);
}
fn set_rate(info: &mut [u8; 34], rate: u32) {
    info[10] = (rate >> 12) as u8;
    info[11] = (rate >> 4) as u8;
    info[12] = (info[12] & 0x0F) | (((rate & 0xF) as u8) << 4);
}
fn set_total(info: &mut [u8; 34], total: u64) {
    info[13] = (info[13] & 0xF0) | ((total >> 32) as u8 & 0x0F);
    info[14..18].copy_from_slice(&(total as u32).to_be_bytes());
}
fn total_of(info: &[u8; 34]) -> u64 {
    (u64::from(info[13] & 0x0F) << 32)
        | u64::from(u32::from_be_bytes([info[14], info[15], info[16], info[17]]))
}

/// 只有 STREAMINFO、总样本数为 0 的流（无音频帧）：合法的空流，正常结束。STREAMINFO 的边界：
/// 位深 4 合法、3 不合法；最大块 16 合法、15 不合法；采样率 0 不合法。
#[test]
fn streaminfo_field_boundaries() {
    let (mut info, _) = split(&fixture(BASE));
    set_total(&mut info, 0);
    let ok = |i: [u8; 34]| result(&[(0, i.to_vec())], &[]);
    assert_eq!(ok(info).0, Ok(()), "空流");
    let mut i = info;
    set_bits(&mut i, 4);
    assert_eq!(ok(i).0, Ok(()), "位深 4");
    set_bits(&mut i, 3);
    assert_eq!(ok(i).1, Some(FlacError::BadStreamInfo), "位深 3");
    let mut i = info;
    i[2..4].copy_from_slice(&16u16.to_be_bytes());
    assert_eq!(ok(i).0, Ok(()), "最大块 16");
    i[2..4].copy_from_slice(&15u16.to_be_bytes());
    assert_eq!(ok(i).1, Some(FlacError::BadStreamInfo), "最大块 15");
    let mut i = info;
    set_rate(&mut i, 0);
    assert_eq!(ok(i).1, Some(FlacError::BadStreamInfo), "采样率 0");
}

/// STREAMINFO 声明的最大帧长度超过输入窗口：按容量不足拒绝并给出原因；为 0（未知）时放行。
#[test]
fn declared_max_frame_beyond_window() {
    let (mut info, frames) = split(&fixture(BASE));
    let window = Capacity::DESKTOP.input_window as u32;
    let mut d = dec(Capacity::DESKTOP);
    info[7..10].copy_from_slice(&(window + 1).to_be_bytes()[1..]);
    assert_eq!(
        run(&mut d, &build(&[(0, info.to_vec())], &frames)).0,
        Err(DecodeError::CapacityExceeded)
    );
    assert_eq!(d.last_error(), Some(FlacError::FrameTooLarge));
    assert_eq!(
        d.capacity_rejection(),
        Some(CapabilityError::FlacFrame {
            bytes: window + 1,
            window
        })
    );
    info[7..10].copy_from_slice(&[0, 0, 0]);
    let mut d = dec(Capacity::DESKTOP);
    assert_eq!(
        run(&mut d, &build(&[(0, info.to_vec())], &frames)).0,
        Ok(())
    );
    assert_eq!(d.capacity_rejection(), None);
}

/// 首个元数据块必须是长度 34 的 STREAMINFO（§8.1、§8.2）；之后的末块可以是长度 0 的 PADDING，
/// 文件在它的块头之后恰好结束也是合法的空流。
#[test]
fn metadata_block_order_and_lengths() {
    let (mut info, _) = split(&fixture(BASE));
    set_total(&mut info, 0);
    assert_eq!(
        result(&[(0, info[..33].to_vec())], &[]).1,
        Some(FlacError::BadStreamInfo)
    );
    assert_eq!(
        result(&[(1, vec![0; 8]), (0, info.to_vec())], &[]).1,
        Some(FlacError::BadStreamInfo)
    );
    assert_eq!(
        result(&[(0, info.to_vec()), (1, Vec::new())], &[]).0,
        Ok(())
    );
}

/// 结束时核对总样本数（§8.2）：解出的样本多于声明为不一致；声明的高 4 位非零（> 2^32）时
/// 实际样本少于声明，为截断。
#[test]
fn total_samples_checked_at_end() {
    let (mut info, frames) = split(&fixture(BASE));
    let n = total_of(&info);
    assert!(n > 0);
    set_total(&mut info, n - 1);
    assert_eq!(
        result(&[(0, info.to_vec())], &frames).1,
        Some(FlacError::TotalSamplesMismatch)
    );
    set_total(&mut info, n + (1 << 32));
    assert_eq!(
        result(&[(0, info.to_vec())], &frames).1,
        Some(FlacError::Truncated)
    );
}

/// VORBIS_COMMENT（§8.6）：厂商串 + 字段数 + 各字段（小端长度 + 内容），块内其余字节为尾部。
fn comment(fields: &[&[u8]], tail: usize) -> Vec<u8> {
    let mut c = 6u32.to_le_bytes().to_vec();
    c.extend_from_slice(b"vendor");
    c.extend_from_slice(&(fields.len() as u32).to_le_bytes());
    for f in fields {
        c.extend_from_slice(&(f.len() as u32).to_le_bytes());
        c.extend_from_slice(f);
    }
    c.extend(std::iter::repeat_n(0xAA, tail));
    c
}

/// 带注释块解码，返回结果与声道掩码。
fn with_comment(fields: &[&[u8]], tail: usize) -> (Result<(), FlacError>, Option<u32>) {
    let (info, frames) = split(&fixture(BASE));
    let data = build(&[(0, info.to_vec()), (4, comment(fields, tail))], &frames);
    let mut d = dec(Capacity::DESKTOP);
    let r = run(&mut d, &data).0;
    let mask = d.format().map(|f| f.layout().mask());
    (
        r.map_err(|_| d.last_error().unwrap_or(FlacError::Unsupported)),
        mask,
    )
}

/// 声道掩码标签（WAVEFORMATEXTENSIBLE_CHANNEL_MASK）的识别与边界：
/// - 字段长度超过 64 字节时不解析（按普通字段跳过），即使内容以标签开头；
/// - 跳过的长字段在前、标签字段在后：标签仍被识别；长字段是最后一个：注释在它之后结束；
/// - 块内字段之后的尾部字节跳过；
/// - 十六进制 1–8 位合法，0 位或 9 位不合法；只有标签没有值不合法。
#[test]
fn channel_mask_tag_in_comments() {
    let tag = b"WAVEFORMATEXTENSIBLE_CHANNEL_MASK=";
    let field = |v: &str| [&tag[..], v.as_bytes()].concat();
    let long: Vec<u8> = vec![b'A'; 100];
    let mut long_tag = field("0x");
    long_tag.extend(std::iter::repeat_n(b'F', 70));
    // 标准立体声为 0x3；用 0x5（FL FC）检验标签是否生效。
    assert_eq!(with_comment(&[&field("0x5")], 0), (Ok(()), Some(0x5)));
    assert_eq!(
        with_comment(&[&long_tag], 0),
        (Ok(()), Some(0x3)),
        "超长字段不解析"
    );
    assert_eq!(
        with_comment(&[&long, &field("0x5")], 0),
        (Ok(()), Some(0x5)),
        "长字段在前"
    );
    assert_eq!(
        with_comment(&[&field("0x5"), &long], 0),
        (Ok(()), Some(0x5)),
        "长字段在后"
    );
    assert_eq!(
        with_comment(&[&field("0x5")], 9),
        (Ok(()), Some(0x5)),
        "尾部字节"
    );
    assert_eq!(
        with_comment(&[&field("0x00000005")], 0),
        (Ok(()), Some(0x5))
    );
    for bad in ["0x000000005", "0x", ""] {
        assert_eq!(
            with_comment(&[&field(bad)], 0).0,
            Err(FlacError::BadChannelMask),
            "{bad:?}"
        );
    }
}

/// 窗口恰好被填满、源已结束而帧仍不完整：是截断（不是“帧超出窗口”——窗口之外没有数据）。
#[test]
fn window_full_at_eof_is_truncation() {
    let data = fixture("unknown_max_frame.flac");
    let (info, frames) = split(&data);
    let small = Capacity {
        input_window: 1000,
        ..Capacity::DESKTOP
    };
    assert!(frames.len() > 1000);
    let mut d = dec(small);
    let cut = build(&[(0, info.to_vec())], &frames[..1000]);
    assert_eq!(run(&mut d, &cut).0, Err(DecodeError::Truncated));
    assert_eq!(d.last_error(), Some(FlacError::Truncated));
}

/// 常量：帧头最长 16 字节（同步与标志 2、块大小与采样率 1、声道与位深 1、编号 7、块大小 2、
/// 采样率 2、CRC-8 1）；多声道桌面配置的输入窗口不小于 1 MiB。解码器需求（资源计划用）在
/// STREAMINFO 之前未知，之后为声明值与本机容量。
#[test]
fn constants_and_demand() {
    assert_eq!(MAX_FRAME_HEADER_BYTES, 16);
    // 运行时取值（编译期断言会让变异测试把错误判为“不可编译”而不是“被测试发现”）。
    let multi = std::hint::black_box(Capacity::DESKTOP_MULTICHANNEL);
    assert!(multi.input_window >= 1 << 20);
    let data = fixture(BASE);
    let (info, _) = split(&data);
    let mut d = dec(Capacity::DESKTOP);
    assert!(d.demand().is_none());
    assert_eq!(run(&mut d, &data).0, Ok(()));
    let Some(dm) = d.demand() else {
        panic!("STREAMINFO 之后应有需求")
    };
    assert_eq!(
        dm.max_block,
        u32::from(u16::from_be_bytes([info[2], info[3]]))
    );
    assert_eq!(
        dm.max_frame,
        u32::from_be_bytes([0, info[7], info[8], info[9]])
    );
    assert_eq!(dm.block_capacity, Capacity::DESKTOP.max_block as u32);
    assert_eq!(dm.window, Capacity::DESKTOP.input_window as u32);
}
