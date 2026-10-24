# QyBit

QyBit 是用 Rust 编写的本地音频播放器，用来把音频文件里的样本原样送到声卡。它在 Windows 上以 WASAPI 独占模式输出，在 Linux 上直接打开 ALSA 硬件设备，不经过系统混音器，不重采样，不改变位深，也不做任何音量缩放或音效处理。设备不能按原格式播放时，QyBit 会说明原因并停止，而不是悄悄转换后继续播放。

播放器由一个与操作系统无关的核心 QyBit Core 驱动。核心负责解析、解码、播放调度和输出数据的组织，不依赖标准库与堆内存，因此同一套代码既用于桌面上的命令行和图形界面，也可以运行在单片机上，从 SD 卡等存储经 I²S 播放。

## 播放方式

采样率、声道数和位深都按文件本身的格式请求设备。打开设备后，QyBit 会读回设备实际接受的参数并逐项核对，不一致就拒绝播放。播放结束后给出报告，写明源格式、设备格式、欠载和丢帧情况；只要出现欠载，这段播放就不会被记为完整。

软件本身不限制采样率、声道数、位深和 FLAC 块大小，限制只来自格式规范、设备能力和可用内存。资源不够时，播放前会给出具体的数值和原因。

WAV、FLAC、AIFF 解码器由 QyBit 自己实现，不调用 FFmpeg 或其他外部程序。整数样本从解码到输出一直以整数处理，到最后一步才按设备格式放置，中间不转成浮点。

## 支持的格式

- WAV：整数 PCM（含 8 位）、IEEE 浮点、WAVE_FORMAT_EXTENSIBLE（含声道掩码）、RF64 / BW64（超过 4 GiB 的文件）
- FLAC：1–8 声道，最高 32 位
- AIFF 与 AIFF-C 未压缩：`NONE`、`twos`、`sowt`、`fl32`、`fl64`

文件开头若有 ID3v2 标签，会先跳过再识别格式。常见的不规范 WAV 文件头（如流式写入程序留下的占位长度）在意图明确时可以正常播放，报告中会写明修正了什么；有歧义的文件仍会被拒绝。

ALAC、AAC、MP3、Opus、DSD 等格式可以通过扩展接入，见下文“扩展格式”。

## 运行环境

| 平台 | 输出方式 | 说明 |
| --- | --- | --- |
| Windows | WASAPI 独占 | x64 Windows 10 1607 或更新 |
| Linux | ALSA 硬件设备（`hw:`），直接使用内核接口 | 完全静态的程序，不依赖 alsa-lib 或任何系统库，可在任意发行版上运行。提供 x86_64、aarch64、armv7、loongarch64、riscv64 五个架构 |
| 单片机 | I²S / SAI + DMA | 带原子比较交换指令的 Cortex-M（M3 / M4 / M7 / M33），具体要求见下文 |

Linux 上普通用户需要属于 `audio` 组才能打开 `/dev/snd/` 下的设备。

## 获取

预编译程序由 GitHub Actions 构建，在 [Actions](https://github.com/qybit-lab/qybit/actions) 页面最近一次成功运行的构件中下载：

- `qybit-windows`：`qybit.exe`（命令行）和 `qybit-gui.exe`（图形界面），单个可执行文件，不需要安装运行库；
- `qybit-linux-<架构>`：`qybit` 和 `qybit-gui`，下载后 `chmod +x` 即可运行。

也可以从源码构建，见“从源码构建”。

## 命令行

```
qybit list-devices                          列出输出设备
qybit --device <设备> <文件>...              按顺序播放
qybit matrix --device <设备> --out <目录>    测试设备支持的格式，写出 JSON / CSV 报告
qybit mixer --device <设备>                  （Linux）查看声卡的硬件混音器，只读
qybit decode <输入> <输出.wav>               用 QyBit 的解码器把文件解码为标准 WAV
qybit tone <输出.wav> [--rate 44100] [--bits 16]   生成测试音
```

Windows 上的设备是端点 ID，形如 `{0.0.0.00000000}.{…}`。在 PowerShell 中要加单引号，否则花括号会被当成脚本块：

```
qybit list-devices
qybit --device '{0.0.0.00000000}.{…}' a.flac b.wav
```

Linux 上的设备写作 `hw:卡,设备` 或 `hw:CARD=名称,DEV=编号`：

```
qybit list-devices
qybit --device hw:CARD=DAC,DEV=0 a.flac b.wav
```

Linux 版只接受 `hw:` 设备，`plughw`、`default`、`dmix`、`pipewire`、`pulse` 这类会转换或混音的设备会被拒绝。设备正被 PipeWire 或 PulseAudio 占用时，QyBit 会报告占用它的进程。

播放过程中会实时显示每首曲目的源格式、向设备请求的格式和设备的回应。按一次 Ctrl+C 立即停止并输出完整报告，再按一次直接结束进程。

退出码：`0` 完整播放；`2` 播放完成但有欠载；`1` 失败（设备不支持、打开失败、设备出错或被中断）。

### 音量

独占输出绕过了系统混音器，QyBit 本身不做数字音量，送出的是满幅的源信号。因此 Windows 系统的音量滑块在独占播放时通常不起作用。请先把音箱、耳放或 DAC 的音量调低，再开始播放。

Linux 版在播放期间会把声卡自己的硬件音量设为 40%，并打开它的静音开关，播放结束或按 Ctrl+C 后恢复原来的设置。这只调整声卡的硬件控制，送出的数据不变。可以用 `--volume 60%` 指定音量，`--volume off` 不做任何调整，`--volume-control Headphone` 指定要调的控制。声卡没有硬件音量时照常播放，不会用软件缩放代替。没有桌面环境的系统上，主板声卡开机后常处于静音状态，`qybit mixer` 可以查看各个控制的当前值。

### 可选的格式调整

下面两个选项会改变送出的数据，默认关闭，开启后报告中会明确标出：

- `--mono-to-stereo`：设备只接受立体声时，把单声道的每个样本原样复制到左右两路，不混音，不改增益；
- `--float-to-int`：设备不接受浮点格式时，把浮点样本转为设备支持的整数位数，不加抖动，超出范围的样本限幅，报告中给出不精确和限幅的样本数。

### 排查问题

`qybit decode` 用 QyBit 自己的解码器把文件写成标准 WAV，可以用任何播放器试听，用来判断问题出在解码还是设备输出。`qybit matrix` 逐一尝试常见的采样率、位深和声道组合，记录设备接受哪些。Windows 上驱动声称支持某种格式、实际播放却是杂音时，可以用 `--container 32` 等参数只请求某一种容器位宽来排查。

## 图形界面

`qybit-gui` 在 Windows 上使用系统原生控件，在 Linux 上自行绘制界面（X11，Wayland 桌面通过 XWayland 运行，也可以经 SSH 的 X11 转发使用）。两个平台的菜单、选项和操作相同。

- 播放列表：添加文件或文件夹，从文件管理器拖入；点击列标题排序，拖动行调整顺序；
- 播放、暂停、停止、上一首、下一首，拖动进度条或用 ← / → 定位；
- 输出设备下拉框，设备插拔后列表自动更新；
- 状态栏显示播放状态、样本是否保持原样，以及采样率指示灯：灰色为未播放，琥珀色为正在打开设备，绿色表示设备以源采样率打开，红色表示设备不接受；
- 音量滑块调节的是设备自身的硬件音量，设备没有硬件音量时不可用。Linux 版与命令行相同，只在播放期间调整，结束后恢复；
- 键盘媒体键，可选在窗口不在前台时也响应；
- 日志区记录每次打开设备的协商过程，可以复制；
- 可选的“记住播放列表与设置”，默认关闭。设置保存在 `%APPDATA%\QyBit\qybit-gui.cfg`（Windows）或 `~/.config/qybit/qybit-gui.cfg`（Linux）。

Linux 版的文件面板路径栏支持 fcitx5、ibus 等输入法（通过 XIM，需设置 `XMODIFIERS`）。界面控件通过 AT-SPI 提供给 Orca 等读屏软件，读屏软件运行时自动启用。Linux 版图形界面同样是完全静态的程序。

## 关于“保真”的边界

QyBit 能保证的是软件这一侧：从文件到交给驱动的数据，样本值、采样率和声道顺序都不变。驱动和设备内部如何处理这些数据，不在软件的控制范围内。设备报告接受了某种格式，也不等于 DAC 收到的数据与文件逐位一致。需要确认这一点时，可以用数字回录设备采集输出，与文件对比。

## 单片机

`qy-host-embedded` 是一个不绑定具体开发板的嵌入式宿主，与桌面版共用同一个核心和解码器。它从板载存储读取曲目（块设备上的 FAT16 / FAT32，或者没有文件系统、按偏移读取的闪存），经 I²S 和 DMA 输出，通过串口接受 `ls`、`play`、`pause`、`resume`、`stop`、`next`、`prev`、`seek`、`status` 等命令。移植到一块新板只需实现存储和音频输出两个接口。

最低要求：带原子比较交换指令的 Cortex-M（`thumbv7m`、`thumbv7em` 或 `thumbv8m.main`），有 I²S 或 SAI 与 DMA；内存视配置而定：只播放 WAV 与 AIFF 的精简配置约 90 KiB，加上 FLAC 的完整配置约 360 KiB。仓库中的 `crates/qy-board` 是 STM32F411 的板级适配示例。缓冲大小和读卡时序按公式计算，详见 [docs/m3-t2-embedded-host.md](docs/m3-t2-embedded-host.md)。

## 扩展格式

新的格式以 Rust crate 的形式在编译时接入，不需要修改本仓库：实现解码器接口，再把它和内置格式组合起来，就能得到支持新格式的同一个命令行、图形界面和嵌入式播放器。扩展可以声明解码结果为有损，报告和界面会相应标出；也可以通过 DoP 或原生 DSD 把 1 位 DSD 数据送到支持的设备。

`qy-testkit` 提供契约测试，检查解码器在任意输入分块、复位重用和定位下的行为是否符合要求。`examples/qy-ext-demo` 是一个扩展示例，包含解码器、组合方式和命令行、图形界面、嵌入式三种组装。接口说明见 [docs/x1-extensions.md](docs/x1-extensions.md)，版本与兼容规则见 [docs/e4-versioning.md](docs/e4-versioning.md)，各版本的变化见 [CHANGELOG.md](CHANGELOG.md)。扩展按版本 tag 以 git 依赖引用本仓库：

```toml
[dependencies]
qy-core = { git = "https://github.com/qybit-lab/qybit", tag = "v0.1.0" }
qy-player = { git = "https://github.com/qybit-lab/qybit", tag = "v0.1.0" }
```

## 从源码构建

需要 Rust 1.88 或更新。

```
cargo build --release -p qybit -p qybit-gui
```

Linux 静态程序：

```
rustup target add x86_64-unknown-linux-musl
cargo build --release -p qybit -p qybit-gui --target x86_64-unknown-linux-musl
```

其他架构把目标换成 `aarch64-unknown-linux-musl`、`armv7-unknown-linux-musleabihf`、`loongarch64-unknown-linux-musl` 或 `riscv64gc-unknown-linux-musl`。链接使用工具链自带的 `rust-lld`，不需要交叉编译的 C 工具链。不需要读屏支持时，可以加 `--no-default-features` 构建 `qybit-gui`，程序约小 5.6 MB。

运行测试：

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

依赖清单、嵌入式目标的构建、测试工具和需要真实设备的测试见 [docs/development.md](docs/development.md)。

## 代码结构

| crate | 内容 |
| --- | --- |
| `qy-core` | 播放核心与调度引擎，`no_std`，不使用堆内存 |
| `qy-codec-wav`、`qy-codec-flac`、`qy-codec-aiff` | 解码器，`no_std`，不使用堆内存 |
| `qy-codec-auto` | 按文件头选择解码器；桌面与嵌入式的资源配置 |
| `qy-pcm-format` | 格式描述、按设备格式放置样本（pack）、声道映射、帧队列 |
| `qy-player` | 桌面播放器层：渲染循环、格式协商、播放报告 |
| `qy-host-wasapi`、`qy-host-alsa` | Windows 与 Linux 的输出后端 |
| `qy-host-embedded` | 嵌入式宿主 |
| `qy-host-sim` | 模拟宿主：用真实的核心与解码器，只模拟文件、时间和设备，用于测试 |
| `qy-ui` | 与平台无关的界面模型 |
| `qybit`、`qybit-gui` | 命令行与图形界面 |
| `qy-testkit` | 解码器契约测试 |

设计说明在 [docs/](docs/) 目录，其中 [docs/arch-core-mcu.md](docs/arch-core-mcu.md) 介绍核心的整体结构。

## 许可

MIT 或 Apache-2.0，任选其一。
