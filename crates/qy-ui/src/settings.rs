//! 记住播放列表与设置（GUI T6，由用户选择开启，默认关闭）。
//!
//! 文本格式，每行 `键=值`，`#` 开头为注释；只取第一个 `=` 分隔，值可以含 `=`：
//!
//! ```text
//! version=1
//! device=<端点标识>
//! log_visible=1
//! log_height=120
//! window=100,80,1100,780,0
//! sort=name,asc
//! media_keys=1
//! file=C:\音乐\a.flac
//! ```
//!
//! 文件不可信：未知的键、格式不对的行忽略并计数，不 `panic`；超过 [`MAX_BYTES`] 的文件不读。
//! **不保存**单声道复制与浮点转整数：这两项是明确开启的非严格选项，每次启动默认关闭（AGENTS §3）。
//! 无法写成一行的路径（不是 UTF-8，或含换行）不保存，计入跳过数。

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use crate::model::SortKey;

/// 设置文件的版本。
pub const VERSION: u32 = 1;
/// 读取的上限（字节）。
pub const MAX_BYTES: u64 = 16 * 1024 * 1024;

/// 窗口在还原状态下的位置与大小（屏幕坐标，物理像素）与是否最大化。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub maximized: bool,
}

/// 保存的内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// 播放列表（按列表顺序）。
    pub files: Vec<PathBuf>,
    /// 选定的输出设备（平台端点标识）；`None` 为默认设备。
    pub device: Option<String>,
    pub log_visible: bool,
    /// 日志区高度（96 DPI 下的像素）；0 为默认比例。
    pub log_height: u32,
    pub window: Option<Window>,
    /// 列表排序依据与是否反向。
    pub sort: Option<(SortKey, bool)>,
    /// 全局媒体键（窗口不在前台时也响应）。
    pub media_keys: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            device: None,
            log_visible: true,
            log_height: 0,
            window: None,
            sort: None,
            media_keys: true,
        }
    }
}

/// 解析结果：设置与忽略的行数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub settings: Settings,
    pub ignored: usize,
}

/// 能否写成一行（键值都不能含换行）。
fn one_line(s: &str) -> bool {
    !s.contains(['\n', '\r'])
}

impl Settings {
    /// 写成文本；返回文本与无法保存而跳过的路径数。
    pub fn to_text(&self) -> (String, usize) {
        let mut out = format!("# QyBit 图形界面的设置（自动生成）\nversion={VERSION}\n");
        let mut skipped = 0;
        if let Some(d) = self.device.as_deref().filter(|d| one_line(d)) {
            out.push_str(&format!("device={d}\n"));
        }
        out.push_str(&format!(
            "log_visible={}\nlog_height={}\nmedia_keys={}\n",
            u8::from(self.log_visible),
            self.log_height,
            u8::from(self.media_keys)
        ));
        if let Some(w) = self.window {
            out.push_str(&format!(
                "window={},{},{},{},{}\n",
                w.left,
                w.top,
                w.right,
                w.bottom,
                u8::from(w.maximized)
            ));
        }
        if let Some((key, desc)) = self.sort {
            let dir = if desc { "desc" } else { "asc" };
            out.push_str(&format!("sort={},{dir}\n", key.name()));
        }
        for f in &self.files {
            match f.to_str().filter(|s| one_line(s) && !s.is_empty()) {
                Some(s) => out.push_str(&format!("file={s}\n")),
                None => skipped += 1,
            }
        }
        (out, skipped)
    }

    /// 解析文本：格式不对的行与未知的键忽略并计数；缺少的项取默认值。
    pub fn parse(text: &str) -> Parsed {
        let mut s = Settings::default();
        let mut ignored = 0;
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                ignored += 1;
                continue;
            };
            let ok = match key {
                "version" => value.parse::<u32>().is_ok_and(|v| v == VERSION),
                "device" => {
                    s.device = (!value.is_empty()).then(|| value.to_string());
                    true
                }
                "log_visible" => flag(value).map(|v| s.log_visible = v).is_some(),
                "media_keys" => flag(value).map(|v| s.media_keys = v).is_some(),
                "log_height" => value
                    .parse::<u32>()
                    .ok()
                    .filter(|&h| h <= 100_000)
                    .map(|h| s.log_height = h)
                    .is_some(),
                "window" => window(value).map(|w| s.window = Some(w)).is_some(),
                "sort" => sort(value).map(|v| s.sort = Some(v)).is_some(),
                "file" if !value.is_empty() => {
                    s.files.push(PathBuf::from(value));
                    true
                }
                _ => false,
            };
            if !ok {
                ignored += 1;
            }
        }
        Parsed {
            settings: s,
            ignored,
        }
    }
}

fn flag(v: &str) -> Option<bool> {
    match v {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

fn window(v: &str) -> Option<Window> {
    let mut it = v.split(',');
    let mut n = || it.next()?.trim().parse::<i32>().ok();
    let (left, top, right, bottom) = (n()?, n()?, n()?, n()?);
    let maximized = flag(it.next()?.trim())?;
    if it.next().is_some() || right <= left || bottom <= top {
        return None;
    }
    Some(Window {
        left,
        top,
        right,
        bottom,
        maximized,
    })
}

fn sort(v: &str) -> Option<(SortKey, bool)> {
    let (key, dir) = v.split_once(',')?;
    let desc = match dir {
        "asc" => false,
        "desc" => true,
        _ => return None,
    };
    Some((SortKey::from_name(key)?, desc))
}

/// 默认位置：Windows 为 `%APPDATA%\QyBit\qybit-gui.cfg`；其他系统为
/// `$XDG_CONFIG_HOME/qybit/qybit-gui.cfg`（未设置时 `~/.config/qybit/`）。找不到时为 `None`。
pub fn default_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?).join("QyBit")
    } else {
        match std::env::var_os("XDG_CONFIG_HOME") {
            Some(x) if !x.is_empty() => PathBuf::from(x).join("qybit"),
            _ => PathBuf::from(std::env::var_os("HOME")?)
                .join(".config")
                .join("qybit"),
        }
    };
    Some(base.join("qybit-gui.cfg"))
}

/// 读取；文件不存在时为 `Ok(None)`。
pub fn load(path: &Path) -> io::Result<Option<Parsed>> {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if meta.len() > MAX_BYTES {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("设置文件超过 {MAX_BYTES} 字节"),
        ));
    }
    let bytes = fs::read(path)?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(Some(Settings::parse(&text)))
}

/// 保存：先写同目录下的临时文件，再替换原文件（写到一半中断时原文件不受影响）。返回跳过的
/// 路径数。
pub fn save(path: &Path, s: &Settings) -> io::Result<usize> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let (text, skipped) = s.to_text();
    let tmp = path.with_extension("cfg.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)?;
    Ok(skipped)
}

/// 删除（关闭“记住”时）；文件不存在不算错误。
pub fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Settings {
        Settings {
            files: vec![
                PathBuf::from("/音乐/a=b.flac"),
                PathBuf::from("C:\\Music\\b.wav"),
            ],
            device: Some("{0.0.0.00000000}.{abc}".into()),
            log_visible: false,
            log_height: 140,
            window: Some(Window {
                left: -10,
                top: 20,
                right: 1000,
                bottom: 700,
                maximized: true,
            }),
            sort: Some((SortKey::Length, true)),
            media_keys: false,
        }
    }

    #[test]
    fn round_trip() {
        let s = sample();
        let (text, skipped) = s.to_text();
        assert_eq!(skipped, 0);
        let p = Settings::parse(&text);
        assert_eq!(p.ignored, 0, "{text}");
        assert_eq!(p.settings, s);
    }

    #[test]
    fn defaults_and_damaged_lines() {
        let p = Settings::parse("");
        assert_eq!(p.settings, Settings::default());
        assert!(p.settings.log_visible && p.settings.media_keys);
        let text = "version=1\r\nlog_visible=yes\nwindow=1,2,3\nwindow=5,5,1,1,0\nsort=name,up\n\
                    sort=bogus,asc\nlog_height=-3\nunknown=1\nno equals sign\nfile=\n\
                    # 注释\nfile=/a.wav\nversion=99\n";
        let p = Settings::parse(text);
        assert_eq!(p.ignored, 10);
        assert_eq!(p.settings.files, vec![PathBuf::from("/a.wav")]);
        assert_eq!(p.settings.window, None);
        assert_eq!(p.settings.sort, None);
        assert!(p.settings.log_visible);
    }

    #[test]
    fn unrepresentable_paths_are_skipped() {
        let mut s = Settings {
            files: vec![PathBuf::from("/a\nb.wav"), PathBuf::from("/ok.flac")],
            device: Some("bad\nid".into()),
            ..Settings::default()
        };
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            s.files
                .push(PathBuf::from(std::ffi::OsStr::from_bytes(b"/\xFF.wav")));
        }
        let (text, skipped) = s.to_text();
        assert_eq!(skipped, if cfg!(unix) { 2 } else { 1 });
        let p = Settings::parse(&text);
        assert_eq!(p.settings.files, vec![PathBuf::from("/ok.flac")]);
        assert_eq!(p.settings.device, None);
    }

    #[test]
    fn non_strict_options_are_not_saved() {
        let (text, _) = sample().to_text();
        assert!(!text.contains("mono") && !text.contains("float"), "{text}");
    }

    #[test]
    fn save_load_remove() {
        let dir = std::env::temp_dir().join(format!("qy-settings-{}", std::process::id()));
        let path = dir.join("sub").join("qybit-gui.cfg");
        assert_eq!(load(&path).ok(), Some(None));
        let s = sample();
        assert_eq!(save(&path, &s).ok(), Some(0));
        let back = load(&path).ok().flatten().map(|p| p.settings);
        assert_eq!(back, Some(s));
        assert!(!path.with_extension("cfg.tmp").exists());
        assert!(remove(&path).is_ok());
        assert!(remove(&path).is_ok(), "不存在时也不算错误");
        assert_eq!(load(&path).ok(), Some(None));
        // 非 UTF-8 内容按替换字符读出，不 panic。
        std::fs::create_dir_all(path.parent().unwrap_or(&dir)).ok();
        std::fs::write(&path, b"file=/\xFF.wav\nlog_height=9\n").ok();
        let p = load(&path).ok().flatten();
        assert_eq!(p.map(|p| p.settings.log_height), Some(9));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
