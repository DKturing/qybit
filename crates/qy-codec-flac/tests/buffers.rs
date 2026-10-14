//! M1 T1：容量配置、结构体大小与容量不足错误。零分配复用见 `no_realloc.rs`。

use qy_codec_flac::{Buffer, Capacity, FlacDecoder, InitError};
use qy_codec_wav::WavDecoder;
use qy_core::{Core, CoreError, PCM_WORK_SAMPLES};

fn leak<T: Copy + Default>(n: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); n].into_boxed_slice())
}

#[test]
fn desktop_capacity_matches_task_card() {
    let c = Capacity::DESKTOP;
    assert_eq!(c.input_window, 1_048_576);
    assert_eq!(c.workspace_len(), 2 * 65_535);
    assert_eq!(c.workspace_len() * 8, 1_048_560);
    assert_eq!(c.pcm_len() * 4, 524_280);
    // M3 T1：MCU 容量由公式得出（覆盖 24 位，原手写 32 KiB），见 `verbatim_bound_formula`。
    let m = Capacity::MCU;
    assert_eq!(
        (m.max_block, m.max_channels, m.input_window),
        (4_608, 2, 28_244)
    );
    assert_eq!(m.ram_bytes(), 28_244 + 4_608 * 2 * 8 + 4_608 * 2 * 4);
    // F1：桌面多声道容量，8 声道不压缩帧上界独立计算：
    // 16 + ⌈(8 × 8 + 65535 × 8 × 32) / 8⌉ + 2 = 16 + 2 097 128 + 2。
    let d = Capacity::DESKTOP_MULTICHANNEL;
    assert_eq!((d.max_block, d.max_channels), (65_535, 8));
    assert_eq!(d.input_window, 2_097_146);
    assert_eq!(d.workspace_len() * 8, 65_535 * 8 * 8);
}

/// 不压缩帧上界：独立按 RFC 9639 逐项相加核对，并与最大块 verbatim 夹具的实际帧长比较。
#[test]
fn verbatim_bound_formula() {
    use qy_codec_flac::verbatim_frame_bound;
    // 帧头上限 16 字节 + 各声道（8 位子帧头 + 块 × 位宽，side 声道 +1 位）+ CRC-16。
    let independent = |block: usize, ch: usize, bits: usize| {
        let mut sub = 0usize;
        for c in 0..ch {
            let w = bits + usize::from(ch == 2 && c == 1);
            sub += 8 + block * w;
        }
        16 + sub.div_ceil(8) + 2
    };
    for (b, c, n) in [
        (4_608, 2, 24),
        (4_608, 2, 32),
        (4_608, 1, 16),
        (65_535, 2, 32),
        (16_384, 2, 24),
        (1, 1, 4),
    ] {
        assert_eq!(
            verbatim_frame_bound(b, c, n),
            independent(b, c, n),
            "{b}/{c}/{n}"
        );
    }
    assert_eq!(verbatim_frame_bound(4_608, 2, 24), 28_244);
    assert_eq!(verbatim_frame_bound(4_608, 2, 32), 37_460);
    assert_eq!(verbatim_frame_bound(65_535, 2, 32), 532_492);
    // 最大块 verbatim 夹具（65535 × 2 × 32 位，独立声道）：实际帧长不超过上界。
    let data = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/max_block_verbatim_32_stereo.flac"),
    )
    .unwrap_or_default();
    // fLaC(4) + STREAMINFO 块（4 + 34）之后即为唯一一帧。
    let frame = data.len() - (4 + 4 + 34);
    assert!(frame <= verbatim_frame_bound(65_535, 2, 32), "{frame}");
    // 上界不应过松：夹具为独立声道（无 side 声道的 +1 位，约 65535 / 8 字节），帧头也短于
    // 最大 16 字节；扣除这两项后实际帧长应达到上界。
    let slack = 65_535usize.div_ceil(8) + qy_codec_flac::MAX_FRAME_HEADER_BYTES;
    assert!(
        frame + slack >= verbatim_frame_bound(65_535, 2, 32),
        "上界过松：帧 {frame}"
    );
    assert_eq!(
        Capacity::for_limits(65_535, 2, 32)
            .with_min_window(1 << 20)
            .input_window,
        1 << 20
    );
}

/// 用 `cap` 解码整个文件（每次给出全部剩余输入，EOF），返回输出帧数或错误明细。
fn decode_all(data: &[u8], cap: Capacity) -> Result<usize, qy_codec_flac::FlacError> {
    use qy_core::{Decoder, Status};
    let Ok(mut d) = FlacDecoder::new(cap, leak(cap.workspace_len()), leak(cap.pcm_len())) else {
        panic!("init");
    };
    let (mut pos, mut frames) = (0, 0);
    for _ in 0..16 {
        match d.decode(&data[pos..], true) {
            Ok(p) => {
                pos += p.consumed;
                match p.status {
                    Status::Frames(n) => {
                        frames += n;
                        d.release(n);
                    }
                    Status::End => return Ok(frames),
                    Status::NeedInput | Status::NeedOutput => {}
                }
            }
            Err(_) => {
                return Err(d
                    .last_error()
                    .unwrap_or(qy_codec_flac::FlacError::Unsupported));
            }
        }
    }
    panic!("未结束");
}

/// 输入窗口正好等于最大块不压缩帧的长度时解码成功，少 1 字节时返回容量错误：
/// STREAMINFO 声明了最大帧长时在文件头即拒绝；声明为 0（未知）时在该帧处拒绝。
#[test]
fn window_exactly_fits_max_block_verbatim_frame() {
    use qy_codec_flac::FlacError;
    let data = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/max_block_verbatim_32_stereo.flac"),
    )
    .unwrap_or_default();
    let frame = data.len() - (4 + 4 + 34);
    let cap = |window| Capacity {
        max_block: 65_535,
        max_channels: 2,
        input_window: window,
    };
    assert_eq!(decode_all(&data, cap(frame)), Ok(65_535));
    assert_eq!(
        decode_all(&data, cap(frame - 1)),
        Err(FlacError::FrameTooLarge)
    );
    // STREAMINFO 最大帧长（偏移 4 + 4 + 7，24 位）置 0：只能在帧处发现超出窗口。
    let mut unknown = data.clone();
    unknown[15..18].fill(0);
    assert_eq!(decode_all(&unknown, cap(frame)), Ok(65_535));
    assert_eq!(
        decode_all(&unknown, cap(frame - 1)),
        Err(FlacError::FrameTooLarge)
    );
}

#[test]
fn structs_hold_no_block_sized_storage() {
    // 结构体大小与容量无关：只含切片与少量状态。
    // 上限依据：T1 的 128 字节是经验值；T2 加入帧解析状态与 STREAMINFO 字段（另有
    // test-metrics 统计 40 字节）后为 184 字节。最小的块缓冲（MCU：4608 × 2 × 8 字节
    // = 73 728 字节）远大于上限，故上限以内足以证明没有内嵌块大小的存储。
    // W1：WavDecoder 由 128 增至 176 字节（RIFF 长度用法与声明值、源的绝对偏移、data 占位读取量、
    // RIFF 实际长度、丢弃的尾部字节数与 data 状态的尾部/声明长度字段，都是标量），
    // Core<WavDecoder> 由 232 增至 280 字节，Core 的上限相应由 256 调为 320；FlacDecoder 不变。
    let d = std::mem::size_of::<FlacDecoder>();
    let c = std::mem::size_of::<Core<WavDecoder>>();
    assert!(d <= 256, "FlacDecoder {d} 字节");
    assert!(c <= 320, "Core {c} 字节");
}

#[test]
fn capacity_errors_are_explicit() {
    let c = Capacity::DESKTOP;
    let r = FlacDecoder::new(c, leak(c.workspace_len() - 1), leak(c.pcm_len()));
    assert_eq!(
        r.err(),
        Some(InitError::CapacityExceeded {
            buffer: Buffer::Workspace,
            required: 131_070,
            provided: 131_069
        })
    );
    let r = FlacDecoder::new(c, leak(c.workspace_len()), leak(c.pcm_len() - 2));
    assert_eq!(
        r.err(),
        Some(InitError::CapacityExceeded {
            buffer: Buffer::Pcm,
            required: 131_070,
            provided: 131_068
        })
    );
    let Ok(wav) = WavDecoder::new(leak(PCM_WORK_SAMPLES)) else {
        panic!("wav");
    };
    let r = Core::with_window(leak(c.input_window - 1), c.input_window, wav);
    assert!(matches!(
        r,
        Err(CoreError::CapacityExceeded {
            required: 1_048_576,
            provided: 1_048_575
        })
    ));
}

/// F1：声道数超出宿主容量（双声道容量上的 6 声道流）在 STREAMINFO 处报容量不足，
/// 不是“不支持的格式”；同一个流在 8 声道容量下正常解码。
#[test]
fn channels_beyond_capacity_are_capacity_errors() {
    use qy_codec_flac::FlacError;
    let data = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/cli_24_6ch_96k_51.flac"),
    )
    .unwrap_or_default();
    assert_eq!(
        decode_all(&data, Capacity::DESKTOP),
        Err(FlacError::TooManyChannels)
    );
    assert_eq!(decode_all(&data, Capacity::DESKTOP_MULTICHANNEL), Ok(6_000));
}
