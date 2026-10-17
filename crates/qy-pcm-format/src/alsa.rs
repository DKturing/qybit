//! ALSA 格式映射、设备名检查与已应用参数确认（M2 T2，纯逻辑）。
//!
//! 不依赖 alsa-lib、不调用系统 API，`no_std`、无 alloc：后端（M2 T3）用 alsa-lib 设置与读回
//! 参数后，把读回值交给 [`confirm`] 核对；不一致就明确报错，不接受近似值。
//!
//! 出处（alsa-lib `include/pcm.h`，取自 alsa-project/alsa-lib master）：
//! - `snd_pcm_format_t`：`SND_PCM_FORMAT_S16_LE` = 2；`SND_PCM_FORMAT_S24_LE` = 6（“Signed
//!   24 bit Little Endian using low three bytes in 32-bit word”）；`SND_PCM_FORMAT_S32_LE` = 10；
//!   `SND_PCM_FORMAT_S24_3LE` = 32（“Signed 24bit Little Endian in 3bytes format”）；
//!   `SND_PCM_FORMAT_FLOAT_LE` = 14、`FLOAT_BE` = 15、`FLOAT64_LE` = 16、`FLOAT64_BE` = 17
//!   （“Float 32/64 bit Little/Big Endian, Range -1.0 to 1.0”，F3）。
//! - `SND_PCM_ACCESS_RW_INTERLEAVED`；`snd_pcm_hw_params_set_rate_resample` /
//!   `snd_pcm_hw_params_get_rate_resample`；`snd_pcm_hw_params_current`；
//!   `snd_pcm_hw_params_get_rate_numden`（精确有理数采样率）；`snd_pcm_hw_params_get_sbits`
//!   （设备实际使用的有效位数）。
//!
//! `S24_LE` 的 24 位在 32 位字的**低**三字节（[`Placement::LsbAligned`]，pack 符号扩展高字节），
//! 与 WASAPI 32/24（有效位在**高** 24 位，低字节补零）不同，二者不得混用。

use core::fmt;

use crate::{
    ChannelLayout, DeviceFormat, Endian, FormatError, MAX_LAYOUT_CHANNELS, Placement,
    SampleEncoding, SourceFormat, Speaker,
};

/// 支持的 ALSA 样本格式（小端有符号整数；F3 起另有小端 IEEE 754 浮点）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlsaFormat {
    /// `S16_LE`：C16 / V16。
    S16Le,
    /// `S24_3LE`：C24 / V24（3 字节）。
    S24_3Le,
    /// `S24_LE`：C32 / V24，有效位在低 24 位，高字节符号扩展。
    S24Le,
    /// `S32_LE`：C32 / V32。
    S32Le,
    /// `FLOAT_LE`：binary32（F3）。
    FloatLe,
    /// `FLOAT64_LE`：binary64（F3）。
    Float64Le,
}

/// 不支持的 ALSA 格式类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatRejection {
    /// 大端格式（如 `S16_BE`）。
    BigEndian,
    /// 浮点格式（如 `FLOAT_LE`）。
    Float,
    /// 无符号整数格式（如 `U16_LE`）。
    Unsigned,
    /// 其他未支持的格式（8 位、20 位、压缩、IEC958 等）或未知名称/代码。
    Other,
    /// 设备格式没有对应的 ALSA 格式（如 WASAPI 32/24 高位对齐、非小端）。
    NoAlsaEquivalent,
}

impl AlsaFormat {
    /// 全部支持的整数格式（设备格式矩阵按这些格式测试）。
    pub const ALL: [AlsaFormat; 4] = [Self::S16Le, Self::S24_3Le, Self::S24Le, Self::S32Le];

    /// 查询设备时测试的全部格式：整数与浮点（F3）。
    pub const QUERY: [AlsaFormat; 6] = [
        Self::S16Le,
        Self::S24_3Le,
        Self::S24Le,
        Self::S32Le,
        Self::FloatLe,
        Self::Float64Le,
    ];

    /// 样本编码。
    pub const fn encoding(self) -> SampleEncoding {
        match self {
            Self::FloatLe => SampleEncoding::Float32,
            Self::Float64Le => SampleEncoding::Float64,
            _ => SampleEncoding::Int,
        }
    }

    /// alsa-lib 中的格式名。
    pub const fn name(self) -> &'static str {
        match self {
            Self::S16Le => "S16_LE",
            Self::S24_3Le => "S24_3LE",
            Self::S24Le => "S24_LE",
            Self::S32Le => "S32_LE",
            Self::FloatLe => "FLOAT_LE",
            Self::Float64Le => "FLOAT64_LE",
        }
    }

    /// `snd_pcm_format_t` 数值。
    pub const fn code(self) -> i32 {
        match self {
            Self::S16Le => 2,
            Self::S24Le => 6,
            Self::S32Le => 10,
            Self::S24_3Le => 32,
            Self::FloatLe => 14,
            Self::Float64Le => 16,
        }
    }

    /// 容器位宽 C。
    pub const fn container_bits(self) -> u8 {
        match self {
            Self::S16Le => 16,
            Self::S24_3Le => 24,
            Self::S24Le | Self::S32Le | Self::FloatLe => 32,
            Self::Float64Le => 64,
        }
    }

    /// 有效位数 V。
    pub const fn valid_bits(self) -> u8 {
        match self {
            Self::S16Le => 16,
            Self::S24_3Le | Self::S24Le => 24,
            Self::S32Le | Self::FloatLe => 32,
            Self::Float64Le => 64,
        }
    }

    /// 有效位放置方式（C = V 时两种放置等价，统一记为高位对齐）。
    pub const fn placement(self) -> Placement {
        match self {
            Self::S24Le => Placement::LsbAligned,
            _ => Placement::MsbAligned,
        }
    }

    /// 本格式、指定设备声道布局（设备上的样本顺序）下的设备格式。
    pub const fn device_format_layout(
        self,
        sample_rate: u32,
        layout: ChannelLayout,
    ) -> Result<DeviceFormat, FormatError> {
        if self.encoding().is_float() {
            return DeviceFormat::float_layout(
                sample_rate,
                layout,
                self.encoding(),
                Endian::Little,
            );
        }
        DeviceFormat::with_layout(
            sample_rate,
            layout,
            self.container_bits(),
            self.valid_bits(),
            Endian::Little,
            self.placement(),
        )
    }

    /// 本格式下的单/双声道设备格式（默认布局）。
    pub const fn device_format(
        self,
        sample_rate: u32,
        channels: u16,
    ) -> Result<DeviceFormat, FormatError> {
        if self.encoding().is_float() {
            return match ChannelLayout::default_for(channels) {
                Some(l) => self.device_format_layout(sample_rate, l),
                None => Err(FormatError::UndefinedLayout),
            };
        }
        DeviceFormat::new(
            sample_rate,
            channels,
            self.container_bits(),
            self.valid_bits(),
            Endian::Little,
            self.placement(),
        )
    }

    /// 由 `snd_pcm_format_t` 数值识别。
    pub const fn from_code(code: i32) -> Result<Self, FormatRejection> {
        match code {
            2 => Ok(Self::S16Le),
            6 => Ok(Self::S24Le),
            10 => Ok(Self::S32Le),
            32 => Ok(Self::S24_3Le),
            14 => Ok(Self::FloatLe),
            16 => Ok(Self::Float64Le),
            // S16_BE S24_BE S32_BE S20_BE S24_3BE S20_3BE S18_3BE FLOAT_BE FLOAT64_BE
            3 | 7 | 11 | 15 | 17 | 26 | 33 | 37 | 41 => Err(FormatRejection::BigEndian),
            // U8 U16 U24 U32 U20 U24_3 U20_3 U18_3
            1 | 4 | 5 | 8 | 9 | 12 | 13 | 27 | 28 | 34 | 35 | 38 | 39 | 42 | 43 => {
                Err(FormatRejection::Unsigned)
            }
            _ => Err(FormatRejection::Other),
        }
    }

    /// 由 alsa-lib 格式名识别。
    pub fn from_name(name: &str) -> Result<Self, FormatRejection> {
        match name {
            "S16_LE" => Ok(Self::S16Le),
            "S24_3LE" => Ok(Self::S24_3Le),
            "S24_LE" => Ok(Self::S24Le),
            "S32_LE" => Ok(Self::S32Le),
            "FLOAT_LE" => Ok(Self::FloatLe),
            "FLOAT64_LE" => Ok(Self::Float64Le),
            n if n.starts_with("FLOAT") && n.ends_with("BE") => Err(FormatRejection::BigEndian),
            n if n.starts_with("FLOAT") => Err(FormatRejection::Float),
            n if n.ends_with("BE") => Err(FormatRejection::BigEndian),
            n if n.starts_with('U') => Err(FormatRejection::Unsigned),
            _ => Err(FormatRejection::Other),
        }
    }

    /// 设备格式对应的 ALSA 格式。只认小端；C = 32、V = 24 时只认低位对齐（`S24_LE`），
    /// WASAPI 式高位对齐没有对应格式。
    pub const fn from_device(f: &DeviceFormat) -> Result<Self, FormatRejection> {
        if !matches!(f.endian(), Endian::Little) {
            return Err(FormatRejection::BigEndian);
        }
        match f.encoding() {
            SampleEncoding::Float32 => return Ok(Self::FloatLe),
            SampleEncoding::Float64 => return Ok(Self::Float64Le),
            SampleEncoding::Int => {}
        }
        match (f.container_bits(), f.valid_bits(), f.placement()) {
            (16, 16, _) => Ok(Self::S16Le),
            (24, 24, _) => Ok(Self::S24_3Le),
            (32, 32, _) => Ok(Self::S32Le),
            (32, 24, Placement::LsbAligned) => Ok(Self::S24Le),
            _ => Err(FormatRejection::NoAlsaEquivalent),
        }
    }
}

impl fmt::Display for AlsaFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// 候选格式列表（固定容量，不分配）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidates {
    items: [Option<AlsaFormat>; 4],
    len: usize,
}

impl Candidates {
    pub fn as_slice(&self) -> impl Iterator<Item = AlsaFormat> + '_ {
        self.items[..self.len].iter().flatten().copied()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// 候选格式：只取 V ≥ N（不改变样本）的整数格式；按 V 升序，V 相同时按 `preference` 中的
/// 先后（设备偏好）排序。`preference` 中未列出的格式不作为候选；重复项只计一次。浮点源（F3）
/// 只有同一浮点编码的格式（设备接受时）。
pub fn candidates(src: &SourceFormat, preference: &[AlsaFormat]) -> Candidates {
    let mut out = Candidates {
        items: [None; 4],
        len: 0,
    };
    if src.encoding().is_float() {
        if let Some(&f) = preference.iter().find(|f| f.encoding() == src.encoding()) {
            out.items[0] = Some(f);
            out.len = 1;
        }
        return out;
    }
    for v in [16u8, 24, 32] {
        for &f in preference {
            let dup = out.items[..out.len].contains(&Some(f));
            if !f.encoding().is_float()
                && f.valid_bits() == v
                && v >= src.bits()
                && !dup
                && out.len < out.items.len()
            {
                out.items[out.len] = Some(f);
                out.len += 1;
            }
        }
    }
    out
}

/// 设备名被拒绝的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceNameError {
    Empty,
    /// `plughw:`：plug 插件会自动转换格式、采样率与声道。
    Plug,
    /// `default` / `sysdefault`：通常经 PipeWire/PulseAudio 或 dmix，会混音或重采样。
    Default,
    /// `dmix:` / `dsnoop:`：软件混音，可能重采样。
    Dmix,
    /// `pipewire`：经 PipeWire 图，可能混音、重采样或改变格式。
    PipeWire,
    /// `pulse`：经 PulseAudio，会混音与重采样。
    Pulse,
    /// 其他 PCM 名（插件或别名），无法确认不转换。
    NotHw,
    /// `hw:` 后的卡号/设备号格式不正确。
    Malformed,
}

impl fmt::Display for DeviceNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "未指定设备名；需要 hw:卡,设备",
            Self::Plug => {
                "plughw 会自动转换格式、采样率与声道，不满足严格保真；请改用对应的 hw:卡,设备"
            }
            Self::Default => {
                "default/sysdefault 通常经 PipeWire、PulseAudio 或 dmix，会混音或重采样；请指定 hw:卡,设备"
            }
            Self::Dmix => "dmix/dsnoop 是软件混音，可能重采样；请指定 hw:卡,设备",
            Self::PipeWire => "pipewire 设备经 PipeWire 处理，可能混音、重采样或改变格式；请指定 hw:卡,设备",
            Self::Pulse => "pulse 设备经 PulseAudio，会混音与重采样；请指定 hw:卡,设备",
            Self::NotHw => "只接受 hw: 设备；其他 PCM 名可能是插件或别名，无法确认不转换音频",
            Self::Malformed => "hw: 设备名格式不正确；应为 hw:卡号[,设备号[,子设备号]] 或 hw:CARD=名称[,DEV=设备号]",
        })
    }
}

/// 声卡引用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Card<'a> {
    Index(u32),
    Name(&'a str),
}

/// 已检查的 `hw:` 设备名。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HwDevice<'a> {
    pub card: Card<'a>,
    pub device: u32,
    pub subdevice: Option<u32>,
}

impl HwDevice<'_> {
    /// 卡号为数字时，对应的播放设备节点 `/dev/snd/pcmC{卡}D{设备}p` 的卡号与设备号
    /// （宿主可据此查找占用进程）。
    pub fn playback_node(&self) -> Option<(u32, u32)> {
        match self.card {
            Card::Index(c) => Some((c, self.device)),
            Card::Name(_) => None,
        }
    }
}

fn parse_u32(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn valid_card_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// 检查设备名：只接受 `hw:`，其他会转换或混音的 PCM 一律拒绝。
///
/// 接受：`hw:0`、`hw:0,1`、`hw:0,1,2`、`hw:名称,1,2`（名称以字母开头）、`hw:CARD=名称`、
/// `hw:CARD=名称,DEV=1`、`hw:CARD=名称,DEV=1,SUBDEV=0`（名称为字母、数字、`_`、`-`）。
pub fn check_device_name(name: &str) -> Result<HwDevice<'_>, DeviceNameError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(DeviceNameError::Empty);
    }
    let (kind, args) = match name.split_once(':') {
        Some((k, a)) => (k, Some(a)),
        None => (name, None),
    };
    match kind {
        "hw" => {}
        "plughw" | "plug" => return Err(DeviceNameError::Plug),
        "default" | "sysdefault" => return Err(DeviceNameError::Default),
        "dmix" | "dsnoop" => return Err(DeviceNameError::Dmix),
        "pipewire" => return Err(DeviceNameError::PipeWire),
        "pulse" => return Err(DeviceNameError::Pulse),
        _ => return Err(DeviceNameError::NotHw),
    }
    let args = args.ok_or(DeviceNameError::Malformed)?;
    let mut parts = args.split(',');
    let first = parts.next().ok_or(DeviceNameError::Malformed)?;
    let named = first.starts_with("CARD=");
    let card = match first.strip_prefix("CARD=") {
        Some(n) if valid_card_name(n) => match parse_u32(n) {
            Some(i) => Card::Index(i),
            None => Card::Name(n),
        },
        Some(_) => return Err(DeviceNameError::Malformed),
        // 位置写法：卡号或声卡 id（以字母开头，如 `hw:Loopback,0,1`）。
        None => match parse_u32(first) {
            Some(i) => Card::Index(i),
            None if first.starts_with(|c: char| c.is_ascii_alphabetic())
                && valid_card_name(first) =>
            {
                Card::Name(first)
            }
            None => return Err(DeviceNameError::Malformed),
        },
    };
    let field = |p: Option<&str>, key: &str| -> Result<Option<u32>, DeviceNameError> {
        match p {
            None => Ok(None),
            Some(p) => {
                let v = if named {
                    p.strip_prefix(key)
                        .and_then(|r| r.strip_prefix('='))
                        .ok_or(DeviceNameError::Malformed)?
                } else {
                    p
                };
                parse_u32(v).map(Some).ok_or(DeviceNameError::Malformed)
            }
        }
    };
    let device = field(parts.next(), "DEV")?.unwrap_or(0);
    let subdevice = field(parts.next(), "SUBDEV")?;
    if parts.next().is_some() {
        return Err(DeviceNameError::Malformed);
    }
    Ok(HwDevice {
        card,
        device,
        subdevice,
    })
}

/// 访问方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    RwInterleaved,
    MmapInterleaved,
    Other,
}

/// 后端请求的硬件参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HwRequest {
    pub format: AlsaFormat,
    pub sample_rate: u32,
    pub channels: u16,
    /// 设备声道布局（设备上的样本顺序，见 [`plan_chmap`]）；声道数必须等于 `channels`。
    pub layout: ChannelLayout,
    pub period_frames: u32,
    pub buffer_frames: u32,
}

/// 设置后读回的已应用参数（A1 起为内核 `HW_PARAMS` 写回的值，与 alsa-lib
/// `snd_pcm_hw_params_current` 读到的是同一组字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HwApplied {
    pub access: Access,
    /// 读回的 `snd_pcm_format_t` 数值。
    pub format_code: i32,
    /// `snd_pcm_hw_params_get_rate_numden` 的精确有理数采样率。
    pub rate_num: u32,
    pub rate_den: u32,
    pub channels: u32,
    pub period_frames: u32,
    pub buffer_frames: u32,
    /// `snd_pcm_hw_params_get_rate_resample`：alsa-lib 是否会重采样。
    pub resample: bool,
    /// `snd_pcm_hw_params_get_sbits`：设备实际使用的有效位数（读不到为 `None`）。
    pub significant_bits: Option<u32>,
}

/// 读回参数与请求不一致的项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamMismatch {
    Access(Access),
    /// 读回的格式不是请求的格式（格式被改写）。
    Format {
        requested: AlsaFormat,
        applied_code: i32,
    },
    /// 采样率不是精确的请求值（含非整数的近似值）。
    Rate {
        requested: u32,
        num: u32,
        den: u32,
    },
    Channels {
        requested: u16,
        applied: u32,
    },
    Period {
        requested: u32,
        applied: u32,
    },
    Buffer {
        requested: u32,
        applied: u32,
    },
    /// alsa-lib 重采样未关闭。
    Resample,
    /// 设备实际有效位少于源有效位 N，会丢失精度。
    SignificantBits {
        source: u8,
        applied: u32,
    },
}

impl fmt::Display for ParamMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Access(a) => write!(f, "访问方式为 {a:?}，要求 RW_INTERLEAVED"),
            Self::Format {
                requested,
                applied_code,
            } => match AlsaFormat::from_code(applied_code) {
                Ok(a) => write!(f, "格式被改为 {a}，请求 {requested}"),
                Err(_) => write!(f, "格式被改为代码 {applied_code}，请求 {requested}"),
            },
            Self::Rate {
                requested,
                num,
                den,
            } => {
                write!(f, "采样率为 {num}/{den} Hz，请求精确的 {requested} Hz")
            }
            Self::Channels { requested, applied } => {
                write!(f, "声道为 {applied}，请求 {requested}")
            }
            Self::Period { requested, applied } => {
                write!(f, "周期为 {applied} 帧，请求 {requested} 帧")
            }
            Self::Buffer { requested, applied } => {
                write!(f, "缓冲为 {applied} 帧，请求 {requested} 帧")
            }
            Self::Resample => f.write_str("alsa-lib 重采样未关闭"),
            Self::SignificantBits { source, applied } => {
                write!(f, "设备有效位 {applied} 少于源有效位 {source}，会丢失精度")
            }
        }
    }
}

/// 全部不一致项（不分配）：保存请求、读回值与不一致项的位掩码，迭代时生成每一项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mismatches {
    req: HwRequest,
    applied: HwApplied,
    source_bits: u8,
    mask: u8,
}

const M_ACCESS: u8 = 1 << 0;
const M_FORMAT: u8 = 1 << 1;
const M_RATE: u8 = 1 << 2;
const M_CHANNELS: u8 = 1 << 3;
const M_PERIOD: u8 = 1 << 4;
const M_BUFFER: u8 = 1 << 5;
const M_RESAMPLE: u8 = 1 << 6;
const M_SBITS: u8 = 1 << 7;

impl Mismatches {
    fn item(&self, bit: u8) -> ParamMismatch {
        let (r, a) = (&self.req, &self.applied);
        match bit {
            M_ACCESS => ParamMismatch::Access(a.access),
            M_FORMAT => ParamMismatch::Format {
                requested: r.format,
                applied_code: a.format_code,
            },
            M_RATE => ParamMismatch::Rate {
                requested: r.sample_rate,
                num: a.rate_num,
                den: a.rate_den,
            },
            M_CHANNELS => ParamMismatch::Channels {
                requested: r.channels,
                applied: a.channels,
            },
            M_PERIOD => ParamMismatch::Period {
                requested: r.period_frames,
                applied: a.period_frames,
            },
            M_BUFFER => ParamMismatch::Buffer {
                requested: r.buffer_frames,
                applied: a.buffer_frames,
            },
            M_RESAMPLE => ParamMismatch::Resample,
            _ => ParamMismatch::SignificantBits {
                source: self.source_bits,
                applied: a.significant_bits.unwrap_or(0),
            },
        }
    }

    /// 按固定顺序（访问方式、格式、采样率、声道、周期、缓冲、重采样、有效位）列出。
    pub fn iter(&self) -> impl Iterator<Item = ParamMismatch> + '_ {
        (0..8u8)
            .map(|i| 1u8 << i)
            .filter(|bit| self.mask & bit != 0)
            .map(|bit| self.item(bit))
    }

    pub fn len(&self) -> usize {
        self.mask.count_ones() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.mask == 0
    }

    pub fn contains(&self, pred: impl Fn(&ParamMismatch) -> bool) -> bool {
        self.iter().any(|m| pred(&m))
    }
}

impl fmt::Display for Mismatches {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, m) in self.iter().enumerate() {
            if i > 0 {
                f.write_str("；")?;
            }
            write!(f, "{m}")?;
        }
        Ok(())
    }
}

/// 确认失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmError {
    /// 请求本身无法构成设备格式，或不能无损承载源（采样率/声道不同、N > V）。
    Request(FormatError),
    /// 读回参数与请求不一致。
    Mismatch(Mismatches),
}

/// 核对读回的已应用参数：访问方式、格式、精确采样率、声道、周期、缓冲必须与请求一致，
/// alsa-lib 重采样必须关闭，设备有效位（可读时）不得少于源有效位 N。全部一致时返回
/// 设备格式；否则返回全部不一致项。
pub fn confirm(
    src: &SourceFormat,
    req: &HwRequest,
    applied: &HwApplied,
) -> Result<DeviceFormat, ConfirmError> {
    if req.layout.channels() != req.channels {
        return Err(ConfirmError::Request(FormatError::UndefinedLayout));
    }
    let format = req
        .format
        .device_format_layout(req.sample_rate, req.layout)
        .map_err(ConfirmError::Request)?;
    if !format.accepts(src) {
        return Err(ConfirmError::Request(FormatError::ValidBits));
    }
    let exact_rate = applied.rate_den != 0
        && u64::from(applied.rate_num) == u64::from(req.sample_rate) * u64::from(applied.rate_den);
    let checks = [
        (applied.access != Access::RwInterleaved, M_ACCESS),
        (applied.format_code != req.format.code(), M_FORMAT),
        (!exact_rate, M_RATE),
        (applied.channels != u32::from(req.channels), M_CHANNELS),
        (applied.period_frames != req.period_frames, M_PERIOD),
        (applied.buffer_frames != req.buffer_frames, M_BUFFER),
        (applied.resample, M_RESAMPLE),
        (
            applied
                .significant_bits
                .is_some_and(|b| b < u32::from(src.bits())),
            M_SBITS,
        ),
    ];
    let mask = checks
        .iter()
        .filter(|(bad, _)| *bad)
        .fold(0u8, |m, (_, bit)| m | bit);
    if mask == 0 {
        Ok(format)
    } else {
        Err(ConfirmError::Mismatch(Mismatches {
            req: *req,
            applied: *applied,
            source_bits: src.bits(),
            mask,
        }))
    }
}

/// ALSA 声道映射类型（`snd_pcm_chmap_type`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChmapType {
    /// `SND_CHMAP_TYPE_NONE`：位置未指定。
    None,
    /// `SND_CHMAP_TYPE_FIXED`：固定位置，不能改。
    Fixed,
    /// `SND_CHMAP_TYPE_VAR`：位置可自由交换，可用 `snd_pcm_set_chmap` 设置。
    Var,
    /// `SND_CHMAP_TYPE_PAIRED`：成对可交换。本项目按列出的顺序使用，不做成对交换。
    Paired,
}

/// `SND_CHMAP_POSITION_MASK`：位置值所在的低 16 位。
pub const CHMAP_POSITION_MASK: u32 = 0xffff;
/// `SND_CHMAP_PHASE_INVERSE`：该声道反相。
pub const CHMAP_PHASE_INVERSE: u32 = 1 << 16;
/// `SND_CHMAP_DRIVER_SPEC`：驱动自定义的位置值（不是标准位置）。
pub const CHMAP_DRIVER_SPEC: u32 = 1 << 17;
/// `SND_CHMAP_MONO`。
pub const CHMAP_MONO: u32 = 2;

/// WAVE 扬声器位置与 ALSA 位置值（alsa-lib `include/pcm.h` 的 `SND_CHMAP_*`，已与系统头文件
/// 核对）的对应：FL=3、FR=4、RL=5、RR=6、FC=7、LFE=8、SL=9、SR=10、RC=11、FLC=12、FRC=13、
/// TC=21、TFL=22、TFR=23、TFC=24、TRL=25、TRR=26、TRC=27。ALSA 的 rear（RL/RR/RC）即 WAVE 的
/// back（BL/BR/BC）。
const CHMAP_OF_SPEAKER: [u32; 18] = [
    3,  // FrontLeft
    4,  // FrontRight
    7,  // FrontCenter
    8,  // LowFrequency
    5,  // BackLeft（RL）
    6,  // BackRight（RR）
    12, // FrontLeftOfCenter（FLC）
    13, // FrontRightOfCenter（FRC）
    11, // BackCenter（RC）
    9,  // SideLeft
    10, // SideRight
    21, // TopCenter
    22, // TopFrontLeft
    24, // TopFrontCenter
    23, // TopFrontRight
    25, // TopBackLeft（TRL）
    27, // TopBackCenter（TRC）
    26, // TopBackRight（TRR）
];

/// ALSA 声道映射不可用的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChmapError {
    /// 设备不报告声道映射，多于 2 声道时无法确认布局。
    Unavailable,
    /// 设备没有与源扬声器位置集合相同的映射。
    NoMatchingMap,
    /// 映射含反相声道（`SND_CHMAP_PHASE_INVERSE`），会改变信号。
    PhaseInverse { channel: usize },
    /// 映射含 WAVE 无对应的位置（如 `UNKNOWN`、`NA`、宽声道、低音左右）。
    UnknownPosition { channel: usize, position: u32 },
    /// 映射有重复位置或声道数为 0 / 超过 18。
    Invalid,
    /// 设置后读回的映射与计划不符。
    ReadBack,
}

impl fmt::Display for ChmapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Unavailable => {
                f.write_str("设备不报告声道映射（chmap），无法确认多声道的扬声器布局")
            }
            Self::NoMatchingMap => f.write_str("设备没有与源扬声器位置相同的声道映射"),
            Self::PhaseInverse { channel } => write!(f, "设备声道 {channel} 为反相映射"),
            Self::UnknownPosition { channel, position } => {
                write!(
                    f,
                    "设备声道 {channel} 的位置 {position} 没有对应的扬声器位置"
                )
            }
            Self::Invalid => f.write_str("设备声道映射无效（重复位置或声道数不符）"),
            Self::ReadBack => f.write_str("设置后读回的声道映射与请求不符"),
        }
    }
}

/// 扬声器位置的 ALSA 位置值。
pub const fn chmap_position(s: Speaker) -> u32 {
    CHMAP_OF_SPEAKER[s as usize]
}

/// 由 ALSA 映射（原始位置值，含标志位）得到设备布局。单声道映射 `[MONO]` 视为 `[FC]`。
pub fn layout_from_chmap(raw: &[u32]) -> Result<ChannelLayout, ChmapError> {
    if raw.len() == 1 && raw[0] & CHMAP_POSITION_MASK == CHMAP_MONO {
        if raw[0] & CHMAP_PHASE_INVERSE != 0 {
            return Err(ChmapError::PhaseInverse { channel: 0 });
        }
        return Ok(ChannelLayout::MONO);
    }
    if raw.is_empty() || raw.len() > MAX_LAYOUT_CHANNELS {
        return Err(ChmapError::Invalid);
    }
    let mut list = [Speaker::FrontLeft; MAX_LAYOUT_CHANNELS];
    for (i, (&p, slot)) in raw.iter().zip(list.iter_mut()).enumerate() {
        if p & CHMAP_PHASE_INVERSE != 0 {
            return Err(ChmapError::PhaseInverse { channel: i });
        }
        let pos = p & CHMAP_POSITION_MASK;
        // 驱动自定义位置或未知标志位：没有可确认的扬声器位置。
        if p & !(CHMAP_POSITION_MASK | CHMAP_PHASE_INVERSE) != 0 {
            return Err(ChmapError::UnknownPosition {
                channel: i,
                position: p,
            });
        }
        let found = CHMAP_OF_SPEAKER.iter().position(|&c| c == pos);
        let Some(bit) = found.and_then(|b| Speaker::from_bit(b as u32)) else {
            return Err(ChmapError::UnknownPosition {
                channel: i,
                position: pos,
            });
        };
        *slot = bit;
    }
    ChannelLayout::from_positions(&list[..raw.len()]).ok_or(ChmapError::Invalid)
}

/// 声道映射计划。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChmapPlan {
    /// 设备不报告映射；单/双声道按默认布局使用（报告为“不可查询”）。
    Unqueried(ChannelLayout),
    /// 可变映射：设置为该布局（与源相同顺序），之后必须读回核对。
    Set(ChannelLayout),
    /// 固定映射：设备即为该布局；与源顺序不同时 pack 纯置换。
    Fixed(ChannelLayout),
}

impl ChmapPlan {
    /// 计划的设备布局。
    pub const fn layout(&self) -> ChannelLayout {
        match *self {
            Self::Unqueried(l) | Self::Set(l) | Self::Fixed(l) => l,
        }
    }
}

/// 按 `snd_pcm_query_chmaps` 的结果选择声道映射（tasks/F1.md §3）：
/// - 有声道数相同的可变映射且其位置集合包含源的全部位置：设置为源布局（不需要置换）；
/// - 否则取位置集合与源相同的固定（或成对）映射，按其顺序使用（pack 纯置换）；
/// - 设备没有该声道数的映射：单/双声道按默认布局，多于 2 声道拒绝；
/// - 有该声道数的映射但位置集合都不同：拒绝。
///
/// 含反相或无对应位置的映射跳过，不会被选中。
pub fn plan_chmap(
    src: &ChannelLayout,
    maps: &[(ChmapType, &[u32])],
) -> Result<ChmapPlan, ChmapError> {
    let n = usize::from(src.channels());
    let candidates = maps.iter().filter(|(_, m)| m.len() == n);
    let mut fixed = None;
    let mut any = false;
    for (t, m) in candidates {
        any = true;
        let Ok(layout) = layout_from_chmap(m) else {
            continue;
        };
        if !layout.same_positions(src) {
            continue;
        }
        match t {
            ChmapType::Var => return Ok(ChmapPlan::Set(*src)),
            ChmapType::Fixed | ChmapType::Paired => {
                if fixed.is_none() || layout == *src {
                    fixed = Some(layout);
                }
            }
            ChmapType::None => {}
        }
    }
    if let Some(l) = fixed {
        return Ok(ChmapPlan::Fixed(l));
    }
    if any {
        return Err(ChmapError::NoMatchingMap);
    }
    // 没有该声道数的映射：单/双声道按默认布局（M2 行为不变），更多声道无法确认布局。
    match ChannelLayout::default_for(src.channels()) {
        Some(l) if l == *src => Ok(ChmapPlan::Unqueried(l)),
        _ => Err(ChmapError::Unavailable),
    }
}

/// 核对读回的映射（`snd_pcm_get_chmap`）是否等于计划的设备布局。
pub fn confirm_chmap(plan: &ChmapPlan, read_back: &[u32]) -> Result<ChannelLayout, ChmapError> {
    let got = layout_from_chmap(read_back)?;
    if got == plan.layout() {
        Ok(got)
    } else {
        Err(ChmapError::ReadBack)
    }
}

/// Linux errno（与 `asm-generic/errno-base.h` 一致）。
pub mod errno {
    pub const EPERM: i32 = 1;
    pub const ENOENT: i32 = 2;
    pub const EBUSY: i32 = 16;
    pub const ENODEV: i32 = 19;
    pub const EACCES: i32 = 13;
    pub const EINVAL: i32 = 22;
}

/// 打开 `hw` 设备失败的分类（`snd_pcm_open` 返回负 errno）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenError {
    /// `EBUSY`：设备被占用（常见于 PipeWire、PulseAudio、JACK 或其他播放器）。
    Busy,
    /// `ENOENT` / `ENODEV`：卡或设备不存在。
    NoDevice,
    /// `EACCES` / `EPERM`：无权限（通常需要 `audio` 组）。
    Permission,
    /// 其他错误（errno 取正值）。
    Other(i32),
}

impl OpenError {
    /// 由 alsa-lib 返回值（负 errno）或正 errno 分类。
    pub const fn from_errno(err: i32) -> Self {
        let e = if err < 0 { err.saturating_neg() } else { err };
        match e {
            errno::EBUSY => Self::Busy,
            errno::ENOENT | errno::ENODEV => Self::NoDevice,
            errno::EACCES | errno::EPERM => Self::Permission,
            other => Self::Other(other),
        }
    }
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Busy => f.write_str(
                "设备被占用（EBUSY）：通常是 PipeWire、PulseAudio、JACK 或其他播放器正在使用该 hw 设备。\
                 请先释放占用者（如停止对应服务或在其设置中让出设备）后重试；本程序不会改用共享或混音路径",
            ),
            Self::NoDevice => f.write_str("找不到该声卡或设备（ENOENT/ENODEV）"),
            Self::Permission => {
                f.write_str("无权打开该设备（EACCES/EPERM）：通常需要把用户加入 audio 组")
            }
            Self::Other(e) => write!(f, "打开设备失败（errno {e}）"),
        }
    }
}

#[cfg(test)]
mod tests;
