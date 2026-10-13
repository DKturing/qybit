# M1 T3：参考工具与接入记录

## 1. 参考工具

| 工具 | 版本 | 用途 |
| --- | --- | --- |
| Xiph `flac` / `metaflac` | 1.4.3（生成器要求 ≥ 1.4.0，启动时检查 `flac --version`） | CLI 夹具编码、显式格式参考解码、实际格式与分支核对 |
| claxon | 未使用 | — |

生成命令：`cargo run -p qy-gen-fixtures -- flac`（实现：`tools/qy-gen-fixtures/src/flac.rs`、`flac/reference.rs`），结果写入 `crates/qy-codec-flac/tests/fixtures/MANIFEST.txt`。每个夹具的清单条目包括：

- **生成命令**：CLI 夹具记录完整的 `flac` 命令行；手写夹具记录构造说明。
- **种子**：来自固定常量，见 `reference::seeds`。
- **SHA-256 摘要**：工具 crate 新增依赖 `sha2 = "=0.10.9"`，只用于计算夹具摘要。
- **参考解码**（只对有效夹具做）：
  - 位深为 8 的整数倍时，用 `flac -d --force-raw-format --endian=little --sign=signed` 输出原始 PCM，其有符号性、位深和字节序都是显式指定的。
  - 其他位深（12/20 位）libFLAC 不支持原始输出，改用 `flac -d` 输出 WAV，再由独立读取器还原 N 位右对齐样本。
  - 两种方式都必须与已知原始 PCM 逐样本一致，才写出 `.s32le` 参考。
- **实际格式核对**：用 `metaflac --show-sample-rate/--show-channels/--show-bps/--show-max-blocksize/--show-total-samples` 读取，必须与 `.meta` 一致。
- **实际分支核对**：解析 `flac --analyze` 输出的子帧类型/阶数、残差方法、escape 分区、wasted bits、声道分配和块大小。每个夹具都有分支要求（`a|b` 表示任一，`!x` 表示不得出现），见 `reference::expected_branches`。不假设命令选项一定会产生目标分支；某条要求不满足时，生成器直接失败。
- **生成器无法从命令行得到的分支**：包括 LPC 32 阶、escape 分区、33 位侧声道极值、非常用采样率码、可变块和最大块等，采用独立规范夹具，即按 RFC 9639 手写编码并注明来源。这些夹具仍由 libFLAC 参考解码交叉核对。
- **错误夹具**：不做参考解码，结果与错误明细写入 `.meta`。

关于 claxon：本任务卡规定 claxon 只能用于已验证支持的夹具，且其错误或缺失特性不得作为通过依据。为避免引入一个只能部分覆盖的参考，本项目**完全不使用 claxon**，全部参考均来自 libFLAC 1.4.3。

T3 只重新生成了清单（新增摘要、种子和逐夹具核对行），所有 `.flac`/`.meta`/`.s32le` 字节均未改变。

## 2. 自动识别解码器 `qy-codec-auto`

- `no_std`、无 alloc、静态分发。`AutoDecoder` 同时持有 `WavDecoder` 与 `FlacDecoder`，两者使用宿主启动时提供的各自独立工作区。
- 每首曲目在 `reset` 后读取前 4 字节，按文件头选择解码器，不依据扩展名：
  - `RIFF`/`RIFX`/`RF64`/`BW64` 交给 WAV 解码器，由后者再区分是否支持；
  - `fLaC` 交给 FLAC 解码器；
  - 其他返回 `Unsupported`；
  - 不足 4 字节时：未到 EOF 返回 `NeedInput`，已到 EOF 返回 `Truncated`。
- `required_window()` 返回 FLAC 容量要求的输入窗口；`flac_error()` 提供 FLAC 错误明细。
- 冻结的 `Decoder` 接口没有改变。

## 3. 宿主接入

- `SimHost`（`SimHost::new` / `with_capacity`）与 `HostResources` 都改为 `Core<AutoDecoder>`。启动时一次性分配以下缓冲：
  - WAV PCM 工作区；
  - FLAC 桌面容量工作区（`i64` 131070）与 PCM（`i32` 131070）；
  - 1 MiB 输入窗口，经 `Core::with_window` 传入；
  - pack 暂存区。
- 停止、换曲和设备重配置都复用这同一套缓冲；M0 的代次、背压、drain 和 pack 路径没有改动。
- 播放驱动遇到解码失败时，在报告的 `messages` 中附上 FLAC 错误明细（例如 `FLAC 错误明细：FrameCrc`），CLI 摘要会显示。
- CLI 用法改为 `<文件>...`，WAV 与 FLAC 都按文件头识别。

## 4. 测试

| 测试 | 内容 | 结果 |
| --- | --- | --- |
| `qy-host-sim/tests/sim_flac.rs::flac_seeded_scenarios` | 1200 个固定种子（0–1199），按 `种子 % 6` 分类，见下方列表 | 全部通过；debug 构建约 17 s，release 约 2 s。故障统计：Malformed 149、Truncated 20、Unsupported 31、Read 200；拒绝旧回报 114 次，设备断开 66 次 |
| `sim_flac.rs::concurrent_hosts_have_independent_workspaces` | 4 个线程各持一个 `SimHost`，各跑 12 个种子（10000–10047）；先核对 4 个实例共 20 个大缓冲的地址区间互不重叠，再用单实例顺序重放，结果须逐字节相同 | 通过 |
| `qy-host-wasapi` `player::tests::flac_and_wav_mixed_playlist` | FakeBackend 播放 WAV 16 → FLAC 16（同格式共用一个流，无缝衔接）→ FLAC 20 位（选 24/24）→ FLAC 32 位 192 kHz → 最大块 32 位；期望字节由 libFLAC 参考样本按独立公式计算 | 通过（Linux；Windows 目标经 wine 通过） |
| `player::tests::flac_decode_error_drains_verified_frames` | 第二帧 CRC 错误：只播放已校验的 512 帧，然后报告 Malformed 及明细；文件缺 `fLaC` 标记时按内容判为 Unsupported | 通过 |
| `tests/device.rs::exclusive_playback_flac_44k_16bit` | 真机：独占播放 libFLAC 生成的 44.1 kHz/16 位夹具，要求 20000 帧排空且无欠载 | **未运行**（无 Windows 音频设备），默认忽略 |

`flac_seeded_scenarios` 的场景分类（`种子 % 6`）：

- **0–1 正常**：FLAC/WAV 混排，包含短读、零长度读、慢读和停顿，设备为 WASAPI 整包或短写。
- **2 解码故障**：在列表中插入 19 个错误夹具之一。
- **3 读取错误**：读取在随机偏移处失败。
- **4 停止重启**：重启后在途的旧读取必须被拒绝。
- **5 最大块**：65535 帧 32 位的帧，读取以 1–16 字节为主；部分种子模拟设备断开。

上表中整个测试只创建一个 `SimHost`，跨全部种子复用。期望字节由独立公式 `s << (C − N)` 计算，FLAC 样本取自 libFLAC 参考的 `.s32le`。

**复现**：用 `QY_SIM_SEED=<n> cargo test -p qy-host-sim --test sim_flac` 单独重跑一个种子。

**规范依据（修正测试期望）**：`err_no_marker` 在 qy-codec-flac 单独解码时结果为 Malformed（缺少 `fLaC` 标记）。经自动识别层时，这个文件不进入 FLAC 解码器，按“未知容器”判为 Unsupported。测试对缺标记的文件按此规则设期望，其余错误夹具仍与 `.meta` 一致。

**变异检查**（检查后已恢复源码）：

- 把模拟宿主输入窗口真正缩小为 64 KiB：205 个种子失败（最大块报 `InputWindow`）。
- `AutoDecoder::reset` 不重置 FLAC 解码器：1063 个种子失败。
- 补充说明：只改 `with_window` 的 `window` 参数而不缩小切片，属于无效变异，因为该参数只用于核对切片长度。

## 5. FLAC fuzz

目标：`fuzz/fuzz_targets/flac_decode.rs`，使用桌面容量工作区，只泄漏一次。

**检查项**：

- 不 panic；
- 消耗量不越界；
- EOF 后不返回 `NeedInput`；
- `NeedInput` 时窗口未满；
- 样本都在 N 位范围内；
- 用首字节作种子做分块与部分取出，结果、错误明细、格式和样本都须与一次性输入完全一致。

| 项 | 值 |
| --- | --- |
| 工具链 | cargo 1.101.0-nightly (29c5daa1a 2026-10-07)，cargo-fuzz 0.13.2 |
| 初始语料 | 44 个 FLAC 夹具 × 首字节 `0x00`/`0x85`（88 个），`fuzz/corpus/flac_decode`（不入库） |
| 参数 | `cargo +nightly fuzz run flac_decode -- -max_total_time=600 -max_len=1048576 -seed=1` |
| 时长 | 600 s |
| 结果 | 195503 次执行（平均 325 次/s），覆盖 415 → 499，特征 1238 → 1720，新增语料 1875，语料最终 363 个（5270 KB），峰值 RSS 400 MB；**无崩溃、无断言失败**，无 artifact |

CI 的 `fuzz-smoke` 改为矩阵，`wav_decode` 与 `flac_decode` 各跑 60 秒。

## 6. 验收与未完成

- 自动测试：fmt、Clippy（Linux / Windows 目标 / thumbv7em）、全部测试、no_std 编译（`qy-core`、`qy-codec-wav`、`qy-pcm-format`、`qy-codec-flac`、`qy-codec-auto`）都在本地运行并通过；Windows 目标测试经 wine 运行。
- **真机测试未运行**：`exclusive_playback_48k_16bit`、`exclusive_playback_flac_44k_16bit`、`format_matrix_and_sequential` 需要 Windows 与真实音频设备，当前环境没有。软件检查与模拟不代表真实 DAC 已验证。
- ALSA/snd-aloop 属于 M2，尚未开始。
