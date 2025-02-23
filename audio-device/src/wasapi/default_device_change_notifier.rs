use crate::wasapi::com;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Media::Audio;
use windows::Win32::Media::Audio::{
    EDataFlow, ERole, IMMNotificationClient, IMMNotificationClient_Impl,
};
use windows::Win32::System::Threading;
use windows::core::PCWSTR;

#[windows::core::implement(IMMNotificationClient)]
pub struct CommsImmNotificationClient(HANDLE);

pub struct DefaultDeviceChangeHandle(pub HANDLE, #[allow(dead_code)] IMMNotificationClient);

impl CommsImmNotificationClient {
    pub fn listen_for_default_device_changes() -> DefaultDeviceChangeHandle {
        com::initialized();
        let handle = unsafe { Threading::CreateEventA(None, false, false, None) }.unwrap();
        let interface: IMMNotificationClient = Self(handle).into();
        unsafe {
            com::ENUMERATOR
                .0
                .RegisterEndpointNotificationCallback(&interface)
        }
        .unwrap();
        DefaultDeviceChangeHandle(handle, interface)
    }
}

impl IMMNotificationClient_Impl for CommsImmNotificationClient_Impl {
    fn OnDeviceStateChanged(
        &self,
        _: &PCWSTR,
        _: Audio::DEVICE_STATE,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnDeviceAdded(&self, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnDeviceRemoved(&self, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        _: EDataFlow,
        _: ERole,
        _: &PCWSTR,
    ) -> windows::core::Result<()> {
        unsafe {
            Threading::SetEvent(self.0).ok().unwrap();
        }
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _: &PCWSTR,
        _: &windows::Win32::Foundation::PROPERTYKEY,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}
