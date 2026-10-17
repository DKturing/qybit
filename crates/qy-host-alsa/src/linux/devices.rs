//! 列出可播放的 `hw` 设备（A1：内核控制接口 `CARD_INFO`、`PCM_NEXT_DEVICE`、`PCM_INFO`，与
//! alsa-lib `snd_card_next`、`snd_ctl_pcm_next_device`、`snd_ctl_pcm_info` 的结果相同）。

use super::ctl::{Ctl, cards};
use crate::kernel::abi::{STREAM_PLAYBACK, c_text};

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
    for card in cards() {
        let ctl = Ctl::open(card).map_err(|e| format!("声卡 {card}：{e}"))?;
        let info = ctl.card_info().map_err(|e| format!("声卡 {card}：{e}"))?;
        let id = c_text(&info.id);
        let card_name = c_text(&info.name);
        let mut dev = -1;
        while let Some(d) = ctl
            .next_pcm_device(dev)
            .map_err(|e| format!("声卡 {card}：{e}"))?
        {
            if let Ok(pi) = ctl.pcm_info(d, 0, STREAM_PLAYBACK) {
                out.push(HwEntry {
                    name: format!("hw:CARD={id},DEV={d}"),
                    card_index: info.card,
                    card_name: card_name.clone(),
                    pcm_name: c_text(&pi.name),
                });
            }
            let Ok(next) = i32::try_from(d) else {
                break;
            };
            dev = next;
        }
    }
    Ok(out)
}
