# GUI T1 选型样例

选型样例（第一轮 egui/eframe、iced、Slint，已删除，见 docs/gui-t1-selection.md；第二轮 Windows 原生控件 `win32/` 与 Slint），内容相同：工具栏按钮、1000 行播放列表、每 250 ms 刷新一次的状态行、中文文字。各自是独立的 Cargo 项目（不属于主工作区），依赖版本锁定在各自的 `Cargo.lock`。

测量由 `.github/workflows/gui-eval.yml` 在 Windows 与 Linux runner 上运行，结论见 `docs/gui-t1-selection.md`。选定框架后，未选中的样例在 T1 记录完成时删除。
