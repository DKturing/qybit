//! 测试用磁盘镜像（M3 T2）：一个 MBR 分区表 + 一个 FAT16 分区（类型 0x0E）或 FAT32 分区（类型 0x0C，
//! R1 fuzz 语料用），根目录放入给定文件。
//!
//! 只用于测试与构建脚本（std）。FAT 由成熟的 `fatfs` 格式化与写入，与被测的读取路径
//! （`qy-host-embedded` 经 `embedded-sdmmc`）相互独立。镜像内容是确定的（固定卷标、无时间戳）。
//!
//! 布局：扇区 0 为 MBR（分区表项 1：状态 0、类型 0x0E FAT16 LBA、起始 LBA [`PARTITION_START`]），
//! 其后为分区。FAT16 至少需要 4085 个簇：簇为 512 字节时分区至少约 2.1 MB（放进 MCU 演示的闪存），
//! 簇为 4 KiB 时约 16.8 MB（更接近真实 SD 卡，宿主侧仿真使用）。

use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};

/// 扇区字节数。
pub const SECTOR: usize = 512;
/// 分区起始扇区（与常见格式化工具一样按 1 MiB 对齐之外的小值即可；这里取 63）。
pub const PARTITION_START: u32 = 63;
/// FAT16 的最小簇数（FAT 规范：少于 4085 个簇为 FAT12）。
const FAT16_MIN_CLUSTERS: usize = 4085;
/// FAT32 的最小簇数（FAT 规范：少于 65525 个簇为 FAT16）。
const FAT32_MIN_CLUSTERS: usize = 65525;

/// 分区的 FAT 类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// FAT16，分区类型 0x0E（FAT16 LBA）。
    Fat16,
    /// FAT32，分区类型 0x0C（FAT32 LBA）。簇为 512 字节时镜像约 33 MB。
    Fat32,
}

/// 分区在整盘镜像中的窗口（`fatfs` 只看到分区本身）。
struct Partition<'a> {
    disk: &'a mut Cursor<Vec<u8>>,
    start: u64,
    len: u64,
    pos: u64,
}

impl Read for Partition<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = (buf.len() as u64).min(self.len.saturating_sub(self.pos)) as usize;
        self.disk.seek(SeekFrom::Start(self.start + self.pos))?;
        let n = self.disk.read(&mut buf[..n])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for Partition<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = (buf.len() as u64).min(self.len.saturating_sub(self.pos)) as usize;
        if n == 0 && !buf.is_empty() {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "分区已满"));
        }
        self.disk.seek(SeekFrom::Start(self.start + self.pos))?;
        let n = self.disk.write(&buf[..n])?;
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for Partition<'_> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let p = match to {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        match p {
            Some(p) if p <= self.len => {
                self.pos = p;
                Ok(p)
            }
            _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "越界定位")),
        }
    }
}

/// 生成镜像（簇 512 字节）：`files` 为（文件名，内容），文件名可以是长文件名（`fatfs` 同时写入
/// 8.3 短名）。
pub fn build(files: &[(&str, &[u8])]) -> io::Result<Vec<u8>> {
    build_with(files, SECTOR)
}

/// 生成镜像，簇大小 `cluster_bytes`（512 的 2 的幂倍）。分区大小按内容自动取：每个文件向上取整到
/// 簇，加目录与余量，且不小于 FAT16 的最小簇数。
pub fn build_with(files: &[(&str, &[u8])], cluster_bytes: usize) -> io::Result<Vec<u8>> {
    build_kind(files, cluster_bytes, Kind::Fat16)
}

/// 生成指定 FAT 类型的镜像，簇大小 `cluster_bytes`（512 的 2 的幂倍）。
pub fn build_kind(
    files: &[(&str, &[u8])],
    cluster_bytes: usize,
    kind: Kind,
) -> io::Result<Vec<u8>> {
    if cluster_bytes < SECTOR || !cluster_bytes.is_power_of_two() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "簇大小无效"));
    }
    let spc = cluster_bytes / SECTOR;
    let data_clusters: usize = files
        .iter()
        .map(|(_, d)| d.len().div_ceil(cluster_bytes))
        .sum();
    // 每个文件另留 2 个簇（目录项与长文件名余量），再留 64 个簇。
    // FAT32 另留 1024 个簇的余量（保留区与 FAT 更大，`fatfs` 自行布局）。
    let (min, entry_bytes, part_type, fat_type) = match kind {
        Kind::Fat16 => (FAT16_MIN_CLUSTERS + 64, 2, 0x0E, fatfs::FatType::Fat16),
        Kind::Fat32 => (FAT32_MIN_CLUSTERS + 1024, 4, 0x0C, fatfs::FatType::Fat32),
    };
    let clusters = (data_clusters + 2 * files.len() + 64).max(min);
    // 保留区、两份 FAT 与根目录（FAT16 为 512 项）。
    let fat_sectors = (clusters * entry_bytes).div_ceil(SECTOR);
    let part_sectors = clusters * spc + 2 * fat_sectors + 32 + 64;
    let total = PARTITION_START as usize + part_sectors;
    let mut disk = Cursor::new(vec![0u8; total * SECTOR]);

    // MBR：分区表项 1（偏移 446），签名 0x55AA。
    {
        let mbr = disk.get_mut();
        let e = &mut mbr[446..462];
        e[0] = 0x00;
        e[4] = part_type;
        e[8..12].copy_from_slice(&PARTITION_START.to_le_bytes());
        e[12..16].copy_from_slice(&(part_sectors as u32).to_le_bytes());
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
    }

    let mut part = Partition {
        disk: &mut disk,
        start: u64::from(PARTITION_START) * SECTOR as u64,
        len: (part_sectors * SECTOR) as u64,
        pos: 0,
    };
    fatfs::format_volume(
        &mut part,
        fatfs::FormatVolumeOptions::new()
            .fat_type(fat_type)
            .bytes_per_sector(SECTOR as u16)
            .bytes_per_cluster(cluster_bytes as u32)
            .total_sectors(part_sectors as u32)
            .volume_label(*b"QYBIT      "),
    )?;
    part.seek(SeekFrom::Start(0))?;
    {
        let fs = fatfs::FileSystem::new(&mut part, fatfs::FsOptions::new())?;
        if fs.fat_type() != fat_type {
            return Err(io::Error::other("格式化结果的 FAT 类型与请求不符"));
        }
        {
            let root = fs.root_dir();
            for (name, data) in files {
                let mut f = root.create_file(name)?;
                f.truncate()?;
                f.write_all(data)?;
                f.flush()?;
            }
        }
        fs.unmount()?;
    }
    Ok(disk.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回读：MBR 签名与分区项正确，`fatfs` 能重新打开并读出相同内容。
    #[test]
    fn round_trip() -> io::Result<()> {
        round_trip_kind(Kind::Fat16, 0x0E)?;
        round_trip_kind(Kind::Fat32, 0x0C)
    }

    fn round_trip_kind(kind: Kind, part_type: u8) -> io::Result<()> {
        let a: Vec<u8> = (0..1500u32).map(|i| (i * 7) as u8).collect();
        let img = build_kind(&[("A.WAV", &a), ("long name.flac", b"fLaC")], SECTOR, kind)?;
        assert_eq!(&img[510..512], &[0x55, 0xAA]);
        assert_eq!(img[446 + 4], part_type);
        let mut disk = Cursor::new(img);
        let len = disk.get_ref().len() as u64 - u64::from(PARTITION_START) * SECTOR as u64;
        let mut part = Partition {
            disk: &mut disk,
            start: u64::from(PARTITION_START) * SECTOR as u64,
            len,
            pos: 0,
        };
        let fs = fatfs::FileSystem::new(&mut part, fatfs::FsOptions::new())?;
        assert_eq!(
            fs.fat_type(),
            match kind {
                Kind::Fat16 => fatfs::FatType::Fat16,
                Kind::Fat32 => fatfs::FatType::Fat32,
            }
        );
        let mut got = Vec::new();
        fs.root_dir().open_file("A.WAV")?.read_to_end(&mut got)?;
        assert_eq!(got, a);
        let mut got = Vec::new();
        fs.root_dir()
            .open_file("long name.flac")?
            .read_to_end(&mut got)?;
        assert_eq!(got, b"fLaC");
        Ok(())
    }
}
