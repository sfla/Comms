mod buffer;
pub mod ipc;
mod platform;

use crate::audio::buffer::{MicBuffer, RcMultiplexer};
use crate::audio::ipc::{AudioCommand, AudioReceiver};
pub use crate::audio::platform::AudioPlatform;
use crate::network::SendToNetworkThreadError;
use crate::network::ipc::NetworkHandle;
use crate::ui::ipc::{AudioIPCInitStatus, UICommand};
use audio_device::audio_connection::EventLoop;
use std::fmt::{Display, Formatter};
use std::sync::mpsc;
use std::sync::mpsc::Receiver;

#[cfg(windows)]
mod wasapi;
#[cfg(windows)]
pub type AudioPlatformImpl = crate::audio::wasapi::WasapiAudioPlatform;

#[cfg(unix)]
mod pulseaudio;
#[cfg(unix)]
pub type AudioPlatformImpl = crate::audio::pulseaudio::PulseAudioPlatform;

pub fn run(
    parameters: <AudioPlatformImpl as AudioPlatform>::Parameters,
    network_handle: NetworkHandle,
    ui_handle: async_channel::Sender<UICommand>,
    registration: <AudioPlatformImpl as AudioPlatform>::Registration,
    receiver: Receiver<AudioCommand>,
) -> AudioError {
    let multiplexer = RcMultiplexer::new();
    let (tx, rx) = mpsc::channel();
    let receiver = AudioReceiver::new(
        multiplexer.clone(),
        registration,
        receiver,
        ui_handle.clone(),
        network_handle.clone(),
        tx,
    );
    let mic_buffer = match MicBuffer::new(network_handle, ui_handle.clone(), rx) {
        Ok(b) => b,
        Err(e) => return e.into(),
    };
    let event_loop =
        match AudioPlatformImpl::create_event_loop(parameters, receiver, mic_buffer, multiplexer) {
            Ok(l) => l,
            Err(e) => {
                ui_handle
                    .try_send(UICommand::AudioIPCInitStatus(AudioIPCInitStatus::Error(e)))
                    .unwrap();
                return AudioError::InitializationError;
            }
        };
    ui_handle
        .try_send(UICommand::AudioIPCInitStatus(AudioIPCInitStatus::Ready))
        .unwrap();
    event_loop.run();
}

#[derive(Debug)]
pub enum AudioError {
    InitializationError,
    SendToNetworkThread(SendToNetworkThreadError),
    Opus(audiopus::Error),
}

impl Display for AudioError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioError::InitializationError => f.write_str("Initialization failed"),
            AudioError::SendToNetworkThread(e) => e.fmt(f),
            AudioError::Opus(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for AudioError {}

impl From<SendToNetworkThreadError> for AudioError {
    fn from(err: SendToNetworkThreadError) -> Self {
        AudioError::SendToNetworkThread(err)
    }
}

impl From<audiopus::Error> for AudioError {
    fn from(err: audiopus::Error) -> Self {
        AudioError::Opus(err)
    }
}
