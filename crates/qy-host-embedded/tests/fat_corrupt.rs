//! 损坏文件系统的回归用例（R1 §4）：fuzz 发现的问题与按 FAT 规范（fatgen103）构造的各类损坏结构。
//!
//! 镜像由 `qy-fat-image`（`fatfs` 格式化）生成，FAT16 与 FAT32 各跑一遍。损坏位置由本文件按规范
//! 独立解析引导扇区与目录项得到，不依赖被测的 `fatcheck`。每个用例检查：不 panic；损坏的文件被跳过
//! 并计入 `corrupt_entries`（或整卷以 `FatError::Corrupt` 拒绝）；其余文件照常读出且内容一致；
//! 读取总量有上界。

use std::cell::Cell;
use std::fmt;

use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};
use qy_fat_image::Kind;
use qy_host_embedded::{FatError, FatStorage, NoClock, Storage};

const SECTOR: usize = 512;
/// 单个文件读取上限：远大于镜像中最大的文件，超过即视为无界读取。
const READ_LIMIT: usize = 1 << 20;

struct Disk {
    data: Vec<u8>,
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

impl BlockDevice for Disk {
    type Error = OutOfRange;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), OutOfRange> {
        for (i, b) in blocks.iter_mut().enumerate() {
            self.reads.set(self.reads.get() + 1);
            let off = (start.0 as usize + i) * SECTOR;
            let src = self.data.get(off..off + SECTOR).ok_or(OutOfRange)?;
            b.contents.copy_from_slice(src);
        }
        Ok(())
    }

    fn write(&self, _: &[Block], _: BlockIdx) -> Result<(), OutOfRange> {
        Err(OutOfRange)
    }

    fn num_blocks(&self) -> Result<BlockCount, OutOfRange> {
        Ok(BlockCount((self.data.len() / SECTOR) as u32))
    }
}

type Fs = FatStorage<Disk, NoClock, 8>;

fn files() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("A.WAV", bytes(3000, 1)),
        ("B.FLA", bytes(700, 2)),
        ("C.WAV", bytes(5000, 3)),
    ]
}

/// 不重复的内容（乘法散列），读错簇必然与参考不同。
fn bytes(n: u32, seed: u32) -> Vec<u8> {
    (0..n)
        .map(|i| ((i.wrapping_add(seed << 16)).wrapping_mul(0x9E37_79B1) >> 24) as u8)
        .collect()
}

fn image(kind: Kind) -> Vec<u8> {
    let f = files();
    let refs: Vec<(&str, &[u8])> = f.iter().map(|(n, d)| (*n, d.as_slice())).collect();
    match qy_fat_image::build_kind(&refs, SECTOR, kind) {
        Ok(i) => i,
        Err(e) => panic!("镜像：{e}"),
    }
}

/// 按规范独立解析的布局（字节偏移均为整盘绝对偏移）。
struct Layout {
    boot: usize,
    fat: usize,
    fat32: bool,
}

fn le16(b: &[u8], o: usize) -> usize {
    usize::from(u16::from_le_bytes([b[o], b[o + 1]]))
}

fn le32(b: &[u8], o: usize) -> usize {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as usize
}

fn layout(img: &[u8]) -> Layout {
    let boot = le32(img, 446 + 8) * SECTOR;
    let bs = &img[boot..boot + SECTOR];
    Layout {
        boot,
        fat: boot + le16(bs, 14) * SECTOR,
        fat32: le16(bs, 22) == 0,
    }
}

/// 短文件名目录项的偏移（32 字节对齐、属性不是长文件名 0x0F）。
fn dir_entry(img: &[u8], name: &[u8; 11]) -> usize {
    (0..img.len() / 32)
        .map(|i| i * 32)
        .find(|&o| &img[o..o + 11] == name && img[o + 11] != 0x0F)
        .unwrap_or_else(|| panic!("找不到目录项 {:?}", String::from_utf8_lossy(name)))
}

fn start_cluster(img: &[u8], l: &Layout, e: usize) -> usize {
    let hi = if l.fat32 { le16(img, e + 20) } else { 0 };
    hi << 16 | le16(img, e + 26)
}

fn set_start_cluster(img: &mut [u8], l: &Layout, e: usize, c: u32) {
    img[e + 26..e + 28].copy_from_slice(&(c as u16).to_le_bytes());
    if l.fat32 {
        img[e + 20..e + 22].copy_from_slice(&((c >> 16) as u16).to_le_bytes());
    }
}

/// 写第 1 份 FAT 的表项（`embedded-sdmmc` 与 `fatcheck` 都只读第 1 份）。
fn set_fat(img: &mut [u8], l: &Layout, cluster: usize, value: u32) {
    if l.fat32 {
        let o = l.fat + cluster * 4;
        img[o..o + 4].copy_from_slice(&value.to_le_bytes());
    } else {
        let o = l.fat + cluster * 2;
        img[o..o + 2].copy_from_slice(&(value as u16).to_le_bytes());
    }
}

fn read_track(s: &mut Fs, track: u32) -> Result<Vec<u8>, FatError<OutOfRange>> {
    s.open(track)?;
    let mut out = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        let n = s.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        assert!(out.len() <= READ_LIMIT, "第 {track} 首无界读取");
    }
    s.close();
    Ok(out)
}

fn names(s: &Fs) -> Vec<String> {
    (0..s.track_count())
        .map(|t| {
            let mut n = String::new();
            let _ = s.track_name(t, &mut n);
            n
        })
        .collect()
}

/// 扫描并读完全部曲目，核对未损坏的文件内容一致；返回播放列表与损坏计数。
fn scan_and_read(img: Vec<u8>) -> (Vec<String>, u32, Result<u32, FatError<OutOfRange>>) {
    let disk = Disk {
        data: img,
        reads: Cell::new(0),
    };
    let (mut s, r) = Fs::new(disk, NoClock);
    let list = names(&s);
    let reference = files();
    for (t, name) in list.iter().enumerate() {
        let got = match read_track(&mut s, t as u32) {
            Ok(g) => g,
            Err(e) => panic!("{name} 读取失败：{e:?}"),
        };
        let want = &reference
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("意外的曲目 {name}"))
            .1;
        assert_eq!(&got, want, "{name} 内容");
    }
    // 重新扫描结果相同。
    let again = s.rescan();
    assert_eq!(names(&s), list);
    assert_eq!(
        format!("{again:?}"),
        format!("{r:?}"),
        "重新扫描的结果应与首次相同"
    );
    let corrupt = s.corrupt_entries;
    (list, corrupt, r)
}

fn kinds() -> [Kind; 2] {
    [Kind::Fat16, Kind::Fat32]
}

#[test]
fn intact_images_scan_and_read() {
    for kind in kinds() {
        let (list, corrupt, r) = scan_and_read(image(kind));
        assert_eq!(list, ["A.WAV", "B.FLA", "C.WAV"], "{kind:?}");
        assert_eq!(corrupt, 0, "{kind:?}");
        assert!(matches!(r, Ok(3)), "{kind:?}: {r:?}");
    }
}

/// fuzz 发现（`crash-82f0d370…`：FAT16 第 1 份 FAT 中一个表项的低字节改为 0）：簇链中出现空闲表项 0，
/// `embedded-sdmmc` 0.10.0 的 `cluster_to_block` 计算 `0 - 2` 溢出 panic。现在该文件被跳过。
#[test]
fn chain_hits_free_entry() {
    for kind in kinds() {
        let mut img = image(kind);
        let l = layout(&img);
        let e = dir_entry(&img, b"A       WAV");
        let c = start_cluster(&img, &l, e);
        set_fat(&mut img, &l, c, 0);
        let (list, corrupt, r) = scan_and_read(img);
        assert_eq!(list, ["B.FLA", "C.WAV"], "{kind:?}");
        assert_eq!(corrupt, 1, "{kind:?}");
        assert!(matches!(r, Ok(2)), "{kind:?}: {r:?}");
    }
}

/// 目录项起始簇 0、1（保留）与越过最后一个数据簇。
#[test]
fn start_cluster_out_of_range() {
    for kind in kinds() {
        for bad in [0u32, 1, 0x0FFF_FFF0] {
            let mut img = image(kind);
            let l = layout(&img);
            let e = dir_entry(&img, b"C       WAV");
            let bad = if l.fat32 { bad } else { bad & 0xFFFF };
            set_start_cluster(&mut img, &l, e, bad);
            let (list, corrupt, _) = scan_and_read(img);
            assert_eq!(list, ["A.WAV", "B.FLA"], "{kind:?} 起始簇 {bad:#x}");
            assert_eq!(corrupt, 1, "{kind:?} 起始簇 {bad:#x}");
        }
    }
}

/// 簇链环路（指回自身）、坏簇标记、提前结束（文件长度大于簇链）与指向越界簇。
#[test]
fn chain_loop_bad_and_short() {
    for kind in kinds() {
        let probe = image(kind);
        let l = layout(&probe);
        let (eoc, bad, beyond) = if l.fat32 {
            (0x0FFF_FFFF, 0x0FFF_FFF7, 0x0FFF_FFF0)
        } else {
            (0xFFFF, 0xFFF7, 0xFFF0)
        };
        let c = start_cluster(&probe, &l, dir_entry(&probe, b"A       WAV"));
        for (what, value) in [
            ("环路", c as u32),
            ("坏簇", bad),
            ("提前结束", eoc),
            ("越界簇", beyond),
        ] {
            let mut img = probe.clone();
            set_fat(&mut img, &l, c, value);
            let (list, corrupt, _) = scan_and_read(img);
            assert_eq!(list, ["B.FLA", "C.WAV"], "{kind:?} {what}");
            assert_eq!(corrupt, 1, "{kind:?} {what}");
        }
    }
}

fn get_fat(img: &[u8], l: &Layout, cluster: usize) -> usize {
    if l.fat32 {
        le32(img, l.fat + cluster * 4) & 0x0FFF_FFFF
    } else {
        le16(img, l.fat + cluster * 2)
    }
}

/// 沿簇链取出文件的全部簇（基础镜像未损坏，链必然结束）。
fn chain(img: &[u8], l: &Layout, start: usize) -> Vec<usize> {
    let end = if l.fat32 { 0x0FFF_FFF8 } else { 0xFFF8 };
    let mut out = vec![start];
    loop {
        let n = get_fat(img, l, out[out.len() - 1]);
        if n >= end {
            return out;
        }
        out.push(n);
    }
}

/// 较长的环路：链中间指回前面（环在文件长度以内），以及最后一个簇指回起始簇（环在文件数据之后）。
/// 两者都拒绝；最后一个簇的表项改为空闲 0（文件数据之后无效）不影响读取，照常接受并读出正确内容。
#[test]
fn longer_loops_and_tail_entry() {
    for kind in kinds() {
        let probe = image(kind);
        let l = layout(&probe);
        let c = chain(
            &probe,
            &l,
            start_cluster(&probe, &l, dir_entry(&probe, b"A       WAV")),
        );
        assert_eq!(c.len(), 6, "{kind:?}：3000 字节 / 512 字节簇");
        for (what, at, to, rejected) in [
            ("中间环路", c[3], c[1] as u32, true),
            ("尾部环路", c[5], c[0] as u32, true),
            ("尾部空闲", c[5], 0, false),
        ] {
            let mut img = probe.clone();
            set_fat(&mut img, &l, at, to);
            let (list, corrupt, _) = scan_and_read(img);
            if rejected {
                assert_eq!(list, ["B.FLA", "C.WAV"], "{kind:?} {what}");
                assert_eq!(corrupt, 1, "{kind:?} {what}");
            } else {
                assert_eq!(list, ["A.WAV", "B.FLA", "C.WAV"], "{kind:?} {what}");
                assert_eq!(corrupt, 0, "{kind:?} {what}");
            }
        }
    }
}

/// 文件长度远大于卷容量：不沿链读取，直接跳过。
#[test]
fn size_larger_than_volume() {
    for kind in kinds() {
        let mut img = image(kind);
        let e = dir_entry(&img, b"B       FLA");
        img[e + 28..e + 32].copy_from_slice(&u32::MAX.to_le_bytes());
        let (list, corrupt, _) = scan_and_read(img);
        assert_eq!(list, ["A.WAV", "C.WAV"], "{kind:?}");
        assert_eq!(corrupt, 1, "{kind:?}");
    }
}

/// 引导扇区损坏：整卷拒绝，曲目数 0，以 `Corrupt` 返回而不是交给 FAT 库。
#[test]
fn boot_sector_rejected() {
    for kind in kinds() {
        let probe = image(kind);
        let l = layout(&probe);
        let edits: [(&str, usize, u8); 4] = [
            ("每扇区字节数 0", 12, 0),
            ("每簇扇区数 0", 13, 0),
            ("每簇扇区数 3", 13, 3),
            ("FAT 份数 0", 16, 0),
        ];
        for (what, off, value) in edits {
            let mut img = probe.clone();
            img[l.boot + off] = value;
            let (list, _, r) = scan_and_read(img);
            assert!(list.is_empty(), "{kind:?} {what}");
            assert!(
                matches!(r, Err(FatError::Corrupt)),
                "{kind:?} {what}: {r:?}"
            );
        }
    }
}

/// FAT32 根目录簇链环路与无效起始簇：整卷拒绝。
#[test]
fn fat32_root_chain_rejected() {
    let probe = image(Kind::Fat32);
    let l = layout(&probe);
    let root = le32(&probe, l.boot + 44);
    let mut looped = probe.clone();
    set_fat(&mut looped, &l, root, root as u32);
    let mut zero = probe.clone();
    zero[l.boot + 44..l.boot + 48].copy_from_slice(&0u32.to_le_bytes());
    for (what, img) in [("环路", looped), ("起始簇 0", zero)] {
        let (list, _, r) = scan_and_read(img);
        assert!(list.is_empty(), "{what}");
        assert!(matches!(r, Err(FatError::Corrupt)), "{what}: {r:?}");
    }
}

/// 打开前重新核对：扫描之后目录项被改坏（换卡而未重新扫描），打开以 `Corrupt` 返回。
#[test]
fn reverified_before_open() {
    for kind in kinds() {
        let img = image(kind);
        let l = layout(&img);
        let e = dir_entry(&img, b"A       WAV");
        let disk = Disk {
            data: img,
            reads: Cell::new(0),
        };
        let (mut s, r) = Fs::new(disk, NoClock);
        assert!(matches!(r, Ok(3)), "{kind:?}: {r:?}");
        s.device(|d| set_start_cluster(&mut d.data, &l, e, 0));
        // embedded-sdmmc 的块缓存可能还留着目录扇区；重新核对直接读块设备。
        let r = s.open(0);
        assert!(matches!(r, Err(FatError::Corrupt)), "{kind:?}: {r:?}");
        assert!(s.device(|d| d.reads.get()).is_some_and(|n| n > 0));
    }
}

/// 扫描之后簇链被改坏（换卡而未重新扫描）：打开不再沿整条链走，读取前逐跳核对。空闲表项、越界簇、
/// 坏簇标记与提前结束都在 `embedded-sdmmc` 走到那一跳之前以 `Corrupt` 返回，此前读出的字节与原文件
/// 一致。环路在文件长度以内时可能先读出重复的簇才被发现，但读取有界（以文件长度为限）、不 panic；
/// 重新扫描时按整条链核对，照常拒绝。
#[test]
fn chain_corrupted_after_scan_is_caught_while_reading() {
    for kind in kinds() {
        let probe = image(kind);
        let l = layout(&probe);
        let c = chain(
            &probe,
            &l,
            start_cluster(&probe, &l, dir_entry(&probe, b"A       WAV")),
        );
        let (bad, beyond) = if l.fat32 {
            (0x0FFF_FFF7, 0x0FFF_FFF0)
        } else {
            (0xFFF7, 0xFFF0)
        };
        let eoc = if l.fat32 { 0x0FFF_FFFF } else { 0xFFFF };
        let want = files()[0].1.clone();
        for (what, at, value, caught) in [
            ("空闲表项", c[2], 0u32, true),
            ("越界簇", c[1], beyond, true),
            ("坏簇", c[3], bad, true),
            ("提前结束", c[4], eoc, true),
            ("环路", c[3], c[1] as u32, false),
        ] {
            let disk = Disk {
                data: probe.clone(),
                reads: Cell::new(0),
            };
            let (mut s, r) = Fs::new(disk, NoClock);
            assert!(matches!(r, Ok(3)), "{kind:?} {what}: {r:?}");
            s.device(|d| set_fat(&mut d.data, &l, at, value));
            assert!(s.open(0).is_ok(), "{kind:?} {what}：打开只读目录项");
            let mut got = Vec::new();
            let mut buf = [0u8; 512];
            let res = loop {
                match s.read(&mut buf) {
                    Ok(0) => break Ok(()),
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                    Err(e) => break Err(e),
                }
                assert!(got.len() <= READ_LIMIT, "{kind:?} {what}：无界读取");
            };
            if caught {
                assert!(
                    matches!(res, Err(FatError::Corrupt)),
                    "{kind:?} {what}: {res:?}"
                );
                assert!(
                    !got.is_empty() && want.starts_with(&got),
                    "{kind:?} {what}：前缀"
                );
            } else {
                assert!(got.len() <= want.len(), "{kind:?} {what}：以文件长度为限");
            }
            s.close();
            // 重新扫描：整条链核对，文件被跳过。
            assert!(matches!(s.rescan(), Ok(2)), "{kind:?} {what}");
            assert_eq!(names(&s), ["B.FLA", "C.WAV"], "{kind:?} {what}");
        }
    }
}
