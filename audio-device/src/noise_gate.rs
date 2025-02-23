use dasp::Sample;
use ringbuffer::{AllocRingBuffer, RingBuffer};

const ATTACK_SAMPLE_COUNT: usize = 400;
const HOLD_SAMPLE_COUNT: usize = 10000;
const RELEASE_SAMPLE_COUNT: usize = 8000;

pub enum State {
    Off,
    Attack(usize),
    Active,
    Hold(usize),
    Release(usize),
}

impl State {
    fn is_active(&self) -> bool {
        !matches!(self, State::Off)
    }

    fn update_threshold_state(&mut self, is_above: bool) {
        *self = match self {
            State::Off => {
                if is_above {
                    State::Attack(0)
                } else {
                    State::Off
                }
            }
            State::Attack(progress) => State::Attack(*progress),
            State::Active => {
                if is_above {
                    State::Active
                } else {
                    State::Hold(0)
                }
            }
            State::Hold(progress) => {
                if is_above {
                    State::Active
                } else {
                    State::Hold(*progress)
                }
            }
            State::Release(progress) => {
                if is_above {
                    let percentage_from_full =
                        1f64 - (*progress as f64 / RELEASE_SAMPLE_COUNT as f64);
                    let attack_progress =
                        (percentage_from_full * ATTACK_SAMPLE_COUNT as f64) as usize;
                    State::Attack(attack_progress)
                } else {
                    State::Release(*progress)
                }
            }
        }
    }

    fn attenuate_sample(&mut self, sample: f32) -> f32 {
        let (new_state, sample) = match self {
            State::Off => (State::Off, Sample::EQUILIBRIUM),
            &mut State::Attack(mut progress) => {
                progress += 1;
                let state = if progress == ATTACK_SAMPLE_COUNT {
                    State::Active
                } else {
                    State::Attack(progress)
                };

                (
                    state,
                    sample.mul_amp(progress as f32 / ATTACK_SAMPLE_COUNT as f32),
                )
            }
            State::Active => (State::Active, sample),
            &mut State::Hold(mut progress) => {
                progress += 1;
                let state = if progress == HOLD_SAMPLE_COUNT {
                    State::Release(0)
                } else {
                    State::Hold(progress)
                };
                (state, sample)
            }
            &mut State::Release(mut progress) => {
                progress += 1;
                let state = if progress == RELEASE_SAMPLE_COUNT {
                    State::Off
                } else {
                    State::Release(progress)
                };
                (
                    state,
                    sample.mul_amp(1f32 - (progress as f32 / RELEASE_SAMPLE_COUNT as f32)),
                )
            }
        };
        *self = new_state;
        sample
    }
}

pub struct GateNoiseFilter {
    pub recent_chunks: AllocRingBuffer<f32>,
    pub threshold: f32,
    pub state: State,
    was_active: bool,
}

pub enum ChangedState {
    NONE,
    OPEN,
    CLOSED,
}

impl GateNoiseFilter {
    pub fn new(sliding_window_chunk_count: usize) -> Self {
        GateNoiseFilter {
            recent_chunks: AllocRingBuffer::new(sliding_window_chunk_count),
            threshold: 0f32,
            state: State::Off,
            was_active: false,
        }
    }

    pub fn update_window(&mut self, next_chunk: &[f32]) -> ChangedState {
        let mut accumulator = 0f32;
        for sample in next_chunk {
            accumulator += sample * sample;
        }

        let rms = (accumulator / next_chunk.len() as f32).sqrt();

        let max = next_chunk
            .iter()
            .map(|s| s.abs())
            .max_by(|x, y| x.partial_cmp(y).unwrap())
            .unwrap();

        self.recent_chunks.push(max);

        let noise_floor = self
            .recent_chunks
            .iter()
            .fold(f32::INFINITY, |a, &b| a.min(b));
        self.threshold = noise_floor * 1.1 + 0.002;
        self.state.update_threshold_state(rms > self.threshold);
        let is_now_active = self.state.is_active();
        let state_change = match (self.was_active, is_now_active) {
            (false, true) => ChangedState::OPEN,
            (true, false) => ChangedState::CLOSED,
            _ => ChangedState::NONE,
        };
        self.was_active = is_now_active;
        state_change
    }

    pub fn process(&mut self, sample: f32) -> f32 {
        self.state.attenuate_sample(sample)
    }

    pub fn reset(&mut self) {
        self.recent_chunks.clear();
        self.threshold = 0f32;
        self.state = State::Off;
    }
}
