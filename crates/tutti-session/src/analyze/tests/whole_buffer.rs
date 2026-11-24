//! The whole-buffer analyses through `Session::analyze`: the route is one
//! generic path for every measure, so two figures show what the session adds.
//! An alignment's delay in seconds shows the audio's rate reaches the measure,
//! and a stereo triad read as one chord shows the measure reads the folded mono.
//! Each analysis's own figures are pinned by tutti-measure's tests.

use tutti_buffers::{ChannelLayout, Float, Interleaved, Mono, Wave};
use tutti_fixtures::signal;
use tutti_measure::{
    AlignConfig, AlignTo, ChordConfig, ChordQuality, Measure, PitchClass, StreamMeter,
};
use tutti_rt::SplitMix64;
use tutti_units::{Amplitude, Hz, Samples};

use super::{RATE, session};
use crate::analyze::{AudioContext, AudioInput};

/// The output of `analysis` over `audio`, with the job and the measure both
/// unwrapped.
fn analyzed<S, A, T, E>(audio: Wave<S>, analysis: A) -> T
where
    S: Float,
    A: Measure<S> + Send + 'static,
    A::Context: AudioContext,
    for<'a> <A::Meter as StreamMeter>::Input<'a>: AudioInput<'a, S>,
    <A::Meter as StreamMeter>::Output: Send + 'static + Into<Result<T, E>>,
    E: std::fmt::Debug,
{
    tutti_fixtures::block_on(session().analyze(audio, analysis))
        .unwrap()
        .into()
        .unwrap()
}

/// The sum of unit-share sines at `freqs`, `seconds` long, at [`RATE`].
fn tones(freqs: &[Hz], amplitude: Amplitude, len: Samples) -> Wave<f32> {
    let mut sum = vec![0.0f32; len.get()];
    for &hz in freqs {
        for (total, sample) in sum
            .iter_mut()
            .zip(signal::sine::<f32>(hz, amplitude, RATE, len))
        {
            *total += sample;
        }
    }
    Wave::from_mono(RATE, Mono::new(&sum))
}

/// **Align**: a take 25 frames late against its noise reference is a delay of
/// 25 frames in seconds, at the audio's rate.
#[test]
fn a_take_25_frames_late_is_a_delay_of_25_frames_in_seconds_through_the_alignment() {
    let mut rng = SplitMix64::new(3);
    let reference: Vec<f64> = (0..4000)
        .map(|_| f64::from(rng.next_u32()) / f64::from(u32::MAX) - 0.5)
        .collect();
    let mut take = vec![0.0; 25];
    take.extend_from_slice(&reference[..reference.len() - 25]);
    let measure = AlignTo::new(
        AlignConfig::new(Samples::new(100)),
        Wave::from_mono(RATE, Mono::new(&reference)),
    );
    let take = Wave::from_mono(RATE, Mono::new(&take));
    let found = analyzed(take, measure);
    assert_eq!(found.lag(), 25);
    let want = 25.0 / RATE.get();
    assert!((found.delay(RATE).get() - want).abs() < 0.05 / RATE.get());
}

/// **Fold**: the same triad in two equal channels reads the same chord, so the
/// analysis is of the folded mono.
#[test]
fn a_stereo_c_major_triad_reads_as_c_major_through_the_folded_mono() {
    let mono = tones(
        &[Hz::new(261.626), Hz::new(329.628), Hz::new(391.995)],
        Amplitude::new(0.25),
        Samples::new(96_000),
    );
    let mut stereo = Wave::<f32>::new(2, RATE);
    for &sample in mono.channel(0).samples() {
        stereo
            .append(Interleaved::new(&[sample, sample], ChannelLayout::STEREO).unwrap())
            .unwrap();
    }
    let spans = analyzed(stereo, ChordConfig::new());
    let chords: Vec<_> = spans.iter().map(|s| (s.root, s.quality)).collect();
    assert_eq!(chords, [(PitchClass::C, ChordQuality::Major)]);
}
