//! The graph's sample-accuracy contract for the synth as a graph
//! node: a note-on at offset `k` of the event input sounds from frame `k`
//! (plus the compiled arrival), exactly, on every path the harness runs —
//! direct, behind PDC, through an event fan-in, across recompiles, in ragged
//! blocks, and delivered by a scheduled `At::Frame` or `At::Beat` command.
//!
//! A square oscillator with an instant attack. Its note's first sample is
//! exactly zero (the voice's lanes ramp from zero), so the response is found
//! one frame after its onset: a constant lead of one frame
//! (`Row::with_lead`), the same on every path.

use tutti_fixtures::midi;
use tutti_graph::EventKind;
use tutti_graph::contract::{Detect, Excite, Row};
use tutti_polysynth::{Adsr, OscillatorType, PolySynth, SynthConfig};
use tutti_ump::MidiChannel;
use tutti_units::{Normalized, Samples};

fn synth_row() -> Row {
    let note = midi::note_on(MidiChannel::FIRST, 69, 0xFFFF);
    Row::new(
        "PolySynth",
        || {
            Box::new(PolySynth::<f32>::from(
                SynthConfig::new()
                    .oscillator(OscillatorType::Square {
                        pulse_width: Normalized::of(0.5),
                    })
                    .envelope(Adsr {
                        attack: tutti_units::Seconds::new(0.0),
                        ..Default::default()
                    }),
            ))
        },
        Excite::Event {
            port: 0,
            kind: EventKind::Midi(note),
        },
        Detect::Threshold(1e-6),
    )
    .with_lead(Samples::new(1))
}

tutti_graph::contract_tests!(event polysynth => synth_row());
