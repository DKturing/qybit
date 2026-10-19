//! pack：唯一的输出表示转换边界（AGENTS §3，M0 T3）。
//!
//! 输入为解码器输出的交错、右对齐 `i32`（有效位 N），输出为设备格式的完整帧字节。
//! N ≤ V 时按 `s × 2^(V−N)` 放入 V 位，保持精度与相对幅度（满幅仍为满幅），再按
//! 放置方式放入 C 位容器：
//! - [`Placement::MsbAligned`]（WASAPI 32/24 等）：V 位居容器高位，低 C−V 位为 0；
//! - [`Placement::LsbAligned`]（ALSA `S24_LE` 等）：V 位居容器低位，高位符号扩展。
//!
//! N > V 拒绝；样本超出 N 位有符号范围视为溢出并拒绝，不截断、不饱和。
//! 浮点源（F3）只放入同一浮点编码的设备格式：按位复制，只做字节序与声道位置的排列，不做数值运算。
//! DSD（D1，tasks/D1.md）只放入 DSD 设备格式：每个 32 位 DSD 字按传输方式拆成 e 个设备帧——原生
//! `DSD_U8` / `DSD_U16_*` / `DSD_U32_*` 按字节序原样写出；DoP 每个设备帧写标记（`0x05` / `0xFA`）
//! 与 16 位 DSD。不改变任何一位 DSD 数据，不转 PCM。
//! pack 在生产端运行；渲染线程只搬运其结果，不做任何位移或字节序转换。

use core::marker::PhantomData;

use crate::{
    DOP_MARKERS, DeviceFormat, DsdTransport, DsdWord, Endian, MAX_LAYOUT_CHANNELS, Placement,
    SampleEncoding, SourceFormat, dop_bytes,
};

mod sealed {
    pub trait Sealed {}
}

/// 样本处理标记。只能是 [`Exact`] 或 [`Altered`]。
pub trait Fidelity: sealed::Sealed {}

/// 样本值与源逐位一致（解码器的直接输出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exact {}
/// 样本值已被改变。唯一的产生者是明确开启的浮点转整数（F3，[`crate::convert`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Altered {}

impl sealed::Sealed for Exact {}
impl sealed::Sealed for Altered {}
impl Fidelity for Exact {}
impl Fidelity for Altered {}

/// 交错 PCM 视图，带格式与处理标记。
///
/// 类型标记只约束软件操作，不证明设备端 bit-perfect（AGENTS §3）。
#[derive(Debug, Clone, Copy)]
pub struct Pcm<'a, M: Fidelity> {
    samples: &'a [i32],
    format: SourceFormat,
    _marker: PhantomData<M>,
}

/// pack 错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackError {
    /// 样本数不是声道数的整数倍。
    PartialFrame,
    /// 采样率或扬声器位置集合与设备格式不同（禁止隐式重采样、声道增减或混音）。
    FormatMismatch,
    /// N > V：设备有效位不足以无损容纳源精度。
    Precision { source_bits: u8, valid_bits: u8 },
    /// 样本超出 N 位有符号范围。
    SampleOutOfRange { index: usize },
    /// 源与设备的样本编码不同（F3：浮点只按位放入同一浮点编码的设备格式；整数不放入浮点格式）。
    Encoding,
}

impl<'a> Pcm<'a, Exact> {
    /// 由解码器输出构造精确 PCM。仅供直接承载解码结果的宿主/核心使用，
    /// 任何改变样本值的操作都不得返回 `Pcm<Exact>`。
    pub fn exact(samples: &'a [i32], format: SourceFormat) -> Result<Self, PackError> {
        Self::checked(samples, format)
    }
}

impl<'a> Pcm<'a, Altered> {
    /// 改变了样本值的 PCM。只在本 crate 内由明确开启的转换构造（[`crate::convert`]）。
    pub(crate) fn altered(samples: &'a [i32], format: SourceFormat) -> Result<Self, PackError> {
        Self::checked(samples, format)
    }
}

impl<'a, M: Fidelity> Pcm<'a, M> {
    fn checked(samples: &'a [i32], format: SourceFormat) -> Result<Self, PackError> {
        if !samples.len().is_multiple_of(format.words_per_frame()) {
            return Err(PackError::PartialFrame);
        }
        Ok(Self {
            samples,
            format,
            _marker: PhantomData,
        })
    }
}

impl<M: Fidelity> Pcm<'_, M> {
    /// 解码输出原样（浮点为位模式，binary64 每样本两个字）。
    pub fn samples(&self) -> &[i32] {
        self.samples
    }

    pub fn format(&self) -> SourceFormat {
        self.format
    }

    pub fn frames(&self) -> usize {
        self.samples.len() / self.format.words_per_frame()
    }
}

/// 检查源与设备格式是否可无损 pack。
pub fn check(src: &SourceFormat, dev: &DeviceFormat) -> Result<(), PackError> {
    if src.sample_rate() != dev.sample_rate() || !src.layout().same_positions(&dev.layout()) {
        return Err(PackError::FormatMismatch);
    }
    if src.encoding() != dev.encoding() || src.encoding().is_dsd() != dev.dsd_transport().is_some()
    {
        return Err(PackError::Encoding);
    }
    if src.encoding().is_float() || src.encoding().is_dsd() {
        return Ok(());
    }
    if src.bits() > dev.valid_bits() {
        return Err(PackError::Precision {
            source_bits: src.bits(),
            valid_bits: dev.valid_bits(),
        });
    }
    Ok(())
}

/// 设备第 j 个声道取源的哪个声道：按扬声器位置对应（F1）。位置集合相同由 [`check`] 保证。
fn channel_map(
    src: &SourceFormat,
    dev: &DeviceFormat,
) -> Result<[u8; MAX_LAYOUT_CHANNELS], PackError> {
    let mut map = [0u8; MAX_LAYOUT_CHANNELS];
    let s = src.layout();
    for (slot, &pos) in map.iter_mut().zip(dev.layout().positions()) {
        let i = s.index_of(pos).ok_or(PackError::FormatMismatch)?;
        *slot = i as u8;
    }
    Ok(map)
}

/// 声道处理方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChannelMode {
    /// 严格路径：扬声器位置集合与设备相同，按位置纯置换（默认）。
    #[default]
    Exact,
    /// 明确开启的非严格选项（tasks/O1.md，AGENTS §3）：单声道 `[FC]` 的每个样本原样复制到立体声
    /// `[FL FR]` 设备格式的两个声道。样本值不变、不混音、不改增益，但声道布局改变，调用方必须在
    /// 报告中标为“已改变”。
    DuplicateMono,
}

/// 按声道处理方式检查源与设备格式是否可 pack：[`ChannelMode::Exact`] 同 [`check`]；
/// [`ChannelMode::DuplicateMono`] 要求源为 `[FC]`、设备为 `[FL FR]`，采样率相同且 N ≤ V。
pub fn check_mode(
    src: &SourceFormat,
    dev: &DeviceFormat,
    mode: ChannelMode,
) -> Result<(), PackError> {
    match mode {
        ChannelMode::Exact => check(src, dev),
        ChannelMode::DuplicateMono => {
            // 单声道复制（O1）只批准用于 PCM。
            if src.encoding().is_dsd() {
                return Err(PackError::Encoding);
            }
            if src.sample_rate() != dev.sample_rate()
                || src.layout() != crate::ChannelLayout::MONO
                || dev.layout() != crate::ChannelLayout::STEREO
            {
                return Err(PackError::FormatMismatch);
            }
            if src.encoding() != dev.encoding() {
                return Err(PackError::Encoding);
            }
            if !src.encoding().is_float() && src.bits() > dev.valid_bits() {
                return Err(PackError::Precision {
                    source_bits: src.bits(),
                    valid_bits: dev.valid_bits(),
                });
            }
            Ok(())
        }
    }
}

/// 样本在 C 位容器中的左移量：高位放置为 C − N（V 位居高位、低位补 0），低位放置为 V − N。
/// 调用方保证 N ≤ V ≤ C ≤ 32。
fn placement_shift(placement: Placement, c: u32, v: u32, n: u32) -> u32 {
    match placement {
        Placement::MsbAligned => c - n,
        Placement::LsbAligned => v - n,
    }
}

/// 一个样本写入设备字节（pack 中唯一的数值运算；M2 T5 的 Kani 证明对象）。`s` 已检查在 N 位
/// 有符号范围内，`shift` 来自 [`placement_shift`]，`dst` 为一个容器（C/8 字节）。
fn place_sample(s: i64, shift: u32, endian: Endian, dst: &mut [u8]) {
    let bytes = dst.len();
    // shift ≤ 31 且 s 在 N 位内，结果落在 C 位有符号范围内，不会溢出。
    let word = ((s << shift) as u32).to_le_bytes();
    match endian {
        Endian::Little => dst.copy_from_slice(&word[..bytes]),
        Endian::Big => {
            for (k, b) in dst.iter_mut().enumerate() {
                *b = word[bytes - 1 - k];
            }
        }
        // 高半字在前，半字内小端；16 位容器只有一个半字（构造时已拒绝 24 位容器）。
        Endian::HalfWordSwapped => {
            if bytes == 4 {
                dst.copy_from_slice(&[word[2], word[3], word[0], word[1]]);
            } else {
                dst.copy_from_slice(&word[..bytes]);
            }
        }
    }
}

/// 把 `src` 尽可能多的完整帧 pack 到 `out`，返回帧数（受 `out` 容量限制）。
///
/// 设备声道顺序与源不同（位置集合相同）时按位置纯置换：每个样本值原样放入对应扬声器的
/// 设备声道，不增减、不混合声道（tasks/F1.md §6）。
///
/// 出错时 `out` 内容未定义，调用方不得入队。
pub fn pack<M: Fidelity>(
    src: &Pcm<'_, M>,
    dev: &DeviceFormat,
    out: &mut [u8],
) -> Result<usize, PackError> {
    pack_with(src, dev, ChannelMode::Exact, out)
}

/// 同 [`pack`]，声道处理方式由 `mode` 给出。[`ChannelMode::DuplicateMono`] 时设备第 0、1 声道都取
/// 源的唯一声道。
pub fn pack_with<M: Fidelity>(
    src: &Pcm<'_, M>,
    dev: &DeviceFormat,
    mode: ChannelMode,
    out: &mut [u8],
) -> Result<usize, PackError> {
    check_mode(&src.format, dev, mode)?;
    let map = match mode {
        ChannelMode::Exact => channel_map(&src.format, dev)?,
        // 设备 [FL FR] 两个声道都取源声道 0（[FC]）。
        ChannelMode::DuplicateMono => [0u8; MAX_LAYOUT_CHANNELS],
    };
    if src.format.encoding().is_float() {
        return Ok(pack_float(src, dev, &map, out));
    }
    if let Some(t) = dev.dsd_transport() {
        return Ok(pack_dsd(src, dev, t, &map, out));
    }
    let src_channels = usize::from(src.format.channels());
    let n = u32::from(src.format.bits());
    let v = u32::from(dev.valid_bits());
    let c = u32::from(dev.container_bits());
    let bytes = (c / 8) as usize;
    let align = usize::from(dev.block_align());
    let channels = usize::from(dev.channels());
    let frames = src.frames().min(out.len() / align);
    let shift = placement_shift(dev.placement(), c, v, n);
    let lo = -(1i64 << (n - 1));
    let hi = (1i64 << (n - 1)) - 1;
    for (f, (frame, dst_frame)) in src
        .samples
        .chunks_exact(src_channels)
        .zip(out[..frames * align].chunks_exact_mut(align))
        .enumerate()
    {
        for (&from, dst) in map[..channels]
            .iter()
            .zip(dst_frame.chunks_exact_mut(bytes))
        {
            let from = usize::from(from);
            let s = i64::from(frame[from]);
            if s < lo || s > hi {
                return Err(PackError::SampleOutOfRange {
                    index: f * src_channels + from,
                });
            }
            place_sample(s, shift, dev.endian(), dst);
        }
    }
    Ok(frames)
}

/// 浮点按位复制（F3）：每个样本的位模式按设备字节序写出，设备第 j 个声道取 `map[j]`。
/// 编码相同由 [`check_mode`] 保证。不做数值运算。
fn pack_float<M: Fidelity>(
    src: &Pcm<'_, M>,
    dev: &DeviceFormat,
    map: &[u8; MAX_LAYOUT_CHANNELS],
    out: &mut [u8],
) -> usize {
    let words = src.format.encoding().words();
    let src_words = src.format.words_per_frame();
    let bytes = words * 4;
    let align = usize::from(dev.block_align());
    let channels = usize::from(dev.channels());
    let frames = src.frames().min(out.len() / align);
    for (frame, dst_frame) in src
        .samples
        .chunks_exact(src_words)
        .zip(out[..frames * align].chunks_exact_mut(align))
    {
        for (&from, dst) in map[..channels]
            .iter()
            .zip(dst_frame.chunks_exact_mut(bytes))
        {
            let at = usize::from(from) * words;
            let le: [u8; 8] = match src.format.encoding() {
                SampleEncoding::Float64 => {
                    let lo = u64::from(frame[at] as u32);
                    let hi = u64::from(frame[at + 1] as u32);
                    (hi << 32 | lo).to_le_bytes()
                }
                _ => u64::from(frame[at] as u32).to_le_bytes(),
            };
            match dev.endian() {
                Endian::Big => {
                    for (k, b) in dst.iter_mut().enumerate() {
                        *b = le[bytes - 1 - k];
                    }
                }
                // 浮点设备格式只有小端与大端（构造时拒绝半字交换）。
                Endian::Little | Endian::HalfWordSwapped => dst.copy_from_slice(&le[..bytes]),
            }
        }
    }
    frames
}

/// DSD 按位展开（D1）：每个源帧（每声道一个 32 位 DSD 字）写成 e 个设备帧，设备第 j 个声道取
/// `map[j]`。编码与传输由 [`check_mode`] 保证。
fn pack_dsd<M: Fidelity>(
    src: &Pcm<'_, M>,
    dev: &DeviceFormat,
    t: DsdTransport,
    map: &[u8; MAX_LAYOUT_CHANNELS],
    out: &mut [u8],
) -> usize {
    let src_channels = usize::from(src.format.channels());
    let align = usize::from(dev.block_align());
    let channels = usize::from(dev.channels());
    let sample = usize::from(t.sample_bytes());
    let dev_frame = channels * sample;
    let frames = src.frames().min(out.len() / align);
    for (frame, dst_word) in src
        .samples
        .chunks_exact(src_channels)
        .zip(out[..frames * align].chunks_exact_mut(align))
    {
        for (k, dst_frame) in dst_word.chunks_exact_mut(dev_frame).enumerate() {
            for (&from, dst) in map[..channels]
                .iter()
                .zip(dst_frame.chunks_exact_mut(sample))
            {
                dsd_sample(frame[usize::from(from)] as u32, k, t, dst);
            }
        }
    }
    frames
}

/// DSD 字 `w`（最早的位在 bit 31）的第 `k` 个设备帧中一个声道的样本（D1 的唯一位运算；Kani 证明
/// 对象）。原生格式的值总是“最早的字节在最高位”，`_BE` / `_LE` 是这个值的字节序；DoP 为
/// 标记 + 16 位（最早的位在 bit 15），按容器放置。
fn dsd_sample(w: u32, k: usize, t: DsdTransport, dst: &mut [u8]) {
    // 第 k 个 16 位半字（k = 0 为高半字，时间上在前）；第 k 个字节同理。
    let half = (w >> (16 * (1 - (k & 1)))) as u16;
    match t {
        DsdTransport::Native(DsdWord::U32Be) => dst.copy_from_slice(&w.to_be_bytes()),
        DsdTransport::Native(DsdWord::U32Le) => dst.copy_from_slice(&w.to_le_bytes()),
        DsdTransport::Native(DsdWord::U16Be) => dst.copy_from_slice(&half.to_be_bytes()),
        DsdTransport::Native(DsdWord::U16Le) => dst.copy_from_slice(&half.to_le_bytes()),
        DsdTransport::Native(DsdWord::U8) => dst[0] = (w >> (8 * (3 - (k & 3)))) as u8,
        DsdTransport::Dop(c) => {
            let v = (u32::from(DOP_MARKERS[k & 1]) << 16) | u32::from(half);
            let le = dop_bytes(v, c);
            let n = dst.len();
            dst.copy_from_slice(&le[..n]);
        }
    }
}

/// M2 T5：Kani 有界证明（`cargo kani -p qy-pcm-format`）。
///
/// 独立的解包参考：按字节序取回 C 位容器字并符号扩展，按放置方式取出 V 位值，再去掉 V−N 位
/// 的零填充，必须等于原样本；被填充的低位必须为 0（相对幅度不变）。
#[cfg(kani)]
mod proofs {
    use super::*;
    use crate::ChannelLayout;

    /// 从设备字节取回容器字（低 C 位）。
    fn word_of(b: &[u8], endian: Endian) -> u32 {
        let mut w = [0u8; 4];
        match endian {
            Endian::Little => w[..b.len()].copy_from_slice(b),
            Endian::Big => {
                for (k, x) in b.iter().enumerate() {
                    w[b.len() - 1 - k] = *x;
                }
            }
            Endian::HalfWordSwapped => {
                if b.len() == 4 {
                    w = [b[2], b[3], b[0], b[1]];
                } else {
                    w[..b.len()].copy_from_slice(b);
                }
            }
        }
        u32::from_le_bytes(w)
    }

    /// 给定 C、放置方式与字节序：所有 N ≤ V ≤ C 与任意 N 位样本，`placement_shift` +
    /// `place_sample` 不 panic、不溢出，解包后等于原样本，被填充的低位为 0。
    fn roundtrip(c: u8, placement: Placement, endian: Endian) {
        let n: u32 = kani::any();
        let v: u32 = kani::any();
        let c = u32::from(c);
        kani::assume((1..=32).contains(&n) && n <= v && v <= c);
        let s: i32 = kani::any();
        let s = i64::from(s);
        kani::assume(s >= -(1i64 << (n - 1)) && s < (1i64 << (n - 1)));
        let bytes = (c / 8) as usize;
        let mut out = [0u8; 4];
        place_sample(
            s,
            placement_shift(placement, c, v, n),
            endian,
            &mut out[..bytes],
        );
        let w = word_of(&out[..bytes], endian);
        // 符号扩展 C 位容器字。
        let ws = i64::from(((w << (32 - c)) as i32) >> (32 - c));
        let value = match placement {
            // V 位居高位：低 C−V 位为 0。
            Placement::MsbAligned => {
                assert!(ws & ((1i64 << (c - v)) - 1) == 0);
                ws >> (c - v)
            }
            // V 位居低位，高位为符号扩展：值本身在 V 位有符号范围内。
            Placement::LsbAligned => {
                assert!(ws >= -(1i64 << (v - 1)) && ws < (1i64 << (v - 1)));
                ws
            }
        };
        assert!(value & ((1i64 << (v - n)) - 1) == 0);
        assert!(value >> (v - n) == s);
    }

    macro_rules! roundtrip_proofs {
        ($($name:ident: $c:expr, $p:ident, $e:ident;)*) => {$(
            #[kani::proof]
            #[kani::solver(kissat)]
            fn $name() {
                roundtrip($c, Placement::$p, Endian::$e);
            }
        )*};
    }

    // 24 位容器不能半字交换（构造时拒绝），其余组合全部覆盖：3 种容器 × 2 种放置 × 3 种字节序 − 2。
    roundtrip_proofs! {
        c16_msb_le: 16, MsbAligned, Little;
        c16_msb_be: 16, MsbAligned, Big;
        c16_msb_hw: 16, MsbAligned, HalfWordSwapped;
        c16_lsb_le: 16, LsbAligned, Little;
        c16_lsb_be: 16, LsbAligned, Big;
        c16_lsb_hw: 16, LsbAligned, HalfWordSwapped;
        c24_msb_le: 24, MsbAligned, Little;
        c24_msb_be: 24, MsbAligned, Big;
        c24_lsb_le: 24, LsbAligned, Little;
        c24_lsb_be: 24, LsbAligned, Big;
        c32_msb_le: 32, MsbAligned, Little;
        c32_msb_be: 32, MsbAligned, Big;
        c32_msb_hw: 32, MsbAligned, HalfWordSwapped;
        c32_lsb_le: 32, LsbAligned, Little;
        c32_lsb_be: 32, LsbAligned, Big;
        c32_lsb_hw: 32, LsbAligned, HalfWordSwapped;
    }

    /// 24 位容器的半字交换在构造时拒绝（pack 因此不必处理这种组合）。
    #[kani::proof]
    fn c24_half_word_swap_rejected() {
        let v: u8 = kani::any();
        let p = if kani::any() {
            Placement::MsbAligned
        } else {
            Placement::LsbAligned
        };
        let r = DeviceFormat::with_layout(
            48_000,
            ChannelLayout::MONO,
            24,
            v,
            Endian::HalfWordSwapped,
            p,
        );
        assert!(r.is_err());
    }

    /// D1：任意 DSD 字、任意设备帧序号 k（0..e）：`dsd_sample` 写出的字节按位取回后，正好是该字
    /// 时间顺序上第 k 段（原生 U32 / U16 / U8 为第 k 个 4 / 2 / 1 字节段，DoP 为第 k 个 16 位段），
    /// 字节序与 DoP 标记符合规范；参考按位逐个比较，不用被测的移位。
    fn dsd_roundtrip(t: DsdTransport) {
        let w: u32 = kani::any();
        let k: usize = kani::any();
        let e = t.expansion() as usize;
        kani::assume(k < e);
        let n = usize::from(t.sample_bytes());
        let mut out = [0u8; 4];
        dsd_sample(w, k, t, &mut out[..n]);
        // 时间顺序的第 i 位（i = 0 最早）。
        let bit = |i: usize| (w >> (31 - i)) & 1;
        let payload = 32 / e; // 每设备帧每声道的 DSD 位数
        // 取出设备样本中按时间顺序排列的 DSD 字节（最早的在前）。
        let mut bytes = [0u8; 4];
        let count = payload / 8;
        match t {
            DsdTransport::Native(DsdWord::U32Be | DsdWord::U16Be | DsdWord::U8) => {
                bytes[..count].copy_from_slice(&out[..count]);
            }
            DsdTransport::Native(DsdWord::U32Le | DsdWord::U16Le) => {
                for j in 0..count {
                    bytes[j] = out[count - 1 - j];
                }
            }
            DsdTransport::Dop(c) => {
                // 小端容器中 24 位值的三个字节：低（较晚）、中（较早）、标记。
                let base = if matches!(c, crate::DopContainer::S32LeMsb) {
                    1
                } else {
                    0
                };
                bytes[0] = out[base + 1];
                bytes[1] = out[base];
                assert!(out[base + 2] == DOP_MARKERS[k]);
                match c {
                    crate::DopContainer::S32LeMsb => assert!(out[0] == 0),
                    crate::DopContainer::S24LeLsb => {
                        assert!(out[3] == if DOP_MARKERS[k] & 0x80 != 0 { 0xFF } else { 0 });
                    }
                    crate::DopContainer::S24_3Le => {}
                }
            }
        }
        for j in 0..payload {
            let got = (bytes[j / 8] >> (7 - j % 8)) & 1;
            assert!(u32::from(got) == bit(k * payload + j));
        }
    }

    macro_rules! dsd_proofs {
        ($($name:ident: $t:expr;)*) => {
            $(
                #[kani::proof]
                fn $name() {
                    dsd_roundtrip($t);
                }
            )*
        };
    }

    dsd_proofs! {
        dsd_u8: DsdTransport::Native(DsdWord::U8);
        dsd_u16_le: DsdTransport::Native(DsdWord::U16Le);
        dsd_u16_be: DsdTransport::Native(DsdWord::U16Be);
        dsd_u32_le: DsdTransport::Native(DsdWord::U32Le);
        dsd_u32_be: DsdTransport::Native(DsdWord::U32Be);
        dop_s24_3le: DsdTransport::Dop(crate::DopContainer::S24_3Le);
        dop_s32le: DsdTransport::Dop(crate::DopContainer::S32LeMsb);
        dop_s24le: DsdTransport::Dop(crate::DopContainer::S24LeLsb);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(rate: u32, ch: u16, bits: u8) -> SourceFormat {
        match SourceFormat::new(rate, ch, bits) {
            Ok(f) => f,
            Err(e) => panic!("{e:?}"),
        }
    }

    fn dev(ch: u16, c: u8, v: u8, e: Endian, p: Placement) -> DeviceFormat {
        match DeviceFormat::new(48_000, ch, c, v, e, p) {
            Ok(f) => f,
            Err(e) => panic!("{e:?}"),
        }
    }

    fn pack_vec(
        samples: &[i32],
        ch: u16,
        n: u8,
        d: &DeviceFormat,
    ) -> Result<([u8; 64], usize), PackError> {
        let pcm = Pcm::exact(samples, src(48_000, ch, n))?;
        let mut out = [0u8; 64];
        let frames = pack(&pcm, d, &mut out)?;
        Ok((out, frames * usize::from(d.block_align())))
    }

    /// 低位放置（ALSA `S24_LE` 等）且 N < V：样本乘 2^(V−N) 后放在容器低 V 位，高位为符号扩展。
    /// 期望字节手写（变异测试发现此前只有 N = V 的低位放置用例）。
    #[test]
    fn lsb_aligned_with_fewer_source_bits() {
        // 16 位样本 → 32 位容器、低 24 位有效：×256，高 8 位为符号。
        let d = dev(1, 32, 24, Endian::Little, Placement::LsbAligned);
        let Ok((out, len)) = pack_vec(&[0x7FFF, -0x8000, -1, 1], 1, 16, &d) else {
            panic!()
        };
        assert_eq!(
            &out[..len],
            &[
                0x00, 0xFF, 0x7F, 0x00, // 0x7FFF00
                0x00, 0x00, 0x80, 0xFF, // −0x800000
                0x00, 0xFF, 0xFF, 0xFF, // −256
                0x00, 0x01, 0x00, 0x00, // 256
            ]
        );
        // 12 位样本 → 16 位容器、低 14 位有效（大端）：×4，高 2 位为符号。
        let d = dev(1, 16, 14, Endian::Big, Placement::LsbAligned);
        let Ok((out, len)) = pack_vec(&[0x7FF, -0x800, -1], 1, 12, &d) else {
            panic!()
        };
        assert_eq!(&out[..len], &[0x1F, 0xFC, 0xE0, 0x00, 0xFF, 0xFC]);
    }

    /// (样本, 声道, N, 设备格式, 期望字节)
    type Case = (&'static [i32], u16, u8, DeviceFormat, &'static [u8]);

    /// 独立手写的期望字节。
    #[test]
    fn independent_expected_bytes() {
        use Endian::*;
        use Placement::*;
        let cases: &[Case] = &[
            // 16/16 立体声 LE
            (
                &[0x1234, -2],
                2,
                16,
                dev(2, 16, 16, Little, MsbAligned),
                &[0x34, 0x12, 0xFE, 0xFF],
            ),
            // N=16 → WASAPI 32/24：左移 16
            (
                &[0x1234, -2],
                2,
                16,
                dev(2, 32, 24, Little, MsbAligned),
                &[0, 0, 0x34, 0x12, 0, 0, 0xFE, 0xFF],
            ),
            // N=24 → 32/24
            (
                &[0x123456, -8_388_608],
                2,
                24,
                dev(2, 32, 24, Little, MsbAligned),
                &[0, 0x56, 0x34, 0x12, 0, 0, 0, 0x80],
            ),
            // N=24 → 24/24
            (
                &[0x123456, -1],
                2,
                24,
                dev(2, 24, 24, Little, MsbAligned),
                &[0x56, 0x34, 0x12, 0xFF, 0xFF, 0xFF],
            ),
            // N=16 → 24/24 单声道
            (
                &[0x1234, -32_768],
                1,
                16,
                dev(1, 24, 24, Little, MsbAligned),
                &[0, 0x34, 0x12, 0, 0, 0x80],
            ),
            // N=32 → 32/32
            (
                &[i32::MIN, i32::MAX],
                2,
                32,
                dev(2, 32, 32, Little, MsbAligned),
                &[0, 0, 0, 0x80, 0xFF, 0xFF, 0xFF, 0x7F],
            ),
            // N=20 → 32/24 单声道：左移 12
            (
                &[0x7FFFF],
                1,
                20,
                dev(1, 32, 24, Little, MsbAligned),
                &[0, 0xF0, 0xFF, 0x7F],
            ),
            // ALSA S24_LE（32 容器低 24 位，符号扩展）：N=16 左移 8；N=24 原值
            (
                &[0x1234],
                1,
                16,
                dev(1, 32, 24, Little, LsbAligned),
                &[0, 0x34, 0x12, 0],
            ),
            (
                &[-1],
                1,
                24,
                dev(1, 32, 24, Little, LsbAligned),
                &[0xFF, 0xFF, 0xFF, 0xFF],
            ),
            (
                &[-8_388_608],
                1,
                24,
                dev(1, 32, 24, Little, LsbAligned),
                &[0, 0, 0x80, 0xFF],
            ),
            (
                &[8_388_607],
                1,
                24,
                dev(1, 32, 24, Little, LsbAligned),
                &[0xFF, 0xFF, 0x7F, 0],
            ),
            // 大端
            (
                &[0x1234],
                1,
                16,
                dev(1, 16, 16, Big, MsbAligned),
                &[0x12, 0x34],
            ),
            (
                &[0x123456],
                1,
                24,
                dev(1, 32, 24, Big, MsbAligned),
                &[0x12, 0x34, 0x56, 0],
            ),
        ];
        for (i, (s, ch, n, d, want)) in cases.iter().enumerate() {
            let r = pack_vec(s, *ch, *n, d);
            let Ok((out, len)) = r else {
                panic!("case {i}: {r:?}")
            };
            assert_eq!(&out[..len], *want, "case {i}");
        }
    }

    #[test]
    fn rejects_precision_mismatch_and_overflow() {
        let d = dev(2, 32, 24, Endian::Little, Placement::MsbAligned);
        assert_eq!(
            pack_vec(&[0, 0], 2, 32, &d).map(|r| r.1),
            Err(PackError::Precision {
                source_bits: 32,
                valid_bits: 24
            })
        );
        let d16 = dev(2, 16, 16, Endian::Little, Placement::MsbAligned);
        assert_eq!(
            pack_vec(&[0, 0], 2, 24, &d16).map(|r| r.1),
            Err(PackError::Precision {
                source_bits: 24,
                valid_bits: 16
            })
        );
        assert_eq!(
            pack_vec(&[0], 1, 16, &d16).map(|r| r.1),
            Err(PackError::FormatMismatch)
        );
        assert_eq!(
            pack_vec(&[0, 0, 0], 2, 16, &d16).map(|r| r.1),
            Err(PackError::PartialFrame)
        );
        assert_eq!(
            pack_vec(&[0, 40_000], 2, 16, &d16).map(|r| r.1),
            Err(PackError::SampleOutOfRange { index: 1 })
        );
        assert_eq!(
            pack_vec(&[-32_769, 0], 2, 16, &d16).map(|r| r.1),
            Err(PackError::SampleOutOfRange { index: 0 })
        );
        // 恰在 N 位范围边界：−2^(N−1) 与 2^(N−1) − 1 接受，再大 1 拒绝（M2 T5）。
        assert_eq!(
            pack_vec(&[-32_768, 32_767], 2, 16, &d16)
                .map(|r| (r.1, [r.0[0], r.0[1], r.0[2], r.0[3]])),
            Ok((4, [0x00, 0x80, 0xFF, 0x7F]))
        );
        assert_eq!(
            pack_vec(&[0, 32_768], 2, 16, &d16).map(|r| r.1),
            Err(PackError::SampleOutOfRange { index: 1 })
        );
        let Ok(rate) = DeviceFormat::wasapi(44_100, 2, 16, 16) else {
            panic!()
        };
        assert_eq!(
            pack_vec(&[0, 0], 2, 16, &rate).map(|r| r.1),
            Err(PackError::FormatMismatch)
        );
    }

    #[test]
    fn packs_only_whole_frames_that_fit() {
        let d = dev(2, 24, 24, Endian::Little, Placement::MsbAligned);
        let s = [1, 2, 3, 4, 5, 6];
        let Ok(pcm) = Pcm::exact(&s, src(48_000, 2, 24)) else {
            panic!()
        };
        let mut out = [0u8; 13];
        assert_eq!(pack(&pcm, &d, &mut out), Ok(2));
        assert_eq!(&out[..12], &[1, 0, 0, 2, 0, 0, 3, 0, 0, 4, 0, 0]);
    }

    /// 测试专用逆变换（独立于 pack 的实现方式：逐字节组装再算术右移）。
    fn unpack(bytes: &[u8], n: u32, d: &DeviceFormat) -> i32 {
        let c = u32::from(d.container_bits());
        let v = u32::from(d.valid_bits());
        let mut word: u32 = 0;
        for k in 0..bytes.len() {
            let b = match d.endian() {
                Endian::Little => bytes[k],
                Endian::Big => bytes[bytes.len() - 1 - k],
                // 第 k 个有效字节（从低到高）：低半字在后两个字节。
                Endian::HalfWordSwapped if bytes.len() == 4 => bytes[(k + 2) % 4],
                Endian::HalfWordSwapped => bytes[k],
            };
            word |= u32::from(b) << (8 * k);
        }
        // 先移到 32 位顶端再算术右移，恢复符号。
        let top = (word << (32 - c)) as i32;
        match d.placement() {
            Placement::MsbAligned => {
                assert_eq!(
                    word & ((1u64 << (c - v)) as u32).wrapping_sub(1),
                    0,
                    "低填充位必须为 0"
                );
                top >> (32 - n)
            }
            Placement::LsbAligned => {
                let val = top >> (32 - c);
                assert_eq!(val & ((1i64 << (v - n)) - 1) as i32, 0);
                val >> (v - n)
            }
        }
    }

    /// 高半字在前（STM32 SPI/I²S 16 位 DMA）：独立参考字节。
    #[test]
    fn half_word_swapped_bytes() {
        // 32/32：0x1234_5678 → 34 12 78 56；−1 → ff ff ff ff；最小值 → 00 80 00 00。
        let d = dev(1, 32, 32, Endian::HalfWordSwapped, Placement::MsbAligned);
        let s = [0x1234_5678, -1, i32::MIN];
        let Ok((out, len)) = pack_vec(&s, 1, 32, &d) else {
            panic!()
        };
        assert_eq!(
            &out[..len],
            &[
                0x34, 0x12, 0x78, 0x56, 0xff, 0xff, 0xff, 0xff, 0x00, 0x80, 0x00, 0x00
            ]
        );
        // 32/24 左对齐：24 位 0x12_3456 → 字 0x1234_5600 → 34 12 00 56。
        let d = dev(1, 32, 24, Endian::HalfWordSwapped, Placement::MsbAligned);
        let Ok((out, len)) = pack_vec(&[0x12_3456], 1, 24, &d) else {
            panic!()
        };
        assert_eq!(&out[..len], &[0x34, 0x12, 0x00, 0x56]);
        // 16/16 与小端相同。
        let d = dev(1, 16, 16, Endian::HalfWordSwapped, Placement::MsbAligned);
        let Ok((out, len)) = pack_vec(&[0x1234], 1, 16, &d) else {
            panic!()
        };
        assert_eq!(&out[..len], &[0x34, 0x12]);
        // 24 位容器无此表示。
        assert_eq!(
            DeviceFormat::new(
                48_000,
                1,
                24,
                24,
                Endian::HalfWordSwapped,
                Placement::MsbAligned
            ),
            Err(crate::FormatError::Layout)
        );
    }

    #[test]
    fn roundtrip_preserves_value_and_relative_amplitude() {
        let mut seed = 0x1234_5678_9ABC_DEF0u64;
        for &(c, v) in &[
            (16u8, 16u8),
            (24, 24),
            (32, 24),
            (32, 32),
            (32, 20),
            (24, 20),
        ] {
            for placement in [Placement::MsbAligned, Placement::LsbAligned] {
                for endian in [Endian::Little, Endian::Big, Endian::HalfWordSwapped] {
                    if endian == Endian::HalfWordSwapped && c == 24 {
                        continue;
                    }
                    for ch in [1u16, 2] {
                        let d = dev(ch, c, v, endian, placement);
                        for n in 1..=v {
                            let lo = -(1i64 << (n - 1));
                            let hi = (1i64 << (n - 1)) - 1;
                            let mut s = [0i32; 16];
                            s[0] = lo as i32;
                            s[1] = hi as i32;
                            for x in s.iter_mut().skip(2) {
                                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                                *x = (lo + ((seed >> 20) as i64).rem_euclid(hi - lo + 1)) as i32;
                            }
                            let r = pack_vec(&s, ch, n, &d);
                            let Ok((out, len)) = r else { panic!("{r:?}") };
                            let b = usize::from(c / 8);
                            assert_eq!(len, 16 * b);
                            for (i, chunk) in out[..len].chunks_exact(b).enumerate() {
                                assert_eq!(
                                    unpack(chunk, u32::from(n), &d),
                                    s[i],
                                    "c={c} v={v} n={n} i={i}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// F1：5.1 源（WAVE 顺序 FL FR FC LFE BL BR）送到 HDA 顺序（FL FR BL BR FC LFE）的
    /// ALSA S24_LE 设备：每个样本值原样放入同一扬声器的设备声道（纯置换），期望字节独立写出。
    #[test]
    fn permutes_channels_by_speaker_position() {
        use crate::{ChannelLayout, Speaker::*};
        let (Some(wave51), Some(hda)) = (
            ChannelLayout::from_mask(0x3F),
            ChannelLayout::from_positions(&[
                FrontLeft,
                FrontRight,
                BackLeft,
                BackRight,
                FrontCenter,
                LowFrequency,
            ]),
        ) else {
            panic!("layouts");
        };
        let (Ok(src), Ok(dev)) = (
            SourceFormat::with_layout(48_000, wave51, 16),
            DeviceFormat::with_layout(48_000, hda, 32, 24, Endian::Little, Placement::LsbAligned),
        ) else {
            panic!("formats");
        };
        // 两帧：声道值即“帧号 × 16 + 声道编号”，负值覆盖符号扩展。
        let samples = [1, 2, 3, 4, 5, 6, -17, -18, -19, -20, -21, -22];
        let Ok(pcm) = Pcm::exact(&samples, src) else {
            panic!("pcm");
        };
        let mut out = [0u8; 48];
        assert_eq!(pack(&pcm, &dev, &mut out), Ok(2));
        // S24_LE（LsbAligned，C32/V24，N16）：s << 8，按设备顺序 FL FR BL BR FC LFE。
        let order = [0usize, 1, 4, 5, 2, 3];
        let mut expect = [0u8; 48];
        for f in 0..2 {
            for (j, &from) in order.iter().enumerate() {
                let v = (samples[f * 6 + from] << 8) as u32;
                expect[(f * 6 + j) * 4..(f * 6 + j) * 4 + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        assert_eq!(out, expect);
        // 位置集合不同（5.1 侧环绕）：拒绝。
        let Some(side) = ChannelLayout::from_mask(0x60F) else {
            panic!("side");
        };
        let Ok(dev_side) =
            DeviceFormat::with_layout(48_000, side, 32, 24, Endian::Little, Placement::LsbAligned)
        else {
            panic!("dev");
        };
        assert_eq!(
            pack(&pcm, &dev_side, &mut out),
            Err(PackError::FormatMismatch)
        );
        // 负向越界同样拒绝（−2^(N−1) − 1）。
        let low = [1, 2, 3, 4, 5, 6, 7, 8, -32_769, 10, 11, 12];
        let Ok(pcm) = Pcm::exact(&low, src) else {
            panic!("pcm");
        };
        assert_eq!(
            pack(&pcm, &dev, &mut out),
            Err(PackError::SampleOutOfRange { index: 8 })
        );
        // 超范围样本报告的是源样本下标（第 2 帧的 FC = 下标 8）。
        let bad = [1, 2, 3, 4, 5, 6, 7, 8, 40_000, 10, 11, 12];
        let Ok(pcm) = Pcm::exact(&bad, src) else {
            panic!("pcm");
        };
        assert_eq!(
            pack(&pcm, &dev, &mut out),
            Err(PackError::SampleOutOfRange { index: 8 })
        );
    }

    /// F3：浮点按位复制。binary32 / binary64、小端与大端、声道按位置置换、单声道复制；位模式（含
    /// NaN、−0、次正规数）原样写出，期望字节独立写出。编码不同一律拒绝。
    #[test]
    fn float_samples_copied_bit_exact() {
        use crate::{ChannelLayout, SampleEncoding::*, Speaker::*};
        let stereo = ChannelLayout::STEREO;
        let Ok(src32) = SourceFormat::float(48_000, stereo, Float32) else {
            panic!()
        };
        let x: [u32; 4] = [0x3F00_0000, 0x7FC0_0001, 0x8000_0000, 0x0000_0001];
        let w = x.map(|b| b as i32);
        let Ok(pcm) = Pcm::exact(&w, src32) else {
            panic!()
        };
        let Ok(le) = DeviceFormat::float_layout(48_000, stereo, Float32, Endian::Little) else {
            panic!()
        };
        let mut out = [0u8; 16];
        assert_eq!(pack(&pcm, &le, &mut out), Ok(2));
        let want: std::vec::Vec<u8> = x.iter().flat_map(|b| b.to_le_bytes()).collect();
        assert_eq!(&out[..], &want[..]);
        let Ok(be) = DeviceFormat::float_layout(48_000, stereo, Float32, Endian::Big) else {
            panic!()
        };
        assert_eq!(pack(&pcm, &be, &mut out), Ok(2));
        let want: std::vec::Vec<u8> = x.iter().flat_map(|b| b.to_be_bytes()).collect();
        assert_eq!(&out[..], &want[..]);
        // binary64：每样本两个字（低、高）；左右声道顺序反过来的设备（[FR FL] 不是掩码顺序，
        // 用通用构造）按位置置换。
        let Ok(src64) = SourceFormat::float(48_000, stereo, Float64) else {
            panic!()
        };
        let y: [u64; 2] = [(-0.75f64).to_bits(), 0x7FF0_0000_0000_0001];
        let w = [
            y[0] as u32 as i32,
            (y[0] >> 32) as u32 as i32,
            y[1] as u32 as i32,
            (y[1] >> 32) as u32 as i32,
        ];
        let Ok(pcm) = Pcm::exact(&w, src64) else {
            panic!()
        };
        assert_eq!(pcm.frames(), 1);
        let Some(rl) = ChannelLayout::from_positions(&[FrontRight, FrontLeft]) else {
            panic!()
        };
        let Ok(dev) = DeviceFormat::float_layout(48_000, rl, Float64, Endian::Little) else {
            panic!()
        };
        assert_eq!(dev.block_align(), 16);
        let mut out = [0u8; 16];
        assert_eq!(pack(&pcm, &dev, &mut out), Ok(1));
        let want: std::vec::Vec<u8> = [y[1], y[0]].iter().flat_map(|b| b.to_le_bytes()).collect();
        assert_eq!(&out[..], &want[..]);
        // 单声道复制（O1）同样按位复制。
        let Ok(mono) = SourceFormat::float(48_000, ChannelLayout::MONO, Float32) else {
            panic!()
        };
        let Ok(pcm) = Pcm::exact(&[0x3E80_0000u32 as i32], mono) else {
            panic!()
        };
        let mut out = [0u8; 8];
        assert_eq!(
            pack_with(&pcm, &le, ChannelMode::DuplicateMono, &mut out),
            Ok(1)
        );
        assert_eq!(out, [0, 0, 0x80, 0x3E, 0, 0, 0x80, 0x3E]);
        // 编码不同：浮点不放入整数格式，整数不放入浮点格式，binary32 不放入 binary64。
        let Ok(int_dev) = DeviceFormat::wasapi(48_000, 2, 32, 32) else {
            panic!()
        };
        let Ok(pcm32) = Pcm::exact(&[0, 0], src32) else {
            panic!()
        };
        let mut out = [0u8; 16];
        assert_eq!(pack(&pcm32, &int_dev, &mut out), Err(PackError::Encoding));
        // [FR FL] 与 [FL FR] 位置相同，不同的是编码。
        assert_eq!(pack(&pcm32, &dev, &mut out), Err(PackError::Encoding));
        let Ok(dev64) = DeviceFormat::float_layout(48_000, stereo, Float64, Endian::Little) else {
            panic!()
        };
        assert_eq!(pack(&pcm32, &dev64, &mut out), Err(PackError::Encoding));
        let Ok(int_src) = Pcm::exact(&[0, 0], src(48_000, 2, 16)) else {
            panic!()
        };
        assert_eq!(pack(&int_src, &le, &mut out), Err(PackError::Encoding));
        // binary64 半帧（奇数个字）不是完整帧。
        assert_eq!(
            Pcm::exact(&[0, 0, 0], src64).map(|p| p.frames()),
            Err(PackError::PartialFrame)
        );
        // 浮点设备格式不能半字交换。
        assert_eq!(
            DeviceFormat::float_layout(48_000, stereo, Float32, Endian::HalfWordSwapped),
            Err(crate::FormatError::Layout)
        );
    }

    /// O1：单声道复制到左右两路。样本值不变：每帧两份相同的容器字，与独立参考逐字节一致；
    /// 只接受 [FC] → [FL FR]，其余布局与精度不足时拒绝；严格模式下同样的组合仍按格式不符拒绝。
    #[test]
    fn duplicate_mono_copies_each_sample_to_both_channels() {
        let mono = src(48_000, 1, 16);
        let samples = [32_767, -32_768, 0, -1, 1, 12_345];
        let Ok(pcm) = Pcm::exact(&samples, mono) else {
            panic!()
        };
        let Ok(d) = DeviceFormat::wasapi(48_000, 2, 32, 24) else {
            panic!()
        };
        let mut out = [0u8; 48];
        assert_eq!(
            pack_with(&pcm, &d, ChannelMode::DuplicateMono, &mut out),
            Ok(6)
        );
        // 独立参考：16 位样本左对齐放入 32 位容器（左移 16），左右各一份。
        let mut want = [0u8; 48];
        for (k, &x) in samples.iter().enumerate() {
            let w = ((x as u32) << 16).to_le_bytes();
            want[k * 8..k * 8 + 4].copy_from_slice(&w);
            want[k * 8 + 4..k * 8 + 8].copy_from_slice(&w);
        }
        assert_eq!(&out[..], &want[..]);
        // 严格模式不接受声道数不同。
        assert_eq!(pack(&pcm, &d, &mut out), Err(PackError::FormatMismatch));
        // 源不是 [FC]（如只占左声道位置的单声道）、源为立体声、设备不是 [FL FR]：拒绝。
        let Some(left) = crate::ChannelLayout::from_mask(0x1) else {
            panic!()
        };
        let Ok(left_mono) = SourceFormat::with_layout(48_000, left, 16) else {
            panic!()
        };
        assert_eq!(
            check_mode(&left_mono, &d, ChannelMode::DuplicateMono),
            Err(PackError::FormatMismatch)
        );
        assert_eq!(
            check_mode(&src(48_000, 2, 16), &d, ChannelMode::DuplicateMono),
            Err(PackError::FormatMismatch)
        );
        let Ok(mono_dev) = DeviceFormat::wasapi(48_000, 1, 16, 16) else {
            panic!()
        };
        assert_eq!(
            check_mode(&mono, &mono_dev, ChannelMode::DuplicateMono),
            Err(PackError::FormatMismatch)
        );
        // 采样率不同、精度不足：拒绝（不重采样、不降精度）。
        let Ok(d44) = DeviceFormat::wasapi(44_100, 2, 32, 24) else {
            panic!()
        };
        assert_eq!(
            check_mode(&mono, &d44, ChannelMode::DuplicateMono),
            Err(PackError::FormatMismatch)
        );
        let Ok(d16) = DeviceFormat::wasapi(48_000, 2, 16, 16) else {
            panic!()
        };
        assert_eq!(
            check_mode(&src(48_000, 1, 24), &d16, ChannelMode::DuplicateMono),
            Err(PackError::Precision {
                source_bits: 24,
                valid_bits: 16
            })
        );
        // N = V 时接受（精度刚好够）。
        let Ok(d16_16) = DeviceFormat::wasapi(48_000, 2, 16, 16) else {
            panic!()
        };
        assert_eq!(
            check_mode(&mono, &d16_16, ChannelMode::DuplicateMono),
            Ok(())
        );
        // 输出容量只够 2 帧时只 pack 2 帧。
        let mut small = [0u8; 16];
        assert_eq!(
            pack_with(&pcm, &d, ChannelMode::DuplicateMono, &mut small),
            Ok(2)
        );
        assert_eq!(&small[..], &want[..16]);
    }
}
