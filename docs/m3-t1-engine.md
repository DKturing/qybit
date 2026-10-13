# M3 T1：核心下沉记录

日期：2026-10-13。任务卡：[tasks/M3.md](../tasks/M3.md) T1。本卡不需要硬件；实机部分属于 T2/T3，未运行。

实现：
- `crates/qy-core/src/engine.rs`：`no_std`、无 alloc 的播放调度引擎；
- `crates/qy-pcm-format/src/queue.rs`：队列接口 `FrameSink`/`FrameSource`、队列预算公式；
- `crates/qy-pcm-format/src/static_queue.rs`：静态帧队列（`bbqueue`，`static-queue` 特性）；
- `crates/qy-codec-auto/src/profile.rs`：配置档与 RAM 预算；`wav`/`flac` 特性；
- `crates/qy-host-sim`、`crates/qy-player`：改用引擎，删除重复的播放规则；
- `crates/qy-mcu-demo`：thumbv7em 演示程序（不在主工作区，单独构建）；
- 测试：`qy-host-sim/tests/sim_engine.rs`（新场景）、`tests/mcu_demo.rs`（QEMU 核对）、
  `qy-pcm-format/src/static_queue/tests.rs`、`qy-codec-auto` 配置档测试、`qy-codec-flac/tests/buffers.rs` 窗口边界测试。

## 1. 引擎放在哪里

放在 `qy-core` 的新模块 `engine`，不另建 crate。理由：

- 引擎包住 `Core`，要用 `read_target`、`pcm`、`Fault` 等核心类型，约束也与核心相同（`no_std`、无 alloc、不访问文件/线程/时钟）；
- 冻结的 `Action` / `Event` / `Decoder` 一个字都没改，引擎只在外面新增一层；
- 少一个 crate，依赖方向不变：codec 依赖 core，core 不依赖 codec。

## 2. 引擎接口与规则

宿主只做两件事：执行引擎给出的 `HostAction`，用 `HostEvent` 回报结果。`Engine::poll()` 返回 `Action`（要执行的操作）、`Wait`（在等读取、等队列空间或等设备）、`Yield`（已连续推进多步，可立即再调用）或 `Idle`。

| 宿主操作 | 宿主回报 |
| --- | --- |
| `Open` / `Read` / `Close`（核心动作原样转交） | `Opened` / `OpenFailed`、`ReadDone` / `ReadEof` / `ReadFailed` |
| `SelectDevice`（`Identical` 策略：宿主为源选择设备格式） | `DeviceSelected` / `DeviceUnsupported` |
| `OpenDevice`（打开设备并按设备格式重配置队列） | `DeviceOpened` / `DeviceOpenFailed` |
| `StartDevice`、`DrainDevice`、`StopDevice`、`PauseDevice`、`ResumeDevice` | `DeviceDrained`（排空完成） |
| `TrackSkipped`、`Finished`、`Failed` | — |

从 `qy-player` 迁入的规则：

- **预填**：已入队帧达到 `Capabilities::prefill_ms` 或队列已满才启动设备；
- **设备复用**：`ReusePolicy::Accepts`（真实后端）在当前设备格式能无损承载新源时继续用；`Identical`（模拟宿主）由宿主为每个源选格式，完全相同才继续用；否则先排空再重开；
- **排空**：EOF、失败与换格式前都先播完已入队的有效音频；
- **背压**：队列满返回 `Wait::QueueFull`；
- **设备错误**：`device_lost()` 停止核心，不再排空。

新增规则：

- **能力检查**：在回报 `Configured` 之前按宿主声明的 `Capabilities` 检查源格式，超出时给出具体原因，例如 `采样率 192000 Hz 超出本机配置的上限 96000 Hz`、`本机配置的队列不足以承载该格式：需要 1152000 字节，只有 768000 字节`，不打开设备；
- **暂停/恢复**：已启动的设备收到 `PauseDevice`，输出静音，静音单独计数，不算欠载；暂停时尚未启动的设备在恢复之前不会启动；
- **上一首/下一首**：相对**正在播放**的曲目。引擎记录队列里各曲目第一帧在累计入队帧中的位置（最多 8 个边界，满时暂停解码，等设备播完旧曲目），用“累计入队帧 − 队列中剩余帧”算出设备已播到哪一首；
- **出错策略**：`ErrorPolicy::Stop` 排空后停止；`Skip` 报告 `TrackSkipped` 后继续下一首，可跳过打开、读取、解码、设备不支持与能力不足；最后一首出错时仍然停止并报告 `Failed`。

实现中由测试发现并修正的三个问题：

1. **排空时序**：设备未启动时，引擎原先先发 `StartDevice` 再发 `DrainDevice`。桌面渲染线程会在 EOF 标志设置之前启动，把“不足一包”误判为欠载（`qy-player` 的 `input_faults_drain_queued_audio` 抓到）。改为 `DrainDevice` 本身负责启动设备。
2. **切歌位置**：原先“下一首”以正在**解码**的曲目为准。队列在 48 kHz 下能装 4 秒，解码会领先播放好几首（`sim_engine` 抓到）。改为按播放位置计算，见上。
3. **队列预算**：`rtrb` 的重配置检查写死了 500 ms，MCU 配置档用 250 ms 时会把合法格式拒掉。新增 `ring::reconfigure_for`，按配置档的预填时长核对。

## 3. 静态帧队列

**选型**：`bbqueue =0.7.0`（2026-03-14 发布，作者 James Munns）。2026-10-13 在 crates.io 核对过版本，下载源码审阅。

- 不选 `heapless::spsc`（=0.9.3）：它逐元素入队/出队，不能把一段连续内存交给 DMA；
- `bbqueue` 是 BipBuffer，读写授权都是连续切片，专为 DMA 设计；
- 关闭默认特性：只用 CAS 原子协调（`AtomicCoord`）与轮询通知，不引入 `maitake-sync` 和 `critical-section`。唯一的传递依赖是 `const-init`。

**`unsafe` 边界**（都在 `bbqueue` 内部，本项目代码没有 `unsafe`）：

- 存储 `Inline<N>`：`UnsafeCell<MaybeUninit<[u8; N]>>`，以零初始化，`ptr_len` 返回固定的指针与长度；
- 授权：`from_raw_parts(_mut)` 构造切片，互斥由 `write_in_progress` / `read_in_progress` 两个原子标志保证。重复授权返回 `GrantInProgress`，不会产生重叠切片；
- `Send` 实现与 `Sync for Inline`。

我们不用 `&'static mut [u8]` 作存储：那需要自己实现 `unsafe trait Storage`。队列存储改为随 `StaticFrameQueue<N>` 放在 `static` 中，链接后位于 `.bss`。这是对 AGENTS“大缓冲接收 `&'static mut [T]`”的一处有理由的偏离：存储仍然静态、一次性、类型不带生命周期参数。

**封装**（`static_queue.rs`）：

- `split()` 只能调用一次（原子标志），保证单生产者、单消费者；
- **生产端**：直接 pack 进写授权，不需要 pack 暂存区。尾部空间不足一帧时放弃该授权，改从头部授权；`bbqueue` 记录水位线，读端读到水位线就回到头部。因此每次提交和每次读授权都是完整帧；
- **消费端**：每次至多两次读授权，有界拷贝，不分配、不阻塞，可在 DMA 中断中调用。授权冲突返回 `RingError::Busy`，不自旋；
- **队列计数**：生产端独写“累计入队帧”，消费端独写“累计取走帧”，两者相减即队列中的帧数。只用原子 load/store，不需要读改写；
- **容量保证**：队列满时，尾部和头部的空闲段可能各不足一帧，因此数据预算之外多留 2 帧（`BIP_MARGIN_FRAMES`）；`reconfigure` 按“存储 − 2 帧”核对预算；
- **适用目标**：需要 CAS 原子（Cortex-M3/M4/M7）。Cortex-M0+（如 RP2040）需要 `bbqueue` 的临界区协调，届时另加。

**同一接口**：`FrameSink`（生产端：`push`、`queued_frames`）与 `FrameSource`（消费端：`available_frames`、`pop_frames`、`consume_contiguous`）位于 `qy_pcm_format::queue`。`rtrb` 与 `bbqueue` 两种实现都实现了这组接口，引擎对 `FrameSink` 泛型。

**测试**：

- 随机操作与按字节建模的参考队列逐字节比对：200 轮，每轮随机换帧宽（2/3/4/6/8 字节）后做 20–200 次随机入队、出队、部分提交；
- 断言：队列报满时已入队数据 > 存储 − 2 帧；交出的连续段都是整帧；计数与模型一致；
- 变异检查：把“尾部不足一帧时改从头部授权”去掉，测试以“过早报满 804 / 1000”失败；
- 重配置检查：两端不匹配、代次不递增、容量不足、帧宽不符时各自报错。

## 4. 资源预算与配置档

`qy_pcm_format::queue::queue_bytes(rate, millis, channels, container_bytes)` 计算队列须容纳的数据字节数；`Profile::budget()` 汇总各缓冲。都是编译期可求值的函数，FLAC 窗口来自 `Capacity::for_limits`（不压缩帧上界）。三个配置档共用同一套代码：

| 配置档 | 能力 | 队列 | 输入窗口 | WAV PCM | FLAC 工作区 + PCM | pack 暂存 | 合计 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Desktop | 采样率不设上限；双声道；≤ 32 位；预填 500 ms；`rtrb` | 3 072 000 | 1 048 576 | 32 768 | 1 048 560 + 524 280 | 32 768 | 5 758 952 |
| Embedded Full | ≤ 96 kHz；双声道；≤ 24 位（I²S 32 位槽位）；FLAC 块 ≤ 4608；预填 250 ms（暂定）；静态队列 | 192 016 | 28 244 | 32 768 | 73 728 + 36 864 | 0 | 363 620 |
| Embedded Lite | ≤ 48 kHz；双声道；16 位；只有 WAV；预填 250 ms（暂定）；静态队列 | 48 008 | 8 192 | 32 768 | 0 | 0 | 88 968 |

说明：

- **桌面队列**：采样率不设上限，队列按参考值 768 kHz / 双声道 / C32 / 500 ms 分配。更高的源由引擎按“队列不足”明确拒绝，而不是按采样率拒绝；
- **能力检查的估算**：按承载 N 位的最小容器估算队列需求。设备实际选更大的容器时，宿主在重配置队列时还会再查一次（`reconfigure_for` / 静态 `reconfigure`）；
- **MCU 预填 250 ms 是暂定值**，T2 按实测的 SD 卡最坏读取延迟加余量推出后回写；
- **与任务卡的差异**：任务卡把“FLAC 容量、输入窗口”列在 `Capabilities` 里。这两项改放在 `Profile`，因为 `qy-core` 不能依赖 codec（依赖方向），而引擎只需要采样率、声道、位深、队列与预填时长；
- **WAV PCM 工作区**固定为 `PCM_WORK_SAMPLES`（4096 帧 × 2 声道 × 4 字节），由 WAV 解码器的契约决定。它在 Lite 配置档中占很大比例，是以后可以缩小的点。

**去掉软件人为上限**（同属本卡）：

- 删除 `MAX_SAMPLE_RATE`（192 kHz）；`SourceFormat::new` / `DeviceFormat::new` 只拒绝采样率 0；
- `MAX_CHANNELS = 2` 保留，但含义改为当前实现范围：多于 2 声道的源明确报“尚未实现”（FLAC `ChannelsNotImplemented`；WAV `Unsupported`），见 tasks/FORMATS.md F1；
- **夹具**：
  - 删除 `err_unsupported_384k`（FLAC）与 `unsup_rate_384k`（WAV）；
  - 新增 `rate_384k_stereo_24.flac`（libFLAC 参考核对，覆盖 MID_SIDE 分支）与 `pcm16_stereo_384k.wav`（ffmpeg 第二参考）；
  - `err_unsupported_3ch` 的明细改为 `ChannelsNotImplemented`；
  - 其余夹具逐字节不变；
- **FLAC 窗口**：
  - 不压缩帧上界按 RFC 9639 逐项相加，测试中用独立写法核对：4608/2/24 → 28 244、4608/2/32 → 37 460、65535/2/32 → 532 492；
  - 新增窗口边界测试：输入窗口正好等于最大块不压缩夹具的帧长时解码成功，少 1 字节时返回 `FrameTooLarge`；STREAMINFO 声明最大帧长时在文件头拒绝，声明为 0 时在该帧处拒绝；
  - 该测试发现一处错误分类：调用方给出的输入超过窗口且已到 EOF 时，原先报 `Truncated`。现在窗口之外还有数据时一律报 `FrameTooLarge`。核心给解码器的输入从不超过窗口，因此播放路径的结果不受影响（1200 种子逐种子摘要不变）。

## 5. codec 按需链接

`qy-codec-auto` 新增特性 `wav`、`flac`，默认都开：

- 构造改为组装式：`AutoDecoder::empty().with_wav(..)?.with_flac(..)?`。两者都启用时，原来的 `new(..)` 仍然可用；
- 可识别但未链接或未启用的格式返回 `Unsupported`（`Codec::Disabled`），不会误交给其他解码器；
- 分发仍是静态的：用宏展开 `match`，没有 `dyn`；
- thumbv7em 上 `--no-default-features`、`--features wav`、`--features flac` 三种组合都能编译，Clippy 无告警；
- **链接产物核对**：MCU 演示的 Lite 版 ELF 符号表中 `qy_codec_flac` 符号为 0 个，Full 版为 9 个（LTO 内联后剩下的独立函数）。这项由 `mcu_demo` 测试自动断言。

## 6. 宿主改造

**`qy-host-sim`**：

- `SimHost` 持有 `Engine<AutoDecoder, FrameProducer>`，复用策略 `Identical`；
- 能力为 `SIM_CAPABILITIES`：采样率不设上限，队列沿用 768 000 字节，以保证回归结果不变；
- 新增 `with_profile`（Desktop / Embedded 配置档）。配置档未启用 FLAC 时不加入 FLAC 解码器；
- `Scenario` 新增 `on_error` 与 `controls`（暂停、恢复、上一首、下一首、跳转、停止）；
- `Report` 新增 `skipped`、`pause_silence_frames`、`controls_done`；有曲目被跳过时 `fidelity_intact()` 为假；
- `Report::digest()` 生成逐种子摘要：各设备段字节的哈希、欠载、故障与计数。

**`qy-player`**：

- `HostResources` 改为按配置档分配，桌面用 `DESKTOP`，队列从 768 000 字节扩到 3 072 000 字节；持有引擎，复用策略 `Accepts`；
- `play()` 只执行引擎的操作：同步读文件、经后端打开设备、启动/排空渲染线程；
- `PlaybackReport.fault` 的类型改为 `EngineFault`，新增 `skipped`。摘要用 `fault_text`，核心故障仍按原名显示（如 `Configure`），能力不足显示具体原因；
- **桌面渲染循环尚未实现暂停**（M0 范围外）。播放驱动不会发起暂停，收到暂停动作时记为协议错误。

WASAPI、ALSA 后端和 CLI 只做了类型适配（`fault_text`），行为没有变化。

## 7. 验收

| 项目 | 结果 |
| --- | --- |
| WAV 1000 种子、FLAC 1200 种子回归 | **逐种子摘要与改造前（`b9ba576`）一致**：1000/1000、1200/1200。摘要覆盖每个设备段的全部字节、欠载、故障与计数。FLAC 比对时旧代码使用新夹具集，正常曲目池取 ≤ 192 kHz（旧代码解析不了 384 kHz），因为错误夹具池少了 `err_unsupported_384k`。现在的 FLAC 种子测试按 `SIM_CAPABILITIES` 过滤正常曲目池 |
| M2 整包/短写假设备测试 | 全部通过：`qy-player` 17 项、`qy-host-alsa` 11 项与零分配测试、`qy-host-wasapi` 6 项。期望值未改，只把故障比较包上 `EngineFault::Core` |
| A 层回环（alsa-lib `file` 插件） | 通过：88 项逐字节一致、40 项被拒绝，与 M2 T4 相同 |
| 新增模拟场景 | 7 项，各用种子 1–4 × 两种设备模型（WASAPI 整包 / 短写），读取含零长度读、1–16 字节短读、≤ 20 tick 慢读：<br>• 暂停/恢复：2 × 1.5 s，600–1100 ms 与排空中 2900–3400 ms 各暂停 500 ms，字节一致，WASAPI 暂停静音 2 × 50 包 × 480 帧，不计欠载；<br>• 暂停在设备启动前：恢复后才启动；<br>• 下一首 / 上一首（含换格式）、最后一首时下一首即停止；<br>• 12 首 120 ms 短曲目：边界上限触发背压，输出完整；下一首落在正在播放的曲目之后；<br>• 出错跳过：打开失败、文件头损坏、设备不支持各跳过一次，最后一首出错时停止；<br>• Embedded Full 能力超限：192 kHz 与 32 位在打开设备前被拒绝；<br>• 384 kHz：桌面配置档播放 WAV 16 位与 FLAC 24 位，逐字节一致；默认模拟队列下 384 kHz / 24 位被明确拒绝（需要 1 152 000 字节）。<br>两处变异检查：切歌改回按解码位置、去掉边界上限，测试都会失败 |
| no_std 编译 | 引擎、静态队列与各特性组合都能编译到 thumbv7em，Clippy 无告警；已加入 CI |
| Windows 目标（wine） | 93 项通过（补充验证，不是原生 Windows；原生 Windows 由 CI 运行） |

### MCU 资源演示（QEMU 指令集仿真，不是实机）

`crates/qy-mcu-demo`：thumbv7em 的 `no_std` 二进制，`cortex-m-rt =0.7.5`（`paint-stack`）+ `cortex-m =0.7.7` + `cortex-m-semihosting =0.5.0`，`static_cell =2.1.1`。

- 所有大缓冲都是静态的：`ConstStaticCell` 与静态队列；
- **“SD 卡”**：编译进闪存的夹具，每次读 512 字节；
- **“DMA”**：每个周期从静态队列取至多 256 帧；
- 设备格式按 I²S 32 位槽位：N ≤ 16 用 16/16，否则 32/24；
- 出错策略 `Skip`，按串口命令脚本暂停/恢复一次。

宿主测试 `qy-host-sim/tests/mcu_demo.rs` 在 QEMU `mps2-an386`（Cortex-M4F）中运行 release 版，核对以下各项：

- 每个设备段的帧数与 FNV-1a 64 等于由独立参考（libFLAC `.s32le`、ffmpeg `.ffmpeg.s32le`）按 `s << (C − N)` 放置后的字节；
- 跳过原因、无欠载、暂停计数；
- 栈：经 QEMU 监视器导出 RAM，统计复位时涂上的 0xCCCCCCCC 剩余量。

| | Embedded Full | Embedded Lite |
| --- | --- | --- |
| 播放列表 | WAV 16、FLAC 16、CRC 错误 FLAC（播放已校验的 512 帧后跳过）、192 kHz FLAC（能力拒绝）、96 kHz 24 位 FLAC、48 kHz 20 位 FLAC、48 kHz 24 位 WAV | WAV 16、FLAC（未链接，按 Unsupported 跳过）、24 位 WAV（能力拒绝）、96 kHz WAV（能力拒绝）、8 kHz WAV |
| 设备段 | 44.1k/2/16、96k/1/32-24、48k/1/32-24，全部与独立参考一致 | 44.1k/2/16、8k/1/16，全部一致 |
| `.text` / `.rodata`（含“SD 卡”文件）/ `.data` | 36 024 / 225 296 / 0 字节 | 27 232 / 51 444 / 0 字节 |
| `.bss` | 365 736 字节（预算 363 620 + DMA 半缓冲 2 048 + 控制块） | 91 068 字节（预算 88 968 + 2 048 + 控制块） |
| 栈高水位（自 `_stack_start`，含复位与 `main` 栈帧） | 2 200 字节 | 1 880 字节 |
| `qy_codec_flac` 符号 | 9 | 0 |

`.data` 为 0：所有缓冲都零初始化，位于 `.bss`。栈小是因为大缓冲都是静态的。仿真只证明指令集层面能正确运行和链接，**不涉及时序**：

- DMA 是软件模拟的，只在队列满或排空时取数，因此“欠载 0”不是实时性结论；
- 实时余量、SD 卡延迟与 I²S 输出属于 T2/T3。

## 8. 未运行与未完成

- **实机 MCU**：未运行（T2/T3，需要用户确定开发板并在本地运行）。QEMU 结果不能代替实机，也不证明 DAC 输出。
- **桌面暂停**：引擎和模拟宿主已支持，但 WASAPI/ALSA 渲染循环尚未实现暂停（CLI 不提供暂停）。
- **静态队列**：尚不支持没有 CAS 原子的目标（Cortex-M0+）。
- **多声道**：F1，声道映射尚未实现。
- **真实设备测试**：WASAPI/ALSA 真机测试与回环 B 层（snd-aloop）仍未运行，原因同 M2。
