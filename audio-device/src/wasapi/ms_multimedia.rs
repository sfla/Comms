use crate::wasapi::stream::{Format, SampleFormat, SampleRate};
use std::mem;
use std::mem::MaybeUninit;
use windows::Win32::Foundation;
use windows::Win32::Media::{Audio, KernelStreaming, Multimedia};
use windows::Win32::System::Com;
use windows::core::HRESULT;

#[derive(Debug)]
pub enum MmregSupportedFormatsError {
    DeviceNotAvailable,
    Other(HRESULT),
}

pub fn is_format_supported(
    client: &Audio::IAudioClient3,
    waveformatex: &Audio::WAVEFORMATEX,
) -> Result<Option<Audio::WAVEFORMATEX>, MmregSupportedFormatsError> {
    let mut closest_waveformatex = MaybeUninit::<Audio::WAVEFORMATEX>::uninit();
    match unsafe {
        client.IsFormatSupported(
            Audio::AUDCLNT_SHAREMODE_SHARED,
            waveformatex,
            Some(&mut closest_waveformatex.as_mut_ptr()),
        )
    } {
        Foundation::S_OK => Ok(None),
        Foundation::S_FALSE => Ok(Some(unsafe { closest_waveformatex.assume_init() })),
        Audio::AUDCLNT_E_DEVICE_INVALIDATED => Err(MmregSupportedFormatsError::DeviceNotAvailable),
        err_result => Err(MmregSupportedFormatsError::Other(err_result)),
    }
}

pub fn format_to_waveformatextensible(format: &Format) -> Audio::WAVEFORMATEXTENSIBLE {
    let format_tag = match format.data_type {
        SampleFormat::I16 => MmregFormat::Pcm,
        SampleFormat::F32 => MmregFormat::Extensible,
    };
    let channels = format.channels;
    let sample_rate = format.sample_rate.0;
    let sample_bytes = format.data_type.sample_size() as u16;
    let avg_bytes_per_sec = channels as u32 * sample_rate * sample_bytes as u32;
    let block_align = channels * sample_bytes;
    let bits_per_sample = 8 * sample_bytes;
    let cb_size = match format_tag {
        MmregFormat::Pcm => 0,
        MmregFormat::Extensible => {
            let extensible_size = mem::size_of::<Audio::WAVEFORMATEXTENSIBLE>();
            let ex_size = mem::size_of::<Audio::WAVEFORMATEX>();
            (extensible_size - ex_size) as u16
        }
    };
    let waveformatex = Audio::WAVEFORMATEX {
        wFormatTag: format_tag.as_word(),
        nChannels: channels,
        nSamplesPerSec: sample_rate,
        nAvgBytesPerSec: avg_bytes_per_sec,
        nBlockAlign: block_align,
        wBitsPerSample: bits_per_sample,
        cbSize: cb_size,
    };

    let channel_mask = match channels {
        1 => KernelStreaming::KSAUDIO_SPEAKER_MONO,
        2 => {
            // KernelStreaming::KSAUDIO_SPEAKER_STEREO, which isn't defined in windows-rs yet
            KernelStreaming::SPEAKER_FRONT_LEFT | KernelStreaming::SPEAKER_FRONT_RIGHT
        }
        _ => KernelStreaming::KSAUDIO_SPEAKER_DIRECTOUT,
    };

    let sub_format = match format_tag {
        MmregFormat::Pcm => KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM,
        MmregFormat::Extensible => Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    };
    Audio::WAVEFORMATEXTENSIBLE {
        Format: waveformatex,
        Samples: Audio::WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: bits_per_sample,
        },
        dwChannelMask: channel_mask,
        SubFormat: sub_format,
    }
}

pub unsafe fn format_from_waveformatex_ptr(
    waveformatex_ptr: *const Audio::WAVEFORMATEX,
) -> Option<Format> {
    unsafe {
        let data_type = match (
            (*waveformatex_ptr).wBitsPerSample,
            (*waveformatex_ptr).wFormatTag as u32,
        ) {
            (16, Audio::WAVE_FORMAT_PCM) => SampleFormat::I16,
            (32, Multimedia::WAVE_FORMAT_IEEE_FLOAT) => SampleFormat::F32,
            (n_bits, KernelStreaming::WAVE_FORMAT_EXTENSIBLE) => {
                let waveformatextensible_ptr =
                    waveformatex_ptr as *const Audio::WAVEFORMATEXTENSIBLE;
                let sub = (*waveformatextensible_ptr).SubFormat;
                if n_bits == 16 && sub == KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM {
                    SampleFormat::I16
                } else if n_bits == 32 && sub == Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                    SampleFormat::F32
                } else {
                    return None;
                }
            }
            _ => return None,
        };
        let format = Format {
            channels: (*waveformatex_ptr).nChannels as _,
            sample_rate: SampleRate((*waveformatex_ptr).nSamplesPerSec),
            data_type,
        };
        Some(format)
    }
}

pub struct WaveFormatExPtr(pub *mut Audio::WAVEFORMATEX);

impl Drop for WaveFormatExPtr {
    fn drop(&mut self) {
        unsafe {
            Com::CoTaskMemFree(Some(self.0 as *mut _));
        }
    }
}

enum MmregFormat {
    Pcm,
    Extensible,
}

impl MmregFormat {
    fn as_word(&self) -> u16 {
        match self {
            MmregFormat::Pcm => Audio::WAVE_FORMAT_PCM as u16,
            MmregFormat::Extensible => KernelStreaming::WAVE_FORMAT_EXTENSIBLE as u16,
        }
    }
}
