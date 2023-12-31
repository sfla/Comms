use audio_device::audio_connection::{create_minimal_event_loop, EventLoop};
use audio_device::buffer::{MicSink, PlaybackSource, UnsharedPlaybackSource};
use audiopus::coder::{Decoder, Encoder};
use audiopus::{Application, Channels, SampleRate};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::time::Instant;

const BUFFER_SIZE: usize = 4096;

pub struct Mic {
    sender: Sender<Vec<u8>>,
    pcm_samples: [f32; BUFFER_SIZE],
    pcm_samples_start: usize,
    pcm_samples_end: usize,
    encoder: Encoder,
    time_since_press: Option<Instant>,
    reciever_for_time_since_user_input: Receiver<Instant>,
}

impl Mic {
    fn user_has_pressed(&self) -> bool {
        if let Some(instant_since_press) = &self.time_since_press {
            if instant_since_press.elapsed().as_secs() < 3 {
                return true;
            }
        }
        false
    }
}

impl MicSink for Mic {
    fn append_sample_from_mic(&mut self, sample: f32) {
        self.pcm_samples[self.pcm_samples_end] = sample;
        self.pcm_samples_end = (self.pcm_samples_end + 1) % BUFFER_SIZE;
    }

    fn publish_chunks(&mut self) {
        for enter_press in self.reciever_for_time_since_user_input.try_iter() {
            self.time_since_press = Some(enter_press);
        }

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
            if !self.user_has_pressed() {
                self.sender.send(opus_bytes).unwrap();
            }
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
    fn next_sample(&mut self) -> f32 {
        if self.pcm_samples_end != self.pcm_samples_start {
            let sample = self.pcm_samples[self.pcm_samples_start];
            self.pcm_samples_start = (self.pcm_samples_start + 1) % BUFFER_SIZE;
            sample
        } else {
            match self.receiver.try_recv() {
                Ok(opus_bytes) => {
                    let mut decode_buffer = [0f32; 120];
                    let size = self
                        .decoder
                        .decode_float(Some(&opus_bytes), &mut decode_buffer as &mut [f32], false)
                        .unwrap();

                    for sample in &decode_buffer[1..size] {
                        self.pcm_samples[self.pcm_samples_end] = *sample;
                        self.pcm_samples_end = (self.pcm_samples_end + 1) % BUFFER_SIZE;
                    }

                    decode_buffer[0]
                }
                Err(TryRecvError::Empty) => {
                    let mut decode_buffer = [0f32; 120];
                    let size = self
                        .decoder
                        .decode_float(
                            None as Option<&[u8]>,
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
                Err(TryRecvError::Disconnected) => unimplemented!("channel failed"),
            }
        }
    }
}

pub fn create_pipeline(recv: Receiver<Instant>) -> (Mic, UnsharedPlaybackSource<Playback>) {
    let (tx, rx) = channel();
    (
        Mic {
            sender: tx,
            pcm_samples: [0f32; BUFFER_SIZE],
            pcm_samples_start: 0,
            pcm_samples_end: 0,
            encoder: Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::LowDelay)
                .unwrap(),
            time_since_press: None,
            reciever_for_time_since_user_input: recv,
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
    let (send, recv) = channel();

    let (mic, playback) = create_pipeline(recv);
    let event_loop = create_minimal_event_loop(mic, playback);

    std::thread::spawn(move || loop {
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_ok() {
            send.send(Instant::now()).unwrap();
        }
    });

    event_loop.run();
}
