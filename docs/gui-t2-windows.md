# GUI T2：播放控制、线程模型与 Windows 原生前端

日期：2026-10-15。任务卡：[tasks/GUI.md](../tasks/GUI.md) §T2。选型见 [gui-t1-selection.md](gui-t1-selection.md)。

提交：bb96a2e（T2-A 播放器层控制）、c589876（T2-B `qy-ui` 界面模型）、9f64979（T2-C `qybit-gui` Windows 前端）。

## 1. 结构

```
qybit-gui（Win32 原生控件；只做输入 → 模型操作、模型变化 → 控件显示）
  └─ qy-ui（与平台无关的界面模型，纯安全 Rust）
       ├─ 播放线程：qy-player::play_with（与 CLI 相同的核心引擎、自有解码、pack、独占后端）
       └─ 文件头线程：读取格式与时长、扫描文件夹
```

- GUI 不另建音频路径，不复制播放逻辑；暂停、上一首、下一首、跳曲都由 `qy_core::engine` 执行（M3 T1 起已有）。
- 渲染线程规则不变：暂停时送静音包 / 静音写入（`static` 静音缓冲，不分配），不解码、不 pack、不阻塞。

## 2. 播放器层（`qy-player`，T2-A）

- `Command { Pause, Resume, Next, Prev, Goto(k), Stop }`，经 `PlayControl::commands`（mpsc 接收端）送入；CLI 不传（`None`），行为不变。
- `Status` 快照（是否活动、是否暂停、当前曲目与已播放帧、源格式、设备格式、欠载次数与帧数、暂停静音帧数），由生产线程每 100 ms（`STATUS_INTERVAL`）回调一次，状态变化时立即回调。
- 暂停：引擎发出 `PauseDevice` / `ResumeDevice`，渲染端送静音并单独计数（`paused_frames`），不计为欠载；暂停期间不从队列取帧。尚未启动的设备在暂停时不启动，恢复后才启动。排空阶段不响应暂停，保证剩余音频送完。
- 排空改为非阻塞（`begin_drain` / `poll_drain`），排空期间仍处理命令与状态。
- 报告新增：暂停送出的静音帧数（注明不是欠载）；按操作切换曲目次数（被切走的曲目没有放完，不记为完整播放）。

## 3. 界面模型（`qy-ui`，T2-B）

- **播放列表** `Entry { id, path, probe }`：`probe` 为 待读取 / 已读取（种类、格式、文件头声明的帧数）/ 失败（原因）。文件夹递归扫描，按扩展名（wav、wave、flac、aif、aiff、aifc）筛选并排序，深度上限 32。
- **文件头读取**在后台线程用与播放相同的 `AutoDecoder` 解析到格式确定为止（每次读 64 KiB，上限为解码器所需窗口与 16 MiB 中较小者）；时长来自文件头声明（WAV 数据块长度、AIFF COMM 帧数、FLAC STREAMINFO 总样本数，为 0 时显示未知），不扫描全文件。界面线程不做文件 I/O 与解码。
- **会话**：每次开始播放是一个会话，带编号；事件按会话编号过滤，停止或换会话后旧会话的状态、日志与结束结果不显示为当前状态（核心的代次规则之上再加一层）。会话内跳曲用 `Goto`，不重开设备。
- **事件通道**无界，界面线程卡顿不会阻塞播放线程；后台线程有新事件时调用通知回调（Windows 为合并后的 `PostMessageW`）。
- **显示文字**（`text`）：源格式 → 设备格式、已播放 / 总时长、欠载与暂停计数、保真行（严格路径完整 / 声道已改变 / 有欠载或错误）、每次播放结束的结果行。不出现 “bit-perfect”。
- 结束结果 `Outcome` 记录：是否放完、是否被中断、设备错误、软件路径完整、严格路径完整、欠载次数、按操作切歌次数、错误文字。

## 4. Windows 前端（`qybit-gui`，T2-C）

- 窗口：工具栏（播放/暂停、停止、上一首、下一首、添加文件…）、输出设备下拉框（第 0 项为默认设备并显示其名称，其余为枚举到的端点）、“单声道复制到左右两路”复选框（默认关闭，tasks/O1.md）、虚拟 ListView 播放列表（列：正在播放标记、#、文件、时长、格式）、日志框、两栏状态栏（状态行、保真行）。
- 菜单：文件（添加文件、添加文件夹、移除选中 `Del`、清空列表、退出）、播放（播放/暂停 `Space`、停止、上一首、下一首）、帮助（关于）。
- 操作：双击或回车从该行播放；Delete 移除选中；拖放文件或文件夹到窗口；命令行参数加入播放列表。设备与单声道选项用于下一次打开设备。
- 系统文件对话框（`IFileOpenDialog`，UI 线程 COM STA）；端点枚举在辅助线程（MTA）完成。每显示器 DPI v2；MSVC 构建内嵌 comctl6 清单（build.rs）。GUI 子系统，不弹控制台。
- 没有共享模式、重采样或音量选项；设备不支持格式时显示 WASAPI 后端的明确错误。
- 依赖：只用已在项目中的 `windows` 0.62.2（增加 Gdi、Controls、Shell 等 feature），没有新增第三方 crate。

### 4.1 unsafe 审计

- `unsafe` 只在 `crates/qybit-gui/src/win/sys.rs`（文件级 `#![allow(unsafe_code)]`）；`app.rs`、`qy-ui`、`qy-player` 没有新增 `unsafe`。
- sys.rs 中 30 行涉及 `unsafe`，每个块附 `SAFETY` 说明（22 处，相邻调用共用一条）。主要边界：
  - 窗口过程只在 UI 线程运行；界面状态放在 `thread_local`，后台线程只经 `PostMessageW` 通知（窗口句柄与合并标志为原子量）。
  - `App` 处理输入返回 `Op` 列表，在 `RefCell` 借用结束后才执行控件调用，避免窗口消息重入时重复借用。
  - `WM_NOTIFY` 只在来自播放列表控件的通知上按通知码转换结构体指针；`NMLVKEYDOWN` 按 `read_unaligned` 读取（该结构按 1 字节对齐打包）。
  - `WM_DROPFILES` 按 `DragQueryFileW` 查询的长度分配缓冲，处理后 `DragFinish`。
  - 文本缓冲以 NUL 结尾的 UTF-16 传入，生命周期覆盖调用。

## 5. 验证

### 5.1 自动测试（模拟后端，任何平台）

`qy-player`：
- `goto_next_and_prev_restart_from_target_track`：跳曲、下一首、上一首从目标曲目开头播放，被切走的曲目计入 `user_skips`，报告不再标为软件路径完整。
- `pause_and_resume_keep_bytes_and_count_silence_separately`：暂停/恢复后送到设备的有效字节与不暂停时逐字节相同，静音单独计数，欠载为 0。
- `pause_before_start_defers_device_start`：设备启动前暂停，恢复后才启动。
- 渲染循环：`pause_sends_silence_packets_without_consuming_queue`（整包后端）、`pause_writes_silence_without_consuming_queue`（短写后端）。

`qy-ui`（`crates/qy-ui/tests/ui.rs`，8 项，连续 5 次运行稳定）：
- `probe_reads_format_and_declared_frames`：文件头读取结果与各解码器测试的 `.meta` / `.expect` 参考一致。
- `folder_scan_is_recursive_filtered_and_sorted`。
- `session_controls_follow_engine_rules`：开始、暂停、恢复、下一首、上一首、跳曲、停止的状态序列。
- `ui_stall_does_not_cause_underrun`：界面线程两次各停 1 秒，欠载为 0。
- `stop_then_new_session_ignores_stale_events`。
- `device_failures_are_reported`：播放中设备失效时结束结果记为设备错误，状态行为“输出设备错误：播放已停止”；无法打开设备时状态行给出具体原因。
- `dropping_ui_while_playing_stops_threads`：播放中关闭界面，线程全部结束。
- `text_helpers`。

`qybit-gui` 的 `app.rs` 在 Windows CI 上做 Clippy 与编译检查。

### 5.2 CI 中的 Windows 运行（`gui-windows` 作业，第 89 次 CI 运行，提交 9f64979）

runner 没有音频设备，也没有 GPU；只验证窗口能启动、读取文件头、空闲与正常关闭，**不验证播放**。

- release `qybit-gui.exe`：0.71 MiB（单文件，不需要额外运行库）。
- 以 WAV、FLAC、AIFF 测试文件和一个非音频文件为参数启动，5 秒后测 20 秒空闲：进程 CPU 时间 0 ms，工作集 20 MiB。
- `CloseMainWindow` 后正常退出，退出代码 0。
- 截图写入日志与构建产物 `qybit-windows`（与 `qybit.exe` 一起）；截图内容的逐项核对放在 T3。

## 6. 真机运行（用户，2026-10-15）

用户在 Windows 上运行 CI 产物 `qybit-gui.exe`（WASAPI 独占，与 CLI 真机记录同一端点），报告播放、控制与显示基本正常。

- 48 kHz / 16 位单声道 WAV 在默认设置（严格路径，“单声道复制到左右两路”未勾选）下打开失败：“端点在独占模式下不接受单声道 [FC]（同采样率的立体声可用，见声道探测）”，4 个单声道请求均为 `AUDCLNT_E_UNSUPPORTED_FORMAT`（0x88890008），声道探测得立体声接受 32 000、44 100、48 000 Hz。
  - 这与 O1 的真机记录一致（[o1-mono-to-stereo.md](o1-mono-to-stereo.md) §6）：此前该文件是在 CLI 中开启 `--mono-to-stereo` 后播放的；GUI 的同名选项默认关闭，严格路径按规则拒绝，不自动开启。
  - 改进：报告中的提示原来只写命令行参数 `--mono-to-stereo`，在 GUI 里看不出对应哪个选项。现改为与前端无关的写法：“可明确开启‘单声道复制到左右两路’（命令行 --mono-to-stereo，图形界面同名复选框……）”；源格式行改为“（单声道复制到左右两路：按 2 声道 [FL FR] 请求设备）”。
- 用户第二次运行（7e607c6）：仍打开失败，日志与第一次相同。日志中源格式行没有“单声道复制到左右两路：按 2 声道请求设备”，4 个请求都是 1 声道，说明这次播放按严格路径进行，选项没有作用到这次播放。从界面代码看，复选框状态只在开始新的播放时读取；日志也没有写明这次播放用的是哪个设置，用户无从核对。修正（未在真机上复核）：
  - 每次开始播放时日志第一行写明选项状态：“单声道复制到左右两路：开启（……）”或“关闭（严格路径；设备不接受单声道时该曲目无法播放）”。
  - 播放中勾选或取消：选项同时发给进行中的播放（`qy-player` 新增 `Command::MonoToStereo`，引擎 `set_channel_policy`），日志写“已开启，重新播放或切换曲目后生效（已预读的下一首按原设置）”。引擎对之后开始解码的曲目使用新设置；桌面队列较大，自动播放的下一首通常已解码，所以只保证重新播放或切换曲目（含双击该曲目）后生效。
  - 保真行改按实际播放计算：`Status::channels_altered` 为本次播放已按单声道复制送到设备的曲目数（与报告一致）；原来按复选框当前状态推断，播放中改动选项时会显示错误。
  - 工具栏中设备下拉框改窄（320 → 260，96 DPI 下的像素），复选框文字在默认窗口宽度下完整显示。
  - 设备拒绝格式时整个播放停止（引擎现行规则，未改）：单声道曲目在严格路径下打开失败后，列表中之后的曲目也不再播放。
  - 测试：`qy-player` `mono_to_stereo_command_applies_to_later_tracks`（不接受单声道的假协商：严格路径打开失败并停止；播放中开启并跳到单声道曲目，设备字节等于每样本两份的独立参考）；`qy-ui` `mono_to_stereo_option_reaches_sessions`（默认关闭失败且日志写明关闭；勾选后重新播放按复制播放、保真行写明声道已改变；播放中勾选并跳到该曲目）。
- 勾选后播放该文件：待用户在真机上确认。

## 7. 未运行 / 未完成

- GUI 的逐项真机记录（暂停、跳曲、各状态显示与报告字段对照）：T3。
- 有 GPU 的真实机器上的空闲与播放时 CPU：未测，T3 真机运行时记录。
- Linux 前端（GTK4）：Linux 阶段。
- 定位（seek）：用户决定以后再做。
