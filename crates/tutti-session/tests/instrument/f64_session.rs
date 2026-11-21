//! The instruments in a session of `f64`: a `PolySynth<f64>` and a General MIDI rig
//! built by the session over its own float, rendered through the strip and the mix.

use std::sync::Arc;
use tutti_buffers::{ChannelLayout, InterleavedMut, MaxBlock};
use tutti_music::Adsr;
use tutti_polysynth::{FilterType, OscillatorType, PolySynth, SynthConfig};
use tutti_session::{InstrumentHandle, OfflineSession, OutPort, Root, Session, Stamp};
use tutti_sf2::SoundFont;
use tutti_soundfont::SoundFontConfig;
use tutti_ump::{MidiChannel, NoteNumber, NoteVelocity};
use tutti_units::{Amplitude, SampleRate, Samples, Seconds};

const RATE: SampleRate = SampleRate::SR_48K;
const BLOCK: usize = 128;
/// A4 in twelve-tone equal temperament, the note-69 reference.
const A4_HZ: f64 = 440.0;

fn session() -> OfflineSession<&'static str, f64> {
    Session::builder()
        .float::<f64>()
        .sample_rate(RATE)
        .max_block(MaxBlock::of(Samples::new(BLOCK)))
        .build()
        .expect("an f64 offline session builds")
}

/// Renders `blocks` blocks of the session's stereo output and returns the left
/// channel.
fn render(session: &mut OfflineSession<&'static str, f64>, blocks: usize) -> Vec<f64> {
    let mut left = Vec::with_capacity(blocks * BLOCK);
    for _ in 0..blocks {
        let mut buf = vec![0.0f64; BLOCK * 2];
        session.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
        let _ = session.update();
        left.extend(buf.chunks(2).map(|frame| frame[0]));
    }
    left
}

fn peak(samples: &[f64]) -> f64 {
    samples.iter().fold(0.0, |m, s| m.max(s.abs()))
}

fn sine_synth() -> PolySynth<f64> {
    PolySynth::new(SynthConfig {
        polyphony: std::num::NonZeroUsize::new(4).unwrap(),
        oscillator: OscillatorType::Sine,
        filter: FilterType::None,
        envelope: Adsr {
            attack: Seconds::new(0.001),
            decay: Seconds::new(0.0),
            sustain: Amplitude::new(1.0),
            release: Seconds::new(0.05),
        },
        ..SynthConfig::default()
    })
}

fn play(handle: &mut InstrumentHandle<impl ?Sized>, note: u8) {
    handle
        .keys_mut()
        .note_on(
            Stamp::NextBlock,
            MidiChannel::FIRST,
            NoteNumber::of(note),
            NoteVelocity::new(0xC800),
        )
        .expect("the queue has room");
}

/// The positive-going zero crossings of `samples`.
fn rising_crossings(samples: &[f64]) -> usize {
    samples
        .windows(2)
        .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
        .count()
}

/// **A sine voice of a `PolySynth<f64>` in an `f64` session sounds at the note's
/// frequency.** Note 69 is 440 Hz: one second holds 440 rising zero crossings
/// (the window's ends can move the count by one), and the output is finite and
/// silent before the note.
#[test]
fn an_f64_synth_instrument_sounds_the_notes_frequency() {
    let mut session = session();
    let mut handle = session
        .add_instrument(sine_synth())
        .expect("a synth is an instrument");
    let _ = session.update();
    let before = render(&mut session, 4);
    assert_eq!(peak(&before), 0.0, "silent before the note");

    play(&mut handle, 69);
    let _settle = render(&mut session, 64);
    let second = render(&mut session, (RATE.get() as usize) / BLOCK);
    assert!(second.iter().all(|s| s.is_finite()), "finite output");
    assert!(peak(&second) > 0.0, "the note sounds");
    // A window of exactly 1 s counts each crossing whole: where its ends fall
    // moves the count by at most one, so 1 Hz; the bound is twice that.
    let crossings = rising_crossings(&second) as f64;
    let seconds = second.len() as f64 / RATE.get();
    assert!(
        (crossings / seconds - A4_HZ).abs() <= 2.0,
        "{crossings} crossings in {seconds} s"
    );
}

/// **The strip's gain on an `f64` session scales a steady sine linearly.** A gain of
/// one half halves the steady-state peak of the same held note: the cell holds the
/// `f64` the unit gives, with nothing narrowed on the way.
#[test]
fn an_f64_instrument_gain_scales_a_held_sine_linearly() {
    let mut session = session();
    let mut handle = session
        .add_instrument(sine_synth())
        .expect("a synth is an instrument");
    let _ = session.update();
    play(&mut handle, 69);
    let _settle = render(&mut session, 64);
    let full = peak(&render(&mut session, 16));
    assert!(full > 0.0, "the note sounds");
    session.set(handle.gain().to(Amplitude::new(0.5))).unwrap();
    let _settle = render(&mut session, 64);
    let half = peak(&render(&mut session, 16));
    // A sampled peak of 440 Hz at 48 kHz is low by at most
    // 1 - cos(pi * 440 / 48000), about 4.1e-4, in each window, so the ratio is
    // off by at most about 8.4e-4.
    assert!((half / full - 0.5).abs() < 1e-3, "full {full}, half {half}");
}

/// **A General MIDI rig of an `f64` session is silent before a note and renders it
/// finite and nonzero after.** The mix is the unit's dry pair, its chorus and its
/// reverb through the fader, all of the session's float.
#[test]
fn an_f64_general_midi_rig_is_silent_then_renders_a_finite_note() {
    let path = tutti_fixtures::asset("soundfonts/TimGM6mb.sf2");
    let font = Arc::new(SoundFont::open(path.as_path()).expect("the test font parses"));
    let unit = SoundFontUnit::<f64>::try_from(SoundFontConfig::new(font)).expect("builds");

    let mut session = session();
    let mut gm = session.add_general_midi(unit).expect("a SoundFont unit");
    for channel in 0..2 {
        session
            .graph()
            .set_sources(Root::output(channel), [OutPort::new(gm.out(), channel)])
            .unwrap();
    }
    let _ = session.update();
    assert_eq!(
        peak(&render(&mut session, 4)),
        0.0,
        "silent before the note"
    );

    play(gm.instrument_mut(), 69);
    let heard = render(&mut session, 128);
    assert!(heard.iter().all(|s| s.is_finite()), "finite output");
    assert!(peak(&heard) > 0.0, "the note sounds");

    session.set(gm.gain().to(Amplitude::SILENT)).unwrap();
    let _settle = render(&mut session, 4);
    assert_eq!(
        peak(&render(&mut session, 4)),
        0.0,
        "the fader's zero gain silences the whole mix"
    );
}
