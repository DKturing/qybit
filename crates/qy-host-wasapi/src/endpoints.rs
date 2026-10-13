//! 渲染端点枚举（Windows 平台边界，unsafe 集中于此模块）。
#![allow(unsafe_code)]

use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, IMMDeviceEnumerator, MMDeviceEnumerator, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};

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

/// 列出活动渲染端点的 ID 字符串。
pub fn list_render_endpoint_ids() -> windows::core::Result<Vec<String>> {
    let _com = ComGuard::new()?;
    // SAFETY: COM 已在本线程初始化；接口由 windows crate 管理引用计数，
    // 且全部在 `_com` 析构前释放（局部变量逆序 drop）。
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let collection = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        let count = collection.GetCount()?;
        let mut ids = Vec::new();
        for i in 0..count {
            let device = collection.Item(i)?;
            let pwstr = device.GetId()?;
            let id = pwstr.to_string();
            // GetId 返回的字符串由调用方用 CoTaskMemFree 释放。
            CoTaskMemFree(Some(pwstr.0 as *const _));
            ids.push(id.map_err(|_| {
                windows::core::Error::from(windows::Win32::Foundation::E_UNEXPECTED)
            })?);
        }
        Ok(ids)
    }
}
