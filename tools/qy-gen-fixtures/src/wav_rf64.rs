//! F4：RF64 / BW64（`ds64` 给出 64 位长度）的独立参考夹具。
//!
//! 期望（样本、修正、明细）由这里按构造给出，不经被测解码器得出；ffmpeg 能读的有效夹具另由
//! `wav_main` 用 ffmpeg 逐样本核对。另有两个由 ffmpeg 本身写出的 RF64 文件（可定位输出与输出到
//! 管道），核对真实写入程序的结构。夹具与普通 WAV 夹具放在同一目录，文件名以 `rf64_` / `bw64_`
//! 开头。结构依据见 tasks/F4.md §1。

use std::io::Write as _;
use std::process::{Command, Stdio};

use super::{
    Case, Result, chunk, chunk_head, encode, err_case, ext, fmt_ext, fmt_pcm, pcm, samples,
};

/// `ds64` 的 RIFF 长度由文件实际长度算出（文件长度 − 8）。
pub(crate) const AUTO: u64 = u64::MAX;

/// `ds64` 块：三个 64 位长度、表（块标识 + 64 位长度）与表之后的剩余字节。
pub(crate) fn ds64(
    riff: u64,
    data: u64,
    samples: u64,
    table: &[([u8; 4], u64)],
    extra: &[u8],
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&riff.to_le_bytes());
    body.extend_from_slice(&data.to_le_bytes());
    body.extend_from_slice(&samples.to_le_bytes());
    body.extend_from_slice(&(table.len() as u32).to_le_bytes());
    for (id, size) in table {
        body.extend_from_slice(id);
        body.extend_from_slice(&size.to_le_bytes());
    }
    body.extend_from_slice(extra);
    chunk(b"ds64", &body)
}

/// 拼出 RF64 / BW64 文件：32 位 RIFF 长度为 0xFFFFFFFF；`ds64` 的 RIFF 长度为 [`AUTO`] 时回填为
/// 文件长度 − 8（它位于文件第 20 字节）。
pub(crate) fn rf64(magic: &[u8; 4], ds: Vec<u8>, chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = magic.to_vec();
    out.extend_from_slice(&u32::MAX.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(&ds);
    for c in chunks {
        out.extend_from_slice(c);
    }
    if out.get(20..28) == Some(&AUTO.to_le_bytes()[..]) {
        let len = (out.len() - 8) as u64;
        out[20..28].copy_from_slice(&len.to_le_bytes());
    }
    out
}

/// data 块：32 位长度字段为 `size32`（RF64 通常为 0xFFFFFFFF），体为 `body`（奇数长度补 1 字节）。
pub(crate) fn data(size32: u32, body: &[u8]) -> Vec<u8> {
    let mut b = chunk_head(b"data", size32);
    b.extend_from_slice(body);
    if body.len() % 2 == 1 {
        b.push(0);
    }
    b
}

/// 期望的 `ds64` 明细文本（与测试中 `format!("ds64:{:?}", …)` 一致）。
fn d(detail: &str) -> Option<String> {
    Some(format!("ds64:{detail}"))
}

#[allow(clippy::too_many_arguments)]
fn ok(
    cs: &mut Vec<Case>,
    name: &'static str,
    file: Vec<u8>,
    fmt: (u32, u16, u32),
    c: u32,
    mask: Option<u32>,
    vals: Vec<i64>,
    ffmpeg: bool,
    repairs: Vec<String>,
) {
    cs.push(Case {
        name,
        data: file,
        result: "ok",
        fmt: Some(fmt),
        vals,
        ch: usize::from(fmt.1),
        v: fmt.2,
        c,
        mask,
        ffmpeg,
        repairs,
        short: None,
        bitstream: None,
        detail: None,
    });
}

fn bad(cs: &mut Vec<Case>, name: &'static str, file: Vec<u8>, result: &'static str, detail: &str) {
    let mut c = err_case(name, file, result);
    c.detail = d(detail);
    cs.push(c);
}

/// 用 ffmpeg 把已知的 16 位双声道样本写成 RF64（`-rf64 always`）；`pipe` 时输出到标准输出（不可
/// 定位，ffmpeg 无法回填 ds64）。
fn ffmpeg_rf64(vals: &[i64], pipe: bool, out: &std::path::Path) -> Result<Vec<u8>> {
    let raw: Vec<u8> = vals
        .iter()
        .flat_map(|&v| (v as i16).to_le_bytes())
        .collect();
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-v",
        "error",
        "-y",
        "-f",
        "s16le",
        "-ar",
        "44100",
        "-ac",
        "2",
        "-i",
        "-",
        "-c:a",
        "pcm_s16le",
        "-rf64",
        "always",
        "-f",
        "wav",
    ]);
    if pipe {
        cmd.arg("-");
    } else {
        cmd.arg(out);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("ffmpeg 没有标准输入")?;
    let feeder = std::thread::spawn(move || stdin.write_all(&raw));
    let o = child.wait_with_output()?;
    feeder.join().map_err(|_| "写入 ffmpeg 失败")??;
    if !o.status.success() {
        return Err(format!("ffmpeg 失败: {}", String::from_utf8_lossy(&o.stderr)).into());
    }
    if pipe {
        Ok(o.stdout)
    } else {
        Ok(std::fs::read(out)?)
    }
}

pub fn cases(cs: &mut Vec<Case>) -> Result<()> {
    let list = chunk(b"LIST", b"INFOISFT\x0e\x00\x00\x00Lavf60.16.100\x00");

    // ---- 有效 ----
    // ffmpeg 可定位输出的结构：ds64 28 字节、表为空，data 为 0xFFFFFFFF，三个长度已回填。
    let v = samples(401, 300, 2, 16);
    let body = encode(&v, 16, 16, None);
    let f = rf64(
        b"RF64",
        ds64(AUTO, body.len() as u64, 300, &[], &[]),
        &[
            fmt_pcm(pcm(2, 44_100, 16)),
            list.clone(),
            data(u32::MAX, &body),
        ],
    );
    ok(
        cs,
        "rf64_pcm16_stereo",
        f,
        (44_100, 2, 16),
        16,
        None,
        v,
        true,
        vec![],
    );

    // BW64，单声道 24 位，fmt 之后有 ADM 元数据块（axml、chna，奇数长度）与 JUNK。
    let v = samples(402, 251, 1, 24);
    let body = encode(&v, 24, 24, None);
    let f = rf64(
        b"BW64",
        ds64(AUTO, body.len() as u64, 251, &[], &[]),
        &[
            fmt_pcm(pcm(1, 96_000, 24)),
            chunk(b"chna", &[1, 0, 1, 0, b'A', b'T', b'U', b'_']),
            chunk(b"axml", b"<ebuCoreMain/>"),
            chunk(b"JUNK", &[0u8; 7]),
            data(u32::MAX, &body),
        ],
    );
    ok(
        cs,
        "bw64_pcm24_mono_adm",
        f,
        (96_000, 1, 24),
        24,
        None,
        v,
        true,
        vec![],
    );

    // EXTENSIBLE 5.1，32 位容器 24 位有效；data 的 32 位长度等于 ds64（不是 0xFFFFFFFF）。
    let v = samples(403, 120, 6, 24);
    let body = encode(&v, 32, 24, None);
    let f = rf64(
        b"RF64",
        ds64(AUTO, body.len() as u64, 120, &[], &[]),
        &[
            fmt_ext(ext(6, 48_000, 32, 24, 0x3F)),
            data(body.len() as u32, &body),
        ],
    );
    ok(
        cs,
        "rf64_ext24in32_51_data32",
        f,
        (48_000, 6, 24),
        32,
        Some(0x3F),
        v,
        true,
        vec![],
    );

    // ds64 带表：JUNK 的 32 位长度为 0xFFFFFFFF，实际长度 10 在表中；表之后另有 4 字节。
    let v = samples(404, 64, 2, 16);
    let body = encode(&v, 16, 16, None);
    let mut junk = chunk_head(b"JUNK", u32::MAX);
    junk.extend_from_slice(&[7u8; 10]);
    let f = rf64(
        b"RF64",
        ds64(
            AUTO,
            body.len() as u64,
            64,
            &[(*b"JUNK", 10)],
            &[1, 2, 3, 4],
        ),
        &[fmt_pcm(pcm(2, 44_100, 16)), junk, data(u32::MAX, &body)],
    );
    ok(
        cs,
        "rf64_table_junk",
        f,
        (44_100, 2, 16),
        16,
        None,
        v,
        false,
        vec![],
    );

    // 普通 RIFF 中的 ds64 块（BS.2088 的占位做法）按未知块跳过。
    let v = samples(405, 40, 2, 16);
    let f = super::riff(&[
        chunk(b"ds64", &[0u8; 28]),
        fmt_pcm(pcm(2, 44_100, 16)),
        chunk(b"data", &encode(&v, 16, 16, None)),
    ]);
    ok(
        cs,
        "rf64_ds64_in_riff_skipped",
        f,
        (44_100, 2, 16),
        16,
        None,
        v,
        true,
        vec![],
    );

    // ---- W1 规则在 RF64 上 ----
    // R1 + R2：ffmpeg 输出到管道的结构，ds64 三个值都是 0，data 为 0xFFFFFFFF。
    let v = samples(406, 200, 2, 16);
    let body = encode(&v, 16, 16, None);
    let f = rf64(
        b"RF64",
        ds64(0, 0, 0, &[], &[]),
        &[
            fmt_pcm(pcm(2, 44_100, 16)),
            list.clone(),
            data(u32::MAX, &body),
        ],
    );
    let r = vec![
        format!("RF64:placeholder:0:{}", f.len() - 8),
        format!("data:placeholder:0:{}", body.len()),
    ];
    ok(
        cs,
        "rf64_pipe_placeholder",
        f,
        (44_100, 2, 16),
        16,
        None,
        v,
        false,
        r,
    );

    // R3：ds64 的 RIFF 长度比文件长 100 字节，data 完整。
    let v = samples(407, 90, 1, 16);
    let body = encode(&v, 16, 16, None);
    let mut f = rf64(
        b"RF64",
        ds64(AUTO, body.len() as u64, 90, &[], &[]),
        &[fmt_pcm(pcm(1, 22_050, 16)), data(u32::MAX, &body)],
    );
    let actual = (f.len() - 8) as u64;
    f[20..28].copy_from_slice(&(actual + 100).to_le_bytes());
    let r = vec![format!("RF64:mismatch:{}:{actual}", actual + 100)];
    ok(
        cs,
        "rf64_riff_longer",
        f,
        (22_050, 1, 16),
        16,
        None,
        v,
        false,
        r,
    );

    // R4：ds64 的 data 长度不是整帧（多 3 字节）。
    let v = samples(408, 50, 2, 16);
    let mut body = encode(&v, 16, 16, None);
    body.extend_from_slice(&[9, 9, 9]);
    let f = rf64(
        b"RF64",
        ds64(AUTO, body.len() as u64, 50, &[], &[]),
        &[fmt_pcm(pcm(2, 44_100, 16)), data(u32::MAX, &body)],
    );
    ok(
        cs,
        "rf64_partial_tail",
        f,
        (44_100, 2, 16),
        16,
        None,
        v,
        false,
        vec!["partial:3".into()],
    );

    // R5：ds64 声明的 data 超过 4 GiB（5 GiB），源只有 1000 帧（录音中断）。64 位长度如实报告。
    let v = samples(409, 1000, 2, 16);
    let body = encode(&v, 16, 16, None);
    let declared: u64 = 5 << 30;
    let f = rf64(
        b"RF64",
        // RIFF 体：“WAVE” 4 + ds64 36 + fmt 24 + data 块头 8 + 声明的 data。
        ds64(declared + 72, declared, declared / 4, &[], &[]),
        &[fmt_pcm(pcm(2, 48_000, 16)), data(u32::MAX, &body)],
    );
    cs.push(Case {
        name: "rf64_data_5gib_short",
        data: f,
        result: "Truncated",
        fmt: Some((48_000, 2, 16)),
        ch: 2,
        v: 16,
        c: 16,
        mask: None,
        ffmpeg: false,
        repairs: Vec::new(),
        short: Some((declared, declared - body.len() as u64)),
        bitstream: None,
        detail: None,
        vals: v,
    });

    // ---- 拒绝 ----
    let some = encode(&samples(410, 8, 2, 16), 16, 16, None);
    let fmt = fmt_pcm(pcm(2, 44_100, 16));
    // 第一个块不是 ds64（原“RF64 不支持”的夹具改为此项）。
    let mut f = b"RF64\xff\xff\xff\xffWAVE".to_vec();
    f.extend_from_slice(&fmt);
    f.extend_from_slice(&chunk(b"data", &some));
    bad(
        cs,
        "rf64_no_ds64",
        f,
        "Malformed",
        "Missing { first: [102, 109, 116, 32] }",
    );
    // ds64 只有 20 字节。
    let mut f = b"BW64\xff\xff\xff\xffWAVE".to_vec();
    f.extend_from_slice(&chunk(b"ds64", &[0u8; 20]));
    f.extend_from_slice(&fmt);
    bad(
        cs,
        "bw64_ds64_short",
        f,
        "Malformed",
        "TooShort { size: 20 }",
    );
    // 表声明 3 项，ds64 只容得下 1 项。
    let mut ds = ds64(AUTO, 32, 8, &[(*b"JUNK", 4)], &[]);
    ds[8 + 24..8 + 28].copy_from_slice(&3u32.to_le_bytes());
    let f = rf64(b"RF64", ds, &[fmt.clone(), data(u32::MAX, &some)]);
    bad(
        cs,
        "rf64_table_overrun",
        f,
        "Malformed",
        "TableOverrun { entries: 3, size: 40 }",
    );
    // 第二个 ds64。
    let f = rf64(
        b"RF64",
        ds64(AUTO, 32, 8, &[], &[]),
        &[fmt.clone(), ds64(0, 0, 0, &[], &[]), data(u32::MAX, &some)],
    );
    bad(cs, "rf64_ds64_duplicate", f, "Malformed", "Duplicate");
    // ds64 的 RIFF 长度容纳不下 ds64 本身。
    let f = rf64(
        b"RF64",
        ds64(10, 32, 8, &[], &[]),
        &[fmt.clone(), data(u32::MAX, &some)],
    );
    bad(
        cs,
        "rf64_riff_too_small",
        f,
        "Malformed",
        "RiffTooSmall { riff: 10 }",
    );
    // data 的 32 位长度 32 与 ds64 的 64 不同。
    let mut two = some.clone();
    two.extend_from_slice(&some);
    let f = rf64(
        b"RF64",
        ds64(AUTO, 64, 16, &[], &[]),
        &[fmt.clone(), data(32, &two)],
    );
    bad(
        cs,
        "rf64_data_conflict",
        f,
        "Malformed",
        "DataConflict { chunk: 32, ds64: 64 }",
    );
    // LIST 的长度为 0xFFFFFFFF，表中没有它。
    let mut l = chunk_head(b"LIST", u32::MAX);
    l.extend_from_slice(b"INFO");
    let f = rf64(
        b"RF64",
        ds64(AUTO, 32, 8, &[], &[]),
        &[fmt.clone(), l, data(u32::MAX, &some)],
    );
    bad(
        cs,
        "rf64_no_table_entry",
        f,
        "Malformed",
        "NoTableEntry { chunk: [76, 73, 83, 84] }",
    );
    // 表有 9 项（超过解码器容量 8），第 9 项正是后面要查的块。
    let ids: Vec<([u8; 4], u64)> = (0..9u8)
        .map(|i| ([b'q', b'y', b'T', b'0' + i], 4))
        .collect();
    let mut q = chunk_head(b"qyT8", u32::MAX);
    q.extend_from_slice(&[0u8; 4]);
    let f = rf64(
        b"RF64",
        ds64(AUTO, 32, 8, &ids, &[]),
        &[fmt.clone(), q, data(u32::MAX, &some)],
    );
    bad(
        cs,
        "rf64_table_capacity",
        f,
        "CapacityExceeded",
        "TableCapacity { entries: 9, capacity: 8 }",
    );

    // ---- ffmpeg 写出的真实 RF64 ----
    let dir = std::env::temp_dir().join(format!("qy-gen-rf64-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let v = samples(411, 4410, 2, 16);
    let f = ffmpeg_rf64(&v, false, &dir.join("seekable.wav"))?;
    ok(
        cs,
        "rf64_ffmpeg_seekable",
        f,
        (44_100, 2, 16),
        16,
        None,
        v.clone(),
        true,
        vec![],
    );
    let f = ffmpeg_rf64(&v, true, &dir.join("unused.wav"))?;
    // 输出到管道：ds64 全零，data 为 0xFFFFFFFF；data 是文件的最后一块。
    let bytes = (v.len() * 2) as u64;
    let r = vec![
        format!("RF64:placeholder:0:{}", f.len() - 8),
        format!("data:placeholder:0:{bytes}"),
    ];
    ok(
        cs,
        "rf64_ffmpeg_pipe",
        f,
        (44_100, 2, 16),
        16,
        None,
        v,
        false,
        r,
    );
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
