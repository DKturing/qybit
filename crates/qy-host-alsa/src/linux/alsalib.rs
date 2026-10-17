//! alsa-lib PCM（alsa crate 0.12.1，安全封装；本模块不含 `unsafe`）。A1 起只在默认关闭的
//! `alsa-lib` 特性下编译，供 A 层回环测试使用 alsa-lib 的 `file` 插件；发布版用
//! [`super::KernelPcm`]。
//!
//! 参数设置（出处：alsa-lib PCM hw_params 文档）：
//! - `snd_pcm_hw_params_set_access(RW_INTERLEAVED)`、`set_format`、`set_channels`（精确）；
//! - `set_rate_resample(0)` 关闭 alsa-lib 重采样，`set_rate(rate, dir = 0)` 精确设置采样率
//!   （不用 `set_rate_near`）；
//! - 周期与缓冲用 `*_near` 取设备在该配置空间内接受的值（不改变样本），设置后作为请求值；
//! - `snd_pcm_hw_params` 应用后用 `snd_pcm_hw_params_current` 读回；精确有理数采样率与
//!   有效位从 `snd_pcm_dump_hw_setup` 取得（见 [`crate::dump`]）；
//! - 软件参数：`start_threshold = boundary`（只由显式 `snd_pcm_start` 启动），
//!   `avail_min = 周期`。

use std::ffi::CString;
use std::time::Instant;

use alsa::pcm::{Access as A, Format, HwParams, PCM};
use alsa::{Direction, Output, ValueOr};
use qy_pcm_format::alsa::{Access, AlsaFormat, ChmapType, HwApplied, HwRequest};

use crate::dump::parse_hw_setup;
use crate::pcm::{Configured, HwPlan, Opener, Pcm, PcmError, errno};

fn e(call: &'static str, err: alsa::Error) -> PcmError {
    PcmError {
        call,
        errno: err.errno().saturating_abs(),
    }
}

/// 映射的原始位置值：经 alsa-lib 自己的文本形式（`snd_pcm_chmap_print`）严格解析
/// （见 [`crate::chmap`]），不经 alsa crate 会 `unwrap` 的枚举转换；打印或解析失败为 `None`。
fn chmap_positions(m: &alsa::pcm::Chmap) -> Option<Vec<u32>> {
    use std::fmt::Write as _;
    let mut text = String::new();
    write!(text, "{m}").ok()?;
    crate::chmap::parse_chmap_text(&text)
}

fn to_alsa(f: AlsaFormat) -> Format {
    match f {
        AlsaFormat::S16Le => Format::S16LE,
        AlsaFormat::S24_3Le => Format::S243LE,
        AlsaFormat::S24Le => Format::S24LE,
        AlsaFormat::S32Le => Format::S32LE,
        AlsaFormat::FloatLe => Format::FloatLE,
        AlsaFormat::Float64Le => Format::Float64LE,
    }
}

/// 真实打开器（`snd_pcm_open`，阻塞、播放）。
#[derive(Debug, Clone, Copy, Default)]
pub struct RealOpener;

impl Opener for RealOpener {
    type Pcm = RealPcm;

    fn open(&mut self, name: &str) -> Result<RealPcm, PcmError> {
        let c = CString::new(name).map_err(|_| PcmError {
            call: "snd_pcm_open",
            errno: errno::EINVAL,
        })?;
        let pcm = PCM::open(&c, Direction::Playback, false).map_err(|x| e("snd_pcm_open", x))?;
        Ok(RealPcm {
            pcm,
            rate: 0,
            last_write: None,
        })
    }

    fn occupants(&self, card: u32, device: u32) -> Vec<String> {
        super::occupants::find(card, device)
    }
}

pub struct RealPcm {
    pcm: PCM,
    rate: u32,
    /// 上次写入后的时间与设备中待播帧数（`snd_pcm_delay`），用于估计 xrun 缺失帧。
    last_write: Option<(Instant, i64)>,
}

impl RealPcm {
    /// 测试用：打开任意 PCM 名（绕过 `hw:` 检查，只用于 alsa-lib 内置 `null` 插件的冒烟测试）。
    #[cfg(test)]
    pub(crate) fn open_any(name: &str) -> Result<Self, PcmError> {
        RealOpener.open(name)
    }

    fn read_back(&self) -> Result<HwApplied, PcmError> {
        let cur = self
            .pcm
            .hw_params_current()
            .map_err(|x| e("snd_pcm_hw_params_current", x))?;
        let access = match cur.get_access() {
            Ok(A::RWInterleaved) => Access::RwInterleaved,
            Ok(A::MMapInterleaved) => Access::MmapInterleaved,
            Ok(_) => Access::Other,
            Err(x) => return Err(e("snd_pcm_hw_params_get_access", x)),
        };
        let format_code = cur
            .get_format()
            .map_err(|x| e("snd_pcm_hw_params_get_format", x))? as i32;
        let channels = cur
            .get_channels()
            .map_err(|x| e("snd_pcm_hw_params_get_channels", x))?;
        let period = cur
            .get_period_size()
            .map_err(|x| e("snd_pcm_hw_params_get_period_size", x))?;
        let buffer = cur
            .get_buffer_size()
            .map_err(|x| e("snd_pcm_hw_params_get_buffer_size", x))?;
        let resample = cur
            .get_rate_resample()
            .map_err(|x| e("snd_pcm_hw_params_get_rate_resample", x))?;
        let mut out = Output::buffer_open().map_err(|x| e("snd_output_buffer_open", x))?;
        self.pcm
            .dump_hw_setup(&mut out)
            .map_err(|x| e("snd_pcm_dump_hw_setup", x))?;
        let text = out.buffer_string(|b| String::from_utf8_lossy(b).into_owned());
        let (numden, sbits) = parse_hw_setup(&text);
        // 取不到精确采样率时记为分母 0：核对必然报错。
        let (rate_num, rate_den) = numden.unwrap_or((0, 0));
        Ok(HwApplied {
            access,
            format_code,
            rate_num,
            rate_den,
            channels,
            period_frames: u32::try_from(period).unwrap_or(u32::MAX),
            buffer_frames: u32::try_from(buffer).unwrap_or(u32::MAX),
            resample,
            significant_bits: sbits,
        })
    }
}

impl Pcm for RealPcm {
    fn supported_formats(&mut self) -> Result<Vec<AlsaFormat>, PcmError> {
        let hwp = HwParams::any(&self.pcm).map_err(|x| e("snd_pcm_hw_params_any", x))?;
        hwp.set_access(A::RWInterleaved)
            .map_err(|x| e("snd_pcm_hw_params_set_access", x))?;
        Ok(AlsaFormat::QUERY
            .into_iter()
            .filter(|&f| hwp.test_format(to_alsa(f)).is_ok())
            .collect())
    }

    fn configure(&mut self, plan: &HwPlan) -> Result<Configured, PcmError> {
        let (period, buffer) = {
            let hwp = HwParams::any(&self.pcm).map_err(|x| e("snd_pcm_hw_params_any", x))?;
            hwp.set_access(A::RWInterleaved)
                .map_err(|x| e("snd_pcm_hw_params_set_access", x))?;
            hwp.set_format(to_alsa(plan.format))
                .map_err(|x| e("snd_pcm_hw_params_set_format", x))?;
            hwp.set_channels(u32::from(plan.channels))
                .map_err(|x| e("snd_pcm_hw_params_set_channels", x))?;
            hwp.set_rate_resample(false)
                .map_err(|x| e("snd_pcm_hw_params_set_rate_resample", x))?;
            hwp.set_rate(plan.sample_rate, ValueOr::Nearest)
                .map_err(|x| e("snd_pcm_hw_params_set_rate", x))?;
            let period = hwp
                .set_period_size_near(i64::from(plan.period_frames), ValueOr::Nearest)
                .map_err(|x| e("snd_pcm_hw_params_set_period_size_near", x))?;
            let buffer = hwp
                .set_buffer_size_near(period.saturating_mul(i64::from(plan.periods)))
                .map_err(|x| e("snd_pcm_hw_params_set_buffer_size_near", x))?;
            self.pcm
                .hw_params(&hwp)
                .map_err(|x| e("snd_pcm_hw_params", x))?;
            (period, buffer)
        };
        let applied = self.read_back()?;
        {
            let swp = self
                .pcm
                .sw_params_current()
                .map_err(|x| e("snd_pcm_sw_params_current", x))?;
            let boundary = swp
                .get_boundary()
                .map_err(|x| e("snd_pcm_sw_params_get_boundary", x))?;
            swp.set_start_threshold(boundary)
                .map_err(|x| e("snd_pcm_sw_params_set_start_threshold", x))?;
            swp.set_avail_min(period)
                .map_err(|x| e("snd_pcm_sw_params_set_avail_min", x))?;
            self.pcm
                .sw_params(&swp)
                .map_err(|x| e("snd_pcm_sw_params", x))?;
        }
        self.rate = plan.sample_rate;
        self.last_write = None;
        Ok(Configured {
            request: HwRequest {
                format: plan.format,
                sample_rate: plan.sample_rate,
                channels: plan.channels,
                layout: plan.layout,
                period_frames: u32::try_from(period).unwrap_or(u32::MAX),
                buffer_frames: u32::try_from(buffer).unwrap_or(u32::MAX),
            },
            applied,
        })
    }

    fn hw_free(&mut self) -> Result<(), PcmError> {
        self.pcm.hw_free().map_err(|x| e("snd_pcm_hw_free", x))
    }

    fn start(&mut self) -> Result<(), PcmError> {
        self.pcm.start().map_err(|x| e("snd_pcm_start", x))
    }

    fn drop_now(&mut self) -> Result<(), PcmError> {
        self.pcm.drop().map_err(|x| e("snd_pcm_drop", x))
    }

    fn drain(&mut self) -> Result<(), PcmError> {
        self.pcm.drain().map_err(|x| e("snd_pcm_drain", x))
    }

    fn prepare(&mut self) -> Result<(), PcmError> {
        self.last_write = None;
        self.pcm.prepare().map_err(|x| e("snd_pcm_prepare", x))
    }

    fn wait(&mut self, timeout_ms: u32) -> Result<bool, PcmError> {
        self.pcm
            .wait(Some(timeout_ms))
            .map_err(|x| e("snd_pcm_wait", x))
    }

    fn avail(&mut self) -> Result<u64, PcmError> {
        let n = self.pcm.avail().map_err(|x| e("snd_pcm_avail", x))?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    fn writei(&mut self, frames: &[u8]) -> Result<usize, PcmError> {
        let n = self
            .pcm
            .io_bytes()
            .writei(frames)
            .map_err(|x| e("snd_pcm_writei", x))?;
        if let Ok(d) = self.pcm.delay() {
            self.last_write = Some((Instant::now(), d));
        }
        Ok(n)
    }

    fn xrun_missing_frames(&mut self) -> u32 {
        // 估计：上次写入后设备还有 delay 帧可播；其后的时间都在缺数据。
        let Some((at, delay)) = self.last_write else {
            return 0;
        };
        let elapsed = at.elapsed().as_nanos();
        let played = elapsed.saturating_mul(u128::from(self.rate)) / 1_000_000_000;
        let missing = played.saturating_sub(u128::try_from(delay.max(0)).unwrap_or(0));
        u32::try_from(missing).unwrap_or(u32::MAX)
    }

    fn query_chmaps(&mut self) -> Vec<(ChmapType, Vec<u32>)> {
        self.pcm
            .query_chmaps()
            .filter_map(|(t, m)| {
                let t = match t {
                    alsa::pcm::ChmapType::None => ChmapType::None,
                    alsa::pcm::ChmapType::Fixed => ChmapType::Fixed,
                    alsa::pcm::ChmapType::Var => ChmapType::Var,
                    alsa::pcm::ChmapType::Paired => ChmapType::Paired,
                };
                Some((t, chmap_positions(&m)?))
            })
            .collect()
    }

    fn set_chmap(&mut self, positions: &[u32]) -> Result<(), PcmError> {
        let all = alsa::pcm::ChmapPosition::all();
        let list: Option<Vec<alsa::pcm::ChmapPosition>> = positions
            .iter()
            .map(|&p| all.get(p as usize).copied())
            .collect();
        let Some(list) = list else {
            return Err(PcmError {
                call: "snd_pcm_set_chmap",
                errno: errno::EINVAL,
            });
        };
        let map = alsa::pcm::Chmap::from(&list[..]);
        self.pcm
            .set_chmap(&map)
            .map_err(|x| e("snd_pcm_set_chmap", x))
    }

    fn get_chmap(&mut self) -> Result<Option<Vec<u32>>, PcmError> {
        match self.pcm.get_chmap() {
            Ok(m) => Ok(chmap_positions(&m)),
            // alsa crate 对 NULL 返回 unsupported：设备不支持读回。
            Err(_) => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qy_core::SourceFormat;
    use qy_pcm_format::alsa::confirm;

    /// alsa-lib 内置 `null` 插件（不是 hw 设备，只验证真实绑定的调用路径）：设置参数、
    /// 读回并核对、写入、启动、排空。本地没有 alsa-lib 配置时明确打印“未运行”；CI 中必须运行。
    #[test]
    fn null_plugin_smoke() {
        let mut pcm = match RealPcm::open_any("null") {
            Ok(p) => p,
            Err(err) => {
                assert!(
                    std::env::var_os("CI").is_none(),
                    "CI 中必须运行，但无法打开 alsa-lib null 插件（{err}）"
                );
                eprintln!("未运行：无法打开 alsa-lib null 插件（{err}）");
                return;
            }
        };
        let supported = match pcm.supported_formats() {
            Ok(s) => s,
            Err(err) => panic!("{err}"),
        };
        assert!(supported.contains(&AlsaFormat::S16Le), "{supported:?}");
        let plan = HwPlan {
            format: AlsaFormat::S16Le,
            sample_rate: 48_000,
            channels: 2,
            layout: qy_pcm_format::ChannelLayout::STEREO,
            period_frames: 480,
            periods: 4,
        };
        let c = match pcm.configure(&plan) {
            Ok(c) => c,
            Err(err) => panic!("{err}"),
        };
        eprintln!("null 插件读回：{:?}", c.applied);
        let src = match SourceFormat::new(48_000, 2, 16) {
            Ok(s) => s,
            Err(err) => panic!("{err:?}"),
        };
        assert!(
            confirm(&src, &c.request, &c.applied).is_ok(),
            "{:?} vs {:?}",
            c.request,
            c.applied
        );
        assert_eq!((c.applied.rate_num, c.applied.rate_den), (48_000, 1));
        let frames = vec![0u8; c.request.period_frames as usize * 4];
        let room = pcm.avail().unwrap_or(0);
        assert!(room >= u64::from(c.request.period_frames));
        match pcm.writei(&frames) {
            Ok(n) => assert_eq!(n, c.request.period_frames as usize),
            Err(err) => panic!("{err}"),
        }
        assert_eq!(pcm.start(), Ok(()));
        assert_eq!(pcm.drain(), Ok(()));
    }
}
