# M0 T3：pack 与 Windows 纯格式描述记录

## pack（`qy-pcm-format::pack`）

- 输入是 `Pcm<'_, Exact>`：解码器输出的交错右对齐 `i32` 加 `SourceFormat`（N 位）。`Pcm<Altered>` 只是一个标记类型，M0 没有任何操作会产生它，`pack` 也只接受 `Exact`。类型标记只约束软件操作，不证明设备端 bit-perfect。
- 检查顺序：采样率、声道与设备一致，否则 `FormatMismatch`；N ≤ V，否则 `Precision`；样本在 N 位有符号范围内，否则 `SampleOutOfRange`。不截断、不饱和、不加抖动。
- 放置方式：
  - `MsbAligned`（WASAPI，如 32/24）：容器值 = `s << (C − N)`，低 C−V 位为 0。
  - `LsbAligned`（ALSA `S24_LE` 这类）：容器值 = `s << (V − N)`，高位符号扩展。M2 接入 ALSA 时需对照 ALSA 文档再核对高字节约定。
- 两种放置方式下，满幅都仍是满幅，相对幅度不变。
- 字节序支持小端和大端。只 pack 输出缓冲能容纳的完整帧，返回帧数。
- 生产端用 `FrameProducer::pack_push`：先看队列剩余空间，只把放得下的完整帧 pack 到暂存区再入队，返回帧数，调用方从解码器释放同样帧数。暂存区 `PACK_SCRATCH_BYTES` = 4096 帧 × 8 字节，由宿主启动时分配一次。
- 消费端 `pop_frames` 只按完整帧拷贝，环形回绕时最多两段 `copy_from_slice`，不做位移、字节序或格式转换。

## 纯格式描述（`qy-pcm-format::wave`）

- `WaveFormatExtensible::pcm(rate, channels, C, V)` 和 `from_device(&DeviceFormat)` 给出以下字段：
  - tag = 0xFFFE，cbSize = 22；
  - rate、channels；
  - wBitsPerSample = C，wValidBitsPerSample = V；
  - blockAlign = channels × C/8，avgBytesPerSec = rate × blockAlign；
  - 声道掩码：单声道 FrontCenter(0x4)，双声道 FL|FR(0x3)；
  - 子格式为 `KSDATAFORMAT_SUBTYPE_PCM` {00000001-0000-0010-8000-00AA00389B71}。
- `from_device` 要求小端且高位对齐，否则返回 `Layout`。
- `WaveFormatEx::pcm16(rate, channels)`：tag = 1，cbSize = 0，只用于 16/16 单/双声道；其余字段与 EXTENSIBLE 相同。
- 拒绝：采样率为 0、声道数为 0 或大于 2、V = 0、V > C、容器不属于 16/24/32、`rate × blockAlign` 溢出 u32。纯描述本身不限制 192 kHz 上限，播放策略的上限由 `DeviceFormat` 负责。
- `to_bytes()` 按 Windows `pack(1)` 布局序列化（18 / 40 字节）。测试中的期望字节是手写的。
- 出处：Microsoft Learn 的 `WAVEFORMATEX`、`WAVEFORMATEXTENSIBLE` 文档；ksmedia.h 中的 `SPEAKER_*` 与 `KSDATAFORMAT_SUBTYPE_PCM`。windows crate 0.62.2 的 `WAVEFORMATEX` / `WAVEFORMATEXTENSIBLE` 为 `repr(C, packed(1))`。

## Windows 字段映射（`qy-host-wasapi::sdk_format`，仅 Windows）

纯字段逐项复制到 `windows` crate 的 `WAVEFORMATEX` / `WAVEFORMATEXTENSIBLE` / `GUID`，不调用 COM。测试检查以下几项：

- 结构大小分别为 18、40、16 字节；
- 各字段与 SDK 常量 `WAVE_FORMAT_EXTENSIBLE`、`WAVE_FORMAT_PCM`、`SPEAKER_*`、`KSDATAFORMAT_SUBTYPE_PCM` 相符；
- SDK 结构的内存字节与纯描述的 `to_bytes()` 完全相同。

## 测试一览

| 测试 | 内容 |
| --- | --- |
| `pack::independent_expected_bytes` | 13 组手写期望字节：16/16、N16→32/24、N24→32/24、24/24、N16→24/24、32/32、N20→32/24、S24_LE（含极值）、大端 |
| `pack::roundtrip_...` | 容器/有效位组合 16/16、24/24、32/24、32/32、32/20、24/20，两种放置 × 两种字节序 × 单/双声道 × 所有 N ≤ V。逆变换独立实现，并检查低填充位为 0 |
| `pack::rejects_...` | N > V、采样率/声道不符、半帧、样本越界（正负） |
| `wave::*` | 完整字段矩阵（含 GUID、掩码）、WAVEFORMATEX 与 EXTENSIBLE 共同字段一致、手写 40/18 字节布局、非法与溢出拒绝、`from_device` 布局要求 |
| `qy-host-sim/tests/pack_pipeline.rs` | 真实解码 → pack → 队列 → 不规则消费，重复到超过 768000 字节以跨越回绕点。6 种格式与 WAV 原始 data 字节逐字节一致；16→32/24 与独立公式一致；N > V 和帧宽不符时拒绝且不入队 |
| `qy-host-wasapi::sdk_format::*` | 见上一节（仅 Windows） |

变异检查：把 MSB 位移改成 C−V，或把回绕第二段改成填零，都会让测试失败。
