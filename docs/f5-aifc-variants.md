# F5：AIFF-C 未压缩变体 `twos`、`sowt`、`fl32` / `fl64`

日期：2026-10-18。任务卡：[tasks/F5.md](../tasks/F5.md)。本卡的结果来自自有解码器测试、模拟宿主、fuzz、Kani 与 QEMU，**不是实机结果**。

## 1. 做了什么

用户 2026-10-18 决定把这四种类型纳入基础格式（取代 2026-10-15 “不进 Core”的决定）。`qy-codec-aiff` 现在解码：

| 类型码 | 含义（QuickTime File Format 2001-03-01 表 3-3） | 处理 |
| --- | --- | --- |
| `NONE` | 未压缩（AIFF-C 原文） | 不变 |
| `twos` | 二进制补码，大端 | 与 `NONE` 相同：`sampleSize` 1–32，容器 ceil(N/8) 字节，有效位左对齐，低位填充丢弃 |
| `sowt` | 16 位小端补码 | 小端，其余与 `NONE` 相同；`sampleSize` 1–32（8/24/32 位是 libsndfile 等写入程序的实际用法，含义没有歧义） |
| `fl32` / `FL32` | 32 位浮点 | binary32，大端；`sampleSize` 必须为 32，否则不合规 |
| `fl64` / `FL64` | 64 位浮点 | binary64，大端；`sampleSize` 必须为 64，否则不合规 |

- 浮点按位输出，与 F3 的 WAV 浮点约定相同（binary32 每样本 1 个 `i32`，binary64 每样本 2 个 `i32`，先低后高），接入 F3 的浮点路径：设备接受同一浮点编码时严格路径按位输出；不接受时严格路径拒绝；明确开启的浮点转整数照常可用（报告标为“样本已改变”）。NaN、无穷原样传递。
- 其他规范之外的类型照旧按“规范之外的扩展”拒绝并写明类型码：`raw `、`in24`、`in32`、`42ni`、`23ni`、`ulaw`、`alaw`、`ima4` 等。
- 新明细 `Violation::FloatSampleSize`（浮点类型的 `sampleSize` 与类型不符）；新类型 `SampleType` 与 `AiffDecoder::sample_type()`。
- `qybit decode` 的输出写明样本类型（如“样本类型：AIFF-C sowt（小端整数）”）。播放报告的格式行只写采样率、声道与位数（浮点写“32 位浮点”），没有单独写类型码（需要扩展播放器的诊断接口，未做）。

## 2. 规范出处与核对

- QuickTime File Format（Apple，2001-03-01）第 3 章表 3-3：`twos` “two's-complement … 16-bit big-endian”（表中同时给出 8 位的取值范围）、`sowt` “16-bit little-endian, twos-complement”、`fl32` “32-bit floating point”、`fl64` “64-bit floating point”。来源与校验值见 [specs.md](specs.md)。表中没有写浮点的字节序；AIFF-C 是大端容器，FFmpeg 与 libsndfile 都按大端读取，本项目同样按大端。
- 交叉核对（只读，不复制代码）：
  - libsndfile 1.2.2 `aiff.c`：写入 `twos` / `sowt` 时用于 8、16、24、32 位；读取 `fl32` / `FL32` 与 `fl64` / `FL64` 两种写法；对 `sampleSize` 不是 32 的 `fl32` 损坏文件改写为 32（本项目先严格拒绝，以后有真实样例再按 W1 的方式逐条加入修正）；
  - FFmpeg n7.1 `aiff.c` / `aiffdec.c`：`twos` 按 `sampleSize` 当作大端整数；`sowt` 只当作 16 位小端；`fl32` / `fl64` 为大端浮点。

## 3. 验证

| 层 | 内容 | 结果 |
| --- | --- | --- |
| 独立参考夹具（`tools/qy-gen-fixtures aiff`，`aifc_twos_*`、`aifc_sowt_*`、`aifc_fl*`、`aifc_FL*`、`bad_*`） | `twos`：1、8、12、16、24、32 位；`sowt`：8、12、16、20、24、32 位，单 / 双 / 3 声道；低位填充非零与 SSND offset ≠ 0；`fl32` / `FL32` 双声道 120 帧、`fl64` / `FL64` 单声道 77 帧，含 ±0、次正规数、最大有限值、±∞、静默 NaN 与带载荷的 NaN 及随机位模式；拒绝：浮点 `sampleSize` 与类型不符 3 例；扩展仍拒绝：`raw `、`ulaw`、`in24`、`23ni`。整块、逐字节与随机分块三种输入 | 全部与期望一致 |
| ffmpeg 第二参考 | `twos` 全部位数、`sowt` 16 位逐样本；`fl32`、`fl64` 按 f32le / f64le 原样输出逐位比较（ffmpeg 不认大写的 `FL32` / `FL64`，16 位以外的 `sowt` 它只当 16 位读，这些只用独立参考） | 一致 |
| `qy-testkit` 契约 | 全部 AIFF 夹具（含新类型） | 通过 |
| 模拟宿主（`qy-host-sim/tests/sim_aifc.rs`） | 15 个整数夹具各自在种子 1–4 × 两种设备模型下播放；`sowt`、`twos`、AIFF `NONE` 与 WAV 混排；浮点严格路径（设备接受同一浮点编码 / 不接受）；开启转换后，测试内独立写出的 `fl32` / `fl64` 文件转为 V = 32 / 24 / 16 位，与按 F3 §4 定义独立计算的结果比较 | 设备字节逐字节一致；不接受浮点时按设备不支持拒绝 |
| fuzz | `aiff_decode` 300 秒（语料为全部 AIFF 夹具，7 528 863 次）；`auto_decode` 120 秒（1 527 616 次）。第一次运行时 fuzz 目标自身的检查在 64 位浮点上溢出（`1 << (bits − 1)`，并按每样本一个字计数）：解码器没有问题，fuzz 目标改为按每帧字数计数、只对整数检查取值范围后，该输入通过 | 无崩溃 |
| 定位（`qy-host-sim/tests/core_seek.rs`） | 全部 AIFF 夹具（含新类型）按目标帧定位，可定位与不可定位音源；测试按每帧字数计数（binary64 每样本两个字） | 样本级精确 |
| Kani（`cargo kani -p qy-codec-aiff`） | 原有证明 | 通过 |
| no_std 与 Clippy | thumbv7em 构建与 Clippy；主机、Windows（gnu）全部 crate Clippy `-D warnings` | 通过 |
| QEMU 演示（Embedded Full / Lite） | 照常通过；`.text` 分别增加 1 436 / 1 444 字节（相对 F4），`.bss` 不变 | 通过 |

## 4. 已知限制

- 播放报告的格式行没有单独写类型码（见 §1）。
- `fl32` 的 `sampleSize` 不是 32 的损坏文件按不合规拒绝（libsndfile 会改写后播放）。
- 4、5、6 及以上声道照旧按 F2 的规则（歧义或未定义布局拒绝）。
- 只读，不写 AIFF-C。
- 真实设备上未运行。
