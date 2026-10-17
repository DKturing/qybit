//! 资源配置档（M3 T1）：Desktop / Embedded Lite / Embedded Full 共用同一套代码，只是声明的能力与
//! 启动时分配的缓冲不同。各缓冲大小与 RAM 合计由 [`Profile::budget`] 按公式计算，不手写常量。
//!
//! - 设备帧队列：预算采样率 × 预填时长 × 声道 × 容器字节（[`queue_bytes`]），静态队列再加
//!   [`BIP_MARGIN_FRAMES`] 帧。这是 RAM 的最大头，WAV 与 FLAC 共用；
//! - 输入窗口：FLAC 不压缩帧上界（[`Capacity::for_limits`]），不小于核心最小窗口；
//! - 流式 PCM 工作区：固定 [`PCM_WORK_SAMPLES`] 个 `i32`（流式解码，大小与格式无关），WAV 与 AIFF
//!   共用一块（F2），启用 AIFF 不增加 RAM；
//! - FLAC 工作区（`i64`）与 PCM（`i32`）：块大小 × 声道；
//! - pack 暂存区：只有 `rtrb` 队列需要（静态队列直接 pack 进队列存储）。
//!
//! 桌面不设采样率上限：队列按参考采样率 768 kHz（双声道 C32、500 ms）分配；更高的源由引擎
//! 按“队列不足”明确拒绝，而不是按采样率拒绝。MCU 配置档的 250 ms 预填是暂定值，M3 T2 按实测的
//! SD 卡最坏读取延迟加余量推出后回写。

use qy_core::engine::{BIP_MARGIN_FRAMES, Capabilities, PACK_SCRATCH_BYTES, queue_bytes};
use qy_core::{INPUT_WINDOW_BYTES, PCM_WORK_SAMPLES};

#[cfg(feature = "flac")]
use qy_codec_flac::Capacity;

/// 队列实现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueKind {
    /// `rtrb`（需要 alloc 与 pack 暂存区）。
    Ring,
    /// 静态 `bbqueue`（直接 pack 进存储，另留 [`BIP_MARGIN_FRAMES`] 帧余量）。
    Static,
}

/// 一个配置档。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    pub name: &'static str,
    /// 交给引擎的能力声明（其中 `queue_bytes` 为队列须容纳的数据字节数）。
    pub caps: Capabilities,
    /// 队列预算所按的采样率（桌面不设采样率上限时为参考值）。
    pub budget_rate: u32,
    /// 队列预算所按的设备容器字节数。
    pub container_bytes: usize,
    pub queue: QueueKind,
    /// 是否启用 WAV（需要 `wav` 特性）。
    pub wav: bool,
    /// 是否启用 AIFF / AIFF-C（需要 `aiff` 特性；与 WAV 共用流式工作区）。
    pub aiff: bool,
    /// FLAC 容量；`None` 表示不启用 FLAC。
    #[cfg(feature = "flac")]
    pub flac: Option<Capacity>,
    /// 浮点转整数的工作区样本数（F3，明确开启的选项）；0 表示不提供，开启时按资源不足拒绝。
    pub convert_samples: usize,
}

/// 各缓冲字节数与合计。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub input_window: usize,
    /// 流式 PCM 工作区（WAV 与 AIFF 共用）。
    pub wav_pcm: usize,
    pub flac_workspace: usize,
    pub flac_pcm: usize,
    pub pack_scratch: usize,
    /// 浮点转整数的工作区字节数（F3）。
    pub convert: usize,
    /// 队列存储字节数（含静态队列余量）。
    pub queue: usize,
    pub total: usize,
}

const fn data_bytes(rate: u32, millis: u32, channels: u16, container: usize) -> usize {
    match queue_bytes(rate, millis, channels, container) {
        Some(b) => b,
        None => panic!("队列预算溢出"),
    }
}

impl Profile {
    /// 由目标能力推出配置档：队列数据预算 = `budget_rate` 下 `prefill_ms` 的
    /// `max_channels` × `container_bytes` 字节帧。
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        name: &'static str,
        max_rate: u32,
        budget_rate: u32,
        max_channels: u16,
        max_bits: u8,
        container_bytes: usize,
        prefill_ms: u32,
        queue: QueueKind,
    ) -> Self {
        Self {
            name,
            caps: Capabilities {
                max_rate,
                max_channels,
                max_bits,
                queue_bytes: data_bytes(budget_rate, prefill_ms, max_channels, container_bytes),
                prefill_ms,
            },
            budget_rate,
            container_bytes,
            queue,
            wav: true,
            aiff: true,
            #[cfg(feature = "flac")]
            flac: None,
            convert_samples: 0,
        }
    }

    /// 提供浮点转整数的工作区（样本数，F3）。一次转换的帧数受它限制，大小与流式 PCM 工作区相同
    /// 即可一次转完一批解码输出。
    pub const fn with_convert(mut self, samples: usize) -> Self {
        self.convert_samples = samples;
        self
    }

    /// 提高能力声明中的最大声道数，不改变队列预算（字节数仍按构造时的参考声道数计算；
    /// 声道更多时可承载的采样率相应降低，超出时引擎按“队列不足”拒绝）。
    pub const fn with_max_channels(mut self, channels: u16) -> Self {
        self.caps.max_channels = channels;
        self
    }

    /// 启用/停用 WAV。
    pub const fn with_wav(mut self, on: bool) -> Self {
        self.wav = on;
        self
    }

    /// 启用/停用 AIFF。
    pub const fn with_aiff(mut self, on: bool) -> Self {
        self.aiff = on;
        self
    }

    /// 启用 FLAC 并指定容量。
    #[cfg(feature = "flac")]
    pub const fn with_flac(mut self, cap: Capacity) -> Self {
        self.flac = Some(cap);
        self
    }

    /// 本构建（按特性）实际启用的 FLAC 容量。
    #[cfg(feature = "flac")]
    pub const fn flac_capacity(&self) -> Option<Capacity> {
        self.flac
    }

    /// 本构建是否实际启用 WAV。
    pub const fn wav_enabled(&self) -> bool {
        cfg!(feature = "wav") && self.wav
    }

    /// 本构建是否实际启用 AIFF。
    pub const fn aiff_enabled(&self) -> bool {
        cfg!(feature = "aiff") && self.aiff
    }

    /// 队列存储字节数：数据预算；静态队列另加 [`BIP_MARGIN_FRAMES`] 帧。
    pub const fn queue_storage_bytes(&self) -> usize {
        match self.queue {
            QueueKind::Ring => self.caps.queue_bytes,
            QueueKind::Static => {
                self.caps.queue_bytes
                    + BIP_MARGIN_FRAMES * self.caps.max_channels as usize * self.container_bytes
            }
        }
    }

    /// 各缓冲大小与 RAM 合计。
    pub const fn budget(&self) -> Budget {
        #[allow(unused_mut)]
        let (mut window, mut ws, mut pcm) = (INPUT_WINDOW_BYTES, 0, 0);
        #[cfg(feature = "flac")]
        if let Some(c) = self.flac {
            if c.input_window > window {
                window = c.input_window;
            }
            ws = c.workspace_len() * 8;
            pcm = c.pcm_len() * 4;
        }
        let wav_pcm = if self.wav_enabled() || self.aiff_enabled() {
            PCM_WORK_SAMPLES * 4
        } else {
            0
        };
        let pack_scratch = match self.queue {
            QueueKind::Ring => PACK_SCRATCH_BYTES,
            QueueKind::Static => 0,
        };
        let queue = self.queue_storage_bytes();
        let convert = self.convert_samples * 4;
        Budget {
            input_window: window,
            wav_pcm,
            flac_workspace: ws,
            flac_pcm: pcm,
            pack_scratch,
            convert,
            queue,
            total: window + wav_pcm + ws + pcm + pack_scratch + convert + queue,
        }
    }
}

/// 桌面：不设采样率上限；队列按 768 kHz / 双声道 / C32 / 500 ms 分配（3 072 000 字节，
/// 等于 192 kHz / 8 声道 / C32 / 500 ms），最多 18 声道（布局可描述的上限，F1），FLAC 桌面
/// 多声道容量（块 65 535、8 声道、32 位），`rtrb` 队列；浮点转整数工作区与流式 PCM 工作区同大
/// （F3）。
#[cfg(feature = "flac")]
pub const DESKTOP: Profile =
    Profile::new("Desktop", u32::MAX, 768_000, 2, 32, 4, 500, QueueKind::Ring)
        .with_flac(Capacity::DESKTOP_MULTICHANNEL)
        .with_max_channels(qy_core::MAX_CHANNELS)
        .with_convert(PCM_WORK_SAMPLES);

/// 桌面最小配置（B1）：与 [`DESKTOP`] 相同的能力与队列，只链接 WAV 与 AIFF（不需要 `flac` 特性），
/// 没有 FLAC 工作区；FLAC 文件按“不支持”拒绝。
pub const DESKTOP_WAV: Profile = Profile::new(
    "Desktop (WAV/AIFF)",
    u32::MAX,
    768_000,
    2,
    32,
    4,
    500,
    QueueKind::Ring,
)
.with_max_channels(qy_core::MAX_CHANNELS)
.with_convert(PCM_WORK_SAMPLES);

/// 嵌入式完整版：WAV + AIFF + FLAC，≤ 96 kHz、双声道、≤ 24 位（I²S 32 位槽位），
/// FLAC 块 ≤ 4608（[`Capacity::MCU`]），预填 250 ms（暂定），静态队列；提供浮点转整数工作区
/// （F3：I²S 只输出整数，浮点文件只在明确开启转换时播放）。
#[cfg(feature = "flac")]
pub const EMBEDDED_FULL: Profile = Profile::new(
    "Embedded Full",
    96_000,
    96_000,
    2,
    24,
    4,
    250,
    QueueKind::Static,
)
.with_flac(Capacity::MCU)
.with_convert(PCM_WORK_SAMPLES);

/// 嵌入式精简版：WAV + AIFF（共用流式工作区，不链接 FLAC），≤ 48 kHz、双声道、16 位，预填 250 ms
/// （暂定），静态队列。只开 `wav`（或再加 `aiff`）特性即可构建。不提供浮点转整数工作区（F3：
/// 开启转换时按资源不足拒绝浮点文件）。
pub const EMBEDDED_LITE: Profile = Profile::new(
    "Embedded Lite",
    48_000,
    48_000,
    2,
    16,
    2,
    250,
    QueueKind::Static,
);

#[cfg(all(test, feature = "wav", feature = "flac"))]
mod tests {
    use super::*;

    /// 独立核对：按手算公式（帧数 × 帧宽 + 余量；FLAC 不压缩帧上界）得出的字节数。
    #[test]
    fn budgets_follow_formula() {
        let d = DESKTOP.budget();
        // 768000 × 0.5 = 384000 帧 × 8 字节。
        assert_eq!(d.queue, 3_072_000);
        // F1：FLAC 8 声道不压缩帧上界 16 + ⌈(64 + 65535 × 256) / 8⌉ + 2。
        assert_eq!(d.input_window, 2_097_146);
        assert_eq!(d.wav_pcm, 4096 * 2 * 4);
        assert_eq!(d.flac_workspace, 65_535 * 8 * 8);
        assert_eq!(d.flac_pcm, 65_535 * 8 * 4);
        assert_eq!(d.pack_scratch, 4096 * 8);
        assert_eq!(d.convert, 4096 * 2 * 4);
        assert_eq!(
            d.total,
            3_072_000 + 2_097_146 + 32_768 + 4_194_240 + 2_097_120 + 32_768 + 32_768
        );
        assert_eq!(DESKTOP.caps.max_channels, 18);
        assert_eq!(DESKTOP.caps.max_rate, u32::MAX);

        let f = EMBEDDED_FULL.budget();
        // 96000 × 0.25 = 24000 帧 × 8 字节 + 2 帧余量。
        assert_eq!(f.queue, 24_000 * 8 + 16);
        assert_eq!(f.input_window, 28_244);
        assert_eq!(f.flac_workspace, 4_608 * 2 * 8);
        assert_eq!(f.flac_pcm, 4_608 * 2 * 4);
        assert_eq!(f.pack_scratch, 0);
        assert_eq!(f.convert, 32_768);
        assert_eq!(
            f.total,
            192_016 + 28_244 + 32_768 + 73_728 + 36_864 + 32_768
        );

        let l = EMBEDDED_LITE.budget();
        // 48000 × 0.25 = 12000 帧 × 4 字节 + 2 帧余量；无 FLAC，输入窗口为核心最小窗口。
        assert_eq!(l.queue, 12_000 * 4 + 8);
        assert_eq!(l.input_window, 8_192);
        assert_eq!(
            (l.flac_workspace, l.flac_pcm, l.pack_scratch, l.convert),
            (0, 0, 0, 0)
        );
        assert_eq!(l.total, 48_008 + 8_192 + 32_768);
    }

    /// 能力检查：桌面接受 384 kHz / 24 位双声道；Embedded Full 拒绝 192 kHz 与 32 位。
    #[test]
    fn capability_checks_per_profile() {
        use qy_core::SourceFormat;
        use qy_core::engine::CapabilityError;
        let fmt = |r, c, b| match SourceFormat::new(r, c, b) {
            Ok(f) => f,
            Err(e) => panic!("{e:?}"),
        };
        assert_eq!(DESKTOP.caps.check(&fmt(384_000, 2, 24)), Ok(()));
        assert_eq!(DESKTOP.caps.check(&fmt(768_000, 2, 32)), Ok(()));
        assert_eq!(
            DESKTOP.caps.check(&fmt(1_536_000, 2, 32)),
            Err(CapabilityError::Queue {
                required: 6_144_000,
                available: 3_072_000
            })
        );
        assert_eq!(
            EMBEDDED_FULL.caps.check(&fmt(192_000, 2, 16)),
            Err(CapabilityError::Rate {
                rate: 192_000,
                max: 96_000
            })
        );
        assert_eq!(
            EMBEDDED_FULL.caps.check(&fmt(48_000, 2, 32)),
            Err(CapabilityError::Bits { bits: 32, max: 24 })
        );
        assert_eq!(EMBEDDED_LITE.caps.check(&fmt(44_100, 2, 16)), Ok(()));
    }
}

/// 桌面最小配置（B1）不依赖 `flac` 特性：没有 FLAC 工作区，队列与转换工作区与桌面相同（手算：
/// 768 000 Hz × 0.5 s × 2 声道 × 4 字节 = 3 072 000 字节；转换工作区 8192 × 4 = 32 768 字节）。
#[cfg(test)]
mod desktop_wav {
    use super::*;

    #[test]
    fn no_flac_buffers_same_queue() {
        let b = DESKTOP_WAV.budget();
        assert_eq!((b.flac_workspace, b.flac_pcm), (0, 0));
        assert_eq!(b.queue, 3_072_000);
        assert_eq!(b.convert, 32_768);
        // 核心最小输入窗口 + 流式 PCM + pack 暂存区 + 转换工作区 + 队列。
        assert_eq!(b.total, 8_192 + 32_768 + 32_768 + 32_768 + 3_072_000);
        #[cfg(feature = "flac")]
        assert_eq!(DESKTOP_WAV.flac_capacity(), None);
        assert_eq!(DESKTOP_WAV.caps.max_channels, qy_core::MAX_CHANNELS);
    }
}
