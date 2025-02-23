use std::ops::Deref;

use windows::Win32::Devices::Properties;
use windows::Win32::Foundation;
use windows::Win32::Foundation::WIN32_ERROR;
use windows::Win32::Media::Audio;
use windows::Win32::System::Com::StructuredStorage;
use windows::Win32::System::{Com, Threading};

use crate::buffer::{MicSink, PlaybackSource};
use crate::wasapi::audio_connection::WasapiCreationError;
use crate::wasapi::com;
use crate::wasapi::device::DefaultFormatError::BackendSpecific;
use crate::wasapi::event_loop::WasapiEventLoopRegistration;
use crate::wasapi::ms_multimedia::{
    MmregSupportedFormatsError, WaveFormatExPtr, format_from_waveformatex_ptr,
    format_to_waveformatextensible, is_format_supported,
};
use crate::wasapi::sample_rate_conversion::{
    create_mic_sample_rate_converter, create_playback_sample_rate_converter,
};
use crate::wasapi::stream::{Format, RawWasapiStream, WasapiMicStream, WasapiPlaybackStream};

pub struct InputDevice {
    device: IMMDeviceWrapper,
    pub audio_client: IAudioClientWrapper,
}

pub struct OutputDevice {
    device: IMMDeviceWrapper,
    pub audio_client: IAudioClientWrapper,
}

pub struct IAudioClientWrapper(Audio::IAudioClient3);

impl IAudioClientWrapper {
    pub fn initialize(&self) -> Result<(Format, Audio::WAVEFORMATEX), WasapiCreationError> {
        let audio_client = &self.0;
        let format = self
            .default_format()
            .map_err(WasapiCreationError::NoInputFormat)?;
        let mut format_attempt = format_to_waveformatextensible(&format);

        match is_format_supported(audio_client, &format_attempt.Format) {
            Ok(None) => {}
            Ok(Some(format)) => {
                format_attempt.Format.nChannels = format.nChannels;
                format_attempt.Format.nSamplesPerSec = format.nSamplesPerSec;
                format_attempt.Format.nAvgBytesPerSec = format.nAvgBytesPerSec;
                format_attempt.Format.nBlockAlign = format.nBlockAlign;
                format_attempt.Format.wBitsPerSample = format.wBitsPerSample;
                format_attempt.Format.cbSize = format.cbSize;
            }
            Err(MmregSupportedFormatsError::DeviceNotAvailable) => {
                return Err(WasapiCreationError::InputInitializationFailure(
                    BuildStreamError::DeviceNotAvailable,
                ));
            }
            Err(MmregSupportedFormatsError::Other(hresult)) => {
                return Err(WasapiCreationError::InputInitializationFailure(
                    BuildStreamError::BackendSpecific(format!(
                        "Unexpected failure during format-support checking: {}",
                        hresult.0,
                    )),
                ));
            }
        }

        let mut default_period = 0u32;
        let mut fundamental_period = 0u32;
        let mut min_period = 0u32;
        let mut max_period = 0u32;
        unsafe {
            audio_client
                .GetSharedModeEnginePeriod(
                    &format_attempt.Format,
                    &mut default_period,
                    &mut fundamental_period,
                    &mut min_period,
                    &mut max_period,
                )
                .unwrap();
        }

        match unsafe {
            audio_client.InitializeSharedAudioStream(
                Audio::AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                min_period,
                &format_attempt.Format,
                None,
            )
        } {
            Err(ref e) if e.code() == Audio::AUDCLNT_E_DEVICE_INVALIDATED => {
                return Err(WasapiCreationError::InputInitializationFailure(
                    BuildStreamError::DeviceNotAvailable,
                ));
            }
            Err(e) => {
                let description = format!("{e}");
                return Err(WasapiCreationError::InputInitializationFailure(
                    BuildStreamError::BackendSpecific(description),
                ));
            }
            Ok(()) => (),
        };

        Ok((format, format_attempt.Format))
    }

    pub fn get_buffer_max_frames(&self) -> Result<u32, BuildStreamError> {
        match unsafe { self.0.GetBufferSize() } {
            Err(ref e) if e.code() == Audio::AUDCLNT_E_DEVICE_INVALIDATED => {
                Err(BuildStreamError::DeviceNotAvailable)
            }
            Err(e) => {
                let description = format!("failed to obtain buffer size: {e}");
                Err(BuildStreamError::BackendSpecific(description))
            }
            Ok(max_frames_in_buffer) => Ok(max_frames_in_buffer),
        }
    }

    pub fn create_notify(&self) -> Result<WasapiEventLoopRegistration, BuildStreamError> {
        let event = match unsafe { Threading::CreateEventA(None, false, false, None) } {
            Ok(event) => event,
            Err(err) => {
                return Err(BuildStreamError::BackendSpecific(format!(
                    "Failed to create event: {err:?}"
                )));
            }
        };

        if let Err(e) = unsafe { self.0.SetEventHandle(event) } {
            let description = format!("failed to call SetEventHandle: {e}");
            return Err(BuildStreamError::BackendSpecific(description));
        }

        Ok(WasapiEventLoopRegistration { win_handle: event })
    }

    fn create_client<T: windows_core::Interface>(&self) -> Result<T, BuildStreamError> {
        match unsafe { self.0.GetService() } {
            Err(ref e) if e.code() == Audio::AUDCLNT_E_DEVICE_INVALIDATED => {
                Err(BuildStreamError::DeviceNotAvailable)
            }
            Err(e) => {
                let description = format!("failed to build render client: {e}");
                Err(BuildStreamError::BackendSpecific(description))
            }
            Ok(client) => Ok(client),
        }
    }

    fn default_format(&self) -> Result<Format, DefaultFormatError> {
        let format_ptr = match unsafe { self.0.GetMixFormat() } {
            Err(ref e) if e.code() == Audio::AUDCLNT_E_DEVICE_INVALIDATED => {
                return Err(DefaultFormatError::DeviceNotAvailable);
            }
            Err(e) => {
                let description = format!("{e}");
                return Err(BackendSpecific(description));
            }
            Ok(format) => format,
        };

        let format_ptr = WaveFormatExPtr(format_ptr);
        unsafe {
            format_from_waveformatex_ptr(format_ptr.0)
                .ok_or(DefaultFormatError::StreamTypeNotSupported)
        }
    }

    fn from_immdevice(
        immdevice: &Audio::IMMDevice,
    ) -> Result<IAudioClientWrapper, windows::core::Error> {
        let audio_client: Audio::IAudioClient3 =
            unsafe { immdevice.Activate(Com::CLSCTX_ALL, None) }?;
        Ok(IAudioClientWrapper(audio_client))
    }
}

impl Deref for IAudioClientWrapper {
    type Target = Audio::IAudioClient3;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug)]
pub enum DefaultFormatError {
    DeviceNotAvailable,
    StreamTypeNotSupported,
    BackendSpecific(String),
}

struct IMMDeviceWrapper(Audio::IMMDevice);

#[derive(Debug)]
pub struct DeviceNameError {
    pub description: String,
}

impl IMMDeviceWrapper {
    fn name(&self) -> Result<String, DeviceNameError> {
        let property_store = unsafe { self.0.OpenPropertyStore(Com::STGM_READ) }.unwrap();

        let mut property_value = match unsafe {
            property_store.GetValue(&std::mem::transmute::<
                Foundation::DEVPROPKEY,
                Foundation::PROPERTYKEY,
            >(Properties::DEVPKEY_Device_FriendlyName))
        } {
            Ok(property_value) => property_value,
            Err(err) => {
                let description = format!("failed to retrieve name from property store: {err}");
                return Err(DeviceNameError { description });
            }
        };

        let name_string = property_value.to_string();
        unsafe { StructuredStorage::PropVariantClear(&mut property_value) }.unwrap();
        Ok(name_string)
    }

    fn id(&self) -> String {
        unsafe {
            self.0
                .GetId()
                .expect(
                    "Getting ImmDevice ID should only fail if out of memory or pointer is wrong",
                )
                .to_string()
                .expect("Could not convert ImmDevice ID to string")
        }
    }

    fn default(data_flow: Audio::EDataFlow) -> Option<IMMDeviceWrapper> {
        let communication_device_id = match unsafe {
            com::ENUMERATOR
                .0
                .GetDefaultAudioEndpoint(data_flow, Audio::eCommunications)
                .and_then(|d| d.GetId())
        } {
            Ok(id) => id,
            Err(ref e) if WIN32_ERROR::from_error(e) == Some(Foundation::ERROR_NOT_FOUND) => {
                return None;
            }
            Err(e) => panic!("{}", e),
        };

        let communication_device_id = unsafe { communication_device_id.to_hstring() };
        let device = match unsafe { com::ENUMERATOR.0.GetDevice(&communication_device_id) } {
            Ok(device) => device,
            Err(ref e) if WIN32_ERROR::from_error(e) == Some(Foundation::ERROR_NOT_FOUND) => {
                return None;
            }
            Err(e) => panic!("{}", e),
        };
        Some(IMMDeviceWrapper(device))
    }

    fn endpoints(data_flow: Audio::EDataFlow) -> Vec<IMMDeviceWrapper> {
        let filled_collection = unsafe {
            com::ENUMERATOR
                .0
                .EnumAudioEndpoints(data_flow, Audio::DEVICE_STATE_ACTIVE)
        }
        .unwrap();

        unsafe {
            let device_count = filled_collection.GetCount().unwrap();

            let mut result: Vec<IMMDeviceWrapper> = Vec::new();
            for i in 0..device_count {
                let device = filled_collection.Item(i).unwrap();
                result.push(IMMDeviceWrapper(device));
            }

            result
        }
    }
}

impl InputDevice {
    pub fn name(&self) -> Result<String, DeviceNameError> {
        self.device.name()
    }

    pub fn id(&self) -> String {
        self.device.id()
    }

    pub fn create_client(&self) -> Result<Audio::IAudioCaptureClient, BuildStreamError> {
        self.audio_client.create_client()
    }

    pub fn default() -> Option<InputDevice> {
        com::initialized();
        IMMDeviceWrapper::default(Audio::eCapture).and_then(|device| {
            IAudioClientWrapper::from_immdevice(&device.0)
                .ok()
                .map(|audio_client| InputDevice {
                    device,
                    audio_client,
                })
        })
    }

    pub fn get_list() -> Vec<InputDevice> {
        com::initialized();
        IMMDeviceWrapper::endpoints(Audio::eCapture)
            .into_iter()
            .map(|wrapper| InputDevice {
                audio_client: IAudioClientWrapper::from_immdevice(&wrapper.0).unwrap(),
                device: wrapper,
            })
            .collect()
    }

    pub fn into_stream<M: MicSink>(self) -> Result<WasapiMicStream<M>, WasapiCreationError> {
        println!(
            "Using \"{}\" as the microphone.",
            self.name().expect("Failed to determine mic name")
        );
        let (format, waveformatex) = self.audio_client.initialize()?;
        let mic_converter = create_mic_sample_rate_converter(format.sample_rate.0, 48000);
        Ok(WasapiMicStream::new(
            mic_converter,
            self.id(),
            self.create_client().unwrap(),
            self.audio_client.create_notify().unwrap(),
            RawWasapiStream::new(waveformatex, self.audio_client, format),
        ))
    }
}

impl OutputDevice {
    pub fn name(&self) -> Result<String, DeviceNameError> {
        self.device.name()
    }

    pub fn id(&self) -> String {
        self.device.id()
    }

    pub fn create_client(&self) -> Result<Audio::IAudioRenderClient, BuildStreamError> {
        self.audio_client.create_client()
    }

    pub fn default() -> Option<OutputDevice> {
        com::initialized();
        IMMDeviceWrapper::default(Audio::eRender).and_then(|device| {
            IAudioClientWrapper::from_immdevice(&device.0)
                .ok()
                .map(|audio_client| OutputDevice {
                    device,
                    audio_client,
                })
        })
    }

    pub fn get_list() -> Vec<OutputDevice> {
        com::initialized();
        IMMDeviceWrapper::endpoints(Audio::eRender)
            .into_iter()
            .map(|wrapper| OutputDevice {
                audio_client: IAudioClientWrapper::from_immdevice(&wrapper.0).unwrap(),
                device: wrapper,
            })
            .collect()
    }

    pub fn into_stream<P: PlaybackSource>(
        self,
    ) -> Result<WasapiPlaybackStream<P>, WasapiCreationError> {
        println!(
            "Using \"{}\" as the playback device.",
            self.name()
                .expect("Failed to determine playback device name")
        );
        let (format, waveformatex) = self.audio_client.initialize()?;
        let mic_converter = create_playback_sample_rate_converter(48000, format.sample_rate.0);
        Ok(WasapiPlaybackStream::new(
            mic_converter,
            self.id(),
            self.create_client().unwrap(),
            self.audio_client.create_notify().unwrap(),
            RawWasapiStream::new(waveformatex, self.audio_client, format),
        ))
    }
}

#[derive(Debug)]
pub enum BuildStreamError {
    DeviceNotAvailable,
    FormatNotSupported,
    BackendSpecific(String),
}
