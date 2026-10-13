//! QyBit 自有推送式 FLAC 解码器（M1）。
//!
//! `#![no_std]`、无 alloc。解码器只保存宿主提供的切片与小状态，不内嵌随块大小增长的
//! 数组（AGENTS §2，M1 T1）。三类大缓冲统一为 `&'static mut [T]`：
//!
//! | 缓冲 | 类型 | 归属 |
//! | --- | --- | --- |
//! | 帧输入窗口 | `[u8]` | 核心（[`qy_core::Core::with_window`]） |
//! | 解码/预测/解相关工作区 | `[i64]` | 解码器 |
//! | 最终交错 PCM | `[i32]` | 解码器 |
//!
//! 容量只来自宿主配置（[`Capacity`]），不依据码流头部动态决定；切片不足时构造返回
//! [`InitError::CapacityExceeded`]。桌面宿主启动时 `vec![0; n]` → boxed slice → `Box::leak`
//! 一次；MCU 用 `static_cell::ConstStaticCell` 的 `try_take`（特性 `static-cell`，见 [`mcu`]）。
//! 停止、换曲与重置复用同一套缓冲。
//!
//! M1 T1 只建立缓冲契约；帧解析与 [`qy_core::Decoder`] 实现在 T2。
#![no_std]

/// 解码器容量配置（宿主声明）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    /// 支持的最大块大小（帧/声道）。
    pub max_block: usize,
    pub max_channels: usize,
    /// 帧输入窗口字节数：本配置承诺支持的最大编码帧长度，超过返回容量错误。
    pub input_window: usize,
}

impl Capacity {
    /// 桌面：块大小最大 65535、双声道、编码帧 ≤ 1 MiB。
    pub const DESKTOP: Capacity = Capacity {
        max_block: 65_535,
        max_channels: 2,
        input_window: 1 << 20,
    };

    /// MCU 参考配置：块大小 ≤ 4608（RFC 9639 可流式子集在 ≤ 48 kHz 时的上限）、
    /// 双声道、编码帧 ≤ 32 KiB（足够 24 位双声道 4608 帧的 verbatim 帧）。
    pub const MCU: Capacity = Capacity {
        max_block: 4_608,
        max_channels: 2,
        input_window: 32 * 1024,
    };

    /// `i64` 工作区元素数。
    pub const fn workspace_len(&self) -> usize {
        self.max_block * self.max_channels
    }

    /// `i32` 交错 PCM 元素数。
    pub const fn pcm_len(&self) -> usize {
        self.max_block * self.max_channels
    }
}

/// 缓冲种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Buffer {
    InputWindow,
    Workspace,
    Pcm,
}

/// 初始化错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitError {
    /// 宿主提供的切片短于声明容量。
    CapacityExceeded {
        buffer: Buffer,
        required: usize,
        provided: usize,
    },
    /// 静态缓冲已被取走（每个 `ConstStaticCell` 只能取一次）。
    AlreadyTaken(Buffer),
}

/// 一套宿主缓冲。
pub struct FlacBuffers {
    pub input: &'static mut [u8],
    pub workspace: &'static mut [i64],
    pub pcm: &'static mut [i32],
}

fn check(buffer: Buffer, required: usize, provided: usize) -> Result<(), InitError> {
    if provided < required {
        Err(InitError::CapacityExceeded {
            buffer,
            required,
            provided,
        })
    } else {
        Ok(())
    }
}

impl FlacBuffers {
    /// 按容量检查全部切片长度。
    pub fn check(&self, cap: &Capacity) -> Result<(), InitError> {
        check(Buffer::InputWindow, cap.input_window, self.input.len())?;
        check(Buffer::Workspace, cap.workspace_len(), self.workspace.len())?;
        check(Buffer::Pcm, cap.pcm_len(), self.pcm.len())
    }
}

/// FLAC 解码器（T1：缓冲与容量；解码在 T2）。
pub struct FlacDecoder {
    cap: Capacity,
    workspace: &'static mut [i64],
    pcm: &'static mut [i32],
}

impl FlacDecoder {
    /// 用宿主提供的工作区与 PCM 缓冲构造；长度按 `cap` 检查。
    pub fn new(
        cap: Capacity,
        workspace: &'static mut [i64],
        pcm: &'static mut [i32],
    ) -> Result<Self, InitError> {
        check(Buffer::Workspace, cap.workspace_len(), workspace.len())?;
        check(Buffer::Pcm, cap.pcm_len(), pcm.len())?;
        Ok(Self {
            cap,
            workspace,
            pcm,
        })
    }

    pub fn capacity(&self) -> Capacity {
        self.cap
    }

    pub fn workspace_len(&self) -> usize {
        self.workspace.len()
    }

    pub fn pcm_len(&self) -> usize {
        self.pcm.len()
    }
}

#[cfg(feature = "static-cell")]
pub mod mcu;
