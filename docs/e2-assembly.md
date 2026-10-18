# E2：应用组装入口

日期：2026-10-18。任务卡：[tasks/E2.md](../tasks/E2.md)。本卡的结果来自单元测试、集成测试与假设备（模拟时钟）上的界面模型会话，**不是实机结果**。

## 1. 问题与做法

命令行 `qybit`、图形界面 `qybit-gui` 与界面模型 `qy-ui` 的文件信息读取原来固定使用官方解码器（`qy-codec-auto`），插件仓库无法编出带 Lite / Full 格式的同一套程序。现在：

- 一套格式用 `qy_player::Formats` 描述（`crates/qy-player/src/assembly.rs`），应用主体对它泛型；
- `qybit` 与 `qybit-gui` 的主体改为库，二进制只做组装：官方发行版 `qybit::run(qy_player::Builtin)`、`qybit_gui::run(qy_player::Builtin)`；
- 插件仓库实现自己的 `Formats`，调用同一个入口，得到同一个命令行与图形界面（播放路径、报告、选项与严格规则都相同，只是格式不同）。

核心、解码器与 `Decoder` 契约没有改动；静态分发不变（`Formats::Decoder` 是关联类型，不用 trait 对象解码）。

## 2. 接口

```rust
pub trait Formats: Send + Sync + 'static {
    type Decoder: Decoder + Resources + Diagnostics;
    fn resources(&self) -> Result<HostResources<Self::Decoder>, String>; // 播放（播放线程内调用）
    fn decoder(&self) -> Result<(Self::Decoder, usize), String>;        // `qybit decode`：解码器与输入窗口字节数
    fn prober(&self) -> Result<Box<dyn Probe>, String>;                 // 文件信息读取（读取线程内调用）
    fn extensions(&self) -> &[&'static str];                            // 播放列表、文件对话框
    fn names(&self) -> &str;                                            // `--help` 与“关于”
    fn is_audio_path(&self, path: &Path) -> bool { /* 按 extensions，默认实现 */ }
}

pub trait Probe {
    fn probe(&mut self, path: &Path) -> Result<TrackInfo, String>;
}

pub struct TrackInfo { pub label: &'static str, pub format: SourceFormat, pub frames: Option<u64> }
```

- 播放资源与文件信息读取器都在使用它们的后台线程里创建，所以解码器类型不需要 `Send`（嵌入式也用的解码器不必为桌面线程模型改写）；`Formats` 本身在线程间共享，只要求 `Send + Sync`。
- `qy_player::assembly::read_header(decoder, window, limit, path)`：用任意解码器读文件头到格式确定为止（与播放同一套解析与拒绝规则，不另写解析器；读取总量至多 16 MiB）。插件实现 `Probe` 时直接用它，再按自己的解码器给出格式名、声明的帧数与失败原因。
- `Diagnostics::source_notes`（新增，默认无）：格式的补充说明，`qybit decode` 输出；官方解码器写 AIFF-C 的非 `NONE` 类型（F5）。
- `qy_player::Builtin`：官方解码器（WAV 含 RF64 / BW64、FLAC、AIFF / AIFF-C），扩展名 `qy_player::assembly::BUILTIN_EXTENSIONS`，名称 “WAV / FLAC / AIFF”，资源与 `HostResources::new()` 相同。

### 应用侧的入口

| crate | 入口 | 格式集合用在 |
| --- | --- | --- |
| `qybit` | `qybit::run(formats) -> ExitCode`；`qybit::run_args(&formats, &args)` | `--help` 的格式名称；`decode` 的解码器与补充说明；播放资源；`--device` 后误跟音频文件名的识别 |
| `qybit-gui` | `qybit_gui::run(formats) -> ExitCode`（Windows 原生控件、Linux 自绘，其他平台明确报错） | 播放、文件信息读取、文件夹扫描、Windows 文件对话框的筛选（运行时按扩展名生成）、Linux 文件面板的筛选与标题、“关于”中的格式名称 |
| `qy-ui` | `Ui::with_formats(formats, make_backend, notify)`（`Ui::new` 等于传入 `Builtin`）；`Ui::extensions()`、`format_names()`、`is_audio_path()` | 同上（Windows 与 Linux 前端共用） |

`qy-ui` 原来的 `Prober` / `Kind` 去掉了：格式名由 `TrackInfo::label` 给出（格式列为“`label` 采样率 位数 声道”），文件信息读取统一走 `Formats::prober`。

## 3. 示例组装（`examples/qy-ext-demo`）

`qy_ext_demo::ExtFormats`（`src/formats.rs`）：解码器为 X1 的组合解码器 `ExtDecoder`（QYT1 / QYL1 与 `AutoDecoder`），扩展名为官方的再加 `qyt1`、`qyl1`，名称 “WAV / FLAC / AIFF / QYT1”；文件信息读取用 `read_header`，格式名按选中的解码器给出（QYL1 由 E1 的有损标记区分）。二进制：

- `qybit-ext`（`src/bin/qybit-ext.rs`）：`qybit::run(ExtFormats)`；
- `qybit-ext-gui`（`src/bin/qybit-ext-gui.rs`，`gui` 特性，测试不需要窗口系统）：`qybit_gui::run(ExtFormats)`。

插件仓库照此组装：

```rust
// 插件仓库的命令行二进制
fn main() -> std::process::ExitCode {
    qybit::run(my_plugin::LiteFormats)
}
```

## 4. 行为变化（官方二进制）

播放、报告与严格规则不变。文字上有三处随之改为由格式集合给出：

- `--help`：“按顺序播放 WAV/FLAC/AIFF” → “按顺序播放 WAV / FLAC / AIFF”；
- “关于”：“本地无损播放器：WAV / FLAC / AIFF，自有解码” → “本地播放器：WAV / FLAC / AIFF”（插件组装可能含有损格式，“无损”不再对所有组装成立）；
- Windows 文件对话框的筛选名：“音频文件（WAV、FLAC、AIFF）” → “音频文件（WAV / FLAC / AIFF）”，扩展名列表不变。

`qybit decode` 的输入窗口改为由 `Formats::decoder` 给出（官方与原来相同），pack 缓冲按需增长。

## 5. 验证

| 测试 | 内容 | 结果 |
| --- | --- | --- |
| 全部既有测试（`cargo test --workspace`） | 命令行参数与报告、界面模型会话（假设备）、文件信息读取（FLAC / WAV / AIFF 夹具的格式与声明帧数，与 `.meta` 参考一致）、文件夹扫描 | 通过 |
| `qybit-gui --features sim` 测试 | Linux 自绘前端的文件面板筛选（官方扩展名）等 | 通过 |
| `examples/qy-ext-demo/tests/assembly.rs` | `qybit-ext --help` 列出 “WAV / FLAC / AIFF / QYT1”；`qybit-ext decode` 把 QYT1、QYL1 写成 WAV，data 块与测试自己生成的样本逐字节相同，只有 QYL1 的报告写“有损”；WAV 经同一组合解码器解码，data 块与源相同；`--device a.QYT1` 识别为误跟的音频文件，`a.mp3` 不识别 | 通过 |
| 同上，界面模型 | `Ui::with_formats(ExtFormats, …)` 扫描文件夹：只加入 3 个文件（`.mp3` 不加入），格式列为 “QYT1 44.1 kHz 16 位 2 声道”、“QYL1 44.1 kHz 解码 16 位 2 声道 有损”、“WAV 44.1 kHz 16 位 2 声道”，声明帧数正确；假设备（模拟时钟，真实核心、组合解码器、pack 与渲染循环）播放 QYT1 与 QYL1，送出的有效字节以测试生成的样本开头，软件路径完整 | 通过 |
| Linux 图形界面 Xvfb 核对（`scripts/gui-linux-check.sh`，官方 `qybit-gui` 模拟设备，本地运行） | 状态栏与指示灯、单声道拒绝与复制、拖动排序、XDND 拖放、记住设置与恢复，与原来相同 | 通过 |
| Windows 图形界面模拟设备核对 | CI 的 Windows 作业（runner 上的 exe 截图与交互） | 见提交后的 CI |
| Clippy | Linux 全工作区 `--all-targets`；`qy-ext-demo --features gui`；Windows（`x86_64-pc-windows-gnu` 交叉检查）`qy-host-wasapi`、`qybit`、`qybit-gui`（含 `sim`）、`qy-ext-demo --features gui`；示例图形界面二进制在 Windows 目标上编译 | 通过 |
| CI | Windows 作业增加示例组装的 Clippy 与编译；Linux 图形界面作业增加示例组装（`gui` 特性）的 Clippy；示例测试随原有 Linux 测试作业运行 | 见提交后的 CI |

## 6. 未做

- 示例图形界面 `qybit-ext-gui` 没有做截图核对：它与官方 `qybit-gui` 是同一个主体，格式相关的部分（列表、筛选、名称）由上面的界面模型测试覆盖。
- 运行时动态加载插件不在范围内（X1 §6）。
