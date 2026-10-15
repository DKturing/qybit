//! 端点的硬件音量（GUI T5；Windows 平台边界，unsafe 集中于此模块）。
//!
//! 依据 Microsoft Learn《Endpoint Volume Controls》：`IAudioEndpointVolume` 在设备实现了硬件
//! 音量时使用硬件控制，音量变化在共享与独占模式下都有效；设备没有硬件音量时它是软件音量，
//! 只作用于共享模式，对独占流无效。`QueryHardwareSupport` 的 `ENDPOINT_HARDWARE_SUPPORT_VOLUME`
//! 位表示设备在硬件上实现了音量。
//!
//! 本模块只在该位置位时改变音量；软件音量既对 QyBit 的独占流无效，又会改变其他程序的共享
//! 模式音量，所以不调用。调节硬件音量不改变送到设备的数据（AGENTS §3）。
#![allow(unsafe_code)]

use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    ENDPOINT_HARDWARE_SUPPORT_MUTE, ENDPOINT_HARDWARE_SUPPORT_VOLUME, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, eMultimedia, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::core::{GUID, HSTRING};

/// 一个渲染端点的音量控制。调用方须已在本线程初始化 COM（任意套间），并在同一线程使用。
pub struct EndpointVolume {
    iface: IAudioEndpointVolume,
    support: u32,
}

/// 读回的音量状态。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VolumeLevel {
    /// 0.0–1.0 的音量位置（Windows 音量滑块使用的同一刻度）。
    pub scalar: f32,
    /// 设备报告的音量（dB）。
    pub db: f32,
    /// 设备在硬件上实现了静音且当前处于静音。
    pub muted: bool,
}

impl EndpointVolume {
    /// 打开端点 `id` 的音量控制；`None` 为系统默认渲染端点（多媒体角色，与播放时的默认设备
    /// 相同）。
    pub fn open(id: Option<&str>) -> windows::core::Result<Self> {
        // SAFETY: 调用方已初始化 COM；接口引用计数由 windows crate 管理。
        unsafe {
            let e: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device: IMMDevice = match id {
                Some(id) => e.GetDevice(&HSTRING::from(id))?,
                None => e.GetDefaultAudioEndpoint(eRender, eMultimedia)?,
            };
            let iface: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None)?;
            let support = iface.QueryHardwareSupport()?;
            Ok(Self { iface, support })
        }
    }

    /// 设备在硬件上实现了音量（只有这时 [`Self::set_scalar`] 才会调用系统接口）。
    pub fn hardware_volume(&self) -> bool {
        self.support & ENDPOINT_HARDWARE_SUPPORT_VOLUME != 0
    }

    /// 读回当前音量。静音只在设备硬件实现了静音时报告。
    pub fn level(&self) -> windows::core::Result<VolumeLevel> {
        // SAFETY: 接口有效；只读查询。
        unsafe {
            let scalar = self.iface.GetMasterVolumeLevelScalar()?;
            let db = self.iface.GetMasterVolumeLevel()?;
            let muted = self.support & ENDPOINT_HARDWARE_SUPPORT_MUTE != 0
                && self.iface.GetMute()?.as_bool();
            Ok(VolumeLevel { scalar, db, muted })
        }
    }

    /// 设置音量位置（0.0–1.0，超出时截到范围内）。设备没有硬件音量时返回 `E_NOTIMPL`，
    /// 不改变软件音量。
    pub fn set_scalar(&self, scalar: f32) -> windows::core::Result<()> {
        if !self.hardware_volume() {
            return Err(windows::Win32::Foundation::E_NOTIMPL.into());
        }
        let v = if scalar.is_nan() {
            0.0
        } else {
            scalar.clamp(0.0, 1.0)
        };
        // SAFETY: 接口有效；事件上下文 GUID 为本地值，调用期间有效。
        unsafe { self.iface.SetMasterVolumeLevelScalar(v, &GUID::zeroed()) }
    }
}
