# 规范出处

实现所依据的规范原文及其来源与校验值。原文不放入仓库：Apple 文档有版权；RFC 可自由获取，以官方地址为准。代码注释与任务卡中的条款引用以这里列出的版本为准。

| 规范 | 用于 | 来源 | 校验 |
| --- | --- | --- | --- |
| RFC 9639《Free Lossless Audio Codec (FLAC)》，IETF Standards Track，2024-12 | `qy-codec-flac`，ID3v2 跳过的依据 | 用户提供的 PDF，75 页，xml2rfc 3.25.0 生成，以它为准。IETF 工作组仓库 `ietf-wg-cellar/flac-specification` 的提交 `c311d21`（2024-01-14，此后仓库删除了正文）是发布前的源文本，只作辅助 | PDF SHA-256 `40f1b37b731562359b811a26c6350af13ec67ba91ab7368f739ea46217bfe21f` |
| Apple《Audio Interchange File Format: "AIFF"》Version 1.3，1989-01-04 | F2 `qy-codec-aiff` | 用户提供的 PDF，31 页 | SHA-256 `56902eb9a5410adcde5f928830b26234442070e0ab1f047f564b23af84067e0a` |
| Apple《Audio Interchange File Format AIFF-C》草案，正文日期 1991-08-26（文件名与 PDF 标题为 9/26/91） | F2 `qy-codec-aiff` | 用户提供的 PDF，45 页 | SHA-256 `04417236851b4e5a9c792d144ed6aa73e9154553c60369de3d9919dcf20845bd` |
| Microsoft Learn `WAVEFORMATEX`、`WAVEFORMATEXTENSIBLE` | `qy-pcm-format::wave`、WASAPI 协商 | GitHub `MicrosoftDocs/sdk-api` 中的文档源 | — |
| ID3v2.4.0 结构规范 §3.1 | ID3v2 标签跳过 | id3.org（通过 RFC 9639 的引用核对出处） | — |

**核对记录**：
- **RFC 9639**：PDF 标题、编号、类别、作者（M.Q.C. van Beurden、A. Weaver）与日期正确。与工作组仓库的源文本相比，有 RFC 编辑阶段的措辞修改（如 “STREAMINFO block” 改为 “streaminfo metadata block”）和 Markdown 格式差异，条款引用以 PDF 的章节号为准。M1 实现 FLAC 时 RFC 原文无法取得，章节号按 RFC 结构引用，行为以 libFLAC 源码核对；现已按 PDF 核对 [m1-t2-flac.md](m1-t2-flac.md) 的章节号并更正两处，条款内容的逐条复核另行安排。
- **AIFF 1.3**：修订历史为 1.1（1988-01-21）、1.2（1988-06-17）、1.3（1989-01-04）。
- **AIFF-C**：正文只定义 `NONE`、`ACE2`、`ACE8`、`MAC3`、`MAC6` 五种压缩类型；`sowt`、`twos`、`raw `、`fl32` 等是之后由其他文档引入的扩展。
