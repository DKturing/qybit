# QyBit MAC：macOS 版（CoreAudio 独占、命令行、图形界面）

遵循根目录 AGENTS.md。来源：AGENTS §1“macOS 另列任务，不提前扩展范围”；用户 2026-10-24 决定先**只写任务卡**，不实现。

状态：**任务卡草案，待用户审定（§7），未开始实现**。审定前不新增 crate、不改 CI。

## 1. 范围

- 与 Windows、Linux 版相同的 Core（含播放调度）、自有解码器、pack 与报告；macOS 只新增宿主后端与界面前端，不另写播放器逻辑（AGENTS §1）。
- 命令行 `qybit`：`play`、`list-devices`、`info`、`decode`、`mixer`（只读）等子命令与 Linux 版一致；`--device` 按 CoreAudio 设备 UID 选择。
- 图形界面 `qybit-gui`：菜单、选项、播放列表、日志、状态栏、指示灯、记住设置与 Windows / Linux 版一致（共用 `src/app.rs`）。
- 目标：Apple Silicon（`aarch64-apple-darwin`）与 Intel（`x86_64-apple-darwin`），发布为通用二进制（`lipo`）。macOS 程序不能完全静态链接：只依赖系统框架（CoreAudio、CoreFoundation、AppKit 等），不依赖任何第三方动态库（`otool -L` 核对）。
- 不做：共享模式、系统混音器路径、AirPlay、聚合设备的创建、App Store 发布。

## 2. 输出后端 `qy-host-coreaudio`

依据：Apple *Core Audio* 文档与 `CoreAudio/AudioHardware.h`、`AudioHardwareBase.h` 头文件（实现时逐项记录属性名与出处）。绑定用维护中的 `objc2-core-audio` / `objc2-core-audio-types`（纯 Rust 绑定，不在构建时运行 bindgen；版本锁定，审定后核对许可与版本）。`unsafe` 集中在一个平台边界模块，逐处写明前提，单独审计（AGENTS §2）。

**独占与保真**（对应 WASAPI 独占、ALSA `hw:`）：

1. 独占：设置设备的 `kAudioDevicePropertyHogMode` 为本进程。已被其他进程独占时明确报错并写明占用进程的 pid，不退回共享。
2. 格式：在输出流的可用物理格式（`kAudioStreamPropertyAvailablePhysicalFormats`）中按源格式选择采样率、声道与样本表示（整数优先，容器位宽 C、有效位数 V、字节序），设置 `kAudioStreamPropertyPhysicalFormat`，并把虚拟格式设为同一格式且带 `kAudioFormatFlagIsNonMixable`（不经系统混音器，IOProc 收到的就是物理格式的整数数据）。设备不支持时按协商规则逐个尝试并报告每次被拒的原因；都不支持时明确报错，不转换。
3. 设置后**读回**物理格式、虚拟格式与标称采样率（`kAudioDevicePropertyNominalSampleRate`）核对；读回与请求不一致时拒绝播放并报告。状态区分请求格式、协商结果与读回参数（AGENTS §3）。
4. 播放结束（停止、出错、换设备、退出）时恢复播放前的物理格式、标称采样率并释放独占。
5. 输出：IOProc（实时线程）只从 SPSC 帧队列有界搬运整帧与补静音、计数，不解码、不 pack、不分配、不阻塞（AGENTS §2）。缓冲大小用 `kAudioDevicePropertyBufferFrameSize`，欠载与过载（`kAudioDeviceProcessorOverload`）计入报告。
6. 多声道：按设备的首选声道布局（`kAudioDevicePropertyPreferredChannelLayout`）映射扬声器位置，无法确认时拒绝（F1 规则）。
7. DSD：DoP 以 24 位整数物理格式打开（D1）；CoreAudio 没有原生 DSD 格式。
8. 设备枚举与变化：`kAudioHardwarePropertyDevices` 及其监听（插拔时更新列表，与 Windows 前端相同的“未连接”标注）。

**设备音量**：见 §7 第 3 项（待审定）。

## 3. 命令行

- 发布产物 `qybit`（通用二进制）。`list-devices` 列出输出设备的名称、UID、可用物理格式；`mixer` 只读列出设备音量控件。
- 报告格式与 Windows / Linux 版相同；后端特有的协商记录（尝试过的物理格式、读回值、独占状态）写入报告。

## 4. 图形界面

方案见 §7 第 2 项（待审定）。两种方案共同的要求：

- 文件选择用系统的 `NSOpenPanel`（macOS 自带，不需要像 Linux 那样自绘）；从 Finder 拖放文件；菜单放在系统菜单栏。
- 媒体键与“正在播放”：`MPRemoteCommandCenter` / `MPNowPlayingInfoCenter`（MediaPlayer 框架）。
- 读屏（VoiceOver）与输入法：原生控件方案由系统提供；自绘方案复用 LG2 的无障碍树（`accesskit_macos`），输入法需实现 `NSTextInputClient`（只用于路径输入，若采用 `NSOpenPanel` 则可不需要文字输入）。
- 设置保存到 `~/Library/Application Support/QyBit/qybit-gui.cfg`，格式同其他平台。
- 打包为 `QyBit.app`（`Info.plist`、图标）。

## 5. 验证

| 层 | 内容 |
| --- | --- |
| 构建 | GitHub macOS runner：两个目标的 fmt、Clippy、测试；`lipo` 合成通用二进制；`otool -L` 核对只依赖系统框架；大小记录 |
| 模拟 | 现有模拟宿主、契约测试、场景在 macOS 上运行（与平台无关，应原样通过） |
| 后端单元测试 | 协商、读回核对、恢复逻辑在 trait 之上用假设备测试（同 `qy-host-wasapi` 的 `fake`）；属性读写的错误映射 |
| 虚拟回环 | CI 安装虚拟音频驱动（如 BlackHole，只在 CI 中作测试工具，不随程序发布、不链接），播放到其输出并从其输入回录，与曲目逐字节比对（同步序列对齐）。虚拟回环不证明真实 DAC（AGENTS §3） |
| 界面 | 模拟设备模式下的截图与操作核对（与 Windows / Linux 的 `sim` 特性相同） |
| 真机 | 用户在 Mac 上：内置扬声器 / 耳机、USB DAC 各一台；独占时其他程序无声、结束后恢复；格式读回；欠载计数为 0；**未运行前一律标为未运行** |

## 6. 顺序

T1 后端与命令行（含假设备测试、虚拟回环）→ T2 用户真机验证命令行 → T3 界面方案原型与测量（按 §7 第 2 项的选择）→ T4 界面 → T5 打包与签名（按 §7 第 5 项）。每步完成验收后再进行下一步（AGENTS §4）。

## 7. 待用户审定

1. **最低系统版本**：建议 macOS 11（Big Sur，首个支持 Apple Silicon 的版本），两种架构合成通用二进制。
2. **界面方案**：
   - 甲：AppKit 原生控件（`objc2-app-kit`，与 Windows 版用 Win32 原生控件相同的思路）。读屏、输入法、外观由系统提供；需要另写一套视图代码。
   - 乙：复用 Linux 版的自绘视图（`view.rs`、tiny-skia、cosmic-text），只把 X11 窗口层换成 Cocoa 窗口。三个平台外观一致、视图代码共用；读屏复用 LG2 的无障碍树（`accesskit_macos`）；外观不是 macOS 原生风格。
   - 建议乙（与“界面尽量一致”的既定要求相符，视图与无障碍树已有测试），审定前先做 T3 原型比较大小、空闲 CPU 与 Retina 缩放。
3. **设备音量**：CoreAudio 的音量属性（`kAudioDevicePropertyVolumeScalar` / `kAudioHardwareServiceDeviceProperty_VirtualMainVolume`）由驱动提供，系统接口不说明是硬件实现还是驱动内的软件缩放（Windows 有 `QueryHardwareSupport`，ALSA 有混音器元素可查）。AGENTS §3 不允许以软件缩放代替硬件音量。建议：默认不调整设备音量，滑块只在用户明确开启后调用该属性，并在报告与界面中写明“由驱动实现，未确认是否为硬件音量”；或完全不提供音量调节。请选择。
4. **只接受浮点物理格式的设备**：部分设备只提供 32 位浮点物理格式。把 N ≤ 24 位的整数样本写成 32 位浮点是数值上精确的表示转换（每个整数值都可精确表示，可逆）。建议：作为 pack 的一种设备格式支持并在报告中写明“设备格式为浮点（整数值精确表示）”，N > 24 时拒绝；或一律拒绝。请选择（涉及保真规则的解释，按 AGENTS §4 需用户同意）。
5. **签名与公证**：
   - 不签名：用户首次运行需在“隐私与安全性”中允许（或去掉隔离属性），CI 即可产出；
   - Developer ID 签名并公证：需要 Apple 开发者账号（付费）与 CI 中的证书机密。
   请选择；选后者时由用户提供账号与证书。
6. **结束后恢复格式与采样率**：建议恢复（与 A4“播放之外不改动设备”一致）。

## 8. 完成标准

§5 各层按实际运行结果分别报告；真机、虚拟回环、模拟不得互相替代。§7 各项审定结果写入本卡后开始 T1。
