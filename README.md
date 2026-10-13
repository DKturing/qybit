# QyBit

QyBit Core：Rust 编写的 `no_std`、无 alloc 本地音频播放引擎，桌面播放器、CLI 与嵌入式播放器共用同一核心（定位与 MCU 缺口分析见 [docs/arch-core-mcu.md](docs/arch-core-mcu.md)）。纯本地、严格保真：不重采样、不降精度、不混音，设备不支持源格式时明确报错。

软件不设人为上限：采样率、声道、位深只受格式规范、设备能力与宿主资源限制，并在播放前说明原因。当前支持无损格式 WAV、FLAC，后续无损格式路线见 [tasks/FORMATS.md](tasks/FORMATS.md)；有损格式以后以插件形式另行设计。

长期规则见 [AGENTS.md](AGENTS.md)，阶段任务卡见 [tasks/](tasks/)。

## 当前状态

M0 T1–T6 已实现：自有推送式 WAV 解码（[docs/m0-t2-wav.md](docs/m0-t2-wav.md)），pack 与 Windows 纯格式描述（[docs/m0-t3-pack.md](docs/m0-t3-pack.md)），真实路径模拟（[docs/m0-t4-sim.md](docs/m0-t4-sim.md)，`Action`/`Event`/`Decoder` 自此冻结），WASAPI 独占宿主（[docs/m0-t5-wasapi.md](docs/m0-t5-wasapi.md)），CLI 与真机矩阵（[docs/m0-t6-cli.md](docs/m0-t6-cli.md)）。M1 T1–T3 已实现：自有 FLAC 解码（[docs/m1-t2-flac.md](docs/m1-t2-flac.md)），libFLAC 参考核对、按文件头识别 WAV/FLAC 并接入模拟与桌面播放（[docs/m1-t3-integration.md](docs/m1-t3-integration.md)）。M2 进行中：T1 平台无关播放器层（[docs/m2-t1-player.md](docs/m2-t1-player.md)）、T2 ALSA 格式映射与参数确认（[docs/m2-t2-alsa-format.md](docs/m2-t2-alsa-format.md)）、T3 ALSA 渲染后端与 Linux CLI（[docs/m2-t3-alsa-backend.md](docs/m2-t3-alsa-backend.md)）、T4 两层数字回环（[docs/m2-t4-loopback.md](docs/m2-t4-loopback.md)：A 层 file 插件在 CI 运行；B 层 snd-aloop 未运行，CI runner 内核不含该模块）已完成；验证加强见 [tasks/M2.md](tasks/M2.md)。M3 T1 核心下沉已完成（[docs/m3-t1-engine.md](docs/m3-t1-engine.md)）：`no_std` 播放引擎（预填、换格式排空、暂停、上一首/下一首、出错跳过、能力检查）由模拟、桌面与 MCU 宿主共用，静态帧队列（bbqueue），Desktop / Embedded 配置档与 RAM 预算公式，解码器按需链接，thumbv7em 演示程序在 QEMU 中按独立参考核对通过（指令集仿真，不是实机）。M3 T2/T3 需要实物开发板，未运行。**真机测试尚未运行**，见下方手动步骤。GUI 见 [tasks/GUI.md](tasks/GUI.md)，在 M3 之后再做。

## 工程结构

| crate | 说明 |
| --- | --- |
| `qy-core` | sans-IO 播放核心与播放引擎（`engine`：调度、暂停、切歌、出错策略、能力检查），`no_std`、无 alloc |
| `qy-codec-wav` | 自有推送式 WAV 解码器，`no_std`、无 alloc |
| `qy-codec-flac` | 自有推送式 FLAC 解码器（见 [docs/m1-t2-flac.md](docs/m1-t2-flac.md)），`no_std`、无 alloc；`static-cell` 特性提供 MCU 静态缓冲，`test-metrics` 特性仅供测试统计 |
| `qy-codec-auto` | 按文件头（`RIFF`/`fLaC`）选择自有 WAV/FLAC 解码器，静态分发，`no_std`、无 alloc；`wav`/`flac` 特性按需链接；`profile` 提供 Desktop / Embedded Full / Embedded Lite 配置档与 RAM 预算 |
| `qy-pcm-format` | 与平台 SDK 无关的格式字段、WAVEFORMATEX/EXTENSIBLE 纯描述、ALSA 格式映射/设备名/参数确认与 pack，`no_std`；设备帧队列接口与预算公式；`ring` 特性提供桌面用的 `rtrb` 队列，`static-queue` 特性提供 MCU 用的静态 `bbqueue` 队列 |
| `qy-host-sim` | 模拟宿主：真实核心/解码/pack/队列，模拟文件、时间与设备；WAV 1000 种子与 FLAC 1200 种子场景、并发实例测试 |
| `qy-player` | 平台无关播放器层：宿主资源、播放驱动、整包（WASAPI）与短写（ALSA）两种渲染循环、协商与播放报告；`test-support` 特性提供假平台 API 与假后端 |
| `qy-host-wasapi` | WASAPI 独占宿主：协商记录（HRESULT）、格式矩阵，Windows 绑定仅在 Windows 目标编译；真机测试见 `tests/device.rs`（默认忽略） |
| `qy-host-alsa` | Linux ALSA 严格输出宿主：只接受 hw 设备，参数设置后读回核对，短写渲染；alsa-lib 绑定仅在 Linux 目标编译，其余逻辑跨平台可测；真机测试见 `tests/device.rs`（默认忽略） |
| `qybit` | 命令行程序，二进制名 `qybit` |
| `qy-mcu-demo` | MCU 资源演示（thumbv7em，不在主工作区，单独构建）：模拟读卡与 DMA 的纯软件宿主，在 QEMU 中由 `qy-host-sim/tests/mcu_demo.rs` 核对 |

## 依赖

| 依赖 | 版本 | 用途 |
| --- | --- | --- |
| `rtrb` | =0.4.0 | 成熟 SPSC 环形缓冲，存放已 pack 的设备帧字节 |
| `windows` | =0.62.2 | Windows SDK 绑定（COM、WASAPI），仅 Windows 目标 |
| `alsa` | =0.12.1 | alsa-lib 安全封装，仅 Linux 目标（构建需要 libasound 开发包，如 `libasound2-dev`） |
| `libc` | =0.2.190 | Linux 渲染线程实时调度（`sched_setscheduler`），仅 Linux 目标 |
| `static_cell` | =2.1.1 | MCU 一次性静态零缓冲（`ConstStaticCell`），`qy-codec-flac` 的 `static-cell` 特性与 `qy-mcu-demo` |
| `bbqueue` | =0.7.0 | 成熟静态 SPSC BipBuffer（关闭默认特性，只用 CAS 原子协调），MCU 设备帧队列，仅 `qy-pcm-format` 的 `static-queue` 特性 |
| `cortex-m-rt` / `cortex-m` / `cortex-m-semihosting` | =0.7.5 / =0.7.7 / =0.5.0 | Cortex-M 运行时、MSP/WFI 与半主机输出，仅 `qy-mcu-demo` |

## 本地检查

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --target thumbv7em-none-eabihf -p qy-core -p qy-codec-wav -p qy-pcm-format -p qy-codec-flac -p qy-codec-auto
cargo build --target thumbv7em-none-eabihf -p qy-codec-flac --features static-cell
cargo test -p qy-codec-flac --features test-metrics   # 含整帧重试复杂度测试
cargo test -p qy-pcm-format --features ring,static-queue
cargo build --target thumbv7em-none-eabihf -p qy-pcm-format --features static-queue
cargo build --target thumbv7em-none-eabihf -p qy-codec-auto --no-default-features --features wav
```

MCU 演示（需要 `qemu-system-arm` ≥ 8，`mps2-an386`）：

```sh
cd crates/qy-mcu-demo
cargo build --release --target thumbv7em-none-eabihf --features hold
cp target/thumbv7em-none-eabihf/release/qy-mcu-demo target/qy-mcu-demo-full.elf
cargo build --release --target thumbv7em-none-eabihf --no-default-features --features hold
cp target/thumbv7em-none-eabihf/release/qy-mcu-demo target/qy-mcu-demo-lite.elf
cd ../..
QY_MCU_DEMO_ELF=crates/qy-mcu-demo/target/qy-mcu-demo-full.elf \
QY_MCU_DEMO_LITE_ELF=crates/qy-mcu-demo/target/qy-mcu-demo-lite.elf \
  cargo test -p qy-host-sim --test mcu_demo -- --nocapture
```

测试夹具由 `cargo run -p qy-gen-fixtures -- [wav|flac|all]` 生成：WAV 需要 PATH 中有 ffmpeg 作为第二参考，FLAC 需要 Xiph `flac`/`metaflac` ≥ 1.4.0（编码、显式格式参考解码、实际格式与 `--analyze` 分支核对，清单含 SHA-256 与种子）。fuzz 步骤见 [docs/m0-t2-wav.md](docs/m0-t2-wav.md)（`wav_decode`）与 [docs/m1-t3-integration.md](docs/m1-t3-integration.md)（`flac_decode`）。

| fuzz 依赖 | 版本 | 用途 |
| --- | --- | --- |
| `libfuzzer-sys` | =0.4.10 | libFuzzer 绑定，仅 `fuzz/`（独立 workspace，nightly） |
| `sha2` | =0.10.9 | 夹具 SHA-256 摘要，仅 `tools/qy-gen-fixtures` |

## 使用（Windows）

```
qybit list-devices                          # 列出端点 ID 与名称
qybit --device <端点ID> a.wav b.flac        # WASAPI 独占顺序播放（按文件头识别 WAV/FLAC）
qybit matrix --device <端点ID> --out report # 设备格式矩阵，写出 JSON/CSV
```

播放时显示源格式、每次独占请求的格式与 HRESULT、MMCSS 结果、有效帧、尾部填充与欠载。设备不支持源格式或独占打开失败时直接报错，不回退共享模式、不转换音频。输出不宣称设备端 bit-perfect。

## 使用（Linux）

```
qybit list-devices                              # 列出 hw:CARD=…,DEV=… 设备
qybit --device hw:CARD=DAC,DEV=0 a.flac b.wav   # ALSA hw 顺序播放（按文件头识别 WAV/FLAC）
qybit matrix --device hw:CARD=DAC,DEV=0 --out report
```

只接受 `hw:` 设备，拒绝 `plughw`/`default`/`dmix`/`pipewire`/`pulse` 等会转换或混音的设备。设备被 PipeWire/PulseAudio 占用（EBUSY）时报告占用进程，不改用共享路径。参数设置后读回核对（格式、精确采样率、声道、周期、缓冲、重采样关闭、有效位），不一致即报错。

## 真机测试（Linux ALSA，手动，需要音频设备）

```
QYBIT_ALSA_DEVICE=hw:CARD=DAC,DEV=0 cargo test -p qy-host-alsa --test device -- --ignored --test-threads=1 --nocapture
```

包括 48 kHz/16 位播放、FLAC 播放与 48 项格式矩阵（报告写入 `QYBIT_REPORT_DIR`，默认 `target/qybit-report`）。只证明参数读回一致与播放运行，不证明 DAC 输入样本逐位一致。

数字回环（M2 T4）：A 层 `cargo test -p qy-host-alsa --test loopback_file`（alsa-lib file 插件，任何 Linux 可跑）；B 层 `scripts/aloop-loopback.sh`（需要 snd-aloop 内核模块）。报告写入 `target/qybit-report/qybit-alsa-loopback-*.json`。均为虚拟回环，不证明真实 DAC。

## 真机测试（手动，需要 Windows 与音频设备）

```
qybit list-devices
set QYBIT_DEVICE_ID=<端点ID>
set QYBIT_REPORT_DIR=<报告目录>   (可选，默认 target/qybit-report)
cargo test -p qy-host-wasapi --test device -- --ignored --test-threads=1 --nocapture
```

- `exclusive_playback_48k_16bit`：48 kHz/16 位双声道 1 秒独占播放。
- `exclusive_playback_flac_44k_16bit`：自有 FLAC 解码，libFLAC 生成的 44.1 kHz/16 位双声道夹具（20000 帧）独占播放。
- `format_matrix_and_sequential`：44.1–192 kHz × 16/16、24/24、32/24、32/32 × 单/双声道（EXTENSIBLE），16/16 另测 WAVEFORMATEX，共 60 项；再用两个不同采样率顺序换曲。生成 `qybit-device-matrix.json` / `.csv`。

未设置 `QYBIT_DEVICE_ID` 或设备不存在时测试明确失败，不跳过。接口成功与播放统计只证明兼容与运行，不证明 DAC 输入样本逐位一致。
