//! M2 T5：cargo-mutants 发现的 `qy-codec-auto` 测试缺口：无法识别时的说明文字、配置档开关与
//! RAM 预算、经 `Resources` 接口（引擎取修正、需求与能力不足原因）转发到当前解码器、明细与访问器。

#![cfg(all(feature = "wav", feature = "flac", feature = "aiff"))]

use std::path::PathBuf;

use qy_codec_auto::profile::{DESKTOP, EMBEDDED_FULL, Profile};
use qy_codec_auto::{AutoDecoder, Codec, UnrecognizedHeader};
use qy_codec_flac::Capacity;
use qy_codec_wav::WavDetail;
use qy_core::engine::Resources;
use qy_core::{DecodeError, Decoder, PCM_WORK_SAMPLES, Status};

fn leak<T: Copy + Default>(n: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); n].into_boxed_slice())
}

fn auto() -> AutoDecoder {
    let c = Capacity::DESKTOP;
    match AutoDecoder::new(
        leak(PCM_WORK_SAMPLES),
        c,
        leak(c.workspace_len()),
        leak(c.pcm_len()),
    ) {
        Ok(d) => d,
        Err(e) => panic!("{e:?}"),
    }
}

fn read(rel: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// 一次喂入（EOF），取走全部输出。
fn run(d: &mut AutoDecoder, data: &[u8]) -> Result<(), DecodeError> {
    d.reset();
    let mut pos = 0;
    loop {
        let p = d.decode(&data[pos..], true)?;
        pos += p.consumed;
        match p.status {
            Status::End => return Ok(()),
            Status::NeedInput => panic!("EOF 后 NeedInput"),
            _ => {
                let ch = d.format().map_or(1, |f| usize::from(f.channels()));
                d.release(d.pcm().len() / ch);
            }
        }
    }
}

/// 常见的非本程序格式：说明文字写明推断的类型。
#[test]
fn unrecognized_header_texts() {
    let text = |m: &[u8; 4]| {
        format!(
            "{}",
            UnrecognizedHeader {
                magic: *m,
                skipped_tag: 0,
                form_type: None,
            }
        )
    };
    assert!(text(b"wvpk").contains("WavPack"));
    assert!(text(b"MAC ").contains("Monkey's Audio"));
    assert!(text(b"DSD ").contains("DSD"));
    assert!(text(b"FRM8").contains("DSD"));
    // MPEG 帧同步：0xFF 后高 3 位全 1；只有其中一个条件成立时不是。
    assert!(text(&[0xFF, 0xFB, 0x90, 0x00]).contains("MPEG"));
    assert!(!text(&[0xFF, 0x1B, 0x90, 0x00]).contains("MPEG"));
    assert!(!text(&[0xFE, 0xFB, 0x90, 0x00]).contains("MPEG"));
    let tagged = UnrecognizedHeader {
        magic: *b"xxxx",
        skipped_tag: 1,
        form_type: None,
    };
    assert!(format!("{tagged}").contains("跳过开头 1 字节的 ID3v2 标签"));
    let untagged = UnrecognizedHeader {
        skipped_tag: 0,
        ..tagged
    };
    assert!(!format!("{untagged}").contains("ID3v2 标签后"));
}

/// 配置档：WAV 与 AIFF 可分别停用；只启用其中一种时仍计入共用的流式工作区，两者都停用时不计。
#[test]
fn profile_switches_and_budget() {
    let base = EMBEDDED_FULL;
    assert!(base.wav_enabled() && base.aiff_enabled());
    let only_aiff = base.with_wav(false);
    assert!(!only_aiff.wav_enabled() && only_aiff.aiff_enabled());
    let only_wav = base.with_aiff(false);
    assert!(only_wav.wav_enabled() && !only_wav.aiff_enabled());
    let none: Profile = base.with_wav(false).with_aiff(false);
    assert!(!none.wav_enabled() && !none.aiff_enabled());
    let stream = |p: &Profile| p.budget().total;
    assert_eq!(stream(&only_aiff), stream(&base));
    assert_eq!(stream(&only_wav), stream(&base));
    assert_eq!(stream(&base) - stream(&none), PCM_WORK_SAMPLES * 4);
    assert_eq!(
        DESKTOP.flac_capacity(),
        Some(Capacity::DESKTOP_MULTICHANNEL)
    );
}

/// `Resources` 转发：WAV 的结构修正（W1）与 FLAC 的需求、能力不足原因都经自动识别解码器取得；
/// 明细与访问器对应当前曲目。
#[test]
fn resources_follow_current_track() {
    let mut d = auto();
    // WAV：ffmpeg 管道输出（RIFF 与 data 都是占位长度）有 2 条修正。
    let wav = read("../qy-codec-wav/tests/fixtures/fix_ffmpeg_pipe.wav");
    assert_eq!(run(&mut d, &wav), Ok(()));
    assert_eq!(d.codec(), Codec::Wav);
    assert_eq!(d.repair_count(), 2);
    assert!(d.repair(0).is_some() && d.repair(1).is_some() && d.repair(2).is_none());
    // WAV 码流拒绝的明细。
    let dts = read("../qy-codec-wav/tests/fixtures/unsup_dts14_le_cd.wav");
    assert_eq!(run(&mut d, &dts), Err(DecodeError::Unsupported));
    assert!(matches!(d.wav_detail(), Some(WavDetail::Bitstream { .. })));
    // FLAC：需求来自 STREAMINFO；块大小超出 MCU 容量时有能力不足原因。
    let flac = read("../qy-codec-flac/tests/fixtures/cli_16_stereo_44k_default.flac");
    assert_eq!(run(&mut d, &flac), Ok(()));
    assert_eq!(d.repair_count(), 0);
    assert!(d.demand().is_some());
    assert!(d.flac().is_some());
    assert_eq!(d.wav_detail(), None);
    let c = Capacity::MCU;
    let Ok(mut small) = AutoDecoder::new(
        leak(PCM_WORK_SAMPLES),
        c,
        leak(c.workspace_len()),
        leak(c.pcm_len()),
    ) else {
        panic!()
    };
    let big = read("../qy-codec-flac/tests/fixtures/max_block_verbatim_32_stereo.flac");
    assert_eq!(run(&mut small, &big), Err(DecodeError::CapacityExceeded));
    assert!(small.capacity_rejection().is_some());
}
