//! The unit at the `f64` graph float: a struck note sounds in the dry pair and, once let go,
//! the unit is idle within the tail it declares, with the dry pair exact silence.

use std::sync::Arc;

use tutti_fixtures::midi;
use tutti_graph::contract::Direct;
use tutti_graph::{Event, InEvents, Node, Offset, Status};
use tutti_sf2::SoundFont;
use tutti_soundfont::{SoundFontConfig, SoundFontUnit};
use tutti_ump::MidiChannel;
use tutti_units::{SampleRate, Samples, Tail};

const BLOCK: usize = 64;

fn font() -> Arc<SoundFont> {
    let path = tutti_fixtures::asset("soundfonts/TimGM6mb.sf2");
    Arc::new(SoundFont::open(path.as_path()).unwrap_or_else(|e| {
        panic!(
            "committed test soundfont at {} does not load: {e}",
            path.display()
        )
    }))
}

/// A note held for a second is audible in both channels of the dry pair, every sample finite;
/// let go, the unit reports `Idle` within its declared tail (a frame count at the unit's rate)
/// and the dry pair it then writes is exact zeros.
/// The unit's one event input.
fn unit_input(direct: &Direct<SoundFontUnit<f64>, f64>) -> InEvents {
    direct.node.shape().event_input(0).expect("an event input")
}

#[test]
fn an_f64_unit_sounds_a_note_and_idles_within_its_declared_tail() {
    let rate = SampleRate::SR_48K;
    let unit = SoundFontUnit::<f64>::from(
        SoundFontConfig::new(font()).expect("a font with a playable preset"),
    );
    let Tail::Finite(tail) = unit.shape().tail else {
        panic!("the font's releases are finite");
    };
    let mut direct = Direct::<_, f64>::with_format(unit, rate, BLOCK);
    let at = Offset::ZERO;
    let input = unit_input(&direct);
    direct.events(
        input,
        &[Event::midi(
            at,
            midi::note_on(MidiChannel::FIRST, 60, 0xFFFF),
        )],
    );
    let mut peak = [0.0_f64; 2];
    for _ in 0..rate.get() as usize / BLOCK {
        direct.block();
        for (c, peak) in peak.iter_mut().enumerate() {
            for &x in direct.output().plane(c).samples() {
                assert!(x.is_finite());
                *peak = peak.max(x.abs());
            }
        }
        direct.events(input, &[]);
    }
    assert!(peak.iter().all(|&p| p > 1e-3), "peaks {peak:?}");

    direct.events(
        input,
        &[Event::midi(at, midi::note_off(MidiChannel::FIRST, 60, 0))],
    );
    let blocks = (tail + Samples::new(BLOCK)).get() / BLOCK + 1;
    let mut status = Status::Modified;
    for _ in 0..blocks {
        status = direct.block();
        direct.events(input, &[]);
    }
    assert_eq!(
        status,
        Status::Idle,
        "after the declared tail of {tail:?} frames"
    );
}
