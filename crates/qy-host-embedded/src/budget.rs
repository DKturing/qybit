//! 预填与读卡时序的推导（M3 T2）：由最坏读卡延迟推出，不足时启动即明确报错。
//!
//! 记号（时间单位 µs）：
//! - `W`：最大输入单元字节数（FLAC：最大块的不压缩帧上界，即输入窗口；WAV：板级选定的统计窗口，
//!   例如 16 KiB）；`F`：一个最大输入单元至少包含的音频帧数；
//! - `U`：**读取文件中任意连续 `W` 字节的最长耗时**，含其间所有命令开销、偶发停顿与 FAT 簇链查找
//!   （板级实测：反复读取测试文件，取所有长度为 `W` 的窗口中的最大值）；
//! - `D`：解码并 pack 一个最大输入单元的最长耗时（板级实测）；
//! - `O`：**打开一首曲目的最长耗时**（板级实测：目录查找与读取目录项；不含簇链——FAT 存储打开时
//!   不沿整条簇链走，读取时逐跳核对，计入 `U`，见 [`crate::storage`]）；
//! - `T`：最长的 DMA 缓冲周期（最低采样率下一个缓冲的时长）；
//! - `R`：源的最大字节率（字节/秒，见 [`Demand`]）；`fs`：最高采样率；`P`：配置档的预填时长；
//! - `S`：**定位一次的最长耗时**（板级实测：在测试文件中跳到任意位置，含 FAT 簇链查找）；
//! - `J`、`k`：解码需要的读取跳转（E6，`Decoder::take_jump`）的次数上界——`J` 为一首曲目首次输出
//!   之前（文件头阶段，如索引在文件末尾的容器）至多几次，`k` 为数据阶段每个输入单元之前至多几次
//!   （块交错的容器）。由格式（插件）声明，官方格式都是 0。
//!
//! 按启用的每种格式分别核对（同一时刻只播放一种格式，各自的 `W`、`U`、`D` 不同），都满足才启动：
//! 1. **持续负载**：读取与解码占用的时间比例不超过 1：`R · (U + k·S) / W / 10⁶ + D · fs / F / 10⁶ ≤ 1`。
//!    读取是阻塞的（SPI/SDIO 传输占用主循环），所以读取时间与解码时间相加；每秒要读 `R / W` 个单元，
//!    每个至多 `U`，之前至多跳转 `k` 次。
//! 2. **预填**：`P ≥ O + J·(S + U) + U + k·S + D + T`。最坏情况下，开始读取一个最大输入单元时队列只剩
//!    预填量：读取这个单元（至多 `U`，之前至多跳转 `k` 次）、解码（至多 `D`），数据入队后还要等到
//!    下一个 DMA 缓冲边界（至多 `T`）才会被取走；这段时间内设备持续取数，队列不得取空。换曲时，在读取
//!    下一首的第一个单元之前还要先打开它（至多 `O`）并完成文件头阶段的跳转（每次定位至多 `S`、
//!    读取至多 `U`），期间设备仍在播放上一首排队的音频。条件 1 保证此后队列不会越来越空。
//! 3. **定位响应**（只报告，不作启动条件）：用户定位时引擎停止设备、重新打开曲目并定位，到目标处的
//!    第一个单元解码完成至多 `O + J·(S + U) + S + U + D`。
//!
//! 运行时，一首曲目首次输出之前的解析跳转超过声明的 `J` 时，宿主给出通知（见 `Notice`）；`k` 只按
//! 声明计入公式，运行时不核对。超出声明的文件仍可能欠载，欠载照常计数与报告。
//!
//! 未计入的部分：下一首的文件头阶段若要读过很大的元数据（例如 FLAC 中内嵌的大幅图片），读取量取决于
//! 文件本身，不能由板级常数给出上界；这类文件在换曲时仍可能欠载，欠载照常计数与报告。
//!
//! `U` 与 `D` 由板级按格式实测给出（M3 T2 实物验收与 T3 回写配置档）；本模块只按公式核对，不估算。

use core::fmt;

use qy_core::engine::Capabilities;

/// 一种格式的需求与板级实测时序。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Load {
    pub demand: Demand,
    /// 读取任意连续 `W` 字节的最长耗时 `U`（µs）。
    pub unit_read_us: u32,
    /// 解码并 pack 一个最大输入单元的最长耗时 `D`（µs）。
    pub decode_us: u32,
    /// 打开一首曲目的最长耗时 `O`（µs；同一存储对各格式相同）。
    pub open_us: u32,
    /// 首次输出之前至多几次解码需要的跳转 `J`（E6；格式声明，官方格式为 0）。
    pub header_jumps: u32,
    /// 数据阶段每个输入单元之前至多几次跳转 `k`（E6；格式声明，官方格式为 0）。
    pub unit_jumps: u32,
    /// 定位一次的最长耗时 `S`（µs；板级实测，同一存储对各格式相同；没有跳转的格式可为 0）。
    pub jump_us: u32,
}

/// 源对读取的需求（按配置档的能力上限计算）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Demand {
    /// 最大字节率 `R`（字节/秒）。
    pub bytes_per_sec: u64,
    /// 最大输入单元字节数 `W`。
    pub unit_bytes: u32,
    /// 最大输入单元至少包含的音频帧数 `F`。
    pub unit_frames: u32,
}

impl Demand {
    /// WAV：样本容器至多 4 字节；输入单元为 `unit_bytes` 字节（统计窗口）。
    pub const fn wav(caps: &Capabilities, unit_bytes: u32) -> Demand {
        let frame = caps.max_channels as u64 * 4;
        let frames = unit_bytes as u64 / frame;
        Demand {
            bytes_per_sec: caps.max_rate as u64 * frame,
            unit_bytes,
            unit_frames: if frames == 0 { 1 } else { frames as u32 },
        }
    }

    /// FLAC：`window` 为最大块 `max_block` 的不压缩帧上界（输入窗口）；最大字节率按最小块（16 帧）
    /// 的不压缩帧上界 `small_frame_bytes` 计算（块越小每帧开销越大）：`max_rate × small / 16`。
    pub const fn flac(
        caps: &Capabilities,
        window: u32,
        max_block: u32,
        small_frame_bytes: u32,
    ) -> Demand {
        Demand {
            bytes_per_sec: (caps.max_rate as u64 * small_frame_bytes as u64).div_ceil(16),
            unit_bytes: window,
            unit_frames: max_block,
        }
    }
}

/// 核对结果（用于报告）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Margins {
    /// 持续负载（千分比，≤ 1000）。
    pub load_permille: u64,
    /// 最坏情况所需预填 `O + J·(S + U) + U + k·S + D + T`（µs）。
    pub required_prefill_us: u64,
    /// 配置档的预填 `P`（µs）。
    pub prefill_us: u64,
    /// 定位响应 `O + J·(S + U) + S + U + D`（µs，只报告）。
    pub seek_response_us: u64,
    /// 首次输出之前至多几次解析跳转（各格式 `J` 的最大值）。
    pub header_jumps: u32,
}

/// 时序不足。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetError {
    /// 参数无效（没有声明任何格式，或单元为 0 字节或 0 帧）。
    Invalid,
    /// 读取与解码的持续负载超过 100%。
    Load { load_permille: u64 },
    /// 预填不足：需要 `required_us`，配置档只有 `prefill_us`。
    Prefill {
        required_us: u64,
        prefill_us: u64,
        open_us: u64,
        /// 解码需要的跳转：`J·(S + U) + k·S`。
        jumps_us: u64,
        read_us: u64,
        decode_us: u64,
        period_us: u64,
    },
}

impl fmt::Display for BudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid => {
                f.write_str("时序参数无效（没有声明任何格式，或输入单元为 0 字节或 0 帧）")
            }
            Self::Load { load_permille } => write!(
                f,
                "读卡与解码的持续负载为 {}.{}%，超过 100%",
                load_permille / 10,
                load_permille % 10
            ),
            Self::Prefill {
                required_us,
                prefill_us,
                open_us,
                jumps_us,
                read_us,
                decode_us,
                period_us,
            } => {
                write!(
                    f,
                    "预填 {prefill_us} µs 不足：最坏情况需要 {required_us} µs（换曲时打开曲目 {open_us}"
                )?;
                if *jumps_us > 0 {
                    write!(f, " + 解码需要的跳转 {jumps_us}")?;
                }
                write!(
                    f,
                    " + 读取最大输入单元 {read_us} + 解码 {decode_us} + DMA 周期 {period_us}）"
                )
            }
        }
    }
}

/// 按两个条件逐格式核对（见模块说明），返回最坏的余量。`period_us` 为最长 DMA 缓冲周期 `T`。
pub fn check(caps: &Capabilities, loads: &[Load], period_us: u64) -> Result<Margins, BudgetError> {
    let mut worst: Option<Margins> = None;
    for l in loads {
        let m = check_one(caps, l, period_us)?;
        worst = Some(match worst {
            Some(w) => Margins {
                load_permille: w.load_permille.max(m.load_permille),
                required_prefill_us: w.required_prefill_us.max(m.required_prefill_us),
                prefill_us: m.prefill_us,
                seek_response_us: w.seek_response_us.max(m.seek_response_us),
                header_jumps: w.header_jumps.max(m.header_jumps),
            },
            None => m,
        });
    }
    worst.ok_or(BudgetError::Invalid)
}

/// 核对一种格式。
pub fn check_one(caps: &Capabilities, load: &Load, period_us: u64) -> Result<Margins, BudgetError> {
    let demand = &load.demand;
    if demand.unit_bytes == 0 || demand.unit_frames == 0 {
        return Err(BudgetError::Invalid);
    }
    let u = u64::from(load.unit_read_us);
    let d = u64::from(load.decode_us);
    let s = u64::from(load.jump_us);
    let j = u64::from(load.header_jumps);
    let k = u64::from(load.unit_jumps);
    // 每个输入单元的读取（含其前至多 k 次跳转）。各量为 32 位输入的积，64 位不会溢出。
    let unit = u + k * s;
    // 千分比，各项向上取整：R·(U + k·S)/W/10³ 与 D·fs/F/10³。
    let read_load = (demand.bytes_per_sec * unit).div_ceil(u64::from(demand.unit_bytes) * 1000);
    let decode_load = (d * u64::from(caps.max_rate)).div_ceil(u64::from(demand.unit_frames) * 1000);
    let load_permille = read_load + decode_load;
    if load_permille > 1000 {
        return Err(BudgetError::Load { load_permille });
    }
    let o = u64::from(load.open_us);
    let header = j * (s + u);
    let required_us = o + header + unit + d + period_us;
    let prefill_us = u64::from(caps.prefill_ms) * 1000;
    if prefill_us < required_us {
        return Err(BudgetError::Prefill {
            required_us,
            prefill_us,
            open_us: o,
            jumps_us: header + k * s,
            read_us: u,
            decode_us: d,
            period_us,
        });
    }
    Ok(Margins {
        load_permille,
        required_prefill_us: required_us,
        prefill_us,
        seek_response_us: o + header + s + u + d,
        header_jumps: load.header_jumps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPS: Capabilities = Capabilities {
        max_rate: 96_000,
        max_channels: 2,
        max_bits: 24,
        queue_bytes: 192_000,
        prefill_ms: 250,
    };

    fn flac(u: u32, d: u32) -> Load {
        Load {
            demand: Demand::flac(&CAPS, 28_244, 4_608, 116),
            unit_read_us: u,
            decode_us: d,
            open_us: 0,
            header_jumps: 0,
            unit_jumps: 0,
            jump_us: 0,
        }
    }

    /// 独立手算：FLAC 窗口 28 244 字节 / 4608 帧，最小块上界 116 字节；U = 11 ms，D = 20 ms，T = 5.334 ms。
    #[test]
    fn formula_by_hand() {
        // 96000 × 116 / 16 = 696 000 字节/秒。
        assert_eq!(flac(0, 0).demand.bytes_per_sec, 696_000);
        // 读：696000 × 11000 / 28244 / 1000 = 271.06… → 272；解码：20000 × 96000 / 4608 / 1000
        // = 416.66… → 417；合计 689。预填：11000 + 20000 + 5334 = 36 334。
        assert_eq!(
            check(&CAPS, &[flac(11_000, 20_000)], 5_334),
            Ok(Margins {
                load_permille: 689,
                required_prefill_us: 36_334,
                prefill_us: 250_000,
                seek_response_us: 31_000,
                header_jumps: 0
            })
        );
        // 预填不足（字节率设为 1，只看预填）：230000 + 20000 + 5334 = 255 334 > 250 000。
        let mut slow = flac(230_000, 20_000);
        slow.demand.bytes_per_sec = 1;
        assert_eq!(
            check(&CAPS, &[slow], 5_334),
            Err(BudgetError::Prefill {
                required_us: 255_334,
                prefill_us: 250_000,
                open_us: 0,
                jumps_us: 0,
                read_us: 230_000,
                decode_us: 20_000,
                period_us: 5_334
            })
        );
        // 换曲时的打开耗时计入预填：O = 15 000 时需要 15000 + 11000 + 20000 + 5334 = 51 334；
        // O = 214 000 时 214000 + 11000 + 20000 + 5334 = 250 334 > 250 000（只差打开）。
        let open = |o| Load {
            open_us: o,
            ..flac(11_000, 20_000)
        };
        assert_eq!(
            check(&CAPS, &[open(15_000)], 5_334).map(|m| m.required_prefill_us),
            Ok(51_334)
        );
        assert_eq!(
            check(&CAPS, &[open(214_000)], 5_334),
            Err(BudgetError::Prefill {
                required_us: 250_334,
                prefill_us: 250_000,
                open_us: 214_000,
                jumps_us: 0,
                read_us: 11_000,
                decode_us: 20_000,
                period_us: 5_334
            })
        );
        // 负载超限：696000 × 41000 / 28244 / 1000 = 1010.3… → 1011‰。
        assert_eq!(
            check(&CAPS, &[flac(41_000, 0)], 5_334),
            Err(BudgetError::Load {
                load_permille: 1011
            })
        );
        let mut zero = flac(0, 0);
        zero.demand.unit_frames = 0;
        assert_eq!(check(&CAPS, &[zero], 0), Err(BudgetError::Invalid));
        assert_eq!(check(&CAPS, &[], 0), Err(BudgetError::Invalid));
    }

    /// 两种格式分别核对，取各自最坏者；任一不满足即拒绝。
    #[test]
    fn per_codec_worst() {
        let w = Demand::wav(&CAPS, 16_384);
        assert_eq!(w.bytes_per_sec, 96_000 * 8);
        assert_eq!(w.unit_frames, 2_048);
        let wav = Load {
            demand: w,
            unit_read_us: 9_000,
            decode_us: 1_000,
            open_us: 0,
            header_jumps: 0,
            unit_jumps: 0,
            jump_us: 0,
        };
        // WAV：768000 × 9000 / 16384 / 1000 = 421.875 → 422；1000 × 96000 / 2048 / 1000 = 46.875 → 47。
        let m = check(&CAPS, &[wav, flac(11_000, 20_000)], 5_334);
        assert_eq!(
            m,
            Ok(Margins {
                load_permille: 689,
                required_prefill_us: 36_334,
                prefill_us: 250_000,
                seek_response_us: 31_000,
                header_jumps: 0
            })
        );
        assert_eq!(
            check_one(&CAPS, &wav, 5_334).map(|m| m.load_permille),
            Ok(469)
        );
        let bad_wav = Load {
            unit_read_us: 30_000,
            ..wav
        };
        assert!(matches!(
            check(&CAPS, &[bad_wav, flac(11_000, 20_000)], 5_334),
            Err(BudgetError::Load { .. })
        ));
    }

    /// E6：解码需要的跳转。独立手算：FLAC 窗口同上，U = 11 ms，D = 20 ms，T = 5.334 ms，O = 15 ms，
    /// S = 4 ms，J = 3，k = 1。
    #[test]
    fn jumps_by_hand() {
        let l = Load {
            open_us: 15_000,
            header_jumps: 3,
            unit_jumps: 1,
            jump_us: 4_000,
            ..flac(11_000, 20_000)
        };
        // 读：696000 × (11000 + 4000) / 28244 / 1000 = 369.63… → 370；解码 417；合计 787。
        // 预填：15000 + 3 × (4000 + 11000) + 11000 + 4000 + 20000 + 5334 = 100 334。
        // 定位响应：15000 + 45000 + 4000 + 11000 + 20000 = 95 000。
        assert_eq!(
            check(&CAPS, &[l], 5_334),
            Ok(Margins {
                load_permille: 787,
                required_prefill_us: 100_334,
                prefill_us: 250_000,
                seek_response_us: 95_000,
                header_jumps: 3
            })
        );
        // J = 13：15000 + 13 × 15000 + 15000 + 20000 + 5334 = 250 334 > 250 000，只差跳转。
        let many = Load {
            header_jumps: 13,
            ..l
        };
        assert_eq!(
            check(&CAPS, &[many], 5_334),
            Err(BudgetError::Prefill {
                required_us: 250_334,
                prefill_us: 250_000,
                open_us: 15_000,
                jumps_us: 13 * 15_000 + 4_000,
                read_us: 11_000,
                decode_us: 20_000,
                period_us: 5_334
            })
        );
        // 多种格式：取各自 J 的最大值。
        let none = Load {
            open_us: 15_000,
            ..flac(11_000, 20_000)
        };
        assert_eq!(
            check(&CAPS, &[none, l], 5_334).map(|m| m.header_jumps),
            Ok(3)
        );
    }
}
