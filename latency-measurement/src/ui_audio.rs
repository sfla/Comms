use crate::{AudioData, TIME_PLOT_SAMPLES_PER_SECOND};
use audio_device::audio_connection::EventLoop;
use audio_device::audio_connection::create_minimal_event_loop;
use audio_device::buffer::{MicSink, PlaybackSource, UnsharedPlaybackSource};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

const SAMPLE_RATE: f32 = 48000f32;
const SEND_EVERY_NTH_SAMPLE: u8 = (SAMPLE_RATE / TIME_PLOT_SAMPLES_PER_SECOND as f32) as u8;
const LATENCY_TIMEOUT: Duration = Duration::from_secs(1);

pub enum AudioCommand {
    DoMeasure {
        remaining_measurements: u8,
        noise_detection_duration: Duration,
    },
}

enum State {
    Idle,
    DetectNoiseProfile {
        start_time: Instant,
        noise_detection_duration: Duration,
        remaining_measurements: u8,
        current_max_noise_floor: f32,
    },
    ReadyToMeasure {
        noise_threshold: f32,
        remaining_measurements: u8,
        existing_measurements: Vec<Duration>,
    },
    Measuring {
        start_time: Instant,
        noise_threshold: f32,
        remaining_measurements: u8,
        existing_measurements: Vec<Duration>,
    },
    WaitingForSilence {
        recent_samples: VecDeque<f32>,
        noise_threshold: f32,
        remaining_measurements: u8,
        existing_measurements: Vec<Duration>,
    },
}

struct Mic {
    state: Rc<RefCell<Option<State>>>,
    audio_tx: Sender<AudioData>,
    recent_samples: Vec<f32>,
    time_of_first_sample: Option<Instant>,
    samples_since_last_send: u8,
}

impl MicSink for Mic {
    fn append_sample_from_mic(&mut self, sample: f32) {
        let mut state_ref = self.state.borrow_mut();
        let state = state_ref.take().expect("State was unexpectedly empty");
        *state_ref = Some(match state {
            State::DetectNoiseProfile {
                start_time,
                noise_detection_duration,
                remaining_measurements,
                current_max_noise_floor,
            } => {
                let current_max_noise_floor = if sample.abs() > current_max_noise_floor {
                    sample.abs()
                } else {
                    current_max_noise_floor
                };
                if start_time.elapsed() >= noise_detection_duration {
                    let noise_threshold = current_max_noise_floor + 0.3;
                    self.audio_tx
                        .send(AudioData::NoiseProfileDetected(
                            Instant::now(),
                            noise_threshold,
                        ))
                        .unwrap();
                    State::ReadyToMeasure {
                        noise_threshold,
                        remaining_measurements,
                        existing_measurements: Vec::new(),
                    }
                } else {
                    State::DetectNoiseProfile {
                        start_time,
                        noise_detection_duration,
                        remaining_measurements,
                        current_max_noise_floor,
                    }
                }
            }
            State::Measuring {
                start_time,
                noise_threshold,
                mut remaining_measurements,
                mut existing_measurements,
            } => {
                if sample.abs() > noise_threshold {
                    existing_measurements.push(start_time.elapsed());
                    remaining_measurements -= 1;
                    State::WaitingForSilence {
                        recent_samples: VecDeque::new(),
                        noise_threshold,
                        remaining_measurements,
                        existing_measurements,
                    }
                } else if start_time.elapsed() < LATENCY_TIMEOUT {
                    State::Measuring {
                        start_time,
                        noise_threshold,
                        remaining_measurements,
                        existing_measurements,
                    }
                } else {
                    self.audio_tx
                        .send(AudioData::LatencyTimeout(Instant::now(), noise_threshold))
                        .unwrap();
                    State::Idle
                }
            }
            State::WaitingForSilence {
                mut recent_samples,
                noise_threshold,
                existing_measurements,
                remaining_measurements,
            } => {
                recent_samples.push_back(sample);
                if recent_samples.len() <= 1000 {
                    State::WaitingForSilence {
                        recent_samples,
                        noise_threshold,
                        existing_measurements,
                        remaining_measurements,
                    }
                } else {
                    recent_samples.pop_front();
                    let max = recent_samples.iter().fold(0f32, |acc, next| acc.max(*next));
                    if max < noise_threshold {
                        if remaining_measurements == 0 {
                            let latency = existing_measurements
                                .iter()
                                .fold(Duration::ZERO, |acc, latency| acc + *latency)
                                / existing_measurements.len() as u32;
                            self.audio_tx
                                .send(AudioData::MeasuredLatency(latency))
                                .unwrap();
                            State::Idle
                        } else {
                            State::ReadyToMeasure {
                                noise_threshold,
                                existing_measurements,
                                remaining_measurements,
                            }
                        }
                    } else {
                        State::WaitingForSilence {
                            recent_samples,
                            noise_threshold,
                            existing_measurements,
                            remaining_measurements,
                        }
                    }
                }
            }
            state => state,
        });

        if self.time_of_first_sample.is_none() {
            self.time_of_first_sample = Some(Instant::now());
        }

        if self.samples_since_last_send == SEND_EVERY_NTH_SAMPLE {
            self.samples_since_last_send = 0;
        }

        if self.samples_since_last_send == 0 {
            self.recent_samples.push(sample);
        }
        self.samples_since_last_send += 1;
    }

    fn publish_chunks(&mut self) {
        let time = self
            .time_of_first_sample
            .take()
            .expect("Published chunks before any audio received");
        for sample in self.recent_samples.drain(..) {
            self.audio_tx
                .send(AudioData::IncomingSample(time, sample))
                .unwrap();
        }
        self.time_of_first_sample = None;
    }
}

struct Beep {
    state: Rc<RefCell<Option<State>>>,
    audio_tx: Sender<AudioData>,
    command_rx: Receiver<AudioCommand>,
    sample_clock: f32,
    samples_since_last_send: u8,
    last_send_time: Option<Instant>,
}

impl PlaybackSource for Beep {
    fn next_samples(&mut self, count: usize, callback: impl FnOnce((&[f32], Option<&[f32]>))) {
        for command in self.command_rx.try_iter() {
            match command {
                AudioCommand::DoMeasure {
                    remaining_measurements,
                    noise_detection_duration,
                } => {
                    let mut state_ref = self.state.borrow_mut();
                    *state_ref = Some(State::DetectNoiseProfile {
                        start_time: Instant::now(),
                        noise_detection_duration,
                        remaining_measurements,
                        current_max_noise_floor: 0f32,
                    });
                }
            }
        }

        let mut samples = Vec::with_capacity(count);
        for _ in 0..count {
            let mut state_ref = self.state.borrow_mut();
            let mut state = state_ref.take().expect("State was unexpectedly empty");
            let sample = match state {
                State::Idle => 0f32,
                State::DetectNoiseProfile { .. } => 0f32,
                State::ReadyToMeasure {
                    noise_threshold,
                    remaining_measurements,
                    existing_measurements,
                } => {
                    state = State::Measuring {
                        start_time: Instant::now(),
                        noise_threshold,
                        remaining_measurements,
                        existing_measurements,
                    };
                    self.sample_clock = (self.sample_clock + 1.0) % SAMPLE_RATE;
                    (self.sample_clock * 440.0 * 2.0 * std::f32::consts::PI / SAMPLE_RATE).sin()
                }
                State::Measuring { .. } => {
                    self.sample_clock = (self.sample_clock + 1.0) % SAMPLE_RATE;
                    (self.sample_clock * 440.0 * 2.0 * std::f32::consts::PI / SAMPLE_RATE).sin()
                }
                State::WaitingForSilence { .. } => 0f32,
            };
            *state_ref = Some(state);

            if self.samples_since_last_send == SEND_EVERY_NTH_SAMPLE {
                self.samples_since_last_send = 0;
            }

            if self.samples_since_last_send == 0 {
                let now = Instant::now();
                if let Some(last_send_time) = self.last_send_time {
                    if (now - last_send_time) < Duration::from_millis(1) {
                        // This is from the same buffer, so use the time from the beginning of when
                        // processing started on the buffer. let the plot properly pad each sample
                        // as (1000 / TIME_PLOT_SAMPLES_PER_SECOND) milliseconds apart. This works
                        // because we're send samples at TIME_PLOT_SAMPLES_PER_SECOND, so things
                        // _should_ line up nice.
                        self.audio_tx
                            .send(AudioData::OutgoingSample(last_send_time, sample))
                            .unwrap();
                    } else {
                        self.last_send_time = Some(now);
                        self.audio_tx
                            .send(AudioData::OutgoingSample(now, sample))
                            .unwrap();
                    }
                } else {
                    self.last_send_time = Some(now);
                    self.audio_tx
                        .send(AudioData::OutgoingSample(now, sample))
                        .unwrap();
                }
            }
            self.samples_since_last_send += 1;
            samples.push(sample);
        }

        callback((&samples, None));
    }

    fn drop_samples(&mut self) {}
}

pub fn run(audio_tx: Sender<AudioData>, command_rx: Receiver<AudioCommand>) {
    let state = Rc::new(RefCell::new(Some(State::Idle)));
    let event_loop = create_minimal_event_loop(
        Mic {
            state: state.clone(),
            audio_tx: audio_tx.clone(),
            recent_samples: Vec::new(),
            time_of_first_sample: None,
            samples_since_last_send: 0,
        },
        UnsharedPlaybackSource(Beep {
            state,
            audio_tx,
            command_rx,
            sample_clock: 0f32,
            samples_since_last_send: 0,
            last_send_time: None,
        }),
    );
    event_loop.run();
}
