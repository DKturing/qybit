//! 主循环的活跃与休眠统计（L1）。
//!
//! 板级在主循环中用自己的计时器（如 Cortex-M3/M4/M7 的 DWT 周期计数器）量出每轮的活跃时间与每次
//! 休眠（WFI）的时间，交给 [`Activity`] 累计；串口命令 `stats` 经 [`crate::Player::write_stats`]
//! 报告。时间单位是计时器的计数，报告时按板级给出的频率换算。
//!
//! 活跃比例只统计播放与暂停期间：此时 DMA 每个缓冲至少唤醒一次，单次休眠远短于 32 位周期计数器
//! 的回绕周期（96 MHz 下约 44 s）。停止时的休眠可能任意长，只计次数，不计时间。

/// 累计的活跃与休眠时间（计时器计数）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Activity {
    /// 活跃时间合计（播放或暂停期间）。
    pub active: u64,
    /// 休眠时间合计（播放或暂停期间）。
    pub asleep: u64,
    /// 调用 `step` 的次数与单次最长耗时。
    pub steps: u64,
    pub max_step: u32,
    /// 休眠次数（含停止时的休眠）。
    pub sleeps: u64,
}

impl Activity {
    pub const fn new() -> Self {
        Self {
            active: 0,
            asleep: 0,
            steps: 0,
            max_step: 0,
            sleeps: 0,
        }
    }

    /// 记录一次 `step` 的耗时。
    pub fn step(&mut self, ticks: u32) {
        self.steps = self.steps.saturating_add(1);
        self.max_step = self.max_step.max(ticks);
    }

    /// 记录一段活跃时间（一轮主循环中休眠以外的部分）；`session` 为是否在播放或暂停。
    pub fn active(&mut self, ticks: u32, session: bool) {
        if session {
            self.active = self.active.saturating_add(u64::from(ticks));
        }
    }

    /// 记录一次休眠；`session` 同 [`Self::active`]，为假时只计次数。
    pub fn slept(&mut self, ticks: u32, session: bool) {
        self.sleeps = self.sleeps.saturating_add(1);
        if session {
            self.asleep = self.asleep.saturating_add(u64::from(ticks));
        }
    }

    /// 播放与暂停期间的活跃比例（千分比）；尚无记录时为 `None`。
    pub fn active_permille(&self) -> Option<u64> {
        let total = self.active.checked_add(self.asleep)?;
        (total > 0).then(|| self.active.saturating_mul(1000) / total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratio_counts_only_session_time() {
        let mut a = Activity::new();
        assert_eq!(a.active_permille(), None);
        a.step(300);
        a.step(120);
        a.active(400, true);
        a.slept(1_600, true);
        // 停止时的长休眠只计次数。
        a.active(50, false);
        a.slept(u32::MAX, false);
        assert_eq!(a.active_permille(), Some(200));
        assert_eq!((a.steps, a.max_step, a.sleeps), (2, 300, 2));
        assert_eq!((a.active, a.asleep), (400, 1_600));
    }
}
