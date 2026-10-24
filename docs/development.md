# 开发说明

面向修改 QyBit 源码的开发者：依赖、本地检查、测试工具与需要真实设备的测试。使用说明见 [README.md](../README.md)，长期规则见 [AGENTS.md](../AGENTS.md)。

## 依赖

所有依赖锁定版本。

| 依赖 | 版本 | 用途 |
| --- | --- | --- |
| `rtrb` | =0.4.0 | 成熟 SPSC 环形缓冲，存放已 pack 的设备帧字节 |
| `windows` | =0.62.2 | Windows SDK 绑定（COM、WASAPI），仅 Windows 目标 |
| `alsa` | =0.12.1 | alsa-lib 安全封装，仅 `qy-host-alsa` 的 `alsa-lib` 可选特性（只用于 CI 的 A 层回环测试；构建需要 libasound 开发包，如 `libasound2-dev`）；发布版不含 |
| `libc` | =0.2.190 | Linux 系统调用（内核 ALSA 接口的 `ioctl`、`poll`、`fcntl`）与渲染线程实时调度（`pthread_setschedparam`），仅 Linux 目标 |
| `static_cell` | =2.1.1 | MCU 一次性静态零缓冲（`ConstStaticCell`），`qy-codec-flac` 的 `static-cell` 特性与 `qy-mcu-demo` |
| `bbqueue` | =0.7.0 | 成熟静态 SPSC BipBuffer（关闭默认特性，只用 CAS 原子协调），MCU 设备帧队列，仅 `qy-pcm-format` 的 `static-queue` 特性 |
| `cortex-m-rt` / `cortex-m` / `cortex-m-semihosting` | =0.7.5 / =0.7.7 / =0.5.0 | Cortex-M 运行时、MSP/WFI、临界区与半主机输出，仅 `qy-mcu-demo` 与板级示例（板级示例不用半主机） |
| `embedded-sdmmc` | =0.10.0 | 块设备（SD 卡、eMMC、U 盘、闪存镜像等）上的 FAT16、FAT32 读取（no_std，无 alloc），`qy-host-embedded` |
| `xim` | =0.5.0 | XIM 输入法客户端（纯 Rust，经 x11rb），仅 Linux 图形界面的路径栏输入；测试工具 `xim-commit` 另用其 `x11rb-server` 特性 |
| `accesskit` / `accesskit_unix` | =0.25.1 / =0.24.0 | 无障碍树与 AT-SPI 适配（经 zbus 连接 D-Bus，不链接系统库），仅 Linux 图形界面；`accesskit_unix` 由 `qybit-gui` 的 `a11y` 特性（默认开启）带入 |
| `x11rb` | =0.13.2 | X11 协议的纯 Rust 实现（不链接 libX11 / libxcb，不启用 `allow-unsafe-code`），仅 Linux 图形界面 |
| `tiny-skia` | =0.11.4 | 软件绘制的抗锯齿形状，仅 Linux 图形界面 |
| `cosmic-text` | =0.15.0 | 文字排版、字体回退与字形光栅化（直接读系统字体与 fontconfig 配置文件，不链接 libfontconfig），仅 Linux 图形界面 |
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

测试夹具由 `cargo run -p qy-gen-fixtures -- [wav|flac|aiff|all]` 生成：WAV 需要 PATH 中有 ffmpeg 作为第二参考，FLAC 需要 Xiph `flac`/`metaflac` ≥ 1.4.0（编码、显式格式参考解码、实际格式与 `--analyze` 分支核对，清单含 SHA-256 与种子）。fuzz 步骤见 [docs/m0-t2-wav.md](m0-t2-wav.md)（`wav_decode`）、[docs/m1-t3-integration.md](m1-t3-integration.md)（`flac_decode`）、[docs/f2-aiff.md](f2-aiff.md)（`aiff_decode`）、[docs/w1-wav-tolerance.md](w1-wav-tolerance.md)（`auto_decode`）与 [docs/r1-reliability.md](r1-reliability.md)（`fat_storage`，损坏文件系统）。桌面吞吐基准：`cargo run --release -p qy-bench -- decode` 与 `-- e2e`（release 构建，结论只对运行它的机器成立，记录见 [docs/r1-reliability.md](r1-reliability.md) §6）。

| fuzz 依赖 | 版本 | 用途 |
| --- | --- | --- |
| `libfuzzer-sys` | =0.4.10 | libFuzzer 绑定，仅 `fuzz/`（独立 workspace，nightly） |
| `sha2` | =0.10.9 | 夹具 SHA-256 摘要，仅 `tools/qy-gen-fixtures` |
| `fatfs` | =0.3.6 | 生成测试与 MCU 演示用的 FAT16 镜像，仅 `tools/qy-fat-image`（与被测读取路径独立） |

## 真机测试（Linux ALSA，手动，需要音频设备）

```
QYBIT_ALSA_DEVICE=hw:CARD=DAC,DEV=0 cargo test -p qy-host-alsa --test device -- --ignored --test-threads=1 --nocapture
```

包括 48 kHz/16 位播放、FLAC 播放与 48 项格式矩阵（报告写入 `QYBIT_REPORT_DIR`，默认 `target/qybit-report`）。只证明参数读回一致与播放运行，不证明 DAC 输入样本逐位一致。

数字回环（M2 T4）：A 层 `cargo test -p qy-host-alsa --features alsa-lib --test loopback_file`（alsa-lib file 插件，需要 libasound 开发包）；B 层在能加载 snd-aloop 的机器上运行 `scripts/aloop-loopback.sh`，不能加载内核模块时运行 `scripts/aloop-vm.sh`（QEMU 虚拟机，`QYBIT_VM_ARCH=x86_64|aarch64`，见 [docs/a1-kernel-alsa.md](a1-kernel-alsa.md) §8）。报告写入 `target/qybit-report/`。均为虚拟回环，不证明真实 DAC。用户机器上的测试步骤见 [docs/a1-kernel-alsa.md](a1-kernel-alsa.md) §9。

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

## 图形界面核对（Linux，Xvfb）

```
cargo build --release -p qybit-gui --features sim
cargo build --release --manifest-path tools/gui-proto-linux/Cargo.toml --bin xdnd-send
cargo build --release --manifest-path tools/gui-proto-linux/Cargo.toml --features xim-server --bin xim-commit
QY_A11Y_CHECK=1 scripts/gui-linux-check.sh target/release/qybit-gui \
  tools/gui-proto-linux/target/release/xdnd-send <输出目录> tools/gui-proto-linux/target/release/xim-commit
```

需要 Xvfb、xdotool、ImageMagick、中文字体；读屏核对（`QY_A11Y_CHECK=1`）另需 dbus、at-spi2-core、python3-gi 与 gir1.2-atspi-2.0。模拟设备模式走真实核心、解码与 pack，只模拟设备与时间，没有声音输出。
