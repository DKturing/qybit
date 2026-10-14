# M0 T2：流式 WAV 解码记录

实现：`crates/qy-codec-wav/src/lib.rs`。

## 调用契约

`Decoder::decode(input, eof)` 只读取宿主传入的切片，不缓存输入，返回 `Progress { consumed, status }`：

| status | 含义 |
| --- | --- |
| `Frames(n)` | 本次产出 n 个完整帧，从 `pcm()` 读取，用 `release(k)` 取走（可部分取走） |
| `NeedInput` | 需要更多输入；`consumed` 字节已处理，其余由宿主保留到下次调用。`eof = true` 时绝不返回 |
| `NeedOutput` | 工作区仍有未取走样本；此时 `consumed = 0` |
| `End` | RIFF 正常结束（RIFF 之后的多余字节忽略） |

源 EOF 时尚未完成的结构返回 `Truncated`，不会伪装成 `NeedInput` 或 `End`。解码器出错后保持该错误直到 `reset()`。格式在 fmt chunk 解析完成前为 `None`。

单次所需的最大连续输入为 40 字节（fmt 前缀）或一帧（≤ 8 字节），远小于 8192 字节的输入窗口，所以任意分块和短读都能推进。

## 支持范围与错误分类

> 这是 M0 T2 时的记录。之后的变化：M3 T1 去掉采样率上限；F1 支持多声道布局；W1 支持 8 位 PCM，并对真实世界文件头做容错（RIFF/data 占位长度、RIFF 长度不符、不足一帧的尾部可播放并报告修正，data 声明长于文件时报告缺少的字节数）。详见 [w1-wav-tolerance.md](w1-wav-tolerance.md)。

| 输入 | 结果 |
| --- | --- |
| RIFF/WAVE，格式码 1，16/24/32 位，单/双声道，1–192000 Hz | 支持（N = 位宽） |
| 格式码 0xFFFE，子格式 `KSDATAFORMAT_SUBTYPE_PCM`，容器 16/24/32，1 ≤ V ≤ C | 支持（N = V，读取时算术右移 C−V） |
| EXTENSIBLE 声道掩码：单声道 0 或 FrontCenter(0x4)，双声道 0 或 FL\|FR(0x3) | 支持；`channel_mask()` 原样报告 |
| 8 位、浮点（格式码 3 或 FLOAT 子格式）、其他格式码、>2 声道、>192 kHz、32 位以上容器、格式码 1 的非整字节位宽、其他声道布局、RIFX/RF64/BW64 | `Unsupported` |
| 非 RIFF/WAVE、RIFF 大小 < 4、chunk 越出 RIFF、RIFF 尾部不足 8 字节、data 在 fmt 前、重复 fmt/data、缺 data、fmt < 16 字节、EXTENSIBLE fmt < 40 字节或 cbSize < 22、声道/采样率/blockAlign/位宽为 0、blockAlign ≠ 声道 × 容器字节、V = 0 或 V > C、EXTENSIBLE 容器非整字节、掩码位数多于声道数、data 大小不是 blockAlign 的整数倍 | `Malformed` |
| 源在完整结构前结束（含 RIFF 声明长度大于文件） | `Truncated`（之前的完整帧已输出） |

补充说明：

- 未知 chunk 流式跳过；奇数长度 chunk 跳过 1 字节填充。只有当 RIFF 恰好在最后一个 chunk 体末尾结束时，才容忍缺失的最后一个填充字节。
- EXTENSIBLE 容器低 C−V 位按规范不属于样本值，读取时丢弃（夹具 `ext20in24_dirty_pad_bits` 覆盖）。
- `avgBytesPerSec` 解码不使用，因此不校验。
- RIFF 余量和 chunk 计数都用 `u32` 的 checked 运算；帧数受输入长度、data 余量和工作区三者中的最小值限制。

规范出处：Microsoft *Multimedia Programming Interface and Data Specifications 1.0*（RIFF chunk、WORD 对齐填充）；`WAVEFORMATEX` / `WAVEFORMATEXTENSIBLE` 文档（mmreg.h、ksmedia.h：`wValidBitsPerSample`、`dwChannelMask`、`SPEAKER_*`、`KSDATAFORMAT_SUBTYPE_PCM`）。

## 独立参考夹具

- 生成器：`tools/qy-gen-fixtures`，用 `cargo run -p qy-gen-fixtures -- wav` 运行（纯 Rust，不依赖任何 qy- crate；固定种子，重复运行输出逐字节相同）。它不经过本项目的解码器，而是从已知整数样本直接构造 WAV 字节，样本值写入 `.expect`。
- 第二参考：对有效夹具用 ffmpeg 6.1.1-3ubuntu5 解码为 s32le，写入 `.ffmpeg.s32le`。生成器断言它等于 `样本 << (32 − V)`，Rust 测试也独立比较。
- ffmpeg 6.1 会把 C32/V24 的 EXTENSIBLE PCM 误识别为 `pcm_f24le`，所以生成器显式指定输入解码器 `pcm_s{C}le`。脏填充位夹具不做 ffmpeg 比较，因为 ffmpeg 会保留填充位。
- 共 54 个夹具：有效 17 个（含 V < C 的 12/16、20/24、24/32，奇数 chunk，未知 chunk，fmt 18/42 字节，10000 帧大文件），截断 5 个，不支持 12 个，非法 20 个。

测试（`crates/qy-codec-wav/tests/fixtures.rs`）：每个夹具依次以一次性、逐字节、200 个固定种子随机分块（1 字节到 8192 字节）的方式输入，并随机部分取走输出；结果、格式、掩码和全部样本都必须与 `.expect` 一致。`crates/qy-host-sim/tests/core_wav.rs` 通过真实 `qy-core` 的 8192 字节窗口，以 50 个种子的随机短读（含零长度读取）重复验证。

变异检查：把 24 位读取的右移改成固定 8 位，或去掉奇数 chunk 填充，夹具测试都会失败。

## Fuzz

目标：`fuzz/fuzz_targets/wav_decode.rs`（cargo-fuzz / libFuzzer）。输入首字节作为分块种子，检查：不 panic、`consumed` 不越界、EOF 后不返回 `NeedInput`、分块与一次性的结果/格式/样本完全一致。

初始语料：每个夹具加 `\x00` 和 `\x07` 两种首字节，共 108 个文件。

```sh
mkdir -p fuzz/corpus/wav_decode
for f in crates/qy-codec-wav/tests/fixtures/*.wav; do n=$(basename $f)
  (printf '\x00'; cat $f) > fuzz/corpus/wav_decode/$n.s0
  (printf '\x07'; cat $f) > fuzz/corpus/wav_decode/$n.s7; done
cd fuzz && cargo +nightly fuzz run wav_decode -- -max_total_time=600 -max_len=65536 -seed=20261013
```

10 分钟运行记录（2026-10-13，Linux x86_64）：

| 项 | 值 |
| --- | --- |
| 工具链 | rustc 1.101.0-nightly (e9bbadf68 2026-10-12)，cargo-fuzz 0.13.2，libfuzzer-sys 0.4.10 |
| 参数 | `-max_total_time=600 -max_len=65536 -seed=20261013` |
| 时长 | 601 s，2,552,339 次执行，平均 4246 exec/s |
| 初始语料 | 108 个文件（去重后 76 个 / 139 KB），INITED cov 331 ft 895 |
| 结束状态 | cov 355 ft 1267，最小化语料 203 个 / 312 KB，新增单元 944 |
| 失败 | 0（无 panic、无断言失败、无 crash artifact），退出码 0 |

语料没有提交到仓库（`fuzz/corpus` 在 .gitignore 中），可用上面的命令从夹具重建。
