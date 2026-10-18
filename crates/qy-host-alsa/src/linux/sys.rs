//! 内核 ALSA 接口的系统调用边界（A1）：`open`、`fcntl`、`ioctl`、`poll`。本 crate 中与内核
//! 直连有关的 `unsafe` 只在这里（另见 `interrupt`、`priority` 的信号与调度调用），按 AGENTS §2
//! 单独审计。
//!
//! 前提（每处调用共同依赖）：
//! - 结构体按内核 ABI 转写（[`crate::kernel::abi`]，大小在编译期核对）；
//! - 每个 ioctl 编号的大小字段（asm-generic 编码第 16–29 位）必须等于所传结构体的大小，
//!   否则不调用、返回 `EINVAL`，所以内核按编号读写的字节数不会超出 Rust 对象；
//! - 读写帧数据时，内核按“帧数 × 已配置的帧字节数”访问缓冲；调用方传入帧字节数，这里按
//!   切片长度换算帧数，不会让内核越界。
//!
//! 出处：`open(2)`、`fcntl(2)`、`ioctl(2)`、`poll(2)`；ALSA 部分见内核
//! `sound/core/pcm_native.c`、`sound/core/control.c`。

#![allow(unsafe_code)]

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::kernel::abi::{ElemId, ElemList, Xferi, Zeroed};
use crate::pcm::errno;

fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(errno::EIO)
}

/// ioctl 编号中的大小字段。
const fn ioc_size(req: u32) -> usize {
    ((req >> 16) & 0x3fff) as usize
}

/// 以读写、非阻塞方式打开设备节点（PCM 与控制设备相同，与 alsa-lib 一致）：被占用时立即返回
/// `EBUSY` 而不是等待（alsa-lib 默认 `defaults.pcm.nonblock 1`），打开后按 `blocking` 恢复阻塞读写。
pub fn open_rw(path: &Path, blocking: bool) -> Result<File, i32> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| e.raw_os_error().unwrap_or(errno::EIO))?;
    if blocking {
        let fd = f.as_raw_fd();
        // SAFETY: fd 由上面的 File 持有且有效；F_GETFL/F_SETFL 只读写文件状态标志。
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(last_errno());
        }
        // SAFETY: 同上。
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
            return Err(last_errno());
        }
    }
    Ok(f)
}

/// 传结构体指针的 ioctl（读、写或读写方向由编号决定）。成功返回 ioctl 的非负返回值。
pub fn ioctl<T: Copy>(f: &File, req: u32, arg: &mut T) -> Result<i32, i32> {
    if ioc_size(req) != size_of::<T>() {
        return Err(errno::EINVAL);
    }
    // SAFETY: fd 有效；arg 指向一个完整的 T，大小等于编号中声明的大小（上面已核对），内核
    // 只在这个范围内读写；T 只含整数字段，内核写入任何位模式都是合法值。
    let r = unsafe { libc::ioctl(f.as_raw_fd(), req as _, arg as *mut T) };
    if r < 0 { Err(last_errno()) } else { Ok(r) }
}

/// 无参数的 ioctl（`_IO`）。
pub fn ioctl_none(f: &File, req: u32) -> Result<i32, i32> {
    if ioc_size(req) != 0 || req >> 30 != 0 {
        return Err(errno::EINVAL);
    }
    // SAFETY: fd 有效；`_IO` 编号的内核处理不读写用户内存，参数 0 不被解引用。
    let r = unsafe { libc::ioctl(f.as_raw_fd(), req as _, 0usize) };
    if r < 0 { Err(last_errno()) } else { Ok(r) }
}

/// 变长 TLV 读取（`SNDRV_CTL_IOCTL_TLV_READ`）：`buf[0]` 为元素 numid，`buf[1]` 为其后可用的
/// 字节数（由这里按切片长度填写），内核把 TLV 写在 `buf[2..]`。
pub fn ioctl_tlv(f: &File, req: u32, buf: &mut [u32]) -> Result<i32, i32> {
    if ioc_size(req) != 2 * size_of::<u32>() || buf.len() < 2 {
        return Err(errno::EINVAL);
    }
    let room = (buf.len() - 2)
        .checked_mul(size_of::<u32>())
        .and_then(|n| u32::try_from(n).ok())
        .ok_or(errno::EINVAL)?;
    buf[1] = room;
    // SAFETY: fd 有效；内核先读 8 字节头，再向头之后写至多 `room` 字节（`snd_ctl_tlv_ioctl`
    // 按头中的 length 限制），二者都在 buf 之内。
    let r = unsafe { libc::ioctl(f.as_raw_fd(), req as _, buf.as_mut_ptr()) };
    if r < 0 { Err(last_errno()) } else { Ok(r) }
}

/// 列出控制元素（`SNDRV_CTL_IOCTL_ELEM_LIST`）：从第 `offset` 个起，把至多 `ids.len()` 个元素的
/// 标识写入 `ids`。返回（写入的个数, 元素总数）。`ids` 为空时只取总数。
pub fn elem_list(f: &File, req: u32, offset: u32, ids: &mut [ElemId]) -> Result<(usize, u32), i32> {
    if ioc_size(req) != size_of::<ElemList>() {
        return Err(errno::EINVAL);
    }
    let mut l = ElemList::zeroed();
    l.offset = offset;
    l.space = u32::try_from(ids.len()).map_err(|_| errno::EINVAL)?;
    l.pids = ids.as_mut_ptr() as usize;
    // SAFETY: fd 有效；l 的大小与编号一致；内核向 pids 写至多 space 个 `ElemId`
    // （`snd_ctl_elem_list` 按 space 限制），space 等于 ids 的长度；ElemId 只含整数字段。
    let r = unsafe { libc::ioctl(f.as_raw_fd(), req as _, &mut l as *mut ElemList) };
    if r < 0 {
        return Err(last_errno());
    }
    Ok(((l.used as usize).min(ids.len()), l.count))
}

/// 写入交错帧（`SNDRV_PCM_IOCTL_WRITEI_FRAMES`），返回内核实际写入的帧数。
pub fn writei(f: &File, req: u32, bytes: &[u8], frame_bytes: usize) -> Result<usize, i32> {
    if frame_bytes == 0 || ioc_size(req) != size_of::<Xferi>() {
        return Err(errno::EINVAL);
    }
    let mut x = Xferi {
        result: 0,
        buf: bytes.as_ptr() as usize,
        frames: bytes.len() / frame_bytes,
    };
    // SAFETY: fd 有效；x 大小与编号一致；内核从 buf 读至多 frames × frame_bytes 字节，
    // 不超过 bytes.len()（frame_bytes 是已应用参数的帧字节数，由调用方保证）。
    let r = unsafe { libc::ioctl(f.as_raw_fd(), req as _, &mut x as *mut Xferi) };
    if r < 0 {
        return Err(last_errno());
    }
    usize::try_from(x.result).map_err(|_| errno::EIO)
}

/// 读取交错帧（`SNDRV_PCM_IOCTL_READI_FRAMES`），返回内核实际读到的帧数。
pub fn readi(f: &File, req: u32, bytes: &mut [u8], frame_bytes: usize) -> Result<usize, i32> {
    if frame_bytes == 0 || ioc_size(req) != size_of::<Xferi>() {
        return Err(errno::EINVAL);
    }
    let mut x = Xferi {
        result: 0,
        buf: bytes.as_mut_ptr() as usize,
        frames: bytes.len() / frame_bytes,
    };
    // SAFETY: fd 有效；x 大小与编号一致；内核向 buf 写至多 frames × frame_bytes 字节，
    // 不超过 bytes.len()；u8 的任何位模式都合法。
    let r = unsafe { libc::ioctl(f.as_raw_fd(), req as _, &mut x as *mut Xferi) };
    if r < 0 {
        return Err(last_errno());
    }
    usize::try_from(x.result).map_err(|_| errno::EIO)
}

/// `poll` 一个描述符：超时为 `Ok(None)`，否则为返回的事件位。
pub fn poll(f: &File, events: i16, timeout_ms: u32) -> Result<Option<i16>, i32> {
    let mut p = libc::pollfd {
        fd: f.as_raw_fd(),
        events,
        revents: 0,
    };
    let timeout = i32::try_from(timeout_ms).unwrap_or(i32::MAX);
    // SAFETY: p 是一个有效的 pollfd，数量为 1。
    let r = unsafe { libc::poll(&mut p, 1, timeout) };
    match r {
        r if r < 0 => Err(last_errno()),
        0 => Ok(None),
        _ => Ok(Some(p.revents)),
    }
}

pub const POLLOUT: i16 = libc::POLLOUT;
pub const POLLIN: i16 = libc::POLLIN;
pub const POLLERR: i16 = libc::POLLERR;
pub const POLLHUP: i16 = libc::POLLHUP;
pub const POLLNVAL: i16 = libc::POLLNVAL;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::abi::{HwParams, PCM_IOCTL_HW_PARAMS, PCM_IOCTL_PREPARE, Zeroed};

    /// 编号大小与结构体不一致时不发起调用（对普通文件调用也只会得到 EINVAL）。
    #[test]
    fn size_mismatch_is_rejected_before_the_call() {
        let Ok(f) = File::open("/proc/self/stat") else {
            eprintln!("未运行：无法打开 /proc/self/stat");
            return;
        };
        let mut wrong = 0u32;
        assert_eq!(
            ioctl(&f, PCM_IOCTL_HW_PARAMS, &mut wrong),
            Err(errno::EINVAL)
        );
        let mut p = HwParams::zeroed();
        // 大小相符时真的调用：普通文件不支持该 ioctl（ENOTTY）。
        assert_eq!(ioctl(&f, PCM_IOCTL_HW_PARAMS, &mut p), Err(libc::ENOTTY));
        assert_eq!(ioctl_none(&f, PCM_IOCTL_HW_PARAMS), Err(errno::EINVAL));
        assert_eq!(ioctl_none(&f, PCM_IOCTL_PREPARE), Err(libc::ENOTTY));
        assert_eq!(
            writei(&f, PCM_IOCTL_PREPARE, &[0; 4], 4),
            Err(errno::EINVAL)
        );
        assert_eq!(
            ioctl_tlv(&f, PCM_IOCTL_PREPARE, &mut [0; 4]),
            Err(errno::EINVAL)
        );
    }
}
