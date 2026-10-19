//! QyBit PCM 格式描述。
//!
//! 只定义与平台 SDK 无关的格式字段和纯构造函数，不调用任何系统 API，
//! 因此纯逻辑测试可在任何平台运行（M0 T1）。
//!
//! 术语（AGENTS §3）：
//! - N：源有效位数（解码器输出的 `i32` 右对齐，携带 N）。
//! - C：设备容器位宽；V：设备有效位数。
//! - 字节序与有效位在容器中的放置方式（左对齐 / 右对齐）显式区分。
#![no_std]

#[cfg(feature = "ring")]
extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod alsa;
pub mod convert;
pub mod layout;
pub mod pack;
pub mod queue;
#[cfg(feature = "ring")]
pub mod ring;
#[cfg(feature = "static-queue")]
pub mod static_queue;
pub mod wave;

pub use layout::{ChannelLayout, MAX_LAYOUT_CHANNELS, Speaker};

/// 有声道布局可描述的最大声道数（WAVE 的 18 个扬声器位置，F1）。这是格式语义的限制，不是软件
/// 资源上限：更多声道的源没有可确认的扬声器位置，按“未定义布局”拒绝；实际可播放的声道数还受
/// 宿主声明的能力与资源限制（`Capabilities::max_channels`）。
pub const MAX_CHANNELS: u16 = MAX_LAYOUT_CHANNELS as u16;

/// 格式构造错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    /// 采样率为 0。
    SampleRate,
    /// 声道数为 0 或超出支持范围。
    Channels,
    /// 容器位宽不在支持集合内。
    ContainerBits,
    /// 有效位数为 0 或大于容器位宽。
    ValidBits,
    /// 派生字段（blockAlign、字节率等）计算溢出。
    Overflow,
    /// 字节序或有效位放置方式不符合目标描述（如 WASAPI 要求小端、高位对齐）。
    Layout,
    /// 多于 2 声道但没有给出声道布局，或布局声道数与声道数不符（F1）；或布局不是目标描述
    /// 能表示的布局（`WAVEFORMATEX` 只表示标准单声道 `[FC]` 与立体声 `[FL FR]`）。
    UndefinedLayout,
    /// 声道布局不是掩码顺序（按位从低到高），目标描述（WAVEFORMATEXTENSIBLE）无法表示。
    LayoutOrder,
    /// 样本编码与所用构造函数不符（如用浮点构造函数构造整数格式，F3）。
    Encoding,
}

/// 样本编码（F3）：整数 PCM 或 IEEE 754 浮点。
///
/// 浮点样本在解码输出（`&[i32]`）中按位存放、不做数值运算：binary32 每个样本占 1 个 `i32`
/// （`f32::to_bits` 的按位重解释），binary64 每个样本占 2 个 `i32`（先低 32 位、后高 32 位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SampleEncoding {
    /// 整数 PCM（交错、右对齐的 `i32`，有效位 N）。
    #[default]
    Int,
    /// IEEE 754 binary32。
    Float32,
    /// IEEE 754 binary64。
    Float64,
    /// 1 位 DSD 码流（D1，tasks/D1.md）：每帧每声道一个 32 位 DSD 字（`i32` 按位存放），时间上
    /// 最早的位在最高位；帧率为 DSD 位率 ÷ 32。只能原样送到 DSD 设备格式（原生或 DoP 封装），
    /// 不转 PCM。
    Dsd,
}

impl SampleEncoding {
    /// 每个样本在解码输出中占的 `i32` 个数。
    pub const fn words(self) -> usize {
        match self {
            Self::Float64 => 2,
            Self::Int | Self::Float32 | Self::Dsd => 1,
        }
    }

    pub const fn is_float(self) -> bool {
        matches!(self, Self::Float32 | Self::Float64)
    }

    /// 是否为 DSD 码流（D1）。
    pub const fn is_dsd(self) -> bool {
        matches!(self, Self::Dsd)
    }

    /// 浮点编码的位宽（32 / 64）；整数与 DSD 为 `None`。
    pub const fn float_bits(self) -> Option<u8> {
        match self {
            Self::Int | Self::Dsd => None,
            Self::Float32 => Some(32),
            Self::Float64 => Some(64),
        }
    }
}

/// 样本字节序。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
    /// 16 位半字内小端、**最高有效半字在前**（M3 T2）：STM32F4 等的 SPI/I²S 外设数据寄存器为
    /// 16 位，经 16 位 DMA 发送 32 位槽位时先发高半字。32 位容器中值 `0x1234_5678` 的内存字节为
    /// `34 12 78 56`；16 位容器与 [`Endian::Little`] 相同；24 位容器无此表示，构造时拒绝。
    HalfWordSwapped,
}

/// 有效位在容器内的放置方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// 有效位位于容器高位，低位补零（如 WASAPI 32/24）。
    MsbAligned,
    /// 有效位位于容器低位并符号扩展（如 ALSA `S24_LE`）。
    LsbAligned,
}

/// 源 PCM 格式：解码器输出为交错、右对齐的 `i32`，携带有效位数 N 与声道布局；浮点源（F3）
/// 按位存放，见 [`SampleEncoding`]。
///
/// 有损标记（E1，AGENTS §3）：有损格式（由插件提供，如 MP3、AAC）解码出的是重建信号，解码器用
/// [`SourceFormat::lossy`] 标记；此时 [`SourceFormat::bits`] 是解码输出的精度，不是音源位深，报告与
/// 界面据 [`SourceFormat::is_lossy`] 明确写出。标记只影响报告，不改变设备选择与送出的数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceFormat {
    sample_rate: u32,
    channels: u16,
    bits: u8,
    layout: ChannelLayout,
    encoding: SampleEncoding,
    lossy: bool,
}

impl SourceFormat {
    /// 构造单/双声道源格式（默认布局 `[FC]` / `[FL FR]`）；N 必须在 1..=32。
    /// 多于 2 声道必须用 [`SourceFormat::with_layout`] 给出布局，否则返回
    /// [`FormatError::UndefinedLayout`]。
    pub const fn new(sample_rate: u32, channels: u16, bits: u8) -> Result<Self, FormatError> {
        if channels == 0 {
            return Err(FormatError::Channels);
        }
        match ChannelLayout::default_for(channels) {
            Some(layout) => Self::with_layout(sample_rate, layout, bits),
            None => Err(FormatError::UndefinedLayout),
        }
    }

    /// 以声道布局构造源格式；声道数即布局的声道数。
    pub const fn with_layout(
        sample_rate: u32,
        layout: ChannelLayout,
        bits: u8,
    ) -> Result<Self, FormatError> {
        // 采样率不设软件上限（AGENTS §3）；只拒绝 0，派生量溢出另行检查。
        if sample_rate == 0 {
            return Err(FormatError::SampleRate);
        }
        if bits == 0 || bits > 32 {
            return Err(FormatError::ValidBits);
        }
        Ok(Self {
            sample_rate,
            channels: layout.channels(),
            bits,
            layout,
            encoding: SampleEncoding::Int,
            lossy: false,
        })
    }

    /// 浮点源格式（F3）：`encoding` 为 [`SampleEncoding::Float32`] 或 [`SampleEncoding::Float64`]；
    /// [`SourceFormat::bits`] 为浮点位宽 32 / 64。
    pub const fn float(
        sample_rate: u32,
        layout: ChannelLayout,
        encoding: SampleEncoding,
    ) -> Result<Self, FormatError> {
        if sample_rate == 0 {
            return Err(FormatError::SampleRate);
        }
        let Some(bits) = encoding.float_bits() else {
            return Err(FormatError::Encoding);
        };
        Ok(Self {
            sample_rate,
            channels: layout.channels(),
            bits,
            layout,
            encoding,
            lossy: false,
        })
    }

    /// DSD 源格式（D1）：`bit_rate` 为每声道的 DSD 位率（DSD64 为 2 822 400），必须是 32 的
    /// 正整数倍；[`SourceFormat::sample_rate`] 为 DSD 字率（位率 ÷ 32），[`SourceFormat::bits`]
    /// 为 1。
    pub const fn dsd(bit_rate: u32, layout: ChannelLayout) -> Result<Self, FormatError> {
        if bit_rate == 0 || !bit_rate.is_multiple_of(32) {
            return Err(FormatError::SampleRate);
        }
        Ok(Self {
            sample_rate: bit_rate / 32,
            channels: layout.channels(),
            bits: 1,
            layout,
            encoding: SampleEncoding::Dsd,
            lossy: false,
        })
    }

    /// DSD 源的每声道位率（字率 × 32，D1）；PCM 为 `None`。
    pub const fn dsd_bit_rate(&self) -> Option<u32> {
        match self.encoding {
            SampleEncoding::Dsd => Some(self.sample_rate * 32),
            _ => None,
        }
    }

    /// 标记为有损编码的解码输出（E1）。只能由解码器在构造格式时调用；无损格式不调用。
    #[must_use]
    pub const fn lossy(self) -> Self {
        Self {
            lossy: true,
            ..self
        }
    }

    /// 是否为有损编码的解码输出（E1）：为真时 [`SourceFormat::bits`] 是解码精度，不是音源位深。
    pub const fn is_lossy(&self) -> bool {
        self.lossy
    }

    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub const fn channels(&self) -> u16 {
        self.channels
    }

    /// 整数源为有效位数 N；浮点源为浮点位宽（32 / 64）。
    pub const fn bits(&self) -> u8 {
        self.bits
    }

    /// 声道布局（样本顺序）。
    pub const fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// 样本编码（F3）。
    pub const fn encoding(&self) -> SampleEncoding {
        self.encoding
    }

    /// 一帧在解码输出中占的 `i32` 个数（声道数 × 每样本字数）。
    pub const fn words_per_frame(&self) -> usize {
        self.channels as usize * self.encoding.words()
    }

    /// 同采样率、同编码与位数，声道布局换成 `layout`（O1 单声道复制时的设备侧声道）。
    /// 有损标记保持不变。
    pub const fn relayout(&self, layout: ChannelLayout) -> Result<Self, FormatError> {
        let r = match self.encoding {
            SampleEncoding::Int => Self::with_layout(self.sample_rate, layout, self.bits),
            SampleEncoding::Dsd => Self::dsd(self.sample_rate * 32, layout),
            _ => Self::float(self.sample_rate, layout, self.encoding),
        };
        self.keep_lossy(r)
    }

    const fn keep_lossy(&self, r: Result<Self, FormatError>) -> Result<Self, FormatError> {
        match r {
            Ok(f) if self.lossy => Ok(f.lossy()),
            r => r,
        }
    }

    /// 同采样率、同布局、编码与位数换成 `encoding` / `bits` 的格式（整数时 `bits` 为 N）。有损标记保持不变。
    pub const fn with_encoding(
        &self,
        encoding: SampleEncoding,
        bits: u8,
    ) -> Result<Self, FormatError> {
        // PCM 与 DSD 之间没有不改变样本的换法（D1：不转换）。
        if self.encoding.is_dsd() || encoding.is_dsd() {
            return Err(FormatError::Encoding);
        }
        let r = match encoding {
            SampleEncoding::Int => Self::with_layout(self.sample_rate, self.layout, bits),
            _ => Self::float(self.sample_rate, self.layout, encoding),
        };
        self.keep_lossy(r)
    }
}

/// 原生 DSD 的设备样本（ALSA `DSD_*`，D1）。最早的位总在每个字节的最高位；多字节样本按字节序
/// 存放（`Be`：时间上最早的字节在前）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DsdWord {
    /// `DSD_U8`：每声道每设备帧 8 位。
    U8,
    /// `DSD_U16_LE`。
    U16Le,
    /// `DSD_U16_BE`。
    U16Be,
    /// `DSD_U32_LE`。
    U32Le,
    /// `DSD_U32_BE`。
    U32Be,
}

/// DoP 封装所用的 PCM 容器（D1）。DoP v1.1：每个样本的 24 位 = 标记字节（相邻帧交替 `0x05` /
/// `0xFA`）+ 16 位 DSD（最早的位在 bit 15）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DopContainer {
    /// C24 / V24（ALSA `S24_3LE`，WASAPI 24/24）。
    S24_3Le,
    /// C32 / V24 高位对齐、低字节为 0（ALSA `S32_LE`，WASAPI 32/24）。
    S32LeMsb,
    /// C32 / V24 低位对齐、高字节符号扩展（ALSA `S24_LE`）。
    S24LeLsb,
}

/// DSD 在设备上的传输方式（D1，tasks/D1.md §1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DsdTransport {
    /// 原生 DSD 格式（设备以 DSD 打开）。
    Native(DsdWord),
    /// DoP：设备以整数 PCM 打开，DSD 位装在 PCM 样本里。
    Dop(DopContainer),
}

impl DsdTransport {
    /// 每个 DSD 字（32 位）在设备上展开成的设备帧数 e。
    pub const fn expansion(self) -> u32 {
        match self {
            Self::Native(DsdWord::U8) => 4,
            Self::Native(DsdWord::U16Le | DsdWord::U16Be) | Self::Dop(_) => 2,
            Self::Native(DsdWord::U32Le | DsdWord::U32Be) => 1,
        }
    }

    /// 每个设备样本（一个声道、一个设备帧）的字节数。
    pub const fn sample_bytes(self) -> u16 {
        match self {
            Self::Native(DsdWord::U8) => 1,
            Self::Native(DsdWord::U16Le | DsdWord::U16Be) => 2,
            Self::Dop(DopContainer::S24_3Le) => 3,
            Self::Native(DsdWord::U32Le | DsdWord::U32Be)
            | Self::Dop(DopContainer::S32LeMsb | DopContainer::S24LeLsb) => 4,
        }
    }
}

/// DoP 标记（DoP v1.1）：每个 DSD 字展开的两个设备帧依次用 `0x05`、`0xFA`，整个流交替不断。
pub const DOP_MARKERS: [u8; 2] = [0x05, 0xFA];

/// DSD 静音模式（每字节 `0x69`，即 01101001：密度 50%、不含直流）。
pub const DSD_SILENCE: u8 = 0x69;

/// 一帧最多的字节数（18 声道 × 每声道 8 字节：binary64，或 DoP 32 位容器的两个设备帧）。
pub const MAX_FRAME_BYTES: usize = MAX_LAYOUT_CHANNELS * 8;

/// 设备格式的一帧静音（[`DeviceFormat::silence_frame`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SilenceFrame {
    bytes: [u8; MAX_FRAME_BYTES],
    len: usize,
}

impl SilenceFrame {
    /// `len` 字节全零的一帧（PCM 静音）；`len` 超过 [`MAX_FRAME_BYTES`] 时截为上限。
    pub const fn zeros(len: usize) -> Self {
        Self {
            bytes: [0; MAX_FRAME_BYTES],
            len: if len > MAX_FRAME_BYTES {
                MAX_FRAME_BYTES
            } else {
                len
            },
        }
    }

    /// 一帧的字节（长度为 `block_align`）。
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    /// 用静音帧整帧重复填满 `out`（长度应为整帧；多出的不足一帧部分按帧内位置填写）。
    pub fn fill(&self, out: &mut [u8]) {
        if self.len == 0 {
            return;
        }
        for (i, b) in out.iter_mut().enumerate() {
            *b = self.bytes[i % self.len];
        }
    }

    /// 是否全为零字节（PCM 静音）。
    pub fn is_zero(&self) -> bool {
        self.as_bytes().iter().all(|&b| b == 0)
    }
}

/// 设备端格式字段（整数 PCM、浮点 PCM 或 DSD）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceFormat {
    sample_rate: u32,
    channels: u16,
    container_bits: u8,
    valid_bits: u8,
    endian: Endian,
    placement: Placement,
    block_align: u16,
    bytes_per_sec: u32,
    layout: ChannelLayout,
    encoding: SampleEncoding,
    /// DSD 的传输方式（D1）；PCM 为 `None`。
    dsd: Option<DsdTransport>,
}

impl DeviceFormat {
    /// 构造单/双声道设备格式（默认布局）并计算派生字段。多于 2 声道用
    /// [`DeviceFormat::with_layout`]。
    ///
    /// 容器位宽限定为 16/24/32；V 必须在 1..=C；所有派生量做溢出检查。
    pub const fn new(
        sample_rate: u32,
        channels: u16,
        container_bits: u8,
        valid_bits: u8,
        endian: Endian,
        placement: Placement,
    ) -> Result<Self, FormatError> {
        if channels == 0 {
            return Err(FormatError::Channels);
        }
        match ChannelLayout::default_for(channels) {
            Some(layout) => Self::with_layout(
                sample_rate,
                layout,
                container_bits,
                valid_bits,
                endian,
                placement,
            ),
            None => Err(FormatError::UndefinedLayout),
        }
    }

    /// 以设备声道布局（设备上的样本顺序）构造设备格式。
    pub const fn with_layout(
        sample_rate: u32,
        layout: ChannelLayout,
        container_bits: u8,
        valid_bits: u8,
        endian: Endian,
        placement: Placement,
    ) -> Result<Self, FormatError> {
        // 采样率不设软件上限（AGENTS §3）；只拒绝 0，派生量溢出另行检查。
        if sample_rate == 0 {
            return Err(FormatError::SampleRate);
        }
        let channels = layout.channels();
        if !matches!(container_bits, 16 | 24 | 32) {
            return Err(FormatError::ContainerBits);
        }
        if valid_bits == 0 || valid_bits > container_bits {
            return Err(FormatError::ValidBits);
        }
        if matches!(endian, Endian::HalfWordSwapped) && container_bits == 24 {
            return Err(FormatError::Layout);
        }
        let bytes_per_sample = (container_bits / 8) as u16;
        let block_align = match channels.checked_mul(bytes_per_sample) {
            Some(v) => v,
            None => return Err(FormatError::Overflow),
        };
        let bytes_per_sec = match sample_rate.checked_mul(block_align as u32) {
            Some(v) => v,
            None => return Err(FormatError::Overflow),
        };
        Ok(Self {
            sample_rate,
            channels,
            container_bits,
            valid_bits,
            endian,
            placement,
            block_align,
            bytes_per_sec,
            layout,
            encoding: SampleEncoding::Int,
            dsd: None,
        })
    }

    /// 浮点设备格式（F3）：容器即浮点位宽（32 / 64），有效位等于容器。字节序为小端或大端；
    /// 半字交换没有浮点表示，拒绝。
    pub const fn float_layout(
        sample_rate: u32,
        layout: ChannelLayout,
        encoding: SampleEncoding,
        endian: Endian,
    ) -> Result<Self, FormatError> {
        if sample_rate == 0 {
            return Err(FormatError::SampleRate);
        }
        let Some(bits) = encoding.float_bits() else {
            return Err(FormatError::Encoding);
        };
        if matches!(endian, Endian::HalfWordSwapped) {
            return Err(FormatError::Layout);
        }
        let channels = layout.channels();
        let block_align = match channels.checked_mul(bits as u16 / 8) {
            Some(v) => v,
            None => return Err(FormatError::Overflow),
        };
        let bytes_per_sec = match sample_rate.checked_mul(block_align as u32) {
            Some(v) => v,
            None => return Err(FormatError::Overflow),
        };
        Ok(Self {
            sample_rate,
            channels,
            container_bits: bits,
            valid_bits: bits,
            endian,
            placement: Placement::MsbAligned,
            block_align,
            bytes_per_sec,
            layout,
            encoding,
            dsd: None,
        })
    }

    /// DSD 设备格式（D1）：`word_rate` 为 DSD 字率（与 DSD 源的 [`SourceFormat::sample_rate`]
    /// 相同），`transport` 为原生 DSD 或 DoP。本格式的“帧”是一个 DSD 字：
    /// [`DeviceFormat::block_align`] 为一个字在设备上的字节数（e 个设备帧），设备实际的帧率与帧宽
    /// 见 [`DeviceFormat::device_rate`] 与 [`DeviceFormat::device_frame_bytes`]。
    pub const fn dsd(
        word_rate: u32,
        layout: ChannelLayout,
        transport: DsdTransport,
    ) -> Result<Self, FormatError> {
        if word_rate == 0 {
            return Err(FormatError::SampleRate);
        }
        // 设备帧率与位率都不得溢出。
        if word_rate.checked_mul(32).is_none() {
            return Err(FormatError::Overflow);
        }
        let channels = layout.channels();
        let e = transport.expansion() as u16;
        let device_frame = match channels.checked_mul(transport.sample_bytes()) {
            Some(v) => v,
            None => return Err(FormatError::Overflow),
        };
        let block_align = match device_frame.checked_mul(e) {
            Some(v) => v,
            None => return Err(FormatError::Overflow),
        };
        let bytes_per_sec = match word_rate.checked_mul(block_align as u32) {
            Some(v) => v,
            None => return Err(FormatError::Overflow),
        };
        let bits = (transport.sample_bytes() * 8) as u8;
        let (valid_bits, endian, placement) = match transport {
            DsdTransport::Native(DsdWord::U16Be | DsdWord::U32Be) => {
                (bits, Endian::Big, Placement::MsbAligned)
            }
            DsdTransport::Native(_) => (bits, Endian::Little, Placement::MsbAligned),
            DsdTransport::Dop(DopContainer::S24LeLsb) => {
                (24, Endian::Little, Placement::LsbAligned)
            }
            DsdTransport::Dop(_) => (24, Endian::Little, Placement::MsbAligned),
        };
        Ok(Self {
            sample_rate: word_rate,
            channels,
            container_bits: bits,
            valid_bits,
            endian,
            placement,
            block_align,
            bytes_per_sec,
            layout,
            encoding: SampleEncoding::Dsd,
            dsd: Some(transport),
        })
    }

    /// DSD 的传输方式（D1）；PCM 为 `None`。
    pub const fn dsd_transport(&self) -> Option<DsdTransport> {
        self.dsd
    }

    /// 每帧（DSD 为每个字）在设备上的设备帧数：PCM 为 1，DSD 见 [`DsdTransport::expansion`]。
    pub const fn expansion(&self) -> u32 {
        match self.dsd {
            Some(t) => t.expansion(),
            None => 1,
        }
    }

    /// 设备实际打开的帧率：PCM 即 [`DeviceFormat::sample_rate`]；DSD 为字率 × e（DoP 64 倍率时
    /// 为 176.4 kHz，原生 `DSD_U32` 为 88.2 kHz）。构造时已检查不溢出。
    pub const fn device_rate(&self) -> u32 {
        self.sample_rate * self.expansion()
    }

    /// 一个设备帧的字节数：PCM 即 [`DeviceFormat::block_align`]；DSD 为 `block_align` ÷ e。
    pub const fn device_frame_bytes(&self) -> u16 {
        self.block_align / self.expansion() as u16
    }

    /// DoP 在设备上实际打开的整数 PCM 格式（D1）：设备帧率、同布局、容器为 DoP 容器、有效 24 位
    /// （`S24_LE` 低位对齐，其余高位对齐）。原生 DSD 与 PCM 为 `None`。WASAPI 用它填写
    /// `WAVEFORMATEXTENSIBLE`（`KSDATAFORMAT_SUBTYPE_PCM`）。
    pub const fn dop_wire(&self) -> Option<DeviceFormat> {
        let Some(DsdTransport::Dop(c)) = self.dsd else {
            return None;
        };
        let (bits, placement) = match c {
            DopContainer::S24_3Le => (24, Placement::MsbAligned),
            DopContainer::S32LeMsb => (32, Placement::MsbAligned),
            DopContainer::S24LeLsb => (32, Placement::LsbAligned),
        };
        match Self::with_layout(
            self.device_rate(),
            self.layout,
            bits,
            24,
            Endian::Little,
            placement,
        ) {
            Ok(f) => Some(f),
            Err(_) => None,
        }
    }

    /// 一帧静音：PCM 为全零；原生 DSD 为全 `0x69`；DoP 为每个设备帧的标记（`0x05` / `0xFA`）加
    /// `0x6969`，按容器放置（D1）。渲染循环在暂停、欠载、尾部与排空时按帧重复它。
    pub const fn silence_frame(&self) -> SilenceFrame {
        let mut bytes = [0u8; MAX_FRAME_BYTES];
        let len = self.block_align as usize;
        match self.dsd {
            None => {}
            Some(DsdTransport::Native(_)) => {
                let mut i = 0;
                while i < len {
                    bytes[i] = DSD_SILENCE;
                    i += 1;
                }
            }
            Some(DsdTransport::Dop(c)) => {
                let dev = self.device_frame_bytes() as usize;
                let sb = (self.container_bits / 8) as usize;
                let mut i = 0;
                while i < len {
                    let frame = i / dev;
                    let marker = DOP_MARKERS[frame % 2] as u32;
                    let v = (marker << 16) | ((DSD_SILENCE as u32) << 8) | DSD_SILENCE as u32;
                    let le = dop_bytes(v, c);
                    let mut k = 0;
                    while k < sb {
                        bytes[i + k] = le[k];
                        k += 1;
                    }
                    i += sb;
                }
            }
        }
        SilenceFrame { bytes, len }
    }

    /// WASAPI 浮点设备格式（小端，布局为掩码顺序；`KSDATAFORMAT_SUBTYPE_IEEE_FLOAT`）。
    pub const fn wasapi_float(
        sample_rate: u32,
        layout: ChannelLayout,
        encoding: SampleEncoding,
    ) -> Result<Self, FormatError> {
        if !layout.is_wave_order() {
            return Err(FormatError::LayoutOrder);
        }
        Self::float_layout(sample_rate, layout, encoding, Endian::Little)
    }

    /// WASAPI PCM 设备格式（布局为掩码顺序）。
    pub const fn wasapi_layout(
        sample_rate: u32,
        layout: ChannelLayout,
        container_bits: u8,
        valid_bits: u8,
    ) -> Result<Self, FormatError> {
        if !layout.is_wave_order() {
            return Err(FormatError::LayoutOrder);
        }
        Self::with_layout(
            sample_rate,
            layout,
            container_bits,
            valid_bits,
            Endian::Little,
            Placement::MsbAligned,
        )
    }

    /// WASAPI PCM 设备格式：小端、有效位高位对齐（WAVEFORMATEXTENSIBLE 语义）。
    pub const fn wasapi(
        sample_rate: u32,
        channels: u16,
        container_bits: u8,
        valid_bits: u8,
    ) -> Result<Self, FormatError> {
        Self::new(
            sample_rate,
            channels,
            container_bits,
            valid_bits,
            Endian::Little,
            Placement::MsbAligned,
        )
    }

    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub const fn channels(&self) -> u16 {
        self.channels
    }

    /// 容器位宽 C。
    pub const fn container_bits(&self) -> u8 {
        self.container_bits
    }

    /// 有效位数 V。
    pub const fn valid_bits(&self) -> u8 {
        self.valid_bits
    }

    pub const fn endian(&self) -> Endian {
        self.endian
    }

    pub const fn placement(&self) -> Placement {
        self.placement
    }

    /// 一帧（所有声道一个样本）的字节数。
    pub const fn block_align(&self) -> u16 {
        self.block_align
    }

    pub const fn bytes_per_sec(&self) -> u32 {
        self.bytes_per_sec
    }

    /// 设备声道布局（设备上的样本顺序）。
    pub const fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// 样本编码（F3）。
    pub const fn encoding(&self) -> SampleEncoding {
        self.encoding
    }

    /// 源格式能否无损放入本设备格式：采样率一致、编码相同、扬声器位置集合相同（顺序可以不同，
    /// 不同时由 pack 纯置换，见 [`DeviceFormat::reorders`]）；整数还要求 N ≤ V。浮点源只能放入
    /// 同一浮点编码的设备格式（按位复制）。
    pub const fn accepts(&self, src: &SourceFormat) -> bool {
        src.sample_rate == self.sample_rate
            && src.layout.same_positions(&self.layout)
            && same_encoding(src.encoding, self.encoding)
            && (src.encoding.is_float() || src.encoding.is_dsd() || src.bits <= self.valid_bits)
    }

    /// 承载 `src` 时是否需要按设备映射重排声道（位置集合相同而顺序不同）。
    pub fn reorders(&self, src: &SourceFormat) -> bool {
        src.layout.same_positions(&self.layout) && src.layout != self.layout
    }
}

/// 两个编码是否相同（`const` 上下文不能用 `==`）。
pub const fn same_encoding(a: SampleEncoding, b: SampleEncoding) -> bool {
    matches!(
        (a, b),
        (SampleEncoding::Int, SampleEncoding::Int)
            | (SampleEncoding::Float32, SampleEncoding::Float32)
            | (SampleEncoding::Float64, SampleEncoding::Float64)
            | (SampleEncoding::Dsd, SampleEncoding::Dsd)
    )
}

/// 24 位 DoP 值 `v`（标记在 bit 23–16）按容器写成的小端字节（前 `sample_bytes` 个有效）。
pub const fn dop_bytes(v: u32, c: DopContainer) -> [u8; 4] {
    match c {
        DopContainer::S24_3Le => [v as u8, (v >> 8) as u8, (v >> 16) as u8, 0],
        DopContainer::S32LeMsb => [0, v as u8, (v >> 8) as u8, (v >> 16) as u8],
        // 低 24 位，高字节为 bit 23 的符号扩展（ALSA S24_LE）。
        DopContainer::S24LeLsb => {
            let hi = if v & 0x80_0000 != 0 { 0xFF } else { 0 };
            [v as u8, (v >> 8) as u8, (v >> 16) as u8, hi]
        }
    }
}

/// 给定采样率下 `millis` 毫秒对应的帧数（向上取整）。
pub const fn frames_for_millis(sample_rate: u32, millis: u32) -> u64 {
    (sample_rate as u64 * millis as u64).div_ceil(1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// E1：有损标记默认关闭；换布局、换编码后保持；不影响设备是否接受。
    #[test]
    fn lossy_flag_is_kept_and_does_not_affect_acceptance() {
        let Ok(f) = SourceFormat::new(44_100, 2, 24) else {
            panic!("格式");
        };
        assert!(!f.is_lossy());
        let l = f.lossy();
        assert!(l.is_lossy() && l != f);
        assert_eq!((l.sample_rate(), l.channels(), l.bits()), (44_100, 2, 24));
        assert!(
            l.relayout(ChannelLayout::STEREO)
                .is_ok_and(|x| x.is_lossy())
        );
        assert!(
            l.with_encoding(SampleEncoding::Int, 16)
                .is_ok_and(|x| x.is_lossy() && x.bits() == 16)
        );
        assert!(
            l.with_encoding(SampleEncoding::Float32, 32)
                .is_ok_and(|x| x.is_lossy())
        );
        assert!(
            f.relayout(ChannelLayout::STEREO)
                .is_ok_and(|x| !x.is_lossy())
        );
        let Ok(d) = DeviceFormat::new(44_100, 2, 32, 24, Endian::Little, Placement::MsbAligned)
        else {
            panic!("设备格式");
        };
        assert_eq!(d.accepts(&l), d.accepts(&f));
        assert!(d.accepts(&l));
    }

    #[test]
    fn device_format_derived_fields() {
        let f = DeviceFormat::new(44_100, 2, 16, 16, Endian::Little, Placement::MsbAligned);
        assert_eq!(
            f.map(|f| (f.block_align(), f.bytes_per_sec())),
            Ok((4, 176_400))
        );
        let f = DeviceFormat::new(96_000, 2, 32, 24, Endian::Little, Placement::MsbAligned);
        assert_eq!(
            f.map(|f| (f.block_align(), f.bytes_per_sec())),
            Ok((8, 768_000))
        );
    }

    #[test]
    fn device_format_rejects_invalid() {
        let le = Endian::Little;
        let p = Placement::MsbAligned;
        assert_eq!(
            DeviceFormat::new(0, 2, 16, 16, le, p),
            Err(FormatError::SampleRate)
        );
        // 采样率不设软件上限（M3 T1）：只有派生量溢出才拒绝。
        assert!(DeviceFormat::new(384_000, 2, 16, 16, le, p).is_ok());
        assert!(DeviceFormat::new(768_000, 2, 32, 32, le, p).is_ok());
        assert_eq!(
            DeviceFormat::new(u32::MAX, 2, 32, 32, le, p),
            Err(FormatError::Overflow)
        );
        assert_eq!(
            DeviceFormat::new(48_000, 0, 16, 16, le, p),
            Err(FormatError::Channels)
        );
        // F1：多于 2 声道没有默认布局，必须用 with_layout 给出（tasks/F1.md §1）。
        assert_eq!(
            DeviceFormat::new(48_000, 3, 16, 16, le, p),
            Err(FormatError::UndefinedLayout)
        );
        let Some(l51) = ChannelLayout::from_mask(0x3F) else {
            panic!("5.1");
        };
        let d = DeviceFormat::with_layout(48_000, l51, 24, 24, le, p);
        assert_eq!(d.map(|d| (d.channels(), d.block_align())), Ok((6, 18)));
        let Some(hda) = ChannelLayout::from_positions(&[
            Speaker::FrontLeft,
            Speaker::FrontRight,
            Speaker::BackLeft,
            Speaker::BackRight,
            Speaker::FrontCenter,
            Speaker::LowFrequency,
        ]) else {
            panic!("hda");
        };
        assert_eq!(
            DeviceFormat::wasapi_layout(48_000, hda, 32, 24),
            Err(FormatError::LayoutOrder)
        );
        assert!(DeviceFormat::wasapi_layout(48_000, l51, 32, 24).is_ok());
        assert_eq!(
            DeviceFormat::new(48_000, 2, 8, 8, le, p),
            Err(FormatError::ContainerBits)
        );
        assert_eq!(
            DeviceFormat::new(48_000, 2, 32, 0, le, p),
            Err(FormatError::ValidBits)
        );
        assert_eq!(
            DeviceFormat::new(48_000, 2, 24, 32, le, p),
            Err(FormatError::ValidBits)
        );
    }

    #[test]
    fn source_format_and_accepts() {
        let dev = DeviceFormat::new(48_000, 2, 32, 24, Endian::Little, Placement::MsbAligned);
        let Ok(dev) = dev else {
            panic!("valid format rejected")
        };
        let src16 = SourceFormat::new(48_000, 2, 16);
        let src24 = SourceFormat::new(48_000, 2, 24);
        let src32 = SourceFormat::new(48_000, 2, 32);
        let mono = SourceFormat::new(48_000, 1, 16);
        let other_rate = SourceFormat::new(44_100, 2, 16);
        assert_eq!(src16.map(|s| dev.accepts(&s)), Ok(true));
        assert_eq!(src24.map(|s| dev.accepts(&s)), Ok(true));
        assert_eq!(src32.map(|s| dev.accepts(&s)), Ok(false));
        assert_eq!(mono.map(|s| dev.accepts(&s)), Ok(false));
        assert_eq!(other_rate.map(|s| dev.accepts(&s)), Ok(false));
        assert_eq!(SourceFormat::new(48_000, 2, 0), Err(FormatError::ValidBits));
        assert_eq!(
            SourceFormat::new(48_000, 6, 16),
            Err(FormatError::UndefinedLayout)
        );
        // 位置集合相同、顺序不同：可承载（pack 纯置换）；集合不同：不可承载。
        let (Some(wave51), Some(side51)) = (
            ChannelLayout::from_mask(0x3F),
            ChannelLayout::from_mask(0x60F),
        ) else {
            panic!("layouts");
        };
        let Some(hda) = ChannelLayout::from_positions(&[
            Speaker::FrontLeft,
            Speaker::FrontRight,
            Speaker::BackLeft,
            Speaker::BackRight,
            Speaker::FrontCenter,
            Speaker::LowFrequency,
        ]) else {
            panic!("hda");
        };
        let le = Endian::Little;
        let lsb = Placement::LsbAligned;
        let (Ok(src), Ok(dev_hda), Ok(dev_side)) = (
            SourceFormat::with_layout(48_000, wave51, 24),
            DeviceFormat::with_layout(48_000, hda, 32, 24, le, lsb),
            DeviceFormat::with_layout(48_000, side51, 32, 24, le, lsb),
        ) else {
            panic!("formats");
        };
        assert!(dev_hda.accepts(&src) && dev_hda.reorders(&src));
        assert!(!dev_side.accepts(&src) && !dev_side.reorders(&src));
        assert_eq!(
            SourceFormat::new(48_000, 2, 33),
            Err(FormatError::ValidBits)
        );
    }

    /// F3：浮点格式的构造、派生字段与承载判断。
    #[test]
    fn float_formats() {
        let st = ChannelLayout::STEREO;
        let Ok(s32) = SourceFormat::float(96_000, st, SampleEncoding::Float32) else {
            panic!()
        };
        assert_eq!((s32.bits(), s32.words_per_frame()), (32, 2));
        let Ok(s64) = SourceFormat::float(96_000, st, SampleEncoding::Float64) else {
            panic!()
        };
        assert_eq!((s64.bits(), s64.words_per_frame()), (64, 4));
        assert_eq!(
            SourceFormat::float(96_000, st, SampleEncoding::Int),
            Err(FormatError::Encoding)
        );
        assert_eq!(
            SourceFormat::float(0, st, SampleEncoding::Float32),
            Err(FormatError::SampleRate)
        );
        let Ok(d64) = DeviceFormat::wasapi_float(96_000, st, SampleEncoding::Float64) else {
            panic!()
        };
        assert_eq!(
            (
                d64.container_bits(),
                d64.valid_bits(),
                d64.block_align(),
                d64.bytes_per_sec()
            ),
            (64, 64, 16, 1_536_000)
        );
        let Ok(d32) = DeviceFormat::wasapi_float(96_000, st, SampleEncoding::Float32) else {
            panic!()
        };
        assert!(d32.accepts(&s32) && !d32.accepts(&s64) && d64.accepts(&s64));
        let Ok(i32dev) = DeviceFormat::wasapi(96_000, 2, 32, 32) else {
            panic!()
        };
        let Ok(int24) = SourceFormat::new(96_000, 2, 24) else {
            panic!()
        };
        assert!(!i32dev.accepts(&s32) && !d32.accepts(&int24) && i32dev.accepts(&int24));
        assert_eq!(s32.with_encoding(SampleEncoding::Int, 24), Ok(int24));
    }

    #[test]
    fn frames_for_half_second() {
        assert_eq!(frames_for_millis(192_000, 500), 96_000);
        assert_eq!(frames_for_millis(44_100, 500), 22_050);
        assert_eq!(frames_for_millis(11_025, 500), 5_513);
    }
}
