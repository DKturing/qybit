# QyFi

Rust 极简本地音频播放器。纯本地、严格保真：不重采样、不降精度、不混音，设备不支持源格式时明确报错。

长期规则见 [AGENTS.md](AGENTS.md)，阶段任务卡见 [tasks/](tasks/)。

## 当前状态

M0 T2 已完成：自有推送式 WAV 解码（见 [docs/m0-t2-wav.md](docs/m0-t2-wav.md)）。尚不能播放音频：pack 与 Windows 格式描述（T3）、模拟验证（T4）、WASAPI 独占播放（T5）、CLI 播放（T6）均未实现。

## 工程结构

| crate | 说明 |
| --- | --- |
| `qy-core` | sans-IO 播放核心，`no_std`、无 alloc |
| `qy-codec-wav` | 自有推送式 WAV 解码器，`no_std`、无 alloc |
| `qy-pcm-format` | 与平台 SDK 无关的格式字段与纯构造函数，`no_std`；`ring` 特性提供宿主用的设备帧 SPSC 队列 |
| `qy-host-sim` | 模拟宿主 |
| `qy-host-wasapi` | Windows WASAPI 宿主（Windows 绑定仅在 Windows 目标编译） |
| `qyfi` | 命令行程序，二进制名 `qyfi` |

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
