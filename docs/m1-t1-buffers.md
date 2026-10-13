# M1 T1：统一缓冲接口记录

实现：`crates/qy-codec-flac/src/lib.rs`、`src/mcu.rs`；核心新增 `Core::with_window`。

## 缓冲契约

| 缓冲 | 类型与长度（桌面） | 字节 | 归属 |
| --- | --- | ---: | --- |
| 帧输入窗口 | `u8 × 1048576` | 1 048 576 | 核心：`Core::with_window(input, Capacity::input_window, decoder)` |
| 解码/预测/解相关工作区 | `i64 × (2×65535)` | 1 048 560 | 解码器：`FlacDecoder::new(cap, workspace, pcm)` |
| 最终交错 PCM | `i32 × (2×65535)` | 524 280 | 解码器 |

- 三类缓冲都是 `&'static mut [T]`，类型不带生命周期泛型。输入窗口与工作区分属核心和解码器，不存在重叠的可变借用。
- 容量只来自宿主配置 `Capacity`，不依据码流头部决定。构造时切片长度不足，返回 `InitError::CapacityExceeded { buffer, required, provided }`；核心窗口不足返回 `CoreError::CapacityExceeded`。
- 本配置承诺支持 ≤ 1 MiB 的编码帧，超过返回容量错误。约 512 KiB 的 PCM 只代表最终 PCM 和 verbatim 帧的数量级，不是所有合法编码帧长度的上界。
- 工作区可在不同阶段复用，但不会用 unsafe 重新解释切片。

## 其他开销（桌面，不随块大小增长）

| 项 | 大小 |
| --- | ---: |
| SPSC 设备帧队列（rtrb，宿主启动时分配一次） | 768 000 字节 |
| pack 暂存区 | 32 768 字节 |
| `FlacDecoder` 结构体（切片 + 容量；T2 加入的解析状态同样是小状态） | 56 字节 |
| `Core<WavDecoder>` 结构体 | 176 字节 |
| `FrameProducer` / `FrameConsumer` | 各 40 字节 |

结构体大小在 x86_64 debug 下测得。`structs_hold_no_block_sized_storage` 断言 `FlacDecoder` ≤ 128 字节、`Core` ≤ 256 字节，以证明二者不内嵌随块大小增长的存储。

## 桌面与 MCU

- **桌面**：宿主启动时 `vec![0; n]`，转为 boxed slice 后 `Box::leak` 一次，整个程序生命期复用；停止、换曲、重置都不重新分配，也不再次 leak。全程是 safe 的零初始化，不使用原地未初始化内存 API。
- **MCU**（特性 `static-cell`）：`static_cell` 固定为 `=2.1.1`。
  - `mcu::from_cells` 对三个 `ConstStaticCell::new([0; N])` 调用 `try_take`，转为切片并按容量检查；已被取走时返回 `InitError::AlreadyTaken`，不 panic。
  - `mcu::take_buffers` 是参考配置 `Capacity::MCU` 的静态缓冲：块大小 ≤ 4608、双声道、编码帧 ≤ 32 KiB。按 RFC 9639 可流式子集，4608 是 ≤ 48 kHz 时的块大小上限；更大的块或超出窗口的帧返回容量错误。三块缓冲共 32 768 + 73 728 + 36 864 = 143 360 字节。
  - **`.bss` 核验**：在 thumbv7em-none-eabihf release 下，用 `llvm-objdump` 检查 `qy-codec-flac` 的目标文件，三个静态缓冲分别位于 `.bss.…mcu5INPUT`（0x8001）、`.bss.…mcu9WORKSPACE`（0x12008）、`.bss.…mcu3PCM`（0x9004），都是 BSS 节，不占 flash。多出的字节是 `taken` 标志及对齐填充。**实际 MCU 工程的最终链接 map 尚未核验**，需要在目标工程里确认。

## 测试

| 测试 | 内容 |
| --- | --- |
| `buffers::desktop_capacity_matches_task_card` | 桌面与 MCU 容量数值与任务卡一致 |
| `buffers::structs_hold_no_block_sized_storage` | 结构体大小上界 |
| `buffers::capacity_errors_are_explicit` | 工作区、PCM、输入窗口各少 1–2 个元素时返回明确的容量错误 |
| `no_realloc::desktop_buffers_on_256k_stack_reuse_without_allocation` | 在显式 256 KiB 栈的 debug 线程内：safe 堆分配桌面容量的三块缓冲；构造解码器与 1 MiB 窗口的核心；夹具从文件读入堆；20 次播放、停止、换曲循环中计数分配器为 0。单独成一个测试二进制，避免并行线程干扰计数 |
| `mcu::static_buffers_taken_once_with_declared_capacity` | 静态缓冲长度、全零、可构造解码器；第二次获取返回 `AlreadyTaken` |
| CI no_std | thumbv7em 下 `qy-codec-flac` 开、关 `static-cell` 各编译一次，并跑 Clippy |

零分配检测的有效性已验证：在循环里人为加入分配，测试即失败（多出 20 次分配）。连续运行 10 次都稳定通过。

“不崩溃”不等于测得精确的最大栈用量；MCU 实机的 RAM 和性能另行验证。

## 未完成（依赖 T2）

- 256 KiB 栈测试中的“最大块解码”：需要 T2 的真实 FLAC 帧解码。T1 不用空壳实现凑数，T2 完成后加入同一测试。
- `Core<FlacDecoder>` 的构造与播放：`FlacDecoder` 在 T2 实现 `Decoder` 之后才能交给核心。T1 中 1 MiB 窗口的核心路径暂用 WAV 解码器驱动。
