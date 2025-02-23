use crate::buffer::{MicSink, PlaybackSource};
use dasp::Sample;
use dasp::interpolate::Interpolator;
use dasp::interpolate::linear::Linear;
use dasp::sample::FromSample;

pub trait MicSampleRateConverter<M: MicSink> {
    fn feed_next(&mut self, sample: f64, mic_sink: &mut M);
}

pub trait PlaybackSampleRateConverter<P: PlaybackSource> {
    fn convert_f32(
        &mut self,
        destination: &mut [f32],
        playback_source: &mut P,
        channel_count: usize,
    );
    fn convert_i16(
        &mut self,
        destination: &mut [i16],
        playback_source: &mut P,
        channel_count: usize,
    );
}

// TODO f32 instead of f64?
pub struct LinearSampleRateConverter {
    interpolator: Linear<[f64; 1]>,
    interpolation_value: u32,
    source_rate: u32,
    destination_rate: u32,
}

impl<M: MicSink> MicSampleRateConverter<M> for LinearSampleRateConverter {
    fn feed_next(&mut self, sample: f64, mic_sink: &mut M) {
        if self.interpolation_value >= self.destination_rate {
            self.interpolator.next_source_frame([sample]);
            self.interpolation_value -= self.destination_rate;
        }

        while self.interpolation_value < self.destination_rate {
            let sample = self
                .interpolator
                .interpolate(self.interpolation_value as f64 / self.destination_rate as f64);
            self.interpolation_value += self.source_rate;
            mic_sink.append_sample_from_mic(sample[0].to_sample());
        }
    }
}

impl<P: PlaybackSource> PlaybackSampleRateConverter<P> for LinearSampleRateConverter {
    fn convert_f32(
        &mut self,
        destination: &mut [f32],
        playback_source: &mut P,
        channel_count: usize,
    ) {
        convert(
            &mut self.interpolator,
            &mut self.interpolation_value,
            destination,
            playback_source,
            self.source_rate,
            self.destination_rate,
            channel_count,
        );
    }

    fn convert_i16(
        &mut self,
        destination: &mut [i16],
        playback_source: &mut P,
        channel_count: usize,
    ) {
        convert(
            &mut self.interpolator,
            &mut self.interpolation_value,
            destination,
            playback_source,
            self.source_rate,
            self.destination_rate,
            channel_count,
        );
    }
}

fn convert<T: FromSample<f64> + Copy, P: PlaybackSource>(
    interpolator: &mut Linear<[f64; 1]>,
    interpolation_value: &mut u32,
    destination: &mut [T],
    playback_source: &mut P,
    source_rate: u32,
    destination_rate: u32,
    channel_count: usize,
) {
    let source_to_target_ratio = (source_rate as f64) / (destination_rate as f64);
    let destination_sample_count = destination.len() / channel_count;
    let samples_to_fetch_count = (destination_sample_count as f64 * source_to_target_ratio
        - (1f64 - (*interpolation_value as f64 / destination_rate as f64)))
        .ceil()
        .max(0f64) as usize;

    playback_source.next_samples(samples_to_fetch_count, |(window_1, window_2)| {
        let mut source_samples = window_1.iter().chain(window_2.unwrap_or_default().iter());
        let mut sample_index: usize = 0;

        while sample_index < destination_sample_count {
            while *interpolation_value >= destination_rate {
                interpolator.next_source_frame([source_samples
                    .next()
                    .expect("Should always have loaded enough samples")
                    .to_sample()]);
                *interpolation_value -= destination_rate;
            }

            let sample: T = interpolator
                .interpolate(*interpolation_value as f64 / destination_rate as f64)[0]
                .to_sample();
            for channel_index in 0..channel_count {
                destination[sample_index * channel_count + channel_index] = sample;
            }
            *interpolation_value += source_rate;
            sample_index += 1;
        }
    });
}

struct PassthroughSampleRateConverter;
impl<M: MicSink> MicSampleRateConverter<M> for PassthroughSampleRateConverter {
    fn feed_next(&mut self, sample: f64, mic_sink: &mut M) {
        mic_sink.append_sample_from_mic(sample.to_sample())
    }
}

impl<P: PlaybackSource> PlaybackSampleRateConverter<P> for PassthroughSampleRateConverter {
    fn convert_f32(
        &mut self,
        destination: &mut [f32],
        playback_source: &mut P,
        channel_count: usize,
    ) {
        playback_source.next_samples(destination.len() / channel_count, |(window_1, window_2)| {
            for (i, sample) in window_1
                .iter()
                .chain(window_2.unwrap_or_default().iter())
                .enumerate()
            {
                for channel_index in 0..channel_count {
                    destination[(i * channel_count) + channel_index] = *sample;
                }
            }
        });
    }

    fn convert_i16(
        &mut self,
        destination: &mut [i16],
        playback_source: &mut P,
        channel_count: usize,
    ) {
        playback_source.next_samples(destination.len() / channel_count, |(window_1, window_2)| {
            for (i, sample) in window_1
                .iter()
                .chain(window_2.unwrap_or_default().iter())
                .enumerate()
            {
                for channel_index in 0..channel_count {
                    destination[(i * channel_count) + channel_index] = sample.to_sample();
                }
            }
        });
    }
}

pub fn create_mic_sample_rate_converter<M: MicSink>(
    source_hz: u32,
    target_hz: u32,
) -> Box<dyn MicSampleRateConverter<M>> {
    if source_hz == target_hz {
        return Box::new(PassthroughSampleRateConverter);
    }

    Box::new(LinearSampleRateConverter {
        interpolator: Linear::new([0f64; 1], [0f64; 1]),
        interpolation_value: 0,
        source_rate: source_hz,
        destination_rate: target_hz,
    })
}

pub fn create_playback_sample_rate_converter<P: PlaybackSource>(
    source_hz: u32,
    target_hz: u32,
) -> Box<dyn PlaybackSampleRateConverter<P>> {
    if source_hz == target_hz {
        return Box::new(PassthroughSampleRateConverter);
    }

    Box::new(LinearSampleRateConverter {
        interpolator: Linear::new([0f64; 1], [0f64; 1]),
        interpolation_value: 0,
        source_rate: source_hz,
        destination_rate: target_hz,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockPlaybackSource(Vec<f32>);

    impl PlaybackSource for MockPlaybackSource {
        fn next_samples(&mut self, count: usize, callback: impl FnOnce((&[f32], Option<&[f32]>))) {
            callback((&self.0[..1], Some(&self.0[1..count])));
        }

        fn drop_samples(&mut self) {
            self.0.clear();
        }
    }

    #[test]
    fn test_convert_e2e() {
        let mut interpolator = Linear::new([1f64; 1], [1f64; 1]);
        let mut interpolator_value = 0u32;

        let mut destination = [0f64; 44100];
        let mut source = MockPlaybackSource([1f32; 24000].to_vec());
        convert(
            &mut interpolator,
            &mut interpolator_value,
            &mut destination,
            &mut source,
            48000,
            44100,
            2,
        );
        assert_eq!(destination, [1f64; 44100]);
    }

    #[test]
    fn test_convert_values() {
        let mut interpolator = Linear::new([-1f64; 1], [0f64; 1]);
        let mut interpolator_value = 3u32;

        let mut destination = [0f64; 3];
        let mut source = MockPlaybackSource(vec![3f32, 6f32]);
        convert(
            &mut interpolator,
            &mut interpolator_value,
            &mut destination,
            &mut source,
            2,
            3,
            1,
        );
        assert_eq!(destination, [0f64, 2f64, 4f64]);
    }
}
