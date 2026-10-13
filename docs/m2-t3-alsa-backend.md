# M2 T3：ALSA 渲染后端与 CLI 记录

## 依赖

| 依赖 | 版本 | 用途 |
| --- | --- | --- |
| `alsa` | =0.12.1 | alsa-lib 安全封装，仅 Linux 目标。2026-07-31 发布，未撤回；依赖 `alsa-sys` 0.6.1 |
| `libc` | =0.2.190 | 渲染线程 `sched_setscheduler`，仅 Linux 目标；与锁文件中 alsa 的依赖为同一版本 |

构建需要 libasound 开发包：Ubuntu 上为 `libasound2-dev`，本环境为 1.2.11。CI 的 Linux 作业已安装。

`unsafe` 只出现在 `linux/priority.rs`：调用 `sched_getscheduler` / `sched_getparam` / `sched_setscheduler`，pid 为 0，只作用于调用线程，每处都有 SAFETY 说明。alsa-lib 调用全部经 alsa crate 的安全封装。

## 结构（`crates/qy-host-alsa`）

| 模块 | 内容 | 平台 |
| --- | --- | --- |
| `pcm.rs` | 与 alsa-lib 一一对应的最小 PCM 操作 trait（`Pcm`/`Opener`），错误为“调用名 + 正 errno” | 全平台 |
| `api.rs` | `Pcm` 到 `qy_player` 短写循环 `WriteApi` 的适配与错误映射 | 全平台 |
| `backend.rs` | 只接受 `hw:` 设备；按候选格式设置参数、读回、核对，不一致则释放参数换下一个 | 全平台 |
| `attempt.rs` | 协商记录：请求、alsa-lib 返回、读回值、核对结果 | 全平台 |
| `matrix.rs` | 48 项格式矩阵、顺序换曲、确定性 JSON/CSV | 全平台 |
| `dump.rs` | 解析 `snd_pcm_dump_hw_setup`，取精确采样率与有效位 | 全平台 |
| `linux/real.rs` | alsa crate 真实绑定 | Linux |
| `linux/devices.rs` | 枚举可播放的 hw 设备 | Linux |
| `linux/occupants.rs` | 查找占用设备节点的进程 | Linux |
| `linux/priority.rs` | `SCHED_FIFO` 实时调度 | Linux |
| `testing.rs` | errno 层的假 PCM（`test-support`） | 全平台 |

## 参数设置与读回

依据 alsa-lib PCM 文档；`pcm.h`、`pcm.c` 取自 alsa-project/alsa-lib master，本环境实际下载核对。

- **严格设置**：
  - access 为 `RW_INTERLEAVED`，格式与声道精确设置；
  - `set_rate_resample(false)` 关闭 alsa-lib 重采样；
  - `set_rate(rate, dir = 0)` 精确设置采样率，不用 `set_rate_near`。
- **周期与缓冲**：用 `*_near` 取设备在该配置空间内接受的值（目标周期 10 ms，缓冲为 4 个周期）。这两项不影响样本值；取到的值作为请求值，参与读回核对。
- **读回**：`snd_pcm_hw_params` 应用后，用 `snd_pcm_hw_params_current` 读回以下项，交给 `qy_pcm_format::alsa::confirm` 核对：
  - 访问方式、格式、声道、周期、缓冲、重采样标志；
  - 精确采样率与有效位：alsa crate 0.12.1 没有封装 `get_rate_numden` / `get_sbits`，也不暴露底层指针，所以从 `snd_pcm_dump_hw_setup` 解析。`pcm.c` 中这两行打印的正是 PCM 对象上已应用的 `rate_num/rate_den` 与 `msbits`。
  - 解析不到精确采样率时记为分母 0，核对必然报错（严格失败，不放行）。
- **软件参数**：`start_threshold = boundary`，即只由显式 `snd_pcm_start` 启动；`avail_min` 设为一个周期。
- **打开失败**：
  - 设备名不是 `hw:` 时，在调用 `snd_pcm_open` 之前就拒绝；
  - `EBUSY` 时报告占用者，方法是扫描 `/proc/*/fd` 中指向 `/dev/snd/pcmC{卡}D{设备}p` 的进程，并说明不会改用共享或混音路径；
  - `EACCES` 时提示加入 `audio` 组。

## 渲染

渲染使用 `qy_player::render::write` 短写循环：不解码、不 pack、不分配、不阻塞于生产端，每次按 `snd_pcm_wait` 与 `snd_pcm_avail` 搬运完整帧。

| alsa-lib 返回 | 处理 |
| --- | --- |
| `writei` 短写 | 只提交实际写出的帧，其余留在队列下次继续 |
| `EPIPE`（xrun） | 记为欠载（写入序号、设备帧位置、缺失帧），用 `snd_pcm_prepare` 恢复，重新预填后再 `start`；该段播放记录不再是完整保真，退出码 2 |
| `EAGAIN` / `EINTR` | 本次写入 0 帧 |
| `ESTRPIPE` / `ENODEV` 等 | 设备错误，停止，退出码 1 |
| `drain` 返回 `EPIPE` | 已写入的数据都已播完，按排空完成处理 |

xrun 缺失帧的估计：每次写入后记录 `snd_pcm_delay` 与当时的时间，按“距上次写入的时长 × 采样率 − 当时设备中待播帧数”估算。不调用 `snd_pcm_status`，因为它每次都要分配。

EOF 时用 `snd_pcm_drain` 播完设备中剩余音频，不补尾部静音（尾部填充恒为 0）。

实时调度：`SCHED_FIFO`（优先级 70）只作尝试，结果写入报告，失败不阻止播放，结束时恢复原策略。

## CLI（Linux）

- `qybit list-devices`：列出 `hw:CARD=<id>,DEV=<n>`、声卡名与 PCM 名。
- `qybit --device hw:… a.flac b.wav`：顺序播放。
- `qybit matrix --device hw:… [--out 目录]`：48 项矩阵（6 种采样率 × 4 种格式 × 单/双声道）加顺序换曲，写出 `qybit-alsa-device-matrix.json` / `.csv`。
- 退出码同 M0：0 完整播放；2 完成但有欠载；1 失败。

本机（无声卡）实测：
- `list-devices` 输出“没有可播放的 ALSA hw 设备”；
- `--device plughw:0,0` 被拒绝并说明原因，退出码 1；
- `--device hw:0,0` 报 `snd_pcm_open errno 2（ENOENT）`，退出码 1。

## 测试

| 测试 | 内容 |
| --- | --- |
| `tests::negotiation_picks_smallest_valid_bits_in_device_order` | 16 位源在设备只接受 `S24_LE`/`S32_LE` 时选 `S24_LE`（低位对齐）；32 位源无合适格式时打开失败、不降精度 |
| `tests::readback_mismatch_and_setting_errors_try_next_candidate` | `set_rate` 失败后释放参数换下一个候选；读回时格式被改写、近似采样率、有效位不足，都导致打开失败并列出原因 |
| `tests::device_name_and_open_errors` | `plughw` / `default` / `pipewire` / `dmix` / `front` 被拒绝，且不调用 `snd_pcm_open`；`EBUSY` 报告占用进程；`EACCES` 提示加入 `audio` 组 |
| `tests::playback_bytes_exact_with_short_writes` | 3 个种子的随机短写；FLAC/WAV 混排（含 65535 帧最大块、20 位、32 位 192 kHz）；设备字节与 libFLAC 参考按独立公式放置后一致，无填充；`S24_LE` 为低 24 位加符号扩展，对照独立公式 |
| `tests::xrun_is_underrun_and_device_loss_is_error` | 两次 xrun：欠载 2 次 / 74 帧，各用一次 prepare 恢复，有效字节完整无重复，退出码 2；`ENODEV` / `ESTRPIPE`：设备错误，退出码 1 |
| `tests::decode_fault_drains_and_summary_reports_readback` | CRC 错误只播已校验帧；摘要列出请求、读回一致与精确采样率、已应用参数、短写、回录证据 |
| `tests::fake_matrix_records_capabilities_and_playback` | 48 项与设备能力一致；顺序换曲；JSON 确定、CSV 49 行 |
| `dump::tests::parses_alsa_lib_dump_format` | 转储解析，含非整数采样率与缺项 |
| `linux::real::tests::null_plugin_smoke` | **真实 alsa-lib 绑定**：用 alsa-lib 内置 `null` 插件（不是 hw 设备，测试内部绕过设备名检查），完成设置、读回、核对、写入、启动、排空。本机读回 `S16_LE 48000/1，有效位 16，周期 480，缓冲 1920` |
| `tests/no_alloc.rs` | 计数分配器只统计渲染线程；ALSA 短写循环在随机短写、一次 xrun 恢复与排空过程中**零分配** |
| `tests/device.rs` | 真机测试 3 项（48 kHz/16 位、FLAC、矩阵），需要 `QYBIT_ALSA_DEVICE`，默认忽略，**未运行** |

测试中修正的两处假设备脚本问题：

1. **格式被改写的用例写错了**：读回被改成 `S32_LE` 时，`S32_LE` 候选本身是一致的，打开成功是正确行为。改为读回被改成 `S16_LE`。
2. **xrun 注入点落在了预填阶段**：注入点原来按“第 k 次写入”计数，第二次落在恢复后的重新预填阶段，此时设备未运行，真实设备不会 xrun。改为按“运行中的第 k 次写入”计数，并用 3 遍连播保证运行时长足够。

## 变异检查

检查后源码均已恢复。

| 变异 | 结果 |
| --- | --- |
| 渲染循环中插入一次分配 | 零分配测试失败 |
| 写入 `EPIPE` 当作设备错误 | 1 个测试失败 |
| 不核对读回 | 1 个测试失败 |
| 不检查设备名 | 1 个测试失败 |

## 验证

- fmt 通过。
- Clippy 通过：Linux workspace，以及 Windows 目标（qy-host-alsa、qy-player、qy-host-wasapi、qybit）。
- 测试全部通过：Linux 全 workspace；Windows 目标经 wine 运行（qy-host-alsa 8 项与零分配测试，其余 crate 不变）。
- no_std 编译通过。
- **本环境没有声卡**（无 `/dev/snd`，也没有 snd-aloop 模块），所以真机测试 3 项未运行。snd-aloop 数字回环属于 T4。
