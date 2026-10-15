# M2 T5：验证加强（Kani、cargo-mutants、场景 fuzz、Miri）

日期：2026-10-15。任务卡：[tasks/M2.md](../tasks/M2.md) §T5。开发机为 4 vCPU 的 KVM 虚拟机（见 [r1-reliability.md §6.1](r1-reliability.md)）。本卡全部是软件验证，不涉及真实设备。

## 1. 工具与版本

| 工具 | 版本 | 用途 |
| --- | --- | --- |
| Kani | kani-verifier 0.68.0（CBMC 6.11.0，Kani 自带 nightly-2026-08-21） | 有界模型检查 |
| cargo-mutants | 27.1.0 | 变异测试 |
| cargo-fuzz / libFuzzer | 0.13.2（CI 用 nightly-2026-10-12） | 场景 fuzz |
| Miri | nightly 2026-10-12（miri 0.1.0 e9bbadf682） | 未定义行为检查 |
| rustc（常规构建与测试） | 1.97.0（`rust-toolchain.toml`） | — |

CI：`.github/workflows/verify.yml` 每周一与手动运行 Kani 与 cargo-mutants（不阻塞提交）；场景 fuzz 加入 `ci.yml` 的 fuzz 冒烟（每次提交 60 秒）。

## 2. Kani 有界证明

命令：在各 crate 目录 `cargo kani`。全部 24 个证明通过，没有删除或放宽断言。

| 证明 | 内容与边界 | 求解时间 |
| --- | --- | --- |
| `qy-pcm-format` pack，16 个 | 对每种容器 C ∈ {16, 24, 32}、放置方式（高位/低位）与字节序（小端、大端、半字交换；24 位不能半字交换）：所有 1 ≤ N ≤ V ≤ C 与任意 N 位样本，`placement_shift` + `place_sample` 不 panic、不溢出；按独立方法解包（按字节序取回容器字、符号扩展、取出 V 位、去掉 V−N 位零填充）后等于原样本，填充位为 0 | 1.3–3.5 秒 / 个 |
| `qy-pcm-format` 24 位半字交换 | 任意有效位与放置方式下构造被拒绝（pack 因此不必处理） | 0.1 秒 |
| `qy-codec-flac` CRC-8 / CRC-16 | 查表实现与按位定义（逐位移位、最高位为 1 时异或多项式）对长度 0–4 的任意输入相等 | 5–7 秒 |
| `qy-codec-flac` STREAMINFO | 34 字节全为任意值：解析与能力检查不 panic；成功时声道 1–8、位深 4–32、采样率 > 0、最大块 ≥ 16 | 6.5 秒 |
| `qy-codec-wav` RIFF 头、块头、fmt | 任意字节（块头另加任意的 RIFF 剩余长度与“data 已读完”状态；fmt 另加任意声明长度）：不 panic；fmt 成功时块对齐 = 容器字节 × 声道数，右移量 < 32 | 1.4–9.6 秒 |
| `qy-codec-aiff` 80 位采样率 | 任意 10 字节：不 panic（移位量受检查）；结果与独立计算的精确值一致，`Some(r)` 时 r ≥ 1 且 r 精确等于 m × 2^(e − 16383 − 63)，正的有限整数且 ≤ `u32::MAX` 时必为 `Some` | 3.0 秒 |

**证明边界**：
- pack 的证明对象是单个样本的位移与按字节序写出，这是 pack 中唯一的数值运算，为此从 `pack_with` 中抽出了 `placement_shift` 与 `place_sample` 两个函数（行为不变）。对整个 `pack` 做证明时，每个组合 28 分钟以上仍未求解完（内含 18 个声道位置的映射循环与迭代器）。声道映射、逐帧遍历与范围检查仍由单元测试、往返测试与 fuzz 覆盖。
- 解码器的证明对象是文件头解析函数，不是整个解码状态机：对整个 `decode` 的证明同样超过 15 分钟未完成。
- 任务卡写于 F2 之前，未列 AIFF；AIFF 的 80 位采样率换算适合做精确性证明，一并加入。

## 3. cargo-mutants

### 3.1 运行方式

`.cargo/mutants.toml` 排除 Kani 证明（`proofs::` 模块不是测试），并且不复制被 `.gitignore` 忽略的文件（否则子目录中的 `target` 会占满构建目录的磁盘）。

| crate | 命令 | 测试范围 |
| --- | --- | --- |
| `qy-codec-wav` | `cargo mutants -p qy-codec-wav` | 本 crate |
| `qy-codec-flac` | `cargo mutants -p qy-codec-flac --features static-cell,test-metrics` | 本 crate（含 MCU 静态缓冲与统计特性的测试） |

### 3.2 结果

| crate | 变异总数 | 首轮 存活 | 补测试后 被杀 / 存活 / 超时 / 不可编译 | 用时 |
| --- | --- | --- | --- | --- |
| `qy-codec-wav` | 312 | 31 | 280 / 5 / 2 / 25 | 2 分钟 |
| `qy-codec-flac` | 771 | 129 | 691 / 18 / 14 / 48 | 22 分钟 |

超时是变异后陷入死循环（例如循环步长被改为 `*=`），由超时检出，计为发现。

### 3.3 补充的测试（首轮存活项对应的真实缺口）

**WAV**（`crates/qy-codec-wav/tests/boundaries.rs`）：
- 码流识别的近似样本：14 位 DTS 同步模式的半字节、16 位 DTS 帧头各字段的边界与下一帧同步字、IEC 61937 的数据类型与长度；
- 码流扫描只针对 16 位双声道整容器 PCM（单声道、24 位、有效位少于容器的文件照常按 PCM 播放）；
- fmt 的零字段、EXTENSIBLE 的 `cbSize` 越界、奇数长度 fmt 的填充；
- RIFF 长度为占位值或写短时块的填充与结束情形；
- 明细文字。

**FLAC**（`src/frame.rs` 单元测试，`tests/boundaries.rs`）：
- 帧级：类 UTF-8 编号 1–7 字节全部长度与非法编码；帧头的全部块大小码、采样率码、位深码；只差一项的流参数不一致；侧声道解相关后单个声道溢出；预测阶数等于/大于块大小；首分区样本数等于/小于预测阶数；残差 zigzag 上界 2^32 − 1 与溢出；
- 流级：STREAMINFO 位深、最大块、采样率的边界；声明的最大帧长度超出窗口；元数据块顺序与长度；结束时总样本数核对（含 > 2^32）；声道掩码标签的识别（超长字段不解析、长字段前后、尾部字节、十六进制位数）；窗口满时 EOF 为截断；
- 其他：`FlacBuffers::check` 逐项检查、读越界时的位置、重试统计。

**代码化简**（不改变行为，变异测试指出的死代码）：FLAC 中一个只有注释的空 `if` 块；重试门槛中 n ≥ 1 时不起作用的 `.max(n + 1)`。

### 3.4 存活变异的结论

**WAV（5 个，均为等价变异）**：

| 位置 | 变异 | 结论 |
| --- | --- | --- |
| `dts16_frame` 合并 48 位帧头（2 处） | `\|` → `^` | 三个 16 位字左移后互不重叠，或与异或相同 |
| `step`：`tail > 0` | → `>=` | tail 为 0 时把“丢弃 0 字节”记为 0，与不记相同 |
| `step`：`rest > 0` | → `>=` | 同上 |
| `step`：码流扫描 `avail.len() < need` | → `<=` | 恰好够时多等一次输入，扫描的窗口相同，结果与分块无关 |

**FLAC（18 个）**：

| 位置 | 变异 | 结论 |
| --- | --- | --- |
| `MAX_FRAME_HEADER_BYTES` 中的 `2 + 2` | → `2 * 2` | 巧合相等（都是 4） |
| `with_min_window`：`>` | → `>=` | 相等时两个分支取同一个值 |
| `DESKTOP_MULTICHANNEL` 的最小窗口 `1 << 20` | → `>>` | 8 声道的 verbatim 上界（2 097 146 字节）已大于 1 MiB，最小窗口不起作用 |
| 位字段合并（STREAMINFO 4 处、声道掩码十六进制、`Bits::read` / `read_signed`、编号、mid/side、残差 zigzag，共 10 处） | `\|` → `^` | 合并的位互不重叠 |
| `step`：标记 `avail.len() < 4` | → `<=` | 恰好 4 字节时多等一次输入；EOF 时两者都以截断结束 |
| `subframe`：`k > 0`（wasted bits 左移） | → `>=` | 左移 0 位不变 |
| `read_signed` 的 0 位分支 | 删除 | 不可达：有效位深 ≥ 1（wasted bits 小于子帧位深），escape 的 0 位单独处理 |
| 残差一元码商 `q > u32::MAX`（2 处） | → `>=` / `==` | 不可达：商达到 2^32 需要至少 2^32 个连续 0 位（512 MiB），超过任何输入窗口；该检查是防御性的 |

## 4. 待补

`qy-pcm-format`、`qy-codec-auto`、`qy-core` 的变异测试，场景 fuzz 的 10 分钟记录与 Miri 结果正在运行，完成后写入本节之前。
