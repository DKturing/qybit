# M0 T5：WASAPI 宿主记录

实现：`crates/qy-host-wasapi/src/`

| 模块 | 平台 | 内容 |
| --- | --- | --- |
| `render.rs` | 全平台 | 渲染线程循环，基于 `RenderApi` / `ThreadPriority` trait，可注入假 API |
| `player.rs` | 全平台 | 生产端播放驱动（真实核心、WAV 解码、pack），基于 `Backend` trait |
| `wasapi.rs` | 仅 Windows | WASAPI 独占事件驱动后端与 MMCSS，平台 `unsafe` 集中于此 |
| `sdk_format.rs` | 仅 Windows | 纯格式描述到 SDK 结构的逐字段映射（T3） |
| `fake.rs` / `testutil.rs` | 仅测试 | 假 API、假后端、真实解码 + pack 供数 |

## 协商（`wasapi.rs`）

协商在渲染线程内进行：COM 以 MTA 初始化，用端点 ID 调 `IMMDeviceEnumerator::GetDevice` 取得设备。`IAudioClient` 等 COM 接口不是 `Send`，所以创建和使用都在同一个线程内。

1. **生成候选格式**（`player::candidates`）：从 (16,16)、(24,24)、(32,24)、(32,32) 中取 V ≥ N 的组合，V 小的优先，同 V 时容器小的优先。只列出 pack 能无损放置的表示。
2. **逐个尝试候选格式**：
   1. 每次尝试都 `Activate` 一个新的 `IAudioClient`；
   2. 以独占模式调 `IsFormatSupported`，`ppClosestMatch` 为空，不是 `S_OK` 就换下一个候选；
   3. `GetDevicePeriod` 取默认周期；
   4. `Initialize(EXCLUSIVE, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, period, period, fmt)`。独占事件驱动模式要求缓冲时长等于周期。
3. **对齐重试**：若返回 `AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED`，用 `GetBufferSize` 的帧数算出对齐周期 `round(10^7 × frames / rate)`，释放旧客户端，在新激活的客户端上重试一次。
4. **成功后**：
   - `CreateEventW` 创建自动复位事件，句柄由 RAII 关闭；
   - `SetEventHandle`；
   - `GetBufferSize` 取得端点缓冲帧数；
   - `GetService<IAudioRenderClient>`。

格式描述默认用 `WAVEFORMATEXTENSIBLE`。`WasapiBackend::descriptor_16` 可以让 16/16 改用 `WAVEFORMATEX`，供 T6 矩阵测试；运行时不会自动在两种描述之间回退。

明确不做的事：不用 `AUTOCONVERTPCM`、`SRC_DEFAULT_QUALITY` 等转换标志，不回退到共享模式，不调用 `GetMixFormat`。

协商记录（`Negotiation` / `Attempt`）保存：源格式、每次尝试的设备格式与描述类型、`IsFormatSupported` HRESULT、`Initialize` HRESULT、对齐重试 HRESULT、周期、端点缓冲帧数和最终选中的尝试。这些只是“请求与协商结果”，不代表设备端实际收到的样本。

## 渲染（`render.rs`）

- **启动**：先 `GetBuffer`/`ReleaseBuffer` 填第一包（初始端点缓冲），再 `Start`。
- **整包**：每个事件请求、释放的帧数都等于 `GetBufferSize` 的值，不按队列可用量缩小。`GetBuffer` 与 `ReleaseBuffer` 在同一线程内严格配对。
- **拷贝**：从队列按完整帧取数，环形回绕时最多拷贝两段；包内剩余部分写零字节，即有符号 PCM 的静音。释放标志恒为 0，从不用 `AUDCLNT_BUFFERFLAGS_SILENT`，因为部分有效的包一旦标 SILENT，其中的有效音频也会被静音。
- **欠载与尾部填充的区分**：每包先读 EOF 标志再取数据。生产端在推入最后一帧之后才置 EOF，所以读到 EOF 时该格式的全部数据都已可见。
  - 未确认 EOF 时数据不足，记为欠载，记录包序号、设备帧位置和缺失帧数。
  - 确认 EOF 后的不足是尾部填充（`tail_fill_frames`），不算欠载。
- **排空**：最后一包有效音频提交后，再提交 2 个静音包（`drain_frames`），等对应事件到达后 `Stop`，结束状态为 `Drained`。
- **停止**：收到停止请求时立即 `Stop`，不排空，结束状态为 `Stopped`。
- **错误**：API 错误或事件等待超时（2 s）时，先尽力 `Stop`，结束状态为 `Error(ApiError{op, hresult})`，例如设备失效 `AUDCLNT_E_DEVICE_INVALIDATED`。
- **MMCSS**：渲染线程自己调用 `AvSetMmThreadCharacteristicsW("Pro Audio")`，失败时记录 Win32 错误码。只要注册成功，无论正常结束、停止还是出错，都在本线程调用 `AvRevertMmThreadCharacteristics`，并记录结果。MMCSS 注册成功不能代替欠载测试。
- 循环只做有界拷贝、填零和原子计数，不解码、不 pack、不分配、不读文件、不等待 UI。

## 生产端（`player.rs`）

- 在调用线程（非实时）同步读文件，执行核心动作，pack 后入队。
- 队列已有 ≥ 500 ms 时启动渲染；播放结束或失败时也直接启动，把已有数据播完。
- 新源格式能被当前设备格式无损承载（同采样率、同声道、N ≤ V）时继续使用当前流，否则先排空当前流（置 EOF 并等渲染线程退出），再按新格式打开。
- 打开失败时回报 `ConfigureFailed`，没有任何回退。
- 播放失败时先排空已入队的有效音频再停。
- 渲染线程出错时，停止核心并报告 `device_error`。
- `PlaybackReport::software_path_intact()` 只表示软件路径完整：播放完成、无失败、无欠载、所有流都已排空。它不证明设备端 bit-perfect。

## 自动测试

所有自动测试都不需要 DAC：Linux 与 Windows CI 都跑，本地也在 wine 下跑了 Windows GNU 目标。

| 测试 | 覆盖 |
| --- | --- |
| `sdk_format::*`（仅 Windows） | 字段映射：结构大小、各字段、SDK 常量、内存字节 |
| `render::full_packets_eof_tail_and_drain` | 每包帧数等于端点缓冲帧数、请求与释放配对、首包在 `Start` 前、标志为 0、EOF 尾包填充、2 包排空、整包写满静音、MMCSS 注册与撤销 |
| `render::wraparound_copy_and_underrun_vs_tail` | 用最小队列（8 kHz 下 500 ms，8000 字节）多次回绕；人为停顿 3 次等待以制造欠载；部分有效包内的静音、欠载记录、有效字节与源逐字节一致且无重复；尾部填充与欠载分开计数 |
| `render::stop_request_*` | 停止请求：不排空，MMCSS 撤销 |
| `render::device_invalidated_and_timeout_*` | 设备失效与等待超时：报告 HRESULT、尝试 `Stop`、撤销 MMCSS、调用仍配对 |
| `render::mmcss_failure_*` | 注册失败时记录 Win32 错误码，不调用撤销，播放照常进行 |
| `player::same_format_tracks_*` | 两首同格式曲目共用一个流，无缝衔接 |
| `player::format_change_*` | 4 种格式依次排空、重开：44.1k/16/16、88.2k/24/24、96k 单声道 20 位放进 24/24、192k 的 32/24 源放进 24/24 容器 |
| `player::unsupported_format_*` | 32 位源而设备不支持 32/32：配置失败，不降精度；记录协商；之前的曲目完整播完 |
| `player::input_faults_*` | 打开失败、截断文件：已入队的有效音频（截断前 50 个完整帧）完整播完 |
| `player::device_invalidated_*` | 播放中设备失效：报告设备错误、MMCSS 撤销、消费端归还 |

解码和 pack 都走真实实现。player 测试还会核对假后端记录的完整调用序列。

变异检查（临时改动，跑完恢复），以下 7 项都会让测试失败：

- 部分包标 SILENT；
- 按可用量缩小请求帧数；
- 把欠载计为尾部填充；
- 出错路径不撤销 MMCSS；
- 先 `Start` 后填首包；
- 不排空直接停止；
- 换格式不排空。

## 真机测试（默认忽略）

```
set QYBIT_DEVICE_ID=<端点 ID>
cargo test -p qy-host-wasapi --test device -- --ignored --test-threads=1 --nocapture
```

`exclusive_playback_48k_16bit` 用独占模式播放 1 秒 48 kHz/16 位双声道的低电平方波。它要求：协商成功、排空结束、有效帧 48000、无欠载，并打印协商与 MMCSS 记录。未设置设备 ID 时明确失败。

**状态：未运行。** 当前开发环境没有 Windows 和音频设备；wine 下枚举不到端点，因为容器没有声卡。完整的格式矩阵和 JSON/CSV 报告在 T6。
