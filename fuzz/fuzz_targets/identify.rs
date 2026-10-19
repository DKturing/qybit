//! E3：文件头识别与完整解码的一致性（WAV、AIFF、FLAC 各自的 `identify` 与组合识别
//! `AutoDecoder::identify`），由 `qy_testkit::check_identify` 检查：`NeedMore` 的下界、各前缀的结论
//! 与整个文件相同、`Supported` / `Rejected` / `Foreign` 与完整解码的结论一致。
//!
//! 输入首字节保留（与其他解码目标共用初始语料的格式），其余为文件内容。
#![no_main]

use libfuzzer_sys::fuzz_target;
use qy_codec_aiff::AiffDecoder;
use qy_codec_auto::{AutoDecoder, AutoDetail};
use qy_codec_flac::{Capacity, FlacDecoder};
use qy_codec_wav::WavDecoder;
use qy_core::PCM_WORK_SAMPLES;
use qy_testkit::check_identify;
use std::sync::{Mutex, OnceLock};

struct Decoders {
    wav: WavDecoder,
    aiff: AiffDecoder,
    flac: FlacDecoder,
    auto: AutoDecoder,
    probe: AutoDecoder,
}

fn leak<T: Default + Clone>(n: usize) -> &'static mut [T] {
    Box::leak(vec![T::default(); n].into_boxed_slice())
}

fn auto() -> AutoDecoder {
    let cap = Capacity::DESKTOP_MULTICHANNEL;
    match AutoDecoder::new(
        leak(PCM_WORK_SAMPLES),
        cap,
        leak(cap.workspace_len()),
        leak(cap.pcm_len()),
    ) {
        Ok(d) => d,
        Err(e) => panic!("{e:?}"),
    }
}

fn decoders() -> &'static Mutex<Decoders> {
    static D: OnceLock<Mutex<Decoders>> = OnceLock::new();
    D.get_or_init(|| {
        let cap = Capacity::DESKTOP_MULTICHANNEL;
        let (Ok(wav), Ok(aiff), Ok(flac)) = (
            WavDecoder::new(leak(PCM_WORK_SAMPLES)),
            AiffDecoder::new(leak(PCM_WORK_SAMPLES)),
            FlacDecoder::new(cap, leak(cap.workspace_len()), leak(cap.pcm_len())),
        ) else {
            panic!("工作区");
        };
        Mutex::new(Decoders {
            wav,
            aiff,
            flac,
            auto: auto(),
            probe: auto(),
        })
    })
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

fuzz_target!(|input: &[u8]| {
    let [_, data @ ..] = input else {
        return;
    };
    let Ok(mut guard) = decoders().lock() else {
        return;
    };
    let d = &mut *guard;
    let r = check_identify(qy_codec_wav::identify, &mut d.wav, |w| w.last_detail(), data);
    assert!(r.is_ok(), "WAV：{r:?}");
    let r = check_identify(qy_codec_aiff::identify, &mut d.aiff, |a| a.last_detail(), data);
    assert!(r.is_ok(), "AIFF：{r:?}");
    let r = check_identify(qy_codec_flac::identify, &mut d.flac, |f| f.last_error(), data);
    assert!(r.is_ok(), "FLAC：{r:?}");
    let probe = &d.probe;
    let r = check_identify(|h, eof| probe.identify(h, eof).1, &mut d.auto, auto_detail, data);
    assert!(r.is_ok(), "组合：{r:?}");
});
