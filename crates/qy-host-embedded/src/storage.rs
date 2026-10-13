//! 曲目存储：[`Storage`] 接口与基于 `embedded-sdmmc` 的 FAT 实现 [`FatStorage`]。
//!
//! 存储介质由板级决定，宿主只依赖 [`Storage`]：块设备上的 FAT（SD 卡、eMMC、U 盘、放了 FAT 镜像的
//! 闪存）用 [`FatStorage`]，板级只实现按块读取；不需要文件系统的介质用 [`crate::flat::FlatStorage`]；
//! 其他文件系统由板级直接实现 [`Storage`]。
//!
//! 读取是同步分段的：主循环每步至多读一段（`read_chunk` 字节），期间 DMA 中断照常
//! 取数，因此单次读取的最长耗时直接计入预填公式（见 [`crate::budget`]）。
//!
//! [`FatStorage`] 读取 MBR 第 1 个分区（FAT16/FAT32）根目录中扩展名为 `WAV` 或 `FLA`
//! （`.flac` 的 8.3 短名）的文件，按短文件名排序成播放列表，最多 `M` 首。文件格式由解码器按文件头
//! 识别，扩展名只用于筛选。拔卡或读错时返回错误，由引擎按出错策略处理；重新插卡后用
//! [`Storage::rescan`] 重建（会丢弃 `embedded-sdmmc` 的块缓存，避免读到旧卡内容）。

use core::fmt;
use core::ops::ControlFlow;

use embedded_sdmmc::{
    BlockDevice, Mode, RawDirectory, RawFile, RawVolume, ShortFileName, TimeSource, Timestamp,
    VolumeIdx, VolumeManager,
};

/// 曲目存储。
pub trait Storage {
    type Error: fmt::Debug;

    /// 播放列表中的曲目数。
    fn track_count(&self) -> u32;
    /// 第 `track` 首的显示名。
    fn track_name(&self, track: u32, out: &mut dyn fmt::Write) -> fmt::Result;
    /// 打开第 `track` 首（同时只打开一首，已打开的先关闭）。
    fn open(&mut self, track: u32) -> Result<(), Self::Error>;
    /// 从当前曲目读取至多 `buf.len()` 字节；返回 0 表示文件结束。
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error>;
    /// 关闭当前曲目（可重复调用）。
    fn close(&mut self);
    /// 重新扫描（换卡后）。返回曲目数。
    fn rescan(&mut self) -> Result<u32, Self::Error>;
}

/// 只读访问不需要时间：时间戳固定为 1980-01-01 00:00:00（FAT 最早可表示的时间）。
#[derive(Debug, Clone, Copy, Default)]
pub struct NoClock;

impl TimeSource for NoClock {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp {
            year_since_1970: 10,
            zero_indexed_month: 0,
            zero_indexed_day: 0,
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}

/// 播放列表收录的扩展名（短文件名，大写）。
const EXTENSIONS: [&[u8]; 2] = [b"WAV", b"FLA"];

/// FAT 存储错误。
#[derive(Debug)]
pub enum FatError<E: core::error::Error + 'static> {
    /// `embedded-sdmmc` 报告的错误（含块设备错误）。
    Fs(embedded_sdmmc::Error<E>),
    /// 尚未扫描或扫描失败，卷未打开。
    NotMounted,
    /// 曲目编号越界。
    NoSuchTrack(u32),
    /// 没有打开的曲目。
    NotOpen,
}

impl<E: core::error::Error + 'static> From<embedded_sdmmc::Error<E>> for FatError<E> {
    fn from(e: embedded_sdmmc::Error<E>) -> Self {
        Self::Fs(e)
    }
}

/// 只用 1 个卷、1 个目录、1 个文件句柄。
type Vm<D, T> = VolumeManager<D, T, 1, 1, 1>;

/// SD 卡（或任何块设备）上的 FAT 播放列表。`M` 为最多曲目数。
pub struct FatStorage<D: BlockDevice, T: TimeSource, const M: usize> {
    vm: Option<Vm<D, T>>,
    volume: Option<RawVolume>,
    root: Option<RawDirectory>,
    file: Option<RawFile>,
    names: [Option<ShortFileName>; M],
    count: usize,
    /// 根目录中符合条件的文件超过 `M` 首（只收录排序后的前 `M` 首）。
    pub truncated: bool,
    /// 句柄编号偏移：每次重建卷管理器递增，旧句柄不会与新句柄混淆。
    id_offset: u32,
}

impl<D: BlockDevice, T: TimeSource, const M: usize> FatStorage<D, T, M> {
    /// 创建并扫描。扫描失败（无卡、无分区、不是 FAT）时仍返回存储，曲目数为 0，错误一并返回。
    pub fn new(device: D, time: T) -> (Self, Result<u32, FatError<D::Error>>) {
        let mut s = Self {
            vm: Some(VolumeManager::new_with_limits(device, time, 0)),
            volume: None,
            root: None,
            file: None,
            names: [const { None }; M],
            count: 0,
            truncated: false,
            id_offset: 0,
        };
        let r = s.scan();
        (s, r)
    }

    /// 访问块设备（例如测试中注入故障、板级查询卡状态）。
    pub fn device<R>(&self, f: impl FnOnce(&mut D) -> R) -> Option<R> {
        self.vm.as_ref().map(|vm| vm.device(f))
    }

    fn close_all(&mut self) {
        if let Some(vm) = self.vm.as_ref() {
            if let Some(f) = self.file.take() {
                let _ = vm.close_file(f);
            }
            if let Some(d) = self.root.take() {
                let _ = vm.close_dir(d);
            }
            if let Some(v) = self.volume.take() {
                let _ = vm.close_volume(v);
            }
        }
        self.file = None;
        self.root = None;
        self.volume = None;
    }

    fn scan(&mut self) -> Result<u32, FatError<D::Error>> {
        self.count = 0;
        self.truncated = false;
        self.names = [const { None }; M];
        let vm = self.vm.as_ref().ok_or(FatError::NotMounted)?;
        let volume = vm.open_raw_volume(VolumeIdx(0))?;
        self.volume = Some(volume);
        let root = vm.open_root_dir(volume)?;
        self.root = Some(root);
        let (names, count, truncated) = (&mut self.names, &mut self.count, &mut self.truncated);
        vm.iterate_dir(root, |e| {
            let a = e.attributes;
            if a.is_directory() || a.is_volume() || a.is_hidden() || a.is_system() {
                return ControlFlow::Continue(());
            }
            if !EXTENSIONS.contains(&e.name.extension()) {
                return ControlFlow::Continue(());
            }
            insert_sorted(names, count, truncated, e.name);
            ControlFlow::Continue(())
        })?;
        Ok(self.count as u32)
    }
}

/// 按短文件名升序插入；已满时只保留最小的 `M` 个。
fn insert_sorted<const M: usize>(
    names: &mut [Option<ShortFileName>; M],
    count: &mut usize,
    truncated: &mut bool,
    name: ShortFileName,
) {
    let pos = names[..*count]
        .iter()
        .position(|n| n.as_ref().is_some_and(|n| name < *n))
        .unwrap_or(*count);
    if pos >= M {
        *truncated = true;
        return;
    }
    if *count == M {
        *truncated = true;
    } else {
        *count += 1;
    }
    names[pos..*count].rotate_right(1);
    names[pos] = Some(name);
}

impl<D: BlockDevice, T: TimeSource, const M: usize> Storage for FatStorage<D, T, M> {
    type Error = FatError<D::Error>;

    fn track_count(&self) -> u32 {
        self.count as u32
    }

    fn track_name(&self, track: u32, out: &mut dyn fmt::Write) -> fmt::Result {
        match self.names.get(track as usize).and_then(|n| n.as_ref()) {
            Some(n) => write!(out, "{n}"),
            None => out.write_str("?"),
        }
    }

    fn open(&mut self, track: u32) -> Result<(), Self::Error> {
        self.close();
        let name = *self
            .names
            .get(track as usize)
            .and_then(|n| n.as_ref())
            .ok_or(FatError::NoSuchTrack(track))?;
        let (Some(vm), Some(root)) = (self.vm.as_ref(), self.root) else {
            return Err(FatError::NotMounted);
        };
        self.file = Some(vm.open_file_in_dir(root, name, Mode::ReadOnly)?);
        Ok(())
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        let (Some(vm), Some(file)) = (self.vm.as_ref(), self.file) else {
            return Err(FatError::NotOpen);
        };
        Ok(vm.read(file, buf)?)
    }

    fn close(&mut self) {
        if let (Some(vm), Some(f)) = (self.vm.as_ref(), self.file.take()) {
            let _ = vm.close_file(f);
        }
    }

    fn rescan(&mut self) -> Result<u32, Self::Error> {
        self.close_all();
        // 重建卷管理器：丢弃块缓存（换卡后旧缓存无效）。
        if let Some(vm) = self.vm.take() {
            let (d, t) = vm.free();
            self.id_offset = self.id_offset.wrapping_add(0x1000);
            self.vm = Some(VolumeManager::new_with_limits(d, t, self.id_offset));
        }
        self.scan()
    }
}
