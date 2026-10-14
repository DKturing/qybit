//! DMA 喂数（中断侧）。
//!
//! [`Feeder`] 持有静态帧队列的消费端，由板级 DMA 完成中断调用 [`Feeder::fill`] 填下一个缓冲：
//! 只做有界拷贝（至多两次读授权）、静音补齐与计数，不解码、不 pack、不分配、不阻塞（AGENTS §2）。
//!
//! 主循环与中断共享同一个 `Feeder`，经板级实现的 [`FeederAccess`] 访问（MCU 上用临界区包住的
//! `RefCell`）。主循环只在短时间内设置标志、读取计数，或在 DMA 停止时重配置队列。
//!
//! 计数分开记录（AGENTS §3）：
//! - **欠载**：运行中、未暂停、未排空时队列不足一个缓冲，缺的帧补静音并计数；
//! - **暂停静音**：暂停期间整缓冲输出静音，单独计数，不算欠载；
//! - **排空尾部静音**：排空时队列已空补的静音，不算欠载。

use qy_pcm_format::queue::{FrameSource, Watermark};
use qy_pcm_format::static_queue::StaticConsumer;

/// 输出统计（自启动起累计）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutputStats {
    /// `fill` 调用次数（运行中）。
    pub fills: u64,
    /// 输出的有效帧数。
    pub data_frames: u64,
    /// 输出的静音帧数（欠载、暂停与排空尾部合计）。
    pub silence_frames: u64,
    /// 欠载次数（缓冲未填满的次数）。
    pub underruns: u32,
    /// 欠载缺失的帧数。
    pub underrun_frames: u64,
    /// 暂停期间输出静音的缓冲数。
    pub paused_fills: u64,
    /// 缓冲长度不是完整帧、队列未配置或读授权冲突的次数（应为 0）。
    pub errors: u32,
    /// 当前设备流的队列水位（R1；运行中、未暂停、未排空时每次取数前观测）。
    pub watermark: Watermark,
}

/// 一次 `fill` 的结果：缓冲前 `data_frames` 帧为有效数据，其后为静音。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fill {
    pub data_frames: usize,
    pub silence_frames: usize,
}

/// 中断侧喂数器。`N` 为静态帧队列的存储字节数。
pub struct Feeder<const N: usize> {
    consumer: StaticConsumer<N>,
    running: bool,
    paused: bool,
    draining: bool,
    in_flight: u32,
    /// 自最近一次含有效数据的 `fill` 以来的 `fill` 次数（饱和）。
    since_data: u32,
    stats: OutputStats,
}

impl<const N: usize> Feeder<N> {
    /// `in_flight` 见 [`crate::output::OutputCaps::in_flight`]。
    pub fn new(consumer: StaticConsumer<N>, in_flight: u32) -> Self {
        Self {
            consumer,
            running: false,
            paused: false,
            draining: false,
            in_flight,
            since_data: u32::MAX,
            stats: OutputStats::default(),
        }
    }

    /// 填一个 DMA 缓冲（中断中调用）。缓冲长度必须是当前帧宽的整数倍，否则整缓冲静音并计错。
    pub fn fill(&mut self, out: &mut [u8]) -> Fill {
        let fb = self.consumer.frame_bytes();
        if fb == 0 || out.len() % fb != 0 {
            out.fill(0);
            self.stats.errors = self.stats.errors.saturating_add(1);
            return Fill {
                data_frames: 0,
                silence_frames: 0,
            };
        }
        let frames = out.len() / fb;
        if !self.running {
            out.fill(0);
            return Fill {
                data_frames: 0,
                silence_frames: frames,
            };
        }
        self.stats.fills += 1;
        if self.paused {
            out.fill(0);
            self.stats.paused_fills += 1;
            self.stats.silence_frames += frames as u64;
            self.since_data = self.since_data.saturating_add(1);
            return Fill {
                data_frames: 0,
                silence_frames: frames,
            };
        }
        if !self.draining {
            self.stats
                .watermark
                .observe(self.consumer.available_frames() as u64);
        }
        let n = match self.consumer.pop_frames(out) {
            Ok(n) => n,
            Err(_) => {
                self.stats.errors = self.stats.errors.saturating_add(1);
                0
            }
        };
        out[n * fb..].fill(0);
        let missing = frames - n;
        self.stats.data_frames += n as u64;
        self.stats.silence_frames += missing as u64;
        if missing > 0 && !self.draining {
            self.stats.underruns = self.stats.underruns.saturating_add(1);
            self.stats.underrun_frames += missing as u64;
        }
        self.since_data = if n > 0 {
            0
        } else {
            self.since_data.saturating_add(1)
        };
        Fill {
            data_frames: n,
            silence_frames: missing,
        }
    }

    /// 设备已启动（DMA 运行中）。
    pub fn set_running(&mut self, running: bool) {
        self.running = running;
        if !running {
            self.draining = false;
        }
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    /// 开始排空：此后队列为空补的静音不算欠载。
    pub fn begin_drain(&mut self) {
        self.draining = true;
    }

    /// 排空完成：队列已空，且最后一个含有效数据的缓冲之后又完成了 `in_flight` 次填充
    /// （有效数据都已播完）。
    pub fn is_drained(&self) -> bool {
        self.draining && self.consumer.available_frames() == 0 && self.since_data >= self.in_flight
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub fn stats(&self) -> OutputStats {
        self.stats
    }

    /// 队列中待播帧数。
    pub fn queued_frames(&self) -> usize {
        self.consumer.available_frames()
    }

    /// 设置新设备流的水位阈值（重新开始统计）。
    pub fn set_watermark(&mut self, w: Watermark) {
        self.stats.watermark = w;
    }

    /// 设备重配置前（DMA 已停止）：取得消费端并清除运行状态。
    pub fn reset_for_open(&mut self) -> &mut StaticConsumer<N> {
        self.running = false;
        self.paused = false;
        self.draining = false;
        self.since_data = u32::MAX;
        &mut self.consumer
    }
}

/// 主循环访问共享 `Feeder` 的方式（板级实现，MCU 上为临界区）。`Feeder` 尚未放入时返回 `None`。
pub trait FeederAccess<const N: usize> {
    fn with<R>(&self, f: impl FnOnce(&mut Feeder<N>) -> R) -> Option<R>;
}
