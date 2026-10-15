# QyBit

QyBit Core：Rust 编写的 `no_std`、无 alloc 本地音频播放引擎，桌面播放器、CLI 与嵌入式播放器共用同一核心（定位与 MCU 缺口分析见 [docs/arch-core-mcu.md](docs/arch-core-mcu.md)）。纯本地、严格保真：不重采样、不降精度、不混音，设备不支持源格式时明确报错。

软件不设人为上限：采样率、声道、位深只受格式规范、设备能力与宿主资源限制，并在播放前说明原因。当前支持无损格式 WAV、FLAC、AIFF / AIFF-C（未压缩），后续无损格式路线见 [tasks/FORMATS.md](tasks/FORMATS.md)；有损格式以后以插件形式另行设计。

长期规则见 [AGENTS.md](AGENTS.md)，阶段任务卡见 [tasks/](tasks/)（接下来：M3 T3 实物验证 → [P1 按实测数据的性能优化](tasks/P1.md)；无损格式扩展见 [tasks/FORMATS.md](tasks/FORMATS.md)，下一项为 F3 浮点 PCM）。

## 当前状态

M0 T1–T6 已实现：自有推送式 WAV 解码（[docs/m0-t2-wav.md](docs/m0-t2-wav.md)），pack 与 Windows 纯格式描述（[docs/m0-t3-pack.md](docs/m0-t3-pack.md)），真实路径模拟（[docs/m0-t4-sim.md](docs/m0-t4-sim.md)，`Action`/`Event`/`Decoder` 自此冻结），WASAPI 独占宿主（[docs/m0-t5-wasapi.md](docs/m0-t5-wasapi.md)），CLI 与真机矩阵（[docs/m0-t6-cli.md](docs/m0-t6-cli.md)）。M1 T1–T3 已实现：自有 FLAC 解码（[docs/m1-t2-flac.md](docs/m1-t2-flac.md)），libFLAC 参考核对、按文件头识别 WAV/FLAC 并接入模拟与桌面播放（[docs/m1-t3-integration.md](docs/m1-t3-integration.md)）。M2 进行中：T1 平台无关播放器层（[docs/m2-t1-player.md](docs/m2-t1-player.md)）、T2 ALSA 格式映射与参数确认（[docs/m2-t2-alsa-format.md](docs/m2-t2-alsa-format.md)）、T3 ALSA 渲染后端与 Linux CLI（[docs/m2-t3-alsa-backend.md](docs/m2-t3-alsa-backend.md)）、T4 两层数字回环（[docs/m2-t4-loopback.md](docs/m2-t4-loopback.md)：A 层 file 插件在 CI 运行；B 层 snd-aloop 未运行，CI runner 内核不含该模块）已完成；T5 验证加强已完成（[docs/m2-t5-verification.md](docs/m2-t5-verification.md)）：Kani 有界证明 24 个（pack 单样本内核、CRC、WAV/FLAC/AIFF 文件头整数计算），5 个 crate 的 cargo-mutants（补测试后存活项逐个说明为等价或不可达，CI 每周运行），由 libFuzzer 直接生成模拟场景的 fuzz 目标（10 分钟记录，CI 冒烟），Miri 运行队列与播放器渲染测试（FFI 部分不适用）。M3 T1 核心下沉已完成（[docs/m3-t1-engine.md](docs/m3-t1-engine.md)）：`no_std` 播放引擎（预填、换格式排空、暂停、上一首/下一首、出错跳过、能力检查）由模拟、桌面与 MCU 宿主共用，静态帧队列（bbqueue），Desktop / Embedded 配置档与 RAM 预算公式，解码器按需链接，thumbv7em 演示程序在 QEMU 中按独立参考核对通过（指令集仿真，不是实机）。F1 多声道已完成软件部分（[docs/f1-multichannel.md](docs/f1-multichannel.md)）：WAV EXTENSIBLE 声道掩码与 FLAC 1–8 声道（含 `WAVEFORMATEXTENSIBLE_CHANNEL_MASK` 标签）按扬声器位置保真送到设备，WASAPI 声道掩码、ALSA 声道映射设置与读回，设备顺序不同时纯置换，布局无法确认时明确拒绝；Embedded 配置档仍为双声道；真实多声道设备未运行。M3 T2 通用嵌入式宿主的软件部分已完成（[docs/m3-t2-embedded-host.md](docs/m3-t2-embedded-host.md)）：`qy-host-embedded` 与开发板和存储介质无关（块设备上的 FAT 经 `embedded-sdmmc`，无文件系统的闪存经 `embedded-storage` 标准接口，其他文件系统由板级实现存储接口）、DMA 中断喂数、串口命令，预填按最坏读取耗时公式核对；宿主侧虚拟时间仿真（慢读、读错、拔卡）与 QEMU 中运行同一宿主，按独立参考逐字节核对；附 STM32F411 板级适配示例（只编译）。实物验收需要用户在任一满足最低要求的板上运行，尚未运行。R1 资源计划与可靠性验证已完成（[docs/r1-reliability.md](docs/r1-reliability.md)）：播放前统一给出带数值的能力不足原因与播放计划；MCU、桌面与模拟宿主记录缓冲水位；虚拟时间仿真上的模型随机命令与故障测试（500 种子进 CI）；损坏文件系统 fuzz 发现 `embedded-sdmmc` 的 panic，已在存储层按 FAT 规范独立核对拦下；四个确定性约束写入架构文档；桌面吞吐基准在开发机上测得全部规格达到实时（结论只对该机器成立）。W1 已完成（[docs/w1-wav-tolerance.md](docs/w1-wav-tolerance.md)）：8 位 PCM；真实世界 WAV 文件头（ffmpeg 管道输出的占位长度、RIFF 长度写错、不足一帧的尾部）在意图明确时可播放，只修正结构、不改样本，每条修正写入报告；有歧义的照旧拒绝。F2 已完成（[docs/f2-aiff.md](docs/f2-aiff.md)）：自有 AIFF / AIFF-C（未压缩）解码，按 AIFF 1.3 与 AIFF-C 原文只接受规范定义的内容，拒绝分为不合规、合规但尚不支持、规范之外的扩展三类；与 WAV 共用流式工作区，三个配置档都启用且 RAM 不变。O1 已完成软件部分（[docs/o1-mono-to-stereo.md](docs/o1-mono-to-stereo.md)）：明确开启的单声道复制到左右两路（`--mono-to-stereo`、嵌入式串口命令；样本值不变、不混音，报告标为“声道已改变”），WASAPI 端点物理扬声器配置只作提示。Windows WASAPI 独占已在一个音频端点上真机运行（格式矩阵 70 项、顺序换曲、WAV/FLAC 播放），记录见 [docs/w1-wav-tolerance.md](docs/w1-wav-tolerance.md) §6；ALSA 真机、多声道设备与 MCU 实物尚未运行，见下方手动步骤。GUI 见 [tasks/GUI.md](tasks/GUI.md)，在 M3 之后再做。

## 工程结构

| crate | 说明 |
| --- | --- |
| `qy-core` | sans-IO 播放核心与播放引擎（`engine`：调度、暂停、切歌、出错策略、能力检查），`no_std`、无 alloc |
| `qy-codec-wav` | 自有推送式 WAV 解码器，`no_std`、无 alloc |
| `qy-codec-flac` | 自有推送式 FLAC 解码器（见 [docs/m1-t2-flac.md](docs/m1-t2-flac.md)），`no_std`、无 alloc；`static-cell` 特性提供 MCU 静态缓冲，`test-metrics` 特性仅供测试统计 |
| `qy-codec-aiff` | 自有推送式 AIFF / AIFF-C（未压缩）解码器（见 [docs/f2-aiff.md](docs/f2-aiff.md)），`no_std`、无 alloc |
| `qy-codec-auto` | 按文件头（`RIFF`/`FORM`+`AIFF`/`AIFC`/`fLaC`，前置 ID3v2 标签先跳过）选择自有 WAV/AIFF/FLAC 解码器，静态分发，`no_std`、无 alloc；WAV 与 AIFF 共用一块流式工作区；`wav`/`flac`/`aiff` 特性按需链接；`profile` 提供 Desktop / Embedded Full / Embedded Lite 配置档与 RAM 预算 |
| `qy-pcm-format` | 与平台 SDK 无关的格式字段与声道布局、WAVEFORMATEX/EXTENSIBLE 纯描述、ALSA 格式映射/设备名/参数确认/声道映射与 pack（按扬声器位置纯置换），`no_std`；设备帧队列接口与预算公式；`ring` 特性提供桌面用的 `rtrb` 队列，`static-queue` 特性提供 MCU 用的静态 `bbqueue` 队列 |
| `qy-host-sim` | 模拟宿主：真实核心/解码/pack/队列，模拟文件、时间与设备；WAV 1000 种子与 FLAC 1200 种子场景、并发实例测试 |
| `qy-player` | 平台无关播放器层：宿主资源、播放驱动、整包（WASAPI）与短写（ALSA）两种渲染循环、协商与播放报告；`test-support` 特性提供假平台 API 与假后端 |
| `qy-host-wasapi` | WASAPI 独占宿主：协商记录（HRESULT）、格式矩阵，Windows 绑定仅在 Windows 目标编译；真机测试见 `tests/device.rs`（默认忽略） |
| `qy-host-embedded` | 通用嵌入式宿主（M3 T2），`no_std`、无 alloc：曲目存储接口（介质不限）及两种通用实现（块设备上的 FAT、无文件系统的曲目表 + `ReadStorage`）、I²S/DMA 输出接口与板级能力声明、DMA 中断喂数器、主循环宿主、串口命令、预填与读卡时序公式 |
| `qy-host-alsa` | Linux ALSA 严格输出宿主：只接受 hw 设备，参数设置后读回核对，短写渲染；alsa-lib 绑定仅在 Linux 目标编译，其余逻辑跨平台可测；真机测试见 `tests/device.rs`（默认忽略） |
| `qybit` | 命令行程序，二进制名 `qybit` |
| `qy-bench` | 桌面吞吐基准（R1 §6，开发工具，不发布）：解码 + pack 实时倍数、高规格端到端余量与读取延迟影响 |
| `qy-mcu-demo` | MCU 演示（thumbv7em，不在主工作区，单独构建）：运行 `qy-host-embedded`，闪存中的 FAT16 “SD 卡”镜像、软件模拟 DMA 与串口命令脚本，在 QEMU 中由 `qy-host-sim/tests/mcu_demo.rs` 核对 |
| `qy-board` | 板级适配层（MCU 目标，不在主工作区）：每块板一个模块，按 Cargo 特性选择，只做外设初始化与 `qy-host-embedded` 接口实现；现有 `stm32f411`（SDIO、I²S2 + DMA、USART2）。只验证能编译，未在实物上运行 |

## 依赖

| 依赖 | 版本 | 用途 |
| --- | --- | --- |
| `rtrb` | =0.4.0 | 成熟 SPSC 环形缓冲，存放已 pack 的设备帧字节 |
| `windows` | =0.62.2 | Windows SDK 绑定（COM、WASAPI），仅 Windows 目标 |
| `alsa` | =0.12.1 | alsa-lib 安全封装，仅 Linux 目标（构建需要 libasound 开发包，如 `libasound2-dev`） |
| `libc` | =0.2.190 | Linux 渲染线程实时调度（`sched_setscheduler`），仅 Linux 目标 |
| `static_cell` | =2.1.1 | MCU 一次性静态零缓冲（`ConstStaticCell`），`qy-codec-flac` 的 `static-cell` 特性与 `qy-mcu-demo` |
| `bbqueue` | =0.7.0 | 成熟静态 SPSC BipBuffer（关闭默认特性，只用 CAS 原子协调），MCU 设备帧队列，仅 `qy-pcm-format` 的 `static-queue` 特性 |
| `cortex-m-rt` / `cortex-m` / `cortex-m-semihosting` | =0.7.5 / =0.7.7 / =0.5.0 | Cortex-M 运行时、MSP/WFI、临界区与半主机输出，仅 `qy-mcu-demo` 与板级示例（板级示例不用半主机） |
| `embedded-sdmmc` | =0.10.0 | 块设备（SD 卡、eMMC、U 盘、闪存镜像等）上的 FAT16、FAT32 读取（no_std，无 alloc），`qy-host-embedded` |
| `embedded-storage` | =0.3.2 | 按偏移读取的存储设备标准接口（no_std，无依赖），`qy-host-embedded` 的无文件系统曲目存储 |
| `stm32f4xx-hal` / `stm32f4` | =0.23.0 / =0.16.0 | STM32F4 HAL 与 PAC（打开 `rt`），仅 `qy-board` 的 `stm32f411` 特性 |

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
cargo build --target thumbv7em-none-eabihf -p qy-host-embedded
(cd crates/qy-board && cargo build --release --target thumbv7em-none-eabihf --features stm32f411)   # 板级适配，按板选特性，只编译
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

测试夹具由 `cargo run -p qy-gen-fixtures -- [wav|flac|aiff|all]` 生成：WAV 需要 PATH 中有 ffmpeg 作为第二参考，FLAC 需要 Xiph `flac`/`metaflac` ≥ 1.4.0（编码、显式格式参考解码、实际格式与 `--analyze` 分支核对，清单含 SHA-256 与种子）。fuzz 步骤见 [docs/m0-t2-wav.md](docs/m0-t2-wav.md)（`wav_decode`）、[docs/m1-t3-integration.md](docs/m1-t3-integration.md)（`flac_decode`）、[docs/f2-aiff.md](docs/f2-aiff.md)（`aiff_decode`）、[docs/w1-wav-tolerance.md](docs/w1-wav-tolerance.md)（`auto_decode`）与 [docs/r1-reliability.md](docs/r1-reliability.md)（`fat_storage`，损坏文件系统）。桌面吞吐基准：`cargo run --release -p qy-bench -- decode` 与 `-- e2e`（release 构建，结论只对运行它的机器成立，记录见 [docs/r1-reliability.md](docs/r1-reliability.md) §6）。

| fuzz 依赖 | 版本 | 用途 |
| --- | --- | --- |
| `libfuzzer-sys` | =0.4.10 | libFuzzer 绑定，仅 `fuzz/`（独立 workspace，nightly） |
| `sha2` | =0.10.9 | 夹具 SHA-256 摘要，仅 `tools/qy-gen-fixtures` |
| `fatfs` | =0.3.6 | 生成测试与 MCU 演示用的 FAT16 镜像，仅 `tools/qy-fat-image`（与被测读取路径独立） |

## 使用（Windows）

```
qybit list-devices                          # 列出端点 ID 与名称
qybit --device '<端点ID>' a.wav b.flac      # WASAPI 独占顺序播放（按文件头识别 WAV/FLAC）
qybit matrix --device '<端点ID>' --out report # 设备格式矩阵，写出 JSON/CSV
```

诊断（任何平台）：`qybit decode <输入> <输出.wav>` 用自有解码器与 pack 写成标准 WAV，可用任何播放器试听，用来区分“解码出错”与“设备输出出错”；`qybit tone <输出.wav> --rate 44100 --bits 16` 写一个 1 kHz 正弦测试音。文件开头的 ID3v2 标签（常见于被标签软件写过的 FLAC）先按标签长度跳过再识别容器，播放与 `decode` 都会写明跳过的字节数（[docs/w1-wav-tolerance.md](docs/w1-wav-tolerance.md) §6）。

端点 ID 形如 `{0.0.0.00000000}.{…}`。在 PowerShell 中必须加单引号，否则 `{…}` 会被当成脚本块，设备 ID 传不进程序；cmd 中不需要引号。

播放时实时显示每首曲目、源格式、每次独占请求的格式与 HRESULT、选定的设备格式、结构修正与跳过；按 Ctrl+C 立即停止（不排空）并输出完整报告（MMCSS 结果、有效帧、尾部填充与欠载），再按一次直接结束进程。驱动对某种格式回报支持、实际播放却是杂音时，可用 `--container 16|24|32` 只请求该容器位宽的格式排查（例如 24 位源用 `--container 32` 避开 3 字节紧凑格式），样本值不变。只接受立体声的设备播放单声道文件时，可用 `--mono-to-stereo` 明确开启单声道复制：`[FC]` 的每个样本原样写入左右两路，不混音、不改增益，报告标为“声道已改变”，不再算严格路径。打开设备后报告端点的物理扬声器配置（WASAPI，驱动填写，只作提示）。设备不支持源格式或独占打开失败时直接报错，不回退共享模式、不转换音频。输出不宣称设备端 bit-perfect。

**音量**：独占模式绕过 Windows 混音器，软件增益恒为 1（不做数字音量，不改样本）。所以 Windows 音量滑块和音量合成器通常不起作用，只有设备本身实现了硬件音量时，滑块才可能还有效。请用显示器、音箱、功放或 DAC 自己的音量控制，播放前先调低：独占输出是满幅的源信号。

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
set QYBIT_DEVICE_ID=<端点ID>                (cmd)
$env:QYBIT_DEVICE_ID='<端点ID>'             (PowerShell)
set QYBIT_REPORT_DIR=<报告目录>   (可选，默认 target/qybit-report；PowerShell 为 $env:QYBIT_REPORT_DIR='<目录>')
cargo test -p qy-host-wasapi --test device -- --ignored --test-threads=1 --nocapture
```

- `exclusive_playback_48k_16bit`：48 kHz/16 位双声道 1 秒独占播放。
- `exclusive_playback_flac_44k_16bit`：自有 FLAC 解码，libFLAC 生成的 44.1 kHz/16 位双声道夹具（20000 帧）独占播放。
- `format_matrix_and_sequential`：44.1–192 kHz × 16/16、24/24、32/24、32/32 × 单/双声道（EXTENSIBLE），16/16 另测 WAVEFORMATEX，再加 48 kHz 多声道 3.0/4.0/5.1/5.1 侧/7.1 × 16/16、32/24（F1），共 70 项；再用两个不同采样率顺序换曲。生成 `qybit-device-matrix.json` / `.csv`。

未设置 `QYBIT_DEVICE_ID` 或设备不存在时测试明确失败，不跳过。接口成功与播放统计只证明兼容与运行，不证明 DAC 输入样本逐位一致。
