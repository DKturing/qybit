//! 损坏文件系统 fuzz 目标（R1 §4）：`qy-host-embedded` 的 `FatStorage`（经 `embedded-sdmmc`）。
//!
//! FAT16 至少约 2.1 MB、FAT32 约 33 MB，直接对整块镜像变异效率太低，因此用结构化变异：固定两个有效的
//! 基础镜像（`qy-fat-image` 生成，FAT16 与 FAT32 各一，含 4 个文件，其中一个是长文件名、一个是空文件）。
//! fuzz 输入的第 1 字节最低位选择基础镜像（0 为 FAT16，1 为 FAT32；空输入为 FAT16），其后是一组补丁记录
//! `[偏移 3 字节][长度 1 字节][数据]`：偏移最高位为 1 时落在元数据区（MBR、引导扇区、FAT 表、
//! 根目录与数据区开头，按基础镜像的引导扇区算出），否则落在镜像前 8 MiB。补丁在块设备读取时叠加，
//! 不复制整块镜像。
//!
//! 每个输入：扫描 → 逐首打开并读完（每次 512 字节）→ 重新扫描 → 再读一遍第一首。检查：
//! - 不 panic；
//! - 不越界读取：块设备只服务镜像范围内的块，越界以 `Err` 返回；
//! - 读取总量有上界：单个文件读取不超过 `READ_LIMIT`，块设备读取总数不超过 `BLOCK_LIMIT`，
//!   超过即视为簇链环路等导致的无界读取（失败）；
//! - 错误都以 `Err` 返回。
#![no_main]

use std::cell::Cell;
use std::fmt;
use std::sync::OnceLock;

use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};
use libfuzzer_sys::fuzz_target;
use qy_host_embedded::{FatStorage, NoClock, Storage};

/// 补丁偏移的寻址范围（3 字节偏移去掉最高位）。
const SPAN: usize = 1 << 23;
/// 单个文件读取上限：远大于基础镜像中的文件，超过即为无界读取。
const READ_LIMIT: usize = 8 << 20;
/// 一个输入的块读取总数上限：FAT32 基础镜像约 6.6 万簇，即使走满簇数核对环路也远低于此。
const BLOCK_LIMIT: u64 = 4_000_000;

struct Base {
    image: Vec<u8>,
    /// 元数据区长度（数据区起点之后再加 64 KiB）。
    meta: usize,
}

fn le16(b: &[u8], o: usize) -> usize {
    usize::from(u16::from_le_bytes([b[o], b[o + 1]]))
}

fn le32(b: &[u8], o: usize) -> usize {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as usize
}

fn make(kind: qy_fat_image::Kind) -> Base {
    let a: Vec<u8> = (0..3000u32).map(|i| (i * 31 + 7) as u8).collect();
    let b: Vec<u8> = (0..700u32).map(|i| (i * 13) as u8).collect();
    let c: Vec<u8> = (0..5000u32).map(|i| (i ^ 0x5a) as u8).collect();
    let image = match qy_fat_image::build_kind(
        &[
            ("A.WAV", &a),
            ("B.FLA", &b),
            ("long file name.wav", &c),
            ("D.WAV", &[]),
        ],
        512,
        kind,
    ) {
        Ok(i) => i,
        Err(e) => panic!("基础镜像：{e}"),
    };
    // 数据区起点 = 分区起点 + 保留扇区 + FAT 份数 × FAT 扇区数 + FAT16 根目录扇区数。
    let bs = le32(&image, 446 + 8) * 512;
    let fat_size = match le16(&image, bs + 22) {
        0 => le32(&image, bs + 36),
        f => f,
    };
    let sectors = le16(&image, bs + 14)
        + usize::from(image[bs + 16]) * fat_size
        + (le16(&image, bs + 17) * 32).div_ceil(512);
    let meta = (bs + sectors * 512 + (64 << 10)).min(image.len());
    Base { image, meta }
}

fn base(fat32: bool) -> &'static Base {
    static B16: OnceLock<Base> = OnceLock::new();
    static B32: OnceLock<Base> = OnceLock::new();
    if fat32 {
        B32.get_or_init(|| make(qy_fat_image::Kind::Fat32))
    } else {
        B16.get_or_init(|| make(qy_fat_image::Kind::Fat16))
    }
}

/// 基础镜像加补丁（按顺序叠加，后写的覆盖先写的）。
struct Img<'a> {
    base: &'a [u8],
    patches: Vec<(usize, &'a [u8])>,
    reads: Cell<u64>,
}

#[derive(Debug)]
struct OutOfRange;

impl fmt::Display for OutOfRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("越界")
    }
}

impl core::error::Error for OutOfRange {}

impl BlockDevice for Img<'_> {
    type Error = OutOfRange;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), OutOfRange> {
        for (i, b) in blocks.iter_mut().enumerate() {
            self.reads.set(self.reads.get() + 1);
            assert!(
                self.reads.get() <= BLOCK_LIMIT,
                "块读取超过 {BLOCK_LIMIT} 次（无界读取）"
            );
            let off = (start.0 as usize)
                .checked_add(i)
                .and_then(|x| x.checked_mul(Block::LEN))
                .ok_or(OutOfRange)?;
            let src = self.base.get(off..off + Block::LEN).ok_or(OutOfRange)?;
            b.contents.copy_from_slice(src);
            for &(p, data) in &self.patches {
                // 补丁与本块 [off, off + 512) 的交集。
                let lo = p.max(off);
                let hi = (p + data.len()).min(off + Block::LEN);
                if lo < hi {
                    b.contents[lo - off..hi - off].copy_from_slice(&data[lo - p..hi - p]);
                }
            }
        }
        Ok(())
    }

    fn write(&self, _: &[Block], _: BlockIdx) -> Result<(), OutOfRange> {
        Err(OutOfRange)
    }

    fn num_blocks(&self) -> Result<BlockCount, OutOfRange> {
        Ok(BlockCount((self.base.len() / Block::LEN) as u32))
    }
}

fn read_all<S: Storage>(s: &mut S, track: u32) {
    if s.open(track).is_err() {
        return;
    }
    let mut buf = [0u8; 512];
    let mut total = 0usize;
    loop {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                assert!(n <= buf.len());
                total += n;
                assert!(
                    total <= READ_LIMIT,
                    "第 {track} 首读取超过 {READ_LIMIT} 字节（无界读取）"
                );
            }
        }
    }
    s.close();
}

fuzz_target!(|input: &[u8]| {
    let (fat32, mut rest) = match input.split_first() {
        Some((k, r)) => (k & 1 == 1, r),
        None => (false, input),
    };
    let base = base(fat32);
    let mut patches = Vec::new();
    while rest.len() >= 4 {
        let raw = u32::from(rest[0]) << 16 | u32::from(rest[1]) << 8 | u32::from(rest[2]);
        let len = usize::from(rest[3]).min(rest.len() - 4);
        let region = if raw & 0x80_0000 != 0 {
            base.meta
        } else {
            SPAN.min(base.image.len())
        };
        let off = (raw & 0x7F_FFFF) as usize % region;
        let end = (off + len).min(base.image.len());
        patches.push((off, &rest[4..4 + (end - off)]));
        rest = &rest[4 + len..];
    }
    let dev = Img {
        base: &base.image,
        patches,
        reads: Cell::new(0),
    };
    let (mut s, _) = FatStorage::<_, _, 16>::new(dev, NoClock);
    for t in 0..s.track_count() {
        let mut name = String::new();
        let _ = s.track_name(t, &mut name);
        read_all(&mut s, t);
    }
    let _ = s.rescan();
    if s.track_count() > 0 {
        read_all(&mut s, 0);
    }
});
