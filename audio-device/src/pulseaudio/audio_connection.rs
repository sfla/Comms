use crate::audio_connection::{AudioConnection, DeviceInfo, FoundDevices};
use crate::buffer::{MaybeSharedPlaybackSource, MicSink, PlaybackSource};
use crate::pulseaudio::device::{play, record};
use crate::pulseaudio::event_loop::PulseEventLoopBuilder;
use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context;
use libpulse_binding::context::Context;
use libpulse_binding::mainloop::standard::{IterateResult, Mainloop};
use std::cell::RefCell;
use std::error::Error;
use std::fmt;
use std::fmt::Formatter;
use std::rc::Rc;

struct DeviceQueryStatus {
    finished: bool,
    devices: Vec<DeviceInfo>,
}

impl DeviceQueryStatus {
    pub fn new() -> Self {
        Self {
            finished: false,
            devices: vec![],
        }
    }
}

pub struct PulseParameters {
    pub pulseaudio_server: Option<String>,
}

pub struct PulseAudioConnection {
    pub context: Context,
}

impl PulseAudioConnection {
    pub fn create<P: PlaybackSource>(
        parameters: PulseParameters,
        mic: impl MicSink + 'static,
        playback: impl MaybeSharedPlaybackSource<P> + 'static,
    ) -> PulseEventLoopBuilder {
        let mut mainloop = Mainloop::new().unwrap();
        let mut context = Context::new(&mainloop, "comms").unwrap();
        context
            .connect(
                parameters.pulseaudio_server.as_deref(),
                context::FlagSet::NOAUTOSPAWN,
                None,
            )
            .unwrap();
        context.wait(&mut mainloop).unwrap();

        record(&mut context, mic);
        play(&mut context, playback);
        PulseEventLoopBuilder::new(mainloop, Self { context })
    }
}

impl AudioConnection for PulseAudioConnection {
    fn list_devices(&self, callback: impl FnOnce(FoundDevices) + 'static) {
        let introspector = self.context.introspect();
        let callback = Rc::new(RefCell::new(Option::from(callback)));
        let input_devices = Rc::new(RefCell::new(DeviceQueryStatus::new()));
        let output_devices = Rc::new(RefCell::new(DeviceQueryStatus::new()));

        {
            let callback = callback.clone();
            let input_devices = input_devices.clone();
            let output_devices = output_devices.clone();

            // get all input devices
            introspector.get_source_info_list(move |source_result| match source_result {
                ListResult::Item(source) => {
                    if let Some(name) = &source.name {
                        if let Some(description) = &source.description {
                            let mut input_devices = input_devices.borrow_mut();
                            input_devices.devices.push(DeviceInfo {
                                id: name.clone().into_owned(),
                                label: description.clone().into_owned(),
                            })
                        }
                    }
                }
                ListResult::End => {
                    let mut input_devices = input_devices.borrow_mut();
                    let mut output_devices = output_devices.borrow_mut();

                    input_devices.finished = true;
                    if output_devices.finished {
                        let mut callback = callback.borrow_mut();
                        if let Some(cb) = callback.take() {
                            cb(FoundDevices {
                                input_devices: input_devices.devices.drain(..).collect(),
                                output_devices: output_devices.devices.drain(..).collect(),
                            })
                        };
                    }
                }
                ListResult::Error => panic!("Failed to get audio inputs"),
            });
        }

        introspector.get_sink_info_list(move |sink_result| match sink_result {
            ListResult::Item(sink) => {
                if let Some(name) = &sink.name {
                    if let Some(description) = &sink.description {
                        let mut output_devices = output_devices.borrow_mut();
                        output_devices.devices.push(DeviceInfo {
                            id: name.clone().into_owned(),
                            label: description.clone().into_owned(),
                        })
                    }
                }
            }
            ListResult::End => {
                let mut input_devices = input_devices.borrow_mut();
                let mut output_devices = output_devices.borrow_mut();

                output_devices.finished = true;
                if input_devices.finished {
                    let mut callback = callback.borrow_mut();
                    if let Some(cb) = callback.take() {
                        cb(FoundDevices {
                            input_devices: input_devices.devices.drain(..).collect(),
                            output_devices: output_devices.devices.drain(..).collect(),
                        })
                    };
                }
            }
            ListResult::Error => panic!("Failed to get audio outputs"),
        });
    }
}

#[derive(Debug)]
enum WaitError {
    Failed,
    Cancelled,
}

impl Error for WaitError {}

impl fmt::Display for WaitError {
    fn fmt(&self, f: &mut Formatter) -> Result<(), ::std::fmt::Error> {
        match *self {
            WaitError::Failed => f.write_str("Failed"),
            WaitError::Cancelled => f.write_str("Cancelled"),
        }
    }
}

trait WaitableOperation {
    fn wait(&self, mainloop: &mut Mainloop) -> Result<(), WaitError>;
}

impl WaitableOperation for Context {
    fn wait(&self, mainloop: &mut Mainloop) -> Result<(), WaitError> {
        loop {
            match mainloop.iterate(false) {
                IterateResult::Quit(_) => {
                    return Err(WaitError::Cancelled);
                }
                IterateResult::Err(_) => {
                    return Err(WaitError::Failed);
                }
                IterateResult::Success(_) => {}
            };
            match self.get_state() {
                context::State::Ready => {
                    break;
                }
                context::State::Failed | context::State::Terminated => {
                    return Err(WaitError::Failed);
                }
                _ => {}
            }
        }
        Ok(())
    }
}
