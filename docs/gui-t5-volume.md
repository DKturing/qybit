# GUI T5 设备硬件音量

日期：2026-10-15。任务卡：[tasks/GUI.md](../tasks/GUI.md) §T5。用户决定：Core 与严格路径不变，GUI 窗口可以调节音量，只用设备自身的硬件音量。

## 1. 依据

Microsoft Learn《Endpoint Volume Controls》与《Exclusive-Mode Streams》（文档源 `MicrosoftDocs/win32` 仓库 `desktop-src/CoreAudio/`）：

- `ISimpleAudioVolume`、`IChannelAudioVolume`、`IAudioStreamVolume` 只控制共享模式的会话，对独占流无效。
- `IAudioEndpointVolume` 控制端点设备的音量：设备实现了硬件音量时使用硬件控制，这时音量变化在共享与独占模式下都有效；设备没有硬件音量时它是软件实现，只影响共享模式，独占模式下应用与硬件直接交换数据，绕过软件控制。
- `IAudioEndpointVolume::QueryHardwareSupport` 返回的 `ENDPOINT_HARDWARE_SUPPORT_VOLUME`（1）表示音量由硬件实现；未实现的功能由系统以软件代替。

## 2. 行为

- 格式行右侧：读数标签与滑块（0–100，与 Windows 音量滑块同一刻度 `GetMasterVolumeLevelScalar`），作用于下拉框选定的输出设备（默认设备时取系统默认渲染端点，多媒体角色，与播放时相同）。
- **设备有硬件音量**：拖动或点击滑轨即调用 `SetMasterVolumeLevelScalar`；标签显示读回的 `GetMasterVolumeLevel`（dB），设备在硬件上实现静音且处于静音时标明“已静音”。
- **设备没有硬件音量**：滑块不可用，标签写“设备没有硬件音量，请在 DAC 或功放上调节”。`qy-host-wasapi::volume::EndpointVolume::set_scalar` 在这种设备上直接返回 `E_NOTIMPL`，QyBit 不改变软件音量（它对独占流无效，只会改变其他程序的共享模式音量）。
- **读不到**（无设备、模拟设备、接口失败）：滑块不可用，标签写明原因。
- 系统音量键或其他程序改变了硬件音量：界面在播放状态刷新、切换设备或窗口重新激活时读回；窗口重新激活时重新打开端点（默认设备可能已更换）。
- **保真**：送到设备的数据不变，播放报告、保真状态与“不证明设备端 bit-perfect”的说明都不受影响。设备内部的硬件音量是模拟还是数字实现，QyBit 无法得知；如需确认，请查阅设备说明。
- CLI 不变。

## 3. 实现

- `qy-host-wasapi/src/volume.rs`（Windows 平台边界，`unsafe` 集中于此）：`EndpointVolume::open(id)`（`IMMDevice::Activate` 取 `IAudioEndpointVolume`，`QueryHardwareSupport`）、`hardware_volume()`、`level()`（位置、dB、硬件静音）、`set_scalar()`（只在硬件音量时调用系统接口）。调用方须已初始化 COM；GUI 在界面线程（单线程套间）使用。
- `qybit-gui`：`app::VolumeView`（标签文字与滑块位置，单元测试覆盖）；`sys.rs` 打开、读回与设置，拖动音量滑块期间不按读回值移动滑块；点击滑轨直接移到点击处（与进度条相同）。

## 4. 验证与未运行

- 单元测试：`VolumeView` 的文字、dB 一位小数、静音标记、位置截到 0–100、NaN 不可用。
- CI：Windows 构建与 Clippy；截图步骤输出每个状态的音量标签（模拟设备写“模拟设备，没有音量控制”；runner 没有音频设备，真实 WASAPI 一项写明读取失败的原因）。
- CI 第 110 次运行（提交 85884fe）的截图：[没有音频设备时](gui-t5/no-device.png)，标签写明不可用的原因、滑块不可用；模拟设备各状态的标签均为“模拟设备，没有音量控制”。第 109 次运行的截图发现标签没有设置界面字体（显示为系统默认粗体），已修正。此后把读取失败的原因改为中文（“找不到默认输出设备（…）”）。
- **未运行**：在有硬件音量的设备上实际调节，以及没有硬件音量的设备上的显示，需要用户在真机上运行。
