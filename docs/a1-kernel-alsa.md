# A1：ALSA 内核直连与多架构静态程序

日期：2026-10-17。任务卡：[tasks/A1.md](../tasks/A1.md)。

结论：Linux 版 `qybit` 不再依赖 alsa-lib，直接使用内核 ALSA 接口；每个目标架构编译为一个完全静态的 musl 程序，不依赖任何系统库。真实内核路径已在 QEMU 虚拟机（Ubuntu 通用内核 + snd-aloop 虚拟声卡）中验证；用户机器上的真实声卡尚未运行（步骤见 §8）。

## 1. 结构

| 位置 | 内容 | `unsafe` |
|---|---|---|
| `qy-host-alsa/src/kernel/abi.rs` | `include/uapi/sound/asound.h` 的结构体与 ioctl 编号（PCM 接口 2.0.17、控制接口 2.0.9），`#[repr(C)]` 逐字段转写 | 无 |
| `kernel/hw.rs` | 参数空间运算：掩码与区间、精确设定、取最近值（`set_near`）、回绕点公式 | 无 |
| `kernel/tlv.rs` | 声道映射能力的 TLV 解析 | 无 |
| `linux/sys.rs` | `open`、`fcntl`、`ioctl`、`poll` 的系统调用边界 | 有（§6） |
| `linux/ctl.rs` | 控制设备：声卡信息、PCM 枚举、子设备偏好、声卡 id 解析、“Playback Channel Map” 元素 | 无 |
| `linux/kernel_pcm.rs` | `KernelPcm` / `KernelOpener`：`pcm::Pcm` 与 `pcm::Opener` 的内核实现 | 无 |
| `linux/devices.rs`、`linux/capture.rs` | 设备列表（`qybit list-devices`）、B 层回录 | 无 |
| `linux/priority.rs` | 渲染线程 `SCHED_FIFO` | 有（§6） |
| `linux/alsalib.rs` | alsa-lib 后端，只在默认关闭的 `alsa-lib` 特性下编译（§7） | 无 |

`Pcm` trait 之上的部分（候选格式、协商、读回核对、短写渲染循环、报告、矩阵）没有改动，假 PCM 的全部测试照常运行。

## 2. 与 alsa-lib 的对应

出处：内核 `sound/core/pcm_native.c`、`pcm_lib.c`、`control.c`、`pcm.c`；alsa-lib `src/pcm/pcm_hw.c` 作为同一流程的参照。

| 操作 | alsa-lib（原后端） | 内核接口（现后端） |
|---|---|---|
| 打开 | `snd_pcm_open("hw:…")` | 解析 `hw:` 设备名；声卡名按各卡 `CARD_INFO` 的 id 查找；控制设备上 `PCM_PREFER_SUBDEVICE`；以 `O_NONBLOCK` 打开 `/dev/snd/pcmC{卡}D{设备}p`（被占用立即 `EBUSY`），再恢复阻塞读写；指定子设备时用 `PCM_IOCTL_INFO` 核对，不符重试至多 4 次 |
| 接口版本 | 内部 | `PCM_IOCTL_PVERSION` 主版本须为 2；`PCM_IOCTL_USER_PVERSION` 告知内核本程序转写的版本（旧内核没有这个 ioctl，忽略） |
| 能力查询 | `snd_pcm_hw_params_test_format` | `HW_REFINE`：全空间 → 限定 RW_INTERLEAVED → 逐个格式试收窄 |
| 设置参数 | `set_access/format/channels`、`set_rate_resample(0)`、`set_rate`、`set_period_size_near`、`set_buffer_size_near` | 同样的顺序逐项 `HW_REFINE`：格式（掩码）、声道与采样率（精确，区间 `[v, v]`）、`NORESAMPLE` 标志、周期与缓冲取最近值 |
| 应用与读回 | `snd_pcm_hw_params` + `hw_params_current` + 解析 `snd_pcm_dump_hw_setup` 文本取精确采样率与有效位 | `HW_PARAMS`：内核把最终参数写回同一结构体，直接取格式、访问方式、声道、周期、缓冲、`rate_num/rate_den`、`msbits`、标志，不再解析文本 |
| 软件参数 | `sw_params`：`start_threshold = boundary`、`avail_min = 周期` | `SW_PARAMS`：同上，另加 `stop_threshold = 缓冲`、播放方向整空闲区填静音（§3），然后 `PREPARE` |
| 写入 | `snd_pcm_writei` | `WRITEI_FRAMES`；帧字节数取自内核写回的 `FRAME_BITS` |
| 可写量、状态 | `snd_pcm_avail` | `STATUS`（运行中内核先更新硬件指针）；XRUN → `EPIPE`、挂起 → `ESTRPIPE`、断开 → `ENODEV` |
| 等待 | `snd_pcm_wait` | `poll`；错误事件时按状态给出原因 |
| 启停 | `start/drop/drain/prepare/hw_free` | `START/DROP/DRAIN/PREPARE/HW_FREE` |
| 声道映射 | `snd_pcm_query/set/get_chmap` | 控制设备上 “Playback Channel Map” 元素（接口 PCM、`device` = PCM 设备号、`index` = 子设备号）：`ELEM_INFO` 取 numid → `TLV_READ` 读能力；`ELEM_WRITE` / `ELEM_READ` 设置与读回 |
| 设备列表 | `snd_card_next`、`snd_ctl_pcm_next_device`、`snd_ctl_pcm_info` | `/dev/snd/controlC*` 上 `CARD_INFO`、`PCM_NEXT_DEVICE`、`PCM_INFO` |
| 回录 | `snd_pcm_readi` | `READI_FRAMES` |

错误照旧报告为调用名与正 errno（调用名现在是 ioctl 名，如 `SNDRV_PCM_IOCTL_HW_REFINE（采样率）`）；`api.rs` 的错误映射（`EPIPE`、`EAGAIN`、`ESTRPIPE`、`ENODEV`）不变。

**参数空间运算**（`kernel/hw.rs`）：

- 区间按内核语义处理开闭端与空集；
- 精确设定先核对值在当前范围内，再收窄并确认结果为单值，失败时参数不变；
- `set_near` 先试目标值本身，不行时分别求不小于它的最小可行值与不大于它的最大可行值，取较近者（距离相同取较小者）。

这些都是纯函数，用一个按内核规则收窄的假设备测试。

## 3. 内核行为与取舍

- **整空闲区填静音**：播放方向设 `silence_threshold = 0`、`silence_size = boundary`。出处：内核 `snd_pcm_playback_silence` 中 `silence_size ≥ boundary` 的分支。
  - 机制：内核推进硬件指针时，在持流锁的情况下把刚播完的区域清零，应用写入这段区域之前它已经是静音。
  - 原因：排空在周期边界才结束，硬件指针会越过最后写入的数据。虚拟机实测越过约 219 帧，不到一个周期。这段时间设备播放的是静音，不是缓冲中上一轮的旧音频；欠载被发现之前同理。
  - 写入的样本不被改变：B 层逐字节回环覆盖了这一点。
  - alsa-lib 1.2.10 起在排空前另行填静音，目的相同。
- **32 位时间结构**：`snd_pcm_status` 在 32 位架构上按旧的 32 位时间布局定义。对应的 ioctl 编号所有内核都接受，64 位内核的兼容层也接受。QyBit 不读状态中的时间戳，只用状态、`avail` 和 `delay`。
- **ioctl 编号**：按 asm-generic 编码，只为 x86_64、aarch64、arm、loongarch64、riscv64 核对过。其他架构（如 MIPS、PowerPC 的编码不同）和大端架构在编译期报错。
- **回绕点**：用内核 `snd_pcm_hw_params` 的公式按本进程 `long` 的宽度计算。32 位程序在 64 位内核上运行时，内核兼容层也按 32 位重算。
- **内核最低版本**：
  - 所用 ioctl 都是 PCM 接口 2.0 系列早已有的；
  - 声道映射元素需要内核提供（PCM 接口 2.0.11 起），没有时按“设备不报告声道映射”处理（F1 规则不变）；
  - 程序本身受 Rust 标准库约束，要求 Linux 3.2 起。
  - 打开时 PCM 接口主版本不是 2 即报错。

## 4. 实施中发现的问题

1. **musl 的 `sched_setscheduler` 固定返回 `ENOSYS`**：musl 按 POSIX 的进程级语义有意不实现它。
   - 现象：静态程序在虚拟机中报告“SCHED_FIFO 注册失败（errno 38）”。
   - 修正：改用线程级的 `pthread_getschedparam` / `pthread_setschedparam`，glibc 与 musl 都直接对线程 id 调用内核。修正后虚拟机中“注册成功，已撤销”。
2. **snd-aloop 排空时丢掉最后一段数据**：这是内核的缺陷，不在 QyBit。
   - 代码：`sound/drivers/aloop.c` 的 `copy_play_buf`。播放端处于 DRAINING 时，最后一次复制只拷贝到 `appl_ptr` 为止，其余部分用 `clear_capture_buf` 清零。但 `clear_capture_buf` 以 `capt->buf_pos` 为起点，而 `buf_pos` 要到复制之后才前移，于是刚复制进回录缓冲的最后一段被静音覆盖。
   - 影响范围：v6.1、v6.8、v6.12 与 master（2026-10 取得的源码）都是如此。任何播放程序经 snd-aloop 排空都会受影响。
   - 证据：诊断显示播放端 `appl_ptr` 已含全部帧（例如 1024 + 4096 = 5120 帧），排空正常返回；回录中缺的是最后约 46–512 帧，位置随定时器而变。
   - 处理：B 层测试在待测曲目之后接 100 ms 同格式静音垫片（同一个流内无缝衔接，不参与比对），让待测数据在排空开始之前就完整经过回环。比对范围、同步规则与前导静音都不变，注释中写明了出处。
3. **musl 程序的 32 位测试**：`hw.rs` 的一个测试在 32 位目标上编译期溢出（`1 << 62`），改为按 `usize::BITS` 计算。

## 5. 静态程序

`.cargo/config.toml` 为五个 musl 目标指定 `linker = "rust-lld"` 与 `-C link-self-contained=yes -C target-feature=+crt-static`：用工具链自带的链接器与 musl 启动文件，不需要交叉 C 工具链。

```
rustup target add aarch64-unknown-linux-musl
cargo build --release -p qybit --target aarch64-unknown-linux-musl
```

开发机上的构建结果（release，未去符号）：

| 目标 | Rust 支持级别（按 1.97.0 发行清单：有无 rustc 宿主工具） | 文件大小 | ELF |
|---|---|---|---|
| `x86_64-unknown-linux-musl` | Tier 2（带宿主工具） | 1 316 880 字节 | static-pie |
| `aarch64-unknown-linux-musl` | Tier 2（带宿主工具） | 1 310 888 字节 | 静态 |
| `armv7-unknown-linux-musleabihf` | Tier 2 | 1 254 608 字节 | 静态 |
| `loongarch64-unknown-linux-musl` | Tier 2（带宿主工具） | 3 980 008 字节 | 静态 |
| `riscv64gc-unknown-linux-musl` | Tier 2 | 3 096 384 字节 | 静态 |

五个程序都没有 `PT_INTERP`，也没有 `DT_NEEDED`（`readelf -l` / `readelf -d` 核对）。可加载部分都在 0.9–1.3 MB 之间；loongarch64 与 riscv64 文件更大，是因为符号与展开信息更多。CI 的 `linux-static` 作业每次推送都构建、核对并上传这五个程序（构件名为 `qybit-linux-<架构>`，内含 `qybit` 与 B 层测试程序 `qybit-loopback-aloop`）。

## 6. `unsafe` 审计

| 位置 | 调用 | 前提 |
|---|---|---|
| `sys::open_rw` | `fcntl(F_GETFL/F_SETFL)` | fd 由 `File` 持有且有效；只改文件状态标志 |
| `sys::ioctl<T>` | `ioctl(fd, req, &mut T)` | 调用前核对 ioctl 编号的大小字段等于 `size_of::<T>()`，不等即返回 `EINVAL` 而不调用，所以内核按编号读写的字节不会超出 Rust 对象；`T` 只含整数字段，内核写入任何位模式都合法 |
| `sys::ioctl_none` | `_IO` 类 ioctl | 编号的大小与方向字段为 0；参数 0 不被解引用 |
| `sys::ioctl_tlv` | `TLV_READ` | 头中的 `length` 由这里按切片长度填写，内核只向头之后写至多这么多字节 |
| `sys::writei` / `readi` | `WRITEI_FRAMES` / `READI_FRAMES` | 帧数按切片长度除以帧字节数换算，帧字节数取自内核写回的 `FRAME_BITS`，内核访问不超过切片 |
| `sys::poll` | `poll(&pollfd, 1, t)` | 本地有效的 `pollfd` |
| `priority` | `pthread_getschedparam` / `pthread_setschedparam(pthread_self())` | 只作用于调用线程；输出为本地变量 |
| `interrupt` | `signal(SIGINT)` | 未改（M2 T3） |

结构体布局有两层核对：

- 编译期：对每个目标架构断言大小（`const _: () = assert!(size_of::<HwParams>() == …)`）；
- 测试中：逐项对照参考值，共 75 项大小、偏移、ioctl 编号与常量。64 位参考值从系统头文件 `asound.h` 编译得到，armv7 参考值用 `arm-linux-gnueabihf` 的 C 编译器从同一头文件得到。

`qemu-user` 下的 armv7 单元测试在运行时再核对一次 32 位布局。

## 7. alsa-lib 后端（可选特性，只用于测试）

按 §6.3 甲与 §6.4，原 alsa-lib 后端移到 `linux::alsalib`，只在 `--features alsa-lib` 下编译（需要 libasound 开发包）。它只用于 CI 中的两项测试：

- A 层回环（alsa-lib `file` 插件，`tests/loopback_file.rs`）；
- `null` 插件冒烟测试。

发布版不开启这个特性，`qybit` 不依赖 alsa-lib。默认构建与 musl 构建都不需要 libasound。

## 8. 验证结果

| 层 | 范围 | 结果 |
|---|---|---|
| 宿主单元测试（x86_64 glibc） | `qy-host-alsa`（含内核 ABI 75 项参考值、参数空间、TLV、`sys` 边界）、`qy-pcm-format` 及全工作区 | 通过 |
| `--features alsa-lib` | A 层回环 88 项逐字节一致 + 40 项拒绝；`null` 插件冒烟 | 通过 |
| musl x86_64（原生运行） | `qy-core`、三个解码器、`qy-codec-auto`、`qy-pcm-format`、`qy-player`、`qy-host-alsa`、`qybit` 共 225 项 | 通过 |
| `qemu-user`（aarch64、armv7、loongarch64、riscv64） | 同一组测试（release + 溢出检查与 debug 断言） | 每个架构 225 项通过；`qybit --version` 运行 |
| QEMU 虚拟机 x86_64（Ubuntu 6.8.0-147，snd-aloop） | §8.1 | 通过 |
| QEMU 虚拟机 aarch64（同版本 arm64 内核，aarch64 原生与 armv7 经 32 位兼容层） | §8.1 | 通过（回录缓冲加大后） |
| 用户机器真实声卡 | `list-devices`、矩阵、顺序播放 | **未运行**（§9） |
| loongarch64、riscv64 真实内核；32 位 ARM 原生内核；任何实机 | — | **未运行** |

`qemu-user` 说明：用 debug 构建时，`qy-player` 中按真实时间消费的假设备测试在仿真下会欠载（aarch64 与 armv7 都如此，x86_64 原生不会），所以 CI 用 release 构建并打开溢出检查与 debug 断言。这只说明仿真慢，不是架构相关的错误：armv7 的同一测试在 release 下通过。

### 8.1 虚拟机中的真实内核路径

`scripts/aloop-vm.sh` 用 Ubuntu 通用内核与 snd-aloop 模块启动一个最小虚拟机（busybox + musl 静态测试程序，以 root 运行）。CI 的 `loopback-aloop` 作业每次推送运行 x86_64 与 aarch64 两种虚拟机；runner 提供 KVM 时 x86_64 用 KVM。

x86_64（开发机上软件仿真）结果：

- `qybit list-devices`：列出 `hw:CARD=Loopback,DEV=0/1`。
- 内核直连单元测试：打开每个设备，查询到格式 S16_LE、S24_3LE、S24_LE、S32_LE、FLOAT_LE；snd-aloop 不提供声道映射元素，按“设备不报告”处理。
- 设备测试：
  - 格式矩阵 48 项，参数读回一致 48 项、播放完整 48 项；
  - 顺序换曲 44.1/48 kHz 通过；
  - 48 kHz/16 位 WAV 与 44.1 kHz FLAC 播放通过；
  - SCHED_FIFO 注册成功。
- 命令行：
  - `hw:Loopback,0,0` 被另一个进程占用时报告 EBUSY；
  - 不存在的声卡名报告 ENODEV（`查找声卡 id`）；
  - 不存在的设备号报告 ENOENT。
- B 层回环：
  - 88 项逐字节一致，40 项 N > V 在设备选择阶段被拒绝；
  - 同格式三曲无缝衔接通过，三种格式依次重新打开也通过。
- 重复运行：加垫片后 B 层累计 33 轮，其中 28 轮全部通过。
  - 4 轮各有 1 项因播放端欠载失败，每次都如实报告（如“欠载 1 次 / 747 帧”），没有当作逐字节一致。原因是软件仿真 CPU 下设备缓冲只有 40 ms；这几次也走通了真实内核上的 xrun → `PREPARE` 恢复路径。有 KVM 时预计不会出现，CI 的结果见作业记录。
  - 另有 1 轮是附加检查“三种格式依次重新打开”失败，当时没有保留细节，原因未确认。

aarch64（同版本 arm64 内核，qemu virt，软件仿真）结果：

- aarch64 程序与 armv7 程序（经 64 位内核的 32 位兼容层）都通过：设备列表、内核直连单元测试、设备矩阵 48 项、顺序换曲、WAV/FLAC 播放（SCHED_FIFO 注册成功），B 层 88 项逐字节一致、40 项拒绝、两项附加检查通过。
- 首次运行时 aarch64 的 B 层有 1 项因**回录端**溢出失败（192 kHz，当时回录缓冲为 8 个周期 = 80 ms）。回录只用于核对，缓冲加大到 32 个周期后复测全部通过。
- armv7 经兼容层运行，内核走的是 32 位兼容处理函数；这与 32 位 ARM 原生内核不是同一条代码，原生内核**未运行**。

CI（run 39058369531，提交 6dc07fc，2026-10-17）：`linux-static` 五个架构全部通过（构建、静态核对、`qybit --version`、单元测试，构件已上传）；`loopback-aloop` 两个虚拟机全部通过：x86_64 用 KVM（`-accel kvm -cpu host`），aarch64 用软件仿真，aarch64 与 armv7 程序的设备列表、内核直连单元测试、设备测试与 B 层回环全部为 0（通过）。报告见构件 `qybit-alsa-vm-x86_64`、`qybit-alsa-vm-aarch64`。

2026-10-18：软件仿真的 aarch64 虚拟机中 B 层又出现 1 项欠载（60 帧，如实报告为失败）。此后 `scripts/aloop-vm.sh` 在软件仿真（aarch64，或 x86_64 没有 KVM）时设 `QYBIT_ALOOP_PERIODS=16`：播放端设备缓冲为 16 个周期（160 ms），默认是 4 个周期（40 ms）。这只增加时序余量，比对的字节、同步规则与欠载判定都不变；KVM 与真机仍用默认值。

虚拟回环经过真实内核 ALSA 路径，但不经过任何真实驱动与 DAC，不证明设备端 bit-perfect。

## 9. 用户机器上的测试步骤（真实声卡，x86_64）

1. 取得程序：
   - 从 CI 最新一次运行的构件 `qybit-linux-x86_64` 下载 `qybit` 与 `qybit-loopback-aloop`；
   - 或者在有 Rust 的机器上运行 `cargo build --release -p qybit --target x86_64-unknown-linux-musl`。
   - 然后 `chmod +x qybit qybit-loopback-aloop`。不需要安装 alsa-lib、alsa-utils 或任何库。
2. 权限：普通用户需要在 `audio` 组中（`id` 查看；`sudo usermod -aG audio $USER` 后重新登录），或者用 `sudo` 运行。
3. 列出设备：`./qybit list-devices`，应列出主板声卡的 `hw:CARD=…,DEV=…`。
4. 格式矩阵：`./qybit matrix --device hw:CARD=<id>,DEV=0 --out report`。会播放短促的测试音，先把功放或耳机音量调低。
5. 顺序播放：
   - 先生成测试音：`./qybit tone t48.wav --rate 48000 --bits 24 --seconds 5`、`./qybit tone t44.wav --rate 44100 --bits 16 --seconds 5`，约 −12 dBFS。
   - 然后播放：`./qybit --device hw:CARD=<id>,DEV=0 t48.wav t44.wav`。
   - 应听到两段 1 kHz 正弦，报告中“读回一致”、无欠载。
6. B 层回环（虚拟声卡，可选）：
   - `sudo modprobe snd-aloop`，确认 `cat /proc/asound/cards` 中有 Loopback；
   - 运行 `QYBIT_REPORT_DIR=./report ./qybit-loopback-aloop --ignored --nocapture --test-threads=1`。

2026-10-18 真机结果（用户）：USB 耳机（`hw:CARD=em006,DEV=0`，S24_3LE）播放正常、读回一致、无欠载；主板声卡 `hw:CARD=Generic_1,DEV=0` 读回一致、无欠载，但 3.5 mm 耳机无声，原因是混音器 Master 为 0% 且静音（没有桌面服务去打开它）。格式矩阵 48 项中 8 项读回一致并播放完整，顺序换曲通过。`SCHED_FIFO` 以普通用户运行时为 EPERM（未授予实时调度），没有欠载。为混音器增加了 A2 `qybit mixer`（[a2-mixer.md](a2-mixer.md)）。

设备被 PipeWire / PulseAudio 占用时会报告 EBUSY 与占用进程，需先停止它们。没有桌面的系统通常没有这些服务。把输出与 `report/` 下的 JSON 发回即可记录。
