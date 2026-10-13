//! MCU 静态缓冲（特性 `static-cell`）。
//!
//! 使用 `static_cell::ConstStaticCell::new([0; N])`：初始化值是 const 全零数组，
//! 不在运行时栈上构造，全零对象应进入 `.bss`（static_cell 2.1.1 文档）；实际 MCU
//! 的链接 map 仍需在目标工程中核验。`try_take` 只能成功一次，再次获取返回
//! [`InitError::AlreadyTaken`]。禁止在运行时构造大数组再交给 `StaticCell::init`。

use static_cell::ConstStaticCell;

use crate::{Buffer, Capacity, FlacBuffers, InitError};

/// 从三个 `ConstStaticCell` 一次性取得缓冲并转为切片，再按容量检查。
pub fn from_cells<const I: usize, const W: usize, const P: usize>(
    cap: &Capacity,
    input: &'static ConstStaticCell<[u8; I]>,
    workspace: &'static ConstStaticCell<[i64; W]>,
    pcm: &'static ConstStaticCell<[i32; P]>,
) -> Result<FlacBuffers, InitError> {
    let input = input
        .try_take()
        .ok_or(InitError::AlreadyTaken(Buffer::InputWindow))?;
    let workspace = workspace
        .try_take()
        .ok_or(InitError::AlreadyTaken(Buffer::Workspace))?;
    let pcm = pcm.try_take().ok_or(InitError::AlreadyTaken(Buffer::Pcm))?;
    let buffers = FlacBuffers {
        input: input.as_mut_slice(),
        workspace: workspace.as_mut_slice(),
        pcm: pcm.as_mut_slice(),
    };
    buffers.check(cap)?;
    Ok(buffers)
}

const INPUT_LEN: usize = Capacity::MCU.input_window;
const WORKSPACE_LEN: usize = Capacity::MCU.workspace_len();
const PCM_LEN: usize = Capacity::MCU.pcm_len();

static INPUT: ConstStaticCell<[u8; INPUT_LEN]> = ConstStaticCell::new([0; INPUT_LEN]);
static WORKSPACE: ConstStaticCell<[i64; WORKSPACE_LEN]> = ConstStaticCell::new([0; WORKSPACE_LEN]);
static PCM: ConstStaticCell<[i32; PCM_LEN]> = ConstStaticCell::new([0; PCM_LEN]);

/// 取得 MCU 参考配置（[`Capacity::MCU`]）的静态缓冲；只能成功一次。
pub fn take_buffers() -> Result<FlacBuffers, InitError> {
    from_cells(&Capacity::MCU, &INPUT, &WORKSPACE, &PCM)
}
