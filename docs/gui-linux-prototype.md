# LG0：Linux 自绘界面原型与测量

日期：2026-10-18。

**背景**：GUI T1 原定 Linux 用 GTK4（[gui-t1-selection.md](gui-t1-selection.md) §7）。A1 之后 Linux 版改为与发行版无关（每种架构一个 musl 静态程序），GTK4 必须由系统安装、无法静态打包，与这一点冲突。用户（2026-10-18）同意改为自绘界面，先做原型实测，再写任务卡。

原型在 `tools/gui-proto-linux/`：独立 Cargo 项目，不在工作区内，依赖版本锁定在该目录的 `Cargo.lock`。

## 1. 原型内容

样例与 GUI T1 相同：
- 工具栏六个按钮与设备说明；
- 1000 行播放列表，三列（标题、时长、格式），中文文字；
- 状态行每 250 ms 刷新一次；
- 另加单击选中、双击播放、滚轮、方向键 / PageUp / PageDown / Home / End / 回车 / 空格、文件拖放。

两种窗口后端共用同一绘制层 `src/ui.rs`。

**绘制层**：
- 状态与布局都在这一层，软件绘制。
- 用到的库：
  - tiny-skia 0.11.4：圆角按钮的抗锯齿；
  - cosmic-text 0.15.0：排版与字体回退，按 Unicode 处理双向文字与组合字符；字体由 fontdb 直接读系统字体目录与 fontconfig 的配置文件，不链接 libfontconfig；字形光栅化用 swash。
- 排版结果按字符串缓存，列表滚动时同一行文字不再重新排版。
- 只重绘变化的区域：刷新状态行时只重绘、只提交状态栏那一条。

两种窗口后端：

| | A：`proto-winit` | B：`proto-x11` |
|---|---|---|
| 窗口库 | winit 0.30.13 + softbuffer 0.4.8 | x11rb 0.13.2（`RustConnection`：纯 Rust 实现的 X11 协议，不含 `unsafe`） |
| 显示服务 | X11 与原生 Wayland | X11；Wayland 桌面经 XWayland |
| 系统库 | 运行时加载 libX11、libxcb、libxkbcommon(-x11)、libXcursor、libXi 等；Wayland 时加载 libwayland-client | 无，只用套接字与服务器通信 |
| 静态链接 | 不行：musl 静态链接时找不到 `-ldl`。winit 必须用 `dlopen` 加载系统库，只能用动态链接的 glibc 程序 | 可以：musl `static-pie`，`ldd` 显示 statically linked |
| 自己实现的部分 | 无（窗口、键盘布局、输入法、拖放、缩放都由 winit 处理） | 键盘用核心协议的键码表；缩放读 `Xft.dpi`；文件拖放实现 XDND 版本 5 接收方（约 80 行）；Expose 时直接重送已有像素 |

## 2. 测量

**环境**：
- Ubuntu 24.04 容器，4 核 Xeon 2.1 GHz；
- Xvfb，1280×800，24 位；
- Wayland 用 weston 13（嵌套在 Xvfb 中）。

**测量方法**：
- 原型的 `measure.sh`：程序启动 3 秒后，取 30 秒内进程的 CPU 时间（`/proc/<pid>/stat`），以及内存（VmRSS / VmHWM）。
- 运行时加载的共享库：从 `/proc/<pid>/maps` 统计。
- 绘制与提交耗时：`QY_PROTO_BENCH=1`，滚动 200 次、刷新状态行 200 次，程序内计时。
  - B 每次提交都等服务器处理完（`sync`），A 不等。
- A 用 glibc 动态链接构建，B 用 musl 静态构建，即两者各自的发布形态。

| | A：winit（glibc） | B：x11rb（musl 静态） | 参照：Windows 原生控件（GUI T1 §6） |
|---|---|---|---|
| 全新 release 编译 | 69 秒 | 36 秒 | 21 秒 |
| 程序大小（strip） | 5.6 MiB | 3.5 MiB（其他架构 2.9–3.2 MiB） | 0.14 MiB |
| 依赖包（去重） | 107 | 53 | 0 个新增 |
| 运行时加载的共享库 | 19 个 | 0 个 | 系统 DLL |
| 状态行刷新时 CPU（30 秒） | 80 ms（0.27%） | 100 ms（0.33%） | 109 ms（0.36%） |
| 静止时 CPU | 0 | 0 | 0 |
| 内存 VmRSS | 21 MiB | 13–14 MiB | 工作集 17 MiB |
| 首帧（启动到画面出现） | 30–46 ms | 31–35 ms | — |
| 整窗重绘 | 17 ms | 21 ms | — |
| 列表滚动一步：重绘 + 提交 | 1.1 + 0.8 ms | 1.5 + 1.4 ms | — |
| 状态行：重绘 + 提交 | 0.3 + 0.8 ms | 0.3 + 1.4 ms | — |

- 两者的刷新 CPU 与 Windows 原生控件同一量级，静止时都不占 CPU。
- 文字排版缓存前，列表滚动一步要 12–16 ms；缓存后约 1–1.5 ms。正式实现必须保留这类缓存。
- B 的整窗重绘比 A 慢约 20%，主要是 musl 的内存分配器较慢。界面只在窗口尺寸变化时整窗重绘，影响不大。
- B 提交时把 RGBA 转为 BGRX，并经套接字 `PutImage` 传给服务器。用 MIT-SHM 共享内存可以省去复制，但需要 `unsafe` 的系统调用；原型中没有用。

## 3. 功能核对（截图核对）

| 项目 | A：winit | B：x11rb |
|---|---|---|
| 中文显示（Xvfb，系统字体文泉驿正黑） | 正常 | 正常 |
| 原生 Wayland（weston） | 运行正常；但 winit 在 Wayland 上自绘的标题栏不显示中文（它的标题栏字体没有回退，“原型”两字成了方块） | — |
| Wayland 桌面经 XWayland（weston `--xwayland`） | — | 正常，标题栏由合成器绘制，中文正常；这个静态 musl 程序未作任何改动 |
| 点击、双击、滚轮、键盘（xdotool） | 未逐项核对（由 winit 处理） | 正常 |
| 文件拖放 | winit 0.30 只在 X11 上支持，Wayland 上不支持 | 正常。用测试拖放源 `xdnd-send`（同目录）拖入含中文、空格、`%` 的路径，按 RFC 3986 解码正确，并回送 `XdndFinished` |
| 高 DPI | 由 winit 提供缩放系数 | `Xft.dpi: 144` 时按 1.5 倍绘制，正常 |
| 输入法（中文输入） | winit 支持（X11 用 XIM，Wayland 用 text-input） | 未实现。要用 XIM 协议自己实现；播放器只有搜索框需要输入文字 |
| 读屏软件（无障碍） | 无 | 无 |

两者共同的限制：
- **字体**：直接读系统字体，系统没有中文字体时中文显示为方块，需要在程序中说明。
- **文件对话框**：自绘界面没有系统文件对话框。
  - 可以经 xdg-desktop-portal（D-Bus）调用桌面的对话框，但精简系统上没有这项服务；
  - 因此正式版需要一个自绘的简单文件浏览面板，再加拖放与命令行参数。

## 4. 结论与建议

**建议用 B（x11rb，纯 Rust X11，musl 静态）**：
- 与命令行版一致，每种架构一个文件，放到任何带 X11 或 XWayland 的发行版上都能直接运行，不依赖任何系统库。
- GNOME、KDE 等主流 Wayland 桌面都带 XWayland，在这些桌面上不需要另外安装。
- 资源占用与 Windows 原生控件同一量级；依赖少一半，编译快一倍，程序小 40%，内存少约 1/3。
- 不含 `unsafe`（x11rb 不启用 `allow-unsafe-code` 特性时禁止 `unsafe`）。

**B 的代价**：
- 不是原生 Wayland。在分数缩放的 Wayland 桌面上，X11 程序可能被合成器放大而显得模糊（取决于桌面版本与设置）。
- 输入法、文件浏览面板要自己做。以后需要时，可以再加一个纯 Rust 的原生 Wayland 后端（wayland-client 的 Rust 实现），绘制层不变。

**A（winit）的情况**：
- 原生 Wayland 和输入法现成可用。
- 但程序必须动态链接 glibc，并在运行时依赖系统的 X11 / xkbcommon / Wayland 库，与 A1 定下的“与发行版无关、完全静态”不一致；Wayland 上的标题栏也不显示中文。

**另一种可能**：没有桌面的机器可以直接画到 Linux 帧缓冲（DRM/KMS 或 `/dev/fb0`），从 `/dev/input` 读键盘鼠标。同一个绘制层再加一个后端即可，也能静态链接，适合接显示屏的单板机。原型没有做，只作为以后的选项记录在这里。

## 5. 运行原型

```
cd tools/gui-proto-linux
cargo build --release                                   # proto-winit、proto-x11、xdnd-send
cargo build --release --no-default-features --features x11-backend \
  --bin proto-x11 --target x86_64-unknown-linux-musl     # 静态（链接设置见仓库 .cargo/config.toml）
QY_PROTO_STATIC=1 target/release/proto-x11              # 不刷新状态行
QY_PROTO_BENCH=1  target/release/proto-x11              # 测量后退出
./measure.sh <程序> <输出目录>                           # 需要已运行的 X 服务器（DISPLAY）
```
