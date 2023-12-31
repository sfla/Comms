use audio_device::audio_connection::{create_minimal_event_loop, EventLoop};
use audio_device::buffer::{MicSink, PlaybackSource, UnsharedPlaybackSource};
use audiopus::coder::{Decoder, Encoder};
use audiopus::{Application, Channels, SampleRate};
use dasp::Sample;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};

const BUFFER_SIZE: usize = 4800;

pub struct Mic {
    sender: Sender<Vec<u8>>,
    pcm_samples: [f32; BUFFER_SIZE],
    pcm_samples_start: usize,
    pcm_samples_end: usize,
    encoder: Encoder,
}

impl MicSink for Mic {
    fn append_sample_from_mic(&mut self, sample: f32) {
        self.pcm_samples[self.pcm_samples_end] = sample;
        self.pcm_samples_end = (self.pcm_samples_end + 1) % BUFFER_SIZE;
    }

    fn publish_chunks(&mut self) {
        while self.pcm_samples_end < self.pcm_samples_start
            || self.pcm_samples_end - self.pcm_samples_start >= 120
        {
            let pcm_buffer =
                &self.pcm_samples[self.pcm_samples_start..self.pcm_samples_start + 120];
            let mut opus_buffer = [0u8; 2048];
            let size = self
                .encoder
                .encode_float(pcm_buffer, &mut opus_buffer)
                .unwrap();
            let opus_bytes = opus_buffer[..size].to_vec();
            self.sender.send(opus_bytes).unwrap();
            self.pcm_samples_start = (self.pcm_samples_start + 120) % BUFFER_SIZE;
        }
    }
}

pub struct Playback {
    receiver: Receiver<Vec<u8>>,
    pcm_samples: [f32; BUFFER_SIZE],
    pcm_samples_start: usize,
    pcm_samples_end: usize,
    decoder: Decoder,
}

impl PlaybackSource for Playback {
    fn next_samples(&mut self, count: usize, callback: impl FnOnce((&[f32], Option<&[f32]>))) {
        let mut samples = Vec::with_capacity(count);
        for _ in 0..count {
            let sample = if self.pcm_samples_end != self.pcm_samples_start {
                let sample = self.pcm_samples[self.pcm_samples_start];
                self.pcm_samples_start = (self.pcm_samples_start + 1) % BUFFER_SIZE;
                sample
            } else {
                match self.receiver.try_recv() {
                    Ok(opus_bytes) => {
                        let mut decode_buffer = [0f32; 120];
                        let size = self
                            .decoder
                            .decode_float(
                                Some(&opus_bytes),
                                &mut decode_buffer as &mut [f32],
                                false,
                            )
                            .unwrap();

                        for sample in &decode_buffer[1..size] {
                            self.pcm_samples[self.pcm_samples_end] = *sample;
                            self.pcm_samples_end = (self.pcm_samples_end + 1) % BUFFER_SIZE;
                        }

                        decode_buffer[0]
                    }
                    Err(TryRecvError::Empty) => Sample::EQUILIBRIUM,
                    Err(TryRecvError::Disconnected) => unimplemented!("channel failed"),
                }
            };
            samples.push(sample);
        }
        callback((&samples, None));
    }

    fn drop_samples(&mut self) {}
}

pub fn create_pipeline() -> (Mic, UnsharedPlaybackSource<Playback>) {
    let (tx, rx) = channel();
    (
        Mic {
            sender: tx,
            pcm_samples: [0f32; BUFFER_SIZE],
            pcm_samples_start: 0,
            pcm_samples_end: 0,
            encoder: Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::LowDelay)
                .unwrap(),
        },
        UnsharedPlaybackSource(Playback {
            receiver: rx,
            pcm_samples: [0f32; BUFFER_SIZE],
            pcm_samples_start: 0,
            pcm_samples_end: 0,
            decoder: Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap(),
        }),
    )
}

fn main() {
    let (mic, playback) = create_pipeline();
    let event_loop = create_minimal_event_loop(mic, playback);
    event_loop.run();
}
