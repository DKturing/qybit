//! X1：用 `qy-testkit` 对全部 WAV 夹具检查解码器契约（分块无关、状态语义、格式不伪造、
//! 出错与复位、复用），并检查定位只输出目标帧及之后的样本。与参考样本的逐样本比对在
//! `fixtures.rs` 中。

use std::path::PathBuf;

use qy_testkit::{Ending, SeekSupport, check, check_seek};

fn make() -> qy_codec_wav::WavDecoder {
    let pcm = Box::leak(vec![0i32; qy_core::PCM_WORK_SAMPLES].into_boxed_slice());
    let Ok(d) = qy_codec_wav::WavDecoder::new(pcm) else {
        panic!("工作区");
    };
    d
}

fn fixtures() -> Vec<(String, Vec<u8>)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        panic!("夹具目录");
    };
    let mut v: Vec<(String, Vec<u8>)> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "wav"))
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
fn all_fixtures_satisfy_the_decoder_contract() {
    let files = fixtures();
    assert!(files.len() >= 40, "夹具数 {}", files.len());
    for (name, bytes) in &files {
        if let Err(v) = check(make, &[bytes.as_slice()]) {
            panic!("{name}：{v}");
        }
    }
    // 复用：同一个解码器依次解码全部夹具（中间复位）。
    let all: Vec<&[u8]> = files.iter().map(|(_, b)| b.as_slice()).collect();
    match check(make, &all) {
        Ok(r) => {
            let ok = r.iter().filter(|d| d.ending == Ending::End).count();
            assert!(
                ok > 0 && ok < r.len(),
                "应同时有可播放与被拒绝的夹具：{ok}/{}",
                r.len()
            );
        }
        Err(v) => panic!("{v}"),
    }
}

#[test]
fn seek_outputs_only_target_and_later() {
    let files = fixtures();
    for want in ["pcm16_stereo_44k.wav", "ext20in24_dirty_pad_bits.wav"] {
        let Some((_, bytes)) = files.iter().find(|(n, _)| n == want) else {
            panic!("缺少夹具 {want}");
        };
        let Ok(full) = check(make, &[bytes.as_slice()]) else {
            panic!("{want}");
        };
        let total = full[0].frames() as u64;
        let targets = [
            0,
            1,
            total / 3,
            total / 2 + 7,
            total.saturating_sub(1),
            total,
            total + 100,
        ];
        match check_seek(make, bytes, &targets) {
            Ok(s) => assert_eq!(s, SeekSupport::Supported, "{want}"),
            Err(v) => panic!("{want}：{v}"),
        }
    }
}
