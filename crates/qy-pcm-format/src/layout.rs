//! 声道布局（F1）：每个声道对应的扬声器位置。
//!
//! 位置集合取 WAVE 的 18 个 `SPEAKER_*` 位（Microsoft `WAVEFORMATEXTENSIBLE` /
//! ksmedia.h；位序号 0–17），[`Speaker`] 的判别值即位序号。布局按**样本顺序**列出位置：
//! - WAV EXTENSIBLE 与 WASAPI：样本按掩码位从低到高排列（[`ChannelLayout::from_mask`]）；
//! - FLAC：RFC 9639 §9.1.3 规定 1–8 声道的固定顺序（[`ChannelLayout::flac_default`]），
//!   这 8 种顺序都等于对应掩码按位从低到高的顺序；
//! - ALSA：设备的声道映射可以是任意顺序（见 `crate::alsa` 的 chmap 对应表）。
//!
//! 单声道布局为 `[FrontCenter]`，双声道为 `[FrontLeft, FrontRight]`。多于 2 声道的源必须
//! 由文件声明布局，未声明时明确拒绝（tasks/F1.md §1、§6）。

/// 扬声器位置（判别值 = WAVE `SPEAKER_*` 位序号）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Speaker {
    FrontLeft = 0,
    FrontRight = 1,
    FrontCenter = 2,
    LowFrequency = 3,
    BackLeft = 4,
    BackRight = 5,
    FrontLeftOfCenter = 6,
    FrontRightOfCenter = 7,
    BackCenter = 8,
    SideLeft = 9,
    SideRight = 10,
    TopCenter = 11,
    TopFrontLeft = 12,
    TopFrontCenter = 13,
    TopFrontRight = 14,
    TopBackLeft = 15,
    TopBackCenter = 16,
    TopBackRight = 17,
}

/// 布局可表示的最大声道数（WAVE 扬声器位置数）。
pub const MAX_LAYOUT_CHANNELS: usize = 18;

const ALL: [Speaker; MAX_LAYOUT_CHANNELS] = [
    Speaker::FrontLeft,
    Speaker::FrontRight,
    Speaker::FrontCenter,
    Speaker::LowFrequency,
    Speaker::BackLeft,
    Speaker::BackRight,
    Speaker::FrontLeftOfCenter,
    Speaker::FrontRightOfCenter,
    Speaker::BackCenter,
    Speaker::SideLeft,
    Speaker::SideRight,
    Speaker::TopCenter,
    Speaker::TopFrontLeft,
    Speaker::TopFrontCenter,
    Speaker::TopFrontRight,
    Speaker::TopBackLeft,
    Speaker::TopBackCenter,
    Speaker::TopBackRight,
];

impl Speaker {
    /// 由 WAVE 位序号取位置。
    pub const fn from_bit(bit: u32) -> Option<Speaker> {
        if bit < MAX_LAYOUT_CHANNELS as u32 {
            Some(ALL[bit as usize])
        } else {
            None
        }
    }

    /// WAVE `SPEAKER_*` 掩码位。
    pub const fn mask(self) -> u32 {
        1 << (self as u8)
    }

    /// 简称（报告用）。
    pub const fn abbrev(self) -> &'static str {
        match self {
            Speaker::FrontLeft => "FL",
            Speaker::FrontRight => "FR",
            Speaker::FrontCenter => "FC",
            Speaker::LowFrequency => "LFE",
            Speaker::BackLeft => "BL",
            Speaker::BackRight => "BR",
            Speaker::FrontLeftOfCenter => "FLC",
            Speaker::FrontRightOfCenter => "FRC",
            Speaker::BackCenter => "BC",
            Speaker::SideLeft => "SL",
            Speaker::SideRight => "SR",
            Speaker::TopCenter => "TC",
            Speaker::TopFrontLeft => "TFL",
            Speaker::TopFrontCenter => "TFC",
            Speaker::TopFrontRight => "TFR",
            Speaker::TopBackLeft => "TBL",
            Speaker::TopBackCenter => "TBC",
            Speaker::TopBackRight => "TBR",
        }
    }
}

/// 声道布局：按样本顺序的扬声器位置，位置互不重复。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ChannelLayout {
    positions: [Speaker; MAX_LAYOUT_CHANNELS],
    len: u8,
}

impl core::fmt::Debug for ChannelLayout {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("[")?;
        for (i, p) in self.positions().iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            f.write_str(p.abbrev())?;
        }
        f.write_str("]")
    }
}

impl core::fmt::Display for ChannelLayout {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(self, f)
    }
}

/// FLAC 声道分配 0–7 的位置顺序（RFC 9639 §9.1.3）。
const FLAC_DEFAULT_MASKS: [u32; 8] = [
    0x4,   // 1：单声道（FC）
    0x3,   // 2：左、右
    0x7,   // 3：左、右、中
    0x33,  // 4：前左、前右、后左、后右
    0x37,  // 5：前左、前右、中、后左、后右
    0x3F,  // 6：前左、前右、中、低音、后左、后右
    0x70F, // 7：前左、前右、中、低音、后中、侧左、侧右
    0x63F, // 8：前左、前右、中、低音、后左、后右、侧左、侧右
];

impl ChannelLayout {
    /// 单声道 `[FC]`。
    pub const MONO: ChannelLayout = match Self::from_mask(0x4) {
        Some(l) => l,
        None => panic!("mono"),
    };

    /// 双声道 `[FL FR]`。
    pub const STEREO: ChannelLayout = match Self::from_mask(0x3) {
        Some(l) => l,
        None => panic!("stereo"),
    };

    /// 由 WAVE 掩码构造：样本按掩码位从低到高排列。掩码为 0 或含第 18 位及以上时返回 `None`。
    pub const fn from_mask(mask: u32) -> Option<ChannelLayout> {
        if mask == 0 || mask >> MAX_LAYOUT_CHANNELS != 0 {
            return None;
        }
        let mut positions = [Speaker::FrontLeft; MAX_LAYOUT_CHANNELS];
        let mut len = 0usize;
        let mut bit = 0u32;
        while bit < MAX_LAYOUT_CHANNELS as u32 {
            if mask & (1 << bit) != 0 {
                positions[len] = ALL[bit as usize];
                len += 1;
            }
            bit += 1;
        }
        Some(ChannelLayout {
            positions,
            len: len as u8,
        })
    }

    /// 由按样本顺序的位置列表构造；为空、超过 18 个或有重复位置时返回 `None`。
    pub const fn from_positions(list: &[Speaker]) -> Option<ChannelLayout> {
        if list.is_empty() || list.len() > MAX_LAYOUT_CHANNELS {
            return None;
        }
        let mut positions = [Speaker::FrontLeft; MAX_LAYOUT_CHANNELS];
        let mut seen = 0u32;
        let mut i = 0;
        while i < list.len() {
            let m = list[i].mask();
            if seen & m != 0 {
                return None;
            }
            seen |= m;
            positions[i] = list[i];
            i += 1;
        }
        Some(ChannelLayout {
            positions,
            len: list.len() as u8,
        })
    }

    /// 单声道 / 双声道的默认布局；其他声道数没有默认布局。
    pub const fn default_for(channels: u16) -> Option<ChannelLayout> {
        match channels {
            1 => Some(Self::MONO),
            2 => Some(Self::STEREO),
            _ => None,
        }
    }

    /// FLAC 独立声道分配（1–8 声道）的默认布局（RFC 9639 §9.1.3）。
    pub const fn flac_default(channels: u16) -> Option<ChannelLayout> {
        if channels == 0 || channels > 8 {
            return None;
        }
        Self::from_mask(FLAC_DEFAULT_MASKS[channels as usize - 1])
    }

    /// 声道数。
    pub const fn channels(&self) -> u16 {
        self.len as u16
    }

    /// 按样本顺序的位置。
    pub fn positions(&self) -> &[Speaker] {
        &self.positions[..usize::from(self.len)]
    }

    /// 位置集合的 WAVE 掩码（与顺序无关）。
    pub const fn mask(&self) -> u32 {
        let mut m = 0;
        let mut i = 0;
        while i < self.len as usize {
            m |= self.positions[i].mask();
            i += 1;
        }
        m
    }

    /// 是否按掩码位从低到高排列（WAVE / WASAPI 能直接表示的顺序）。
    pub const fn is_wave_order(&self) -> bool {
        let mut i = 1;
        while i < self.len as usize {
            if (self.positions[i - 1] as u8) >= (self.positions[i] as u8) {
                return false;
            }
            i += 1;
        }
        true
    }

    /// 两个布局的位置集合是否相同（顺序可以不同）。
    pub const fn same_positions(&self, other: &ChannelLayout) -> bool {
        self.len == other.len && self.mask() == other.mask()
    }

    /// 位置在本布局中的样本下标。
    pub fn index_of(&self, s: Speaker) -> Option<usize> {
        self.positions().iter().position(|&p| p == s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Speaker::*;

    #[test]
    fn masks_and_orders() {
        assert_eq!(ChannelLayout::MONO.positions(), &[FrontCenter]);
        assert_eq!(ChannelLayout::STEREO.positions(), &[FrontLeft, FrontRight]);
        // 独立写出 RFC 9639 §9.1.3 的顺序逐项核对。
        let expect: [&[Speaker]; 8] = [
            &[FrontCenter],
            &[FrontLeft, FrontRight],
            &[FrontLeft, FrontRight, FrontCenter],
            &[FrontLeft, FrontRight, BackLeft, BackRight],
            &[FrontLeft, FrontRight, FrontCenter, BackLeft, BackRight],
            &[
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                BackLeft,
                BackRight,
            ],
            &[
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                BackCenter,
                SideLeft,
                SideRight,
            ],
            &[
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                BackLeft,
                BackRight,
                SideLeft,
                SideRight,
            ],
        ];
        for (i, e) in expect.iter().enumerate() {
            let Some(l) = ChannelLayout::flac_default(i as u16 + 1) else {
                panic!("{i}");
            };
            assert_eq!(l.positions(), *e, "{} 声道", i + 1);
            assert!(l.is_wave_order());
        }
        assert_eq!(ChannelLayout::flac_default(0), None);
        assert_eq!(ChannelLayout::flac_default(9), None);
        assert_eq!(ChannelLayout::default_for(3), None);
        // 5.1 侧环绕 0x60F。
        let Some(side) = ChannelLayout::from_mask(0x60F) else {
            panic!("0x60F");
        };
        assert_eq!(
            side.positions(),
            &[
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                SideLeft,
                SideRight
            ]
        );
        assert_eq!(side.mask(), 0x60F);
        assert_eq!(ChannelLayout::from_mask(0), None);
        assert_eq!(ChannelLayout::from_mask(1 << 18), None);
        assert_eq!(
            ChannelLayout::from_mask(0x3FFFF).map(|l| l.channels()),
            Some(18)
        );
        assert_eq!(Speaker::from_bit(17), Some(TopBackRight));
        assert_eq!(Speaker::from_bit(18), None);
    }

    #[test]
    fn positions_order_and_sets() {
        // ALSA HDA 常见的 5.1 顺序：FL FR RL RR FC LFE。
        let Some(hda) = ChannelLayout::from_positions(&[
            FrontLeft,
            FrontRight,
            BackLeft,
            BackRight,
            FrontCenter,
            LowFrequency,
        ]) else {
            panic!("hda");
        };
        let Some(wave) = ChannelLayout::from_mask(0x3F) else {
            panic!("wave");
        };
        assert!(!hda.is_wave_order() && wave.is_wave_order());
        assert!(hda.same_positions(&wave) && hda != wave);
        assert_eq!(hda.index_of(FrontCenter), Some(4));
        assert_eq!(wave.index_of(FrontCenter), Some(2));
        let Some(side) = ChannelLayout::from_mask(0x60F) else {
            panic!("side");
        };
        assert!(!side.same_positions(&wave));
        assert_eq!(ChannelLayout::from_positions(&[FrontLeft, FrontLeft]), None);
        assert_eq!(ChannelLayout::from_positions(&[]), None);
        extern crate std;
        assert_eq!(std::format!("{hda}"), "[FL FR BL BR FC LFE]");
    }
}
