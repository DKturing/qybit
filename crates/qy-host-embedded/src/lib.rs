//! QyBit 通用嵌入式宿主（M3 T2）：与开发板无关，`no_std`、无 alloc。
//!
//! 把 `qy_core::engine` 的宿主操作接到几个小接口上，板级适配只做外设初始化与接口实现：
//! - [`storage::Storage`]：读取曲目。存储介质由板级决定，宿主不假定 SD 卡。两种通用实现：
//!   [`storage::FatStorage`] 基于成熟的 `embedded-sdmmc`，读任何块设备（SD 卡、eMMC、U 盘、放了
//!   FAT 镜像的闪存）上的 FAT16/FAT32；[`flat::FlatStorage`] 不需要文件系统，曲目表 + 任何实现了
//!   `embedded_storage::ReadStorage` 的设备（内存映射闪存、SPI NOR 等）。其他文件系统由板级
//!   直接实现 `Storage`；
//! - [`output::AudioOutput`]：I²S/SAI + DMA 的配置、启动与停止；格式能力由
//!   [`output::OutputCaps`] 声明；
//! - [`feeder::Feeder`]：DMA 完成中断里调用的喂数器（只做有界搬运、静音补齐与计数），与主循环经
//!   [`feeder::FeederAccess`] 共享；
//! - [`command`]：串口命令按行解析，与具体串口外设无关；
//! - [`budget`]：由最坏读卡延迟推出预填是否足够，不足时 [`player::Player::new`] 直接报错。
//!
//! 播放规则全部在引擎里（预填、续用、换格式排空、暂停、切歌、出错跳过、能力检查），本 crate
//! 只执行 I/O 与设备操作并回报结果。大缓冲（帧队列、输入窗口、解码工作区、DMA 缓冲）由板级用
//! `ConstStaticCell` 与静态队列在 `.bss` 中分配，启动时取得一次。
#![no_std]

pub mod budget;
pub mod command;
mod fatcheck;
pub mod feeder;
pub mod flat;
pub mod output;
pub mod player;
pub mod storage;

pub use budget::{BudgetError, Demand, Load};
pub use command::{Command, LineBuffer, ParseError};
pub use feeder::{Feeder, FeederAccess, Fill, OutputStats};
pub use flat::{FlatError, FlatStorage, FlatTrack, MemoryStorage};
pub use output::{AudioOutput, OutputCaps, SelectError, SlotFormat};
pub use player::{Notice, Player, Status, Step};
pub use storage::{FatError, FatStorage, NoClock, Storage};
