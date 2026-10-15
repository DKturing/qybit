//! 不依赖文件系统的曲目存储（M3 T2）：曲目表 + 按偏移读取的存储设备。
//!
//! 存储设备用 Rust 嵌入式社区的标准接口 `embedded_storage::ReadStorage`（按字节偏移读取），
//! 因此任何实现了它的介质都可直接使用：片内闪存、内存映射的 QSPI/OSPI 闪存（见 [`MemoryStorage`]）、
//! SPI NOR 闪存或 EEPROM 驱动等。曲目表（名称、偏移、长度）由板级给出，通常在构建时与闪存镜像
//! 一起生成。需要按文件名管理曲目时改用文件系统（FAT 见 [`crate::storage::FatStorage`]；
//! 其他文件系统由板级实现 [`Storage`]）。
//!
//! 曲目表在构造与重新扫描时逐项核对：偏移 + 长度不得溢出，也不得超出设备容量，否则拒绝并指出
//! 是哪一项；读取不会越过曲目末尾。

use core::fmt;

use embedded_storage::ReadStorage;

use crate::storage::Storage;

/// 一首曲目在存储设备上的位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlatTrack {
    pub name: &'static str,
    pub offset: u32,
    pub len: u32,
}

/// 无文件系统存储的错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlatError<E> {
    /// 存储设备报告的错误。
    Device(E),
    /// 曲目表第 `track` 项超出设备容量或偏移 + 长度溢出。
    OutOfRange { track: u32 },
    /// 曲目编号越界。
    NoSuchTrack(u32),
    /// 没有打开的曲目。
    NotOpen,
}

/// 曲目表 + 存储设备。
pub struct FlatStorage<R: ReadStorage> {
    device: R,
    tracks: &'static [FlatTrack],
    /// 当前曲目的（读取位置，结束位置）。
    open: Option<(u32, u32)>,
    /// 当前曲目序号（定位时取曲目起点）。
    open_track: Option<u32>,
}

fn validate(tracks: &[FlatTrack], capacity: usize) -> Result<u32, u32> {
    for (i, t) in tracks.iter().enumerate() {
        let i = u32::try_from(i).unwrap_or(u32::MAX);
        let end = t.offset.checked_add(t.len).ok_or(i)?;
        if usize::try_from(end).map_or(true, |e| e > capacity) {
            return Err(i);
        }
    }
    u32::try_from(tracks.len()).map_err(|_| u32::MAX)
}

impl<R: ReadStorage> FlatStorage<R> {
    /// 创建并核对曲目表。
    pub fn new(device: R, tracks: &'static [FlatTrack]) -> Result<Self, FlatError<R::Error>> {
        validate(tracks, device.capacity()).map_err(|track| FlatError::OutOfRange { track })?;
        Ok(Self {
            device,
            tracks,
            open: None,
            open_track: None,
        })
    }

    pub fn device(&mut self) -> &mut R {
        &mut self.device
    }
}

impl<R: ReadStorage> Storage for FlatStorage<R>
where
    R::Error: fmt::Debug,
{
    type Error = FlatError<R::Error>;

    fn track_count(&self) -> u32 {
        u32::try_from(self.tracks.len()).unwrap_or(u32::MAX)
    }

    fn track_name(&self, track: u32, out: &mut dyn fmt::Write) -> fmt::Result {
        match self.tracks.get(track as usize) {
            Some(t) => out.write_str(t.name),
            None => out.write_str("?"),
        }
    }

    fn open(&mut self, track: u32) -> Result<(), Self::Error> {
        self.open = None;
        let t = self
            .tracks
            .get(track as usize)
            .ok_or(FlatError::NoSuchTrack(track))?;
        // 构造时已核对不溢出。
        let end = t
            .offset
            .checked_add(t.len)
            .ok_or(FlatError::OutOfRange { track })?;
        self.open = Some((t.offset, end));
        self.open_track = Some(track);
        Ok(())
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        let (pos, end) = self.open.ok_or(FlatError::NotOpen)?;
        let left = (end - pos) as usize;
        let n = buf.len().min(left);
        if n == 0 {
            return Ok(0);
        }
        self.device
            .read(pos, &mut buf[..n])
            .map_err(FlatError::Device)?;
        // n ≤ end − pos，不会溢出。
        self.open = Some((pos + n as u32, end));
        Ok(n)
    }

    fn seek(&mut self, offset: u64) -> bool {
        let (Some(track), Some((_, end))) = (self.open_track, self.open) else {
            return false;
        };
        let Some(t) = self.tracks.get(track as usize) else {
            return false;
        };
        match u32::try_from(offset)
            .ok()
            .and_then(|o| t.offset.checked_add(o))
        {
            Some(pos) if pos <= end => {
                self.open = Some((pos, end));
                true
            }
            _ => false,
        }
    }

    fn close(&mut self) {
        self.open = None;
    }

    fn rescan(&mut self) -> Result<u32, Self::Error> {
        self.open = None;
        validate(self.tracks, self.device.capacity())
            .map_err(|track| FlatError::OutOfRange { track })
    }
}

/// 内存映射的存储（片内闪存、XIP 方式映射的 QSPI/OSPI 闪存、RAM）：读取即拷贝。
pub struct MemoryStorage(pub &'static [u8]);

/// 读取越过存储末尾。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutOfBounds;

impl ReadStorage for MemoryStorage {
    type Error = OutOfBounds;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), OutOfBounds> {
        let start = offset as usize;
        let src = start
            .checked_add(bytes.len())
            .and_then(|end| self.0.get(start..end))
            .ok_or(OutOfBounds)?;
        bytes.copy_from_slice(src);
        Ok(())
    }

    fn capacity(&self) -> usize {
        self.0.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static DATA: [u8; 10] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9];

    #[test]
    fn reads_within_track_bounds() {
        static TRACKS: [FlatTrack; 2] = [
            FlatTrack {
                name: "a",
                offset: 2,
                len: 5,
            },
            FlatTrack {
                name: "b",
                offset: 9,
                len: 1,
            },
        ];
        let Ok(mut s) = FlatStorage::new(MemoryStorage(&DATA), &TRACKS) else {
            panic!("表应有效");
        };
        assert_eq!(s.track_count(), 2);
        assert_eq!(s.read(&mut [0; 4]), Err(FlatError::NotOpen));
        assert_eq!(s.open(0), Ok(()));
        let mut b = [0u8; 3];
        assert_eq!(s.read(&mut b), Ok(3));
        assert_eq!(b, [2, 3, 4]);
        assert_eq!(s.read(&mut b), Ok(2));
        assert_eq!(&b[..2], &[5, 6]);
        assert_eq!(s.read(&mut b), Ok(0));
        assert_eq!(s.open(1), Ok(()));
        assert_eq!(s.read(&mut b), Ok(1));
        assert_eq!(b[0], 9);
        assert_eq!(s.open(2), Err(FlatError::NoSuchTrack(2)));
    }

    #[test]
    fn rejects_out_of_range_tables() {
        static BEYOND: [FlatTrack; 2] = [
            FlatTrack {
                name: "ok",
                offset: 0,
                len: 10,
            },
            FlatTrack {
                name: "beyond",
                offset: 5,
                len: 6,
            },
        ];
        static OVERFLOW: [FlatTrack; 1] = [FlatTrack {
            name: "overflow",
            offset: u32::MAX,
            len: 2,
        }];
        assert!(matches!(
            FlatStorage::new(MemoryStorage(&DATA), &BEYOND),
            Err(FlatError::OutOfRange { track: 1 })
        ));
        assert!(matches!(
            FlatStorage::new(MemoryStorage(&DATA), &OVERFLOW),
            Err(FlatError::OutOfRange { track: 0 })
        ));
        let mut m = MemoryStorage(&DATA);
        assert_eq!(m.read(8, &mut [0; 3]), Err(OutOfBounds));
        assert_eq!(m.read(u32::MAX, &mut [0; 1]), Err(OutOfBounds));
    }
}
