//! 基准生成器的 FLAC 编码与 libFLAC 交叉核对：Xiph `flac` 命令行（≥ 1.4）解码出的样本必须与
//! 生成器的源样本逐个相等。未安装 `flac` 时跳过并明确说明（不算通过）。

use std::process::Command;

use qy_bench::{Codec, Spec};
use qy_gen_fixtures::bench::Predictor;

#[test]
fn libflac_decodes_bench_files() {
    let Ok(v) = Command::new("flac").arg("--version").output() else {
        eprintln!("未运行：未安装 Xiph flac 命令行");
        return;
    };
    eprintln!(
        "libFLAC 核对：{}",
        String::from_utf8_lossy(&v.stdout).trim()
    );
    let dir = std::env::temp_dir().join(format!("qy-bench-libflac-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
    let mut n = 0;
    for (rate, ch, bits) in [
        (44_100, 2, 16),
        (96_000, 8, 24),
        (384_000, 2, 32),
        (192_000, 8, 32),
    ] {
        for block in [4096, 16384] {
            for predictor in [Predictor::Fixed(2), Predictor::Lpc(32)] {
                let spec = Spec {
                    codec: Codec::Flac { block, predictor },
                    rate,
                    channels: ch,
                    bits,
                };
                // 0.2 秒加零头，末帧短于块长。
                let (data, samples) = spec.file(0.2 + 123.0 / f64::from(rate));
                let f = dir.join(format!("t{n}.flac"));
                let raw = dir.join(format!("t{n}.raw"));
                n += 1;
                std::fs::write(&f, &data).unwrap_or_else(|e| panic!("{e}"));
                let out = Command::new("flac")
                    .args([
                        "-d",
                        "-s",
                        "-f",
                        "--force-raw-format",
                        "--endian=little",
                        "--sign=signed",
                        "-o",
                    ])
                    .arg(&raw)
                    .arg(&f)
                    .output()
                    .unwrap_or_else(|e| panic!("{e}"));
                assert!(
                    out.status.success(),
                    "{}：libFLAC 解码失败：{}",
                    spec.label(),
                    String::from_utf8_lossy(&out.stderr)
                );
                let bytes = std::fs::read(&raw).unwrap_or_else(|e| panic!("{e}"));
                let w = bits as usize / 8;
                let got: Vec<i32> = bytes
                    .chunks_exact(w)
                    .map(|c| {
                        let mut b = [0u8; 4];
                        b[4 - w..].copy_from_slice(c);
                        i32::from_le_bytes(b) >> (32 - bits)
                    })
                    .collect();
                assert_eq!(got.len(), samples.len(), "{}", spec.label());
                assert!(got == samples, "{}：libFLAC 解码样本不符", spec.label());
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
