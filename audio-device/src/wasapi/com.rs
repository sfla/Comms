use windows::Win32::Media::Audio;
use windows::Win32::System::Com;

lazy_static! {
    pub static ref ENUMERATOR: Enumerator = {
        initialized();

        // building the devices enumerator object
        unsafe {
            let enumerator: Audio::IMMDeviceEnumerator = Com::CoCreateInstance(
                &Audio::MMDeviceEnumerator,
                None,
                Com::CLSCTX_ALL,
            ).unwrap();
            Enumerator(enumerator)
        }
    };
}

pub struct Enumerator(pub Audio::IMMDeviceEnumerator);
unsafe impl Send for Enumerator {}
unsafe impl Sync for Enumerator {}

thread_local!(static COM_INITIALIZED: ComInitialized = {
    unsafe {
        Com::CoInitializeEx(None, Com::COINIT_MULTITHREADED).unwrap();
        ComInitialized
    }
});

pub struct ComInitialized;

impl Drop for ComInitialized {
    fn drop(&mut self) {
        unsafe { Com::CoUninitialize() };
    }
}

pub fn initialized() {
    COM_INITIALIZED.with(|_| {});
}
