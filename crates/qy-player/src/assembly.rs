//! 应用组装入口（E2，tasks/E2.md）：应用（`qybit`、`qybit-gui`，或插件仓库里组装的同一套应用）
//! 用的一套格式。
//!
//! 应用不再固定使用官方解码器：命令行与图形界面的主体对 [`Formats`] 泛型，二进制只负责组装，
//! 官方发行版传入 [`Builtin`]（WAV、FLAC、AIFF）。插件仓库实现自己的 [`Formats`]（例如把 Lite 的
//! 解码器与 `AutoDecoder` 组合成一个解码器），调用同一个入口即得到带新格式的命令行与图形界面。
//!
//! 播放资源与文件信息读取器都在使用它们的后台线程里创建（[`Formats::resources`]、
//! [`Formats::prober`]），因此解码器类型不需要 `Send`；`Formats` 本身在线程间共享，只需
//! `Send + Sync`。

use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::Path;

use qy_codec_auto::{AutoDecoder, Codec};
use qy_core::engine::Resources;
use qy_core::{DecodeError, Decoder, Jump, JumpGuard, SourceFormat, Status};

use crate::{Diagnostics, HostError, HostResources};

/// 文件信息读取的结果：格式名、源格式与文件头声明的总帧数（不可信，只用于显示时长）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackInfo {
    /// 格式名（如 `"FLAC"`），显示在播放列表的格式列。
    pub label: &'static str,
    pub format: SourceFormat,
    pub frames: Option<u64>,
}

/// 文件信息读取器：读取文件头到格式确定为止（与播放同一套解析与拒绝规则，不另写解析器）。
pub trait Probe {
    /// 读取 `path` 的文件头；无法播放时给出原因（与播放时的拒绝一致）。
    fn probe(&mut self, path: &Path) -> Result<TrackInfo, String>;
}

/// 一套格式（E2）。
pub trait Formats: Send + Sync + 'static {
    /// 播放用的解码器（组合解码器也可以，见 `examples/qy-ext-demo`）。
    type Decoder: Decoder + Resources + Diagnostics;

    /// 播放用的宿主资源（工作区在调用时一次性分配，整个程序复用）。在播放线程里调用。
    fn resources(&self) -> Result<HostResources<Self::Decoder>, String>;

    /// 单独解码用的解码器与它需要的输入窗口字节数（`qybit decode`）。
    fn decoder(&self) -> Result<(Self::Decoder, usize), String>;

    /// 文件信息读取器（有自己的一套工作区，与播放的互不共用）。在读取线程里调用。
    fn prober(&self) -> Result<Box<dyn Probe>, String>;

    /// 可加入播放列表、文件对话框中列出的扩展名（小写，不含点）。
    fn extensions(&self) -> &[&'static str];

    /// 格式名称，用于帮助与“关于”文字（如 `"WAV / FLAC / AIFF"`）。
    fn names(&self) -> &str;

    /// `path` 的扩展名是否在 [`Formats::extensions`] 中（不区分大小写）。
    fn is_audio_path(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| self.extensions().iter().any(|x| e.eq_ignore_ascii_case(x)))
    }
}

/// 官方解码器：WAV（含 RF64 / BW64）、FLAC、AIFF / AIFF-C（`qy-codec-auto`），按桌面配置档组装，
/// 与 [`HostResources::new`] 相同。
#[derive(Debug, Clone, Copy, Default)]
pub struct Builtin;

/// 官方解码器的扩展名。
pub const BUILTIN_EXTENSIONS: [&str; 8] =
    ["wav", "wave", "rf64", "bw64", "flac", "aif", "aiff", "aifc"];

impl Formats for Builtin {
    type Decoder = AutoDecoder;

    fn resources(&self) -> Result<HostResources, String> {
        HostResources::new().map_err(|e| host_error(&e))
    }

    fn decoder(&self) -> Result<(AutoDecoder, usize), String> {
        let d = crate::decoder_for(&crate::desktop_profile()).map_err(|e| host_error(&e))?;
        let window = d.required_window();
        Ok((d, window))
    }

    fn prober(&self) -> Result<Box<dyn Probe>, String> {
        let (decoder, window) = self.decoder()?;
        Ok(Box::new(BuiltinProbe {
            decoder,
            limit: window,
            window: Vec::with_capacity(CHUNK),
        }))
    }

    fn extensions(&self) -> &[&'static str] {
        &BUILTIN_EXTENSIONS
    }

    fn names(&self) -> &str {
        "WAV / FLAC / AIFF"
    }
}

fn host_error(e: &HostError) -> String {
    format!("播放资源分配失败：{e:?}")
}

/// 每次读取的字节数；文件头读取的总量上限（按实际读取的字节累计，跳转后重复读取的也计入；元数据
/// 过大时放弃显示，不影响播放）。
const CHUNK: usize = 64 * 1024;
const READ_LIMIT: usize = 16 * 1024 * 1024;

/// 用 `decoder` 读取 `path` 的文件头，直到格式确定或出错（插件实现 [`Probe`] 时可直接用）。
/// `window` 为复用的读取缓冲；`limit` 为解码器需要的最大输入窗口（超过 16 MiB 时按 16 MiB）；
/// 读取总量至多 16 MiB。解码需要的读取跳转（E6，`Decoder::take_jump`）按与核心相同的规则执行
/// （`qy_core::JumpGuard`）：跳转后旧位置的输入与 EOF 作废，从新位置读取。
/// 出错时给出解码错误，原因的文字由调用方按自己的明细补充。
pub fn read_header<D: Decoder>(
    decoder: &mut D,
    window: &mut Vec<u8>,
    limit: usize,
    path: &Path,
) -> Result<SourceFormat, HeaderError> {
    let mut file = File::open(path).map_err(|e| HeaderError::Io(format!("无法打开：{e}")))?;
    decoder.reset();
    window.clear();
    let limit = limit.min(READ_LIMIT);
    let mut eof = false;
    let mut guard = JumpGuard::new();
    let mut total = 0usize;
    loop {
        let progress = decoder.decode(window, eof).map_err(HeaderError::Decode)?;
        let consumed = progress.consumed.min(window.len());
        window.drain(..consumed);
        let to = match guard.take(decoder, progress.status) {
            Ok(Jump::None) => None,
            Ok(Jump::Parse(o) | Jump::Seek(o)) => Some(o),
            Err(f) => return Err(HeaderError::Io(f.to_string())),
        };
        // 读到第一个样本即可：同一次调用中的跳转不再执行。
        if let (Some(o), Status::NeedInput) = (to, progress.status) {
            file.seek(SeekFrom::Start(o))
                .map_err(|e| HeaderError::Io(format!("{}：{e}", qy_core::JumpFault::Unseekable)))?;
            window.clear();
            eof = false;
            guard.restart();
            continue;
        }
        match progress.status {
            Status::Frames(_) | Status::NeedOutput | Status::End => break,
            Status::NeedInput if eof => return Err(HeaderError::Io("文件不完整".into())),
            Status::NeedInput => {
                if window.len() >= limit || total >= READ_LIMIT {
                    return Err(HeaderError::Io("文件头过大，无法读取格式".into()));
                }
                let old = window.len();
                window.resize(old + CHUNK, 0);
                let n = loop {
                    match file.read(&mut window[old..]) {
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        r => break r.map_err(|e| HeaderError::Io(format!("读取失败：{e}")))?,
                    }
                };
                window.truncate(old + n);
                total = total.saturating_add(n);
                guard.on_read(n);
                eof = n == 0;
            }
        }
    }
    decoder
        .format()
        .ok_or_else(|| HeaderError::Io("文件中没有音频".into()))
}

/// [`read_header`] 的错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderError {
    /// 解码器拒绝（明细由调用方从解码器取得）。
    Decode(DecodeError),
    /// 文件读取问题或文件不完整，附说明。
    Io(String),
}

/// 官方解码器的文件信息读取器。
struct BuiltinProbe {
    decoder: AutoDecoder,
    limit: usize,
    window: Vec<u8>,
}

impl Probe for BuiltinProbe {
    fn probe(&mut self, path: &Path) -> Result<TrackInfo, String> {
        let format = match read_header(&mut self.decoder, &mut self.window, self.limit, path) {
            Ok(f) => f,
            Err(HeaderError::Io(s)) => return Err(s),
            Err(HeaderError::Decode(e)) => return Err(self.reason(e)),
        };
        let label = match self.decoder.codec() {
            Codec::Wav => "WAV",
            Codec::Flac => "FLAC",
            Codec::Aiff => "AIFF",
            _ => return Err("无法识别的文件".into()),
        };
        Ok(TrackInfo {
            label,
            format,
            frames: self.decoder.declared_frames(),
        })
    }
}

impl BuiltinProbe {
    fn reason(&self, e: DecodeError) -> String {
        let d = &self.decoder;
        if let Some(u) = d.unrecognized() {
            return format!("无法识别的文件：{u}");
        }
        if let Some(w) = d.wav_detail() {
            return format!("{e:?}：{w}");
        }
        if let Some(a) = d.aiff_detail() {
            return format!("{e:?}：{a}");
        }
        #[cfg(feature = "flac")]
        if let Some(f) = d.flac_error() {
            return format!("{e:?}：{f:?}");
        }
        if let Some(name) = d.disabled() {
            return format!("本构建未启用 {name} 解码器");
        }
        format!("{e:?}")
    }
}
