use crate::audio::ipc::EventLoopCreationError;
use audio_device::audio_connection::DeviceInfo;
use comms_lib::packet::ClientId;
use std::time::Duration;

pub enum AudioIPCInitStatus {
    Ready,
    Error(EventLoopCreationError),
}

pub enum UICommand {
    AudioIPCInitStatus(AudioIPCInitStatus),
    DevicesFound {
        _input_devices: Vec<DeviceInfo>,
        _output_devices: Vec<DeviceInfo>,
    },
    UserIsSpeaking {
        client_id: ClientId,
        duration: Duration,
    },
    CurrentUserStartedSpeaking,
    CurrentUserStoppedSpeaking,
}
