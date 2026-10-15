# F3：浮点 PCM（WAV 32/64 位 IEEE 浮点）

日期：2026-10-15。任务卡：[tasks/F3.md](../tasks/F3.md)。规范出处见 [specs.md](specs.md)。

本卡的结果来自以下几类检查，**不是实机结果**：
- 自有解码器测试与模拟宿主；
- A 层数字回环（alsa-lib `file` 插件）；
- fuzz；
- QEMU 指令集仿真。

WASAPI 与 ALSA 真机上的浮点独占播放、MCU 实物都未运行（见 §7）。

## 1. 解码（`qy-codec-wav`）

- **识别**：格式码 3（`WAVE_FORMAT_IEEE_FLOAT`），或 EXTENSIBLE 且子格式为 `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT`（`{00000003-0000-0010-8000-00AA00389B71}`）。
- **位宽**：
  - `wBitsPerSample` 为 32 或 64；EXTENSIBLE 时它是容器位宽，`wValidBitsPerSample` 必须等于容器；
  - 16/24 位浮点按“合规但尚不支持”拒绝（`Unsupported`）；
  - EXTENSIBLE 有效位不等于容器按不合规拒绝（`Malformed`）；
  - Microsoft Learn `WAVEFORMATEX` 对格式码 3 只写到 “`wBitsPerSample` should be set to 32”。64 位按任务卡 §2 接受：EXTENSIBLE 下容器位宽可以是 8 的任意倍数，ffmpeg 写出的 64 位浮点 WAV 就是 EXTENSIBLE + IEEE_FLOAT（见 [specs.md](specs.md)）。
- **`fact` chunk**：按“先严格”执行，暂无容错条目。
  - 必须出现在 `data` 之前，且帧数等于 `data` 长度 / 块对齐，否则按不合规拒绝（明细 `WavDetail::Fact`）；
  - 覆盖的拒绝情形：缺少、在 `data` 之后、重复、帧数不一致；
  - ffmpeg 输出到管道时的样子（RIFF 与 `data` 长度为占位值 0xFFFFFFFF、没有 `fact`）同样按缺少 `fact` 拒绝；W1 对整数 PCM 的占位长度容错不用于浮点；
  - 以后按真实样例逐条加容错（同 W1）。
- **样本**：
  - 小端位模式按位放入 `pcm()`，不做数值运算；
  - binary32 每个样本 1 个 `i32`，binary64 每个样本 2 个 `i32`（低字在前）；
  - `release(frames)` 仍按帧计，帧的字数为 `SourceFormat::words_per_frame()`。

## 2. 严格路径：原样输出到浮点设备

- **类型**：
  - `SampleEncoding { Int, Float32, Float64 }` 加入 `SourceFormat` 与 `DeviceFormat`；
  - 原有构造函数仍为 `Int`；
  - `DeviceFormat::accepts` 要求编码相同。
- **pack**：
  - 编码相同时按位复制，按设备字节序写出，F1 的声道置换同样适用；
  - 编码不同时返回 `PackError::Encoding`，不做任何数值运算。
- **WASAPI**：
  - 浮点源只请求 EXTENSIBLE + `IEEE_FLOAT`，`wBitsPerSample = wValidBitsPerSample` = 浮点位宽；
  - 端点拒绝时报告“端点在独占模式下不接受 32 位浮点（IEEE_FLOAT）；严格路径不转换（可明确开启浮点转整数，样本会改变）”。
- **ALSA**：
  - 使用 `SND_PCM_FORMAT_FLOAT_LE`（14）与 `FLOAT64_LE`（16），设置后读回已应用参数；
  - 大端浮点（`FLOAT_BE` / `FLOAT64_BE`）按字节序不同拒绝。
- **非有限值**：NaN 与 ±∞ 在严格路径中原样送出。报告给出个数，结论仍是严格路径。
- **binary32 → binary64、整数 → 浮点**：不做（任务卡 §8 决定 3）。设备只接受另一种浮点时按设备不支持拒绝。

## 3. 转换选项：浮点转整数（明确开启，样本已改变）

- **开启方式**（默认关闭）：
  - CLI：`--float-to-int`；
  - GUI：复选框“浮点转整数（会改变样本）”；
  - 嵌入式：板级 `Player::set_float_policy`，或串口命令 `float-to-int on|off`；
  - 开启后对之后开始的曲目生效。
- **映射**（`qy_pcm_format::convert`，`Pcm<Altered>` 唯一的产生者）：
  - 先算 `s = round_half_even(x × 2^(V−1))`，再限幅到 `[−2^(V−1), 2^(V−1) − 1]`；
  - 只用整数运算：按 IEEE 754 的符号、指数、尾数直接缩放与舍入（含次正规数），不依赖硬件浮点，桌面与没有双精度 FPU 的 MCU 结果逐位相同；
  - 不加抖动，同一输入总得到同一输出。
- **协商**：
  - 先按源格式请求同一浮点编码；设备不接受时，依次请求 32 → 24 → 16 位整数，取第一档被接受的格式；
  - 超出本机能力的档直接跳过，例如 Embedded Full 只到 24 位，32 位一档跳过；
  - 浮点源本身超出本机能力（按浮点宽度估计的队列放不下）时，同样从第一档整数起试。例如 Embedded Full 播放 f64 双声道 96 kHz 时，预填需 384 000 字节，队列只有 192 016 字节，转为 24 位后放得下；
  - 同一浮点格式此前已被当前设备拒绝时，相邻曲目直接续用当前整数设备，不重新协商。
- **非有限值**：
  - 遇到 NaN / ±∞ 时，该曲目在第一个非有限样本所在帧停止，之前的帧照常排空；
  - 报告“含非有限样本，无法转换”及帧位置（`EngineFault::NonFinite { frame }`）；
  - 跳过策略下继续下一首（任务卡 §8 决定 2）。
- **工作区**：
  - 宿主启动时一次性提供 `&'static mut [i32]`，大小与流式 PCM 工作区相同（8 192 个样本，32 768 字节）；
  - Desktop 与 Embedded Full 提供，Embedded Lite 不提供；Lite 开启转换后，浮点曲目按资源不足拒绝（`CapabilityError::ConvertWorkspace`）。
- **报告**：
  - 每首被转换的曲目都报告“曲目 N 样本已改变：32 位浮点转为 24 位整数（明确开启；不加抖动，round-half-even）”，并给出样本数、限幅数与不精确数；
  - 不精确数为 0 时写明“每个都能被 V 位整数精确表示……转换未损失数值，但表示已改变”；
  - 结论不再是严格路径；
  - 嵌入式有通知 `EVENT samples-altered track=N float->intV explicit` 与 `EVENT float-stats`，状态行含 `converted=` 与 `float_to_int=`。
- **O1 报告的修正**：此前连续同格式的单声道曲目只有第一首报告“声道已改变”。现在“声道已改变”与“样本已改变”都在每首曲目第一次入队时报告。

## 4. 夹具与参考

- **生成器**：`tools/qy-gen-fixtures`（`wav_float.rs`，纯 Rust），在 `qy-codec-wav/tests/fixtures/float` 下生成 16 个夹具。
  - 可播放的 7 个：
    - `f32_stereo_44k_tag3`、`f32_mono_48k_ext`；
    - `f64_stereo_96k_tag3`、`f64_51_48k_ext`（5.1 掩码）；
    - `f32_fact_first_list`（`fact` 在前、`LIST` 在中间）；
    - `f32_specials`、`f64_specials`。
  - 特殊值覆盖：0、−0、±1.0、略大于 1.0、次正规数、最小/最大有限值、静默与信号 NaN、±∞，以及由 16/24 位整数生成的值与任意值。
  - 拒绝情形的 9 个：
    - 16/24 位浮点；
    - EXTENSIBLE 有效位为 24；
    - 缺少 `fact`、`fact` 在 `data` 之后、重复、帧数不一致；
    - 占位长度、截断。
- **独立期望**：生成器直接写出位模式（`.expect`），测试逐位比较。
- **第二参考 ffmpeg**：
  - `-f f32le` / `-f f64le` 原样输出，与解码结果按位一致；
  - 转换参考为 `pcm_s16le` / `pcm_s32le`。ffmpeg 的 SIMD 转换在 |x| ≥ 1 时溢出（例如 2^20 转 16 位得到 −32768），所以 ffmpeg 转换参考只用于 [−1, 1) 内的值；限幅按独立公式核对。
- **转换参考**：测试内用 `f64` 与 `i128` 按定义计算，不调用被测实现。
  - 舍入边界：V = 1 / 2 / 8 / 16 / 20 / 24 / 32 各 2 000 个随机整数位置，正负两个方向，正好半个 LSB 及其 ± 1 ulp，binary32 与 binary64 都覆盖；
  - 随机位模式：200 000 个（binary32 与 binary64，V 随机取上述值）；
  - 特殊值：0、−0、±1.0、次正规数、最大有限值、NaN 与 ±∞；由整数生成的浮点转换后精确（不精确数为 0）；
  - fuzz 目标 `float_convert` 对任意位模式与 V = 1…32 核对同一独立公式（§6）。

## 5. 验收结果（任务卡 §7）

| 项 | 结果 |
| --- | --- |
| 1 解码与参考 | 7 个可播放夹具与独立期望按位一致，与 ffmpeg 第二参考一致；9 个拒绝情形的类别与明细符合期望 |
| 2 任意分块 | 1 字节、随机、整块三种分块结果相同 |
| 3 转换 | 与独立公式逐样本一致；V = 16 / 32 在 [−1, 1) 内与 ffmpeg 一致；限幅与不精确计数正确 |
| 4 fuzz | 见 §6 |
| 5 模拟宿主（`qy-host-sim/tests/sim_float.rs`，6 项） | 种子 1–4 × 两种设备模型（WASAPI 整包 / 短写），读取含零长度读、短读与慢读：<br>• 浮点设备上逐位一致（含 NaN 与无穷，计数）；<br>• 整数设备上关闭转换时拒绝；开启时转为设备的最佳整数格式并与参考一致；<br>• NaN 在所在帧停止；<br>• 没有工作区时按资源不足拒绝；<br>• 浮点超出队列容量时，开启转换照常播放，严格路径按队列不足拒绝；<br>• 浮点与整数混排 |
| 6 A 层回环（ALSA `file` 插件） | `FLOAT_LE` 与 `FLOAT64_LE`（48 kHz / 双声道）严格路径按位提交，对齐后逐字节一致（Match）；设备只接受 `S16_LE` 时严格路径拒绝，开启转换后与独立参考一致（共 6 行，全部通过） |
| 7 no_std 与 QEMU | thumbv7em 构建与 Clippy 通过；QEMU 演示见下 |
| 8 文档 | 本文与 [specs.md](specs.md) |

**QEMU 演示**（`mps2-an386`，指令集仿真）：
- **Full**：串口脚本在播放前发送 `float-to-int on`。`T10.WAV`（f32 双声道 44.1 kHz）的处理：
  - 先请求浮点，I²S 不接受（`UNSUPPORTED Float`）；
  - 32 位整数超出 24 位上限，跳过；
  - 转为 24 位放在 32/24 槽位。设备段的 FNV-1a 等于 ffmpeg 原样输出的位模式按定义独立换算的结果；
  - 报告 `ALTERED track=9 float->int24` 与 `samples=600 clipped=0 inexact=133 nonfinite=0`。
- **Lite**：保持严格路径。`T06.WAV`（f32 单声道 48 kHz）因 I²S 不接受浮点被跳过，其余曲目照常。

**配置档预算与代码量**（thumbv7em release，演示程序，不含 `hold`）：

| 配置档 | 转换工作区 | 预算合计 | `.text`（F3 前 → 后） | `.bss`（F3 前 → 后） |
| --- | --- | --- | --- | --- |
| Desktop | 32 768 字节 | 队列等不变，另加 32 768 字节 | 桌面程序未逐项记录 | 堆分配 |
| Embedded Full | 32 768 字节 | 396 388 字节 | 102 320 → 117 812（+15 492） | 365 736 → 398 504（+32 768，即转换工作区） |
| Embedded Lite | 无 | 88 968 字节（不变） | 85 716 → 100 548（+14 832） | 91 068（不变） |

- Lite 没有转换工作区，但仍链接了浮点识别、按位 pack 与引擎的协商回退。转换函数是否被链接没有逐个符号分析；
- 栈高水位（`hold` 构建，QEMU）：Full 17 244 字节，Lite 9 508 字节。

## 6. fuzz 记录

开发机（4 核），libFuzzer，`cargo +nightly fuzz`：

- **`float_convert`**（新目标）：
  - 输入：首字节选择 V 与编码，其余为样本位模式；
  - 检查：不 panic；逐样本与独立公式一致、在 V 位范围内；整块转换与逐样本一致，统计正确，非有限值报告第一个下标；
  - 初始语料：7 个 ffmpeg 原样输出的浮点样本文件 × 3 种首字节（binary32 V = 1、binary32 V = 8、binary64 V = 24）；
  - 命令：`cargo +nightly fuzz run float_convert -- -max_total_time=600 -max_len=4096 -seed=1`；
  - 结果：601 秒，15 839 973 次执行，0 崩溃。
- **`wav_decode`**：
  - 本地已有语料加入 16 个浮点夹具 × 2 种分块前缀（共 814 个输入）；
  - 命令：`-max_total_time=600 -max_len=65536 -seed=1`；
  - 结果：601 秒，3 358 267 次执行，0 崩溃。
- 两个目标都加入 CI 冒烟（各 60 秒）；`auto_decode` 的 CI 语料也加入浮点夹具。
- **`auto_decode` 冒烟发现的问题**：CI 第 116、117 次运行中，`f64_specials` 夹具在分块输入时比一次性输入少一帧。原因在 fuzz 目标本身：它按 `样本数 / 声道数` 计帧，binary64 每个样本占两个字，于是取走一帧的字却 `release` 了两帧。解码器与核心按 `words_per_frame()` 计帧，不受影响。目标已改为按 `words_per_frame()` 计帧，崩溃输入复现后通过；用 CI 语料（含浮点夹具）本地冒烟 60 秒，0 崩溃。

## 7. 未运行与限制

- **真实设备未运行**：
  - WASAPI 独占浮点（端点是否接受 `IEEE_FLOAT` 取决于驱动）与开启转换后的整数回退；
  - ALSA 真机的 `FLOAT_LE`；
  - MCU 实物。
- **规范原文未取得**：IEEE 754-2019 与 RIFF 1991（《Multimedia Programming Interface and Data Specifications 1.0》）。
  - `fact` 规则按任务卡执行；binary32 / binary64 的编码按通行定义实现，并与 Rust `f32::from_bits` / `f64::from_bits` 及 ffmpeg 交叉核对；
  - 请用户提供原文后补核。
- **`fact` 无容错**：缺少或不一致一律拒绝，以后按真实样例逐条加。
- **AIFF-C 的 `fl32` / `fl64`**：照旧按“规范之外的扩展”拒绝（任务卡 §8 决定 1）。
