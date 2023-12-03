use std::collections::VecDeque;
use std::env;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use crate::audio::{NOISE_GATE_CHUNK_LEN, SAMPLES_IN_CHUNK};
use eframe::egui;
use eframe::egui::Context;
use eframe::Frame;
use egui::*;
use egui_plot::{HLine, Line, Plot};
use egui_plot::{Legend, PlotPoints, Polygon};

const SAMPLES_IN_BUCKET: usize = 16;
const GRAPH_DURATION_IN_SAMPLES: usize = NOISE_GATE_CHUNK_LEN * SAMPLES_IN_CHUNK;
const GRAPH_DURATION: Duration =
    Duration::from_millis((GRAPH_DURATION_IN_SAMPLES as f64 / 44.800) as u64);
const DURATION_BETWEEN_BUCKETS: Duration =
    Duration::from_nanos((1_000_000_000f64 / (44800 / SAMPLES_IN_BUCKET) as f64) as u64);

pub enum IncomingAudioPlotData {
    SampleChunk(Vec<f32>),
    NoiseGateThreshold(f32),
    ChunkRms(f32),
    ChunkMax(f32),
    StateChange(u8),
}

pub enum Command {
    ToggleMute,
    StartRecording,
    StopRecording(PathBuf),
    PlayRecording(PathBuf),
    PlayLoopback,
}

fn samples_to_line(samples: &VecDeque<(Instant, f32)>) -> Line {
    Line::new(PlotPoints::from_parametric_callback(
        move |t| {
            let (instant, sample) = samples.get(t as usize).copied().unwrap();
            (
                -((instant
                    .elapsed()
                    .as_millis()
                    .min(GRAPH_DURATION.as_millis())) as f64),
                sample as f64,
            )
        },
        0f64..samples.len() as f64,
        samples.len(),
    ))
}

pub struct AudioPlot {
    receiver: Receiver<IncomingAudioPlotData>,
    aggregate: Vec<f32>,
    threshold: f32,
    buckets: VecDeque<(Instant, f32)>,
    chunk_rmses: VecDeque<(Instant, f32)>,
    chunk_maxes: VecDeque<(Instant, f32)>,
    state: u8,
    last_bucket_instant: Instant,
    recording_start: Option<Instant>,
    recording_end: Option<Instant>,
}

impl AudioPlot {
    fn new(receiver: Receiver<IncomingAudioPlotData>) -> Self {
        Self {
            receiver,
            aggregate: Vec::new(),
            threshold: 0f32,
            buckets: VecDeque::new(),
            chunk_rmses: VecDeque::new(),
            chunk_maxes: VecDeque::new(),
            state: 0u8,
            last_bucket_instant: Instant::now(),
            recording_start: None,
            recording_end: None,
        }
    }
}

impl AudioPlot {
    fn update(&mut self) {
        let Self {
            receiver,
            aggregate,
            buckets,
            chunk_rmses,
            chunk_maxes,
            last_bucket_instant,
            ..
        } = self;

        for incoming_data in receiver.try_iter() {
            match incoming_data {
                IncomingAudioPlotData::SampleChunk(chunk) => {
                    aggregate.extend(chunk);
                    while aggregate.len() >= SAMPLES_IN_BUCKET {
                        let next_bucket = aggregate
                            .drain(..SAMPLES_IN_BUCKET)
                            .map(|sample| sample.abs())
                            .sum::<f32>()
                            / SAMPLES_IN_BUCKET as f32;

                        let now = Instant::now();

                        if last_bucket_instant.elapsed() > GRAPH_DURATION {
                            *last_bucket_instant = Instant::now();
                        }

                        let bucket_duration = last_bucket_instant.duration_since(now);
                        let bucket_instant = if bucket_duration < DURATION_BETWEEN_BUCKETS {
                            *last_bucket_instant = last_bucket_instant
                                .checked_add(DURATION_BETWEEN_BUCKETS)
                                .unwrap();
                            *last_bucket_instant
                        } else {
                            *last_bucket_instant = now;
                            now
                        };
                        buckets.push_back((bucket_instant, next_bucket));
                    }
                }
                IncomingAudioPlotData::NoiseGateThreshold(threshold) => {
                    self.threshold = threshold;
                }
                IncomingAudioPlotData::ChunkRms(rms) => {
                    chunk_rmses.push_back((Instant::now(), rms))
                }
                IncomingAudioPlotData::ChunkMax(max) => {
                    chunk_maxes.push_back((Instant::now(), max))
                }
                IncomingAudioPlotData::StateChange(state) => self.state = state,
            }
        }

        buckets.retain(|(instant, _)| instant.elapsed() < GRAPH_DURATION);
        chunk_rmses.retain(|(instant, _)| instant.elapsed() < GRAPH_DURATION);
        chunk_maxes.retain(|(instant, _)| instant.elapsed() < GRAPH_DURATION);
    }

    fn buckets(&mut self) -> Line {
        let buckets = &self.buckets;

        if buckets.is_empty() {
            return Line::new(PlotPoints::default());
        }

        samples_to_line(buckets)
            .color(Color32::from_rgb(100, 220, 100))
            .name(format!("Samples (groups of {SAMPLES_IN_BUCKET})"))
    }

    fn rmses(&mut self) -> Line {
        let chunks = &self.chunk_rmses;

        if chunks.is_empty() {
            return Line::new(PlotPoints::default());
        }

        samples_to_line(chunks)
            .color(Color32::from_rgb(220, 100, 100))
            .name("Chunk RMSes")
    }

    fn maxes(&mut self) -> Line {
        let chunks = &self.chunk_maxes;

        if chunks.is_empty() {
            return Line::new(PlotPoints::default());
        }

        samples_to_line(chunks)
            .color(Color32::from_rgb(100, 100, 220))
            .name("Chunk maxes")
    }

    fn threshold(&self) -> HLine {
        let threshold_color = match self.state {
            0 => Color32::from_rgb(255, 50, 255),
            1 => Color32::from_rgb(255, 50, 50),
            2 => Color32::from_rgb(50, 50, 255),
            3 => Color32::from_rgb(50, 50, 255),
            4 => Color32::from_rgb(50, 255, 50),
            _ => Color32::from_rgb(50, 50, 50),
        };

        HLine::new(self.threshold)
            .color(threshold_color)
            .name("Threshold")
    }

    fn recording(&self) -> Polygon {
        if let Some(start) = self.recording_start {
            let start_x = -((start.elapsed().as_millis().min(GRAPH_DURATION.as_millis())) as f64);
            let end_x = if let Some(end) = self.recording_end {
                -((end.elapsed().as_millis().min(GRAPH_DURATION.as_millis())) as f64)
            } else {
                0f64
            };

            if start_x == end_x {
                // Beginning and end are equivalent (such as when recording happened before
                // beginning of current window) - don't show a polygon at all.
                Polygon::new(PlotPoints::default())
            } else {
                Polygon::new(PlotPoints::from_iter([
                    [start_x, 0f64],
                    [start_x, 1f64],
                    [end_x, 1f64],
                    [end_x, 0f64],
                ]))
            }
        } else {
            Polygon::new(PlotPoints::default())
        }
        .fill_color(Color32::from_rgba_unmultiplied(180, 120, 120, 128))
        .name("Recording")
    }
}

impl Widget for &mut AudioPlot {
    fn ui(self, ui: &mut Ui) -> Response {
        ui.ctx().request_repaint();
        self.update();

        Plot::new("Demo Plot")
            .include_x(-(GRAPH_DURATION.as_millis() as f64))
            .include_x(0f64)
            .include_y(0f64)
            .include_y(0.05f64)
            .legend(Legend::default())
            .show(ui, |plot| {
                plot.line(self.buckets());
                plot.line(self.rmses());
                plot.line(self.maxes());
                plot.hline(self.threshold());
                plot.polygon(self.recording());
            })
            .response
    }
}

struct App {
    time: AudioPlot,
    is_muted: bool,
    recording_path: Option<PathBuf>,
    last_playback_recording: Option<PathBuf>,
    playback_source_is_loopback: bool,
    command_tx: Sender<Command>,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &Context, _: &mut Frame) {
        let Self {
            time,
            is_muted,
            recording_path,
            last_playback_recording,
            playback_source_is_loopback,
            command_tx,
        } = self;

        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            // The top panel is often a good place for a menu bar:
            egui::menu::bar(ui, |ui| {
                egui::menu::menu_button(ui, "File", |ui| {
                    if ui.button("Quit").clicked() {
                        ctx.send_viewport_cmd(ViewportCommand::Close)
                    }
                });
            });
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                let mute_control_text = if *is_muted { "Unmute" } else { "Mute" };
                if ui.button(mute_control_text).clicked() {
                    *is_muted = !*is_muted;
                    command_tx.send(Command::ToggleMute).unwrap();
                }
                if recording_path.is_none() && ui.button("Play recording file").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_directory(
                            last_playback_recording
                                .as_ref()
                                .map(|p| p.parent().unwrap().to_owned())
                                .unwrap_or_else(|| {
                                    env::current_exe().unwrap().parent().unwrap().to_owned()
                                }),
                        )
                        .set_file_name(
                            last_playback_recording
                                .as_ref()
                                .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
                                .unwrap_or_else(|| "comms-audio-dump.dmp".to_string()),
                        )
                        .add_filter("Comms audio dump files", &["dmp"])
                        .add_filter("All Files", &["*"])
                        .pick_file()
                    {
                        command_tx
                            .send(Command::PlayRecording(path.clone()))
                            .unwrap();
                        *playback_source_is_loopback = false;
                        *last_playback_recording = Some(path);
                    }
                }
                if !*playback_source_is_loopback {
                    if ui.button("Play recording again").clicked() {
                        command_tx
                            .send(Command::PlayRecording(
                                last_playback_recording.to_owned().unwrap(),
                            ))
                            .unwrap();
                    }
                    if ui.button("Play microphone").clicked() {
                        command_tx.send(Command::PlayLoopback).unwrap();
                        *playback_source_is_loopback = true;
                    }
                } else if recording_path.is_none() {
                    if ui.button("Record to file").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .set_directory(env::current_exe().unwrap().parent().unwrap())
                            .set_file_name("comms-audio-dump.dmp")
                            .add_filter("Comms audio dump files", &["dmp"])
                            .add_filter("All Files", &["*"])
                            .save_file()
                        {
                            command_tx.send(Command::StartRecording).unwrap();
                            time.recording_start = Some(Instant::now());
                            time.recording_end = None;
                            *recording_path = Some(path);
                        }
                    }
                } else if ui.button("Stop recording").clicked() {
                    command_tx
                        .send(Command::StopRecording(recording_path.take().unwrap()))
                        .unwrap();
                    time.recording_end = Some(Instant::now());
                }
            });
            ui.add_sized([ui.available_width(), ui.available_height()], time);
        });
    }
}

pub fn run(command_tx: Sender<Command>, time_plot_receiver: Receiver<IncomingAudioPlotData>) {
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "Live noise gate UI",
        native_options,
        Box::new(|_| {
            Box::new(App {
                time: AudioPlot::new(time_plot_receiver),
                is_muted: false,
                recording_path: None,
                last_playback_recording: None,
                playback_source_is_loopback: true,
                command_tx,
            })
        }),
    )
    .unwrap();
}
