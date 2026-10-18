# A2：声卡硬件混音器（`qybit mixer`）

日期：2026-10-18。任务卡：[tasks/A2.md](../tasks/A2.md)。

## 1. 起因

用户在真机上测试 A1：

- USB 耳机播放正常；
- 主板声卡（HD-Audio）参数读回一致、无欠载，但 3.5 mm 耳机听不到声音；
- `amixer` 显示该卡的 Master 为 0%（−65.25 dB）且静音。

没有桌面的系统上，没有 PulseAudio、PipeWire 或 `alsactl restore` 去打开混音器，编解码器就停在开机时的静音状态。这类系统也常常没有安装 alsa-utils。

## 2. 用法

```
qybit mixer --device hw:CARD=Generic_1                 # 列出该卡的混音器控制
qybit mixer --device hw:CARD=Generic_1 Master 40% on   # 音量 40%，打开静音开关
qybit mixer --device hw:CARD=Generic_1 Master -20dB    # 不超过 −20 dB 的最大档
qybit mixer --device hw:CARD=Generic_1 'Auto-Mute Mode' Disabled
```

- **设备名**：同播放用的 `hw:` 名称，只用其中的声卡部分。
- **列表**：按“基名 + 方向”分组，如 `Master Playback Volume` 与 `Master Playback Switch` 合为一行 `Master（播放）`。每行给出原始值与范围、百分比（按原始值线性换算，与 amixer 相同）、dB（设备提供 dB 信息时）、开关状态、枚举的当前项与可选项，并标出只读与未启用。
- **值**：
  - `on` / `off`：开关；
  - `N%`：百分比，四舍五入后按步长对齐；
  - `NdB`：取不超过该值的最大档位；
  - 整数：原始值；
  - 枚举项名称。
  - 可以给多个值，每个值用到组内第一个接受它的元素上。所有声道设为同一值。
- **控制名**：不区分大小写，依次匹配：
  1. 完整元素名；
  2. 播放方向的基名；
  3. 未写明方向的基名；
  4. 录音方向的基名。
  - 序号写成 `名称,序号`。
- 设置后从内核读回，显示新状态。值不在范围内、控制只读、没有该选项或该控制没有 dB 信息时，明确报错且不写入。

**播放前提示**：Linux 上 `qybit --device …` 打开设备之前，若该卡播放方向（含未写明方向）有控制处于关闭或最小音量，就打印一行提示，例如：

> 提示：声卡 Generic_1（HD-Audio Generic）的混音器中，以下播放控制处于关闭或最小音量：Master（0%，-65.25 dB；关）、Mic（0%；关）……

- 只作提示，不阻止播放，不自动改动。
- “最小音量”指静音，或最小档且增益低于 0 dB；没有 dB 信息时为最小档。最小档为 0 dB 的增益控制（如话筒增强 `Mic Boost`）不会让声音变小，所以不列出。
- 规则对所有声卡相同，不按控制名或设备型号写特例。所以本来就该关闭的控制（如话筒监听 Mic Playback）也会列出，提示中写明了这一点。
- 按 alsa-lib `base_len` 的规则：`Capture Volume` / `Capture Switch` 是录音方向的总控制，不列出；基名以 `-` 结尾时不拆分。
  - 2026-10-18 用户真机上第一版提示列出了 `Capture`、`Mic Boost`、`Internal Mic Boost`，按上面两条修正。

## 3. 保真

- 只调设备自身的硬件控制（编解码器的放大器与静音、USB 设备的功能单元等）。送到设备的数据不变，软件增益仍恒为 1。
- 只在用户明确运行命令时改动（AGENTS §3）。
- QyBit 不保存、不恢复混音器状态（§7）。

## 4. 实现

| 位置 | 内容 |
|---|---|
| `kernel/abi.rs` | `snd_ctl_elem_list` 与 `ELEM_LIST`；元素类型、接口与访问位常量；`snd_ctl_elem_info` 的整数范围与枚举名，`snd_ctl_elem_value` 的 `long` / `long long` / 枚举项读写（32 位 `long` 按半字） |
| `kernel/tlv.rs` | dB 信息：DB_SCALE（含静音标志）、DB_MINMAX(_MUTE)、DB_LINEAR、DB_RANGE（嵌套不超过 4 层），容器中取第一项；换算规则与 alsa-lib `src/control/tlv.c` 相同 |
| `kernel/mixer.rs` | 纯逻辑：按 alsa-lib 简单混音器的后缀规则分组、显示、值解析、百分比与 dB 换算、名称匹配、播放前提示规则 |
| `linux/sys.rs` | `ELEM_LIST` 的系统调用边界：内核向 `pids` 写至多 `space` 个元素标识，`space` 等于缓冲长度 |
| `linux/ctl.rs` | 元素列表（每次 256 个）、`ELEM_INFO`（含枚举项名）、`ELEM_READ` / `ELEM_WRITE`、`TLV_READ` |
| `linux/mixer.rs` | 列出接口为 MIXER 的元素、设置与读回、播放前提示 |

出处：内核 `include/uapi/sound/asound.h`、`include/uapi/sound/tlv.h`、`sound/core/control.c`；alsa-lib `src/control/tlv.c`、`src/mixer/simple_none.c`。

## 5. 验证

| 层 | 内容 | 结果 |
|---|---|---|
| 纯函数 | 名称分组、显示文字、值解析、百分比 ↔ 原始值（含步长）、dB 目标取档、名称匹配、提示规则；dB TLV 四种类型与容器、损坏与嵌套过深的输入；`snd_ctl_elem_list` 布局（64 位对照头文件编译值，32 位按头文件布局）与常量 | 通过，含 qemu-user 下的 32 位 armv7 |
| 用户机器的数值 | DB_SCALE −65.25 dB、每档 0.75 dB、0..87：0 档 −65.25 dB、87 档 0 dB，与 amixer 一致 | 通过（单元测试） |
| QEMU 虚拟机（snd-dummy，`tests/mixer.rs`） | 见下 | x86_64、aarch64、armv7（经 64 位内核的 32 位兼容层）通过 |
| 用户机器真实声卡（2026-10-18） | `Master 40% on` 读回 35/87、−39.00 dB、开；之后 3.5 mm 耳机出声，读回一致、无欠载 | 通过（真机，用户运行） |

虚拟机中的检查。期望值按 `sound/drivers/dummy.c` 与 alsa-lib 换算规则独立算出：`Master Volume` 范围 −50..100，−45 dB 起每档 0.30 dB。

- 列出：
  - `Master：音量 0（范围 -50..100），33%，-30.00 dB`；
  - 录音开关；
  - 枚举 `External I/O Box`。
- 设置后读回：
  - `40%` → 10（−27.00 dB）；
  - `-20dB` → 33（−20.10 dB）；
  - 原始值 100 → 0.00 dB；
  - 录音开关打开；
  - 枚举选 None 后，`CD Volume` 显示为未启用，再恢复。
- 报错：
  - 原始值 101 越界；
  - 没有开关的组收到 `off`；
  - 不存在的枚举项；
  - 未知控制（列出可用名称）。
- 播放前提示：Master 在最小档时出现 `Master（0%，-45.00 dB）`，恢复后不再列出 Master。

## 6. 用户机器上的步骤

```
./qybit mixer --device hw:CARD=Generic_1
./qybit mixer --device hw:CARD=Generic_1 Master 40% on
./qybit --device hw:CARD=Generic_1,DEV=0 sound/a_check.wav
```

- 播放前先把耳机从耳朵上拿开，或者从较低的音量开始。
- 如果列表里还有 Headphone、Front、Speaker 等处于关闭或 0% 的控制，同样打开。
- 如果有 `Auto-Mute Mode`，可试 `Disabled`。

## 7. 设置是否持久

混音器的值保存在内核驱动中，直到重启、驱动重新加载或设备重新插拔（USB 设备）为止。开机时驱动回到默认值，HD-Audio 编解码器的默认值常常是静音。QyBit 不保存状态，要开机后自动生效，有两种做法：

- **装有 alsa-utils 时**：`sudo alsactl store` 把当前状态保存到 `/var/lib/alsa/asound.state`。Debian 的 alsa-utils 软件包带有开机恢复（udev 规则与 `alsa-restore` 服务），通常也会在关机时自动保存。
- **不装 alsa-utils 时**：开机运行一次 `qybit mixer`，例如一个 systemd 服务：

```
# /etc/systemd/system/qybit-mixer.service
[Unit]
Description=QyBit：打开声卡混音器
After=sound.target

[Service]
Type=oneshot
ExecStart=/usr/local/bin/qybit mixer --device hw:CARD=Generic_1 Master 40% on

[Install]
WantedBy=multi-user.target
```

  然后运行 `sudo systemctl enable qybit-mixer.service`。

