# M1 T2：整帧解析与有界重试记录

实现：`crates/qy-codec-flac/src/`

| 文件 | 内容 |
| --- | --- |
| `lib.rs` | 元数据状态机、整帧重试门槛、`Decoder` 实现 |
| `frame.rs` | 整帧解析与重建 |
| `bits.rs` | 位读取器 |
| `crc.rs` | CRC-8 / CRC-16 |

## 依据与出处

以 RFC 9639 为准，以下各条均与 Xiph libFLAC 1.4（`src/libFLAC/stream_decoder.c`，取自 xiph/flac master）逐项对照。本开发环境的网络策略拦截了 rfc-editor.org、ietf.org 与 IETF CELLAR 规范仓库，**RFC 原文无法在此环境直接取得**；章节号按 RFC 9639 的结构引用，行为以 libFLAC 源码交叉核对。

2026-10-14 补充：取得 RFC 9639 原文 PDF（版本见 [specs.md](specs.md)）后核对了下表的章节号，更正两处：子帧一行原写 §9.2.3（实为 Constant Subframe），解相关一行原写 §9.2.3（应为 §4.2 与 §9.1.3）。各条款内容与 RFC 原文的逐条复核另行安排。

| 规则 | RFC 9639 | 实现 |
| --- | --- | --- |
| "fLaC" 标记；元数据块头：最后块标志 + 7 位类型 + 24 位长度；STREAMINFO 必须是第一个块且长度 34；类型 127 禁用；其余块流式跳过、不缓存（含 100 KB 图片） | §8、§8.1 | `lib.rs` |
| STREAMINFO 字段：最小/最大块（16/16）、最小/最大帧（24/24）、采样率 20、声道−1 3、位深−1 5、总样本 36、MD5 128。位深范围 4–32 | §8.2 | `parse_stream_info` |
| 帧头：同步码 0b111111111111100 + 阻塞策略位；块大小码 0 保留；采样率码 15 禁用；声道码 11–15 保留；位深码 3 保留；保留位须为 0；类 UTF-8 编码号（固定块 ≤ 6 字节，可变块 ≤ 7 字节）；非常用块大小 8/16 位（值 +1）；非常用采样率 8 位 kHz / 16 位 Hz / 16 位 10 Hz；CRC-8 多项式 0x07 | §9.1 | `frame.rs` |
| 子帧：零填充位；类型 常量 / verbatim / 固定 0–4 阶 / LPC 1–32 阶，其余保留；wasted bits 标志 + 一元码（k−1）；侧声道多 1 位（32 位源的侧声道为 33 位） | §9.2.1（子帧头与类型）、§9.2.2（wasted bits）、§9.2.3–§9.2.6（各类型） | `subframe` |
| LPC：精度字段 0b1111 无效；右移量为负无效（libFLAC 同样视为失步） | §9.2.6 | `subframe` |
| 残差：00 Rice（4 位参数，escape 0b1111）、01 Rice2（5 位参数，escape 0b11111），1x 保留；分区阶数须整除块大小，且首分区样本数 ≥ 预测阶数；escape 为 5 位原始位宽，0 表示全零；残差须能以 32 位有符号数表示 | §9.2.7 | `residual` |
| 帧尾：零填充到字节边界（须为 0），CRC-16 多项式 0x8005 | §9.3 | `decode_inner` |
| 解相关：left/side（R = L − S）、side/right（L = S + R）、mid/side（M = (M<<1) \| (S&1)；L = (M+S)>>1；R = (M−S)>>1） | §4.2（解相关与侧声道多 1 位）、§9.1.3（声道码） | `decode_inner` |

## 整帧解析与有界重试

- **每次尝试都从帧头重新解析整帧**，不保存 Rice 或子帧的位级暂停状态。子帧先写进 `i64` 工作区。读完 CRC-16 并校验通过后，才做解相关、范围检查，并写入交错 `i32` PCM 后发布；任何失败尝试写入的数据都不会对外可见。
- **缺输入时**：记 `retry_at = min(max(2n, n+1), 窗口容量)`，n 为本次尝试时已缓冲的长度，做了溢出检查且必然大于 n。输入未达门槛且未到 EOF 时，直接返回 `NeedInput`，不重新解析。`retry_at` 只是重试门槛，不代表帧长度。
- **窗口已满仍不完整**：返回 `CapacityExceeded`（明细 `FrameTooLarge`）。
- **源 EOF**：绕过门槛，做至多一次最终解析。完整的短尾帧可以成功；不完整则报 `Truncated`。码流结束时，若 STREAMINFO 给出了总样本数，还会核对实际样本数，从而发现恰好在帧边界的截断。
- **成功后**：按实际帧长度消耗输入，保留后续字节，门槛归零。已解码、等待输出空间时返回 `NeedOutput`，不重解该帧，也不消耗输入。
- **STREAMINFO 不可信**：
  - 最大帧长度非零且超出窗口，或最大块大小超出容量时，元数据阶段就报容量错误。
  - 最大帧长度为 0（未知）或谎报时，仍有界累积，以窗口容量为准。
  - 每一帧的块大小、声道、位深、采样率都与 STREAMINFO 和容量核对；阻塞策略中途改变视为错误。
- 32 位源的侧声道按 33 位处理，预测、残差、还原都用 `i64`，并逐样本检查范围。最终样本在位深范围内，才转为 `i32`，不截断。
- **超出支持范围的**：3 声道及以上、采样率 > 192 kHz（PCM 格式层上限），都返回 `Unsupported`，不降精度、不改声道。
- **错误明细**：`DecodeError` 是冻结接口，只有 Malformed、Unsupported、Truncated、CapacityExceeded 四类。更细的原因由 `FlacDecoder::last_error()` 给出 `FlacError`，如 `HeaderCrc`、`FrameCrc`、`NegativeShift`、`ResidualOverflow` 等。
- **不校验 MD5**（STREAMINFO 的 MD5 为全零时表示未知）。帧尾 CRC-16 已经保证帧数据完整；整流 MD5 可以作为后续增强。
- **不做失步重同步**：遇到同步码不符、帧尾之后有多余字节等情况，都明确报错，不跳过。

## 测试统计（特性 `test-metrics`）

- 统计字段 `Metrics`、更新代码和查询接口都放在 `#[cfg(any(test, feature = "test-metrics"))]` 之后。该特性默认关闭，正式构建不含计数器，只用 core 整数类型。
- 集成测试 `tests/metrics.rs` 在 Cargo.toml 中声明 `required-features = ["test-metrics"]`，需显式运行 `cargo test -p qy-codec-flac --features test-metrics`。
- thumbv7em 下开、关 `test-metrics` 都能编译，也都通过 Clippy（CI 两种都跑）。

## 夹具与独立参考（`tools/qy-gen-fixtures/src/flac.rs`，`cargo run -p qy-gen-fixtures -- flac`）

共 44 个夹具，清单见 `crates/qy-codec-flac/tests/fixtures/MANIFEST.txt`，其中记录了工具版本、生成命令、来源和分支覆盖。生成结果可复现：两次生成逐字节相同。

- **命令行编码 11 个**：Xiph `flac` 1.4.3 编码已知样本的 WAV。覆盖 16/24/32 位、单/双声道、44.1–192 kHz、`-8 -e -m`、`-l 12`、`-l 0`（仅固定预测）、`--lax`（32 位）、噪声（verbatim）、静音声道（常量）、低位全零（wasted bits）、24 位噪声（Rice2）、块大小 1000 与 16384。
- **按规范手写 14 个**：最大块 verbatim（65535 × 2 × 32 位，约 512 KiB 帧）、32 位侧声道 33 位极值（left/side、side/right、mid/side，以及 33 位上的固定预测与 LPC）、escape 分区（12 位、0 位）与 Rice 参数 0/14、Rice2 参数 23 与 Rice2 escape、wasted bits 1–5、固定 0–4 阶、LPC 32 阶（精度 15、位移 15、分区阶数 4）、可变块与非常用块大小码 6/7、采样率继承 STREAMINFO、非常用采样率码 12/13/14、元数据跳过、最大帧长度未知、最大帧长度谎报、短尾帧。
- **错误 19 个**：
  - 校验与截断：帧头 CRC、帧 CRC、帧内截断、帧边界截断（总样本数不足）；
  - 结构：无标记、首块不是 STREAMINFO、元数据类型 127；
  - 子帧与残差：负位移、精度 0b1111、分区阶数不整除、残差超出 32 位、预测溢出、帧尾填充非零；
  - 一致性与保留码：帧参数与 STREAMINFO 不符、阻塞策略改变、块大小保留码、声道保留码；
  - 不支持：3 声道、384 kHz。
- **libFLAC 核对**：每个有效夹具都用 `flac -d`（libFLAC）解码，要求与源样本逐样本相等，否则生成器失败。期望值取源样本，不来自本解码器。
- **分支覆盖**：由 `flac --analyze` 独立确认，缺任何一项生成器都会失败。覆盖：CONSTANT、VERBATIM、FIXED 0–4 阶、LPC（含 32 阶）、INDEPENDENT、LEFT_SIDE、RIGHT_SIDE、MID_SIDE、RICE、RICE2、wasted bits、escape 分区、块大小 65535。

## 测试与结果

| 测试 | 内容 | 结果 |
| --- | --- | --- |
| `fixtures::all_fixtures_match_reference_under_any_chunking` | 44 个夹具：一次性、逐字节、随机分块（50 个种子，> 200 KB 的夹具 5 个种子），并随机部分取走输出；结果、错误明细、格式、全部样本都与参考一致 | 通过（debug 5.4 s） |
| `behavior::window_just_enough_and_one_byte_short` | 窗口恰为帧长时成功；少 1 字节时报容量错误 | 通过 |
| `behavior::unknown_or_lying_max_frame_still_bounded_by_window` | 最大帧长度未知或谎报为 100、窗口 1000 字节：实际帧超出窗口时报容量错误；桌面容量下正常解码 | 通过 |
| `behavior::streaminfo_block_larger_than_capacity_is_rejected` | 用 MCU 容量（块 4608）解码 65535 块的流：`BlockTooLarge` | 通过 |
| `behavior::eof_bypasses_threshold_for_complete_short_frame` | 尾帧给到 n−3 字节，门槛为 2n；补齐后仍低于门槛时不解析；EOF 时最终解析成功，门槛归零；截断尾帧报 `Truncated` | 通过 |
| `behavior::threshold_resets_between_consecutive_frames` | 门槛 20 → 成功后归零 → 第二帧按新的 n=3 计门槛 6 | 通过 |
| `behavior::need_output_does_not_redecode_or_consume` | 未取走时连续 3 次 `NeedOutput`：消耗为 0，PCM 不变；支持部分取走 | 通过 |
| `behavior::incomplete_input_never_publishes_partial_audio` | 不完整输入时 PCM 始终为空 | 通过 |
| `metrics::max_block_frame_bytewise_is_log_attempts_linear_scan` | 逐字节喂入 F = 524 292 字节的帧 | 解析尝试 21 次（界 ⌈log2 F⌉+2 = 22）；累计扫描 1 572 867 字节 ≈ 3F（界 5F）；样本运算 262 140；decode 调用 524 294 次 |
| `metrics::compressed_frames_count_samples_separately_from_input_scan` | 高压缩帧：输入扫描 ≤ 5F，样本运算按样本数单独计 | 通过 |
| `no_realloc`（T1 补完） | 256 KiB 栈，`Core<FlacDecoder>` 解码最大块 5 轮，零分配 | 通过 |
| 单元：`crc`、`bits` | CRC-8/SMBUS 与 CRC-16/BUYPASS 的标准校验值；位读取、33 位有符号数、一元码、越界返回 `Incomplete` | 通过 |

**变异检查**：以下 12 项临时改动都会让测试失败：

- zigzag 符号写错；
- 不校验 CRC-16；
- 门槛每次只加 1（解析次数退化为 O(F)）；
- 不还原 wasted bits；
- LPC 位移改用除法；
- 侧声道不加 1 位；
- EOF 不绕过门槛；
- 不核对总样本数；
- escape 0 位也读取；
- `NeedOutput` 时重新解码；
- mid 不补回最低位；
- side/right 还原错。

“mid/side 中右移改为除以 2”是等价变异：mid ± side 恒为偶数，两者结果相同，不算测试漏洞。

## 未包含（属 T3 或后续）

- FLAC 解码器接入 `qy-host-sim` 与桌面播放，以及 FLAC 的 fuzz 与故障模拟（T3）。
- 夹具摘要（哈希）、claxon 对照（T3）。
- MD5 校验、失步重同步。
