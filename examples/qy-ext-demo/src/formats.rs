//! 应用组装（E2）：把组合解码器、文件信息读取与扩展名交给 `qybit::run` / `qybit_gui::run`。
//! 插件仓库组装 Lite / Full 的命令行与图形界面时照此实现 [`Formats`]。

use std::path::Path;

use qy_codec_auto::Codec;
use qy_codec_auto::profile::DESKTOP;
use qy_core::Decoder;
use qy_player::assembly::{BUILTIN_EXTENSIONS, HeaderError, read_header};
use qy_player::{Diagnostics, Formats, HostResources, Probe, TrackInfo};

use crate::ExtDecoder;

/// 官方解码器的扩展名再加示例格式（`.qyt1` 无损、`.qyl1` 有损）。
pub const EXT_EXTENSIONS: [&str; BUILTIN_EXTENSIONS.len() + 2] = {
    let mut all = [""; BUILTIN_EXTENSIONS.len() + 2];
    let mut i = 0;
    while i < BUILTIN_EXTENSIONS.len() {
        all[i] = BUILTIN_EXTENSIONS[i];
        i += 1;
    }
    all[i] = "qyt1";
    all[i + 1] = "qyl1";
    all
};

/// 示例格式集合：WAV / FLAC / AIFF（`AutoDecoder`）与 QYT1（[`ExtDecoder`]）。
#[derive(Debug, Clone, Copy, Default)]
pub struct ExtFormats;

impl Formats for ExtFormats {
    type Decoder = ExtDecoder;

    fn resources(&self) -> Result<HostResources<ExtDecoder>, String> {
        let (decoder, window) = self.decoder()?;
        HostResources::with_decoder(decoder, window, DESKTOP)
            .map_err(|e| format!("播放资源分配失败：{e:?}"))
    }

    fn decoder(&self) -> Result<(ExtDecoder, usize), String> {
        let d = ExtDecoder::desktop(true);
        let window = d.required_window();
        Ok((d, window))
    }

    fn prober(&self) -> Result<Box<dyn Probe>, String> {
        let (decoder, limit) = self.decoder()?;
        Ok(Box::new(ExtProbe {
            decoder,
            limit,
            window: Vec::new(),
        }))
    }

    fn extensions(&self) -> &[&'static str] {
        &EXT_EXTENSIONS
    }

    fn names(&self) -> &str {
        "WAV / FLAC / AIFF / QYT1"
    }
}

/// 文件信息读取：与播放同一个组合解码器读到格式确定为止。
struct ExtProbe {
    decoder: ExtDecoder,
    limit: usize,
    window: Vec<u8>,
}

impl Probe for ExtProbe {
    fn probe(&mut self, path: &Path) -> Result<TrackInfo, String> {
        let format = match read_header(&mut self.decoder, &mut self.window, self.limit, path) {
            Ok(f) => f,
            Err(HeaderError::Io(s)) => return Err(s),
            Err(HeaderError::Decode(e)) => {
                let mut lines = Vec::new();
                self.decoder.failure_details(&mut lines);
                if let Some(u) = self.decoder.auto().unrecognized() {
                    lines.push(format!("无法识别的文件：{u}"));
                }
                return Err(if lines.is_empty() {
                    format!("{e:?}")
                } else {
                    format!("{e:?}：{}", lines.join("；"))
                });
            }
        };
        let (label, frames) = if self.decoder.is_qyt1() {
            let label = if format.is_lossy() { "QYL1" } else { "QYT1" };
            (label, self.decoder.qyt1().declared_frames())
        } else {
            let auto = self.decoder.auto();
            let label = match auto.codec() {
                Codec::Wav => "WAV",
                Codec::Flac => "FLAC",
                Codec::Aiff => "AIFF",
                _ => return Err("无法识别的文件".into()),
            };
            (label, auto.declared_frames())
        };
        debug_assert_eq!(self.decoder.format(), Some(format));
        Ok(TrackInfo {
            label,
            format,
            frames,
        })
    }
}
