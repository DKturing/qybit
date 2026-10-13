# QyBit

Rust 极简本地音频播放器。纯本地、严格保真：不重采样、不降精度、不混音，设备不支持源格式时明确报错。

长期规则见 [AGENTS.md](AGENTS.md)，阶段任务卡见 [tasks/](tasks/)。

## 当前状态

M0 T1–T6 已实现：自有推送式 WAV 解码（[docs/m0-t2-wav.md](docs/m0-t2-wav.md)），pack 与 Windows 纯格式描述（[docs/m0-t3-pack.md](docs/m0-t3-pack.md)），真实路径模拟（[docs/m0-t4-sim.md](docs/m0-t4-sim.md)，`Action`/`Event`/`Decoder` 自此冻结），WASAPI 独占宿主（[docs/m0-t5-wasapi.md](docs/m0-t5-wasapi.md)），CLI 与真机矩阵（[docs/m0-t6-cli.md](docs/m0-t6-cli.md)）。**真机测试尚未运行**，见下方手动步骤。GUI 见 [tasks/GUI.md](tasks/GUI.md)，在 M0–M2 完成后再做。

## 工程结构

| crate | 说明 |
| --- | --- |
| `qy-core` | sans-IO 播放核心，`no_std`、无 alloc |
| `qy-codec-wav` | 自有推送式 WAV 解码器，`no_std`、无 alloc |
| `qy-pcm-format` | 与平台 SDK 无关的格式字段、WAVEFORMATEX/EXTENSIBLE 纯描述与 pack，`no_std`；`ring` 特性提供宿主用的设备帧 SPSC 队列 |
| `qy-host-sim` | 模拟宿主：真实核心/解码/pack/队列，模拟文件、时间与设备；1000 种子场景测试 |
| `qy-host-wasapi` | WASAPI 独占宿主：跨平台渲染循环与播放驱动（可注入假 API 测试），Windows 绑定仅在 Windows 目标编译；真机测试见 `tests/device.rs`（默认忽略） |
| `qybit` | 命令行程序，二进制名 `qybit` |

## 依赖

| 依赖 | 版本 | 用途 |
| --- | --- | --- |
| `rtrb` | =0.4.0 | 成熟 SPSC 环形缓冲，存放已 pack 的设备帧字节 |
| `windows` | =0.62.2 | Windows SDK 绑定（COM、WASAPI），仅 Windows 目标 |

## 本地检查

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --target thumbv7em-none-eabihf -p qy-core -p qy-codec-wav -p qy-pcm-format
```

WAV 测试夹具由 `cargo run -p qy-gen-fixtures` 生成（需要 PATH 中有 ffmpeg 作为第二参考），fuzz 步骤见 [docs/m0-t2-wav.md](docs/m0-t2-wav.md)。

| fuzz 依赖 | 版本 | 用途 |
| --- | --- | --- |
| `libfuzzer-sys` | =0.4.10 | libFuzzer 绑定，仅 `fuzz/`（独立 workspace，nightly） |

## 使用（Windows）

```
qybit list-devices                          # 列出端点 ID 与名称
qybit --device <端点ID> a.wav b.wav         # WASAPI 独占顺序播放
qybit matrix --device <端点ID> --out report # 设备格式矩阵，写出 JSON/CSV
```

播放时显示源格式、每次独占请求的格式与 HRESULT、MMCSS 结果、有效帧、尾部填充与欠载。设备不支持源格式或独占打开失败时直接报错，不回退共享模式、不转换音频。输出不宣称设备端 bit-perfect。

## 真机测试（手动，需要 Windows 与音频设备）

```
qybit list-devices
set QYBIT_DEVICE_ID=<端点ID>
set QYBIT_REPORT_DIR=<报告目录>   (可选，默认 target/qybit-report)
cargo test -p qy-host-wasapi --test device -- --ignored --test-threads=1 --nocapture
```

- `exclusive_playback_48k_16bit`：48 kHz/16 位双声道 1 秒独占播放。
- `format_matrix_and_sequential`：44.1–192 kHz × 16/16、24/24、32/24、32/32 × 单/双声道（EXTENSIBLE），16/16 另测 WAVEFORMATEX，共 60 项；再用两个不同采样率顺序换曲。生成 `qybit-device-matrix.json` / `.csv`。

未设置 `QYBIT_DEVICE_ID` 或设备不存在时测试明确失败，不跳过。接口成功与播放统计只证明兼容与运行，不证明 DAC 输入样本逐位一致。
