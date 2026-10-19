# E4：接口版本

日期：2026-10-19。任务卡：[tasks/E4.md](../tasks/E4.md)。**版本规则（§2）请用户审定**；审定前按本文执行，改动时同步修改本文、CHANGELOG 与兼容检查脚本。

## 1. 版本与依赖方式

- 工作区所有 crate 共用一个版本号（`Cargo.toml` 的 `[workspace.package] version`），当前为 **0.1.0**；对应 git tag `v0.1.0`。版本记录见 [CHANGELOG.md](../CHANGELOG.md)。
- 暂不发布 crates.io（AGENTS §4：先在 GitHub 开发）。插件仓库按 tag 以 git 依赖：

```toml
[dependencies]
qy-core = { git = "https://github.com/DKturing/qybit", tag = "v0.1.0" }
qy-player = { git = "https://github.com/DKturing/qybit", tag = "v0.1.0" }
qybit = { git = "https://github.com/DKturing/qybit", tag = "v0.1.0" }        # 组装命令行（E2）

[dev-dependencies]
qy-testkit = { git = "https://github.com/DKturing/qybit", tag = "v0.1.0" }   # 契约测试
```

同一个仓库的多个 crate 必须用同一个 tag（Cargo 按 tag 解析为同一个提交，类型才是同一个）。依赖 `branch = "main"` 不受下面的规则保护。

## 2. 兼容规则（请审定）

按 Cargo 的 SemVer 约定，0.x 阶段以**次版本号**区分兼容系列：

| 修改 | 版本 | CHANGELOG |
| --- | --- | --- |
| 破坏性修改（见下） | 次版本号 +1，修订号归零（0.1.3 → 0.2.0） | 在新版本条目下写“破坏性”小节，逐项写明旧写法与新写法 |
| 兼容的新增与修正 | 修订号 +1（0.1.0 → 0.1.1） | “新增” / “修正” |
| 只改报告措辞、文档、测试、内部实现 | 不必单独发版，随下一个版本 | 可不写 |

**算破坏性的修改**（对 §3 范围内的接口）：

- 删除或改名公开的类型、函数、方法、trait、常量、模块路径或 Cargo 特性；改变签名（参数、返回类型、泛型约束）；
- 给 trait 增加**没有默认实现**的方法（实现它的插件不再能编译）；增加有默认实现的方法不算（E5 的 `take_preroll` 属于这一种）；
- 给公开的 `enum` 增加变体（插件的穷尽 `match` 不再能编译）；给字段全公开的结构体增加字段；
- 不改签名但改变契约语义，使原来遵守契约的插件变为违约（例如收紧 `Decoder` 的状态规则、改变 `i32` 样本的对齐约定、改变 `Identify` 各回答的含义）；
- 提高最低 Rust 版本（`rust-version`）。

**不算破坏性**：报告、命令行与界面的文字（不是机器接口，测试只断言关键片段）；`#[doc(hidden)]` 项、`test-support` 等测试特性下的项；新增类型、函数与有默认实现的 trait 方法；修正与契约文档不一致的错误行为（在 CHANGELOG 中写“修正”，如果插件可能依赖了错误行为，同时写明影响）。

1.0 之后改为以主版本号区分兼容系列，规则同上。何时到 1.0 由用户决定。

## 3. 对外接口的范围

插件仓库与板级代码可以依赖的接口（以下 crate 的公开项，除 §2 的例外）：

| crate | 内容 |
| --- | --- |
| `qy-core` | `Decoder`、`Status`、`Progress`、`DecodeError`、`SeekError`、`Identify`、`magic_matches`；`SourceFormat`、`ChannelLayout`、`SampleEncoding` 与 DSD 类型（重新导出自 `qy-pcm-format`）；容量常量（`INPUT_WINDOW_BYTES`、`PCM_WORK_SAMPLES`、`MAX_CHANNELS` 等）；`engine` 的 `Resources` 与宿主所用的 `Engine`、`HostAction`、配置档；`Core` / `Action` / `Event`（M0 T4 起冻结） |
| `qy-pcm-format` | `DeviceFormat`、DSD 传输类型、`pack`、帧队列的公开类型（`ring`、`static-queue` 特性） |
| `qy-codec-wav` / `-flac` / `-aiff` | 解码器构造函数、`identify`、错误明细类型 |
| `qy-codec-auto` | `AutoDecoder`（构造、`identify`、`codec`、明细）、`Codec`、`AutoDetail`、`profile` 配置档、`wav` / `flac` / `aiff` 特性 |
| `qy-player` | `HostResources::with_decoder`、`Diagnostics`、`Formats` / `Builtin` / `Probe` / `TrackInfo` / `read_header`（E2）、`Backend` / `PendingStream` / `RunningStream`、`TrackSource`、`play_with` / `PlayControl` / `Command` / `command_channel` / `Status`、`PlaybackReport` 的字段与方法 |
| `qy-host-embedded` | 存储、输出与喂数器的 trait 及宿主入口（板级适配用） |
| `qybit` / `qybit-gui` | `qybit::run`、`qybit::run_args`、`qybit_gui::run`（E2） |
| `qy-testkit` | 契约测试工具，见 §4 |

其余 crate（`qy-host-sim`、`qy-host-alsa`、`qy-host-wasapi`、`qy-ui`、`qy-gen-fixtures`、`qy-fat-image`、`qy-bench`、`qy-mcu-demo`、`qy-board`）是本仓库的应用与工具，不作对外承诺。契约本身（各扩展点必须遵守的约定）见 [docs/x1-extensions.md](x1-extensions.md)，它的版本就是本仓库的版本。

## 4. `qy-testkit`

公开的契约测试工具，只作 dev-dependency（§1 的写法）。函数：

- `check(make, sources)`：每个源分别用一次性、逐字节与随机分块输入解码，再用同一个解码器依次解码全部源（中间 `reset`），检查状态语义、格式不伪造、分块无关与复位复用；返回每个源的 `Decoded`（样本与结束方式），由插件与自己的独立参考逐个比较；
- `check_seek(make, file, targets)`：定位后只输出目标帧及之后（含 E5 的预滚），不支持时返回 `Unsupported`；
- `check_identify(identify, decoder, detail, file)`：文件头识别（E3）在所有前缀上与完整解码一致；
- `decode(decoder, file, chunking)`：单次解码，供插件自己的测试使用。

违约时返回 `Violation`，文字写明哪一条、哪一步。`qy-testkit` 的公开项同样受 §2 约束；给它增加新的检查函数不算破坏，但让原来通过的正确解码器不再通过的收紧算破坏（应该先在 CHANGELOG 中说明依据）。

## 5. 兼容检查

`scripts/compat-check.py TAG`：取出 TAG 时的 `examples/qy-ext-demo`（代表一个按该 tag 编写的仓库外插件：解码器、识别、预滚、DSD、桌面与嵌入式组装、命令行组装，以及它的全部测试），把对本仓库 crate 的依赖改为指向**当前**源码树，作为独立工作区编译库、二进制与测试（不运行）。

- 当前版本与 TAG 属于同一兼容系列时，编译失败即破坏性修改，退出码 1；
- 已升到新的兼容系列时，编译失败只作提示（退出码 0），但 `CHANGELOG.md` 必须有新版本的条目，否则失败。

CI 的 `compat` 作业以 `git describe --tags --abbrev=0 --match 'v*'` 找到最近的版本 tag 运行它；还没有 tag 时跳过。

它能发现的是“编译不通过”的破坏（签名、删除、enum 变体、trait 必需方法）；语义上的破坏要靠契约测试与 CHANGELOG 审查，脚本不保证发现。

**验证**（2026-10-19，开发机）：

| 情形 | 结果 |
| --- | --- |
| 对当前提交（未修改） | 通过 |
| 故意破坏：给 `qy_player::read_header` 增加一个参数，版本不变 | 失败，E0061（示例 `src/formats.rs` 调用参数个数不符），输出“兼容检查失败……（破坏性修改）”，退出码 1 |
| 故意破坏：给 `Decoder` 增加一个没有默认实现的方法，版本不变 | 失败，E0046（实现 `Decoder` 的 crate 缺少该方法），退出码 1 |
| 同一破坏，版本升到 0.2.0 但 CHANGELOG 没有条目 | 失败：要求 CHANGELOG 条目，退出码 1 |
| 同一破坏，版本 0.2.0 且 CHANGELOG 有 `## [0.2.0]` | 提示“已声明为新的兼容系列”，退出码 0 |

验证后故意破坏均已还原。
