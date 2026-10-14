# F2：AIFF / AIFF-C（未压缩 PCM）

日期：2026-10-14。任务卡：[tasks/F2.md](../tasks/F2.md)。规范版本与校验值见 [specs.md](specs.md)。本卡的结果来自自有解码器测试、模拟、A 层数字回环（alsa-lib `file` 插件）、fuzz 与 QEMU，**不是实机结果**。

## 1. 原则

Core 最严格，只接受规范定义的内容（用户决定）。拒绝分三类，各有独立的明细项（`AiffDetail`），报告写明类别；以后增加支持时只改对应的一项：

| 类别 | `DecodeError` | 情形 |
| --- | --- | --- |
| 不符合规范 | `Malformed` / `Truncated` | 缺少或重复必需的 chunk、COMM/FVER 长度不符、chunk 越出 FORM、长度为负、FORM 末尾残余字节、`numChannels` < 1、`sampleSize` 不在 1–32、SSND 装不下 COMM 声明的帧、文件比声明的短 |
| 符合规范但尚不支持 | `Unsupported` | SSND 在 COMM 之前；采样率不是正整数或超出 32 位；FVER 版本不是 AIFCVersion1；`ACE2`/`ACE8`/`MAC3`/`MAC6` 压缩；4、6 声道（规范约定有歧义）；5 声道及以上（规范无约定） |
| 规范之外的扩展 | `Unsupported` | AIFF-C 原文未定义的 `compressionType`：`sowt`、`twos`、`raw `、`fl32`、`ulaw` 等 |

## 2. 实现（`crates/qy-codec-aiff`）

- `no_std`、无 alloc，调用级可暂停，任意分块；不可信输入不 panic。工作区与 WAV 相同（`PCM_WORK_SAMPLES`）。
- **结构**：
  - FORM 类型 `AIFF` / `AIFC`；chunk 长度为有符号 32 位（`long`），奇数长度后有 1 字节填充，填充也必须在 FORM 之内；
  - AIFF 的 COMM 长度恒为 18；AIFF-C 的 COMM 为 22 + pstring（计数 + 文本，总长为偶数）；
  - AIFF-C 的 FVER 有且只有一个，长度 4。
- **采样率**：80 位扩展精度精确换算为整数（`extended_to_u32`），不四舍五入；非整数、0、负数、无穷与超出 `u32` 的值拒绝，明细给出 10 字节原值。
- **样本**：
  - 1–32 位补码、大端、左对齐，每样本占 ⌈N/8⌉ 字节；算术右移取出 N 位，低位填充丢弃（与 WAV 一致）；
  - 帧数以 COMM 的 `numSampleFrames` 为准，SSND 中帧之后的字节不是样本；`offset` 字节跳过，`blockSize` 只是对齐提示。
- **声道**：1 → `[FC]`，2 → `[FL FR]`，3 → `[FL FR FC]`（AIFF 1.3 的约定），按 F1 的规则送到设备。
- **其他 chunk**：可选与未知的 chunk 都跳过，如 NAME、ANNO、COMT、`ID3 `、MARK、INST、APPL。FORM 之后的字节不属于本文件，忽略。
- **最大连续输入**：278 字节（AIFF-C 的 COMM）或一帧，远小于核心最小窗口 8192 字节。

## 3. 接入

- **`qy-codec-auto`**：
  - `FORM` 开头时读到 12 字节，按形式类型分派；其他 IFF 形式类型按无法识别拒绝，并写明形式类型；
  - 新特性 `aiff` 默认开启。
- **共用工作区**：
  - WAV 与 AIFF 都是流式 PCM 容器，共用一块工作区；
  - 识别出另一种容器时，工作区的所有权移交给对应的解码器，不复制、不重新分配、不用 `unsafe`；
  - 新增接口：`with_stream_pcm`、`with_aiff`、`stream_capacity`、`aiff_detail`；`WavDecoder` / `AiffDecoder` 新增 `into_pcm`。
- **配置档**：新增 `aiff` 开关，Desktop、Embedded Full、Embedded Lite 都启用（用户决定）。流式工作区只计一次，RAM 预算不变。
- **宿主**：模拟宿主、桌面播放器、`qybit decode`、嵌入式测试、QEMU 演示与 `qy-board` 都已接入。播放失败时实时输出与报告写明“AIFF 错误明细：…”。

**资源（QEMU 演示，thumbv7em release，与加入 AIFF 之前的 `a1d2891` 比较）**：

| 构建 | `.text` | `.bss` | 栈高水位 |
| --- | --- | --- | --- |
| Embedded Full | 86 972 → 98 136 字节（+11 164） | 365 736 字节，不变 | 13 028 字节（栈区 157 896） |
| Embedded Lite | 73 096 → 78 744 字节（+5 648） | 91 068 字节，不变 | 7 308 字节（栈区 432 564） |

## 4. 验证

- **夹具**（`cargo run -p qy-gen-fixtures -- aiff`，纯 Rust 构造，期望独立给出）：
  - 55 个，其中 14 个与 ffmpeg 6.1.1 逐样本一致；
  - ffmpeg 不作第二参考的情形：低位填充非零、SSND 帧后剩余字节、零帧、FORM 后多余字节（ffmpeg 的处理与规范不同或不适用）；
  - 覆盖：
    - 1、8、12、16、20、24、32 位；
    - 1/2/3 声道；
    - 8 k–192 k 及 11 025、2 822 400 Hz；
    - AIFF-C `NONE` 的 pstring 两种奇偶；
    - 可选与未知 chunk、`offset` ≠ 0、奇数长度填充；
    - 三类拒绝的全部明细项。
- **任意分块**：每个夹具按整块、1 字节、随机分块输入，结果、格式、明细与样本都与期望相同。
- **自动识别**：
  - WAV → AIFF → WAV → AIFF-C → AIFF 交替解码，结果与单独解码相同，工作区容量不变；
  - 只启用 WAV 时 AIFF 按“未启用”拒绝；只启用 AIFF（`with_stream_pcm`）时可单独工作；
  - 其他 IFF 形式类型写明；FORM 头残缺按截断处理。
- **模拟宿主**（真实核心、解码、pack 与队列；零长度读、短读与慢读）：
  - 12 个可播放夹具 × 种子 1–4 × 两种设备模型，设备字节逐字节等于独立公式 `s << (C − N)`；
  - AIFF、WAV、FLAC 混排，中间夹一个 `sowt` 文件：被拒文件按策略跳过，其余连续成一段，逐字节一致；
  - 2 822 400 Hz 单声道 24 位超出 Desktop 队列，在打开设备前按“队列不足”拒绝，给出所需 4 233 600 与可用 3 072 000 字节。
- **桌面播放器**：AIFF 曲目设备字节等于独立参考；`sowt` 文件的实时输出写明“AIFF 错误明细：AIFF-C 规范之外的扩展……'sowt'”。
- **A 层回环**（alsa-lib `file` 插件）：同步 WAV + 待测 WAV + AIFF（48 kHz / 双声道 / 24 位）在一个流内，对齐后逐字节一致（Match）。
- **fuzz**：
  - 新目标 `aiff_decode`：分块与一次性输入的结果、明细、格式与样本一致；样本在 N 位范围内；
  - `auto_decode` 同时比较 AIFF 明细，初始语料加入 AIFF 夹具；
  - 两个目标都加入 CI 冒烟；记录见 §5。
- **其他**：no_std（thumbv7em）构建与 Clippy 覆盖 `qy-codec-aiff` 及 `qy-codec-auto` 的特性组合；QEMU 演示 Full 与 Lite 照常通过；`qy-board`（stm32f411）可编译。

## 5. fuzz 记录

开发机（4 核），libFuzzer，`cargo +nightly fuzz`：

- **`aiff_decode`**：
  - 初始语料：55 个 AIFF 夹具 × 3 种分块前缀；
  - 命令：`cargo +nightly fuzz run aiff_decode -- -max_total_time=600 -max_len=65536 -seed=1`；
  - 结果：600 秒，15 982 891 次执行，0 崩溃。
- **`auto_decode`**：
  - 初始语料加入 AIFF 夹具；
  - 命令：`-max_total_time=120 -max_len=1048576 -seed=1`；
  - 结果：120 秒，110 860 次执行，0 崩溃。

## 6. 未运行与限制

- **真实设备未运行**：WASAPI、ALSA 真机播放 AIFF，以及 MCU 实物。
- **规范之外的常见类型暂不支持**：`sowt`（小端）与 `twos` 实际很常见，按“规范之外的扩展”拒绝；是否加入、依据哪份文档，另行决定。
- **SSND 在 COMM 之前**：合规但暂不支持。扩展方案（引擎新增“从头重开曲目”的动作，不破坏冻结接口）见任务卡 §2。
- **长度不一致**：一律按不合规处理，以后按真实样例逐条加容错（同 W1）。
