//! 后台读取文件头（格式与声明的时长）与扫描文件夹。用自有解码器解析文件头（与播放同一套
//! 解析与拒绝规则），不另写解析器；只读到格式确定为止，不解码整个文件。

use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

use qy_codec_auto::{AutoDecoder, Codec};
use qy_core::{Decoder, PCM_WORK_SAMPLES, SourceFormat, Status};

use crate::session::{Notify, UiEvent};

/// 容器类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Wav,
    Flac,
    Aiff,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Wav => "WAV",
            Self::Flac => "FLAC",
            Self::Aiff => "AIFF",
        }
    }
}

/// 文件头给出的曲目信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackInfo {
    pub kind: Kind,
    pub format: SourceFormat,
    /// 文件头声明的总帧数（不可信，只用于显示时长；未声明时为 `None`）。
    pub frames: Option<u64>,
}

/// 文件头读取状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    Pending,
    Ready(TrackInfo),
    /// 无法播放的原因（与播放时的拒绝一致）。
    Failed(String),
}

/// 可加入播放列表的扩展名（小写比较）。
pub const EXTENSIONS: [&str; 8] = ["wav", "wave", "rf64", "bw64", "flac", "aif", "aiff", "aifc"];

/// 扫描文件夹的最大深度（防止符号链接环）。
const MAX_DEPTH: usize = 32;

/// 每次读取的字节数；文件头读取的总量上限（元数据过大时放弃显示，不影响播放）。
const CHUNK: usize = 64 * 1024;
const READ_LIMIT: usize = 16 * 1024 * 1024;

pub(crate) enum ProbeRequest {
    Files(Vec<(u64, PathBuf)>),
    Folder(PathBuf),
}

pub(crate) fn is_audio_path(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// 递归列出文件夹中的音频文件（按路径排序）。
pub fn scan_folder(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() && depth < MAX_DEPTH => stack.push((p, depth + 1)),
                Ok(t) if t.is_file() && is_audio_path(&p) => out.push(p),
                _ => {}
            }
        }
    }
    out.sort();
    out
}

/// 文件头读取器：持有一套解码工作区（启动时分配一次，与播放线程的互不共用）。
pub struct Prober {
    decoder: AutoDecoder,
    window: Vec<u8>,
}

impl Prober {
    pub fn new() -> Result<Self, String> {
        let cap = qy_codec_auto::profile::DESKTOP
            .flac_capacity()
            .ok_or_else(|| "配置档未启用 FLAC".to_string())?;
        let leak_i32 = |n: usize| Box::leak(vec![0i32; n].into_boxed_slice());
        let workspace = Box::leak(vec![0i64; cap.workspace_len()].into_boxed_slice());
        let decoder = AutoDecoder::new(
            leak_i32(PCM_WORK_SAMPLES),
            cap,
            workspace,
            leak_i32(cap.pcm_len()),
        )
        .map_err(|e| format!("解码器初始化失败：{e:?}"))?;
        Ok(Self {
            decoder,
            window: Vec::with_capacity(CHUNK),
        })
    }

    /// 读取文件头直到格式确定（或出错）。
    pub fn probe(&mut self, path: &Path) -> Result<TrackInfo, String> {
        let mut file = File::open(path).map_err(|e| format!("无法打开：{e}"))?;
        self.decoder.reset();
        self.window.clear();
        let limit = self.decoder.required_window().min(READ_LIMIT);
        let mut eof = false;
        loop {
            let progress = self
                .decoder
                .decode(&self.window, eof)
                .map_err(|e| self.reason(e))?;
            let consumed = progress.consumed.min(self.window.len());
            self.window.drain(..consumed);
            match progress.status {
                Status::Frames(_) | Status::NeedOutput | Status::End => break,
                Status::NeedInput if eof => return Err("文件不完整".into()),
                Status::NeedInput => {
                    if self.window.len() >= limit {
                        return Err("文件头过大，无法读取格式".into());
                    }
                    let old = self.window.len();
                    self.window.resize(old + CHUNK, 0);
                    let n = loop {
                        match file.read(&mut self.window[old..]) {
                            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                            r => break r.map_err(|e| format!("读取失败：{e}"))?,
                        }
                    };
                    self.window.truncate(old + n);
                    eof = n == 0;
                }
            }
        }
        let kind = match self.decoder.codec() {
            Codec::Wav => Kind::Wav,
            Codec::Flac => Kind::Flac,
            Codec::Aiff => Kind::Aiff,
            _ => return Err("无法识别的文件".into()),
        };
        let format = self
            .decoder
            .format()
            .ok_or_else(|| "文件中没有音频".to_string())?;
        Ok(TrackInfo {
            kind,
            format,
            frames: self.decoder.declared_frames(),
        })
    }

    fn reason(&self, e: qy_core::DecodeError) -> String {
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
        if let Some(f) = d.flac_error() {
            return format!("{e:?}：{f:?}");
        }
        format!("{e:?}")
    }
}

/// 读取线程：依次处理请求，结果以事件交给界面线程。
pub(crate) fn run(requests: Receiver<ProbeRequest>, events: Sender<UiEvent>, notify: Notify) {
    let mut prober = match Prober::new() {
        Ok(p) => Some(p),
        Err(e) => {
            let _ = events.send(UiEvent::Fatal(e));
            notify();
            None
        }
    };
    while let Ok(req) = requests.recv() {
        match req {
            ProbeRequest::Files(list) => {
                for (entry, path) in list {
                    let result = match prober.as_mut() {
                        Some(p) => p.probe(&path),
                        None => Err("文件头读取不可用".into()),
                    };
                    if events.send(UiEvent::Probed { entry, result }).is_err() {
                        return;
                    }
                    notify();
                }
            }
            ProbeRequest::Folder(dir) => {
                let files = scan_folder(&dir);
                if events.send(UiEvent::Scanned { files }).is_err() {
                    return;
                }
                notify();
            }
        }
    }
}
