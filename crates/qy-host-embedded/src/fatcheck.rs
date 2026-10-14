//! FAT 结构的独立核对（R1 §4）：在交给 `embedded-sdmmc` 之前拦下会让它 panic 或无界读取的损坏结构。
//!
//! 背景：`embedded-sdmmc` 0.10.0 的读取路径对损坏的结构没有全部做范围检查，例如目录项起始簇号
//! 小于 2 时 `cluster_to_block` 中的 `c - 2` 溢出（fuzz 发现；release 构建下静默回绕、读错块），
//! `next_cluster` 对越界簇号 `panic!`，簇链环路会让读取一直进行到声明的文件长度。损坏的卡是
//! 不可信输入（AGENTS §4），因此在本层按 FAT 规范（Microsoft “FAT32 File System Specification”，
//! fatgen103）自行解析引导扇区、读取原始目录项与 FAT 表项，核对：
//! - 引导扇区：每扇区 512 字节、每簇扇区数为 1–128 的 2 的幂、保留扇区 ≥ 1、FAT 份数 ≥ 1、
//!   扇区总数与 FAT 大小非零、数据区非空，所有计算用 checked 运算；FAT 能容纳全部簇的表项；
//!   FAT16 的根目录项数非零，FAT32 的根目录起始簇有效；簇数 < 4085（FAT12）拒绝；
//! - 文件：长度为 0 时不读数据；否则起始簇在 `2 ..= 簇数 + 1`，前 ⌈长度 / 簇字节数⌉ 个簇每一跳都是
//!   有效数据簇（之后才允许结束标记），整条链无环路（步数以簇数为上界，另用 Brent 算法提前发现）；
//! - FAT32 根目录：簇链每一跳有效，至多走簇数步，必须以结束标记终止。
//!
//! 只读，不修改卡上数据。读取经块设备完成，块设备错误原样返回。

use embedded_sdmmc::{Block, BlockDevice, BlockIdx};

/// 文件系统几何信息（扇区号均为整盘绝对 LBA）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Geometry {
    fat_start: u32,
    fat32: bool,
    sectors_per_cluster: u32,
    clusters: u32,
    root_cluster: u32,
}

/// FAT 规范：簇数少于 4085 为 FAT12，少于 65525 为 FAT16，否则为 FAT32。
const FAT16_MIN: u32 = 4085;
const FAT32_MIN: u32 = 65525;

fn u16_at(b: &[u8; 512], o: usize) -> u32 {
    u32::from(u16::from_le_bytes([b[o], b[o + 1]]))
}

fn u32_at(b: &[u8; 512], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn read_block<D: BlockDevice>(d: &D, lba: u32) -> Result<[u8; 512], D::Error> {
    let mut blk = [Block::new()];
    d.read(&mut blk, BlockIdx(lba))?;
    Ok(blk[0].contents)
}

/// 解析 MBR 第 1 个分区与其引导扇区。结构无效时返回 `Ok(None)`。
pub(crate) fn geometry<D: BlockDevice>(d: &D) -> Result<Option<Geometry>, D::Error> {
    let mbr = read_block(d, 0)?;
    if mbr[510] != 0x55 || mbr[511] != 0xAA {
        return Ok(None);
    }
    let entry = 446;
    if mbr[entry] & 0x7F != 0 || !matches!(mbr[entry + 4], 0x04 | 0x06 | 0x0E | 0x0B | 0x0C) {
        return Ok(None);
    }
    let lba_start = u32_at(&mbr, entry + 8);
    let bs = read_block(d, lba_start)?;
    Ok(parse_boot_sector(&bs, lba_start))
}

fn parse_boot_sector(bs: &[u8; 512], lba_start: u32) -> Option<Geometry> {
    if u16_at(bs, 11) != 512 {
        return None;
    }
    let spc = u32::from(bs[13]);
    if spc == 0 || spc > 128 || !spc.is_power_of_two() {
        return None;
    }
    let reserved = u16_at(bs, 14);
    let fats = u32::from(bs[16]);
    let root_entries = u16_at(bs, 17);
    let total = match u16_at(bs, 19) {
        0 => u32_at(bs, 32),
        t => t,
    };
    let fat_size = match u16_at(bs, 22) {
        0 => u32_at(bs, 36),
        f => f,
    };
    if reserved == 0 || fats == 0 || total == 0 || fat_size == 0 {
        return None;
    }
    let root_dir_sectors = (root_entries * 32).div_ceil(512);
    let meta = fats
        .checked_mul(fat_size)?
        .checked_add(reserved)?
        .checked_add(root_dir_sectors)?;
    let data_sectors = total.checked_sub(meta)?;
    let clusters = data_sectors / spc;
    if clusters < FAT16_MIN {
        return None;
    }
    let fat32 = clusters >= FAT32_MIN;
    let entry_bytes: u64 = if fat32 { 4 } else { 2 };
    // FAT 必须容纳簇 0 ..= 簇数 + 1 的表项。
    if u64::from(fat_size) * 512 < (u64::from(clusters) + 2) * entry_bytes {
        return None;
    }
    let root_cluster = if fat32 {
        let r = u32_at(bs, 44);
        if r < 2 || r > clusters + 1 {
            return None;
        }
        r
    } else {
        if root_entries == 0 {
            return None;
        }
        0
    };
    Some(Geometry {
        fat_start: lba_start.checked_add(reserved)?,
        fat32,
        sectors_per_cluster: spc,
        clusters,
        root_cluster,
    })
}

/// 一个 FAT 表项的读取器（缓存最近读过的一个扇区）。
struct FatReader<'a, D: BlockDevice> {
    d: &'a D,
    g: Geometry,
    cached: Option<(u32, [u8; 512])>,
}

/// 表项的含义。
enum Next {
    Cluster(u32),
    End,
    Invalid,
}

impl<D: BlockDevice> FatReader<'_, D> {
    fn next(&mut self, cluster: u32) -> Result<Next, D::Error> {
        let width: u32 = if self.g.fat32 { 4 } else { 2 };
        let offset = u64::from(cluster) * u64::from(width);
        let Some(sector) = u32::try_from(offset / 512)
            .ok()
            .and_then(|s| self.g.fat_start.checked_add(s))
        else {
            return Ok(Next::Invalid);
        };
        let within = (offset % 512) as usize;
        let blk = match self.cached {
            Some((s, b)) if s == sector => b,
            _ => {
                let b = read_block(self.d, sector)?;
                self.cached = Some((sector, b));
                b
            }
        };
        let (value, eoc, bad) = if self.g.fat32 {
            (u32_at(&blk, within) & 0x0FFF_FFFF, 0x0FFF_FFF8, 0x0FFF_FFF7)
        } else {
            (u16_at(&blk, within), 0xFFF8, 0xFFF7)
        };
        Ok(if value >= eoc {
            Next::End
        } else if value == bad || !self.valid(value) {
            Next::Invalid
        } else {
            Next::Cluster(value)
        })
    }

    fn valid(&self, c: u32) -> bool {
        c >= 2 && c <= self.g.clusters + 1
    }
}

/// 文件的簇链是否安全：长度为 0 时不读数据；否则起始簇有效，前 ⌈长度 / 簇字节数⌉ 个簇每一跳都有效，
/// 并且整条链无环路。
///
/// 无环判定：无环的链至多有“簇数”个簇，因此沿链走到结束标记，步数以簇数为上界，超过即为环路；
/// 另用 Brent 算法（只记一个标记簇）提前发现环路，避免在大卷上走满簇数。文件数据之后的表项无效时
/// 不影响读取（读取以文件长度为界），只要前面的簇有效即接受。
pub(crate) fn file_ok<D: BlockDevice>(
    d: &D,
    g: Geometry,
    start: u32,
    size: u32,
) -> Result<bool, D::Error> {
    if size == 0 {
        return Ok(true);
    }
    let mut r = FatReader { d, g, cached: None };
    if !r.valid(start) {
        return Ok(false);
    }
    let cluster_bytes = u64::from(g.sectors_per_cluster) * 512;
    let needed = u64::from(size).div_ceil(cluster_bytes);
    if needed > u64::from(g.clusters) {
        return Ok(false);
    }
    let (mut c, mut mark) = (start, start);
    let (mut power, mut lam) = (1u64, 0u64);
    // 已走过的簇数（含起始簇）。
    let mut walked = 1u64;
    loop {
        match r.next(c)? {
            Next::Cluster(n) => c = n,
            Next::End => return Ok(walked >= needed),
            Next::Invalid => return Ok(walked >= needed),
        }
        walked += 1;
        if c == mark || walked > u64::from(g.clusters) {
            return Ok(false);
        }
        lam += 1;
        if lam == power {
            mark = c;
            power *= 2;
            lam = 0;
        }
    }
}

/// FAT32 根目录的簇链是否安全（FAT16 根目录是固定区域，恒为真）。
pub(crate) fn root_ok<D: BlockDevice>(d: &D, g: Geometry) -> Result<bool, D::Error> {
    if !g.fat32 {
        return Ok(true);
    }
    let mut r = FatReader { d, g, cached: None };
    let mut c = g.root_cluster;
    for _ in 0..g.clusters {
        match r.next(c)? {
            Next::Cluster(n) => c = n,
            Next::End => return Ok(true),
            Next::Invalid => return Ok(false),
        }
    }
    // 走满簇数仍未结束：环路。
    Ok(false)
}

/// 读取原始 32 字节目录项，返回（起始簇，文件长度）。位置越界时返回 `None`。
pub(crate) fn dir_entry<D: BlockDevice>(
    d: &D,
    g: Geometry,
    block: BlockIdx,
    offset: u32,
) -> Result<Option<(u32, u32)>, D::Error> {
    let o = offset as usize;
    if o > 512 - 32 {
        return Ok(None);
    }
    let b = read_block(d, block.0)?;
    let hi = if g.fat32 { u16_at(&b, o + 20) } else { 0 };
    let lo = u16_at(&b, o + 26);
    Ok(Some(((hi << 16) | lo, u32_at(&b, o + 28))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boot(spc: u8, reserved: u16, fats: u8, root: u16, total32: u32, fat16: u16) -> [u8; 512] {
        let mut b = [0u8; 512];
        b[11..13].copy_from_slice(&512u16.to_le_bytes());
        b[13] = spc;
        b[14..16].copy_from_slice(&reserved.to_le_bytes());
        b[16] = fats;
        b[17..19].copy_from_slice(&root.to_le_bytes());
        b[22..24].copy_from_slice(&fat16.to_le_bytes());
        b[32..36].copy_from_slice(&total32.to_le_bytes());
        b
    }

    /// 独立手算：保留 1、2 份 FAT 各 20 扇区、根目录 512 项（32 扇区）、总 5000 扇区、每簇 1 扇区
    /// → 数据区 4927 扇区 = 4927 簇（FAT16），FAT 需 (4927 + 2) × 2 = 9858 字节 ≤ 20 × 512。
    #[test]
    fn parses_and_rejects_boot_sectors() {
        let g = parse_boot_sector(&boot(1, 1, 2, 512, 5000, 20), 63);
        assert_eq!(
            g,
            Some(Geometry {
                fat_start: 64,
                fat32: false,
                sectors_per_cluster: 1,
                clusters: 4927,
                root_cluster: 0
            })
        );
        // 每簇扇区数 0、3（不是 2 的幂）；FAT 份数 0；保留 0；总扇区数不足；FAT 太小；FAT12。
        for b in [
            boot(0, 1, 2, 512, 5000, 20),
            boot(3, 1, 2, 512, 5000, 20),
            boot(1, 1, 0, 512, 5000, 20),
            boot(1, 0, 2, 512, 5000, 20),
            boot(1, 1, 2, 512, 60, 20),
            boot(1, 1, 2, 512, 5000, 19),
            boot(1, 1, 2, 512, 4000, 20),
        ] {
            assert_eq!(parse_boot_sector(&b, 63), None);
        }
        let mut b = boot(1, 1, 2, 512, 5000, 20);
        b[12] = 0; // 512 = 0x0200，高字节清零后每扇区 0 字节
        assert_eq!(parse_boot_sector(&b, 63), None);
    }
}
