# M2 T4：两层数字回环记录

实现：
- `crates/qy-host-alsa/src/loopback.rs`：同步对齐、逐字节比对、报告；
- `src/linux/capture.rs`：snd-aloop 回录线程；
- `tests/loopback_file.rs`：A 层；
- `tests/loopback_aloop.rs`：B 层；
- `tests/common/`：测试信号、WAV 写入、期望字节；
- `scripts/aloop-loopback.sh`：B 层本地运行脚本。

## 同步与比对

- 每个用例由两首同格式的曲目组成，播放时无缝衔接为一个流：
  - **同步曲目**：1024 帧伪随机满幅样本（SplitMix64，固定种子），不含零样本；
  - **待测曲目**：64 帧真实前导静音，接着各声道的极值（最小、最大、−1、1、0），然后是随机段，最后一帧为满幅。
- 回录中找到同步曲目的期望设备字节，其后字节必须与待测曲目的期望设备字节逐字节一致。前导静音必须保留：测试中专门验证过，少一个静音字节就报不一致，不允许对齐到第一个非零字节来凑结果。
- 测试信号生成、WAV 写入与期望字节公式都在测试里独立实现，不调用被测的解码器和 pack：
  - N = 16/24/32 用 WAV 格式码 1；N = 20 用 EXTENSIBLE 24/20。
  - `S16_LE` / `S24_3LE` / `S32_LE`：左移 C − N 位，取 C/8 字节。
  - `S24_LE`：左移 24 − N 位放在 32 位字的低 24 位，最高字节为符号扩展。
- **用例矩阵**：4 种采样率（44.1/48/96/192 kHz）× 4 种格式 × 单/双声道 × 4 种源位深（N = 16/20/24/32），共 128 项。
  - N ≤ V 的 88 项必须逐字节一致；
  - N > V 的 40 项必须在设备选择阶段被拒绝。
- **报告**：`qybit-alsa-loopback-<层>.json`，写到 `QYBIT_REPORT_DIR`，默认 `target/qybit-report`。每项记录以下内容，并注明是哪一层、虚拟回环不证明 DAC：
  - 请求格式与周期/缓冲；
  - 读回参数；
  - 回录字节数、同步偏移、比对字节数；
  - 结论。

## A 层：alsa-lib `file` 插件

- **播放路径**：全部走真实路径，即 `qy_player::play` → 核心 → 自有解码 → pack → 队列 → 短写渲染 → alsa crate → alsa-lib。
- **设备映射**：测试用的打开器把 `hw:` 设备名映射到 alsa-lib 内置定义 `file:FILE=<路径>,FORMAT=raw`，即 `type file`、`slave.pcm null`（见 `/usr/share/alsa/alsa.conf`）。每次打开写一个新文件。
- **与任务卡的差异**：任务卡写的是“生成临时 asoundrc”。我改用内置定义，原因有两条：
  - alsa crate 0.12.1 没有 `snd_pcm_open_lconf`；
  - 进程内修改 `HOME` / `ALSA_CONFIG_PATH` 在 Rust 2024 中需要 `unsafe` 的 `set_var`。

  内置定义与临时 asoundrc 的类型和 slave 相同，也能按每次打开指定文件路径。
- **附加检查**：
  - 同格式三曲一个流无缝衔接；
  - 不同格式在同一次播放内换曲后重新打开：44.1 kHz/2ch/`S16_LE` → 96 kHz/1ch/`S24_3LE`，两个流各自对齐比对。
- **本地结果**：128 项中 88 项逐字节一致、40 项 N > V 被拒绝，两项附加检查通过，耗时约 0.3 秒。
  - 运行这么快，是因为 `null` slave 不按实时节奏消耗数据。
  - 报告中 `S24_LE` 读回的 msbits 为 32，这是 `file` 插件自己的值；它 ≥ N，核对通过。
- **CI**：在“通用模块 (ubuntu-latest)”作业中运行，报告作为附件 `qybit-alsa-loopback-file` 上传。CI 环境下 alsa-lib 打不开 `null` 插件时测试直接失败，所以 CI 通过就说明 A 层确实执行了。
- **范围**：这一层不经过任何驱动，只证明软件路径提交给 alsa-lib 的字节正确。

## B 层：snd-aloop

- **流程**：每个用例先以与播放端相同的严格参数打开 `hw:<卡>,1,<子设备>` 并启动回录，再经真实播放路径向 `hw:<卡>,0,<子设备>` 播放，然后停止回录、对齐比对。
  - 用例矩阵与 A 层相同；N > V 的用例不打开回录，只检查被拒绝。
  - 卡名用 `QYBIT_ALOOP_CARD` 指定，默认 `Loopback`；子设备用 `QYBIT_ALOOP_SUBDEV` 指定，默认 0。
- **附加检查**：
  - 同格式三曲无缝衔接；
  - 不同格式依次重新打开：`S16_LE` → `S24_LE` → `S32_LE`，每种格式各用一次独立的回录会话。snd-aloop 要求回录端与播放端格式一致，所以“同一次 play 调用内换格式重开”由 A 层与 T3 的假设备测试覆盖。
- **为配合 B 层而改的设备名检查**：位置写法的卡号现在也可以是以字母开头的声卡 id（如 `hw:Loopback,0,1`），ALSA 本身支持这种写法。
- **CI 作业“回环 B 层 (snd-aloop)”**：安装 `linux-modules-extra-$(uname -r)` 后尝试 `modprobe snd-aloop`。
  - **本次结果：未运行**。runner 内核为 `6.17.0-1022-azure`，日志报 `modprobe: FATAL: Module snd-aloop not found in directory /lib/modules/6.17.0-1022-azure`，即 Azure 内核的模块包里没有声卡模块。
  - 作业输出警告“B 层未运行（A 层结果不能代替 B 层）”，并写入作业摘要。
  - 今后 runner 能加载这个模块时，作业会自动运行 B 层并上传报告。
- **本地也未运行**：本开发环境同样无法加载该模块（没有 `/lib/modules`，也没有 `/dev/snd`）。手动运行测试时，没有 Loopback 声卡会明确失败并给出原因。
- **本地运行步骤**：需要 Linux、可加载 snd-aloop（多数发行版在 `linux-modules-extra` 包里）、libasound 开发包，并且 Loopback 声卡未被 PipeWire / PulseAudio / WirePlumber 占用（必要时临时停止这些服务）。

  ```
  scripts/aloop-loopback.sh
  # 或
  sudo modprobe snd-aloop
  cargo test -p qy-host-alsa --test loopback_aloop -- --ignored --nocapture --test-threads=1
  ```

  报告为 `target/qybit-report/qybit-alsa-loopback-snd-aloop.json`。

## 变异检查

把 pack 中 `S24_LE`（低位对齐）的位移改成高位对齐：A 层 24 个 `S24_LE` 用例全部报不一致（逐字节一致从 88 降到 64）。源码已恢复。

## 验证

- fmt 通过。
- Clippy 通过：Linux workspace 与 Windows 目标。
- 测试通过：Linux 全 workspace；Windows 目标经 wine 运行。两个回环测试只在 Linux 编译。
- no_std 编译通过。
- 已在 CI 运行的：A 层（file 插件）。
- 未运行的：
  - B 层（snd-aloop）——CI runner 与本环境都不能加载该模块；
  - 真实设备测试——没有声卡。

所有回环都是数字回环，不证明真实 DAC 输入逐位一致。
