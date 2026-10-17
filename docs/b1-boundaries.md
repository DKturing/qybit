# B1 边界整理：完成记录

任务卡：[tasks/B1.md](../tasks/B1.md)。提交：`4842bfe`（B1-A）、`fdf8f64`（B1-B）、`4d6911f`（B1-C），以及本文所在的 B1-D 提交。

**结论**：
- 核心不再认识任何格式；
- 两项可选策略（O1 单声道复制、F3 浮点转整数）的状态都收进一个内部结构，引擎只经少数调用点使用它；
- 不含 FLAC 的桌面配置可以组装、测试，并按“不支持”拒绝 FLAC 文件且说明原因；
- 行为不变：全部现有测试照常通过，报告文字与 R1 起相同。

## 1. 核心只表达资源（B1-A）

- `qy_core::engine::Unit { codec, name }`：解码器所说的“单元”在报告中的叫法，由解码器给出。
  - FLAC 提供 `qy_codec_flac::BLOCK`（“FLAC”“块”）与 `FRAME`（“FLAC”“帧”）。
  - 核心只有通用叫法 `Unit::DECODE`（“解码单元”）与 `Unit::INPUT`（“输入单元”）。
- `CapabilityError`：
  - `FlacBlock` / `FlacFrame` 改为 `DecodeUnit { frames, max, unit }` / `InputUnit { bytes, window, unit }`；
  - FLAC 曲目的文字照旧是“FLAC 最大块 65535 帧超出本机配置的 4608 帧”“FLAC 声明的最大帧 … 字节超出本机输入窗口 … 字节”（`qy-codec-flac/tests/boundaries.rs` 核对原文）。
- `DecoderDemand`：
  - 字段改为 `max_unit_frames`、`unit_frames_capacity`、`max_unit_bytes`（0 表示未声明）、`window`，另附 `decode_unit`、`input_unit`；
  - `PlaybackPlan` 的文字由单元叫法生成，FLAC 曲目仍写“FLAC 最大块 …/… 帧，最大帧 …/… 字节”。
- 嵌入式串口的播放计划事件字段改为 `decode_unit=` / `input_unit=`。
- `grep -i flac crates/qy-core/src` 只剩注释中的举例（8 处），没有类型、字段或报告文字。

## 2. 可选策略隔离（B1-B）

- `qy_core::engine::options`（模块私有，`pub(super)`）中的 `Options` 持有两项选项的全部状态：
  - 策略；
  - 转换工作区；
  - 本曲的请求阶段；
  - 设备拒绝浮点的记录（只在一次会话内有效）；
  - 首次入队通知；
  - NaN / 无穷计数与转换统计。
- 公开类型 `ChannelPolicy`、`FloatPolicy`、`FloatReport` 由 `engine` 重新导出；`Engine` 的公开方法（`set_channel_policy`、`set_float_policy`、`set_convert_workspace` 等）不变。
- 引擎只经以下调用点使用它，新增第三项选项时按这些点接入：

| 阶段 | 调用点 | 作用 |
| --- | --- | --- |
| 会话 | `new_session` | 开始 / 停止时清除只在会话内有效的记录 |
| 呈现 | `begin`、`presentation`、`reuse_converted` | 源格式 → 请求格式（O1 的 `[FC] → [FL FR]`、F3 的整数阶段）；同格式续用时是否仍需转换 |
| 失败回退 | `check_failed`、`fallback` | 能力检查失败或设备拒绝时，重试下一档或拒绝（没有转换工作区时按资源不足） |
| 配置 | `configured`、`pack_mode` | 设备确认后记下实际格式；pack 是否复制声道 |
| 入队 | `push` | 是否经转换；NaN / 无穷时给出停止位置 |
| 报告 | `first_output`、`add_stats`、`take_float_report` | 每首第一次入队时的“已改变”通知；关闭时的统计 |

## 3. 最小配置可组装（B1-C）

- **特性**：
  - 工作区依赖 `qy-codec-auto` 改为 `default-features = false`，每个成员声明自己链接的格式；
  - `qy-player` 增加默认开启的 `flac` 特性（透传 `qy-codec-auto/flac`）。
- **组装**：
  - `HostResources::with_profile` 按配置档启用、且本构建已链接的部分组装，不再要求 FLAC，`HostError::Profile` 已删除。组装的部分包括：WAV 与 AIFF 共用的流式工作区、FLAC 容量、浮点转换工作区（`convert_samples` 为 0 时不提供）与队列大小（`caps.queue_bytes`）；
  - `HostResources::new()` 在启用 `flac` 时用 `DESKTOP`，否则用新增的 `DESKTOP_WAV`（能力与队列相同，没有 FLAC 工作区）。
- **拒绝原因**：
  - `AutoDecoder::disabled()` 在文件是可识别的容器、但本构建未链接或宿主未启用其解码器时给出格式名；
  - 播放器报告写“本构建未启用 FLAC 解码器，按不支持拒绝”。

五种组合：

| 组合 | 怎样验证 |
| --- | --- |
| 1. WAV-only（`qy-player --no-default-features`） | CI 新增 Clippy 与测试：34 个测试，含新增的 `wav_only_build_rejects_flac_with_reason`。只依赖 FLAC 夹具的 3 个测试按特性跳过。`cargo tree` 确认不链接 `qy-codec-flac` |
| 2. WAV + FLAC（默认） | 原有全部测试（36 个，含组合 3 的新测试） |
| 3. 不含浮点转换工作区 | 新增 `profile_without_convert_workspace`（两种特性下都运行）：整数曲目照常播放，开启转换后浮点曲目按“没有转换工作区”拒绝，不打开设备 |
| 4. 无 GUI（CLI 与库） | `qy-player`、`qy-ui`、`qy-host-alsa` 测试与 Windows 目标的 `qybit` Clippy；GUI 与它们各自独立构建 |
| 5. MCU Embedded Full / Lite | QEMU 演示两种配置按独立参考核对通过；thumbv7em 构建与 Clippy |

## 4. 尺寸（thumbv7em，QEMU 演示 release）

B1 之前为 `59de1c7`，之后为本提交，均用 `llvm-size -A` 测量。

| 配置 | `.text` 之前 → 之后 | `.rodata` | `.data` + `.bss`（RAM） |
| --- | --- | --- | --- |
| Embedded Full | 119 204 → 121 360（+2 156，+1.8%） | −972 | 399 160 → 399 160（不变） |
| Embedded Lite | 102 132 → 104 696（+2 564，+2.5%） | −876 | 91 724 → 91 724（不变） |

- `.rodata` 中约 2.2 MB 是内嵌的 FAT 演示镜像，比较时只看差值。
- 增加的代码主要来自三处：
  - 能力不足原因的 `Debug` 多了单元字段（+764）；
  - `Engine::request_device` 内联了选项的调用点（+1 030）；
  - 演示程序格式化报告文字（+268）。
- 第一次测量时 `.text` 增加 6.1 KB：`Unit` 派生的 `Debug` 把 `str` 的转义格式化与 Unicode 表链接进来，约 3.7 KB。现改为手写 `Debug`（按原文写出两个名称）。
- 配置档的 RAM 预算公式没有改变；新增的 `DESKTOP_WAV` 为 3 178 496 字节（`DESKTOP` 为 11 558 810 字节）。

## 5. 与任务卡的差异

1. **没有新增 `Resources::codec()`**：跳过曲目时，宿主格式化错误之前，引擎可能已为下一首复位了解码器，这时再问解码器“你是什么格式”会得到错误的答案。所以单元的叫法随错误与需求一起给出（`Unit` 字段）。
2. **单元以 `&'static Unit` 携带**：按值携带会使 `FlacDecoder` 从 448 字节增至 472 字节，超出解码器结构体的大小上限（`qy-codec-flac/tests/buffers.rs`，S1 起为 448 字节，用来证明没有内嵌块缓冲）。
3. **新增 `DESKTOP_WAV` 配置档与 `AutoDecoder::disabled()`**：属于组装层与报告，用于组合 1 和“说明原因”的验收。

## 6. 验证

本地（Linux 开发机）运行并通过：
- `cargo fmt --check`；
- CI 中全部 Clippy 命令，加上 WAV-only 组合；
- `cargo test`：qy-core、三个 codec、qy-codec-auto、qy-host-sim、qy-player（两种特性）、qy-ui、qy-host-alsa（含 A 层 file 插件回环）、qy-host-embedded、qy-fat-image；
- 模型随机测试 500 个种子（release）；
- qy-bench 与 qy-gen-fixtures；
- thumbv7em 构建与 Clippy；
- `x86_64-pc-windows-gnu` 目标的 `qy-host-wasapi` / `qybit` / `qybit-gui` Clippy；
- QEMU 演示（Full / Lite）；
- fuzz 目标构建（nightly-2026-10-12）。

Windows 上的测试由 CI 运行。本卡不涉及设备端行为，真实设备没有重新运行。

## 7. 未做

- 可回收的缓冲所有权：B1 §7 的范围外，限制已写入 [arch-core-mcu.md](arch-core-mcu.md) §6。
- 运行时插件与格式注册表：范围外；编译期扩展的示例与契约测试在 X1。
