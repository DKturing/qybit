# O1：单声道复制到左右两路（明确开启）与端点扬声器提示

日期：2026-10-14。任务卡：[tasks/O1.md](../tasks/O1.md)。本卡的结果来自 pack 单元测试、模拟宿主、桌面播放器与 WASAPI 假后端、嵌入式宿主仿真与 QEMU，**不是实机结果**；Windows 真机试听由用户进行。

## 1. 原则

- **严格路径不变**：默认关闭；关闭时所有曲目的设备格式、设备字节与之前逐字节相同。
- **只复制，不改样本**：单声道 `[FC]` 的每个样本原样写入 `[FL FR]` 两路；不混音、不改增益、不改采样率与位深（AGENTS §3 批准的唯一非严格选项）。
- **报告写明已改变**：每首被复制的曲目都单独记录，结论不再写“软件路径无损”。
- **扬声器提示只警告**：端点报告的物理扬声器配置来自驱动，可能缺失或不准，只作提示，不影响协商与播放。

## 2. 实现

- **pack**（`qy-pcm-format`，唯一的表示转换边界）：
  - 新增 `ChannelMode { Exact, DuplicateMono }`、`check_mode`、`pack_with`；`pack` 等于 `pack_with(.., Exact, ..)`；
  - `DuplicateMono` 只接受源 `[FC]`、设备 `[FL FR]`、采样率相同且 N ≤ V，其余组合按格式不符或精度不足拒绝；
  - 输入仍是 `Pcm<Exact>`：类型标记约束的是样本值（AGENTS §3），复制不改变样本值；声道布局的改变由显式的 `ChannelMode` 参数与引擎报告承担，没有隐式入口；
  - 队列 `FrameSink::push`、`ring` 与 `static_queue` 的 `pack_push_with` 传递该参数。
- **引擎**（`qy_core::engine`，桌面、模拟、嵌入式共用）：
  - `ChannelPolicy { Strict（默认）, DuplicateMono }`，`set_channel_policy` / `channel_policy`；
  - 开启且源为 `[FC]` 时，能力检查与设备协商使用同采样率、同位数的 `[FL FR]`，并发出 `HostAction::ChannelsDuplicated { track }`，计入 `EngineStats.duplicated_tracks`；
  - **按曲目锁定**：配置设备时决定该曲目是否复制，播放中改变策略只影响之后配置的曲目，已打开的设备流不会改用另一种 pack；
  - 只对 `[FC]` 生效：立体声、只占左声道位置的单声道 `[FL]` 等照严格规则处理；复制的单声道与同采样率的立体声可以续用同一设备流。
- **宿主**：
  - CLI：`qybit --device <设备> --mono-to-stereo <文件>...`（Windows 与 Linux），开启时先打印说明；
  - 桌面播放器：实时输出与报告“曲目 N 声道已改变：单声道 [FC] 复制为 [FL FR]（明确开启；样本值未改变，不混音）”；结论为“样本值未改变，但部分曲目的声道布局已按明确开启的选项改变……不是严格路径”；新增 `PlaybackReport::strict_path_intact`（`software_path_intact` 且没有改变声道）；退出码不变（完整播放仍为 0）；
  - 模拟宿主：`Report.duplicated`，`fidelity_intact` 要求为空；
  - 嵌入式宿主：板级启动配置 `Player::set_channel_policy`，或串口命令 `mono-to-stereo on|off`（回应 `OK mono-to-stereo=on altered mono[FC]->stereo[FL FR] samples-unchanged applies-to=next-tracks`）；通知 `EVENT channels-altered track=N mono[FC]->stereo[FL FR] samples-unchanged`；状态行新增 `duplicated=`、`mono_to_stereo=on|off`。

## 3. 端点扬声器提示

- **出处**（核对 Microsoft Learn 原文，MicrosoftDocs 仓库）：
  - `PKEY_AudioEndpoint_PhysicalSpeakers`（Mmdeviceapi.h）：音频端点的声道配置掩码，表示扬声器的物理配置，`PROPVARIANT` 的 `vt` 为 `VT_UI4`；
  - 掩码的位定义见 `KSAUDIO_CHANNEL_CONFIG`（ksmedia.h）：`SPEAKER_FRONT_LEFT` 0x1、`SPEAKER_FRONT_RIGHT` 0x2、`SPEAKER_FRONT_CENTER` 0x4……与 `WAVEFORMATEXTENSIBLE` 声道掩码相同；值 0 为 `KSAUDIO_SPEAKER_DIRECTOUT`，声道不对应扬声器位置；
  - 属性键 `{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},3`，取自 `windows` 0.62.2 的定义。
- **读取**（`qy-host-wasapi`）：协商前从端点属性存储只读读取；只在 `vt` 为 `VT_UI4` 时取值，不用 `PropVariantToUInt32` 做类型转换；属性缺失、类型不符或读取失败都记为“端点未报告”。
- **报告**（协商记录 `Negotiation.speakers`，实时输出与报告共用）：

| 情形 | 输出 |
| --- | --- |
| 掩码包含设备流的全部位置 | `物理扬声器配置（驱动报告）：0x3 [FL FR]` |
| 掩码缺少设备流的位置 | 同上，另加“提示：端点报告的物理扬声器为 [FL FR]，设备流的 [FC] 不在其中，可能听不到。该信息由驱动填写，可能不准；协商与播放照常进行。” |
| 掩码为 0 | 写明 `KSAUDIO_SPEAKER_DIRECTOUT`，不提示 |
| 属性缺失或读取失败 | `物理扬声器配置：端点未报告`，不提示 |
| ALSA | `物理扬声器配置：不可查询（本后端没有对应属性）`，不推测 |

比较对象是设备流的布局（协商结果）：开启单声道复制时为 `[FL FR]`。

## 4. 验证

- **pack 单元测试**：复制后每帧两份相同的容器字，与独立参考逐字节一致；非 `[FC]` 源、立体声源、非 `[FL FR]` 设备、采样率不同、精度不足都拒绝；严格模式下同一组合按格式不符拒绝；输出容量只够部分帧时只 pack 完整帧。
- **模拟宿主**（`crates/qy-host-sim/tests/sim_mono_duplicate.rs`；真实核心、解码、pack 与队列；零长度读、短读、慢读；种子 1–4 × WASAPI 整包 / 短写）：
  - 开启：WAV 16 位 8 kHz、WAV 24 位 48 kHz、AIFF 8 位 8 kHz、AIFF 20 位 96 kHz 单声道，在任意布局设备与只支持立体声的设备上，设备字节都等于独立公式 `s << (C − N)` 每样本两份；`duplicated == [0]`，记录不算严格路径完整；
  - 关闭：同样四个文件与严格路径逐字节相同；只支持立体声的设备按配置失败拒绝，不静默复制；
  - 开启时 `[FL]` 单声道照严格规则播放；`[FC]` → 立体声 → `[FC]` 连成一段，立体声曲目逐字节不变，只有两首单声道记为已改变。
- **桌面播放器**（假后端，整包与两个种子的短写）：开启时设备为双声道，字节等于 data 块每 2 字节重复一次；实时行、报告与结论文字正确，退出码 0；关闭时按 `[FC]` 严格播放。
- **WASAPI 假后端**：掩码包含（0x3F、0x4 对单声道）、缺少（0x3 对单声道、0x4 对立体声）、属性缺失、掩码 0 六种情形，提示与否正确，且都照常播放、设备字节与 data 块相同。ALSA 假后端报告“不可查询”。
- **嵌入式宿主仿真**（虚拟时间，FAT 镜像，只支持双声道的板）：
  - 严格：单声道 WAV 与 AIFF 按“声道不支持”拒绝，只播放立体声曲目；
  - 串口 `mono-to-stereo on` 后三首都播放，与独立参考逐字节一致，通知、计数与状态行正确；
  - 播放中 `mono-to-stereo off`：正在播放的单声道照复制播完，之后的单声道按严格规则拒绝，无输出错误。
- **CLI**：`--mono-to-stereo` 解析测试。
- **其他**：
  - QEMU 演示 Full 与 Lite 照常通过（演示本身未开启复制）。thumbv7em release：Full `.text` 93 600 字节、`.bss` 365 736 字节不变、栈高水位 11 852 字节；Lite `.text` 80 204 字节、`.bss` 91 068 字节不变、栈高水位 8 468 字节。与 F2 记录相比 `.text` Full −4 536、Lite +1 460 字节，变化来自编译器对改动后 pack 循环的内联取舍，未逐项分析；
  - no_std（thumbv7em）构建与 Clippy、`qy-board`（stm32f411）编译、Windows 目标（`x86_64-pc-windows-msvc`）的 `qy-host-wasapi` 与 `qybit` Clippy 通过；Windows 上的测试由 CI 运行。

## 5. 顺带修正（F2 遗漏）

嵌入式 FAT 存储只收录扩展名 `WAV` 与 `FLA` 的文件，AIFF 文件不会进入播放列表。现在同时收录 `AIF`（`.aiff` / `.aifc` 的 8.3 短名），嵌入式仿真中的 `C.AIF` 覆盖这一点。

## 6. 未运行与限制

- **Windows 真机**：`--mono-to-stereo` 试听与物理扬声器掩码的实际值需要用户运行，记录报告中的掩码与提示；
- **ALSA 真机、MCU 实物**：未运行；
- **不做**：多声道下混、上混与任意声道重映射。
