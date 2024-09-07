use std::sync::mpsc::{Receiver, SendError, Sender};

use crate::audio::buffer::{MicCommand, RcMultiplexer};
use crate::audio::{AudioPlatform, AudioPlatformImpl};
use crate::ui::ipc::UICommand;
use crate::{NetworkHandle, RESOURCES_DIR};
use audio_device::audio_connection::{AudioConnection, EventLoopNotify};
use audio_device::buffer::{InterruptReceiver, MaybeSharedPlaybackSource};
use comms_lib::packet::{AudioIndex, ClientId};
use comms_lib::OpusAudioFrame;
use lewton::inside_ogg::OggStreamReader;
use std::fmt;
use std::fmt::{Display, Formatter};
use std::io::Cursor;
use std::time::{Duration, Instant};

#[cfg_attr(unix, allow(dead_code))]
#[derive(Debug)]
pub enum EventLoopCreationError {
    InputDeviceError(String),
    OutputDeviceError(String),
}

pub struct AudioUser {
    pub id: ClientId,
    pub volume: u16,
}

pub enum AudioCommand {
    NewClient(ClientId, u16),
    DropClient(ClientId),
    ServerDisconnect,
    AudioFromClient(ClientId, OpusAudioFrame, bool),
    ListDevices,
    ChangeMainVolume(u16),
    ChangeClientVolume(ClientId, u16),
    UserJoined,
    UserLeft,
    ServerConnect(Vec<AudioUser>),
    RecordRawOneMinute,
}

impl Display for AudioCommand {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), fmt::Error> {
        match *self {
            AudioCommand::NewClient(_, _) => f.write_str("NewClient"),
            AudioCommand::DropClient(_) => f.write_str("DropClient"),
            AudioCommand::ServerDisconnect => f.write_str("ServerDisconnect"),
            AudioCommand::AudioFromClient(..) => f.write_str("AudioFromClient"),
            AudioCommand::ListDevices => f.write_str("ListDevices"),
            AudioCommand::ChangeMainVolume(..) => f.write_str("ChangeMainVolume"),
            AudioCommand::ChangeClientVolume(..) => f.write_str("ChangeClientVolume"),
            AudioCommand::UserJoined => f.write_str("UserJoined"),
            AudioCommand::UserLeft => f.write_str("UserLeft"),
            AudioCommand::ServerConnect { .. } => f.write_str("ServerConnect"),
            AudioCommand::RecordRawOneMinute => f.write_str("RecordRawOneMinute"),
        }
    }
}

#[derive(Clone)]
pub struct AudioHandle {
    pub command_tx: Sender<AudioCommand>,
    pub notify: <AudioPlatformImpl as AudioPlatform>::Notify,
}

impl AudioHandle {
    pub fn send_audio(
        &mut self,
        index: AudioIndex,
        id: ClientId,
        bytes: Vec<u8>,
        is_peer_to_peer: bool,
    ) {
        self.send(AudioCommand::AudioFromClient(
            id,
            OpusAudioFrame { index, bytes },
            is_peer_to_peer,
        ))
        .unwrap();
    }

    pub fn send(&self, command: AudioCommand) -> Result<(), SendError<AudioCommand>> {
        let result = self.command_tx.send(command);
        self.notify.set_ready();
        result
    }
}

pub struct AudioReceiver<R> {
    pub(crate) multiplexer: RcMultiplexer,
    pub(crate) registration: R,
    pub(crate) command_rx: Receiver<AudioCommand>,
    pub(crate) ui_handle: async_channel::Sender<UICommand>,
    pub(crate) network_handle: NetworkHandle,
    pub(crate) mic_command_tx: Sender<MicCommand>,
}

impl<R> AudioReceiver<R> {
    pub fn new(
        multiplexer: RcMultiplexer,
        registration: R,
        command_rx: Receiver<AudioCommand>,
        ui_handle: async_channel::Sender<UICommand>,
        network_handle: NetworkHandle,
        mic_command_tx: Sender<MicCommand>,
    ) -> Self {
        Self {
            multiplexer,
            registration,
            command_rx,
            ui_handle,
            network_handle,
            mic_command_tx,
        }
    }
}

impl<R> InterruptReceiver<R> for AudioReceiver<R> {
    fn registration(&self) -> &R {
        &self.registration
    }

    fn invoke<AC: AudioConnection>(&mut self, connection: &AC) {
        for command in self.command_rx.try_iter() {
            let command: AudioCommand = command;
            match command {
                AudioCommand::NewClient(id, volume) => self.multiplexer.with(|multiplexer| {
                    multiplexer
                        .new_client(id, volume as f32 / 100.0)
                        .expect("Failed to create new multiplexer client")
                }),
                AudioCommand::DropClient(id) => self.multiplexer.with(|multiplexer| {
                    multiplexer.drop_client(id);
                }),
                AudioCommand::ServerDisconnect => {
                    let file =
                        Cursor::new(RESOURCES_DIR.get_file("self-left.ogg").unwrap().contents());
                    let ogg_reader = OggStreamReader::new(file).unwrap();
                    self.multiplexer.with(|multiplexer| {
                        multiplexer.clear();
                        multiplexer.play_sound_effect(ogg_reader);
                    })
                }
                AudioCommand::AudioFromClient(client_id, chunk, is_peer_to_peer) => {
                    self.multiplexer.with(|multiplexer| {
                        multiplexer
                            .append_chunk_from_client(
                                client_id,
                                chunk,
                                is_peer_to_peer,
                                &self.network_handle,
                                &mut self.ui_handle,
                            )
                            .expect("Failed to decode audio from multiplexer client")
                    })
                }
                AudioCommand::ListDevices => {
                    let ui_handle = self.ui_handle.clone();
                    connection.list_devices(move |devices| {
                        ui_handle
                            .try_send(UICommand::DevicesFound {
                                _input_devices: devices.input_devices,
                                _output_devices: devices.output_devices,
                            })
                            .unwrap();
                    });
                }
                AudioCommand::ChangeMainVolume(volume) => self.multiplexer.with(|multiplexer| {
                    multiplexer.change_main_volume(volume as f32 / 100.0);
                }),
                AudioCommand::ChangeClientVolume(id, volume) => {
                    self.multiplexer.with(|multiplexer| {
                        multiplexer.change_client_volume(id, volume as f32 / 100.0);
                    });
                }
                AudioCommand::ServerConnect(users) => {
                    let file = Cursor::new(
                        RESOURCES_DIR
                            .get_file("self-joined.ogg")
                            .unwrap()
                            .contents(),
                    );
                    let ogg_reader = OggStreamReader::new(file).unwrap();
                    self.multiplexer.with(|multiplexer| {
                        multiplexer.play_sound_effect(ogg_reader);
                        for u in users {
                            multiplexer
                                .new_client(u.id, u.volume as f32 / 100.0)
                                .unwrap();
                        }
                    });
                    self.mic_command_tx
                        .send(MicCommand::PublishCurrentState)
                        .unwrap();
                }
                AudioCommand::UserJoined => {
                    let file = Cursor::new(
                        RESOURCES_DIR
                            .get_file("other-joined.ogg")
                            .unwrap()
                            .contents(),
                    );
                    let ogg_reader = OggStreamReader::new(file).unwrap();
                    self.multiplexer
                        .with(|multiplexer| multiplexer.play_sound_effect(ogg_reader))
                }
                AudioCommand::UserLeft => {
                    let file =
                        Cursor::new(RESOURCES_DIR.get_file("other-left.ogg").unwrap().contents());
                    let ogg_reader = OggStreamReader::new(file).unwrap();
                    self.multiplexer
                        .with(|multiplexer| multiplexer.play_sound_effect(ogg_reader))
                }
                AudioCommand::RecordRawOneMinute => self
                    .mic_command_tx
                    .send(MicCommand::RecordRawUntil(
                        Instant::now() + Duration::from_secs(60),
                    ))
                    .unwrap(),
            }
        }
    }
}
