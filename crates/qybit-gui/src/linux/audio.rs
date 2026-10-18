//! 输出设备与播放期间的设备硬件音量（Linux）。
//!
//! - 设备：内核直连 ALSA 的 `hw` 播放设备（[`list_playback`]）；没有系统默认设备，下拉框第 0 项
//!   为“请选择输出设备”（[`crate::app::DefaultDevice::None`]）。
//! - 音量（tasks/A4.md，与命令行 `--volume` 相同）：开始播放时把该卡的一组播放音量控制设为界面
//!   上的百分比（缺省 40%，与 alsamixer 相同的映射）并打开同组的静音开关，播放结束（停止、出错、
//!   换设备、退出）时恢复原值；播放中拖动滑块只改这组控制，结束时仍恢复为播放前的值。播放之外
//!   不改动混音器（AGENTS §3）。只调设备自身的硬件音量，送出的数据不变；没有可用的硬件音量时照常
//!   播放并说明原因，不以软件缩放代替。

use std::sync::{Arc, Mutex, MutexGuard};

use qy_host_alsa::AlsaBackend;
use qy_host_alsa::linux::KernelOpener;
use qy_host_alsa::linux::devices::list_playback;
use qy_host_alsa::linux::mixer::{PlaybackVolume, playback_hint};
use qy_host_alsa::linux::priority::RtPriority;
use qy_player::Backend;
use qy_player::report::OpenFailure;
use qy_ui::session::Notify;

use crate::app::Device;

/// 播放期间默认的设备硬件音量（与命令行相同，tasks/A4.md）。
pub const DEFAULT_PERCENT: u32 = 40;

/// 全部可播放的 hw 设备。枚举失败时为空并给出原因。
pub fn devices() -> (Vec<Device>, Option<String>) {
    match list_playback() {
        Ok(list) if list.is_empty() => (
            Vec::new(),
            Some(
                "没有找到可播放的 ALSA hw 设备：/dev/snd 下没有声卡，或当前用户没有访问权限\
                 （把用户加入 audio 组后重新登录）"
                    .into(),
            ),
        ),
        Ok(list) => (
            list.into_iter()
                .map(|e| Device {
                    name: Some(format!("{} — {}（{}）", e.card_name, e.pcm_name, e.name)),
                    id: e.name,
                })
                .collect(),
            None,
        ),
        Err(e) => (Vec::new(), Some(format!("读取 ALSA 设备列表失败：{e}"))),
    }
}

struct Inner {
    /// 播放时是否调整设备硬件音量（“播放”菜单，可关闭）。
    enabled: bool,
    percent: u32,
    /// 当前播放的设备与音量守卫（播放结束时恢复）。
    active: Option<(String, Option<PlaybackVolume>)>,
    /// 交给界面写入日志的说明。
    notes: Vec<String>,
    /// 当前的读数（如 `Master：音量 59（范围 0..87），40%，-21.00 dB`）。
    reading: Option<String>,
}

/// 播放线程与界面线程共享的音量状态。
pub struct Volume {
    inner: Mutex<Inner>,
    notify: Notify,
}

impl Volume {
    pub fn new(percent: u32, enabled: bool, notify: Notify) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                enabled,
                percent: percent.min(100),
                active: None,
                notes: Vec::new(),
                reading: None,
            }),
            notify,
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn percent(&self) -> u32 {
        self.lock().percent
    }

    pub fn enabled(&self) -> bool {
        self.lock().enabled
    }

    /// 开关“播放时调整设备硬件音量”。关闭时正在播放的会话立即恢复原值。
    pub fn set_enabled(&self, on: bool) {
        let mut g = self.lock();
        g.enabled = on;
        if !on
            && let Some((_, guard)) = g.active.as_mut()
            && let Some(v) = guard.take()
        {
            let line = match v.restore() {
                Ok(l) | Err(l) => l,
            };
            g.notes.push(line);
            g.reading = None;
        }
    }

    /// 界面的音量滑块。正在播放并已调整音量时立即写入这组控制（不写入日志，标签显示读数）。
    pub fn set_percent(&self, percent: u32) {
        let mut g = self.lock();
        g.percent = percent.min(100);
        let p = g.percent;
        let mut reading = None;
        if let Some((_, Some(v))) = g.active.as_mut() {
            reading = Some(v.set_percent(p));
        }
        match reading {
            Some(Ok(r)) => g.reading = Some(r),
            Some(Err(e)) => g.notes.push(format!("调整设备硬件音量失败：{e}")),
            None => {}
        }
    }

    /// 取出待写入日志的说明。
    pub fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut self.lock().notes)
    }

    /// 音量标签的文字。
    pub fn label(&self) -> String {
        let g = self.lock();
        if !g.enabled {
            return "播放时不调整设备硬件音量".into();
        }
        match &g.active {
            Some((_, Some(_))) => {
                let name = g
                    .reading
                    .as_deref()
                    .and_then(|r| r.split('：').next())
                    .unwrap_or("");
                format!("设备硬件音量 {}%（{name}，播放结束后恢复）", g.percent)
            }
            Some((_, None)) => format!("未调整设备硬件音量（见日志），设定 {}%", g.percent),
            None => format!("播放时设备硬件音量 {}%", g.percent),
        }
    }

    /// 播放线程：会话开始（创建后端）时调用。
    fn begin(&self, device: &str) {
        let mut g = self.lock();
        let mut notes = Vec::new();
        let guard = if g.enabled {
            match PlaybackVolume::apply(device, g.percent, None) {
                Ok((v, line)) => {
                    notes.push(line);
                    g.reading = v.describe().ok();
                    Some(v)
                }
                Err(e) => {
                    notes.push(format!("未调整设备硬件音量：{e}（不以软件缩放代替）"));
                    None
                }
            }
        } else {
            None
        };
        if let Some(h) = playback_hint(device) {
            notes.push(h);
        }
        g.active = Some((device.to_string(), guard));
        g.notes.extend(notes);
        drop(g);
        (self.notify)();
    }

    /// 播放线程：会话结束（后端释放）时调用，恢复原值。
    fn end(&self) {
        let mut g = self.lock();
        if let Some((_, Some(v))) = g.active.take() {
            let line = match v.restore() {
                Ok(l) => l,
                Err(e) => format!("警告：{e}"),
            };
            g.notes.push(line);
        }
        g.active = None;
        g.reading = None;
        drop(g);
        (self.notify)();
    }
}

/// 界面用的 ALSA 输出后端：内核直连的 hw 设备，会话期间持有音量守卫。
pub struct GuiBackend {
    inner: AlsaBackend<KernelOpener, RtPriority>,
    volume: Arc<Volume>,
}

type Inner2 = AlsaBackend<KernelOpener, RtPriority>;

impl Backend for GuiBackend {
    type Attempt = <Inner2 as Backend>::Attempt;
    type Pending = <Inner2 as Backend>::Pending;

    fn open(
        &mut self,
        source: &qy_core::SourceFormat,
    ) -> Result<Self::Pending, OpenFailure<Self::Attempt>> {
        self.inner.open(source)
    }
}

impl Drop for GuiBackend {
    fn drop(&mut self) {
        self.volume.end();
    }
}

/// 播放线程按设备标识创建后端（[`qy_ui::Ui::new`] 的 `make_backend`）。
pub fn make_backend(
    volume: Arc<Volume>,
) -> impl FnMut(Option<&str>) -> Result<GuiBackend, String> + Send + 'static {
    move |device: Option<&str>| {
        let device = device.ok_or("未选择输出设备：请先在设备下拉框中选择")?;
        volume.begin(device);
        Ok(GuiBackend {
            inner: AlsaBackend::new(device, KernelOpener, RtPriority::default()),
            volume: volume.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::Volume;

    #[test]
    fn label_follows_state() {
        let v = Volume::new(40, true, Arc::new(|| {}));
        assert_eq!(v.label(), "播放时设备硬件音量 40%");
        v.set_percent(150);
        assert_eq!(v.percent(), 100);
        v.set_enabled(false);
        assert_eq!(v.label(), "播放时不调整设备硬件音量");
        assert!(v.take_notes().is_empty());
    }
}
