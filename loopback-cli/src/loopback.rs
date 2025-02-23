use audio_device::audio_connection::{EventLoop, create_minimal_event_loop};
use audio_device::buffer::{MicSink, PlaybackSource, UnsharedPlaybackSource};
use dasp::Sample;
use std::sync::mpsc::{Receiver, Sender, channel};

pub struct MicForLoopback {
    sender: Sender<f32>,
}

impl MicSink for MicForLoopback {
    fn append_sample_from_mic(&mut self, sample: f32) {
        self.sender.send(sample).unwrap();
    }

    fn publish_chunks(&mut self) {}
}

pub struct PlaybackForLoopback {
    receiver: Receiver<f32>,
}

impl PlaybackSource for PlaybackForLoopback {
    fn next_samples(&mut self, count: usize, callback: impl FnOnce((&[f32], Option<&[f32]>))) {
        let mut samples = Vec::with_capacity(count);
        for _ in 0..count {
            samples.push(self.receiver.try_recv().unwrap_or(Sample::EQUILIBRIUM));
        }

        callback((&samples, None));
    }

    fn drop_samples(&mut self) {
        self.receiver.try_iter().last();
    }
}

pub fn create_pipeline() -> (MicForLoopback, UnsharedPlaybackSource<PlaybackForLoopback>) {
    let (tx, rx) = channel();
    (
        MicForLoopback { sender: tx },
        UnsharedPlaybackSource(PlaybackForLoopback { receiver: rx }),
    )
}

fn main() {
    let (mic, playback) = create_pipeline();
    let event_loop = create_minimal_event_loop(mic, playback);
    event_loop.run();
}
