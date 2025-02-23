use crate::ui::{Command, IncomingAudioPlotData};
use audio_device::audio_connection::{EventLoop, create_minimal_event_loop};
use audio_device::buffer::{MicSink, PlaybackSource, UnsharedPlaybackSource};
use audio_device::noise_gate::{GateNoiseFilter, State};
use dasp::Sample;
use std::fs::File;
use std::io::{Read, Write};
use std::iter;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

pub const BUFFER_SIZE: usize = 4800;
pub const SAMPLES_IN_CHUNK: usize = 480;
pub const NOISE_GATE_CHUNK_LEN: usize = 128;

pub struct MicForLoopback {
    playback_sender: Sender<Vec<f32>>,
    pcm_samples: [f32; BUFFER_SIZE],
    pcm_samples_start: usize,
    pcm_samples_end: usize,
}

impl MicSink for MicForLoopback {
    fn append_sample_from_mic(&mut self, sample: f32) {
        self.pcm_samples[self.pcm_samples_end] = sample;
        self.pcm_samples_end = (self.pcm_samples_end + 1) % BUFFER_SIZE;
    }

    fn publish_chunks(&mut self) {
        while self.pcm_samples_end < self.pcm_samples_start
            || self.pcm_samples_end - self.pcm_samples_start >= SAMPLES_IN_CHUNK
        {
            let next_samples = &self.pcm_samples
                [self.pcm_samples_start..self.pcm_samples_start + SAMPLES_IN_CHUNK];
            self.playback_sender.send(next_samples.to_vec()).unwrap();
            self.pcm_samples_start = (self.pcm_samples_start + 480) % BUFFER_SIZE;
        }
    }
}

pub struct PlaybackForLoopback {
    receiver: Receiver<Vec<f32>>,
    command_rx: Receiver<Command>,
    audio_plot_sender: Sender<IncomingAudioPlotData>,
    gate: GateNoiseFilter,
    pcm_samples: [f32; BUFFER_SIZE],
    pcm_samples_start: usize,
    pcm_samples_end: usize,
    is_muted: bool,
    recording_file_buffer: Option<Vec<f32>>,
    playback_file_buffer: Option<Vec<f32>>,
}

impl PlaybackSource for PlaybackForLoopback {
    fn next_samples(&mut self, count: usize, callback: impl FnOnce((&[f32], Option<&[f32]>))) {
        for command in self.command_rx.try_iter() {
            match command {
                Command::ToggleMute => {
                    self.is_muted = !self.is_muted;
                }
                Command::StartRecording => {
                    self.recording_file_buffer = Some(Vec::new());
                }
                Command::StopRecording(path) => {
                    if let Some(buffer) = self.recording_file_buffer.take() {
                        let buffer: &[f32] = &buffer;
                        let bytes = unsafe {
                            std::slice::from_raw_parts(
                                buffer.as_ptr() as *const u8,
                                buffer.len() * 4,
                            )
                        };
                        let mut file = File::create(path).unwrap();
                        file.write_all(bytes).unwrap();
                    }
                }
                Command::PlayRecording(path) => {
                    if let Ok(mut file) = File::open(path) {
                        let mut bytes: Vec<u8> = Vec::new();
                        file.read_to_end(&mut bytes).unwrap();
                        let bytes: &[u8] = &bytes;
                        assert_eq!(bytes.len() % 4, 0);
                        assert_eq!(bytes.as_ptr() as usize % std::mem::align_of::<f32>(), 0);
                        let buffer: &[f32] = unsafe {
                            std::slice::from_raw_parts(
                                bytes.as_ptr() as *const f32,
                                bytes.len() / 4,
                            )
                        };
                        self.playback_file_buffer = Some(Vec::from(buffer));
                        self.gate.reset();
                    }
                }
                Command::PlayLoopback => {
                    self.playback_file_buffer = None;
                    self.gate.reset();
                }
            }
        }

        let mut samples = Vec::with_capacity(count);
        for _ in 0..count {
            let sample = if self.pcm_samples_end != self.pcm_samples_start {
                let sample = self.pcm_samples[self.pcm_samples_start];
                self.pcm_samples_start = (self.pcm_samples_start + 1) % BUFFER_SIZE;
                sample
            } else {
                // Always attempt to pull from receiver to keep it cleared.
                let samples = match self.receiver.try_recv() {
                    Ok(pcm_samples) => pcm_samples,
                    Err(TryRecvError::Empty) => {
                        samples.push(Sample::EQUILIBRIUM);
                        return;
                    }
                    Err(TryRecvError::Disconnected) => unimplemented!("channel failed"),
                };
                let samples = if let Some(buffer) = &mut self.playback_file_buffer {
                    buffer
                        .drain(..SAMPLES_IN_CHUNK.min(buffer.len()))
                        .chain(iter::repeat(Sample::EQUILIBRIUM))
                        .take(SAMPLES_IN_CHUNK)
                        .collect()
                } else {
                    samples
                };

                self.audio_plot_sender
                    .send(IncomingAudioPlotData::SampleChunk(samples.clone()))
                    .unwrap();

                let mut accumulator = 0f32;
                for sample in &samples {
                    accumulator += sample * sample;
                }

                let rms = (accumulator / samples.len() as f32).sqrt();
                let max = samples
                    .iter()
                    .map(|s| s.abs())
                    .max_by(|x, y| x.partial_cmp(y).unwrap())
                    .unwrap();

                self.gate.update_window(&samples);
                self.audio_plot_sender
                    .send(IncomingAudioPlotData::ChunkMax(max))
                    .unwrap();
                self.audio_plot_sender
                    .send(IncomingAudioPlotData::ChunkRms(rms))
                    .unwrap();
                self.audio_plot_sender
                    .send(IncomingAudioPlotData::StateChange(match self.gate.state {
                        State::Off => 0,
                        State::Attack(_) => 1,
                        State::Active => 2,
                        State::Hold(_) => 3,
                        State::Release(_) => 4,
                    }))
                    .unwrap();
                self.audio_plot_sender
                    .send(IncomingAudioPlotData::NoiseGateThreshold(
                        self.gate.threshold,
                    ))
                    .unwrap();

                for sample in &samples[1..] {
                    self.pcm_samples[self.pcm_samples_end] = *sample;
                    self.pcm_samples_end = (self.pcm_samples_end + 1) % BUFFER_SIZE;
                }

                samples[0]
            };

            if let Some(buffer) = &mut self.recording_file_buffer {
                buffer.push(sample);
            }

            let sample = self.gate.process(sample.to_sample()).to_sample();
            samples.push(if self.is_muted { 0.0 } else { sample });
        }

        callback((&samples, None));
    }

    fn drop_samples(&mut self) {
        self.receiver.try_iter().last();
    }
}

pub fn create_pipeline(
    command_rx: Receiver<Command>,
    audio_plot_sender: Sender<IncomingAudioPlotData>,
) -> (MicForLoopback, UnsharedPlaybackSource<PlaybackForLoopback>) {
    let (tx, rx) = channel();
    (
        MicForLoopback {
            playback_sender: tx,
            pcm_samples: [0f32; BUFFER_SIZE],
            pcm_samples_start: 0,
            pcm_samples_end: 0,
        },
        UnsharedPlaybackSource(PlaybackForLoopback {
            receiver: rx,
            command_rx,
            audio_plot_sender,
            pcm_samples: [0f32; BUFFER_SIZE],
            pcm_samples_start: 0,
            pcm_samples_end: 0,
            gate: GateNoiseFilter::new(NOISE_GATE_CHUNK_LEN),
            is_muted: false,
            recording_file_buffer: None,
            playback_file_buffer: None,
        }),
    )
}

pub fn run(command_rx: Receiver<Command>, audio_plot_sender: Sender<IncomingAudioPlotData>) {
    let (mic, playback) = create_pipeline(command_rx, audio_plot_sender);
    let event_loop = create_minimal_event_loop(mic, playback);
    event_loop.run();
}
