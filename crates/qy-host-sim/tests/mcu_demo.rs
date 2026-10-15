//! M3 T1/T2：MCU 演示程序（`crates/qy-mcu-demo`，thumbv7em：通用嵌入式宿主 `qy-host-embedded`、
//! 闪存中的 FAT16 “SD 卡”镜像、软件模拟 DMA 与串口命令脚本）在 QEMU `mps2-an386`（Cortex-M4F）
//! 中运行的结果核对。这是指令集仿真，不是实机运行。
//!
//! 需要先构建带 `hold` 特性的演示程序，并通过环境变量给出 ELF 路径：
//! - `QY_MCU_DEMO_ELF`：Embedded Full（默认特性 + `hold`）；
//! - `QY_MCU_DEMO_LITE_ELF`：Embedded Lite（`--no-default-features --features hold`）。
//!
//! 未给出时测试标记为未运行并通过；设置 `QY_MCU_DEMO_REQUIRED` 时（CI 的 mcu-demo 作业）必须运行。
//! 核对内容：
//! - 每个设备段的格式、帧数与 FNV-1a 64 校验和等于由独立参考（FLAC：libFLAC `.s32le`；
//!   WAV：ffmpeg `.ffmpeg.s32le`；浮点 WAV：ffmpeg 原样输出的位模式按定义独立换算为 N 位整数）
//!   按公式 `s << (C − N)` 放置后的字节；
//! - 浮点曲目（F3）：Full 开启 `float-to-int` 后转为 24 位并报告“样本已改变”及统计，Lite 严格
//!   路径按设备不接受浮点跳过；
//! - 跳过的曲目及原因、无欠载、暂停静音单独计数；
//! - ELF 各段大小（`.text`/`.rodata`/`.data`/`.bss`）；Lite 版不含任何 `qy_codec_flac` 符号；
//! - 栈高水位：运行结束后经 QEMU 监视器导出 RAM，统计复位时涂上的 0xCCCCCCCC 剩余量。
//!
//! 报告写入 `target/qybit-report/qybit-mcu-demo.txt`。

// QEMU 监视器经 Unix 套接字访问；该检查只在 Linux CI 作业中运行。
#![cfg(unix)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn fixtures(dir: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../{dir}/tests/fixtures"))
}

fn read_i32(path: &Path) -> Vec<i32> {
    match std::fs::read(path) {
        Ok(b) => b
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
        Err(e) => panic!("{}: {e}", path.display()),
    }
}

/// 源样本（右对齐，N 位）：FLAC 取 libFLAC 参考，WAV 取 ffmpeg 左对齐 s32 右移 32 − N 位。
/// 浮点 WAV（`float/…`，F3 转换）：取 ffmpeg 原样输出的 binary32 位模式，按定义独立换算为 N 位
/// 整数：x·2^(N−1) 就近取偶、限幅到 N 位范围（f32 → f64 与乘 2 的幂都是精确的）。
fn samples(name: &str, bits: u32) -> Vec<i32> {
    if let Some(stem) = name
        .strip_prefix("float/")
        .and_then(|n| n.strip_suffix(".wav"))
    {
        let full = f64::from(1u32 << (bits - 1));
        read_i32(&fixtures("qy-codec-wav").join(format!("float/{stem}.ffmpeg.raw")))
            .into_iter()
            .map(|w| {
                let x = f64::from(f32::from_bits(w as u32));
                assert!(x.is_finite(), "{name}: 参考含非有限值");
                (x * full).round_ties_even().clamp(-full, full - 1.0) as i32
            })
            .collect()
    } else if let Some(stem) = name.strip_suffix(".flac") {
        read_i32(&fixtures("qy-codec-flac").join(format!("{stem}.s32le")))
    } else if let Some(stem) = name.strip_suffix(".wav") {
        read_i32(&fixtures("qy-codec-wav").join(format!("{stem}.ffmpeg.s32le")))
            .into_iter()
            .map(|s| s >> (32 - bits))
            .collect()
    } else {
        panic!("{name}")
    }
}

/// 独立公式：N 位样本放入 C 位容器（左对齐），小端 C/8 字节。
fn place(s: &[i32], n: u32, c: u32) -> Vec<u8> {
    s.iter()
        .flat_map(|&x| {
            let w = ((i64::from(x)) << (c - n)) as u32;
            w.to_le_bytes()[..(c / 8) as usize].to_vec()
        })
        .collect()
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 期望的设备段：(采样率, 声道, C, V, [(文件, N, 起始帧)])。
type SegSpec = (u32, u16, u32, u32, &'static [(&'static str, u32, usize)]);

/// 演示脚本在 `play` 之后立即 `seek 5`（S1）：第一首（44.1 kHz，300 帧）从第 220 帧开始。
const SEEK_FRAME: usize = 220;

struct Expect {
    /// （FAT 短文件名，夹具文件）：播放列表由 `ls` 命令的回应核对。
    tracks: &'static [(&'static str, &'static str)],
    segments: &'static [SegSpec],
    skipped: &'static [(u32, &'static str)],
    pause_periods: u32,
    has_flac: bool,
    /// F3 浮点曲目的回应与通知行（按出现顺序）。
    float: &'static [&'static str],
}

const FULL: Expect = Expect {
    tracks: &[
        ("T01.WAV", "pcm16_stereo_44k.wav"),
        ("T02.FLA", "cli_16_stereo_44k_default.flac"),
        ("T03.FLA", "err_frame_crc.flac"),
        ("T04.WAV", "ext24in32_6ch_51.wav"),
        ("T05.FLA", "cli_16_6ch_48k_51_side.flac"),
        ("T06.FLA", "cli_32_stereo_192k.flac"),
        ("T07.FLA", "cli_24_mono_96k_lpc12.flac"),
        ("T08.FLA", "fixed_orders_and_lpc32.flac"),
        ("T09.WAV", "pcm24_mono_48k_odd_list.wav"),
        ("T10.WAV", "float/f32_stereo_44k_tag3.wav"),
    ],
    segments: &[
        // WAV 16 → FLAC 16 → CRC 错误前已校验的 512 帧：同格式共用一个设备段。
        (
            44_100,
            2,
            16,
            16,
            &[
                ("pcm16_stereo_44k.wav", 16, SEEK_FRAME),
                ("cli_16_stereo_44k_default.flac", 16, 0),
                ("err_frame_crc.flac", 16, 0),
            ],
        ),
        (96_000, 1, 32, 24, &[("cli_24_mono_96k_lpc12.flac", 24, 0)]),
        // 20 位 FLAC 与 24 位 WAV 都由 32/24 承载（N ≤ V），共用一个设备段。
        (
            48_000,
            1,
            32,
            24,
            &[
                ("fixed_orders_and_lpc32.flac", 20, 0),
                ("pcm24_mono_48k_odd_list.wav", 24, 0),
            ],
        ),
        // F3：脚本在播放前开启 `float-to-int`。I²S 不接受浮点，32 位整数超出 Embedded Full 的
        // 24 位上限，转为 24 位整数放在 32/24 槽位（样本已改变）。
        (
            44_100,
            2,
            32,
            24,
            &[("float/f32_stereo_44k_tag3.wav", 24, 0)],
        ),
    ],
    skipped: &[
        (2, "Core(Decode(Malformed))"),
        // F1：Embedded 为双声道。6 声道 WAV 由引擎按能力不足拒绝；6 声道 FLAC 在 STREAMINFO 处
        // 即超出解码器的双声道容量，R1 起同样报能力不足（原先只报 CapacityExceeded）。都不打开设备。
        (3, "Capability(Channels { channels: 6, max: 2 })"),
        (4, "Capability(Channels { channels: 6, max: 2 })"),
        (5, "Capability(Rate { rate: 192000, max: 96000 })"),
    ],
    pause_periods: 50,
    has_flac: true,
    float: &[
        "OK float-to-int=on altered samples-changed applies-to=next-tracks",
        // 先按源格式请求浮点，设备不接受，再按转换回退。
        "QYBIT-UNSUPPORTED Float",
        "QYBIT-ALTERED track=9 float->int24",
        "QYBIT-FLOAT track=9 samples=600 clipped=0 inexact=133 nonfinite=0",
    ],
};

const LITE: Expect = Expect {
    tracks: &[
        ("T01.WAV", "pcm16_stereo_44k.wav"),
        ("T02.FLA", "cli_16_stereo_44k_default.flac"),
        ("T03.WAV", "ext16_6ch_51_side.wav"),
        ("T04.WAV", "pcm24_mono_48k_odd_list.wav"),
        ("T05.WAV", "pcm32_stereo_96k.wav"),
        ("T06.WAV", "float/f32_mono_48k_ext.wav"),
        ("T07.WAV", "pcm16_mono_8k_fmt18.wav"),
    ],
    segments: &[
        (
            44_100,
            2,
            16,
            16,
            &[("pcm16_stereo_44k.wav", 16, SEEK_FRAME)],
        ),
        (8_000, 1, 16, 16, &[("pcm16_mono_8k_fmt18.wav", 16, 0)]),
    ],
    skipped: &[
        // 未链接 FLAC：按 Unsupported 拒绝。
        (1, "Core(Decode(Unsupported))"),
        (2, "Capability(Channels { channels: 6, max: 2 })"),
        (3, "Capability(Bits { bits: 24, max: 16 })"),
        (4, "Capability(Rate { rate: 96000, max: 48000 })"),
        // F3：严格路径（未开启 `float-to-int`），I²S 不接受浮点：设备格式选择失败，跳过。
        (5, "Core(Configure)"),
    ],
    // S1：第一首定位到第 220 帧后只剩 80 帧，脚本在第 2 个周期暂停时设备正在换曲（暂停静音只在设备
    // 运行时计数），暂停静音由 2 个周期变为 1 个。
    pause_periods: 1,
    has_flac: false,
    float: &["QYBIT-UNSUPPORTED Float"],
};

fn expected_seg_lines(e: &Expect) -> Vec<String> {
    e.segments
        .iter()
        .enumerate()
        .map(|(i, (rate, ch, c, v, files))| {
            let bytes: Vec<u8> = files
                .iter()
                .flat_map(|(name, n, start)| {
                    let s = samples(name, *n);
                    place(&s[start * usize::from(*ch)..], *n, *c)
                })
                .collect();
            let frames = bytes.len() / (usize::from(*ch) * (*c / 8) as usize);
            format!(
                "QYBIT-SEG i={i} rate={rate} ch={ch} c={c} v={v} frames={frames} fnv={:016x}",
                fnv(&bytes)
            )
        })
        .collect()
}

/// ELF32 小端：各节大小与符号名。
struct Elf {
    sections: BTreeMap<String, u64>,
    /// 符号名与值。
    symbols: Vec<(String, usize)>,
}

fn u16_at(b: &[u8], o: usize) -> usize {
    b.get(o..o + 2)
        .map_or(0, |x| usize::from(u16::from_le_bytes([x[0], x[1]])))
}

fn u32_at(b: &[u8], o: usize) -> usize {
    b.get(o..o + 4)
        .map_or(0, |x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]) as usize)
}

fn cstr(b: &[u8], o: usize) -> String {
    let s = b.get(o..).unwrap_or(&[]);
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf8_lossy(&s[..end]).into_owned()
}

fn parse_elf(path: &Path) -> Elf {
    let b = std::fs::read(path).unwrap_or_default();
    assert_eq!(b.get(..5), Some(&b"\x7fELF\x01"[..]), "不是 ELF32");
    let (shoff, shentsize, shnum, shstrndx) = (
        u32_at(&b, 0x20),
        u16_at(&b, 0x2e),
        u16_at(&b, 0x30),
        u16_at(&b, 0x32),
    );
    let sh = |i: usize, f: usize| u32_at(&b, shoff + i * shentsize + f);
    let names_off = sh(shstrndx, 16);
    let mut sections = BTreeMap::new();
    let mut symbols = Vec::new();
    for i in 0..shnum {
        let name = cstr(&b, names_off + sh(i, 0));
        sections.insert(name, sh(i, 20) as u64);
        if sh(i, 4) == 2 {
            // SHT_SYMTAB：每项 16 字节，名字在 sh_link 指向的字符串表。
            let (off, size, link) = (sh(i, 16), sh(i, 20), sh(i, 24));
            let strtab = sh(link, 16);
            for k in 0..size / 16 {
                let e = off + k * 16;
                symbols.push((cstr(&b, strtab + u32_at(&b, e)), u32_at(&b, e + 4)));
            }
        }
    }
    Elf { sections, symbols }
}

struct Run {
    lines: Vec<String>,
    /// 栈顶（`_stack_start`）以下的高水位（含复位处理与 main 自身的栈帧）。
    stack_used: usize,
    /// 其中 main 入口（读 MSP 处）以下的部分。
    below_main: usize,
    stack_reserved: usize,
}

/// 在 QEMU 中运行（`hold` 版本跑完后停在 WFI），读输出，导出栈区内存后退出。
fn run_qemu(elf: &Path, tag: &str, stack_start: usize) -> Run {
    // 监视器命令按空白分隔参数：路径中不能有空格。
    let tag: String = tag.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let dir = std::env::temp_dir().join(format!("qybit-mcu-{}-{tag}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let sock = dir.join("monitor.sock");
    let dump = dir.join("ram.bin");
    let mut child = match Command::new("qemu-system-arm")
        .args(["-cpu", "cortex-m4", "-machine", "mps2-an386", "-nographic"])
        .args([
            "-serial",
            "null",
            "-semihosting-config",
            "enable=on,target=native",
        ])
        .arg("-monitor")
        .arg(format!("unix:{},server,nowait", sock.display()))
        .arg("-kernel")
        .arg(elf)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => panic!("无法启动 qemu-system-arm: {e}"),
    };
    let Some(out) = child.stdout.take() else {
        panic!("stdout");
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut lines = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(l) => {
                let done = l.starts_with("QYBIT-DONE") || l.starts_with("QYBIT-PANIC");
                lines.push(l);
                if done {
                    break;
                }
            }
            Err(_) => {
                let _ = child.kill();
                panic!("QEMU 超时或提前退出：{lines:#?}");
            }
        }
    }
    let field = |key: &str| -> usize {
        lines
            .iter()
            .find_map(|l| {
                l.split_whitespace()
                    .find_map(|w| w.strip_prefix(&format!("{key}=0x")))
            })
            .and_then(|h| usize::from_str_radix(h, 16).ok())
            .unwrap_or_else(|| panic!("缺少 {key}"))
    };
    let (msp, heap) = (field("stack_top"), field("heap_start"));
    let top = stack_start;
    assert!(heap < msp && msp <= top, "栈区间异常");
    let mut mon = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(e) => panic!("连接 QEMU 监视器失败: {e}"),
    };
    let _ = mon.set_read_timeout(Some(Duration::from_secs(30)));
    let wait_prompt = |m: &mut UnixStream| {
        let mut acc = Vec::new();
        let mut buf = [0u8; 4096];
        while !acc.ends_with(b"(qemu) ") {
            match m.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => acc.extend_from_slice(&buf[..n]),
            }
        }
    };
    wait_prompt(&mut mon);
    // 文件名须加引号：否则以 “/” 开头的路径会被当作长度表达式中的除号。
    let cmd = format!("pmemsave {heap} {} \"{}\"\n", top - heap, dump.display());
    let _ = mon.write_all(cmd.as_bytes());
    wait_prompt(&mut mon);
    let _ = mon.write_all(b"quit\n");
    let _ = child.wait();
    let ram = std::fs::read(&dump).unwrap_or_default();
    assert_eq!(ram.len(), top - heap, "内存导出不完整");
    let unused = ram.chunks_exact(4).take_while(|w| *w == [0xcc; 4]).count() * 4;
    let _ = std::fs::remove_dir_all(&dir);
    let lowest = heap + unused;
    Run {
        lines,
        stack_used: top - lowest,
        below_main: msp.saturating_sub(lowest),
        stack_reserved: top - heap,
    }
}

fn check(tag: &str, elf: &Path, e: &Expect, report: &mut String) {
    let info = parse_elf(elf);
    let flac_syms = info
        .symbols
        .iter()
        .filter(|s| s.0.contains("qy_codec_flac"))
        .count();
    assert_eq!(
        flac_syms > 0,
        e.has_flac,
        "{tag}: qy_codec_flac 符号数 {flac_syms}"
    );

    let Some(&(_, stack_start)) = info.symbols.iter().find(|s| s.0 == "_stack_start") else {
        panic!("{tag}: 缺少 _stack_start");
    };
    let r = run_qemu(elf, tag, stack_start);
    let lines = &r.lines;
    assert!(
        lines.iter().any(|l| l == "QYBIT-DONE code=0"),
        "{tag}: {lines:#?}"
    );
    // 播放列表：`ls` 命令回应的 `TRACK i 名称` 行（FAT 根目录按短文件名排序）。
    assert!(
        lines
            .iter()
            .any(|l| *l == format!("OK ls tracks={}", e.tracks.len())),
        "{tag}: ls 回应"
    );
    // S1：`play` 之后立即 `seek 5`：格式尚未确定，先记下，格式确定后（入队音频之前）定位。
    assert!(
        lines.iter().any(|l| l == "OK seek track=0 pending ms=5"),
        "{tag}: seek 回应"
    );
    let tracks: Vec<String> = lines
        .iter()
        .filter_map(|l| l.strip_prefix("TRACK "))
        .map(str::to_owned)
        .collect();
    let want_tracks: Vec<String> = e
        .tracks
        .iter()
        .enumerate()
        .map(|(i, (fat, _))| format!("{i} {fat}"))
        .collect();
    assert_eq!(tracks, want_tracks, "{tag}: 播放列表");
    for cmd in ["OK play from=0", "OK pause", "OK resume"] {
        assert!(lines.iter().any(|l| l == cmd), "{tag}: 缺少回应 {cmd}");
    }
    let Some(status) = lines.iter().find(|l| l.starts_with("STATUS ")) else {
        panic!("{tag}: 缺少状态行");
    };
    for want in [
        "state=stopped",
        "underruns=0",
        "read_errors=0",
        "open_errors=0",
        "output_errors=0",
        "feeder_errors=0",
        "protocol_errors=0",
        // R1 水位：QEMU 中 DMA 只在主循环空闲时取数，不会出现临界。
        "wm_critical=0",
    ] {
        assert!(status.contains(want), "{tag}: {status}");
    }
    let segs: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("QYBIT-SEG"))
        .collect();
    assert_eq!(
        segs.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        expected_seg_lines(e),
        "{tag}: 设备段与独立参考不一致"
    );
    let skipped: Vec<String> = lines
        .iter()
        .filter_map(|l| l.strip_prefix("QYBIT-SKIP "))
        .map(str::to_owned)
        .collect();
    let want: Vec<String> = e
        .skipped
        .iter()
        .map(|(t, f)| format!("track={t} fault={f}"))
        .collect();
    assert_eq!(skipped, want, "{tag}: 跳过的曲目");
    let Some(stat) = lines.iter().find(|l| l.starts_with("QYBIT-STAT")) else {
        panic!("{tag}: 缺少统计");
    };
    for want in [
        "finished=1".to_owned(),
        "underruns=0".to_owned(),
        format!("pause_periods={}", e.pause_periods),
        "errors=0".to_owned(),
        "protocol=0".to_owned(),
    ] {
        assert!(stat.contains(&format!(" {want}")), "{tag}: {stat}");
    }
    assert!(!lines.iter().any(|l| l.starts_with("QYBIT-FAIL")));
    let float: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .filter(|l| {
            l.starts_with("OK float-to-int")
                || l.starts_with("QYBIT-UNSUPPORTED")
                || l.starts_with("QYBIT-ALTERED")
                || l.starts_with("QYBIT-FLOAT")
        })
        .collect();
    assert_eq!(float, e.float, "{tag}: 浮点曲目");

    let s = &info.sections;
    let get = |k: &str| s.get(k).copied().unwrap_or(0);
    report.push_str(&format!(
        "== {tag}（{}）\n{}\nELF：.vector_table {} / .text {} / .rodata {}（含“SD 卡”文件）/ .data {} / .bss {} / .uninit {} 字节\n\
         栈：高水位 {} 字节（自 _stack_start 起，含复位与 main 栈帧；其中 main 入口以下 {} 字节；\
         栈区 {} 字节；复位涂色 0xCCCCCCCC 统计，QEMU 仿真）\n\n",
        elf.display(),
        lines.join("\n"),
        get(".vector_table"),
        get(".text"),
        get(".rodata"),
        get(".data"),
        get(".bss"),
        get(".uninit"),
        r.stack_used,
        r.below_main,
        r.stack_reserved,
    ));
    assert!(
        r.stack_used > 0 && r.stack_used < r.stack_reserved,
        "{tag}: 栈统计异常"
    );
}

#[test]
fn mcu_demo_in_qemu() {
    let required = std::env::var_os("QY_MCU_DEMO_REQUIRED").is_some();
    let mut report = String::from(
        "QyBit MCU 演示（M3 T1/T2）：QEMU mps2-an386（Cortex-M4F）指令集仿真结果，不是实机运行；\
         读卡与解码时序为名义值，不是实测。\n\n",
    );
    let mut ran = 0;
    for (tag, var, e) in [
        ("Embedded Full", "QY_MCU_DEMO_ELF", &FULL),
        ("Embedded Lite", "QY_MCU_DEMO_LITE_ELF", &LITE),
    ] {
        match std::env::var_os(var) {
            Some(p) => {
                // 相对路径按仓库根目录解析（测试的工作目录是本 crate）。
                let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
                check(tag, &root.join(p), e, &mut report);
                ran += 1;
            }
            None => {
                assert!(!required, "{var} 未设置，但要求运行 MCU 演示");
                eprintln!("MCU 演示 {tag}：未运行（未设置 {var}）");
            }
        }
    }
    if ran > 0 {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/qybit-report");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("qybit-mcu-demo.txt");
        if let Err(e) = std::fs::write(&path, &report) {
            panic!("写报告失败: {e}");
        }
        eprintln!("{report}报告：{}", path.display());
    }
}
