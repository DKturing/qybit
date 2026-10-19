# D1：DSD 输出通道

日期：2026-10-19。任务卡：[tasks/D1.md](../tasks/D1.md)。本卡的结果来自单元测试、Kani 证明、假设备与示例的无硬件后端，**不是实机结果**：没有在真实 DAC 上运行，虚拟环境只证明软件送出的字节正确，不证明 DAC 以 DSD 方式解码（AGENTS §3、§4）。

## 1. 做了什么

Full 插件的 DSF / DFF 解码器解出的是 1 位 DSD 码流。本卡让它们能把 DSD 原样送到设备：

- **格式层**（`qy-pcm-format`）：`SampleEncoding::Dsd`；`SourceFormat::dsd(位率, 布局)`（帧 = 每声道一个 32 位 DSD 字，最早的位在 bit 31，`sample_rate()` 为字率 = 位率 ÷ 32）；`DeviceFormat::dsd(字率, 布局, DsdTransport)`，传输方式为原生 `DSD_U8` / `DSD_U16_LE` / `DSD_U16_BE` / `DSD_U32_LE` / `DSD_U32_BE` 或 DoP（`S24_3LE`、`S32_LE` 高 24 位、`S24_LE` 低 24 位）。`device_rate()`、`device_frame_bytes()`、`expansion()` 给出设备侧参数；`dop_wire()` 给出 DoP 在设备上实际打开的 PCM 格式；`silence_frame()` 给出一帧静音（PCM 全零、原生 `0x69`、DoP 标记 + `0x6969`）。
- **pack**：DSD 字按传输方式写成 e 个设备帧（原生按字节序原样拆分；DoP 加标记 `0x05` / `0xFA`）；声道按位置纯置换；不改任何一位。DSD 源不能放进 PCM 设备格式，PCM 源也不能放进 DSD 设备格式；单声道复制（O1）、浮点转整数（F3）不适用于 DSD。
- **为什么“帧”是一个 DSD 字**：引擎、队列、预填、位置与时长都按帧计。让设备格式的帧等于一个字（e 个设备帧），这些计算对 DSD 不需要任何改动；每个字固定展开为完整的 e 个设备帧，且每个字的 DoP 标记都从 `0x05` 开始，所以队列里任何位置都是完整的设备帧，DoP 标记的相位在整个流中连续——包括暂停、欠载与排空时补的静音帧。
- **渲染循环**：暂停、尾部填充与排空写的是设备格式的静音帧（原来写零字节；PCM 不变）。
- **ALSA**（内核接口）：候选“原生优先、DoP 其次”，只取设备支持的；按设备帧率设置与读回核对，周期为 e 的倍数。写入适配层按字换算：可写空间按完整的字报告；`writei` 只写了一个字的一部分时，余下的设备帧留在适配层、按已写入报告，下一次写入与排空前先写完——队列中永远是完整的字。alsa-lib 后端（只用于 A 层回环）同样映射 DSD 格式码。
- **WASAPI**：只有 DoP（PCM 32/24、24/24，设备帧率为字率 × 2），`WAVEFORMATEXTENSIBLE` 按 `dop_wire()` 填写；整包必须是完整的字，端点缓冲帧数为奇数时该候选按不可用处理；`GetBuffer` / `ReleaseBuffer` 按设备帧数调用。设备不接受时说明“WASAPI 没有原生 DSD，本程序不把 DSD 转为 PCM”。
- **嵌入式**：本卡不实现 DSD 输出，DSD 源在选择设备格式时明确拒绝（`SelectError::Dsd`），不放进 PCM 槽位。
- **报告与界面**：源格式“DSD64（2.8224 MHz）/ 2 声道 / 1 位 DSD”；请求与协商写“原生 DSD_U32_BE”或“DoP（以 PCM 176400 Hz / 容器 32 位 / 有效 24 位封装；DAC 须识别 DoP 标记，否则会当作 PCM 播放成噪声）”；播放计划写 DSD 位率、字率与设备帧率；图形界面格式列“DSD64 2.8224 MHz 2 声道”、设备“DSD64 2 声道 DoP（PCM 176.4 kHz 容器 32 位）”、指示灯“DSD64 DoP / 原生”。`qybit decode` 对 DSD 源明确拒绝（WAV 只装 PCM）。

## 2. 规范与参考

- `snd_pcm_format_t` 的 DSD 码（`DSD_U8` = 48、`DSD_U16_LE` = 49、`DSD_U32_LE` = 50、`DSD_U16_BE` = 51、`DSD_U32_BE` = 52）：Linux v6.12 `include/uapi/sound/asound.h`，见 [specs.md](specs.md)。
- 原生 DSD 的字节顺序（最早的字节在值的最高字节，`_LE` / `_BE` 是该值的字节序）与 DoP 的写法（24 位值 = 标记 << 16 | 较早字节 << 8 | 较晚字节，每对设备帧依次 `0x05`、`0xFA`）：MPD v0.23.15 `src/pcm/Dsd32.cxx`、`Dsd16.cxx`、`Dop.cxx`，只读核对，不复制代码。
- DoP 开放标准 v1.1 的原文（dsd-guide.com）2026-10-19 取得时返回 403，未能核对原文；以上 DoP 写法按 MPD 的实现核对。MPD 在 32 位容器（`S24_P32`）的最高字节写 `0xFF`，本实现的 `S24_LE` 写 bit 23 的符号扩展（`0x05` 帧为 `0x00`、`0xFA` 帧为 `0xFF`）；`S24_LE` 只用低 24 位，最高字节不送到 DAC。

## 3. 验证

| 测试 | 内容 | 结果 |
| --- | --- | --- |
| `qy-pcm-format/tests/dsd.rs` | 构造与拒绝（位率非 32 倍数、0、溢出）；8 种传输方式的 e、设备帧率、设备帧宽、`block_align`、字节率数值核对；PCM / DSD 编码隔离；pack 与按位写出的独立参考逐字节一致（单声道、立体声、声道互换、5 声道置换，随机字与全 0 / 全 1 / 首尾位，1、37、300 个字），容量不足时只写完整的字；静音帧等于 DSD 静音字的 pack 结果；ALSA 格式码与名称往返、候选顺序、按设备帧率构造与读回核对；WASAPI DoP 的 `WAVEFORMATEXTENSIBLE` 等于 PCM 176.4 kHz 32/24、24/24，原生 DSD 与 `WAVEFORMATEX` 明确拒绝 | 通过 |
| Kani（`cargo kani`，8 个证明） | 每种传输方式：任意 32 位字与任意设备帧序号，写出的样本按位取回正好是该字时间顺序上的对应段，字节序、DoP 标记与容器的填充字节符合规范 | 通过 |
| `qy-player` 渲染循环 | DoP：整包循环在暂停、尾部填充与排空时写 DoP 静音帧，整个设备流的标记逐设备帧交替；原生 `DSD_U8`：短写循环暂停时写 `0x69`，去掉插入的静音后与源一致 | 通过 |
| `qy-host-alsa`（假 PCM） | 设备支持 `DSD_U32_BE` 时优先原生（88.2 kHz），否则 DoP `S32_LE`（176.4 kHz，周期为偶数）；随机短写（含写了半个字）下设备字节与独立公式逐字节一致；只支持 `S16_LE` 时明确拒绝、不播放 | 通过 |
| `qy-host-wasapi` | DSD 候选只有 DoP 32/24、24/24，只用 EXTENSIBLE；失败说明写明不转 PCM（Windows 交叉 Clippy 通过；真机未运行） | 通过 |
| 示例 `tests/dsd.rs` | 示例 DSD 格式 QYD1（代表 DSF / DFF 插件）经真实引擎、pack、帧队列与整包渲染循环送到无硬件后端：原生 `DSD_U32_BE` 与 DoP 两种设备的有效字节与独立参考逐字节一致；只接受 PCM 的设备明确拒绝；按字直接定位，定位后的流从目标字起一致；QYD1 满足解码器契约与识别一致性 | 通过 |
| 嵌入式 | DSD 源（DSD64 立体声、DSD128 单声道）在选择设备格式时拒绝，原因写明不转 PCM | 通过 |
| `qy-ui` | DSD 源、原生与 DoP 设备的格式文字 | 通过 |
| no_std、QEMU 演示 | `qy-pcm-format`、`qy-core`、`qy-host-embedded` 的 thumbv7em 编译与 Clippy；QEMU 演示照常通过，`.text` Embedded Full / Lite 分别增加 2 228 / 3 116 字节（相对 E5；DSD 的 pack 与静音帧），`.bss` 不变 | 通过 |

## 4. 未做

- 真实 DAC（原生 DSD 与 DoP）未运行；snd-aloop 回环中的 DSD 格式没有运行（虚拟机回环脚本只覆盖 PCM 与浮点）。
- 嵌入式 DSD 输出（I²S DoP 或原生 DSD 外设）。
- DSD 解码器本身（DSF / DFF）由 Full 插件仓库实现；本仓库只有示例 QYD1。
