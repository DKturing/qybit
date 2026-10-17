//! 硬件参数空间的操作（A1）：与 alsa-lib `snd_pcm_hw_params_*` 同义的纯函数。
//!
//! 内核用 `SNDRV_PCM_IOCTL_HW_REFINE` 按设备的约束收窄参数空间（掩码去掉不可行的位、区间
//! 收紧到可行范围，空则返回 `EINVAL`）。这里的函数只改结构体，收窄由调用方提供的 [`Refine`]
//! 完成：真实设备上是 ioctl，测试中是假设备。

use super::abi::{
    HwParams, INTERVAL_EMPTY, INTERVAL_INTEGER, INTERVAL_OPENMAX, INTERVAL_OPENMIN, Interval, Mask,
    Zeroed,
};
use crate::pcm::errno;

/// 按设备约束收窄参数空间（`SNDRV_PCM_IOCTL_HW_REFINE`）；失败为正 errno。
pub trait Refine {
    fn refine(&mut self, p: &mut HwParams) -> Result<(), i32>;
}

impl<F: FnMut(&mut HwParams) -> Result<(), i32>> Refine for F {
    fn refine(&mut self, p: &mut HwParams) -> Result<(), i32> {
        self(p)
    }
}

/// `_snd_pcm_hw_params_any`：全部掩码为满、全部区间为 `[0, u32::MAX]`，请求全部参数。
pub fn any() -> HwParams {
    let mut p = HwParams::zeroed();
    for m in p.masks.iter_mut() {
        *m = Mask {
            bits: [u32::MAX; 8],
        };
    }
    for i in p.intervals.iter_mut() {
        *i = Interval {
            min: 0,
            max: u32::MAX,
            flags: 0,
        };
    }
    p.rmask = u32::MAX;
    p.info = u32::MAX;
    p
}

/// 掩码只留第 `bit` 位。
pub fn set_mask(p: &mut HwParams, mask: usize, bit: u32) {
    let Some(m) = p.masks.get_mut(mask) else {
        return;
    };
    m.bits = [0; 8];
    if let Some(w) = m.bits.get_mut((bit / 32) as usize) {
        *w = 1 << (bit % 32);
    }
}

/// 掩码是否含第 `bit` 位。
pub fn mask_has(p: &HwParams, mask: usize, bit: u32) -> bool {
    p.masks
        .get(mask)
        .and_then(|m| m.bits.get((bit / 32) as usize))
        .is_some_and(|w| w & (1 << (bit % 32)) != 0)
}

/// 掩码只有一位时的位号。
pub fn mask_single(p: &HwParams, mask: usize) -> Option<u32> {
    let m = p.masks.get(mask)?;
    let mut found = None;
    for (k, &w) in m.bits.iter().enumerate() {
        if w == 0 {
            continue;
        }
        if found.is_some() || w.count_ones() != 1 {
            return None;
        }
        found = Some(k as u32 * 32 + w.trailing_zeros());
    }
    found
}

/// 区间设为闭区间 `[min, max]`（整数）。
pub fn set_interval(p: &mut HwParams, param: usize, min: u32, max: u32) {
    if let Some(i) = p.intervals.get_mut(param) {
        *i = Interval {
            min,
            max,
            flags: INTERVAL_INTEGER,
        };
    }
}

/// 区间的可取整数范围（按开闭端换算）；为空时为 `None`。
pub fn interval_range(p: &HwParams, param: usize) -> Option<(u32, u32)> {
    let i = p.intervals.get(param)?;
    if i.flags & INTERVAL_EMPTY != 0 {
        return None;
    }
    let lo = if i.flags & INTERVAL_OPENMIN != 0 {
        i.min.checked_add(1)?
    } else {
        i.min
    };
    let hi = if i.flags & INTERVAL_OPENMAX != 0 {
        i.max.checked_sub(1)?
    } else {
        i.max
    };
    (lo <= hi).then_some((lo, hi))
}

/// 区间只含一个值时的值（应用参数之后各区间都应如此）。
pub fn interval_single(p: &HwParams, param: usize) -> Option<u32> {
    let (lo, hi) = interval_range(p, param)?;
    (lo == hi).then_some(lo)
}

/// 按 `set` 改参数后收窄；收窄失败时参数不变。
fn try_with(
    p: &HwParams,
    r: &mut impl Refine,
    set: impl FnOnce(&mut HwParams),
) -> Option<HwParams> {
    let mut q = *p;
    set(&mut q);
    r.refine(&mut q).ok()?;
    Some(q)
}

/// 精确设定区间参数并收窄（`snd_pcm_hw_params_set_*` 的精确版本）：与当前范围求交，不在当前
/// 范围内或设备不接受该值时为 `EINVAL`，参数不变。
pub fn set_exact(p: &mut HwParams, param: usize, v: u32, r: &mut impl Refine) -> Result<(), i32> {
    if !interval_range(p, param).is_some_and(|(lo, hi)| (lo..=hi).contains(&v)) {
        return Err(errno::EINVAL);
    }
    match try_with(p, r, |q| set_interval(q, param, v, v)) {
        Some(q) if interval_single(&q, param) == Some(v) => {
            *p = q;
            Ok(())
        }
        _ => Err(errno::EINVAL),
    }
}

/// 设为设备在当前参数空间内接受的、最接近 `target` 的值（`snd_pcm_hw_params_set_*_near`）。
/// 先试 `target` 本身；不行时分别求不小于它的最小可行值与不大于它的最大可行值，取较近者，
/// 距离相同取较小者；精确设定时设备仍不接受则试另一个。返回设定的值；参数空间为空时为
/// `EINVAL`，参数不变。
pub fn set_near(
    p: &mut HwParams,
    param: usize,
    target: u32,
    r: &mut impl Refine,
) -> Result<u32, i32> {
    if set_exact(p, param, target, r).is_ok() {
        return Ok(target);
    }
    let (lo, hi) = interval_range(p, param).ok_or(errno::EINVAL)?;
    let up = (target <= hi)
        .then(|| try_with(p, r, |q| set_interval(q, param, target.max(lo), hi)))
        .flatten()
        .and_then(|q| interval_range(&q, param))
        .map(|(a, _)| a);
    let down = (target >= lo)
        .then(|| try_with(p, r, |q| set_interval(q, param, lo, target.min(hi))))
        .flatten()
        .and_then(|q| interval_range(&q, param))
        .map(|(_, b)| b);
    let mut candidates: Vec<u32> = up.into_iter().chain(down).collect();
    candidates.sort_by_key(|&c| (c.abs_diff(target), c));
    candidates.dedup();
    for c in candidates {
        if set_exact(p, param, c, r).is_ok() {
            return Ok(c);
        }
    }
    Err(errno::EINVAL)
}

/// 指针回绕点（内核 `snd_pcm_hw_params` 中 `runtime->boundary` 的同一公式，`long` 为本进程的
/// 宽度；32 位进程在 64 位内核上运行时内核也按 32 位重算）：缓冲长度不断加倍，直到再加倍会
/// 超过 `LONG_MAX − buffer`。
pub fn boundary(buffer_frames: usize) -> usize {
    let max = isize::MAX as usize;
    let mut b = buffer_frames.max(1);
    while b.checked_mul(2).is_some_and(|d| d <= max - buffer_frames) {
        b *= 2;
    }
    b
}

#[cfg(test)]
mod tests {
    use super::super::abi::{
        ACCESS_RW_INTERLEAVED, CHANNELS, MASK_ACCESS, MASK_FORMAT, PERIOD_SIZE, RATE,
    };
    use super::*;

    /// 假设备：周期为 `step` 的倍数且在 `[pmin, pmax]` 内；采样率只接受列出的值；格式只接受
    /// S16_LE（2）与 S32_LE（10）。按内核的语义收窄：掩码求交、区间收紧，空则 `EINVAL`。
    struct Fake {
        step: u32,
        pmin: u32,
        pmax: u32,
        rates: &'static [u32],
        calls: usize,
    }

    impl Refine for Fake {
        fn refine(&mut self, p: &mut HwParams) -> Result<(), i32> {
            self.calls += 1;
            let m = &mut p.masks[MASK_FORMAT].bits[0];
            *m &= (1 << 2) | (1 << 10);
            if *m == 0 {
                return Err(errno::EINVAL);
            }
            let (lo, hi) = interval_range(p, PERIOD_SIZE).ok_or(errno::EINVAL)?;
            let lo = lo.max(self.pmin).div_ceil(self.step) * self.step;
            let hi = hi.min(self.pmax) / self.step * self.step;
            if lo > hi {
                return Err(errno::EINVAL);
            }
            set_interval(p, PERIOD_SIZE, lo, hi);
            let (rlo, rhi) = interval_range(p, RATE).ok_or(errno::EINVAL)?;
            let ok: Vec<u32> = self
                .rates
                .iter()
                .copied()
                .filter(|r| (rlo..=rhi).contains(r))
                .collect();
            match (ok.first(), ok.last()) {
                (Some(&a), Some(&b)) => set_interval(p, RATE, a, b),
                _ => return Err(errno::EINVAL),
            }
            Ok(())
        }
    }

    fn fake() -> Fake {
        Fake {
            step: 64,
            pmin: 64,
            pmax: 8192,
            rates: &[44_100, 48_000, 96_000],
            calls: 0,
        }
    }

    #[test]
    fn masks() {
        let mut p = any();
        assert!(mask_has(&p, MASK_FORMAT, 32));
        assert_eq!(mask_single(&p, MASK_FORMAT), None);
        set_mask(&mut p, MASK_ACCESS, ACCESS_RW_INTERLEAVED);
        assert_eq!(mask_single(&p, MASK_ACCESS), Some(3));
        set_mask(&mut p, MASK_FORMAT, 32);
        assert_eq!(mask_single(&p, MASK_FORMAT), Some(32));
        assert!(!mask_has(&p, MASK_FORMAT, 2));
        set_mask(&mut p, MASK_FORMAT, 300); // 超出掩码：清空，不越界。
        assert_eq!(mask_single(&p, MASK_FORMAT), None);
    }

    #[test]
    fn intervals_with_open_ends() {
        let mut p = any();
        set_interval(&mut p, CHANNELS, 2, 2);
        assert_eq!(interval_single(&p, CHANNELS), Some(2));
        p.intervals[CHANNELS].flags |= INTERVAL_OPENMIN | INTERVAL_OPENMAX;
        assert_eq!(interval_range(&p, CHANNELS), None);
        p.intervals[CHANNELS] = Interval {
            min: 1,
            max: 9,
            flags: INTERVAL_OPENMIN,
        };
        assert_eq!(interval_range(&p, CHANNELS), Some((2, 9)));
        p.intervals[CHANNELS].flags |= INTERVAL_EMPTY;
        assert_eq!(interval_range(&p, CHANNELS), None);
    }

    #[test]
    fn exact_rate_is_exact() {
        let mut d = fake();
        let mut p = any();
        assert_eq!(set_exact(&mut p, RATE, 48_000, &mut d), Ok(()));
        assert_eq!(interval_single(&p, RATE), Some(48_000));
        let before = p;
        assert_eq!(set_exact(&mut p, RATE, 44_100, &mut d), Err(errno::EINVAL));
        assert_eq!(p, before, "失败时参数不变");
        let mut p = any();
        assert_eq!(set_exact(&mut p, RATE, 88_200, &mut d), Err(errno::EINVAL));
    }

    #[test]
    fn near_picks_the_closest_acceptable_value() {
        let mut d = fake();
        // 可接受：64 的倍数。
        for (target, want) in [
            (1024, 1024),
            (1000, 1024), // 960 与 1024 中 1024 较近（24 < 40）
            (1050, 1024),
            (992, 960), // 960 与 1024 同距 32：取较小
            (1, 64),
            (100_000, 8192),
        ] {
            let mut p = any();
            assert_eq!(
                set_near(&mut p, PERIOD_SIZE, target, &mut d),
                Ok(want),
                "目标 {target}"
            );
            assert_eq!(interval_single(&p, PERIOD_SIZE), Some(want));
        }
        // 参数空间已收窄到 [128, 256]：只能在其中取。
        let mut p = any();
        set_interval(&mut p, PERIOD_SIZE, 128, 256);
        assert_eq!(set_near(&mut p, PERIOD_SIZE, 4096, &mut d), Ok(256));
        assert!(d.calls > 0);
    }

    #[test]
    fn near_fails_on_an_empty_space() {
        let mut d = fake();
        d.pmin = 9000; // 大于 pmax：无可行值。
        let mut p = any();
        let before = p;
        assert_eq!(
            set_near(&mut p, PERIOD_SIZE, 1024, &mut d),
            Err(errno::EINVAL)
        );
        assert_eq!(p, before);
    }

    #[test]
    fn boundary_follows_the_kernel_formula() {
        let b = boundary(4096);
        assert_eq!(b % 4096, 0);
        assert!(b.is_power_of_two());
        assert!(b <= isize::MAX as usize - 4096);
        assert!(
            b.checked_mul(2)
                .is_none_or(|d| d > isize::MAX as usize - 4096)
        );
        if usize::BITS == 64 {
            assert_eq!(b, 1 << 62);
        } else {
            assert_eq!(b, 1 << 30);
        }
    }
}
