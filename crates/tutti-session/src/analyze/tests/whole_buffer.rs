//! The whole-buffer analyses through `Session::analyze`: each reads the figure
//! its signal was built with, so the run is shown to route the folded mono at
//! the audio's rate to the measure and to return its output.

use tutti_buffers::{ChannelLayout, Float, Interleaved, Mono, Wave};
use tutti_fixtures::signal;
use tutti_measure::{
    AlignConfig, AlignTo, BeatConfig, ChordConfig, ChordQuality, KeyConfig, KeyProfile, Measure,
    Mode, NoteConfig, PitchClass, PyinConfig, StreamMeter, TempoConfig,
};
use tutti_rt::SplitMix64;
use tutti_ump::NoteNumber;
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
        for (total, sample) in sum.iter_mut().zip(signal::sine(hz, amplitude, RATE, len)) {
            *total += sample;
        }
    }
    Wave::from_mono(RATE, Mono::new(&sum))
}

/// A 10 ms burst of a 1 kHz sine decaying with a time constant of 100 samples,
/// at each of `starts`, over `len` frames.
fn clicks(starts: impl IntoIterator<Item = usize>, len: usize) -> Wave<f64> {
    let mut out = vec![0.0f64; len];
    for start in starts {
        for i in 0..480u32 {
            let phase = std::f64::consts::TAU * 1000.0 * f64::from(i) / RATE.get();
            if let Some(sample) = out.get_mut(start + usize::try_from(i).unwrap()) {
                *sample += phase.sin() * (-f64::from(i) / 100.0).exp();
            }
        }
    }
    Wave::from_mono(RATE, Mono::new(&out))
}

/// Clicks every 24 000 frames (120 BPM at 48 kHz), the first at 0.3 s, over 10 s.
const BEAT: usize = 24_000;
const FIRST_CLICK: usize = 14_400;
const CLICK_LEN: usize = 480_000;

fn click_starts() -> Vec<usize> {
    (FIRST_CLICK..CLICK_LEN - BEAT / 5).step_by(BEAT).collect()
}

/// **Notes**: a 440 Hz sine of one second is one note, A4, within 1.5 cents of
/// pitch.
#[test]
fn a_440_hz_sine_is_one_a4_note_through_the_note_transcription() {
    let wave = tones(&[Hz::new(440.0)], Amplitude::new(0.5), Samples::new(48_000));
    let notes = analyzed(wave, NoteConfig::new());
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(NoteNumber::try_from(notes[0].note).map(u8::from), Ok(69));
    assert!(notes[0].cents.get().abs() < 1.5, "{}", notes[0].cents);
}

/// **pYIN**: a 440 Hz sine is voiced at 440 Hz on every frame, within the
/// grid's half step (5 cents) and the parabola's error at 440 Hz (0.65 cents).
#[test]
fn a_440_hz_sine_tracks_at_440_hz_through_pyin() {
    let wave = tones(&[Hz::new(440.0)], Amplitude::new(0.5), Samples::new(48_000));
    let frames = analyzed(wave, PyinConfig::new());
    assert!(!frames.is_empty());
    for (i, frame) in frames.iter().enumerate() {
        let hz = frame
            .frequency
            .unwrap_or_else(|| panic!("frame {i} unvoiced"));
        let cents = 1200.0 * (hz.get() / 440.0).log2();
        assert!(cents.abs() <= 5.7, "frame {i}: {cents} cents");
    }
}

/// **Tempo**: a click train every 24 000 frames reads 120 BPM at 48 kHz, within
/// a third of a frame of period at the default hop (0.9 BPM).
#[test]
fn a_click_train_every_half_second_reads_120_bpm_through_the_tempo_estimate() {
    let wave = clicks(click_starts(), CLICK_LEN);
    let tempo = analyzed(wave, TempoConfig::new());
    assert!((tempo.bpm.get() - 120.0).abs() <= 0.9, "{}", tempo.bpm);
}

/// **Beats**: every beat of that train lands within one hop (512 frames) of a
/// click, and the grid carries the tempo it was placed at.
#[test]
fn beats_of_a_click_train_land_within_a_hop_of_the_clicks() {
    let starts = click_starts();
    let wave = clicks(starts.iter().copied(), CLICK_LEN);
    let grid = analyzed(wave, BeatConfig::new(TempoConfig::new()));
    assert!(grid.beats.len() >= starts.len() - 2, "{:?}", grid.beats);
    for beat in &grid.beats {
        let nearest = starts.iter().map(|s| s.abs_diff(beat.get())).min().unwrap();
        assert!(
            nearest <= 512,
            "beat {beat:?} is {nearest} frames from a click"
        );
    }
    assert!((grid.bpm.get() - 120.0).abs() <= 0.9, "{}", grid.bpm);
}

/// **Chords**: C4, E4 and G4 sounding together are one C major span over the
/// audio.
#[test]
fn a_c_major_triad_is_one_c_major_chord_through_the_chord_detection() {
    let wave = tones(
        &[Hz::new(261.626), Hz::new(329.628), Hz::new(391.995)],
        Amplitude::new(0.25),
        Samples::new(96_000),
    );
    let spans = analyzed(wave, ChordConfig::new());
    let chords: Vec<_> = spans.iter().map(|s| (s.root, s.quality)).collect();
    assert_eq!(chords, [(PitchClass::C, ChordQuality::Major)]);
}

/// **Key**: a C major scale (C4 to C5, half a second a note) reads as C major
/// against the Krumhansl profile.
#[test]
fn a_c_major_scale_is_the_key_of_c_major_through_the_key_estimate() {
    let scale = [
        261.626, 293.665, 329.628, 349.228, 391.995, 440.000, 493.883, 523.251,
    ];
    let samples: Vec<f32> = scale
        .into_iter()
        .flat_map(|hz| signal::sine(Hz::new(hz), Amplitude::new(0.5), RATE, Samples::new(24_000)))
        .collect();
    let wave = Wave::from_mono(RATE, Mono::new(&samples));
    let estimate = analyzed(wave, KeyConfig::new(KeyProfile::Krumhansl));
    assert_eq!(estimate.key.tonic, PitchClass::C);
    assert_eq!(estimate.key.mode, Mode::Major);
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
