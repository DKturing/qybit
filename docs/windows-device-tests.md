# Windows 真机测试清单（F2 AIFF、F3 浮点、GUI T5 音量）

日期：2026-10-16。用于收尾 Windows 阶段：下列三项的软件部分已完成，但还没有在真机上运行。结果由用户运行后记入本文 §5，并同步到各自的记录（[f2-aiff.md](f2-aiff.md)、[f3-float.md](f3-float.md)、[gui-t5-volume.md](gui-t5-volume.md)）。

## 0. 准备

1. 从最新一次 CI 运行下载构建产物 `qybit-windows`，解压得到 `qybit.exe` 与 `qybit-gui.exe`，放在同一个文件夹，在该文件夹打开 PowerShell。
2. **先把系统音量和耳机/音箱音量调低**：测试音是 1 kHz 正弦，约 −12 dBFS。独占模式不经过系统混音器；设备没有硬件音量时，Windows 的音量滑块不起作用。
3. 列出设备，记下要测试的端点 ID：
   ```powershell
   .\qybit.exe list-devices
   ```
   后面的命令用 `'<ID>'` 表示这个端点 ID（PowerShell 中要加单引号）。
4. 生成测试文件（每个 10 秒）：
   ```powershell
   .\qybit.exe tone f32-44k.wav  --float 32 --rate 44100 --seconds 10
   .\qybit.exe tone f32-48k.wav  --float 32 --rate 48000 --seconds 10
   .\qybit.exe tone f64-48k.wav  --float 64 --rate 48000 --seconds 10
   .\qybit.exe tone t16-44k.aiff --bits 16  --rate 44100 --seconds 10
   .\qybit.exe tone t24-48k.aiff --bits 24  --rate 48000 --seconds 10
   ```
   生成器经过自动测试：浮点样本与按定义计算的值逐位相同；AIFF 用自有解码器读回，与同参数的 WAV 测试音样本相同；ffmpeg 也能正确读出这些文件。

## 1. 浮点 WAV（F3），命令行

**A. 严格路径（默认）**：
```powershell
.\qybit.exe --device '<ID>' f32-48k.wav f64-48k.wav
```
两种可能，都属正常：
- 端点接受 IEEE 浮点：协商记录写“32 位浮点（IEEE_FLOAT）……接受”，样本按位原样送出，听到 1 kHz 正弦；
- 端点不接受：报告写“端点在独占模式下不接受 32 位浮点（IEEE_FLOAT）；严格路径不转换……”，不出声，退出码 1。

**B. 开启转换**：
```powershell
.\qybit.exe --device '<ID>' --float-to-int f32-44k.wav f32-48k.wav f64-48k.wav
```
- 端点不接受浮点时，应依次请求 32 / 24 / 16 位整数并播放；
- 报告中每首都有“曲目 N 样本已改变：……浮点转为 V 位整数（明确开启……）”，限幅数应为 0；
- 结论不写“严格路径”。

## 2. AIFF（F2），命令行

```powershell
.\qybit.exe --device '<ID>' t16-44k.aiff t24-48k.aiff
```
- 应正常播放，协商与 WAV 相同（AIFF 是大端，送到设备前由 pack 放置，样本值不变）；
- 如有自己的 AIFF 音乐文件，可再播放一首试听，并记下文件的格式（采样率 / 位数 / 声道）。

## 3. 图形界面

1. 运行 `qybit-gui.exe`，在设备下拉框选同一端点，把第 0 步生成的文件拖进播放列表。
2. **浮点**：
   - “浮点转整数（会改变样本）”不勾选，播放 `f32-48k.wav`：结果应与 §1 A 相同；
   - 勾选后再播放：结果应与 §1 B 相同，日志中有“样本已改变”，状态栏的保真结论不再是严格路径。
3. **AIFF**：播放 `t16-44k.aiff`，列表中的格式列与时长应正确。
4. **音量（GUI T5）**：看格式行右侧的音量读数与滑块。
   - 设备实现了硬件音量：显示 dB 读数，拖动滑块音量应变化；在 Windows“声音设置”里改变该设备音量后切回本窗口，读数应与之一致；日志与保真结论不变（送到设备的数据不变）；
   - 设备没有硬件音量：滑块不可用，并写明原因。这也是正确结果，记下原文即可。

## 4. 需要回传的内容

- §1 A、§1 B、§2 三条命令的完整终端输出（含最后的报告）；
- GUI 中浮点开 / 关两次播放的日志区文字或截图，以及音量区的截图（滑块可用时拖动前后各一张）；
- 听感：有没有杂音、断续，声道是否正常。

## 5. 结果

（待用户运行后填写。）
