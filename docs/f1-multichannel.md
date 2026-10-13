# F1：多声道记录

日期：2026-10-13。任务卡：[tasks/F1.md](../tasks/F1.md)。本卡的软件部分不需要硬件；真实多声道设备（WASAPI / ALSA 真机）与 B 层 snd-aloop **未运行**。

提交：`3dad5bb`（布局模型、pack 置换、ALSA 映射纯逻辑）、`574eade`（WAV/FLAC 多声道解码与夹具）、`0400096`（模拟场景与设备布局）、`9d80b71`（WASAPI 掩码、ALSA chmap、A 层多声道回环），`2076697`（MCU 演示与 fuzz 目标），以及本记录所在的提交（文档；修正 `qy-codec-auto` 只开 `wav` 特性时的未用导入警告）。

## 1. 规范依据

- **FLAC**（RFC 9639 §9.1.3 声道分配；§8.6 VORBIS_COMMENT）：
  - 0–7 号分配为 1–8 个独立声道。默认顺序都等于对应 WAVE 掩码按位从低到高的顺序：
    1 `0x4`、2 `0x3`、3 `0x7`、4 `0x33`、5 `0x607`、6 `0x60F`、7 `0x70F`、8 `0x63F`。
  - **5、6 声道取侧环绕**。RFC 的文字是 “surround left/right”，参考实现 libFLAC 1.4.3 的 `src/flac/decode.c` 默认掩码表取 `SL|SR`。已经用源码和实际解码输出（解码得到的 WAV 中的 `dwChannelMask`）两方面核对过。
    任务卡草案写的是后环绕 `0x37` / `0x3F`，这是错的，已在卡中更正。需要后环绕的文件由掩码标签声明（libFLAC 编码时总会写入该标签）。
  - `WAVEFORMATEXTENSIBLE_CHANNEL_MASK` 标签的处理：
    - 名称不区分大小写；
    - 值为 `0x`/`0X` 加 1–8 位十六进制数；
    - 置位数必须等于声道数，且不得含保留位；
    - 同一个标签出现多次且值不同时拒绝。
    - 标签声明了就以它为准，否则用默认顺序。
- **WAV**：
  - `WAVEFORMATEXTENSIBLE` 的 `dwChannelMask` 列出存在的扬声器位置，样本按掩码位从低到高排列。依据是 Microsoft 文档 `WAVEFORMATEXTENSIBLE` 与 `KSAUDIO_CHANNEL_CONFIG`。
  - 格式码 1 且多于 2 声道时，规范没有定义布局。
- **ALSA**（alsa-lib `include/pcm.h`、`asound.h` 中的 `SND_CHMAP_*`）：
  - 流程：`snd_pcm_query_chmaps` → 映射可变时 `snd_pcm_set_chmap` → `snd_pcm_get_chmap` 读回核对。
  - 位置值的对应表写在 `qy_pcm_format::alsa::chmap_position` 中，并逐项测试。
- **WASAPI**：设备格式用 `WAVEFORMATEXTENSIBLE`，`dwChannelMask` 等于源布局。只接受 WAVE 掩码顺序，非掩码顺序的布局在构造时拒绝（`FormatError::LayoutOrder`）。

## 2. 布局模型与 pack

- `Speaker`：WAVE 的 18 个 `SPEAKER_*` 位。
- `ChannelLayout`：按样本顺序列出每个声道的位置，`no_std`。
- `SourceFormat` / `DeviceFormat` 携带布局：
  - `::new` 只用于单/双声道，默认布局为 `FC` / `FL FR`；
  - 多于 2 声道必须用 `with_layout`，否则报 `FormatError::UndefinedLayout`。
  - 冻结的 `Action` / `Event` / `Decoder` 没有改动。
- `accepts` 按位置集合判断，`reorders()` 标出设备顺序与源顺序是否不同。
- pack 的样本放置规则不变。设备顺序不同但位置集合相同时，按扬声器位置纯置换，样本值和位宽不变（§6 已定）；位置集合不同时报 `FormatMismatch`。
- 测试：
  - 各布局在 16/24/32 位设备格式下，与独立公式算出的字节逐字节一致；
  - 置换另有独立参考；
  - 去掉置换的变异会被测试发现。

## 3. 解码器、缓冲与配置档

- **WAV**：
  - EXTENSIBLE 掩码给出布局。
  - 多于 2 声道且布局没有完整声明时，报 `Unsupported`，并给出明细 `WavDetail::UndefinedLayout { channels, mask }`。播放器报告中写作“WAV 错误明细”，例如“源文件未声明 6 声道的扬声器布局”。没声明布局的情况包括：格式码 1、掩码为 0、置位数少于声道数、声道数超过可表示的位置数。
  - 掩码置位数多于声道数时报 `Malformed`。
- **FLAC**：
  - 去掉了 2 声道拦截。
  - 流式解析 VORBIS_COMMENT，只取掩码标签，其余注释照旧跳过，所有长度字段都检查是否越界。
  - 元数据结束后才报告格式，因为格式解析完成前不得伪造格式。
  - 新增错误：
    - `BadChannelMask` → `Unsupported`；
    - `BadVorbisComment` → `Malformed`；
    - 声道数超出宿主容量 `TooManyChannels` → `CapacityExceeded`。
- **缓冲**：
  - PCM 工作区与 pack 暂存区仍按双声道大小，更多声道时每批帧数相应减少，结果不变。
  - `MAX_FRAME_BYTES` 按 18 声道 × C32 = 72 字节计算。
  - MCU 演示的 DMA 半缓冲按配置档的最多声道计算，`.bss` 不变。
- **配置档**：
  - **Desktop**：FLAC 用 `Capacity::DESKTOP_MULTICHANNEL`（块 65 535、8 声道、32 位，窗口 2 097 146 字节），`max_channels` 18。队列仍为 3 072 000 字节，更高的“采样率 × 声道”组合由引擎按队列不足拒绝。
  - **Embedded Full / Lite**：仍为双声道。6 声道 WAV 报“6 声道超出本机配置的上限 2 声道”；6 声道 FLAC 在 STREAMINFO 处即超出解码器的双声道容量，报 `CapacityExceeded`。两者都不打开设备。I²S TDM 多声道另列。

## 4. 设备端

- **WASAPI**：
  - 协商请求与源布局相同的掩码。
  - 格式矩阵新增 3.0 `0x7`、4.0 `0x33`、5.1 `0x3F`、5.1 侧 `0x60F`、7.1 `0x63F`，各取 48 kHz × (16/16, 32/24)，共 10 项；矩阵总数从 60 项增加到 70 项。
  - 假设备完整播放这 10 项。真机矩阵未运行。
- **ALSA**：
  - **映射计划**（`plan_chmap`）：
    - 有声道数相同、位置集合覆盖源的可变映射：设置为源布局；
    - 否则取位置集合与源相同的固定（或成对）映射，按设备顺序由 pack 置换；
    - 设备没有该声道数的映射：单/双声道按默认布局（报告“设备不报告”），多于 2 声道拒绝；
    - 有该声道数的映射，但位置集合都不同：拒绝。
    - 含反相或驱动私有位置的映射不会被选中。
  - **读回核对**：可变映射设置后、固定映射直接都要读回核对，不一致即放弃该候选。设备不支持读回时，多声道无法确认，同样放弃。
  - **报告**：尝试记录中写明映射类型、读回映射和结论，例如“声道映射：固定，读回 FL FR RL RR FC LFE，已按设备映射重排声道（纯置换，样本值不变）”。
  - **映射文本的解析**：`alsa` crate 0.12.1 把未知位置值转换为枚举时会 `unwrap`（会 panic）。真实实现因此不取枚举，改为解析 alsa-lib `snd_pcm_chmap_print` 的文本形式（`qy_host_alsa::chmap`，37 个名称），无法解析的映射不列出。这里不用 `unsafe`。
  - **假 PCM 测试**（errno 层）：可变、固定、无映射三种设备，以及读回不符和含反相标志，各自得到正确结论。

## 5. 验证结果

本地 Linux x86_64，2026-10-13。

| 项目 | 结果 |
| --- | --- |
| 夹具 | WAV 63 项（新增 3.0/4.0/5.1/5.1 侧/7.1 与 5 个未定义布局拒绝夹具），由 ffmpeg 第二参考核对；FLAC 59 项（flac 命令行 3–8 声道 × 16/24 位 × 44.1/48/96 kHz 中覆盖组合 7 项，手写无标签/小写标签/侧环绕双声道 4 项，掩码标签与注释错误 5 项），由 libFLAC 参考解码，布局按 libFLAC 解码输出的掩码独立核对；单/双声道夹具音频逐字节不变 |
| 解码 | 多声道夹具与参考逐样本一致，布局等于期望；任意分块 |
| 回归 | 同一夹具池（排除 F1 新夹具）下 WAV 1000 种子与 FLAC 1200 种子逐种子摘要与 F1 之前（`3dad5bb` 的前一提交）一致；含新夹具的完整池同样通过 |
| 模拟场景 | `qy-host-sim/tests/sim_multichannel.rs`：种子 1–4 × 两种设备模型（WASAPI 整包、短写），含零长度读、短读与慢读。覆盖：多声道与双声道混排（换布局先排空再重开）；HDA 5.1 固定顺序置换；位置集合不同被拒绝；未定义布局跳过；Embedded Full 下多声道按能力拒绝。期望字节由参考样本按独立公式计算 |
| A 层回环（alsa-lib `file` 插件） | 原有 128 项不变（88 项逐字节一致、40 项 N > V 拒绝）。新增 3 个多声道项逐字节一致：5.1 同序 S24_3LE、5.1 HDA 序 S16_LE、5.1 侧环绕 S32_LE。另有 1 项“设备不报告映射时 6 声道明确拒绝”。见下方说明 |
| MCU 演示（QEMU mps2-an386） | 见 §6 |
| fuzz | 见 §7 |
| 构建与检查 | `cargo fmt --check`；Clippy（Linux 全工作区、Windows 目标交叉检查、thumbv7em 与 CI 中各特性组合）`-D warnings` 通过；Linux 全工作区测试及 `test-support`、`static-cell,test-metrics`、`ring,static-queue` 特性测试全部通过；thumbv7em no_std 构建通过。本地没有 wine，Windows 目标测试未在本地运行，由 CI 的 Windows 作业运行 |

**A 层回环说明**：`file` / `null` 插件不报告声道映射。多声道项由测试包装真实 PCM，声明一个**固定映射**（只替换 `query_chmaps` / `get_chmap`，其余调用原样交给 alsa-lib），报告中逐项写明“测试声明的固定声道映射”。期望字节在测试内按扬声器位置独立重排，因此回环证明的是置换后的字节路径正确。“设备真的按这个映射摆放声道”由第 4 节的假设备测试覆盖，回环不证明这一点。虚拟回环也不证明真实 DAC。

## 6. MCU 演示

“SD 卡”上新增 6 声道曲目，验证 Embedded 下多声道被明确拒绝。

| | Embedded Full | Embedded Lite |
| --- | --- | --- |
| 新增曲目 | `ext24in32_6ch_51.wav`（96 kHz）、`cli_16_6ch_48k_51_side.flac` | `ext16_6ch_51_side.wav`（48 kHz） |
| 结果 | `Capability(Channels { channels: 6, max: 2 })`、`Core(Decode(CapacityExceeded))`，跳过后继续播放 | `Capability(Channels { channels: 6, max: 2 })`，跳过后继续播放 |
| 设备段 | 与 M3 T1 相同，全部与独立参考一致 | 同左 |
| `.bss` | 365 736 字节（不变） | 91 068 字节（不变） |
| 栈高水位 | 3 072 字节（M3 T1：2 200） | 2 672 字节（M3 T1：1 880） |

栈增加的原因是格式结构携带了布局。用 x86_64 宿主测得的大小（`size_of`）：

| 类型 | F1 前 | F1 后 |
| --- | --- | --- |
| `SourceFormat` | 8 字节 | 28 字节 |
| `DeviceFormat` | 16 字节 | 36 字节 |
| `HostAction` | 32 字节 | 64 字节 |
| `HostEvent` | 24 字节 | 40 字节 |

这些类型按值传递。栈仍远小于栈区（Full 约 155 KiB）。QEMU 是指令集仿真，不是实机。

## 7. fuzz

目标仍是 `fuzz/fuzz_targets/wav_decode.rs` 与 `flac_decode.rs`。F1 做了三处改动：

- FLAC 改用桌面多声道容量（8 声道）；
- WAV 的一次性与分块结果同时比对拒绝明细 `WavDetail`；
- 两者都检查格式中的布局声道数等于声道数。

| 项 | 内容 |
| --- | --- |
| 工具链 | cargo 1.101.0-nightly (29c5daa1a 2026-10-07)，cargo-fuzz 0.13.2 |
| 扩展语料 | 原有本地语料，加上 F1 夹具 × 首字节 `0x00`/`0x85`：WAV 16 个文件 32 项（含 3.0/4.0/5.1/5.1 侧/7.1 与 5 个未定义布局夹具），FLAC 21 个文件 42 项（3–8 声道 CLI 夹具、手写无标签/小写标签/侧环绕、掩码标签与注释错误等）。语料不入库 |
| 参数 | `cargo +nightly fuzz run <目标> -- -max_total_time=600 -max_len=1048576 -seed=11` |
| `wav_decode` | 600 s，2 093 486 次执行，覆盖 408，特征 1326，语料 208 个；**无崩溃、无断言失败**，无 artifact |
| `flac_decode` | 600 s，334 620 次执行；**无崩溃、无断言失败**，无 artifact。最终语料目录共 2053 个文件。本次输出只保留了末尾 40 行，覆盖与特征数没有留存 |

说明：后台启动方式出了差错，同参数的 `wav_decode` / `flac_decode` 各多跑了一个实例，与上表两个实例同时运行（共 4 个进程，4 核），共用语料目录。多出的 `wav_decode` 实例 600 s 共 2 634 097 次执行，覆盖 408，无崩溃；多出的 `flac_decode` 实例日志丢失，但 artifact 目录为空。上表的执行速度受并行影响偏低。

## 8. 未运行与未完成

- **真实多声道设备**：WASAPI 真机多声道矩阵与 ALSA 真机 chmap 都未运行，当前环境没有设备。假设备测试不能代替真机，不证明 DAC 端声道正确。
- **B 层 snd-aloop**：未运行（CI runner 内核不含该模块）。snd-aloop 本身也只证明字节路径，不证明映射。
- **I²S TDM 多声道**：另列，Embedded 配置档仍为双声道。
- 下混、上混、声道选择、Ambisonics 与对象音频不在范围内。
