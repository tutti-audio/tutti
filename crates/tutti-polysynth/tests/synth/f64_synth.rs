//! The synth at `f64`: built, driven as a graph drives it, and heard at the output.

use std::num::NonZeroUsize;
use tutti_fixtures::midi;
use tutti_graph::contract::Direct;
use tutti_graph::{Event, Offset};

use tutti_polysynth::{Adsr, OscillatorType, PolySynth, SynthConfig};
use tutti_ump::{MidiChannel, NoteVelocity, U7};
use tutti_units::{Amplitude, SampleRate, Seconds};

const BLOCK: usize = 64;

/// A sine synth of one voice with a flat envelope, so the output is the oscillator and the
/// voice's gain alone.
fn sine() -> PolySynth<f64> {
    PolySynth::<f64>::new(SynthConfig {
        oscillator: OscillatorType::Sine,
        polyphony: NonZeroUsize::new(1).unwrap(),
        envelope: Adsr::new(
            Seconds::new(0.0),
            Seconds::new(0.0),
            Amplitude::new(1.0),
            Seconds::new(0.1),
        ),
        ..Default::default()
    })
}

/// The left channel of `blocks` blocks after a Note On of `note`, as a graph drives it.
fn play(note: u8, blocks: usize) -> Vec<f64> {
    let mut direct: Direct<PolySynth<f64>, f64> =
        Direct::with_format(sine(), SampleRate::SR_48K, BLOCK);
    let input = tutti_graph::Node::shape(&*direct.node)
        .event_input(0)
        .expect("an event input");
    let on = midi::note_on(
        MidiChannel::FIRST,
        note,
        NoteVelocity::from(U7::of(127)).get(),
    );
    let mut out = Vec::new();
    for b in 0..blocks {
        let events = if b == 0 {
            vec![Event::midi(Offset::ZERO, on)]
        } else {
            vec![]
        };
        direct.events(input, &events);
        direct.block();
        out.extend_from_slice(direct.output().plane(0).samples());
    }
    out
}

/// A sine of 440 Hz crosses zero upward 440 times a second: the count over one second,
/// from the first rising crossing, is 440 give or take the one the end of the window
/// cuts.
#[test]
fn a4_at_f64_rises_through_zero_440_times_a_second() {
    let out = play(69, 48_000 / BLOCK);
    assert!(
        out.iter().any(|s| s.abs() > 0.1),
        "the note is silent at f64"
    );
    let rising = out.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
    assert!(
        (439..=441).contains(&rising),
        "{rising} rising crossings in one second, want 440"
    );
}

/// A centred voice at full velocity peaks at the constant-power centre gain, `sqrt(1/2)`.
/// The samples miss the true peak by at most half a step of phase, `pi * 440 / 48000`
/// radians, so the sampled peak is under the gain by at most `1 - cos` of that, `4.1e-4`
/// of it: the bound is `1e-3` of the gain.
#[test]
fn a_centred_full_velocity_voice_peaks_at_the_constant_power_centre_gain_at_f64() {
    let out = play(69, 48_000 / BLOCK);
    let peak = out.iter().fold(0.0f64, |a, s| a.max(s.abs()));
    let want = core::f64::consts::FRAC_1_SQRT_2;
    assert!(
        (peak - want).abs() < 1e-3 * want,
        "peak {peak}, want {want}"
    );
}
