# W1 真实世界 WAV 的容错与 8 位 PCM：记录

日期：2026-10-14。任务卡：[tasks/W1.md](../tasks/W1.md)。本卡的结果全部来自自有解码器测试、模拟、虚拟时间仿真与 fuzz，**不是实机结果**。

## 1. 真实来源的核对（实现前）

卡上的候选规则要先用真实写入程序的输出核对。下表是在开发机上实际生成文件、读出头部得到的结果。只取头部结构；夹具的样本由生成器重新构造，不把第三方音频放进仓库。

| 写入程序 | 方式 | RIFF 长度 | data 长度 | 其他 |
| --- | --- | --- | --- | --- |
| ffmpeg 6.1.1-3ubuntu5 | 输出到文件 | 正确 | 正确 | data 之前有 `LIST`（`ISFT Lavf60.16.100`） |
| ffmpeg 6.1.1-3ubuntu5 | 输出到管道（`-f wav pipe:1`） | **0xFFFFFFFF** | **0xFFFFFFFF** | 同上；data 是最后一个块；24 位双声道写 EXTENSIBLE |
| arecord（alsa-utils 1.2.9） | 写文件，SIGINT 正常停止 | 正确（已回填） | 正确 | – |
| arecord | `-d 1` 输出到标准输出 | 正确（按时长预先算出） | 正确 | – |
| arecord | 不给时长、输出到标准输出 | **0x80000024** | **0x80000000** | 流式占位值 |
| arecord | 写文件，被 SIGKILL 中止 | **0x80000024** | **0x80000000** | 头部未回填 |

据此确定的规则：

- **R1 / R2 采用 0xFFFFFFFF**，有 ffmpeg 的真实样例。卡上的另一个候选值 0 没有找到写出它的程序，**未纳入**：RIFF 长度 0 仍按 `Malformed` 拒绝，data 长度 0 仍是空数据。找到实际样例后可以按同样的方式加入。
- **arecord 的占位值 0x80000000 不当作占位值**。理由：这个值也可能是一个真实的 2 GiB 文件，两者无法区分。这种文件按 R4 + R5 处理：
  - data 长度 0x80000000 对 6 字节一帧不是整帧；
  - 文件实际比声明短。
  - 结果：播放全部完整帧，再按 `Truncated` 结束，明细写明缺少的字节数。样本与按占位值处理时完全相同，区别只在结束的类别。
  - 夹具：`trunc_arecord_placeholder`。
- 其他写入程序（sox、各种录音软件）的输出**未核对**，本机没有这些程序。

## 2. 规则与实现

实现在 `crates/qy-codec-wav/src/lib.rs`，规则写在模块文档里。解码器仍是推送式：任意分块都能推进，最大连续输入不变。

| 编号 | 情形 | 处理 | 报告 |
| --- | --- | --- | --- |
| R1 | RIFF 长度 = 0xFFFFFFFF | 不以 RIFF 长度为界，读到源末尾 | `Repair::LengthPlaceholder { chunk: RIFF, declared, read }`，`read` 为 RIFF 体实际字节数 |
| R2 | data 长度 = 0xFFFFFFFF | data 延伸到源末尾，按整帧读取；末尾不足一帧的字节丢弃 | `LengthPlaceholder { chunk: data, read }`（另加 `PartialFrameDropped`） |
| R3 | RIFF 长度与源不符，而 data 的声明长度在源内完整 | 见下方两种情况 | `LengthMismatch { chunk: RIFF, declared, actual }` |
| R4 | data 长度不是整帧 | 播放全部完整帧，丢弃不完整的尾帧 | `PartialFrameDropped { bytes }` |
| R5 | data 声明长度超过源 | 播放完整帧后照旧报 `Truncated` | `WavDetail::DataShort { declared, missing }` |

R3 的两种情况：
- **RIFF 偏短**：data 越出 RIFF 不再整首拒绝。data 读完之后，越出 RIFF 的块也不再拒绝。此后以各块声明长度为准，读到源末尾。
- **RIFF 偏长**：data 读完之后源提前结束，视为正常结束。

**照旧拒绝**（意图不明）：
- fmt 字段互相矛盾，例如 blockAlign 与位宽、声道不符（`mal_block_align`、`mal_pipe_block_align`）；
- data 在 fmt 之前；
- 重复的 fmt 或 data（`mal_dup_data`、`mal_riff_placeholder_dup_data`）；
- data 之前的块越出 RIFF（`mal_chunk_before_data_beyond_riff`）：无法判断是 RIFF 还是该块的长度写错；
- RIFF 内不足一个块头的尾部字节（`mal_tail_lt8_in_riff`）：RIFF 与源长度一致，并不是 R3 的情形；
- RIFF 为占位值、data 长度为 0（`mal_riff_placeholder_data_zero`）：可能是未回填的占位，也可能真的为空。

**8 位 PCM**：
- WAVE 规范的 8 位 PCM 无符号、偏移 128，读取时减去 128，N = 8。这是精确的整数表示变换。
- EXTENSIBLE 8 位容器、V < 8 时，减去 128 之后再算术右移 8 − V，与其他位宽的有效位处理相同。
- 设备格式按 pack 规则放置（例如 C16/V16 时左移 8 位），不改变相对幅度。

**修正只决定读到哪里为止和丢弃不完整的尾部**：样本值都来自文件原样，不改、不编造。

**接口**：
- `qy_core::engine::Repair` 是与编解码器无关的修正描述。
- `Resources::repair_count()` / `repair(i)` 是新增的默认方法，冻结的 `Decoder` 不变。
- 引擎在核心关闭曲目时（解码器尚未复位）取出修正，以新增的 `HostAction::Repaired { track, repair }` 逐条交给宿主。`HostAction` 是引擎接口，不在冻结范围内。

**解码器结构大小**：
- 修正只记几个标量，不占缓冲，配置档预算不变。
- `WavDecoder` 由 128 字节增至 176 字节，`Core<WavDecoder>` 由 232 增至 280 字节。
- `qy-codec-flac/tests/buffers.rs` 中 Core 的经验上限相应由 256 调为 320。这项检查的目的（证明没有内嵌块大小的存储；最小的块缓冲为 73 728 字节）不变，依据写在测试注释里。

## 3. 报告

- **桌面**（`qy-player` 播放报告）：
  - 每条修正一行：“曲目 n 按规则修正文件结构：…（样本未改动）”；
  - 有修正时，结论加一句：“软件路径无损，但这不说明文件本身完好”；
  - R5 的截断照旧作为失败报告，“WAV 错误明细”写明“文件比 data 声明的 N 字节短 M 字节”。
- **模拟**（`qy-host-sim` 报告）：`Report::repairs`，不计入回归摘要。
- **嵌入式**：每条修正发一条通知 `EVENT repaired track=n …`；`STATUS` 行增加 `repairs=` 计数。

## 4. 夹具与参考

生成器在 `tools/qy-gen-fixtures`（`cargo run -p qy-gen-fixtures -- wav`）。每个夹具的期望由生成器按构造独立给出：样本、修正的文本形式（`repairs` 行）与 R5 明细（`detail` 行）。现有 WAV 夹具共 87 个。

- **新增 28 个**：
  - 规则 R1–R5 各自“刚好成立 / 刚好不成立”的边界：
    - RIFF = 0xFFFFFFFE 不是占位值，按 R3 处理；
    - data = 0xFFFFFFFE 不是占位值，按 R5 处理；
    - RIFF 偏短 4、偏长 2（不足一个块头）、偏长 2 且源末尾有 3 字节残余、偏长 8（写入程序把文件总长写进 RIFF）、偏长 10；
    - 尾部 1、2、4、17 字节；
    - 截在帧边界与帧中间；
  - 与真实写入程序同构的头部：ffmpeg 管道、arecord 中断；
  - 修正与已有拒绝情形的组合：RIFF 占位 + blockAlign 矛盾、RIFF 占位 + 重复 data、RIFF 占位 + data 为 0；
  - 8 位：单声道奇数长度（含填充）、双声道、EXTENSIBLE 8/8、6/8、6/8 脏填充位、8 位 + 不足一帧尾部。
- **ffmpeg 第二参考**：
  - 共 45 个夹具附 `.ffmpeg.s32le`，比原来多 11 个 W1 夹具。生成器断言 ffmpeg 的输出等于独立样本，测试再独立比较一次。
  - ffmpeg 同样丢弃不足一帧的尾部（报 “Invalid PCM packet”），样本数与本解码器一致。
  - 8 位用 ffmpeg 的 `pcm_u8` 输入解码器。
- **期望变化的现有夹具**（逐项依据，均保留回归覆盖）：

| 原夹具 | 原期望 | 现在 | 依据 |
| --- | --- | --- | --- |
| `mal_chunk_beyond_riff`（RIFF 偏短 4，data 完整，样本全零） | `Malformed` | 由 `fix_riff_short` 取代（同样的结构，改用已知样本），可播放并报 RIFF 不符 | R3 |
| `mal_data_not_frame_multiple`（data 6 字节、4 字节一帧） | `Malformed` | 由 `fix_data_partial_frame` 取代（同结构，已知样本），丢弃 2 字节 | R4 |
| `trunc_riff_claims_more`（RIFF 偏长 10，data 完整） | `Truncated` | 由 `fix_riff_long` 取代（相同字节），可播放并报 RIFF 不符 | R3 |
| `unsup_8bit`（8 位双声道，样本全零） | `Unsupported` | 由 `pcm8_*` / `ext8_*` / `ext6in8_*` 取代（含 0、128、255 等极值） | W1 §2 |
| `trunc_mid_frame` | `Truncated` | 不变，另有明细 `DataShort { 400, 197 }` | R5 |

其余 59 个现有夹具的 `.wav` 与 ffmpeg 参考逐字节不变，`.expect` 只增加 `repairs none` 与 `detail none` 两行。

## 5. 验证

| 检查 | 结果 |
| --- | --- |
| 自有解码器夹具测试 | 87 个夹具，每个都以一次性、逐字节、200 个固定种子随机分块输入，结果、样本、修正、明细、格式与布局全部一致 |
| WAV 1000 种子与 FLAC 1200 种子模拟摘要 | 改动前后逐种子一致（`QY_DIGEST=1`，共 2199 行，在改动前提交的工作树上对照） |
| 新模拟场景 `qy-host-sim/tests/sim_w1.rs` | 17 个可播放夹具 × 种子 1–4 × 两种设备模型（整包 / 短写）：设备字节逐字节等于独立参考、修正与期望一致、保真完整。混排播放列表：修正按曲目编号报告，R5 截断的曲目播放完整帧后按策略跳过 |
| 桌面 `qy-player` | 报告逐条写明修正与 R5 明细；设备字节等于 ffmpeg 第二参考 |
| 嵌入式虚拟时间仿真 | FAT 镜像 + DMA 喂数：`EVENT repaired` 通知、`repairs=3`，截断曲目跳过，设备字节等于参考 |
| QEMU MCU 演示（指令集仿真，不是实机） | Full 与 Lite 照常通过，状态行含 `repairs=0`（演示曲目都是规范文件）。`.bss` 不变（Full 365 736、Lite 91 068 字节）；`.text` Full 82 812 → 85 076、Lite 69 972 → 71 132 字节；栈高水位 Full 12 988 → 11 412、Lite 6 828 → 8 092 字节（栈区分别为 157 896 与 432 564 字节） |
| fuzz `wav_decode` | 见下 |

**fuzz**：

- 新增检查：分块输入与一次性输入的修正必须一致，且修正中的数值不超出输入长度。
- 初始语料：全部 87 个 WAV 夹具，各加分块种子字节 0x00 与 0x87，共 174 个。
- 正式运行：`-max_total_time=600 -max_len=65536 -fork=4 -seed=20261014`。

**第一轮发现的问题**（已修复，这一轮的计数不作为正式记录）：

- **现象**：同一输入，分块输入与一次性输入的结果不同。fuzz 在约 90–120 秒内给出 4 个输入，都属同一类：
  - 一次性输入时，可播放并报 RIFF 不符；
  - 分块输入时，按 `Malformed` 拒绝。
- **原因**：以 RIFF 长度为界、data 已读完、RIFF 余下不足一个块头（< 8 字节）时，解码器在还不知道源是否结束时就按“RIFF 内的残余字节”拒绝。这些字节可能尚未送到，即“RIFF 声明过长”（R3）。
- **修复**：data 读完后，RIFF 余下的字节尚未到齐就等待更多输入或源结束，再做判断。结果与分块无关。
- **回归**：新增夹具 `fix_riff_long_by_2` 与 `fix_riff_long_partial_tail`。夹具测试的逐字节与 200 个随机分块种子都覆盖这一点；把修复改回去，夹具测试即失败（`fix_riff_long_partial_tail: 逐字节`）。

**正式运行**（修复后的最终代码）：

| 时长 | 执行次数 | 初始语料（去重后） | 覆盖 / 特征 / 语料 | 结果 |
| ---: | ---: | ---: | --- | --- |
| 616 秒 | 13 925 876 | 174（119） | 599 / 1951 / 300 | **0 崩溃、0 超时、0 内存超限** |

命令：

```text
cd fuzz
cargo +nightly fuzz run wav_decode corpus/wav_decode_w1b -- \
  -max_total_time=600 -max_len=65536 -fork=4 -ignore_crashes=1 -seed=20261014
```

开发机为 4 vCPU 的 KVM 虚拟机（见 [r1-reliability.md §6.1](r1-reliability.md)）。CI 的 fuzz 冒烟照旧以全部 WAV 夹具为初始语料运行 60 秒。

## 6. 范围外

- RF64 / BW64（超过 4 GB 的录音与广播文件）：另列；
- 浮点 WAV：FORMATS 的 F3；
- 修复文件（写回磁盘）：只读播放，不改文件。
