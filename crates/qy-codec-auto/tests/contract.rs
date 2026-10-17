//! X1：按文件头选择格式的 `AutoDecoder` 也满足解码器契约。用 WAV、FLAC、AIFF 三个解码器的全部
//! 夹具（含被拒绝的），依次由同一个解码器解码，检查分块无关、状态语义与复位复用；并检查对每种
//! 格式的定位。
#![cfg(all(feature = "wav", feature = "flac", feature = "aiff"))]

use std::path::PathBuf;

use qy_codec_auto::AutoDecoder;
use qy_codec_flac::Capacity;
use qy_testkit::{SeekSupport, check, check_seek};

fn leak<T: Default + Clone>(n: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); n].into_boxed_slice())
}

fn make() -> AutoDecoder {
    let cap = Capacity::DESKTOP_MULTICHANNEL;
    let Ok(d) = AutoDecoder::new(
        leak(qy_core::PCM_WORK_SAMPLES),
        cap,
        leak(cap.workspace_len()),
        leak(cap.pcm_len()),
    ) else {
        panic!("工作区");
    };
    d
}

fn fixtures(codec: &str, ext: &str) -> Vec<(String, Vec<u8>)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../{codec}/tests/fixtures"));
    let Ok(rd) = std::fs::read_dir(&dir) else {
        panic!("夹具目录 {}", dir.display());
    };
    let mut v: Vec<(String, Vec<u8>)> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == ext))
        .map(|p| {
            let name = p
                .file_name()
                .map_or(String::new(), |n| n.to_string_lossy().into());
            (name, std::fs::read(&p).unwrap_or_default())
        })
        .collect();
    v.sort();
    v
}

#[test]
fn auto_decoder_satisfies_the_contract_over_all_formats() {
    let mut files = fixtures("qy-codec-wav", "wav");
    files.extend(fixtures("qy-codec-flac", "flac"));
    files.extend(fixtures("qy-codec-aiff", "aiff"));
    assert!(files.len() > 150, "{}", files.len());
    let all: Vec<&[u8]> = files.iter().map(|(_, b)| b.as_slice()).collect();
    if let Err(v) = check(make, &all) {
        panic!("{v}");
    }
    for (codec, ext, name) in [
        ("qy-codec-wav", "wav", "pcm16_stereo_44k.wav"),
        ("qy-codec-flac", "flac", "cli_16_stereo_44k_default.flac"),
        ("qy-codec-aiff", "aiff", "aifc_none_16_stereo.aiff"),
    ] {
        let list = fixtures(codec, ext);
        let Some((_, bytes)) = list.iter().find(|(n, _)| n == name) else {
            panic!("缺少 {name}");
        };
        let Ok(full) = check(make, &[bytes.as_slice()]) else {
            panic!("{name}");
        };
        let total = full[0].frames() as u64;
        match check_seek(
            make,
            bytes,
            &[0, 3, total / 2, total.saturating_sub(1), total],
        ) {
            Ok(s) => assert_eq!(s, SeekSupport::Supported, "{name}"),
            Err(v) => panic!("{name}：{v}"),
        }
    }
}
