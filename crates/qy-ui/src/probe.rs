//! 后台读取文件头（格式与声明的时长）与扫描文件夹。文件头由格式集合的读取器解析
//! （[`Formats::prober`]，与播放同一套解析与拒绝规则），只读到格式确定为止，不解码整个文件。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use qy_player::Formats;
pub use qy_player::TrackInfo;

use crate::session::{Notify, UiEvent};

/// 文件头读取状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    Pending,
    Ready(TrackInfo),
    /// 无法播放的原因（与播放时的拒绝一致）。
    Failed(String),
}

/// 官方解码器可加入播放列表的扩展名（小写比较）；其他格式集合见 [`Formats::extensions`]。
pub const EXTENSIONS: [&str; 8] = qy_player::assembly::BUILTIN_EXTENSIONS;

/// 扫描文件夹的最大深度（防止符号链接环）。
const MAX_DEPTH: usize = 32;

pub(crate) enum ProbeRequest {
    Files(Vec<(u64, PathBuf)>),
    Folder(PathBuf),
}

/// 递归列出文件夹中官方解码器能播放的音频文件（按路径排序）。
pub fn scan_folder(root: &Path) -> Vec<PathBuf> {
    scan_folder_with(root, |p| qy_player::Builtin.is_audio_path(p))
}

/// 递归列出文件夹中 `is_audio` 为真的文件（按路径排序）。
pub fn scan_folder_with(root: &Path, is_audio: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
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
                Ok(t) if t.is_file() && is_audio(&p) => out.push(p),
                _ => {}
            }
        }
    }
    out.sort();
    out
}

/// 读取线程：依次处理请求，结果以事件交给界面线程。
pub(crate) fn run<F: Formats>(
    formats: Arc<F>,
    requests: Receiver<ProbeRequest>,
    events: Sender<UiEvent>,
    notify: Notify,
) {
    let mut prober = match formats.prober() {
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
                let files = scan_folder_with(&dir, |p| formats.is_audio_path(p));
                if events.send(UiEvent::Scanned { files }).is_err() {
                    return;
                }
                notify();
            }
        }
    }
}
