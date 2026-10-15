//! 渲染端点与系统信息（Windows 平台边界，unsafe 集中于此模块）。
//!
//! 读不到的属性记为 `None`，不猜测。驱动版本取端点属性存储中的
//! `DEVPKEY_Device_DriverVersion`（{A8B865DD-2E3D-4094-AD97-E593A70C75D6},3），
//! 端点上通常不存在该属性，此时记为 `None`。
#![allow(unsafe_code)]

use windows::Win32::Devices::FunctionDiscovery::{
    PKEY_Device_DeviceDesc, PKEY_Device_FriendlyName, PKEY_DeviceInterface_FriendlyName,
};
use windows::Win32::Foundation::{ERROR_SUCCESS, PROPERTYKEY};
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    PKEY_AudioEndpoint_PhysicalSpeakers, eMultimedia, eRender,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToBSTR};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, STGM_READ,
};
use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
};
use windows::Win32::System::Variant::VT_UI4;
use windows::core::{GUID, HSTRING, PCWSTR, w};

use qy_player::PhysicalSpeakers;

/// 当前线程的 COM 初始化守卫；Drop 时配对 CoUninitialize。
struct ComGuard;

impl ComGuard {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: 在当前线程初始化 COM（MTA），成功后由 Drop 配对反初始化。
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: 仅在 CoInitializeEx 成功后构造本守卫。
        unsafe { CoUninitialize() };
    }
}

const PKEY_DRIVER_VERSION: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xa8b865dd_2e3d_4094_ad97_e593a70c75d6),
    pid: 3,
};

/// 渲染端点信息。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EndpointInfo {
    pub id: String,
    /// 端点友好名（如“扬声器 (USB DAC)”）。
    pub name: Option<String>,
    /// 适配器（接口）友好名。
    pub interface_name: Option<String>,
    pub description: Option<String>,
    pub driver_version: Option<String>,
}

fn property(device: &IMMDevice, key: &PROPERTYKEY) -> Option<String> {
    // SAFETY: COM 已初始化；属性存储只读打开，PROPVARIANT 读取后清理。
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let mut value = store.GetValue(key).ok()?;
        let text = PropVariantToBSTR(&value).ok().map(|b| b.to_string());
        let _ = PropVariantClear(&mut value);
        text.filter(|s| !s.is_empty())
    }
}

/// 端点属性 `PKEY_AudioEndpoint_PhysicalSpeakers`（Microsoft Learn：声道配置掩码，`VT_UI4`，
/// 位定义见 `KSAUDIO_CHANNEL_CONFIG`）。属性缺失、类型不是 `VT_UI4` 或读取失败时为
/// [`PhysicalSpeakers::NotReported`]；不用 `PropVariantToUInt32` 做类型转换，不猜测。
pub(crate) fn physical_speakers(device: &IMMDevice) -> PhysicalSpeakers {
    // SAFETY: COM 已初始化；属性存储只读打开。只在 vt 为 VT_UI4 时读取联合体的 ulVal，
    // PROPVARIANT 读取后清理。
    unsafe {
        let Ok(store) = device.OpenPropertyStore(STGM_READ) else {
            return PhysicalSpeakers::NotReported;
        };
        let Ok(mut value) = store.GetValue(&PKEY_AudioEndpoint_PhysicalSpeakers) else {
            return PhysicalSpeakers::NotReported;
        };
        let inner = &value.Anonymous.Anonymous;
        let r = if inner.vt == VT_UI4 {
            PhysicalSpeakers::Mask(inner.Anonymous.ulVal)
        } else {
            PhysicalSpeakers::NotReported
        };
        let _ = PropVariantClear(&mut value);
        r
    }
}

fn device_id(device: &IMMDevice) -> windows::core::Result<String> {
    // SAFETY: GetId 返回的字符串由调用方用 CoTaskMemFree 释放。
    unsafe {
        let pwstr = device.GetId()?;
        let id = pwstr.to_string();
        CoTaskMemFree(Some(pwstr.0 as *const _));
        id.map_err(|_| windows::core::Error::from(windows::Win32::Foundation::E_UNEXPECTED))
    }
}

fn info(device: &IMMDevice) -> windows::core::Result<EndpointInfo> {
    Ok(EndpointInfo {
        id: device_id(device)?,
        name: property(device, &PKEY_Device_FriendlyName),
        interface_name: property(device, &PKEY_DeviceInterface_FriendlyName),
        description: property(device, &PKEY_Device_DeviceDesc),
        driver_version: property(device, &PKEY_DRIVER_VERSION),
    })
}

fn enumerator() -> windows::core::Result<IMMDeviceEnumerator> {
    // SAFETY: 调用方已初始化 COM。
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

/// 列出活动渲染端点。
pub fn list_render_endpoints() -> windows::core::Result<Vec<EndpointInfo>> {
    let _com = ComGuard::new()?;
    let e = enumerator()?;
    // SAFETY: 接口由 windows crate 管理引用计数，均在 `_com` 析构前释放。
    unsafe {
        let collection = e.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        let count = collection.GetCount()?;
        let mut out = Vec::new();
        for i in 0..count {
            out.push(info(&collection.Item(i)?)?);
        }
        Ok(out)
    }
}

/// 系统默认的渲染端点（多媒体角色，即一般播放音乐时使用的默认设备）。没有活动的输出设备时
/// 返回错误。
pub fn default_render_endpoint() -> windows::core::Result<EndpointInfo> {
    let _com = ComGuard::new()?;
    let e = enumerator()?;
    // SAFETY: 接口由 windows crate 管理引用计数，均在 `_com` 析构前释放。
    let device = unsafe { e.GetDefaultAudioEndpoint(eRender, eMultimedia)? };
    info(&device)
}

/// 列出活动渲染端点的 ID 字符串。
pub fn list_render_endpoint_ids() -> windows::core::Result<Vec<String>> {
    Ok(list_render_endpoints()?.into_iter().map(|e| e.id).collect())
}

/// 按 ID 查询端点；设备不存在时返回错误。
pub fn endpoint_info(id: &str) -> windows::core::Result<EndpointInfo> {
    let _com = ComGuard::new()?;
    let e = enumerator()?;
    // SAFETY: 同上。
    let device = unsafe { e.GetDevice(&HSTRING::from(id))? };
    info(&device)
}

fn reg_string(name: PCWSTR) -> Option<String> {
    let mut buf = [0u16; 256];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: 缓冲与长度对应；RRF_RT_REG_SZ 保证以 NUL 结尾。
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
            name,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if r != ERROR_SUCCESS {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16(&buf[..len]).ok()
}

fn reg_dword(name: PCWSTR) -> Option<u32> {
    let mut v = 0u32;
    let mut size = 4u32;
    // SAFETY: 输出为 4 字节 DWORD。
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
            name,
            RRF_RT_REG_DWORD,
            None,
            Some((&mut v as *mut u32).cast()),
            Some(&mut size),
        )
    };
    (r == ERROR_SUCCESS).then_some(v)
}

/// 系统版本，如 "Windows 10 Pro 22H2 (build 19045.4291)"。
pub fn os_version() -> Option<String> {
    let product = reg_string(w!("ProductName"))?;
    let display = reg_string(w!("DisplayVersion")).unwrap_or_default();
    let build = reg_string(w!("CurrentBuildNumber")).unwrap_or_default();
    let ubr = reg_dword(w!("UBR"))
        .map(|u| format!(".{u}"))
        .unwrap_or_default();
    Some(format!("{product} {display} (build {build}{ubr})").replace("  ", " "))
}
