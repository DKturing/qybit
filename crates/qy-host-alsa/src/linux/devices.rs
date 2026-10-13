//! 列出可播放的 `hw` 设备（`snd_card_next`、`snd_ctl_pcm_next_device`、`snd_ctl_pcm_info`）。

use alsa::Direction;
use alsa::ctl::{Ctl, DeviceIter};

/// 一个可播放的 hw 设备。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HwEntry {
    /// `hw:CARD=<id>,DEV=<n>`。
    pub name: String,
    pub card_index: i32,
    pub card_name: String,
    pub pcm_name: String,
}

/// 枚举全部声卡上支持播放方向的 PCM 设备。
pub fn list_playback() -> Result<Vec<HwEntry>, String> {
    let mut out = Vec::new();
    for card in alsa::card::Iter::new() {
        let card = card.map_err(|e| e.to_string())?;
        let ctl = Ctl::from_card(&card, false).map_err(|e| e.to_string())?;
        let info = ctl.card_info().map_err(|e| e.to_string())?;
        let id = info.get_id().map_err(|e| e.to_string())?.to_string();
        let card_name = info.get_name().unwrap_or("").to_string();
        for dev in DeviceIter::new(&ctl) {
            let Ok(dev) = u32::try_from(dev) else {
                continue;
            };
            if let Ok(pi) = ctl.pcm_info(dev, 0, Direction::Playback) {
                out.push(HwEntry {
                    name: format!("hw:CARD={id},DEV={dev}"),
                    card_index: card.get_index(),
                    card_name: card_name.clone(),
                    pcm_name: pi.get_name().unwrap_or("").to_string(),
                });
            }
        }
    }
    Ok(out)
}
