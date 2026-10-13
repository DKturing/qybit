# QyBit

Rust 极简本地音频播放器。纯本地、严格保真：不重采样、不降精度、不混音，设备不支持源格式时明确报错。

长期规则见 [AGENTS.md](AGENTS.md)，阶段任务卡见 [tasks/](tasks/)。

## 当前状态

M0 T5 已完成：自有推送式 WAV 解码（[docs/m0-t2-wav.md](docs/m0-t2-wav.md)），pack 与 Windows 纯格式描述（[docs/m0-t3-pack.md](docs/m0-t3-pack.md)），真实路径模拟（[docs/m0-t4-sim.md](docs/m0-t4-sim.md)，`Action`/`Event`/`Decoder` 自此冻结），WASAPI 独占宿主（[docs/m0-t5-wasapi.md](docs/m0-t5-wasapi.md)，真机测试尚未运行）。CLI 播放与真机格式矩阵（T6）未实现。

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

WAV 测试夹具由 `python3 tools/gen_wav_fixtures.py` 生成（需要 ffmpeg），fuzz 步骤见 [docs/m0-t2-wav.md](docs/m0-t2-wav.md)。

| fuzz 依赖 | 版本 | 用途 |
| --- | --- | --- |
| `libfuzzer-sys` | =0.4.10 | libFuzzer 绑定，仅 `fuzz/`（独立 workspace，nightly） |
