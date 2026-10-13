# M2 T1：平台无关播放器层记录

## 结构

新增 `crates/qy-player`（std 库）。原 `qy-host-wasapi` 中与平台无关的部分迁入其中，`render.rs`、`player.rs`、`testutil.rs` 用 `git mv` 迁移，保留历史：

| 模块 | 内容 |
| --- | --- |
| `lib.rs` | `HostResources`，启动时一次性分配，整程序复用，与 M1 相同 |
| `backend.rs` | `Backend` / `PendingStream` / `RunningStream` trait |
| `player.rs` | 生产端播放驱动 `play`，行为与 M1 相同 |
| `render/mod.rs` | 共用类型：`RenderControl`、`RenderReport`、`ApiError`（`ErrorCode`：HRESULT / Win32 / errno / 超时 / 缓冲长度）、`ThreadPriority`、`PriorityRecord` |
| `render/packet.rs` | 整包循环（原 WASAPI 渲染循环），API trait 为 `PacketApi` |
| `render/write.rs` | **新增**短写循环，API trait 为 `WriteApi`，供 ALSA `writei` 一类后端使用 |
| `report.rs` | 协商与播放报告，见下节 |
| `testing/` | `test-support` 特性：夹具与独立参考、整包/短写假 API、假优先级、可替换协商记录的 `FakeBackend` |

`qy-host-wasapi` 只保留以下 WASAPI 特有部分：

- `attempt.rs`：协商记录（HRESULT、`WAVEFORMATEX`/`EXTENSIBLE`）与格式候选；
- `matrix.rs`：格式矩阵；
- Windows 绑定（`wasapi.rs`、`sdk_format.rs`、`endpoints.rs`）。

CLI 改为从 `qy-player` 调用播放驱动。命令语法不变；非 Windows 平台仍明确报错，ALSA 在 T3 加入。

核心的 `Action`/`Event`/`Decoder` 没有改动。

## 两类提交语义

| | 整包（`packet`） | 短写（`write`） |
| --- | --- | --- |
| 提交 | 每包请求与释放固定帧数，`get_buffer`/`release_buffer` 严格配对 | 按 `avail` 写入；后端只接受一部分时，只提交实际写出的帧 |
| 队列不足 | 补设备格式静音：未确认 EOF 记为欠载，已确认 EOF 记为尾部填充 | 不补静音；设备取空由后端报告 xrun，记为欠载（写入序号、设备帧位置、缺失帧），`recover` 后重新预填、再启动 |
| 结束 | 再提交 2 个静音包，等待后 `stop` | 已确认 EOF 且队列已空时**立即** `drain`，不再先等设备 |
| 报告 | `Submission::Packet { frames }` | `Submission::ShortWrite { buffer_frames }` |

短写的零拷贝依赖新增的 `FrameConsumer::consume_contiguous`（`qy-pcm-format`）：
- 直接借出环形缓冲首段的连续完整帧，按后端实际写出的帧数提交；未写出的帧留在队列，下次继续。
- 回绕点切开一个帧时，把这一帧拼到栈上 8 字节临时区再交出。
- 全程有界、不分配、不阻塞。

## 协商记录的四项

各后端的尝试记录实现 `AttemptRecord`，报告和摘要把以下四项分开列出：

| 项 | 来源 | WASAPI |
| --- | --- | --- |
| 请求格式 | `requested()` | 请求的 (C, V) 与描述类型 |
| 协商结果 | `describe()` | 独占 `IsFormatSupported`/`Initialize` 的 HRESULT |
| 可查询的已应用参数 | `applied()` | 只有 `GetBufferSize` 的端点缓冲帧数；格式标为“不可查询”，**不以共享混音格式 `GetMixFormat` 代替** |
| 回录证据 | `StreamReport::evidence` | 目前均为“无（未回录）”，T4 填写 |

协商失败时，摘要不列已应用参数。

## 测试

迁移后原有测试全部保留，断言没有放宽：
- 整包循环 5 项、播放驱动 8 项、宿主资源 1 项，在 `qy-player` 中；
- WASAPI 协商 2 项、矩阵 4 项、Windows 绑定 3 项，在 `qy-host-wasapi` 中。

其中一处摘要措辞有改动：打开失败的提示由“独占打开失败”改为通用的“打开失败”。WASAPI 测试改为断言“打开失败”，并继续断言 `IsFormatSupported(独占) 0x88890008`。

播放驱动的 8 项测试现在对三种假设备各跑一遍：整包设备，以及种子为 1、2 的短写设备。三种设备上断言同一组期望字节：
- 同格式无缝衔接、换格式排空重开、不支持格式不回退、输入故障排空、设备失效；
- FLAC/WAV 混排（含 65535 帧最大块）、FLAC CRC 错误只播已校验帧。

短写设备额外要求：设备只播放真实写入的字节，没有任何填充；调用序列合法；最后一次调用为 `Drain`。

新增测试：

| 测试 | 内容 |
| --- | --- |
| `render::write::short_writes_commit_only_written_frames` | 20 个种子的随机短写，包括设备接受 0 帧；播放字节与源逐字节一致，无重复、无丢失、无填充 |
| `render::write::xrun_is_recorded_recovered_and_restarted` | 生产端停顿导致 xrun；欠载记录完整；每次 `recover` 后重新启动一次；有效字节仍完整 |
| `render::write::stop_error_timeout_and_empty_stream` | 停止不排空；写入错误与等待超时都报告错误、尝试停止、撤销优先级；空流不启动设备 |
| `ring::consume_contiguous_partial_commit_and_split_frame` | 部分提交、零提交、返回值截断、回绕切开帧时拼帧 |
| `player::summary_reports_four_categories_and_conclusion` / `attempt::summary_reports_negotiation_and_conclusion` | 四项分别出现在摘要中；WASAPI 已应用参数中格式标为不可查询 |

## 变异检查

检查后源码均已恢复。

| 变异 | 结果 |
| --- | --- |
| 短写时提交全部所给帧，而不是实际写出的帧 | 7 个测试失败 |
| 去掉“EOF 后立即排空” | 起初表现为挂起。给短写假设备加了“EOF 后反复空等则报告超时”的保护后，9 个测试明确失败 |

## 验证

- `cargo fmt --check` 通过。
- Clippy 全部通过：Linux workspace、`qy-player --features test-support`、Windows 目标（`qy-player`、`qy-host-wasapi`、`qybit`）。
- 测试全部通过：Linux 全 workspace；Windows 目标经 wine 运行，`qy-player` 17 项、`qy-host-wasapi` 9 项、`qybit` 2 项。
- 真机测试 3 项默认忽略，**未运行**，原因是没有 Windows 音频设备。
