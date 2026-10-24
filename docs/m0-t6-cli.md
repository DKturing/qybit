# M0 T6：CLI 与真机集成记录

## CLI（`crates/qybit`，二进制 `qybit`）

| 命令 | 说明 |
| --- | --- |
| `qybit list-devices` | 列出活动渲染端点：每行一个 `端点ID<TAB>友好名` |
| `qybit --device <端点ID> <文件.wav>...` | 先查询设备，不存在则明确失败；然后以 WASAPI 独占模式顺序播放并打印摘要 |
| `qybit matrix --device <端点ID> [--out <目录>]` | 运行 60 项格式矩阵（F1 起另加 10 项多声道，共 70 项，见 docs/f1-multichannel.md）和顺序换曲，写出 JSON/CSV |

- 播放必须用 `--device` 显式给出设备，不支持交互选择（AGENTS M0 范围）。
- 退出码：0 完整播放；2 播放完成但有欠载；1 失败（设备不存在、不支持该格式、独占打开失败、设备错误、文件错误）。
- 非 Windows 平台会明确报错不播放，ALSA 在 M2 实现。

摘要（`PlaybackReport::summary`）逐个输出流列出：

- 源格式（采样率、声道、有效位）；
- 每次独占请求的格式与描述类型、`IsFormatSupported` 与 `Initialize` 的 HRESULT、对齐重试、端点缓冲帧数、周期，以及最终协商成功的是哪一次；
- MMCSS 注册与撤销结果；
- 有效帧、尾部填充、排空静音、欠载（次数、帧数、逐次位置）和渲染结束状态；
- 打开失败、文件错误与曲目故障。

结论句只有三种：完整（并注明这“不证明设备端 bit-perfect”）、有欠载（“不是完整保真”）、未完成。

## 设备格式矩阵（`qy-host-wasapi::matrix`）

- **矩阵项**：6 个采样率 × 4 种 (C,V) × 单/双声道，描述类型为 EXTENSIBLE，共 48 项；16/16 再在每个采样率、单/双声道各测一次 WAVEFORMATEX，共 12 项。合计 60 项，顺序固定。
- **逐项流程**：
  1. 写一个 N = V 的 PCM 测试文件：300 ms、约 −30 dBFS、周期 96 样本的方波。
  2. 用只尝试该 (C,V) 和该描述类型的后端，经真实核心、解码与 pack 播放。
  3. 设备不支持时如实记录，不播放。
- **每项记录**：
  - 设备 ID、友好名、适配器名、驱动版本（读不到为 null）、系统版本（读注册表 `ProductName`/`DisplayVersion`/`CurrentBuildNumber`/`UBR`）、后端版本；
  - 描述类型与请求字段：`wFormatTag`、`cbSize`、`nSamplesPerSec`、`nChannels`、`wBitsPerSample`、`wValidBitsPerSample`、`nBlockAlign`、`nAvgBytesPerSec`、`dwChannelMask`、`SubFormat`；
  - `IsFormatSupported`、`Initialize`、对齐重试的 HRESULT，端点缓冲帧数、周期；
  - MMCSS 注册与撤销；
  - 播放结果：结束状态、有效帧与期望帧、尾部填充、排空、欠载；错误信息。
- **顺序换曲**：从成功播放的项中取两个不同采样率的 EXTENSIBLE 格式，连续播放两首。要求生成两个独立打开的流，各流有效帧数等于对应源帧数（无旧 PCM、尾帧不截断），都以排空结束且无欠载。
- **报告**：`qybit-device-matrix.json` 与 `.csv`。内容确定：不含时间戳，顺序固定；同一设备与系统重复运行，除设备行为本身外输出相同。JSON 中带说明“接口成功与播放统计只证明兼容与运行，不证明 DAC 输入样本逐位一致”。

本阶段默认输出仍用 EXTENSIBLE，没有加入运行时的描述类型回退。是否为 16/16 加入同参数的 WAVEFORMATEX 回退，要根据真机矩阵结果再决定。

后续（2026-10-14）：真机矩阵中出现过 16 位单声道只接受 `WAVEFORMATEX` 的驱动，先为标准单声道与立体声加入了该候选；随后真机上单声道 `WAVEFORMATEX` 被接受却没有可闻输出，现只对标准立体声使用，见 [m0-t5-wasapi.md](m0-t5-wasapi.md)“16/16 的 WAVEFORMATEX 候选”。

## 自动测试（不需要 DAC）

| 测试 | 覆盖 |
| --- | --- |
| `matrix::matrix_has_60_items_in_fixed_order` | 矩阵组成与顺序 |
| `matrix::requested_fields_independent_expected` | 96k/2/32/24 EXTENSIBLE 与 44.1k/1/16 WAVEFORMATEX 的请求字段与独立期望值一致 |
| `matrix::fake_matrix_records_capabilities_and_playback` | 假设备只支持 16/16、32/24 且最高 96 kHz：不支持项记录 HRESULT、不播放；支持项经真实路径完整播放，MMCSS 撤销；顺序换曲 44.1k→48k 产生两个流，帧数与源一致 |
| `matrix::report_is_deterministic_and_well_formed` | JSON 结构配平、字符串转义（中文、引号、逗号）、HRESULT 十六进制、确定性；CSV 34 列、引号转义、字段对齐 |
| `player::summary_reports_negotiation_and_conclusion` | 摘要包含源格式、请求与 HRESULT、MMCSS、欠载、结论措辞与退出码 |
| `qybit::args::*` | 子命令解析；缺少设备、缺少文件、未知参数时明确报错 |

## 真机测试状态

**未运行。** 当前开发环境没有 Windows 和音频设备，wine 下枚举不到端点。手动步骤见 [docs/development.md](development.md) 的“真机测试”一节，需要提供显式设备 ID。真机运行后，应把生成的 JSON/CSV 与设备、驱动信息一起归档。
