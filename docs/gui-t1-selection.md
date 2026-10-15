# GUI T1：技术选型记录

日期：2026-10-15。任务卡：[tasks/GUI.md](../tasks/GUI.md) §T1。用户要求：选“最好、最现代、最省资源、技术最新”的一个。

**第一轮结论**：三个框架中 Slint 1.18.1 最省资源（§4）。用户随后问到自写界面与其他选择，第二轮加测 Windows 原生控件（§6），最终选择待用户决定。

## 1. 候选与样例

三个纯 Rust 框架，样例内容相同（`tools/gui-eval/`，见 §5 的提交）：工具栏两个按钮、1000 行播放列表（文件名、时长、格式三列，中文文字）、每 250 ms 刷新一次的状态行（模拟播放进度显示）。各自是独立 Cargo 项目，依赖版本锁定在各自的 `Cargo.lock`。

| 框架 | 版本 | 许可证 | 最低 Rust | 架构 |
| --- | --- | --- | --- | --- |
| egui / eframe | 0.36.2 | MIT 或 Apache-2.0 | 1.95 | 即时模式：每次重绘重新执行整个界面代码 |
| iced | 0.14.0 | MIT | 1.88 | Elm 架构（消息 → 状态 → 视图），wgpu 渲染，无 GPU 时 tiny-skia 软件渲染 |
| Slint | 1.18.1 | GPL-3.0，或 Slint 免版税许可 2.0，或商业许可 | 1.92 | 声明式界面语言（`.slint`）编译成 Rust，按属性变化局部重绘 |

## 2. 测量（`.github/workflows/gui-eval.yml`）

环境：GitHub Actions `windows-latest` 与 `ubuntu-latest`。**Windows runner 没有 GPU，所有框架都是软件渲染**，CPU 数值是最不利情形；有独立显卡或核显的机器上 egui/iced 的数值会低得多，但三者的相对关系（是否整窗重绘）不变。

空闲 CPU：窗口打开后不操作，30 秒内进程 CPU 时间（`TotalProcessorTime` 差值）。“刷新”为状态行每 250 ms 更新一次，“静止”为不刷新（环境变量 `QY_EVAL_STATIC=1`）。

### 2.1 Windows（Slint 三次、另两个两次运行）

| | 全新 release 编译 | exe 大小 | 刷新时 CPU（30 秒） | 静止 CPU | 工作集 |
| --- | --- | --- | --- | --- | --- |
| Slint | 279–289 秒 | 13.7 MiB | 62–125 ms（0.2–0.4% 单核） | 0 | 25–29 MiB |
| iced | 232 秒 | 10.7 MiB | 9.9–13.0 s（33–43%） | 0 | 74–80 MiB |
| egui | 169 秒 | 13.7 MiB | 4.6–69.8 s（15–233%，两次差别大） | 0 | 90–93 MiB |

- iced 的第一版样例每次刷新都重建 1000 行，改为按依赖缓存（`lazy`，与另两个同样不随状态行重建）后数值基本不变（37% → 33–43%），说明开销在每次整窗重绘，而不是界面构建。
- egui 是即时模式，每次刷新重新执行并重绘整个界面；软件 OpenGL 下两次运行差别很大。
- Slint 只重绘变化的状态行。

### 2.2 Linux（编译）

| | 编译 | 可执行文件 | 构建/运行依赖的系统库 |
| --- | --- | --- | --- |
| Slint | 174 秒 | 22 MB | fontconfig、freetype（构建时需要 `libfontconfig1-dev`；桌面系统都已安装运行库） |
| iced | 125 秒 | 16 MB | 只有 libc（窗口系统库运行时加载） |
| egui | 227 秒 | 20 MB | 只有 libc |

### 2.3 依赖数（`cargo tree` 去重）

| | Windows | Linux |
| --- | --- | --- |
| Slint | 248 | 344 |
| iced | 161 | 239 |
| egui | 182 | 271 |

### 2.4 中文显示（截图核对）

- iced：正常（cosmic-text 按系统字体回退）。
- egui：自带字体不含汉字，需要程序自己从系统字体目录加载中文字体；加载后正常。
- Slint：未指定字体时，“第 / 首 / 曲 / 目”正常，而简体专有的“测 / 试”缺失——字体回退选到了不含简体字形的字体。指定 `default-font-family`（Windows：Microsoft YaHei UI）后完整显示（第三次运行的截图核对），CPU 与内存不变。

## 3. 功能核对

| 需要的功能 | Slint | iced | egui |
| --- | --- | --- | --- |
| 播放列表表格 | `StandardTableView`（虚拟化） | `table`（0.14 起） | `egui_extras::TableBuilder`（虚拟化） |
| 系统文件拖放 | 1.18.1 未转交；经 `unstable-winit-030` 特性的 `on_winit_window_event` 收到 winit `DroppedFile`（已在样例中编译通过，安全代码） | `window::Event::FileDropped` | `dropped_files` |
| 系统文件对话框 | 用 rfd | 用 rfd | 用 rfd |
| Windows 原生风格 | 内置 Fluent 风格（Windows 11 外观） | 自绘主题 | 自绘主题 |
| 跨平台 | Windows / Linux / macOS | 同 | 同 |

## 4. 选择理由

1. **最省资源**：刷新时 CPU 比 iced 低约 100 倍、比 egui 低 70–600 倍；内存约为另两者的 1/3。播放器长时间开着，这一项最重要；音频线程也因此少受界面干扰。
2. **现代**：声明式界面语言与 Rust 逻辑分离，自带 Fluent（Windows 11）风格控件，有 LSP 与实时预览；1.18 为当前版本。
3. **功能齐全**：表格、按钮、下拉框、菜单等标准控件齐备，接近 foobar2000 的布局不需要自绘控件。

需要接受的代价：
- **许可**：选用 Slint 免版税许可 2.0（`LicenseRef-Slint-Royalty-free-2.0`，原文见 crate 内 `LICENSES/`）。条件：在程序的“关于”对话框中显示 `AboutSlint` 控件（或在下载页放 Slint 标识），不得单独再分发 Slint，不得用于嵌入式设备。QyBit 自己的代码仍为 MIT/Apache-2.0；GUI 只用于桌面程序 `qybit-gui`，单片机部分不使用 Slint。
- **拖放**依赖 `unstable-winit-030` 特性（Slint 声明该模块可能在小版本间变化）；版本已锁定，升级 Slint 时需复查。
- **字体**：须显式指定中文字体族（Windows 用 Microsoft YaHei UI；Linux 阶段按 fontconfig 选 Noto Sans CJK SC 等，届时核对）。
- **Linux 构建**需要 `libfontconfig1-dev`。

## 5. 后续

- GUI T2 新建 `qy-gui`（二进制 `qybit-gui`），使用 Slint。
- 选型样例与测量工作流已删除，最后版本见提交 ca09a30（`tools/gui-eval/`、`.github/workflows/gui-eval.yml`），结果保留在本文与 CI 历史（“GUI 选型测量”第 1–4 次运行）中。
- 有 GPU 的真实 Windows 机器上的 CPU 占用，在 GUI T3 真机运行时一并记录。

## 6. 第二轮：Windows 原生控件（2026-10-15）

用户问：自己写 GUI 是否麻烦、有没有其他更省资源的选择。

- **完全自绘的界面库**（自己做渲染、文字排版、输入）：不建议。中文字形与排版、输入法、高 DPI、无障碍、列表滚动等都要自己实现，工作量相当于一个界面框架，远大于播放器本身。
- **Windows 原生控件**：用项目已有的微软官方 `windows` crate（0.62.2，MIT/Apache，WASAPI 已在使用）直接创建系统控件（按钮、ListView、状态栏）。界面由 Windows 绘制，中文、输入法、高 DPI、无障碍、文件拖放（`WM_DROPFILES`）、文件对话框都由系统提供；foobar2000 的默认界面就是这类原生控件。代价：只适用于 Windows，Linux 需要另写一个前端（GTK4，`gtk4` crate，MIT）；调用系统接口需要 `unsafe`，按 AGENTS 规则集中在一个模块并单独审计；外观为经典 Windows 风格（不是 Windows 11 Fluent），深色模式系统不正式支持。
- **其他框架**：fltk-rs（很轻，但内含 C++ 的 FLTK，不是纯 Rust，外观较旧）；floem、xilem、freya（新，GPU 渲染，尚未 1.0、接口不稳定，资源占用不会低于 Slint）；gtk4-rs 在 Windows 上需要随程序分发 GTK 运行库（数十 MB）。

同场测量（样例内容同 §1，加文件拖放；原生样例 `tools/gui-eval/win32/`）：

| | 全新编译 | exe | 刷新时 CPU（30 秒） | 静止 | 工作集 | 新增第三方依赖 |
| --- | --- | --- | --- | --- | --- | --- |
| Windows 原生 | 21 秒 | 0.14 MiB | 109 ms（0.36%） | 0 | 17 MiB | 无（`windows` 已在用） |
| Slint | 267 秒 | 13.7 MiB | 16 ms（0.05%） | 0 | 25–27 MiB | 248 个包 |

- 两者刷新时 CPU 都在 0.05–0.4% 之间，属于测量噪声量级（Slint 四次运行为 16–125 ms），可视为相同。
- 原生：exe 小约 100 倍、编译快约 13 倍、内存少约 1/3，没有新增依赖与许可条件；中文无需指定字体（系统字体链接）。截图核对显示正常。
