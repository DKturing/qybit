//! M2 T2：ALSA 格式映射、候选、设备名、参数确认与打开错误的单元测试。
//! 期望字节均为独立手写，不由被测代码生成。

use super::*;
use crate::pack::{Exact, PackError, Pcm, pack};

fn src(rate: u32, ch: u16, bits: u8) -> SourceFormat {
    match SourceFormat::new(rate, ch, bits) {
        Ok(f) => f,
        Err(e) => panic!("{e:?}"),
    }
}

fn alsa(f: AlsaFormat, ch: u16) -> DeviceFormat {
    match f.device_format(48_000, ch) {
        Ok(d) => d,
        Err(e) => panic!("{e:?}"),
    }
}

/// pack 单声道样本，返回有效字节。
fn packed(samples: &[i32], n: u8, d: &DeviceFormat) -> Result<([u8; 64], usize), PackError> {
    let pcm: Pcm<'_, Exact> = Pcm::exact(samples, src(48_000, 1, n))?;
    let mut out = [0u8; 64];
    let frames = pack(&pcm, d, &mut out)?;
    Ok((out, frames * usize::from(d.block_align())))
}

fn expect_bytes(samples: &[i32], n: u8, d: &DeviceFormat, want: &[u8]) {
    match packed(samples, n, d) {
        Ok((out, len)) => assert_eq!(&out[..len], want, "N={n} → {d:?}"),
        Err(e) => panic!("N={n} → {d:?}: {e:?}"),
    }
}

#[test]
fn mapping_fields_codes_and_names() {
    let table = [
        (
            AlsaFormat::S16Le,
            "S16_LE",
            2,
            16,
            16,
            Placement::MsbAligned,
        ),
        (
            AlsaFormat::S24_3Le,
            "S24_3LE",
            32,
            24,
            24,
            Placement::MsbAligned,
        ),
        (
            AlsaFormat::S24Le,
            "S24_LE",
            6,
            32,
            24,
            Placement::LsbAligned,
        ),
        (
            AlsaFormat::S32Le,
            "S32_LE",
            10,
            32,
            32,
            Placement::MsbAligned,
        ),
    ];
    for (f, name, code, c, v, p) in table {
        assert_eq!((f.name(), f.code()), (name, code));
        assert_eq!(
            (f.container_bits(), f.valid_bits(), f.placement()),
            (c, v, p)
        );
        assert_eq!(AlsaFormat::from_code(code), Ok(f));
        assert_eq!(AlsaFormat::from_name(name), Ok(f));
        let d = alsa(f, 2);
        assert_eq!(d.endian(), Endian::Little);
        assert_eq!(d.block_align(), u16::from(c / 8) * 2);
        assert_eq!(AlsaFormat::from_device(&d), Ok(f));
    }
    // 不支持：大端、浮点、无符号、其他。
    for (code, why) in [
        (3, FormatRejection::BigEndian),  // S16_BE
        (7, FormatRejection::BigEndian),  // S24_BE
        (11, FormatRejection::BigEndian), // S32_BE
        (33, FormatRejection::BigEndian), // S24_3BE
        (14, FormatRejection::Float),     // FLOAT_LE
        (16, FormatRejection::Float),     // FLOAT64_LE
        (4, FormatRejection::Unsigned),   // U16_LE
        (34, FormatRejection::Unsigned),  // U24_3LE
        (0, FormatRejection::Other),      // S8
        (25, FormatRejection::Other),     // S20_LE
        (36, FormatRejection::Other),     // S20_3LE
        (-1, FormatRejection::Other),     // UNKNOWN
    ] {
        assert_eq!(AlsaFormat::from_code(code), Err(why), "code {code}");
    }
    for (name, why) in [
        ("S16_BE", FormatRejection::BigEndian),
        ("S24_3BE", FormatRejection::BigEndian),
        ("FLOAT_LE", FormatRejection::Float),
        ("FLOAT64_BE", FormatRejection::Float),
        ("U32_LE", FormatRejection::Unsigned),
        ("S20_LE", FormatRejection::Other),
        ("s16_le", FormatRejection::Other),
    ] {
        assert_eq!(AlsaFormat::from_name(name), Err(why), "{name}");
    }
    // WASAPI 32/24（高位对齐）与大端设备格式没有对应的 ALSA 格式。
    let Ok(wasapi) = DeviceFormat::wasapi(48_000, 2, 32, 24) else {
        panic!("fmt");
    };
    assert_eq!(
        AlsaFormat::from_device(&wasapi),
        Err(FormatRejection::NoAlsaEquivalent)
    );
    let Ok(be) = DeviceFormat::new(48_000, 2, 16, 16, Endian::Big, Placement::MsbAligned) else {
        panic!("fmt");
    };
    assert_eq!(
        AlsaFormat::from_device(&be),
        Err(FormatRejection::BigEndian)
    );
}

/// 符号与极值：每种 ALSA 格式对照独立手写字节；`S24_LE` 与 WASAPI 32/24 结果不同。
#[test]
fn pack_into_alsa_formats_matches_independent_bytes() {
    use AlsaFormat::*;
    let n16: &[i32] = &[-32_768, 32_767, -1, 0, 1];
    expect_bytes(
        n16,
        16,
        &alsa(S16Le, 1),
        &[0x00, 0x80, 0xFF, 0x7F, 0xFF, 0xFF, 0x00, 0x00, 0x01, 0x00],
    );
    // S24_3LE：左移 8 位，3 字节。
    expect_bytes(
        n16,
        16,
        &alsa(S24_3Le, 1),
        &[
            0x00, 0x00, 0x80, 0x00, 0xFF, 0x7F, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x01,
            0x00,
        ],
    );
    // S24_LE：24 位在低 3 字节、左移 8 位，高字节为符号扩展。
    let s24le_n16: &[u8] = &[
        0x00, 0x00, 0x80, 0xFF, // -32768 << 8 = -8388608 = 0xFF800000
        0x00, 0xFF, 0x7F, 0x00, // 32767 << 8 = 0x007FFF00
        0x00, 0xFF, 0xFF, 0xFF, // -1 << 8 = 0xFFFFFF00
        0x00, 0x00, 0x00, 0x00, //
        0x00, 0x01, 0x00, 0x00, // 1 << 8 = 0x00000100
    ];
    expect_bytes(n16, 16, &alsa(S24Le, 1), s24le_n16);
    // WASAPI 32/24：有效位在高 24 位，左移 16 位、低字节为 0。
    let wasapi_n16: &[u8] = &[
        0x00, 0x00, 0x00, 0x80, // 0x80000000
        0x00, 0x00, 0xFF, 0x7F, // 0x7FFF0000
        0x00, 0x00, 0xFF, 0xFF, // 0xFFFF0000
        0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x01, 0x00, // 0x00010000
    ];
    let Ok(wasapi) = DeviceFormat::wasapi(48_000, 1, 32, 24) else {
        panic!("fmt");
    };
    expect_bytes(n16, 16, &wasapi, wasapi_n16);
    assert_ne!(s24le_n16, wasapi_n16, "S24_LE 与 WASAPI 32/24 不得混淆");

    let n24: &[i32] = &[-8_388_608, 8_388_607, -1];
    expect_bytes(
        n24,
        24,
        &alsa(S24Le, 1),
        &[
            0x00, 0x00, 0x80, 0xFF, 0xFF, 0xFF, 0x7F, 0x00, 0xFF, 0xFF, 0xFF, 0xFF,
        ],
    );
    expect_bytes(
        n24,
        24,
        &wasapi,
        &[
            0x00, 0x00, 0x00, 0x80, 0x00, 0xFF, 0xFF, 0x7F, 0x00, 0xFF, 0xFF, 0xFF,
        ],
    );
    expect_bytes(
        n24,
        24,
        &alsa(S24_3Le, 1),
        &[0x00, 0x00, 0x80, 0xFF, 0xFF, 0x7F, 0xFF, 0xFF, 0xFF],
    );
    // N = 20 → S24_LE：左移 4 位。
    expect_bytes(
        &[-524_288, 524_287],
        20,
        &alsa(S24Le, 1),
        &[0x00, 0x00, 0x80, 0xFF, 0xF0, 0xFF, 0x7F, 0x00],
    );
    // N = 32 → S32_LE。
    expect_bytes(
        &[i32::MIN, i32::MAX, -2],
        32,
        &alsa(S32Le, 1),
        &[
            0x00, 0x00, 0x00, 0x80, 0xFF, 0xFF, 0xFF, 0x7F, 0xFE, 0xFF, 0xFF, 0xFF,
        ],
    );
    // N > V 拒绝，不截断。
    for (n, f, v) in [
        (32u8, S24Le, 24u8),
        (32, S24_3Le, 24),
        (24, S16Le, 16),
        (17, S16Le, 16),
    ] {
        assert_eq!(
            packed(&[0], n, &alsa(f, 1)).map(|r| r.1),
            Err(PackError::Precision {
                source_bits: n,
                valid_bits: v
            }),
            "N={n} → {f}"
        );
    }
}

#[test]
fn candidates_prefer_smallest_valid_bits_then_device_order() {
    use AlsaFormat::*;
    let list = |n: u8, pref: &[AlsaFormat]| {
        let c = candidates(&src(48_000, 2, n), pref);
        let mut out = [None; 4];
        for (slot, f) in out.iter_mut().zip(c.as_slice()) {
            *slot = Some(f);
        }
        (c.len(), out)
    };
    assert_eq!(
        list(16, &AlsaFormat::ALL),
        (4, [Some(S16Le), Some(S24_3Le), Some(S24Le), Some(S32Le)])
    );
    // V 相同（24）时按设备偏好：S24_LE 在前。
    assert_eq!(
        list(16, &[S32Le, S24Le, S24_3Le, S16Le]),
        (4, [Some(S16Le), Some(S24Le), Some(S24_3Le), Some(S32Le)])
    );
    assert_eq!(
        list(20, &AlsaFormat::ALL),
        (3, [Some(S24_3Le), Some(S24Le), Some(S32Le), None])
    );
    assert_eq!(
        list(24, &[S16Le, S32Le]),
        (1, [Some(S32Le), None, None, None])
    );
    assert_eq!(
        list(32, &AlsaFormat::ALL),
        (1, [Some(S32Le), None, None, None])
    );
    // 设备不支持能无损承载的格式：无候选（不降精度）。
    let c = candidates(&src(48_000, 2, 32), &[S16Le, S24Le, S24_3Le]);
    assert!(c.is_empty());
    // 重复项只计一次。
    assert_eq!(
        list(16, &[S16Le, S16Le, S32Le, S32Le]),
        (2, [Some(S16Le), Some(S32Le), None, None])
    );
}

#[test]
fn device_names_only_hw() {
    let ok = [
        ("hw:0", Card::Index(0), 0, None),
        ("hw:1,0", Card::Index(1), 0, None),
        ("hw:2,3", Card::Index(2), 3, None),
        ("hw:0,1,2", Card::Index(0), 1, Some(2)),
        ("hw:CARD=Loopback", Card::Name("Loopback"), 0, None),
        ("hw:CARD=Loopback,DEV=1", Card::Name("Loopback"), 1, None),
        (
            "hw:CARD=USB_DAC-2,DEV=0,SUBDEV=3",
            Card::Name("USB_DAC-2"),
            0,
            Some(3),
        ),
        ("hw:CARD=1,DEV=0", Card::Index(1), 0, None),
        ("  hw:0,0  ", Card::Index(0), 0, None),
        ("hw:Loopback,0,1", Card::Name("Loopback"), 0, Some(1)),
        ("hw:PCH", Card::Name("PCH"), 0, None),
    ];
    for (name, card, device, subdevice) in ok {
        assert_eq!(
            check_device_name(name),
            Ok(HwDevice {
                card,
                device,
                subdevice
            }),
            "{name}"
        );
    }
    assert_eq!(
        check_device_name("hw:1,2").map(|d| d.playback_node()),
        Ok(Some((1, 2)))
    );
    assert_eq!(
        check_device_name("hw:CARD=PCH").map(|d| d.playback_node()),
        Ok(None)
    );
    assert_eq!(
        check_device_name("hw:Loopback,0,1").map(|d| d.playback_node()),
        Ok(None)
    );
    use DeviceNameError::*;
    let bad = [
        ("", Empty),
        ("   ", Empty),
        ("plughw:0,0", Plug),
        ("plughw:CARD=PCH,DEV=0", Plug),
        ("default", Default),
        ("sysdefault:CARD=PCH", Default),
        ("dmix:CARD=PCH", Dmix),
        ("dsnoop", Dmix),
        ("pipewire", PipeWire),
        ("pulse", Pulse),
        ("front:CARD=PCH,DEV=0", NotHw),
        ("surround51", NotHw),
        ("hdmi:0", NotHw),
        ("HW:0", NotHw),
        ("hw", Malformed),
        ("hw:", Malformed),
        ("hw:_x", Malformed),
        ("hw:Loop back", Malformed),
        ("hw:0,x", Malformed),
        ("hw:-1", Malformed),
        ("hw:0,1,2,3", Malformed),
        ("hw:CARD=", Malformed),
        ("hw:CARD=a b", Malformed),
        ("hw:CARD=PCH,1", Malformed),
        ("hw:CARD=PCH,SUB=1", Malformed),
        ("hw:99999999999", Malformed),
    ];
    for (name, why) in bad {
        assert_eq!(check_device_name(name), Err(why), "{name:?}");
    }
}

fn request() -> HwRequest {
    HwRequest {
        format: AlsaFormat::S24_3Le,
        sample_rate: 44_100,
        channels: 2,
        layout: ChannelLayout::STEREO,
        period_frames: 441,
        buffer_frames: 1764,
    }
}

fn applied_ok() -> HwApplied {
    HwApplied {
        access: Access::RwInterleaved,
        format_code: AlsaFormat::S24_3Le.code(),
        rate_num: 44_100,
        rate_den: 1,
        channels: 2,
        period_frames: 441,
        buffer_frames: 1764,
        resample: false,
        significant_bits: Some(24),
    }
}

fn mismatches(src_bits: u8, a: &HwApplied) -> Mismatches {
    match confirm(&src(44_100, 2, src_bits), &request(), a) {
        Err(ConfirmError::Mismatch(m)) => m,
        other => panic!("期望不一致，实际 {other:?}"),
    }
}

#[test]
fn confirm_accepts_exact_parameters() {
    let want = alsa(AlsaFormat::S24_3Le, 2);
    let Ok(want) = DeviceFormat::new(
        44_100,
        2,
        want.container_bits(),
        want.valid_bits(),
        want.endian(),
        want.placement(),
    ) else {
        panic!("fmt");
    };
    assert_eq!(
        confirm(&src(44_100, 2, 24), &request(), &applied_ok()),
        Ok(want)
    );
    // 有理数等于请求值同样精确（88200/2 = 44100）。
    let a = HwApplied {
        rate_num: 88_200,
        rate_den: 2,
        ..applied_ok()
    };
    assert!(confirm(&src(44_100, 2, 16), &request(), &a).is_ok());
    // 有效位读不到时不据此拒绝；读到且 ≥ N 时接受。
    let a = HwApplied {
        significant_bits: None,
        ..applied_ok()
    };
    assert!(confirm(&src(44_100, 2, 24), &request(), &a).is_ok());
    let a = HwApplied {
        significant_bits: Some(20),
        ..applied_ok()
    };
    assert!(confirm(&src(44_100, 2, 16), &request(), &a).is_ok());
}

#[test]
fn confirm_rejects_each_mismatch() {
    // 近似采样率（整数但不等）。
    let m = mismatches(
        24,
        &HwApplied {
            rate_num: 44_099,
            ..applied_ok()
        },
    );
    assert_eq!(m.len(), 1);
    assert!(m.contains(|x| matches!(
        x,
        ParamMismatch::Rate {
            requested: 44_100,
            num: 44_099,
            den: 1
        }
    )));
    // 非整数近似（132301/3 ≈ 44100.33）与分母为 0。
    for (num, den) in [(132_301u32, 3u32), (44_100, 0)] {
        let m = mismatches(
            24,
            &HwApplied {
                rate_num: num,
                rate_den: den,
                ..applied_ok()
            },
        );
        assert!(
            m.contains(|x| matches!(x, ParamMismatch::Rate { .. })),
            "{num}/{den}"
        );
    }
    // 格式被改写（请求 S24_3LE，读回 S32_LE）。
    let m = mismatches(
        24,
        &HwApplied {
            format_code: AlsaFormat::S32Le.code(),
            ..applied_ok()
        },
    );
    assert_eq!(
        m.iter().next(),
        Some(ParamMismatch::Format {
            requested: AlsaFormat::S24_3Le,
            applied_code: 10
        })
    );
    // 周期、缓冲、声道、访问方式、重采样。
    type Case = (HwApplied, fn(&ParamMismatch) -> bool);
    let cases: [Case; 5] = [
        (
            HwApplied {
                period_frames: 448,
                ..applied_ok()
            },
            |x| {
                matches!(
                    x,
                    ParamMismatch::Period {
                        requested: 441,
                        applied: 448
                    }
                )
            },
        ),
        (
            HwApplied {
                buffer_frames: 2048,
                ..applied_ok()
            },
            |x| matches!(x, ParamMismatch::Buffer { applied: 2048, .. }),
        ),
        (
            HwApplied {
                channels: 1,
                ..applied_ok()
            },
            |x| matches!(x, ParamMismatch::Channels { applied: 1, .. }),
        ),
        (
            HwApplied {
                access: Access::MmapInterleaved,
                ..applied_ok()
            },
            |x| matches!(x, ParamMismatch::Access(Access::MmapInterleaved)),
        ),
        (
            HwApplied {
                resample: true,
                ..applied_ok()
            },
            |x| matches!(x, ParamMismatch::Resample),
        ),
    ];
    for (a, pred) in cases {
        let m = mismatches(24, &a);
        assert_eq!(m.len(), 1, "{a:?}");
        assert!(m.contains(pred), "{a:?}");
    }
    // 设备实际有效位少于源 N。
    let m = mismatches(
        24,
        &HwApplied {
            significant_bits: Some(20),
            ..applied_ok()
        },
    );
    assert!(m.contains(|x| matches!(
        x,
        ParamMismatch::SignificantBits {
            source: 24,
            applied: 20
        }
    )));
    // 多项同时不一致：全部列出。
    let m = mismatches(
        24,
        &HwApplied {
            rate_num: 48_000,
            period_frames: 480,
            buffer_frames: 1920,
            resample: true,
            ..applied_ok()
        },
    );
    assert_eq!(m.len(), 4);
}

#[test]
fn confirm_rejects_request_that_cannot_carry_source() {
    // 源 N=24 不能放入 S16_LE；源采样率/声道与请求不同也不接受。
    let r = HwRequest {
        format: AlsaFormat::S16Le,
        ..request()
    };
    assert_eq!(
        confirm(&src(44_100, 2, 24), &r, &applied_ok()),
        Err(ConfirmError::Request(FormatError::ValidBits))
    );
    assert!(matches!(
        confirm(&src(48_000, 2, 16), &request(), &applied_ok()),
        Err(ConfirmError::Request(_))
    ));
    assert!(matches!(
        confirm(&src(44_100, 1, 16), &request(), &applied_ok()),
        Err(ConfirmError::Request(_))
    ));
}

#[test]
fn open_errors_are_classified() {
    assert_eq!(OpenError::from_errno(-16), OpenError::Busy);
    assert_eq!(OpenError::from_errno(16), OpenError::Busy);
    assert_eq!(OpenError::from_errno(-2), OpenError::NoDevice);
    assert_eq!(OpenError::from_errno(-19), OpenError::NoDevice);
    assert_eq!(OpenError::from_errno(-13), OpenError::Permission);
    assert_eq!(OpenError::from_errno(-1), OpenError::Permission);
    assert_eq!(OpenError::from_errno(-22), OpenError::Other(22));
    assert_eq!(OpenError::from_errno(i32::MIN), OpenError::Other(i32::MAX));
}

// ---- F1：声道映射（chmap）----

use crate::Speaker::*;

/// 独立写出的 ALSA 位置值（alsa-lib pcm.h）：FL=3 FR=4 RL=5 RR=6 FC=7 LFE=8 SL=9 SR=10 RC=11。
const FL: u32 = 3;
const FR: u32 = 4;
const RL: u32 = 5;
const RR: u32 = 6;
const FC: u32 = 7;
const LFE: u32 = 8;
const SL: u32 = 9;
const SR: u32 = 10;

fn layout(mask: u32) -> ChannelLayout {
    match ChannelLayout::from_mask(mask) {
        Some(l) => l,
        None => panic!("{mask:#x}"),
    }
}

#[test]
fn chmap_positions_map_to_speakers() {
    assert_eq!(
        layout_from_chmap(&[FL, FR, RL, RR, FC, LFE]).map(|l| l.mask()),
        Ok(0x3F)
    );
    assert_eq!(
        layout_from_chmap(&[FL, FR, RL, RR, FC, LFE]).map(|l| l.is_wave_order()),
        Ok(false)
    );
    assert_eq!(
        layout_from_chmap(&[FL, FR, FC, LFE, SL, SR]),
        Ok(layout(0x60F))
    );
    assert_eq!(layout_from_chmap(&[CHMAP_MONO]), Ok(ChannelLayout::MONO));
    assert_eq!(
        layout_from_chmap(&[FL, FR | CHMAP_PHASE_INVERSE]),
        Err(ChmapError::PhaseInverse { channel: 1 })
    );
    // UNKNOWN=0、NA=1、FLW=16、LLFE=37 没有对应的 WAVE 位置。
    for p in [0, 1, 16, 37] {
        assert_eq!(
            layout_from_chmap(&[FL, p]),
            Err(ChmapError::UnknownPosition {
                channel: 1,
                position: p
            })
        );
    }
    assert_eq!(layout_from_chmap(&[FL, FL]), Err(ChmapError::Invalid));
    assert_eq!(layout_from_chmap(&[]), Err(ChmapError::Invalid));
    // 往返：每个 WAVE 位置的 ALSA 值都能映回同一位置。
    for bit in 0..18 {
        let Some(s) = Speaker::from_bit(bit) else {
            panic!("{bit}");
        };
        assert_eq!(
            layout_from_chmap(&[chmap_position(s)]).map(|l| l.positions()[0]),
            Ok(s)
        );
    }
    assert_eq!(chmap_position(BackCenter), 11);
    assert_eq!(chmap_position(TopBackRight), 26);
}

#[test]
fn chmap_plan_prefers_var_then_fixed() {
    let src51 = layout(0x3F);
    let hda: &[u32] = &[FL, FR, RL, RR, FC, LFE];
    let side: &[u32] = &[FL, FR, FC, LFE, SL, SR];
    let stereo: &[u32] = &[FL, FR];
    // 可变映射：设置为源布局（不置换）。
    assert_eq!(
        plan_chmap(&src51, &[(ChmapType::Var, hda)]),
        Ok(ChmapPlan::Set(src51))
    );
    // 固定映射：按设备顺序，pack 置换。
    let Ok(ChmapPlan::Fixed(l)) = plan_chmap(&src51, &[(ChmapType::Fixed, hda)]) else {
        panic!("fixed");
    };
    assert!(l.same_positions(&src51) && l != src51);
    // 位置集合不同：拒绝。
    assert_eq!(
        plan_chmap(
            &src51,
            &[(ChmapType::Fixed, side), (ChmapType::Fixed, stereo)]
        ),
        Err(ChmapError::NoMatchingMap)
    );
    // 设备不报告映射：多于 2 声道拒绝，双声道按默认布局（M2 行为不变）。
    assert_eq!(plan_chmap(&src51, &[]), Err(ChmapError::Unavailable));
    assert_eq!(
        plan_chmap(&ChannelLayout::STEREO, &[]),
        Ok(ChmapPlan::Unqueried(ChannelLayout::STEREO))
    );
    assert_eq!(
        plan_chmap(&ChannelLayout::STEREO, &[(ChmapType::Fixed, hda)]),
        Ok(ChmapPlan::Unqueried(ChannelLayout::STEREO))
    );
    // 反相映射被跳过。
    let inv: &[u32] = &[FL, FR, RL, RR, FC, LFE | CHMAP_PHASE_INVERSE];
    assert_eq!(
        plan_chmap(&src51, &[(ChmapType::Fixed, inv)]),
        Err(ChmapError::NoMatchingMap)
    );
    // 读回核对。
    let plan = ChmapPlan::Set(src51);
    assert_eq!(confirm_chmap(&plan, &[FL, FR, FC, LFE, RL, RR]), Ok(src51));
    assert_eq!(confirm_chmap(&plan, hda), Err(ChmapError::ReadBack));
}

#[test]
fn confirm_with_reordered_layout() {
    let Ok(hda) = layout_from_chmap(&[FL, FR, RL, RR, FC, LFE]) else {
        panic!("hda");
    };
    let r = HwRequest {
        format: AlsaFormat::S24Le,
        channels: 6,
        layout: hda,
        ..request()
    };
    let a = HwApplied {
        format_code: AlsaFormat::S24Le.code(),
        channels: 6,
        ..applied_ok()
    };
    let Ok(src51) = SourceFormat::with_layout(44_100, layout(0x3F), 24) else {
        panic!("src");
    };
    let Ok(d) = confirm(&src51, &r, &a) else {
        panic!("confirm");
    };
    assert!(d.reorders(&src51));
    assert_eq!(d.layout(), hda);
    // 布局声道数与请求不符：拒绝。
    let bad = HwRequest { channels: 2, ..r };
    assert_eq!(
        confirm(&src51, &bad, &a),
        Err(ConfirmError::Request(FormatError::UndefinedLayout))
    );
}
