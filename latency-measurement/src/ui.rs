mod ui_audio;

use crate::ui_audio::AudioCommand;
use eframe::Frame;
use eframe::egui;
use eframe::egui::Context;
use egui::{Button, Color32, ProgressBar, Response, Ui, Widget};
use egui_plot::{Legend, Line, Plot, PlotPoint, PlotPoints};
use std::ops::Neg;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

const TIME_PLOT_DURATION: Duration = Duration::from_millis(500);
const TIME_PLOT_SAMPLES_PER_SECOND: u32 = 1000u32;
const DETECT_NOISE_TIME: Duration = Duration::from_millis(150);
const MEASUREMENT_COUNT: u8 = 5u8;

#[derive(Debug)]
enum UiState {
    Initial,
    LocalDetectNoiseProfile {
        start: Instant,
    },
    LocalMeasuring {
        noise_threshold: f32,
    },
    RemoteDetectNoiseProfile {
        start: Instant,
        local_latency: Duration,
    },
    RemoteMeasuring {
        noise_threshold: f32,
        local_latency: Duration,
    },
    Results {
        local_latency: Duration,
        remote_latency: Option<Duration>,
    },
    TimedOut {
        local_latency: Option<Duration>,
    },
}

pub enum AudioData {
    IncomingSample(Instant, f32),
    OutgoingSample(Instant, f32),
    NoiseProfileDetected(Instant, f32),
    LatencyTimeout(Instant, f32),
    MeasuredLatency(Duration),
}

struct TimePlot {
    outgoing_samples: Vec<(Instant, f32)>,
    incoming_samples: Vec<(Instant, f32)>,
    noise_threshold: Option<(Instant, f32)>,
}

fn samples_to_values(now: &Instant, samples: &[(Instant, f32)]) -> Vec<PlotPoint> {
    let mut time_marker: Option<Instant> = None;
    let mut existing_samples_at_same_time = 0u128;
    let ms_between_samples = (1000 / TIME_PLOT_SAMPLES_PER_SECOND) as u128;

    samples
        .iter()
        .map(|(instant, sample)| {
            let sample = sample.abs();
            if let Some(time) = time_marker {
                if instant == &time {
                    existing_samples_at_same_time += 1;
                    let millis = (*now - *instant).as_millis() as f64
                        - (existing_samples_at_same_time * ms_between_samples) as f64;
                    PlotPoint::new(millis.neg(), sample)
                } else {
                    existing_samples_at_same_time = 0;
                    time_marker = Some(*instant);
                    PlotPoint::new(((*now - *instant).as_millis() as f64).neg(), sample)
                }
            } else {
                time_marker = Some(*instant);
                PlotPoint::new(((*now - *instant).as_millis() as f64).neg(), sample)
            }
        })
        .collect()
}

impl Widget for &mut TimePlot {
    fn ui(self, ui: &mut Ui) -> Response {
        ui.ctx().request_repaint();

        let now = Instant::now();
        self.incoming_samples
            .retain(|(instant, _)| now.duration_since(*instant) <= TIME_PLOT_DURATION);
        self.outgoing_samples
            .retain(|(instant, _)| now.duration_since(*instant) <= TIME_PLOT_DURATION);

        let incoming_samples_plot_points: Vec<_> = samples_to_values(&now, &self.incoming_samples);
        let outgoing_samples_plot_points: Vec<_> = samples_to_values(&now, &self.outgoing_samples);

        Plot::new("Time Plot")
            .include_x((TIME_PLOT_DURATION.as_millis() as f64).neg())
            .include_x(0f64)
            .include_y(0f64)
            .include_y(1.5f64)
            .legend(Legend::default())
            .show(ui, |plot| {
                plot.line(
                    Line::new(
                        "Incoming Audio",
                        PlotPoints::Owned(incoming_samples_plot_points.clone()),
                    )
                    .name("Incoming Audio")
                    .color(Color32::from_rgba_unmultiplied(0, 255, 0, 32)),
                );
                plot.line(
                    Line::new(
                        "Ougoing Audio",
                        PlotPoints::Owned(outgoing_samples_plot_points.clone()),
                    )
                    .name("Outgoing Audio")
                    .color(Color32::from_rgba_unmultiplied(255, 0, 0, 32)),
                );
                if let Some((instant, noise_threshold)) = self.noise_threshold {
                    let millis = ((now - instant).as_millis() as f64).neg();

                    plot.line(
                        Line::new(
                            "Noise threshold",
                            PlotPoints::from_iter([
                                [millis, noise_threshold as f64],
                                [0f64, noise_threshold as f64],
                            ]),
                        )
                        .name("Noise threshold")
                        .color(Color32::from_rgba_unmultiplied(0, 0, 255, 255)),
                    );
                }
            })
            .response
    }
}

struct App {
    time_plot: TimePlot,
    ui_state: UiState,
    audio_rx: Receiver<AudioData>,
    command_tx: Sender<AudioCommand>,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &Context, _: &mut Frame) {
        for data in self.audio_rx.try_iter() {
            match data {
                AudioData::IncomingSample(when, sample) => {
                    self.time_plot.incoming_samples.push((when, sample));
                }
                AudioData::OutgoingSample(when, sample) => {
                    self.time_plot.outgoing_samples.push((when, sample));
                }
                AudioData::NoiseProfileDetected(when, noise_threshold) => {
                    self.time_plot.noise_threshold = Some((when, noise_threshold));
                    match self.ui_state {
                        UiState::LocalDetectNoiseProfile { .. } => {
                            self.ui_state = UiState::LocalMeasuring { noise_threshold };
                        }
                        UiState::RemoteDetectNoiseProfile { local_latency, .. } => {
                            self.ui_state = UiState::RemoteMeasuring {
                                local_latency,
                                noise_threshold,
                            };
                        }
                        ref state => {
                            panic!("Unexpected state after measuring latency: {state:?}")
                        }
                    }
                }
                AudioData::LatencyTimeout(_, _) => match self.ui_state {
                    UiState::LocalMeasuring { .. } => {
                        self.ui_state = UiState::TimedOut {
                            local_latency: None,
                        }
                    }
                    UiState::RemoteMeasuring { local_latency, .. } => {
                        self.ui_state = UiState::TimedOut {
                            local_latency: Some(local_latency),
                        }
                    }
                    ref state => panic!("Unexpected state after measuring latency: {state:?}"),
                },
                AudioData::MeasuredLatency(latency) => match self.ui_state {
                    UiState::LocalMeasuring { .. } => {
                        self.ui_state = UiState::Results {
                            local_latency: latency,
                            remote_latency: None,
                        };
                    }
                    UiState::RemoteMeasuring { local_latency, .. } => {
                        let remote_latency =
                            latency.checked_sub(local_latency).unwrap_or(Duration::ZERO);
                        self.ui_state = UiState::Results {
                            local_latency,
                            remote_latency: Some(remote_latency),
                        };
                    }
                    _ => panic!("Unexpected state after measuring latency"),
                },
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                ui.add_sized(
                    [ui.available_width() / 2.0, ui.available_height()],
                    &mut self.time_plot,
                );
                ui.vertical(|ui| {
                    ui.heading("Measure Audio latency");
                    if let Some(state) = match &self.ui_state {
                        UiState::Initial => {
                            ui.label(
                                "Plug a cable from your mic port to your speaker port, \
                            then click the \"Measure Latency\" button.",
                            );
                            if ui
                                .add_sized(ui.available_size(), Button::new("Measure Latency"))
                                .clicked()
                            {
                                self.command_tx
                                    .send(AudioCommand::DoMeasure {
                                        remaining_measurements: MEASUREMENT_COUNT,
                                        noise_detection_duration: DETECT_NOISE_TIME,
                                    })
                                    .unwrap();
                                Some(UiState::LocalDetectNoiseProfile {
                                    start: Instant::now(),
                                })
                            } else {
                                None
                            }
                        }
                        UiState::LocalDetectNoiseProfile { start } => {
                            ui.label("Detecting noise floor...");
                            ui.add(ProgressBar::new(
                                start.elapsed().as_millis() as f32
                                    / DETECT_NOISE_TIME.as_millis() as f32,
                            ));
                            None
                        }
                        UiState::LocalMeasuring {
                            noise_threshold, ..
                        } => {
                            ui.label(format!("Noise floor detected at {noise_threshold}"));
                            ui.label("Detecting latency...");
                            None
                        }
                        UiState::RemoteDetectNoiseProfile { start, .. } => {
                            ui.label("Detecting noise floor...");
                            ui.add(ProgressBar::new(
                                start.elapsed().as_millis() as f32
                                    / DETECT_NOISE_TIME.as_millis() as f32,
                            ));
                            None
                        }
                        UiState::RemoteMeasuring {
                            noise_threshold, ..
                        } => {
                            ui.label(format!("Noise floor detected at {noise_threshold}"));
                            ui.label("Detecting latency...");
                            None
                        }
                        UiState::TimedOut { local_latency, .. } => {
                            ui.label("Failed to detect an audio signal.");
                            ui.label(
                                "Plug a cable from your mic port to your speaker port, \
                            then click the \"Measure Latency\" button.",
                            );
                            if let Some(local_latency) = local_latency {
                                ui.label(
                                    "Do \"Measure Local Latency\" if your audio is only looping \
                                on this machine.",
                                );
                                ui.label(
                                    "Do \"Measure Remote Latency\" if your audio is looping \
                                through an external system that you want to test.",
                                );
                                let available_size = ui.available_size();
                                let local_measure_button = ui.add_sized(
                                    egui::vec2(available_size.x, available_size.y / 2f32),
                                    Button::new("Measure Local Latency"),
                                );
                                let remote_measure_button = ui.add_sized(
                                    egui::vec2(available_size.x, available_size.y / 2f32),
                                    Button::new("Measure Remote Latency"),
                                );

                                if local_measure_button.clicked() {
                                    self.command_tx
                                        .send(AudioCommand::DoMeasure {
                                            remaining_measurements: MEASUREMENT_COUNT,
                                            noise_detection_duration: DETECT_NOISE_TIME,
                                        })
                                        .unwrap();
                                    Some(UiState::LocalDetectNoiseProfile {
                                        start: Instant::now(),
                                    })
                                } else if remote_measure_button.clicked() {
                                    self.command_tx
                                        .send(AudioCommand::DoMeasure {
                                            remaining_measurements: MEASUREMENT_COUNT,
                                            noise_detection_duration: DETECT_NOISE_TIME,
                                        })
                                        .unwrap();
                                    Some(UiState::RemoteDetectNoiseProfile {
                                        local_latency: *local_latency,
                                        start: Instant::now(),
                                    })
                                } else {
                                    None
                                }
                            } else if ui
                                .add_sized(ui.available_size(), Button::new("Measure Latency"))
                                .clicked()
                            {
                                self.command_tx
                                    .send(AudioCommand::DoMeasure {
                                        remaining_measurements: MEASUREMENT_COUNT,
                                        noise_detection_duration: DETECT_NOISE_TIME,
                                    })
                                    .unwrap();
                                Some(UiState::LocalDetectNoiseProfile {
                                    start: Instant::now(),
                                })
                            } else {
                                None
                            }
                        }
                        UiState::Results {
                            local_latency,
                            remote_latency,
                            ..
                        } => {
                            ui.label(format!("Local latency: {}ms", local_latency.as_millis()));
                            if let Some(remote_latency) = remote_latency {
                                ui.label(format!(
                                    "Remote latency: {}ms",
                                    remote_latency.as_millis()
                                ));
                            }
                            ui.label(
                                "Do \"Measure Local Latency\" if your audio is only looping \
                            on this machine.",
                            );
                            ui.label(
                                "Do \"Measure Remote Latency\" if your audio is looping \
                            through an external system that you want to test.",
                            );
                            let available_size = ui.available_size();
                            let local_measure_button = ui.add_sized(
                                egui::vec2(available_size.x, available_size.y / 2f32),
                                Button::new("Measure Local Latency"),
                            );
                            let remote_measure_button = ui.add_sized(
                                egui::vec2(available_size.x, available_size.y / 2f32),
                                Button::new("Measure Remote Latency"),
                            );

                            if local_measure_button.clicked() {
                                self.command_tx
                                    .send(AudioCommand::DoMeasure {
                                        remaining_measurements: MEASUREMENT_COUNT,
                                        noise_detection_duration: DETECT_NOISE_TIME,
                                    })
                                    .unwrap();
                                Some(UiState::LocalDetectNoiseProfile {
                                    start: Instant::now(),
                                })
                            } else if remote_measure_button.clicked() {
                                self.command_tx
                                    .send(AudioCommand::DoMeasure {
                                        remaining_measurements: MEASUREMENT_COUNT,
                                        noise_detection_duration: DETECT_NOISE_TIME,
                                    })
                                    .unwrap();
                                Some(UiState::RemoteDetectNoiseProfile {
                                    local_latency: *local_latency,
                                    start: Instant::now(),
                                })
                            } else {
                                None
                            }
                        }
                    } {
                        self.ui_state = state;
                    }
                });
            })
        });
    }
}

fn main() {
    let (audio_tx, audio_rx) = channel();
    let (command_tx, command_rx) = channel();
    std::thread::spawn(|| {
        ui_audio::run(audio_tx, command_rx);
    });
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([500.0, 270.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Latency Measurement UI",
        native_options,
        Box::new(|_| {
            Ok(Box::new(App {
                time_plot: TimePlot {
                    incoming_samples: Vec::new(),
                    outgoing_samples: Vec::new(),
                    noise_threshold: None,
                },
                ui_state: UiState::Initial,
                audio_rx,
                command_tx,
            }))
        }),
    )
    .unwrap();
}
