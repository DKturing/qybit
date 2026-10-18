# 规范出处

实现所依据的规范原文及其来源与校验值。原文不放入仓库：Apple 文档有版权；RFC 可自由获取，以官方地址为准。代码注释与任务卡中的条款引用以这里列出的版本为准。

| 规范 | 用于 | 来源 | 校验 |
| --- | --- | --- | --- |
| RFC 9639《Free Lossless Audio Codec (FLAC)》，IETF Standards Track，2024-12 | `qy-codec-flac`，ID3v2 跳过的依据，S1 定位（§8.2 总样本数、§8.5 SEEKTABLE、§9.1.5 帧号/样本号、§9.1.8 帧头 CRC-8、§9.3 帧尾 CRC-16） | 用户提供的 PDF，75 页，xml2rfc 3.25.0 生成，以它为准。IETF 工作组仓库 `ietf-wg-cellar/flac-specification` 的提交 `c311d21`（2024-01-14，此后仓库删除了正文）是发布前的源文本，只作辅助 | PDF SHA-256 `40f1b37b731562359b811a26c6350af13ec67ba91ab7368f739ea46217bfe21f` |
| Apple《Audio Interchange File Format: "AIFF"》Version 1.3，1989-01-04 | F2 `qy-codec-aiff` | 用户提供的 PDF，31 页 | SHA-256 `56902eb9a5410adcde5f928830b26234442070e0ab1f047f564b23af84067e0a` |
| Apple《Audio Interchange File Format AIFF-C》草案，正文日期 1991-08-26（文件名与 PDF 标题为 9/26/91） | F2 `qy-codec-aiff` | 用户提供的 PDF，45 页 | SHA-256 `04417236851b4e5a9c792d144ed6aa73e9154553c60369de3d9919dcf20845bd` |
| Microsoft Learn `WAVEFORMATEX`、`WAVEFORMATEXTENSIBLE` | `qy-pcm-format::wave`、WASAPI 协商 | GitHub `MicrosoftDocs/sdk-api` 中的文档源 | — |
| Microsoft Learn `WAVEFORMATEX`（mmreg.h）：`WAVE_FORMAT_IEEE_FLOAT`、`nBlockAlign`、`cbSize` | F3 浮点 WAV 识别 | `MicrosoftDocs/sdk-api` 分支 `docs`，`sdk-api-src/content/mmreg/ns-mmreg-waveformatex.md`（2026-10-15 取得） | SHA-256 `6277cc630d2632a7abace70ae405399f14b76c56dba15943bb834363b9dc305b` |
| Microsoft Learn `WAVEFORMATEXTENSIBLE`：`KSDATAFORMAT_SUBTYPE_IEEE_FLOAT` 与格式码 3 的对应 | F3 EXTENSIBLE 浮点、WASAPI 浮点协商 | 同上，`ns-mmreg-waveformatextensible.md` | SHA-256 `7965c8d27febd36549bf145567b5eeb51c39c4170d0f6f9a95954983157370cd` |
| alsa-lib `include/pcm.h`：`SND_PCM_FORMAT_FLOAT_LE`（14）、`FLOAT64_LE`（16）及大端码 | F3 ALSA 浮点格式 | `alsa-project/alsa-lib` 标签 `v1.2.14` | SHA-256 `e60b398dde8a09b815ebc90ced65a370dedc34c6601894e8f084456c9c72dded` |
| IEEE Std 754-2019《IEEE Standard for Floating-Point Arithmetic》（Techstreet 红线版，含 2008 版以来的修改标记） | F3 位模式（§3.4 编码、Table 3.5 参数）与舍入（§4.3.1 roundTiesToEven） | 用户提供的 PDF，45 页，只含到 §5.1 左右；§5.8（浮点转整数）与 §6.2（NaN）不在其中 | SHA-256 `2a7472bd3b4bf20215163692461798c3687758cf8091783c68f6933389b6302e` |
| IBM/Microsoft《Multimedia Programming Interface and Data Specifications 1.0》，1991-08 | RIFF 结构、WAVE（§3 “Waveform Audio File Format”）、`fact` chunk（3-26 页） | 用户提供的 PDF，97 页 | SHA-256 `d2340df3978259b366cbd5acaff049bb3fe81d0d345335a946ee25e4f8b167fa` |
| ID3v2.4.0 结构规范 §3.1 | ID3v2 标签跳过 | id3.org（通过 RFC 9639 的引用核对出处） | — |
| Apple《QuickTime File Format》2001-03-01，第 3 章表 3-3 “Supported QuickTime audio formats”（第 102–103 页） | F5 AIFF-C `twos`、`sowt`、`fl32`、`fl64` 的定义 | Apple Developer 文档存档 `developer.apple.com/library/archive/documentation/QuickTime/QTFF/qtff.pdf`，274 页（2026-10-18 取得） | SHA-256 `fbd57904da54174e86df1aca03b3099af45166690f5eb9a59543d8de7c1649a3` |
| EBU Tech 3306《MBWF / RF64》v1.1，2009-07；ITU-R BS.2088-1，2019-10 | F4 RF64 / BW64 的 `ds64` 结构 | **未取得**：本环境的网络策略拒绝 `tech.ebu.ch` 与 `itu.int`，需要用户提供 PDF；在此之前按下面两份开源实现交叉核对结构 | — |
| FFmpeg `libavformat/wavdec.c`、`aiff.c`、`aiffdec.c`，标签 `n7.1`（GitHub `FFmpeg/FFmpeg`） | F4、F5 的交叉核对（只读，不复制代码） | `raw.githubusercontent.com`（2026-10-18 取得） | `wavdec.c` `8bfda520fe58a0b9f49c9cdb04900dc01b140bee9bbe45a1e23431f56bd8f02e`；`aiff.c` `0c685d570f1e116ff36e8a7031d64a34fbc25859c04cdb54ad7b9403f5be79f3`；`aiffdec.c` `0844566ec44baa247452be8a225ded099cf7c47057ecc7ccbb7bbee2eaed4416` |
| libsndfile `src/rf64.c`、`src/aiff.c`，标签 `1.2.2`（GitHub `libsndfile/libsndfile`） | F4、F5 的交叉核对（只读，不复制代码） | 同上 | `rf64.c` `f37ff4e675f3020b0995f3d4b124c1b8a8843714f8d59c7d21d85851d2eb9cc5`；`aiff.c` `e2cc881d853fa0be7eaced19846b4a93c5ffb64e620f95d9d014235ecd81b4f0` |

**核对记录**：
- **RFC 9639**：PDF 标题、编号、类别、作者（M.Q.C. van Beurden、A. Weaver）与日期正确。与工作组仓库的源文本相比，有 RFC 编辑阶段的措辞修改（如 “STREAMINFO block” 改为 “streaminfo metadata block”）和 Markdown 格式差异，条款引用以 PDF 的章节号为准。M1 实现 FLAC 时 RFC 原文无法取得，章节号按 RFC 结构引用，行为以 libFLAC 源码核对；现已按 PDF 核对 [m1-t2-flac.md](m1-t2-flac.md) 的章节号并更正两处，条款内容的逐条复核另行安排。
- **AIFF 1.3**：修订历史为 1.1（1988-01-21）、1.2（1988-06-17）、1.3（1989-01-04）。
- **AIFF-C**：正文只定义 `NONE`、`ACE2`、`ACE8`、`MAC3`、`MAC6` 五种压缩类型；`sowt`、`twos`、`raw `、`fl32` 等是之后由其他文档引入的扩展。其中 `twos`、`sowt`、`fl32`、`fl64` 的定义见 QuickTime File Format 表 3-3（F5，2026-10-18 起纳入基础格式）。
- **Microsoft `WAVEFORMATEX`（F3）**：文档写“`wBitsPerSample` should be set to 32”（格式码 3），只给出 32 位；64 位浮点按任务卡 §2 接受。`WAVEFORMATEXTENSIBLE` 下 `wBitsPerSample` 为容器位宽，可以是 8 的任意倍数；ffmpeg 6.1 写出的 64 位浮点 WAV 即为 EXTENSIBLE（容器 64 位、子格式 IEEE_FLOAT，2026-10-15 核对）。
- **IEEE 754-2019（F3）**：§3.4 的编码规则与实现一致，逐项核对如下。
  - 偏置指数 E = 2^w − 1 时，T ≠ 0 为 NaN，T = 0 为 ±∞；
  - 1 ≤ E ≤ 2^w − 2 为正规数，值为 (−1)^S × 2^(E−bias) × (1 + 2^(1−p) × T)；
  - E = 0 时，值为 (−1)^S × 2^emin × (0 + 2^(1−p) × T)，emin = 1 − bias，包括 ±0 与次正规数；
  - Table 3.5 的参数：binary32 为 w = 8、t = 23、bias = 127；binary64 为 w = 11、t = 52、bias = 1023；
  - §4.3.1 roundTiesToEven 与转换的舍入规则一致：两个最近值等距时取最低位为偶数者。
  - 转换中“乘以 2^(V−1) 再舍入、限幅”是本项目的定义（任务卡 §4），不是 §5.8 的 convertToInteger 运算本身，两者的舍入方向相同。
  - 这份 PDF 不含 §5.8 与 §6.2，相关条款没有逐字核对。
- **RIFF 1991（F3）**：
  - 原文的 `fact` 规则（3-26 页）是：“The fact chunk is required if the waveform data is contained in a wavl LIST chunk and for all compressed audio formats. The chunk is not required for PCM files using the data chunk format.”；
  - 原文定义的格式类别只有 `WAVE_FORMAT_PCM`（0x0001）和 IBM 的三种专有格式，没有 `WAVE_FORMAT_IEEE_FLOAT`（0x0003）；
  - 因此任务卡 §0 写的“非 PCM 格式需要 `fact`”在 1991 原文中没有依据：原文只要求压缩格式有 `fact`，而 IEEE 浮点不是压缩格式；
  - 用户 2026-10-16 决定：浮点 WAV 的 `fact` 可有可无，没有时帧数按 `data` 长度计算；有则必须与 `data` 一致（见 [f3-float.md](f3-float.md) §1）。
