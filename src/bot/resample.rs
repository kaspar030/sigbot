// An example of using `sample` to efficiently perform decent quality sample rate conversion on a
// WAV file entirely on the stack.

use dasp::{interpolate::sinc::Sinc, ring_buffer, signal, Sample, Signal};

pub fn resample(input: &[i16], sample_rate: usize) -> Vec<f32> {
    // Read the interleaved samples and convert them to a signal.
    let samples: Vec<f32> = input
        .iter()
        .map(|sample| sample.to_sample::<f32>())
        .collect();

    let signal = signal::from_interleaved_samples_iter(samples);

    // Convert the signal's sample rate using `Sinc` interpolation.
    let ring_buffer = ring_buffer::Fixed::from([[0.0]; 100]);
    let sinc = Sinc::new(ring_buffer);
    let new_signal = signal.from_hz_to_hz(sinc, 44100 as f64, sample_rate as f64);

    let output: Vec<f32> = new_signal
        .until_exhausted()
        .map(|frame| frame[0].to_sample::<f32>())
        .collect();

    output
}
