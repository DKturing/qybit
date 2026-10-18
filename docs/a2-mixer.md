# A2 / A4：声卡硬件混音器的查看与播放期间的音量

日期：2026-10-18。任务卡：[tasks/A2.md](../tasks/A2.md)（查看）、[tasks/A4.md](../tasks/A4.md)（播放期间的音量）；[tasks/A3.md](../tasks/A3.md)（保存与恢复）已撤销。

## 1. 起因与用户的决定

用户在真机上测试 A1：

- USB 耳机播放正常；
- 主板声卡（HD-Audio）参数读回一致、无欠载，但 3.5 mm 耳机听不到声音；
- `amixer` 显示该卡的 Master 为 0%（−65.25 dB）且静音。

没有桌面的系统上，没有 PulseAudio、PipeWire 或 `alsactl restore` 去打开混音器，编解码器停在开机时的静音状态。

经过几轮：

- 先做了 `qybit mixer` 设置（A2）和保存与恢复（A3），用户用 `Master 40% on` 打开后 3.5 mm 耳机出声；
- 随后用户决定 qybit 不去调整系统的音量，只在用 qybit 播放时带一个音量参数，默认 40%。

因此（2026-10-18）：

- `qybit mixer` 只保留查看；
- 设置、保存与恢复已移除（A3 撤销）；
- 播放期间临时调整设备硬件音量，结束后恢复（A4）。

## 2. 查看：`qybit mixer --device <hw 设备>`

- **只读**，不修改任何控制。
- **分组**：按 alsa-lib 简单混音器（`src/mixer/simple_none.c`）的后缀规则分组，如 `Master Playback Volume` 与 `Master Playback Switch` 合为一行 `Master（播放）`。
  - `Capture Volume` / `Capture Switch` 是录音方向的总控制；
  - 基名以 `-` 结尾时不拆分。
- **每行给出**：原始值与范围、百分比（§4）、dB（设备提供 dB 信息时）、开关状态、枚举的当前项与可选项，并标出只读与未启用。

## 3. 播放期间的音量：`--volume`

```
qybit --device hw:CARD=Generic_1,DEV=0 a.flac                    # 默认 40%
qybit --device hw:CARD=Generic_1,DEV=0 --volume 60% a.flac
qybit --device hw:CARD=Generic_1,DEV=0 --volume off a.flac       # 不调整
qybit --device hw:CARD=Generic_1,DEV=0 --volume-control Headphone a.flac
```

- **开始播放前**：把一组播放音量控制设为该百分比，打开同组的静音开关，并打印设置后与原来的值。
- **播放结束后**：正常结束、出错或 Ctrl+C 之后，按相反顺序恢复原值（先关开关再恢复音量），读回核对，打印“设备硬件音量已恢复”。与当前值相同的控制不写入，也无需恢复。
- **控制的选择**：
  - `--volume-control` 指定（`名称` 或 `名称,序号`）；
  - 否则按内核文档 *Standard ALSA Control Names*（`Documentation/sound/designs/control-names.rst`）的标准名称依次找 `Master`、`PCM`；
  - 都没有时，若播放方向只有一组含音量的控制就用它；
  - 否则说明候选，不调整，照常播放。
  - 录音方向与没有音量的控制不可选。规则对所有声卡相同。
- **设备没有可用的硬件音量时**：照常播放并说明原因，不以软件缩放代替（AGENTS §3）。
- **限制**：进程被强制结束（第二次 Ctrl+C、SIGKILL、断电）时来不及恢复，设备停在播放时的音量。
- **其他**：
  - 只用于 Linux 命令行。Windows 上显式给 `--volume` 会报错说明，Windows 的硬件音量在 GUI 中（GUI T5）。
  - `qybit matrix` 不调整音量。

**播放前提示**：在调整音量之后打印。若该卡播放方向（含未写明方向）还有控制处于关闭或最小音量，打印一行提示。

- 只作提示，不修改。
- “最小音量”指静音，或最小档且增益低于 0 dB；没有 dB 信息时为最小档。最小档为 0 dB 的增益控制（如话筒增强）不列出。
- 本来就该关闭的控制（如话筒监听 Mic Playback）也会列出，提示中写明了这一点。

## 4. 百分比

与 alsamixer 相同（alsa-utils `alsamixer/volume_mapping.c`）：

- **有 dB 信息、dB 范围不超过 24 dB**：按 dB 线性。
- **有 dB 信息、dB 范围更大**：按 10^((dB − 最大)/60) 映射；最小档不是静音时再按最小档归一。设置时先求目标增益，再取增益最接近的档位（相同时取较小者）。
- **没有 dB 信息**：按原始值线性。

例（用户机器上的 Master：−65.25 dB 起每档 0.75 dB，0..87）：

| 百分比 | 档位 | 增益 |
|---|---|---|
| 0% | 0 | −65.25 dB |
| 15% | 35 | −39.00 dB（第一次用 amixer 式的“40%”设置时的值） |
| 40% | 59 | −21.00 dB |
| 100% | 87 | 0 dB |

**为什么不按原始值线性（amixer 的做法）**：各设备的原始值范围与 dB 步长不同，同一个线性百分比响度差别很大。例如 1 dB 一档、0..127 的 USB 音量，按原始值的 40% 约为 −76 dB，几乎听不见。按 alsamixer 的映射，各设备的 40% 都在 −21 dB 左右。

## 5. 硬件音量与音质

- **QyBit 送出的数据**：不论音量设为多少都不变，软件增益恒为 1。
- **设备内部**：音量控制在设备的哪一级实现由设备决定，ALSA 接口不说明这一点。
  - 可能是 DAC 之后的模拟放大或衰减；
  - 也可能是编解码器或 USB DAC 内部的数字衰减。
  - 数字衰减小于 0 dB 时，设备实际输出的数字信号已不是原样，这与转动设备旋钮相同，是设备自身的行为。
- **要让设备原样使用数据**：用 `--volume 100%`（0 dB），用功放、耳放或音箱自己的音量调节响度。直接接耳机时 0 dB 往往太响，需要按情况取舍。
- **列表中的 dB 值**：由驱动提供，表示该控制的增益，不说明它在设备的数字段还是模拟段。

## 6. 实现与验证

| 位置 | 内容 |
|---|---|
| `kernel/abi.rs` | `snd_ctl_elem_list` 与 `ELEM_LIST`、元素类型 / 接口 / 访问位、`elem_info` 的整数范围与枚举名、`elem_value` 的 `long` / `long long` / 枚举项读写 |
| `kernel/tlv.rs` | dB 信息：DB_SCALE（含静音标志）、DB_MINMAX(_MUTE)、DB_LINEAR、DB_RANGE（嵌套不超过 4 层）、容器；换算与 alsa-lib `src/control/tlv.c` 相同 |
| `kernel/mixer.rs` | 分组、显示、alsamixer 百分比、播放音量控制的选择、提示规则（纯函数） |
| `linux/sys.rs`、`linux/ctl.rs` | `ELEM_LIST` 的系统调用边界；元素列表、信息、读写与 TLV |
| `linux/mixer.rs` | 列表、`PlaybackVolume`（设置并保存原值，`restore` 或析构时恢复并读回核对）、播放前提示 |
| `qybit` | `qybit mixer --device`（只读）；播放的 `--volume` / `--volume-control`，默认 40% |
| `qybit-gui`（Linux） | 音量滑块为播放期间的百分比（默认 40%），播放中拖动即写入，结束时恢复；“播放”菜单可关闭（[gui-linux.md](gui-linux.md) §2） |

| 层 | 内容 | 结果 |
|---|---|---|
| 纯函数 | 名称分组与 alsa-lib 特例、显示文字、提示规则；百分比换算（对数映射、dB 线性、无 dB、静音最小档、步长），对照 alsamixer 公式独立算出的值；控制选择；dB TLV 的四种类型、容器、损坏与嵌套过深；参数解析 | 通过，含 qemu-user 下的 armv7 |
| QEMU 虚拟机（snd-dummy，`tests/mixer.rs`） | 见下 | x86_64、aarch64、armv7（aarch64 虚拟机中的兼容层）均通过 |
| 用户机器（2026-10-18，A2 当时的设置方式） | `Master 40% on`（当时按原始值线性，即 35 档、−39.00 dB）后 3.5 mm 耳机出声 | 通过（真机） |
| 用户机器（A4 默认 40%） | — | **未运行** |

虚拟机中的检查，期望值按 `sound/drivers/dummy.c` 与 alsamixer 公式独立算出：`Master Volume` 范围 −50..100，−45 dB 起每档 0.30 dB，dB 范围 45 dB，按对数映射。

- 列表：初始原始值 0 显示为 `17%，-30.00 dB`。
- 缺省选 Master：40% → 原始值 41、−17.70 dB；播放中改为 60%（Linux 图形界面拖动音量滑块，`PlaybackVolume::set_percent`）→ 原始值 65、−10.50 dB；恢复后列表与之前完全相同（2026-10-18 x86_64 虚拟机通过）。
- 0%：播放前提示列出 `Master（0%，-45.00 dB）`；守卫析构时也恢复。
- 指定 Synth：100% → 0 dB。
- 与当前相同（17%）时不写入，恢复时“无需恢复”。
- 不存在的控制、没有音量的控制：报错，设备不变。
- 命令行：`qybit --device hw:CARD=Dummy,DEV=0 --volume 60%` 播放 1 秒测试音 → Master 原始值 65、−10.50 dB；结束后“已恢复”，`qybit mixer` 的输出与播放前逐字相同。

## 7. 用户机器上的步骤

```
./qybit --device hw:CARD=Generic_1,DEV=0 sound/a_check.wav    # 默认 40%：开始时打印设置，结束时打印已恢复
./qybit mixer --device hw:CARD=Generic_1                       # 结束后查看：Master 应回到播放前的值
```

- 先戴着耳机、从低音量试起。觉得太响或太轻，用 `--volume 25%` 或 `--volume 60%` 调整。
- 如果 3.5 mm 的声音还要经过 Headphone 等开关，而它是关闭的，播放前提示会列出它。这时用 `--volume-control Headphone`；如果 Master 与 Headphone 都需要打开，请把列表发来，再决定如何处理。
