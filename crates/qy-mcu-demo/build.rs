//! 把 memory.x 放到链接器搜索路径，并只对二进制加入 cortex-m-rt 的链接脚本；生成“SD 卡”镜像
//! （MBR + FAT16，簇 512 字节，`qy-fat-image` 用 `fatfs` 生成），由程序编进闪存。
use std::path::PathBuf;
use std::{env, fs};

/// “SD 卡”上的文件：（FAT 文件名，夹具目录，夹具文件）。Embedded Full 与 Lite 各一份。
const FULL: [(&str, &str, &str); 9] = [
    ("T01.WAV", "qy-codec-wav", "pcm16_stereo_44k.wav"),
    ("T02.FLA", "qy-codec-flac", "cli_16_stereo_44k_default.flac"),
    ("T03.FLA", "qy-codec-flac", "err_frame_crc.flac"),
    ("T04.WAV", "qy-codec-wav", "ext24in32_6ch_51.wav"),
    ("T05.FLA", "qy-codec-flac", "cli_16_6ch_48k_51_side.flac"),
    ("T06.FLA", "qy-codec-flac", "cli_32_stereo_192k.flac"),
    ("T07.FLA", "qy-codec-flac", "cli_24_mono_96k_lpc12.flac"),
    ("T08.FLA", "qy-codec-flac", "fixed_orders_and_lpc32.flac"),
    ("T09.WAV", "qy-codec-wav", "pcm24_mono_48k_odd_list.wav"),
];
const LITE: [(&str, &str, &str); 6] = [
    ("T01.WAV", "qy-codec-wav", "pcm16_stereo_44k.wav"),
    ("T02.FLA", "qy-codec-flac", "cli_16_stereo_44k_default.flac"),
    ("T03.WAV", "qy-codec-wav", "ext16_6ch_51_side.wav"),
    ("T04.WAV", "qy-codec-wav", "pcm24_mono_48k_odd_list.wav"),
    ("T05.WAV", "qy-codec-wav", "pcm32_stereo_96k.wav"),
    ("T06.WAV", "qy-codec-wav", "pcm16_mono_8k_fmt18.wav"),
];

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap_or_default());
    if let Err(e) = fs::write(out.join("memory.x"), include_bytes!("memory.x")) {
        println!("cargo::error=写入 memory.x 失败: {e}");
    }
    println!("cargo::rustc-link-search={}", out.display());
    println!("cargo::rerun-if-changed=memory.x");
    println!("cargo::rustc-link-arg-bins=-Tlink.x");

    let full = env::var_os("CARGO_FEATURE_FULL").is_some();
    let list: &[(&str, &str, &str)] = if full { &FULL } else { &LITE };
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default()).join("..");
    let mut data = Vec::new();
    for (_, dir, file) in list {
        let p = root.join(dir).join("tests/fixtures").join(file);
        println!("cargo::rerun-if-changed={}", p.display());
        match fs::read(&p) {
            Ok(b) => data.push(b),
            Err(e) => {
                println!("cargo::error=读取 {} 失败: {e}", p.display());
                return;
            }
        }
    }
    let files: Vec<(&str, &[u8])> = list
        .iter()
        .zip(&data)
        .map(|((name, _, _), d)| (*name, d.as_slice()))
        .collect();
    match qy_fat_image::build(&files) {
        Ok(img) => {
            if let Err(e) = fs::write(out.join("card.img"), img) {
                println!("cargo::error=写入镜像失败: {e}");
            }
        }
        Err(e) => println!("cargo::error=生成镜像失败: {e}"),
    }
}
