//! E3：文件头识别与完整解码一致。WAV、AIFF、FLAC 三个解码器的全部夹具（含被拒绝的、其他格式
//! 的文件）依次经过各解码器自己的 `identify` 与组合识别 `AutoDecoder::identify`，由
//! `qy_testkit::check_identify` 与同一个解码器的完整解码比较（含每个前缀的结论）。另加合成的
//! 边界情形与 ID3v2 标签。本地有 fuzz 语料（`fuzz/corpus/*`，不入库）时一并检查。
#![cfg(all(feature = "wav", feature = "flac", feature = "aiff"))]

use std::path::{Path, PathBuf};

use qy_codec_aiff::AiffDecoder;
use qy_codec_auto::{AutoDecoder, AutoDetail, Codec};
use qy_codec_flac::{Capacity, FlacDecoder};
use qy_codec_wav::WavDecoder;
use qy_core::{DecodeError, Identify};
use qy_testkit::check_identify;

fn leak<T: Default + Clone>(n: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); n].into_boxed_slice())
}

struct Decoders {
    wav: WavDecoder,
    aiff: AiffDecoder,
    flac: FlacDecoder,
    auto: AutoDecoder,
    probe: AutoDecoder,
}

fn decoders() -> Decoders {
    let cap = Capacity::DESKTOP_MULTICHANNEL;
    let (Ok(wav), Ok(aiff), Ok(flac), Ok(auto), Ok(probe)) = (
        WavDecoder::new(leak(qy_core::PCM_WORK_SAMPLES)),
        AiffDecoder::new(leak(qy_core::PCM_WORK_SAMPLES)),
        FlacDecoder::new(cap, leak(cap.workspace_len()), leak(cap.pcm_len())),
        AutoDecoder::new(
            leak(qy_core::PCM_WORK_SAMPLES),
            cap,
            leak(cap.workspace_len()),
            leak(cap.pcm_len()),
        ),
        AutoDecoder::new(
            leak(qy_core::PCM_WORK_SAMPLES),
            cap,
            leak(cap.workspace_len()),
            leak(cap.pcm_len()),
        ),
    ) else {
        panic!("工作区");
    };
    Decoders {
        wav,
        aiff,
        flac,
        auto,
        probe,
    }
}

fn auto_detail(d: &AutoDecoder) -> Option<AutoDetail> {
    if let Some(w) = d.wav_detail() {
        return Some(AutoDetail::Wav(w));
    }
    if let Some(a) = d.aiff_detail() {
        return Some(AutoDetail::Aiff(a));
    }
    if let Some(f) = d.flac_error() {
        return Some(AutoDetail::Flac(f));
    }
    d.disabled().map(AutoDetail::Disabled)
}

/// 一个文件经过四种识别；返回组合识别的结论。
fn check_all(d: &mut Decoders, name: &str, bytes: &[u8]) -> (Codec, Identify<AutoDetail>) {
    let fail = |what: &str, e: qy_testkit::Violation| -> ! { panic!("{name}（{what}）：{e}") };
    let wav = check_identify(
        qy_codec_wav::identify,
        &mut d.wav,
        |w| w.last_detail(),
        bytes,
    )
    .unwrap_or_else(|e| fail("WAV", e));
    let aiff = check_identify(
        qy_codec_aiff::identify,
        &mut d.aiff,
        |a| a.last_detail(),
        bytes,
    )
    .unwrap_or_else(|e| fail("AIFF", e));
    let flac = check_identify(
        qy_codec_flac::identify,
        &mut d.flac,
        |f| f.last_error(),
        bytes,
    )
    .unwrap_or_else(|e| fail("FLAC", e));
    // 组合识别是 `&self` 方法，不改变解码器状态；完整解码要 `&mut`，识别另用一个同样构造的实例。
    let probe = &d.probe;
    let (auto, skipped) = check_identify(
        |h, eof| probe.identify(h, eof).1,
        &mut d.auto,
        auto_detail,
        bytes,
    )
    .unwrap_or_else(|e| fail("组合", e));
    let rest = &bytes[skipped as usize..];
    let (codec, again) = probe.identify(rest, true);
    assert_eq!(again, auto, "{name}");
    // 组合识别与各格式自己的识别一致：认领的格式只有一个，结论就是它的结论。
    let mine = [
        (Codec::Wav, wav.0.map_detail(AutoDetail::Wav)),
        (Codec::Aiff, aiff.0.map_detail(AutoDetail::Aiff)),
        (Codec::Flac, flac.0.map_detail(AutoDetail::Flac)),
    ];
    let claimed: Vec<_> = mine.iter().filter(|(_, r)| r.is_mine()).collect();
    // `Codec::Unknown` 的结论来自 ID3v2 标签头本身（不完整），不属于任何容器。
    if skipped == 0 && codec != Codec::Unknown {
        match claimed.as_slice() {
            [] => assert_eq!(
                (codec, auto),
                (Codec::Unrecognized, Identify::Foreign),
                "{name}"
            ),
            [(c, r)] => assert_eq!((codec, auto), (*c, *r), "{name}"),
            more => panic!("{name}：{} 个格式同时认领", more.len()),
        }
    }
    (codec, auto)
}

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            files(&p, out);
        } else if p
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| matches!(x, "wav" | "flac" | "aif" | "aiff" | "aifc" | "raw"))
        {
            out.push(p);
        }
    }
}

#[test]
fn identify_matches_full_decode_on_all_fixtures() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut paths = Vec::new();
    for c in ["qy-codec-wav", "qy-codec-aiff", "qy-codec-flac"] {
        files(&root.join(c).join("tests/fixtures"), &mut paths);
    }
    paths.sort();
    assert!(paths.len() > 250, "夹具 {} 个", paths.len());
    let mut d = decoders();
    let mut seen = [0usize; 4];
    for p in &paths {
        let bytes = std::fs::read(p).unwrap_or_else(|e| panic!("{}：{e}", p.display()));
        let name = p.display().to_string();
        let (_, r) = check_all(&mut d, &name, &bytes);
        seen[match r {
            Identify::Supported { .. } => 0,
            Identify::Rejected { .. } => 1,
            Identify::Foreign => 2,
            _ => 3,
        }] += 1;
    }
    // 夹具里三种结论都有（支持的、被拒绝的子类型与不合规文件、其他格式）。
    assert!(seen[0] > 100 && seen[1] > 20 && seen[2] > 0, "{seen:?}");
    eprintln!(
        "{} 个夹具：Supported {}，Rejected {}，Foreign {}",
        paths.len(),
        seen[0],
        seen[1],
        seen[2]
    );
}

/// 本地 fuzz 语料（不入库；CI 的 fuzz 目标 `identify` 另外检查同一组约定）。
#[test]
fn identify_matches_full_decode_on_local_corpus() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus");
    let mut d = decoders();
    let mut n = 0usize;
    for t in [
        "auto_decode",
        "wav_decode",
        "aiff_decode",
        "flac_decode",
        "wav_decode_w1",
    ] {
        let Ok(rd) = std::fs::read_dir(root.join(t)) else {
            continue;
        };
        let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries.iter().take(400) {
            let Ok(bytes) = std::fs::read(p) else {
                continue;
            };
            // auto_decode 语料的前两个字节是分块种子与标签参数。
            let data = if t == "auto_decode" {
                bytes.get(2..).unwrap_or(&[])
            } else {
                &bytes[..]
            };
            check_all(&mut d, &p.display().to_string(), data);
            n += 1;
        }
    }
    eprintln!("本地 fuzz 语料 {n} 个");
}

/// 合成的边界情形：短文件头、其他 RIFF / IFF 格式、未启用的解码器、ID3v2 标签。
#[test]
fn identify_edge_cases() {
    let mut d = decoders();
    // 解码时的选择与识别相同：RIFF 但不是 WAVE 的文件按无法识别拒绝，写明形式类型。
    use qy_core::Decoder;
    assert_eq!(
        d.auto.decode(b"RIFF\x10\0\0\0AVI LIST", true),
        Err(DecodeError::Unsupported)
    );
    let Some(u) = d.auto.unrecognized() else {
        panic!("应为无法识别");
    };
    assert_eq!(u.form_type, Some(*b"AVI "));
    assert!(u.to_string().contains("不是 WAVE"), "{u}");
    let probe = &d.probe;
    let id = |h: &[u8], eof: bool| probe.identify(h, eof);
    // 空输入与标记前缀：还要更多字节；文件到此结束则不是任何已知格式（或不完整）。
    assert_eq!(
        id(b"", false).1,
        Identify::NeedMore {
            total: 1,
            container: false
        }
    );
    assert_eq!(
        id(b"RI", false).1,
        Identify::NeedMore {
            total: 3,
            container: false
        }
    );
    assert_eq!(id(b"RI", true), (Codec::Unrecognized, Identify::Foreign));
    assert_eq!(
        id(b"OggS\0\0\0\0", true),
        (Codec::Unrecognized, Identify::Foreign)
    );
    // RIFF 但不是 WAVE（AVI），FORM 但不是 AIFF：不是本程序的格式，插件可以接手。
    assert_eq!(id(b"RIFF\x10\0\0\0AVI LIST", true).1, Identify::Foreign);
    assert_eq!(id(b"FORM\0\0\0\x10ILBMBMHD", true).1, Identify::Foreign);
    // RIFX（大端 WAV）：本格式，不支持。
    let rifx = id(b"RIFX\0\0\0\x24WAVEfmt ", true);
    assert_eq!(rifx.0, Codec::Wav);
    assert!(matches!(
        rifx.1,
        Identify::Rejected {
            error: DecodeError::Unsupported,
            ..
        }
    ));
    // 标记已符合、子类型尚未读到：container 为真。
    assert_eq!(
        id(b"RIFF\xff\xff\xff\xffWAVEJUNK\x00\x00\x10\x00", false).1,
        Identify::NeedMore {
            total: 12 + 8 + 0x10_0000,
            container: true
        }
    );
    // ID3v2 标签：先等足 10 字节，再给出标签长度（标签头 + 声明长度）。
    assert_eq!(
        id(b"ID3\x04\0", false).1,
        Identify::NeedMore {
            total: 10,
            container: false
        }
    );
    assert_eq!(
        id(b"ID3\x04\0\0\0\0\x01\x00", false).1,
        Identify::Skip { bytes: 10 + 128 }
    );
    // fuzz 发现（CI identify 冒烟）：data 长度为 0，其后是 RIFF 内的零长度块，RIFF 最后余下不足
    // 一个块头（6 字节）。余下的字节到齐即可下结论，NeedMore 的下界不得说成整个块头。
    let mut riff_tail = Vec::new();
    riff_tail.extend_from_slice(b"RIFFB\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x44\xac\0\0\x88\x58\x01\0\x02\0\x10\0data\0\0\0\0");
    riff_tail.resize(134, 0);
    riff_tail[100] = 0x14;
    let (codec, result) = check_all(&mut d, "riff-tail", &riff_tail);
    assert_eq!(codec, Codec::Wav);
    assert!(
        matches!(
            result,
            Identify::Rejected {
                error: DecodeError::Malformed,
                ..
            }
        ),
        "{result:?}"
    );
    let probe = &d.probe;
    let id = |h: &[u8], eof: bool| probe.identify(h, eof);
    assert_eq!(
        id(&riff_tail[..68], false).1,
        Identify::NeedMore {
            total: 74,
            container: true
        }
    );
    // fuzz 发现：RF64 的 ds64 块（136 字节）超出 ds64 给出的 RIFF 长度（RIFF 在第 104 字节结束）。
    // 跳过时读到 RIFF 之外的第一个字节即拒绝，下界不得说成整个块。
    let rf64_over: &[u8] = b"\x52\x46\x36\x34\xff\xff\xff\xff\x57\x41\x56\x45\x64\x73\x36\x34\x88\x00\x00\x00\x60\x00\x00\x00\x00\x00\x00\x00\x20\x00\x00\x00\x00\x00\x00\x00\x08\x00\x00\x00\x00\x00\x09\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x2e\x00\x00\x00\x00\x00\x00\x00\x00\x00\xff\xff\xff\xff\xff\xff\xff\xff\x00\x00\x00\x00\x00\x00\x00\x00";
    let (codec, result) = check_all(&mut d, "rf64-skip-over-riff", rf64_over);
    assert_eq!(codec, Codec::Wav);
    assert!(
        matches!(
            result,
            Identify::Rejected {
                error: DecodeError::Malformed,
                ..
            }
        ),
        "{result:?}"
    );
    assert_eq!(
        d.probe.identify(&rf64_over[..48], false).1,
        Identify::NeedMore {
            total: 105,
            container: true
        }
    );
    // 未启用 FLAC 的组合解码器：认出名字，按不支持拒绝。
    let Ok(wav_only) = AutoDecoder::empty().with_wav(leak(qy_core::PCM_WORK_SAMPLES)) else {
        panic!("工作区");
    };
    assert_eq!(
        wav_only.identify(b"fLaC\0\0\0\x22", true),
        (
            Codec::Disabled,
            Identify::Rejected {
                error: DecodeError::Unsupported,
                detail: Some(AutoDetail::Disabled("FLAC"))
            }
        )
    );
}
