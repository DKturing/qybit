# 版本记录

版本规则与对外接口的范围见 [docs/e4-versioning.md](docs/e4-versioning.md)。0.x 阶段次版本号变化表示有破坏性修改，写在该版本的“破坏性”小节。插件仓库按 tag 以 git 依赖（暂不发布 crates.io）。

## [0.1.0] - 2026-10-19

第一个版本 tag。此前各 crate 版本为 `0.0.0`，没有兼容承诺。

### 对外接口

- 解码器契约：`qy_core::Decoder`（`decode` / `pcm` / `release` / `seek` / `seek_request` / `take_preroll` / `reset` / `format`）、`Status`、`Progress`、`DecodeError`、`SeekError`；样本为交错、右对齐的 `i32`，携带有效位数（X1、[docs/x1-extensions.md](docs/x1-extensions.md)）。
- 格式：`SourceFormat`（整数、浮点、DSD；有损标记 E1）、`ChannelLayout`、`DeviceFormat` 与 DSD 传输方式（D1）。
- 文件头识别：`qy_core::Identify`、`magic_matches`，官方解码器的 `identify` 与 `AutoDecoder::identify`（E3）。
- 定位预滚：`Decoder::take_preroll`（E5）。
- DSD 输出通道：`SampleEncoding::Dsd`、`SourceFormat::dsd`、`DeviceFormat::dsd`，ALSA 原生 DSD 与 DoP、WASAPI DoP（D1）。
- 应用组装：`qy_player::Formats` / `Builtin` / `Probe` / `TrackInfo` / `read_header`，`qybit::run` / `run_args`，`qybit_gui::run`（E2）；`HostResources::with_decoder`、`Diagnostics`（X1）。
- 契约测试：`qy-testkit` 的 `check`、`check_seek`、`check_identify`、`decode`。

### 基础格式

WAV（整数、浮点、8 位、EXTENSIBLE、RF64/BW64）、FLAC、AIFF / AIFF-C 未压缩（`NONE`、`twos`、`sowt`、`fl32`/`fl64`），均为自行实现的解码器。
