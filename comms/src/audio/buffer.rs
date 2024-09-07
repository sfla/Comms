use crate::network::ipc::{NetworkCommand, NetworkHandle, Protocol};
use crate::ui::ipc::UICommand;
use audio_device::buffer::{MaybeSharedPlaybackSource, MicSink, PlaybackSource};
use audio_device::noise_gate::{ChangedState, GateNoiseFilter};
use audiopus::coder::{Decoder, Encoder};
use audiopus::{Application, Channels};
use comms_lib::packet::{AudioIndex, ClientId, P2pEstablished};
use comms_lib::serialization::OutgoingPacketFrame;
use comms_lib::{OpusAudioFrame, RawAudioFrame};
use dasp::Sample;
use lewton::inside_ogg::OggStreamReader;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Seek};
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

const BUFFER_SIZE: usize = 4800;

struct ClientQueue {
    decoder: Decoder,
    sample_buffer_offset: usize,
    current_volume: f32,
    current_index: AudioIndex,
    is_peer_to_peer: bool,
}

impl ClientQueue {
    fn new(current_volume: f32) -> Result<Self, audiopus::Error> {
        Ok(Self {
            decoder: Decoder::new(audiopus::SampleRate::Hz48000, Channels::Mono)?,
            sample_buffer_offset: 0,
            current_volume,
            current_index: AudioIndex::new(1).unwrap(),
            is_peer_to_peer: false,
        })
    }
}

pub struct Multiplexer {
    client_queues: HashMap<ClientId, ClientQueue>,
    sound_effect_queues: Vec<Vec<i16>>,
    main_volume: f32,
    // The capacity chosen here is a spit-ball number. The host usually asks for 400 or
    // 800 samples at a time, so it's unlikely to back up this much unless a burst of
    // audio arrives.
    sample_buffer: Box<[f32; BUFFER_SIZE]>,
    sample_buffer_start: usize,
}

impl Multiplexer {
    pub fn new() -> Self {
        Multiplexer {
            client_queues: HashMap::new(),
            sound_effect_queues: Vec::new(),
            main_volume: 1.0,
            sample_buffer: Box::new([Sample::EQUILIBRIUM; BUFFER_SIZE]),
            sample_buffer_start: 0,
        }
    }

    pub fn new_client(&mut self, id: ClientId, volume: f32) -> Result<(), audiopus::Error> {
        self.client_queues.insert(id, ClientQueue::new(volume)?);
        Ok(())
    }

    pub fn drop_client(&mut self, id: ClientId) {
        self.client_queues.remove(&id);
    }

    pub fn append_chunk_from_client(
        &mut self,
        id: ClientId,
        data: OpusAudioFrame,
        is_peer_to_peer: bool,
        network_handle: &NetworkHandle,
        ui_handle: &mut async_channel::Sender<UICommand>,
    ) -> Result<(), audiopus::Error> {
        let queue = match self.client_queues.get_mut(&id) {
            Some(queue) => queue,
            None => return Ok(()),
        };

        if data.index < queue.current_index {
            // This audio data is older than data we've already received, so throw it out
            return Ok(());
        }
        queue.current_index = data.index;

        // If we've received data faster than we can play it (e.g. due to a sudden burst
        // of packets), then throw it away without processing it through opus.
        // (By avoiding processing it through opus, we allow opus to "bridge the wave" for the
        // next packet that _can_ fit).
        if queue.sample_buffer_offset + 120 < BUFFER_SIZE {
            let number_of_samples = decode_opus_frame(
                Some(&data.bytes),
                queue,
                self.sample_buffer_start,
                &mut self.sample_buffer,
                self.main_volume,
            );
            ui_handle
                .try_send(UICommand::UserIsSpeaking {
                    client_id: id,
                    duration: Duration::from_millis(
                        (number_of_samples as f64 / 12.0).round() as u64
                    ),
                })
                .unwrap();
        }

        if !queue.is_peer_to_peer && is_peer_to_peer {
            queue.is_peer_to_peer = true;

            network_handle.command(NetworkCommand::Packet {
                protocol: Protocol::Udp,
                packet: OutgoingPacketFrame::from_packet(
                    None,
                    &P2pEstablished {
                        source_client_id: id,
                    },
                )
                .unwrap(),
                send_response: None,
            })
        }

        Ok(())
    }

    pub fn clear(&mut self) {
        self.client_queues.clear();
    }

    pub fn change_main_volume(&mut self, volume: f32) {
        self.main_volume = volume;
    }

    pub fn change_client_volume(&mut self, id: ClientId, volume: f32) {
        if let Some(client) = self.client_queues.get_mut(&id) {
            client.current_volume = volume;
        }
    }

    pub fn play_sound_effect<T: Read + Seek>(&mut self, mut effect: OggStreamReader<T>) {
        let mut sample_buffer = Vec::new();
        loop {
            match effect.read_dec_packet().unwrap() {
                None => break,
                Some(mut samples) => {
                    // Ignore other streams for now for simplicity
                    if let Some(samples) = samples.get_mut(0) {
                        sample_buffer.append(samples);
                    }
                }
            }
        }
        self.sound_effect_queues.push(sample_buffer);
    }
}

impl PlaybackSource for Multiplexer {
    fn next_samples(&mut self, count: usize, callback: impl FnOnce((&[f32], Option<&[f32]>))) {
        let end_index = (self.sample_buffer_start + count) % BUFFER_SIZE;

        for queue in self.client_queues.values_mut() {
            if queue.sample_buffer_offset < count {
                decode_opus_frame(
                    None,
                    queue,
                    self.sample_buffer_start,
                    &mut self.sample_buffer,
                    self.main_volume,
                );
            }
            queue.sample_buffer_offset = queue.sample_buffer_offset.saturating_sub(count);
        }

        for sound_effect in self.sound_effect_queues.iter_mut() {
            for (i, sample) in sound_effect
                .drain(..count.min(sound_effect.len()))
                .enumerate()
            {
                let sample_buffer_index = (self.sample_buffer_start + i) % BUFFER_SIZE;
                let existing_sample = self.sample_buffer[sample_buffer_index];
                let sample = existing_sample.add_amp(sample.to_sample::<f32>() * self.main_volume);
                self.sample_buffer[sample_buffer_index] = if sample == f32::INFINITY {
                    f32::MAX
                } else {
                    sample
                };
            }
        }
        self.sound_effect_queues.retain(|effect| !effect.is_empty());

        if end_index >= self.sample_buffer_start {
            let windows = (
                &self.sample_buffer[self.sample_buffer_start..end_index],
                None,
            );
            callback(windows);
            for i in self.sample_buffer_start..end_index {
                self.sample_buffer[i] = Sample::EQUILIBRIUM;
            }
        } else {
            let windows = (
                &self.sample_buffer[self.sample_buffer_start..],
                Some(&self.sample_buffer[..end_index]),
            );
            callback(windows);
            for i in self.sample_buffer_start..BUFFER_SIZE {
                self.sample_buffer[i] = Sample::EQUILIBRIUM;
            }
            for i in 0..end_index {
                self.sample_buffer[i] = Sample::EQUILIBRIUM;
            }
        }
        self.sample_buffer_start = end_index;
    }

    fn drop_samples(&mut self) {
        for queue in self.client_queues.values_mut() {
            queue.sample_buffer_offset = 0;
        }
        self.sample_buffer.iter_mut().for_each(|x| *x = 0f32);
    }
}

#[derive(Clone)]
pub struct RcMultiplexer(Rc<RefCell<Multiplexer>>);

impl RcMultiplexer {
    pub fn new() -> Self {
        Self(Rc::new(RefCell::new(Multiplexer::new())))
    }
}

impl MaybeSharedPlaybackSource<Multiplexer> for RcMultiplexer {
    fn with<T>(&mut self, cb: impl FnOnce(&mut Multiplexer) -> T) -> T {
        let mut multiplexer = self.0.borrow_mut();
        cb(&mut multiplexer)
    }
}

pub enum MicCommand {
    RecordRawUntil(Instant),
    PublishCurrentState,
}

pub struct MicBuffer {
    network_handle: NetworkHandle,
    ui_handle: async_channel::Sender<UICommand>,
    sample_buffer: [f32; BUFFER_SIZE],
    sample_buffer_start: usize,
    sample_buffer_end: usize,
    encoder: Encoder,
    noise_gate: GateNoiseFilter,
    index: u64,
    mic_command_rx: Receiver<MicCommand>,
    record_raw_until: Option<Instant>,
    is_speaking: bool,
}

impl MicBuffer {
    pub fn new(
        network_handle: NetworkHandle,
        ui_handle: async_channel::Sender<UICommand>,
        mic_command_rx: Receiver<MicCommand>,
    ) -> Result<Self, audiopus::Error> {
        Ok(Self {
            network_handle,
            ui_handle,
            sample_buffer: [0f32; BUFFER_SIZE],
            sample_buffer_start: 0,
            sample_buffer_end: 0,
            encoder: Encoder::new(
                audiopus::SampleRate::Hz48000,
                Channels::Mono,
                Application::LowDelay,
            )?,
            noise_gate: GateNoiseFilter::new(2048),
            index: 1,
            record_raw_until: None,
            mic_command_rx,
            is_speaking: false,
        })
    }
}

impl MicSink for MicBuffer {
    fn append_sample_from_mic(&mut self, sample: f32) {
        self.sample_buffer[self.sample_buffer_end] = sample;
        self.sample_buffer_end = (self.sample_buffer_end + 1) % BUFFER_SIZE;
    }

    fn publish_chunks(&mut self) {
        while self.sample_buffer_end < self.sample_buffer_start
            || self.sample_buffer_end - self.sample_buffer_start >= 120
        {
            let next_samples =
                &mut self.sample_buffer[self.sample_buffer_start..self.sample_buffer_start + 120];
            let state_change = self.noise_gate.update_window(next_samples);
            match state_change {
                ChangedState::OPEN => {
                    self.is_speaking = true;
                    self.ui_handle
                        .try_send(UICommand::CurrentUserStartedSpeaking)
                        .unwrap()
                }
                ChangedState::CLOSED => {
                    self.is_speaking = false;
                    self.ui_handle
                        .try_send(UICommand::CurrentUserStoppedSpeaking)
                        .unwrap()
                }
                _ => (),
            };

            if let Ok(command) = self.mic_command_rx.try_recv() {
                match command {
                    MicCommand::RecordRawUntil(until) => self.record_raw_until = Some(until),
                    MicCommand::PublishCurrentState => {
                        if self.is_speaking {
                            self.ui_handle
                                .try_send(UICommand::CurrentUserStartedSpeaking)
                                .unwrap();
                        }
                    }
                }
            }

            if let Some(until) = self.record_raw_until.as_mut() {
                if &Instant::now() < until {
                    self.network_handle
                        .raw_audio(RawAudioFrame {
                            samples: next_samples.to_vec(),
                        })
                        .unwrap();
                }
            }

            for sample in next_samples.iter_mut() {
                *sample = self.noise_gate.process(*sample);
            }

            if next_samples.iter().any(|s| *s != f32::EQUILIBRIUM) {
                let mut encode_buffer = [0u8; 120];
                let size = self
                    .encoder
                    .encode_float(next_samples, &mut encode_buffer)
                    .expect("Unsure how to handle opus encoding failure yet");

                let index = AudioIndex::new(self.index).unwrap();
                self.network_handle
                    .audio(OpusAudioFrame {
                        index,
                        bytes: encode_buffer[..size].to_vec(),
                    })
                    .unwrap();
                self.index += 1;
            }

            self.sample_buffer_start = (self.sample_buffer_start + 120) % BUFFER_SIZE;
        }
    }
}

fn decode_opus_frame(
    opus_bytes: Option<&Vec<u8>>,
    queue: &mut ClientQueue,
    sample_buffer_start: usize,
    sample_buffer: &mut Box<[f32; BUFFER_SIZE]>,
    main_volume: f32,
) -> usize {
    let mut decode_buffer = [0f32; 120];
    let number_of_decoded_samples = queue
        .decoder
        .decode_float(opus_bytes, &mut decode_buffer as &mut [f32], false)
        .expect("Failed to produce generated opus audio");

    for (i, new_sample) in decode_buffer[..number_of_decoded_samples]
        .iter()
        .enumerate()
    {
        let sample_buffer_index =
            (sample_buffer_start + queue.sample_buffer_offset + i) % BUFFER_SIZE;
        let existing_sample = sample_buffer[sample_buffer_index];
        let sample = existing_sample.add_amp(*new_sample * main_volume * queue.current_volume);
        sample_buffer[sample_buffer_index] = if sample == f32::INFINITY {
            f32::MAX
        } else {
            sample
        };
    }

    queue.sample_buffer_offset += number_of_decoded_samples;
    debug_assert!(queue.sample_buffer_offset <= BUFFER_SIZE);

    number_of_decoded_samples
}
