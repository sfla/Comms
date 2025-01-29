use crate::audio_connection::{EventLoop, EventLoopNotify};
use crate::buffer::{InterruptReceiver, MaybeSharedPlaybackSource, MicSink, PlaybackSource};
use crate::wasapi::audio_connection::WasapiAudioConnection;
use crate::wasapi::device::{InputDevice, OutputDevice};
use crate::wasapi::stream::StreamError;
use windows::Win32::Foundation;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::{SystemServices, Threading};

pub struct WasapiEventLoopRegistration {
    pub win_handle: HANDLE,
}

unsafe impl Send for WasapiEventLoopRegistration {}

#[derive(Clone)]
pub struct WasapiEventLoopNotify {
    pub win_handle: HANDLE,
}

unsafe impl Send for WasapiEventLoopNotify {}

impl EventLoopNotify for WasapiEventLoopNotify {
    fn set_ready(&self) {
        unsafe {
            Threading::SetEvent(self.win_handle).ok().unwrap();
        }
    }
}

pub fn event_loop_channel() -> (WasapiEventLoopRegistration, WasapiEventLoopNotify) {
    let win_handle = unsafe { Threading::CreateEventA(None, false, false, None) }.unwrap();
    (
        WasapiEventLoopRegistration { win_handle },
        WasapiEventLoopNotify { win_handle },
    )
}

#[derive(Clone)]
enum WasapiHandle {
    MicStream(HANDLE),
    PlaybackStream(HANDLE),
    DefaultDeviceChange(HANDLE),
    Receiver(HANDLE),
}

impl WasapiHandle {
    fn get_win_handle(&self) -> HANDLE {
        match self {
            WasapiHandle::MicStream(handle) => *handle,
            WasapiHandle::PlaybackStream(handle) => *handle,
            WasapiHandle::DefaultDeviceChange(handle) => *handle,
            WasapiHandle::Receiver(handle) => *handle,
        }
    }
}

trait SelectWasapiHandle {
    fn select(&mut self) -> Result<WasapiHandle, String>;
}

impl SelectWasapiHandle for Vec<WasapiHandle> {
    fn select(&mut self) -> Result<WasapiHandle, String> {
        let win_handles: Vec<HANDLE> = self.iter().map(|h| h.get_win_handle()).collect();
        let handle_index = wait_for_handle_signal(&win_handles)?;

        Ok(self.get(handle_index).cloned().unwrap())
    }
}

pub struct WasapiEventLoopBuilder<
    M: MicSink,
    P: PlaybackSource,
    SP: MaybeSharedPlaybackSource<P>,
    I: InterruptReceiver<WasapiEventLoopRegistration>,
> {
    connection: WasapiAudioConnection<M, P, SP>,
    receiver: Option<I>,
}

impl<
        M: MicSink,
        P: PlaybackSource,
        SP: MaybeSharedPlaybackSource<P>,
        I: InterruptReceiver<WasapiEventLoopRegistration>,
    > WasapiEventLoopBuilder<M, P, SP, I>
{
    pub fn new(connection: WasapiAudioConnection<M, P, SP>) -> Self {
        Self {
            connection,
            receiver: None,
        }
    }

    pub fn with_interrupt(mut self, receiver: I) -> Self {
        self.receiver = Some(receiver);
        self
    }

    pub fn create_event_loop(self) -> WasapiEventLoop<M, P, SP, I> {
        WasapiEventLoop {
            connection: self.connection,
            receiver: self.receiver,
        }
    }
}

pub struct WasapiEventLoop<
    M: MicSink,
    P: PlaybackSource,
    SP: MaybeSharedPlaybackSource<P>,
    I: InterruptReceiver<WasapiEventLoopRegistration>,
> {
    connection: WasapiAudioConnection<M, P, SP>,
    receiver: Option<I>,
}

impl<
        M: MicSink,
        P: PlaybackSource,
        SP: MaybeSharedPlaybackSource<P>,
        I: InterruptReceiver<WasapiEventLoopRegistration>,
    > EventLoop for WasapiEventLoop<M, P, SP, I>
{
    type Registration = WasapiEventLoopRegistration;
    type Notify = WasapiEventLoopNotify;
    type AudioConnection = WasapiAudioConnection<M, P, SP>;

    fn run(mut self) -> ! {
        // 4 items:
        // * mic stream
        // * playback stream
        // * default-device-change handle
        // * event loop receiver
        let mut handles = Vec::<WasapiHandle>::with_capacity(4);
        loop {
            handles.clear();
            if let Some(stream) = &self.connection.mic_stream {
                handles.push(WasapiHandle::MicStream(stream.notify.win_handle));
            }
            if let Some(stream) = &self.connection.playback_stream {
                handles.push(WasapiHandle::PlaybackStream(stream.notify.win_handle));
            }
            handles.push(WasapiHandle::DefaultDeviceChange(
                self.connection.on_default_device_change.0,
            ));
            if let Some(receiver) = &self.receiver {
                handles.push(WasapiHandle::Receiver(receiver.registration().win_handle));
            }

            match handles.select() {
                Err(e) => {
                    unimplemented!("Failed to select WASAPI handle: {:?}", e)
                }
                Ok(WasapiHandle::MicStream(_)) => {
                    let Some(stream) = &mut self.connection.mic_stream else {
                        continue;
                    };

                    match stream.invoke(&mut self.connection.mic_sink) {
                        Ok(_) => (),
                        Err(StreamError::BackendSpecific(e)) => {
                            unimplemented!("Mic error: {:?}", e)
                        }
                        Err(StreamError::DeviceNotAvailable) => {
                            eprintln!("Mic device was disconnected");
                            self.connection.mic_stream = None;
                        }
                    }
                }
                Ok(WasapiHandle::PlaybackStream(_)) => {
                    let Some(stream) = &mut self.connection.playback_stream else {
                        continue;
                    };

                    match stream.invoke(&mut self.connection.playback_source) {
                        Ok(_) => (),
                        Err(StreamError::BackendSpecific(e)) => {
                            unimplemented!("Playback error: {:?}", e)
                        }
                        Err(StreamError::DeviceNotAvailable) => {
                            eprintln!("Playback device was disconnected");
                            self.connection.playback_stream = None;
                        }
                    }
                }
                Ok(WasapiHandle::DefaultDeviceChange(_)) => {
                    if let Some((true, new_device)) = InputDevice::default().map(|new_device| {
                        if let Some(current_device) = self.connection.mic_stream.as_ref() {
                            (current_device.device_id != new_device.id(), new_device)
                        } else {
                            (true, new_device)
                        }
                    }) {
                        match new_device.into_stream() {
                            Ok(s) => {
                                s.raw_stream.start();
                                self.connection.mic_stream = Some(s);
                            }
                            Err(e) => eprintln!("Could not convert new mic into stream: {:?}", e),
                        }
                    }

                    if let Some((true, new_device, had_old_device)) =
                        OutputDevice::default().map(|new_device| {
                            if let Some(current_device) = self.connection.playback_stream.as_ref() {
                                (
                                    current_device.device_id != new_device.id(),
                                    new_device,
                                    true,
                                )
                            } else {
                                (true, new_device, false)
                            }
                        })
                    {
                        match new_device.into_stream() {
                            Ok(s) => {
                                if !had_old_device {
                                    self.connection.playback_source.with(|p| {
                                        p.drop_samples();
                                    })
                                }
                                s.raw_stream.start();
                                self.connection.playback_stream = Some(s);
                            }
                            Err(e) => eprintln!(
                                "Could not convert new playback device into stream: {:?}",
                                e
                            ),
                        }
                    }
                }
                Ok(WasapiHandle::Receiver(_)) => {
                    if let Some(receiver) = &mut self.receiver {
                        receiver.invoke(&self.connection);
                    }
                }
            };
        }
    }
}

fn wait_for_handle_signal(handles: &[HANDLE]) -> Result<usize, String> {
    debug_assert!(handles.len() <= SystemServices::MAXIMUM_WAIT_OBJECTS as usize);
    let result = unsafe { Threading::WaitForMultipleObjectsEx(handles, false, u32::MAX, false) };
    if result == Foundation::WAIT_FAILED {
        let err = unsafe { Foundation::GetLastError() };
        let description = format!("WaitForMultipleObjectsEx failed: {err:?}");
        return Err(description);
    }

    let handle_idx = (result.0 - Foundation::WAIT_OBJECT_0.0) as usize;
    Ok(handle_idx)
}
