# E6：解码需要的读取跳转

日期：2026-10-19。任务卡：[tasks/E6.md](../tasks/E6.md)（规则全文与版本决定）。本卡的结果来自单元测试、契约测试、桌面真实引擎、嵌入式虚拟时间与 QEMU，**不是实机结果**。

## 1. 问题与做法

索引在音频数据之后的容器（M4A 的尾置 `moov`）要先跳过数据读到索引，再按索引回到音频包；块交错的容器在数据阶段也要跳。原来解码器只能顺序接收输入，`seek_request` 只用于用户定位，用在解析阶段失败时会反复重开。

按用户 2026-10-19 选定的方案：**解码器决定需要哪些文件字节，核心调度读取与跳转，宿主只执行 I/O**。新方法 `Decoder::take_jump()` 给出下一批字节的位置；核心沿用 `Action::Seek` / `Event::SeekDone` / `SeekFailed`，自己记住是哪种跳转。桌面、模拟与嵌入式宿主不需要新的 I/O 代码，同一个解码器在桌面与 MCU 上都能用。主仓库没有任何格式专用逻辑。

## 2. 接口

| 项 | 内容 |
| --- | --- |
| `Decoder::take_jump(&mut self) -> Option<u64>` | 带默认实现（`None`）；取走即清空，`reset` 清除 |
| `JumpFault` | `Unseekable`（音源不可定位）、`NoProgress`（没有读到字节就再跳）、`Protocol`（两种跳转同时给出，或 `End` 时给出） |
| `Fault::Jump(JumpFault)` | 曲目失败原因（`Action::Failed` 的内容） |
| `JumpGuard` / `Jump` | 跳转规则的唯一实现，核心、`qy_player::read_header`、`qybit decode`、`qy-testkit` 共用 |
| `ParseJump` / `Core::take_parse_jump` | 最近一次解析跳转（位置、是否在首次产出样本之前） |
| `HostAction::ParseJumped { track, offset, header }` | 引擎在对应的 `Seek` 之后给出，宿主计数 |
| `budget::Load` 的 `header_jumps` / `unit_jumps` / `jump_us`，`Margins` 的 `seek_response_us` / `header_jumps` | MCU 时序预算（§4） |
| `Storage::seek` | 位置在文件末尾或之后时成功、之后读取返回 0（原来返回 `false`） |

规则的要点（全文见任务卡 §2）：

- 处理顺序：`consumed` → 预滚 → 取两种跳转并核对 → 无推进检查 → `seek_request`（位置不变）→ 用户定位优先（有待交的定位目标且本次产出了样本时先 `seek`，本次的解析跳转作废；`seek` 不支持时照常跳）→ 解析跳转（定位目标保持待交）→ 按状态继续。
- 跳转成功后输入窗口与 EOF 状态清除，下一次 `decode` 先得到空输入（`eof` 为假），再从新位置读取；跳到末尾之后得到 EOF。已产出、未取走的样本不受影响。
- 解析跳转的 `SeekFailed` 为 `Fault::Jump(Unseekable)`，不重新打开；用户定位失败保持原来的顺序解码回退。
- 防循环两层：核心检测“没有读到字节就再跳”；读到了字节却来回循环的由契约测试的跳转次数上限发现——这只是测试保护，生产环境不检测。
- E3 路由：`NeedMore { container: true }` 即认领容器，路由层提前转交；不表示子格式已经支持；`total` 可能大于输入窗口。
- `read_header` 的 16 MiB 上限按实际读取的字节累计（跳转后重复读取的也计入）。

## 3. 示例：尾部索引格式 QYI1

`examples/qy-ext-demo/src/qyi1.rs`：文件头给出索引位置，数据区是若干个 16 位 PCM 包（包之间可以有填充），索引在文件末尾。解码时读文件头 → 跳到索引 → 读完并核对整个索引才给出格式 → 跳到第一个包 → 顺序解码，下一包不紧接时在产出当前包最后一批样本的同一次调用中跳过去。索引表放在宿主提供的静态切片中，超出容量明确报错。识别函数只看前缀：不含索引时回答 `NeedMore { container: true }`。`ExtDecoder` 把它接在 QYT1 之后，转出的跳转位置加回 ID3v2 标签长度。

## 4. MCU 时序预算

`qy-host-embedded::budget` 增加三项：`J`（首次输出之前至多几次跳转）、`k`（数据阶段每个输入单元之前至多几次）由格式声明，`S`（定位一次的最长耗时，含 FAT 簇链查找）由板级实测；官方格式都是 0，原来的结果不变。

- 持续负载：`R · (U + k·S) / W / 10⁶ + D · fs / F / 10⁶ ≤ 1`；
- 预填：`P ≥ O + J·(S + U) + U + k·S + D + T`；
- 定位响应（只报告）：`O + J·(S + U) + S + U + D`。

运行时，一首曲目首次输出之前的跳转超过声明的 `J`（各格式最大值）时给出 `Notice::JumpBudgetExceeded` 与串口 `EVENT jump-budget-exceeded`；`k` 只按声明计入公式，运行时不核对。`stats` 行增加 `parse_jumps` 与 `jump_over`。

## 5. 验证（2026-10-19，开发机）

| 项 | 内容 | 结果 |
| --- | --- | --- |
| 核心（`crates/qy-core/tests/jumps.rs`，10 项） | 脚本解码器核对每次输入都等于它所在位置的字节：尾置索引流程在 1/7/64/4096/整块读取下逐字节正确；跳转清除旧位置的 EOF；跳到末尾之后得到 EOF、不再读取；不可定位时 `Unseekable` 且只打开一次；`NoProgress`（同一位置、打开后未读即跳、末尾之后再跳）；`Protocol`（两种同时、`End` 时）；用户定位等待文件头跳转并优先、样本从目标处开始；不支持定位时照常跳转并顺序丢弃；停止后旧代次的定位回报被拒绝、复用正确；`JumpGuard` 规则 | 通过 |
| 契约测试（`crates/qy-testkit/tests/jumps.rs`） | 正确的“索引在后”解码器通过 `check` / `check_seek` / `check_identify`；`reset` 后残留、`End` 时跳转、两种同时、未读即跳、来回循环五种违约各自被发现 | 通过 |
| 现有解码器 | WAV / FLAC / AIFF / QYT1 / QYD1 的全部契约、定位与识别测试在新的驱动（跳转后先送空输入）下不改动地通过，FLAC 二分定位照常 | 通过 |
| QYI1 契约与识别（`examples/qy-ext-demo/tests/qyi1.rs`） | 多包带填充与单包：样本与生成时相同；索引截断、索引位置在末尾之后、索引位置落在文件头内、包重叠、索引魔数错误：明确错误，明细与识别一致；包数超出容量为 `CapacityExceeded`；9 个定位目标；前缀不含索引时 `container: true` | 通过 |
| 组合解码器 | 路由按 `container: true` 转交；带 ID3v2 标签时跳转与定位位置正确 | 通过 |
| 桌面真实引擎 | QYI1、WAV、带标签的 QYI1 混排共用一个设备流，字节与参考逐字节一致；报告“文件头 2 次、数据 2 次”；每首只打开一次；用户定位后新流从目标帧起一致；不可定位音源明确失败（桌面策略为停止）、不重新打开；坏文件写明明细 | 通过 |
| 嵌入式虚拟时间 | 可定位存储逐字节一致、`parse_jumps = 4`、无欠载；不可定位存储按策略跳过（计入 `unsupported`）、只打开一次、下一首照常；声明 `J = 1` 时给出 `jump-budget-exceeded` | 通过 |
| 命令行与文件信息 | `qybit-ext decode` 解出带标签 QYI1 的样本与参考一致；`ExtFormats` 的文件信息读取（`read_header`）给出 QYI1、9000 帧与格式 | 通过 |
| MCU 预算公式 | `J`、`k`、`S` 的独立手算用例；原有用例数值不变 | 通过 |
| 兼容 | 全工作区测试（未改动的原有测试全部通过）；Clippy：Linux、Windows gnu 交叉、alsa-lib 特性、thumbv7em | 通过 |
| no_std 与 QEMU | `qy-core`、`qy-host-embedded` thumbv7em 编译与 Clippy；QEMU 演示照常通过 | 通过：QEMU（mps2-an386，指令集仿真，不是实机）Full 与 Lite 照常核对通过；`.text` Full 130 968 字节（D1 后 129 932，+1 036）、Lite 114 464 字节（D1 后 113 984，+480）；`.bss` Full 398 504、Lite 91 068 字节；板级示例 STM32F411 照常编译 |

真实设备与 M4A 文件未运行（M4A 解析在插件仓库）。

## 6. 插件需要做的

插件把尾置 `moov` 的解析接到 `take_jump` 上（沿顶层 box 的长度跳过 `mdat`、读 `moov`、按索引回到音频包，策略由插件决定），在识别函数中对认出的容器回答 `container: true`，并为 MCU 声明 `J`、`k`。现有的读取器包装（`FastStartSource`）可以保留作测试参考。
