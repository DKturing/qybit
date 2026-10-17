//! 渲染端点的变化通知（GUI T6；Windows 平台边界，unsafe 集中于此模块）。
//!
//! 依据 Microsoft Learn《IMMNotificationClient interface》：客户端经
//! `IMMDeviceEnumerator::RegisterEndpointNotificationCallback` 注册后，系统在端点增加、移除、
//! 状态变化、默认设备变化与属性变化时调用回调；回调在系统线程上执行，不得阻塞、不得等待其他
//! 线程，也不得在回调中注册或注销客户端。本模块的回调只调用调用方给出的 `on_change`（前端用它
//! 投递一条窗口消息），重新枚举在前端自己的线程完成。属性变化（如音量、格式属性）不影响设备列表，
//! 不通知。

#![allow(unsafe_code)]

use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::core::{PCWSTR, implement};

/// 变化时调用的函数（在系统线程上调用，必须立即返回）。
type OnChange = Box<dyn Fn() + Send + Sync>;

#[implement(IMMNotificationClient)]
struct Client {
    on_change: OnChange,
}

impl IMMNotificationClient_Impl for Client_Impl {
    fn OnDeviceStateChanged(
        &self,
        _id: &PCWSTR,
        _state: DEVICE_STATE,
    ) -> windows::core::Result<()> {
        (self.on_change)();
        Ok(())
    }

    fn OnDeviceAdded(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        (self.on_change)();
        Ok(())
    }

    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        (self.on_change)();
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        _role: ERole,
        _id: &PCWSTR,
    ) -> windows::core::Result<()> {
        if flow == eRender {
            (self.on_change)();
        }
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _id: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

/// 已注册的端点变化通知；析构时注销。在注册它的线程上使用与析构（调用方须已初始化 COM）。
pub struct EndpointWatcher {
    enumerator: IMMDeviceEnumerator,
    client: IMMNotificationClient,
}

impl EndpointWatcher {
    /// 注册通知。`on_change` 在系统线程上调用，必须立即返回（例如只投递一条消息）。
    pub fn new(on_change: impl Fn() + Send + Sync + 'static) -> windows::core::Result<Self> {
        let client: IMMNotificationClient = Client {
            on_change: Box::new(on_change),
        }
        .into();
        // SAFETY: 调用方已在本线程初始化 COM；接口由 windows crate 管理引用计数，客户端在注销前
        // 由本结构持有。
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            enumerator.RegisterEndpointNotificationCallback(&client)?;
            Ok(Self { enumerator, client })
        }
    }
}

impl Drop for EndpointWatcher {
    fn drop(&mut self) {
        // SAFETY: 注销本结构注册的同一客户端；在注册的线程上调用，不在回调中调用。
        unsafe {
            let _ = self
                .enumerator
                .UnregisterEndpointNotificationCallback(&self.client);
        }
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

    use super::EndpointWatcher;

    /// 注册与注销（CI runner 没有音频设备，端点服务照常提供通知接口）。
    #[test]
    fn register_and_unregister() {
        // SAFETY: 在本测试线程初始化 COM，结束前配对反初始化；监视器在反初始化之前析构。
        let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let w = EndpointWatcher::new(|| {});
        assert!(w.is_ok(), "{:?}", w.err());
        drop(w);
        if com {
            // SAFETY: 与上面成功的 CoInitializeEx 配对。
            unsafe { CoUninitialize() };
        }
    }
}
