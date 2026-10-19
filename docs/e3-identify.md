# E3：格式识别

日期：2026-10-19。任务卡：[tasks/E3.md](../tasks/E3.md)。本卡的结果来自单元测试、一致性测试与 fuzz，**不是实机结果**。

## 1. 问题与做法

插件的路由层（如 Lite 的组合解码器）要决定一个文件交给官方解码器还是插件。原来官方解码器只能在解码中途返回 `Unsupported`，插件为此要自己缓存文件头（2026-10-18 评估的 Lite 包用了 4 KiB 的路由缓冲）。

现在每个官方解码器都有不消耗输入、不需要工作区的识别函数，按与完整解码**同一个状态机**判断：识别时以“识别模式”构造解码器（工作区为空切片，FLAC 的容量不设限），读到样本数据开始处即停。所以识别与解码不会出现两套解析规则。

## 2. 接口

```rust
// qy-core
pub enum Identify<D> {
    Foreign,                                           // 文件头标记不是本格式
    Skip { bytes: u64 },                               // 开头是 n 字节的元数据（ID3v2），跳过后再识别
    NeedMore { total: u64, container: bool },          // 至少共 total 字节才能回答；container：标记已符合
    Supported { format: SourceFormat, data_start: u64 },
    Rejected { error: DecodeError, detail: Option<D> },
}
pub fn magic_matches(head: &[u8], magics: &[&[u8]], eof: bool) -> Option<bool>;

qy_codec_wav::identify(head, eof)  -> Identify<WavDetail>
qy_codec_aiff::identify(head, eof) -> Identify<AiffDetail>
qy_codec_flac::identify(head, eof) -> Identify<FlacError>
AutoDecoder::identify(&self, head, eof) -> (Codec, Identify<AutoDetail>)   // 按本解码器启用的格式
```

- `head` 总是从文件开头算起的前若干字节；不消耗，可以反复用更长的前缀再问。
- `NeedMore { total }` 是下界：少于 `total` 字节的任何前缀都不会给出结论。标记还没读全时 `total` 为已给字节数 + 1（下一个字节就可能排除本格式）。
- 需要的字节数由文件决定，没有固定上限：WAV 的 `fmt ` 之前、AIFF 的 COMM 之前、FLAC 的 STREAMINFO 之后都可以有任意大的块（如 FLAC 嵌入的图片）。16 位双声道 WAV 还要读 data 开头的 4096 字节，确认不是伪装成 PCM 的 DTS / IEC 61937 码流。`container` 为真时文件头标记已经符合，路由层的窗口装不下 `total` 时可以据此直接交给该解码器（它流式跳过大块）。
- 宿主容量（FLAC 块大小、声道数、帧长度超出配置）不在识别范围内，由 `Resources::capacity_rejection` 另行报告。
- 组合识别（`qy-codec-auto`）：先看 ID3v2 标签（合规时 `Skip`，长度超出文件时 `Rejected(Truncated)`），再依次用各格式自己的 `identify`；未启用的格式按标记认出名字、按不支持拒绝（`AutoDetail::Disabled`）；都不是时 `Foreign`、`Codec::Unrecognized`。

### 与完整解码的关系（一致性规则）

| 识别结论 | 完整解码 |
| --- | --- |
| `Supported` | 同一个源格式；只送到 `data_start`（`eof` 为假）时不出错（宿主容量不足除外）。出错只可能在样本数据开始之后（如 AIFF-C 缺 FVER 要到 FORM 末尾才知道） |
| `Rejected` | 在任何样本之前以同一错误失败，明细相同（或先因宿主容量不足失败） |
| `Foreign` | 在任何样本之前失败 |
| 任一前缀上的结论 | 与整个文件的结论相同（与分块无关）；前缀上的 `Skip` 超出文件时，整个文件的结论是 `Rejected(Truncated)` |

`qy_testkit::check_identify(identify, decoder, detail, bytes)` 按上表检查任意格式（插件也用它）：整个文件、0..=512 的每个前缀与之后按 1/8 递增的前缀，以及每个 `NeedMore` 的 `total − 1`。

## 3. 顺带的改动

- `AutoDecoder` 解码时的选择与识别相同：`RIFF` / `RF64` / `BW64` / `RIFX` 但形式类型不是 `WAVE` 的文件（如 AVI）原来交给 WAV 解码器、报“结构非法”，现在按无法识别拒绝，原因写明“RIFF 容器，但形式类型不是 WAVE”（与 `FORM` 非 AIFF 的处理一致）。
- 示例 `examples/qy-ext-demo`：路由改用识别，不缓存文件头；QYT1 也提供自己的 `qyt1::identify`；文件开头的 ID3v2 标签由路由层跳过（只记计数），QYT1 与 WAV 都可以带标签。

## 4. 验证

| 测试 | 内容 | 结果 |
| --- | --- | --- |
| `qy-codec-auto/tests/identify.rs` 夹具 | WAV、AIFF、FLAC 三个解码器的全部 289 个夹具（含被拒绝的、其他格式的文件）经三个格式各自的识别与组合识别，与完整解码比较（`check_identify`）；组合识别与认领格式自己的结论相同、只有一个格式认领。结论：Supported 183、Rejected 88、Foreign 18 | 通过 |
| 同上，本地 fuzz 语料 | 本地 `fuzz/corpus`（不入库）的解码语料 1959 个 | 通过 |
| 同上，边界情形 | 空输入与标记前缀、AVI、IFF ILBM、RIFX、大块之前的 `container: true`、ID3v2 标签、未启用 FLAC 的组合解码器；AVI 解码时按无法识别拒绝 | 通过 |
| fuzz 目标 `identify` | 四种识别在随机输入上与完整解码比较；本地 5 分钟 259 466 次无违约（之前依次发现并修正：标记前缀的下界、16 位双声道码流检查期间提前报 `Supported`、跳过块的下界多算 1 字节、AIFF FVER 的下界、ID3 标签超出文件时的结论、FLAC 宿主容量先于截断）；CI 冒烟 60 秒 | 通过 |
| 示例 `tests/contract.rs` | `qyt1::identify` 的一致性；组合解码器带 ID3v2 标签的 QYT1 与 WAV：样本与不带标签相同（一次性、逐字节、随机分块），跳过字节数等于标签长度，定位正确 | 通过 |
| 全部既有测试、no_std（thumbv7em）Clippy | 官方解码器的输出不变 | 通过 |
| QEMU 演示（Embedded Full / Lite） | 照常通过；`.text` 分别 +1 368 / −48 字节（相对 E2；识别函数本身没有链接进演示，增加的是解码器中的识别模式判断与 RIFF 形式类型检查），`.bss` 不变 | 通过 |

## 5. 未做

- 识别不覆盖宿主容量与样本数据本身的问题（截断、坏帧），这些仍由完整解码报告。
