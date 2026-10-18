# X1：扩展接口、契约与接入方法

任务卡：[tasks/X1.md](../tasks/X1.md)。本文写给要给 QyBit 增加新格式、新音源、新输出或新存储的人（包括用 AI 辅助编写扩展的情况）：每个扩展点要实现什么、必须遵守哪些约定、怎样接入，以及怎样用契约测试证明扩展写对了。

扩展只在**编译期**接入：扩展是一个普通的 Rust crate，实现下面的 trait，在组装层（应用的 `main` 或板级代码）与已有组件组合，静态分发。不需要、也不允许为扩展改核心状态机。运行时动态插件（加载 `.dll` / `.so`、ABI 稳定、版本协商）不在当前范围，以后在桌面宿主外层另行设计，MCU 核心不承担它的成本（X1 §6）。不接入 FFmpeg 等外部解码器。按 2026-10-18 的决定，基础格式之外的格式（Lite：ALAC、AAC-LC、MP3；Full：再加 HE-AAC、WavPack、APE、Vorbis、Opus、DSD）由用户另建的插件仓库用纯 Rust 实现，按本文的接口接入，不合并进本仓库；本仓库为它们提供的公共接口见 [tasks/FORMATS.md](../tasks/FORMATS.md)“公共接口”（AGENTS §1）。

完整的可运行示例：[`examples/qy-ext-demo`](../examples/qy-ext-demo)。

## 分层（2026-10-17 用户确认）

| 层 | 包含 | 规则 |
| --- | --- | --- |
| **核心** | `qy-core`（sans-IO 核心与 `engine` 播放调度） | 不含任何格式：没有解码代码，也不按格式分支；依赖只有 `qy-pcm-format`（PCM 表示、pack 与帧队列接口，不涉及文件格式）。能力不足与播放计划只按资源描述，格式的叫法由解码器给出（B1）。 |
| **官方解码器** | `qy-codec-wav`、`qy-codec-flac`、`qy-codec-aiff`：基础格式（未压缩 PCM 与 FLAC，含 RF64/BW64 与 AIFF-C `twos`/`sowt`/`fl32`/`fl64`，见 [tasks/FORMATS.md](../tasks/FORMATS.md)） | 由项目维护，按规范自行实现，`no_std`、无 `alloc`；每个格式各自可选（`qy-codec-auto` 的 `wav` / `flac` / `aiff` 特性，`qy-player` 的 `flac` 特性）；在仓库内接受完整验证（独立参考逐样本核对、`qy-testkit` 契约测试、fuzz、`no_std` 目标编译，适用处 Kani 与 cargo-mutants）。默认发行版包含：桌面 CLI 与 GUI 默认链接 WAV、FLAC、AIFF；MCU 配置档按资源选择（Embedded Lite 不链接 FLAC）。 |
| **第三方扩展** | 仓库外的格式插件（包括以后自己写的纯 Rust 插件）、音源、输出后端、存储与外设适配 | 按本文的契约编写，在组装层（应用的 `main` 或板级代码）接入，不修改核心；用 `qy-testkit` 证明遵守接口约定。Lite / Full 插件仓库（ALAC、AAC、MP3、WavPack、APE、Vorbis、Opus、DSD 等，2026-10-18 决定）属于这一层。 |

宿主（`qy-player`、`qy-host-*`）与应用（`qybit`、`qybit-gui`、板级程序）不属于这三层：它们是组装层，把核心、官方解码器与扩展组合起来，只执行 I/O 与设备操作。

两层解码器用的是同一套接口（`Decoder` + `Resources`），核心对它们一视同仁；区别只在由谁维护、是否默认包含。官方解码器也可以不编译：关掉对应特性即可，核心不需要改动。

## 1. 扩展点

| 扩展什么 | 实现 | 所在 | 接入位置 |
| --- | --- | --- | --- |
| 新的音频格式 | `Decoder` + `Resources`（桌面另加一行 `Diagnostics`） | `qy-core`、`qy-player` | 组合解码器 → `HostResources::with_decoder`（桌面）/ `Core::with_window` + `Player::new`（嵌入式） |
| 帧队列 | `FrameSink`（生产端）/ `FrameSource`（消费端） | `qy-pcm-format::queue` | `Engine::new` 的 `sink` |
| 桌面输出后端 | `Backend` → `PendingStream` → `RunningStream`，渲染用 `PacketApi`（整包）或 `WriteApi`（短写） | `qy-player` | `play_with` |
| 桌面音源 | `TrackSource`（`Reader: Read + Seek`） | `qy-player` | `play_source` / `play_with` |
| MCU 存储介质 | `Storage` | `qy-host-embedded` | `Player::new` |
| MCU 输出外设 | `AudioOutput` + `FeederAccess` | `qy-host-embedded` | `Player::new`；DMA 中断调用 `Feeder::fill` |

播放规则（预填、续用、换格式排空、暂停、切歌、出错跳过、定位、代次）全部在 `qy_core::engine`，所有宿主共用。扩展只执行 I/O 与设备操作并如实回报，不实现播放逻辑。

## 2. 新格式解码器

### 2.1 写法

示例：[`examples/qy-ext-demo/src/qyt1.rs`](../examples/qy-ext-demo/src/qyt1.rs)（只为说明接口而定义的极简格式 QYT1，不是要支持的生产格式）。

1. **crate 设置**：`#![no_std]`、不用 `alloc`、不写 `unsafe`（工作区的 `unsafe_code = "deny"`）。构造函数接收宿主提供的 `&'static mut [i32]` 工作区（以及需要的其他 `&'static mut` 缓冲），检查容量，不足时返回 `DecodeError::CapacityExceeded`。不在解码器里嵌入大数组。
2. **推送式**：`decode(input, eof)` 只看传入的切片，不调用 `Read` / `Seek`，不等待。返回 `Progress { consumed, status }`；未消费的输入下次由核心原样再送（前面加上新读到的数据）。
3. **状态**（下表“`Decoder`”）。
4. **样本**：交错、右对齐的 `i32`，携带源有效位数 N；不经浮点中转、不缩放、不抖动。浮点源用 `SampleEncoding` 声明，按位传递（F3）。
5. **不可信输入**：所有长度、偏移与乘法都检查（`checked_*`、`get`），不对输入导出的值 `unwrap` / `expect` / 索引越界；出错返回 `DecodeError`，不 `panic`。
6. **资源**：`impl Resources for X {}` 即可（全部方法有默认实现）。格式有“文件头声明的需求超出本机容量”的情况时实现 `capacity_rejection` / `demand`，让播放前报告写明具体数值；只修正结构、不改样本的容错实现 `repair_count` / `repair`（W1）。
7. **桌面报告**：`impl qy_player::Diagnostics for X {}`；需要在报告里写失败明细时实现 `failure_details`。
8. **定位（可选）**：支持时实现 `seek(frame) -> Ok(字节位置)`；需要多次跳转（例如二分查找）时用 `seek_request`。不实现时核心自动顺序解码到目标并丢弃之前的样本，结果同样精确，只是更慢。

### 2.2 接入：组合解码器

核心只持有一个解码器类型（静态分发），所以新格式与已有格式在组装层组成一个枚举式的组合解码器：按文件头选择，之后把每个方法转给选中的那个。见 [`examples/qy-ext-demo/src/lib.rs`](../examples/qy-ext-demo/src/lib.rs) 的 `ExtDecoder`：

- `decode` 第一次被调用时看前 4 字节：是 `QYT1` 用示例解码器，否则交给 `AutoDecoder`（WAV / FLAC / AIFF）；不足 4 字节且源未结束时返回 `NeedInput`，不提前选择；
- `reset` 把两个解码器都复位，并清除选择；
- `format` / `pcm` / `release` / `seek` / `seek_request` / `Resources` / `Diagnostics` 转给选中的解码器。

**桌面**：

```rust
let decoder = ExtDecoder::desktop(true);          // 工作区一次性取得（Box::leak）
let window = decoder.required_window();
let mut res = HostResources::with_decoder(decoder, window, qy_codec_auto::profile::DESKTOP)?;
let report = qy_player::play_with(&mut backend, &mut res, &mut source, control);
```

`HostResources<D>` 对解码器泛型，默认 `AutoDecoder`，原有的 `HostResources::new` / `with_profile` 不变。

**嵌入式**（见 [`examples/qy-ext-demo/src/embedded.rs`](../examples/qy-ext-demo/src/embedded.rs)）：组合解码器交给 `Core::with_window`，再构造 `Engine` 与 `qy_host_embedded::Player`。`Player::new` 按每种启用格式的 `Load` 核对预填与持续负载，新格式要给出自己的 `Demand`（字段公开：最大字节率、最大输入单元字节数、单元至少包含的帧数）和板级实测的读取 / 解码耗时；时序不足时启动即报错（示例里第一次给的宽松耗时就被正确拒绝了：QYT1 的输入单元只有 512 字节，按 2 ms 计持续负载为 600%）。

### 2.3 用 AI 辅助编写格式扩展

把下面这些交给助手，比只说“写一个 XX 解码器”可靠得多：

1. **规范原文**（格式的官方规范或 RFC），要求关键实现处注明出处；
2. **本文 §2.1 与 §3 的契约表**，以及 `qyt1.rs` 作为写法样例；
3. **验收**：用 `qy-testkit` 的 `check` / `check_seek` 跑全部测试文件（§4）；样本与独立参考解码器（只在测试中使用）的输出逐个相同；截断、错误字段、超大声明值等坏文件不 `panic`、返回明确错误；`no_std` 目标编译（`cargo build --target thumbv7em-none-eabihf`）；
4. **禁止事项**：不用现成的生产解码器 crate 替代自己的实现，不转浮点，不在解码器里分配或读文件，不删弱测试。

示例 QYT1 本身就是这样被契约测试纠正的：坏样本出现在一批输入的中间时，最初的实现直接报错、丢掉了同一批里坏样本之前的好帧，于是一次性输入与逐字节输入在出错前输出的帧数不同；`check` 报告了“分块无关”违约，改为先输出好帧、下一次调用再报错后通过。

## 3. 契约表

### 3.1 `Decoder`（`qy-core`，M0 起冻结）

| 项 | 约定 |
| --- | --- |
| 缓冲所有权 | 工作区由宿主以 `&'static mut` 提供，构造时核对容量；`reset` 后复用，不重新分配。 |
| `pcm()` 的有效期 | 返回的切片在下一次 `release` 或 `decode` 之前有效；核心取走后调用 `release(frames)`。 |
| `Frames(n)` | `n > 0`，`pcm()` 至少有 `n` 帧。 |
| `NeedInput` | 只表示“缺输入”；`eof` 为真之后不得再返回。可以同时消费一部分输入（例如文件头）。 |
| `NeedOutput` | 工作区中还有未取走的样本（`pcm()` 非空），宿主取走后再调用。 |
| `End` | 只表示“正常结束”；之后再调用仍为 `End` 或错误，不再产出样本，不返回 `NeedInput`。零产出不能同时表示缺输入与结束。 |
| `consumed` | 不超过 `input.len()`。 |
| 格式 | 新建与 `reset` 之后 `format()` 为 `None`；文件头解析完成前不伪造；产出样本时必须已有格式；同一个源的格式不变。 |
| 有损标记（E1，2026-10-18） | 有损格式（MP3、AAC、Vorbis、Opus 等）的解码器必须用 `SourceFormat::lossy()` 标记格式：样本是解码重建的信号，`bits()` 是解码输出的精度，不是音源位深。报告、命令行、图形界面与嵌入式串口据 `is_lossy()` 写明“有损”。标记只影响报告，不改变设备选择与送出的数据；无损格式不调用。 |
| 分块无关 | 一次性、逐字节与任意分块输入，格式、样本与结束方式（正常结束或哪一种错误）完全相同。 |
| 错误之后 | 出错后在 `reset` 之前不再产出样本；`reset` 后能解码下一个源，结果与新解码器相同。 |
| 定位 | 核心只在文件头解析完成、首次产出样本之后调用 `seek`；之后只输出目标帧及之后的样本；目标等于或超过长度返回 `PastEnd`；不支持返回 `Unsupported`（核心改为顺序解码并丢弃）。核心在每次 `decode` 之后、看状态之前取一次 `seek_request`。 |
| 停止后迟到的回报 | 由核心按代次拒绝（§3.7），解码器不需要处理。 |

### 3.2 `FrameSink` / `FrameSource`（`qy-pcm-format::queue`）

| 项 | `FrameSink`（生产端，非实时线程） | `FrameSource`（消费端，渲染线程 / DMA 中断） |
| --- | --- | --- |
| 缓冲所有权 | 队列存储由宿主启动时取得一次（桌面 `rtrb`、MCU 静态 `bbqueue`），换格式时重配置帧宽，不重新分配。`scratch` 为宿主提供的 pack 暂存区，能直接 pack 进队列存储的实现可以不用。 | 同一队列的另一端。 |
| 部分完成 | `push` 只入队能放下的**完整帧**，返回入队帧数；其余样本留在解码器中，核心下次再送。 | `pop_frames` / `consume_contiguous` 只交出完整帧；`consume_contiguous` 按 `write` 返回的帧数提交，未写出的帧留在队列中，下次从同一位置继续（短写）。 |
| 返回 0 | 队列已满（引擎转为等待 `QueueFull`），不是错误。 | 队列暂时为空；渲染循环按后端规则补静音或不写，并记为欠载（运行中）或暂停 / 排空静音（另计）。 |
| 错误 | 返回 `Err` 时引擎以输出故障停止播放（`EngineFault::Output`），不重试。 | 返回 `Err`（未配置、帧宽不符、读授权冲突）时，桌面渲染循环按取到 0 帧处理；MCU `Feeder::fill` 整缓冲输出静音并计入 `OutputStats::errors`（正常运行应为 0）。 |
| 实时约束 | pack 是唯一的表示转换边界；只在生产端进行。 | 只做有界搬运与计数：不解码、不 pack、不分配、不阻塞。不得自行编写无锁缓冲，用成熟的 SPSC 库（AGENTS §2）。 |

`queued_frames()` 只用于预填与进度判断，消费端同时取数时结果只会偏大。示例 `tests/late_events.rs` 里的 `Recording` 是一个最简单的 `FrameSink`：pack 到内存、从不返回“队列满”。

### 3.3 `Backend`（`qy-player`）

| 项 | 约定 |
| --- | --- |
| 协商 | `open(source)` 只按源格式打开设备流，返回 `PendingStream`（含实际格式、全部尝试记录与提交方式）；不支持或独占打开失败时返回 `OpenFailure`，**不得**退回共享模式、不得转换音频。 |
| 缓冲所有权 | `PendingStream::start(consumer, ctl)` 接收帧队列的消费端；`RunningStream::join` 归还它（线程异常时为 `None`，宿主据此报错）。 |
| 整包（`PacketApi`） | `get_buffer(frames)` 与 `release_buffer(frames, 0)` 严格配对，每包帧数恒为端点缓冲帧数；不足部分由渲染循环补设备格式静音，播放中的不足记为欠载。 |
| 短写（`WriteApi`） | `write` 返回实际写入帧数（可少于所给），未写的帧留在队列中；`Avail::Xrun` / `Written::Xrun` 报告设备取空，渲染循环记为欠载后 `recover`。 |
| 返回 0 | `write` 写入 0 帧表示设备暂时没有空间，循环等待后重试，不重复提交。 |
| 错误 | API 错误立即结束渲染并写入 `RenderReport`，本段播放记录不再描述为完整保真；播放器停止该设备流。 |
| 参数确认 | 后端按平台定义“已应用参数”（ALSA 查询；WASAPI 不用共享混音格式冒充独占格式），无法查询时如实说明。 |

示例 `Recorder`（[`src/lib.rs`](../examples/qy-ext-demo/src/lib.rs)）按真实时间消费、不访问硬件，用整包渲染循环，记录送到“设备”的有效字节。

### 3.4 `TrackSource`（`qy-player`）

| 项 | 约定 |
| --- | --- |
| 读取 | `open(track)` 返回同步读取器；`read` 返回 0 表示结束，`Err` 记为读取失败（按出错策略跳过或停止）。 |
| 定位 | 不可定位的音源在 `seek` 中返回错误，播放器改为顺序解码到目标位置。 |
| 名称 | `name(track)` 只用于显示。 |

示例 `MemorySource`：内存中的曲目，可选不可定位（像管道）。

### 3.5 `Storage`（`qy-host-embedded`）

| 项 | 约定 |
| --- | --- |
| 打开 | 同时只打开一首；`open` 前已打开的先关闭。 |
| 读取 | `read(buf)` 返回读到的字节数，**0 表示文件结束**；`Err` 记为读取错误。每步至多读 `read_chunk` 字节（主循环对命令的响应延迟因此有界）。 |
| 定位 | `seek(offset)` 成功返回 `true`；不支持或越过文件末尾返回 `false`，引擎改为顺序解码（默认实现不支持）。 |
| 错误之后 | 读取错误后引擎按出错策略关闭该曲目；下一首重新 `open`。拔卡等全局故障由 `rescan` 恢复。 |
| 实时约束 | 在主循环中阻塞执行（SPI/SDIO），耗时计入预算公式的 `U` 与 `O`；DMA 中断照常取数。 |

示例 `Tracks`（[`src/embedded.rs`](../examples/qy-ext-demo/src/embedded.rs)）：内存中的曲目，可选不可定位。

### 3.6 `AudioOutput` + `FeederAccess`（`qy-host-embedded`）

| 项 | 约定 |
| --- | --- |
| 能力 | `caps()` 只列出时钟能**精确**产生的采样率与槽位格式；不能精确产生的不列出（否则改变了播放速度与音高）。 |
| 配置 | `configure(format)` 只在 DMA 停止时调用，只接受 `OutputCaps::select` 选出的格式；硬件做不到时返回错误，不得近似。 |
| 启动 / 停止 | `start` 启动循环 DMA；此后每个缓冲完成时，板级中断调用 `Feeder::fill`。`stop` 可重复调用，停止后在途缓冲不再播出。 |
| 缓冲所有权 | DMA 缓冲属于板级；`Feeder` 持有静态帧队列的消费端，主循环与中断经 `FeederAccess::with` 共享（MCU 上为临界区）。 |
| `fill` | 只做有界拷贝、静音补齐与计数；返回 `Fill { data_frames, silence_frames }`。缓冲长度不是完整帧时整缓冲静音并计错。欠载、暂停静音与排空尾部静音分别计数。 |
| 错误 | 配置或启动失败时宿主报告 `OutputError` 并停止播放。 |

示例 `VirtualI2s`：虚拟时间下的乒乓双缓冲，按缓冲周期调用 `Feeder::fill`，记录播出的有效字节；停止时在途缓冲丢弃。

### 3.7 停止后迟到的回报

每首曲目打开时与停止时代次递增。宿主在停止、切歌或定位后才完成的旧 I/O（打开、读取、结束、定位），回报给引擎时返回 `CoreError::Stale`，数据不会写入：`read_target(旧代次, …)` 返回 `None`。所以扩展（解码器、存储、音源、后端）都不需要自己跟踪“这个回报是否已经过期”，只要把动作里带的代次与请求号原样回报。示例 `tests/late_events.rs` 用假宿主验证：第 1 首的第一次读取先不回报，停止并改播第 2 首后注入第 1 首的打开、读取与结束回报，三者都被拒绝，第 2 首的输出与参考逐字节相同。

## 4. 契约测试：`qy-testkit`

只作 dev-dependency：

```toml
[dev-dependencies]
qy-testkit = { path = "crates/qy-testkit" }   # 仓库内为 qy-testkit.workspace = true
```

```rust
use qy_testkit::{Ending, SeekSupport, check, check_seek};

// 每个源分别用一次性、逐字节与随机分块输入解码，并用同一个解码器依次解码全部源（中间 reset）。
let decoded = check(|| MyDecoder::new(leak(4096)), &sources)?;   // Err(Violation) 写明哪一条、哪一步
assert_eq!(decoded[0].ending, Ending::End);
assert_eq!(decoded[0].words, reference_samples);

// 定位：支持时只输出目标帧及之后；不支持时返回 Unsupported（核心顺序解码）。
assert_eq!(check_seek(|| MyDecoder::new(leak(4096)), &file, &[0, 1, 1000, len, len + 1]),
           Ok(SeekSupport::Supported));
```

`check` 只证明解码器遵守接口约定，不证明解码正确：样本仍要与独立参考逐个比较（返回的 `Decoded::words` 就是用来比较的）。

已接入：WAV、FLAC、AIFF 的全部测试文件，`AutoDecoder`（三种格式混合、同一解码器复用），QYT1 与组合解码器 `ExtDecoder`。契约测试本身用一个正确的极简解码器和六种故意违约的变体（`eof` 后返回 `NeedInput`、文件头解析前给出格式、结果依赖分块、出错后继续产出、`reset` 后残留格式、定位后多输出目标前一帧）验证，每种都被发现（`crates/qy-testkit/tests/violations.rs`）。

## 5. 示例 crate 的验证结果（2026-10-17）

`cargo test -p qy-ext-demo`：

| 测试 | 内容 | 结果 |
| --- | --- | --- |
| `contract` | QYT1：1/12/16/24/32 位、单 / 双声道、0 帧，截断、魔数错、头部非法、样本越界；定位支持与不支持。组合解码器：QYT1 / WAV / FLAC / AIFF / 不足 4 字节混合 | 通过 |
| `desktop` | 真实引擎、pack、`rtrb` 帧队列与整包渲染循环，`Recorder` 按真实时间消费：QYT1 与 WAV、FLAC 混排（同格式三首共用一个设备流，另一格式另开）；暂停 / 恢复、切歌、停止；定位（直接、解码器不支持、音源不可定位）；解码错误停止并报告 QYT1 明细 | 通过，逐字节 |
| `embedded` | `qy_host_embedded::Player` + 静态帧队列 + `Feeder`，`Tracks` 与 `VirtualI2s` 在虚拟时间下：混排并按出错策略跳过坏的 QYT1（坏样本之前的帧照常播出）；暂停 / 恢复、下一首、停止；串口 `seek` 命令的直接定位与两种顺序降级（发出 `SeekSequential`）；无欠载 | 通过，逐字节 |
| `late_events` | 假宿主注入停止后迟到的回报（§3.7） | 通过 |

参考字节全部独立得到：QYT1 由测试生成样本后按 `s << (C − N)` 放置；WAV 取文件 data 块原始字节；FLAC 取 libFLAC 生成的 `.s32le`。不调用被测的 pack。

未运行：真实设备（桌面声卡、MCU 实物）；示例 crate 依赖 `std`（桌面部分），没有做 `no_std` 目标编译——`Qyt1Decoder` 只用 `core`，写法满足 `no_std`（同文件的测试辅助函数 `encode` 用 `Vec`），但没有单独验证。

## 6. 为示例改动的 `qy-*` 源码

X1 要求示例只用公开接口；需要改动时说明扩展点不完整。实际只有一处，在 X1-A（提交 `b373fd2`）：

- **`qy-player`：`HostResources` 不能接收自定义解码器**（B1 之后它固定持有 `AutoDecoder`）。改为对解码器泛型 `HostResources<D = AutoDecoder>`，增加 `HostResources::with_decoder(decoder, window, profile)`；`play` / `play_source` / `play_with` 对解码器泛型；报告中原来直接调用 `AutoDecoder` 的两处（失败明细、跳过的标签字节数）改为 `Diagnostics` trait，全部方法有默认实现。原有调用方的写法不变。这是 X1 §2 预先说明的组装层改动。

`qy-host-embedded`、`qy-core`、`qy-pcm-format` 与各解码器没有为示例修改：嵌入式 `Player` 本来就对解码器泛型，`Demand` 的字段公开，新格式可以自行声明需求。
