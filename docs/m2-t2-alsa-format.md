# M2 T2：ALSA 格式映射与参数确认记录

实现位置：`crates/qy-pcm-format/src/alsa.rs`，测试在 `alsa/tests.rs`。模块为 `no_std`，不分配，不依赖 alsa-lib。

## 出处

依据 alsa-lib `include/pcm.h`，取自 alsa-project/alsa-lib master，本环境实际下载核对：

- `snd_pcm_format_t` 的取值：
  - `S16_LE` = 2；
  - `S24_LE` = 6，原注释为 “Signed 24 bit Little Endian using low three bytes in 32-bit word”；
  - `S32_LE` = 10；
  - `S24_3LE` = 32，原注释为 “Signed 24bit Little Endian in 3bytes format”。
  - 大端、无符号、浮点和其他格式的取值也按该头文件逐项分类。
- 本模块参照的接口：
  - `SND_PCM_ACCESS_RW_INTERLEAVED`；
  - `snd_pcm_hw_params_set_rate_resample` / `get_rate_resample`；
  - `snd_pcm_hw_params_current`；
  - `snd_pcm_hw_params_get_rate_numden`（精确的有理数采样率）；
  - `snd_pcm_hw_params_get_sbits`（设备实际使用的有效位数）。

## 格式映射

| ALSA | C | V | 放置方式 | 说明 |
| --- | --- | --- | --- | --- |
| `S16_LE` | 16 | 16 | — | |
| `S24_3LE` | 24 | 24 | — | 3 字节 |
| `S24_LE` | 32 | 24 | 低位对齐（`LsbAligned`） | 有效位在低 3 字节，pack 时符号扩展最高字节 |
| `S32_LE` | 32 | 32 | — | |

- 大端、浮点、无符号和其他格式（8 位、20 位、压缩、IEC958、DSD 等）一律拒绝，并给出类别。
- WASAPI 式的 32/24（有效位在高 24 位）没有对应的 ALSA 格式，`from_device` 明确拒绝，不会与 `S24_LE` 混用。

## 候选顺序

`candidates(源, 设备偏好)` 只取满足 V ≥ N 的格式，即不改变样本的格式：

- 按 V 升序排列；V 相同时按设备偏好的先后排列。
- 设备没有列出的格式不作为候选；重复项只计一次。
- 没有能无损承载源的格式时返回空，不降精度。

## 设备名

`check_device_name` 只接受 `hw:` 设备。接受的写法：`hw:卡[,设备[,子设备]]`、`hw:CARD=名称[,DEV=n[,SUBDEV=n]]`。

以下名称一律拒绝，并说明原因：

| 名称 | 拒绝原因 |
| --- | --- |
| `plughw` / `plug` | 自动转换格式、采样率与声道 |
| `default` / `sysdefault` | 通常经 PipeWire、PulseAudio 或 dmix |
| `dmix` / `dsnoop` | 软件混音 |
| `pipewire` / `pulse` | 经声音服务处理，会混音或重采样 |
| 其他名称 | 可能是插件或别名，无法确认不转换 |

卡号为数字时，可以得到播放设备节点 `/dev/snd/pcmC{卡}D{设备}p` 对应的卡号与设备号，供 T3 查找占用设备的进程。

## 参数确认

后端设置参数后，用 `hw_params_current` 读回已应用参数，交给 `confirm(源, 请求, 读回)` 核对。全部一致才返回设备格式，否则返回**全部**不一致项：

| 项 | 要求 |
| --- | --- |
| 访问方式 | 必须为 `RW_INTERLEAVED` |
| 格式 | 读回的格式代码必须等于请求，格式被改写即报错 |
| 采样率 | 精确：`num == rate × den` 且 `den ≠ 0`；近似整数值和非整数值都报错 |
| 声道、周期、缓冲 | 必须与请求相等 |
| 重采样 | alsa-lib 重采样必须关闭 |
| 有效位 | 读回的 `sbits` 小于源 N 时报错（会丢失精度）；读不到时不据此拒绝 |
| 请求本身 | 不能无损承载源（采样率或声道不同、N > V）时，返回 `Request` 错误 |

## 打开错误

`OpenError::from_errno` 把 `snd_pcm_open` 返回的负 errno 分类：

| errno | 分类 | 提示 |
| --- | --- | --- |
| `EBUSY` | 设备被占用 | 指出常见占用者（PipeWire、PulseAudio、JACK 或其他播放器），说明不会改用共享或混音路径 |
| `ENOENT` / `ENODEV` | 设备不存在 | |
| `EACCES` / `EPERM` | 无权限 | 提示加入 `audio` 组 |
| 其他 | 原样报告 errno | |

## 测试

均为单元测试，期望字节全部独立手写：

| 测试 | 内容 |
| --- | --- |
| `mapping_fields_codes_and_names` | 四种格式的 C/V/放置方式、代码与名称往返；大端、浮点、无符号、其他格式的代码与名称都被拒绝；WASAPI 32/24 和大端设备格式没有对应 ALSA 格式 |
| `pack_into_alsa_formats_matches_independent_bytes` | N=16/20/24/32 的符号与极值打包到四种格式；`S24_LE` 与 WASAPI 32/24 的结果不同，各自对照独立字节；N > V 被拒绝 |
| `candidates_prefer_smallest_valid_bits_then_device_order` | 排序、设备偏好、空候选、去重 |
| `device_names_only_hw` | 9 个合法名与 25 个非法名 |
| `confirm_accepts_exact_parameters` | 精确参数通过；`88200/2` 等价于 44100 Hz；`sbits` 未知或 ≥ N 时通过 |
| `confirm_rejects_each_mismatch` | 近似采样率、非整数采样率、分母为 0、格式被改写、周期、缓冲、声道、访问方式、重采样、有效位不足，以及多项同时不一致时全部列出 |
| `confirm_rejects_request_that_cannot_carry_source` | N > V、采样率不同、声道不同 |
| `open_errors_are_classified` | 各 errno 的分类 |

写测试时手写期望值错了一处：把 `-32768 << 8` 写成了 `0xFFFF8000`。正确值是 −8388608 = `0xFF800000`，小端字节为 `00 00 80 FF`。已按算术修正这条期望，被测代码不变。

**变异检查**（检查后源码已恢复），以下 5 个变异全部被测试抓到：

- `S24_LE` 改为高位对齐：2 个测试失败；
- 不检查周期；
- 候选不按 V 排序；
- 接受 `plughw`；
- 采样率比较忽略分母。

## 验证

- fmt 通过。
- Clippy 通过：Linux（含 `ring` 特性）与 thumbv7em。
- `qy-pcm-format` 测试 21 项通过；启用 `ring` 特性时 26 项通过。
- thumbv7em 编译通过。

本卡是纯逻辑，没有设备相关测试。alsa-lib 绑定与真实设备在 T3。
