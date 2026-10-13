//! 解析 `snd_pcm_dump_hw_setup` 的输出，取精确有理数采样率与有效位。
//!
//! alsa crate 0.12.1 没有包装 `snd_pcm_hw_params_get_rate_numden` 与
//! `snd_pcm_hw_params_get_sbits`，也不暴露底层指针；而 `snd_pcm_dump_hw_setup` 打印的正是
//! PCM 对象上已应用的同一组字段（alsa-lib `src/pcm/pcm.c`，取自 alsa-project/alsa-lib master）：
//!
//! ```text
//!   exact rate   : %g (%u/%u)        ← pcm->rate_num / pcm->rate_den
//!   msbits       : %u                ← pcm->msbits
//! ```
//!
//! 解析不到精确采样率时返回 `None`，调用方按“分母为 0”处理，核对必然报错（严格失败，
//! 不放行）；解析不到有效位时为 `None`，核对不据此拒绝。

/// 从转储文本中取 `(rate_num, rate_den)` 与 `msbits`。
pub fn parse_hw_setup(text: &str) -> (Option<(u32, u32)>, Option<u32>) {
    let mut numden = None;
    let mut msbits = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "exact rate" => {
                numden = value
                    .split_once('(')
                    .and_then(|(_, r)| r.split_once(')'))
                    .and_then(|(frac, _)| frac.split_once('/'))
                    .and_then(|(n, d)| Some((n.trim().parse().ok()?, d.trim().parse().ok()?)));
            }
            "msbits" => msbits = value.trim().parse().ok(),
            _ => {}
        }
    }
    (numden, msbits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_alsa_lib_dump_format() {
        let dump = "  stream       : PLAYBACK\n  access       : RW_INTERLEAVED\n  format       : S24_3LE\n  subformat    : STD\n  channels     : 2\n  rate         : 44100\n  exact rate   : 44100 (44100/1)\n  msbits       : 24\n  buffer_size  : 1764\n  period_size  : 441\n  period_time  : 10000\n";
        assert_eq!(parse_hw_setup(dump), (Some((44_100, 1)), Some(24)));
        // 非整数采样率按原样取出有理数。
        let dump = "  exact rate   : 44100.3 (132301/3)\n  msbits       : 32\n";
        assert_eq!(parse_hw_setup(dump), (Some((132_301, 3)), Some(32)));
        // 缺项或格式异常：取不到。
        assert_eq!(parse_hw_setup(""), (None, None));
        assert_eq!(
            parse_hw_setup("  exact rate   : 0 (x/1)\n  msbits       : n/a\n"),
            (None, None)
        );
        assert_eq!(parse_hw_setup("  exact rate   : 48000\n"), (None, None));
    }
}
