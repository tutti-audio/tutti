//! The session's hardware MIDI out on the system clock: with no audio device,
//! the timer's blocks carry the MIDI clock to the output, and the output's
//! terms are complete from the first block (the audio output's latency is zero,
//! not unknown).

use crate::common::Wire;
use std::sync::Arc;

use tutti_buffers::MaxBlock;
use tutti_session::Session;
use tutti_ump::{Message, MidiGroup, System, Ump};
use tutti_units::{SampleRate, Samples};

/// **A system session sends its MIDI clock to a sink with no device.**
/// The clock node is enabled, the transport rolls, and the timer's blocks
/// carry the 24-PPQN ticks to the output through `Phase::MidiOut`, timed by
/// terms whose audio output is exactly zero.
#[test]
fn the_midi_clock_reaches_the_output_on_the_timers_blocks() {
    let wire = Wire::default();
    let mut system = Session::builder()
        .sample_rate(SampleRate::SR_48K)
        .max_block(MaxBlock::of(Samples::new(480)))
        .system_clock()
        .build()
        .expect("builds");
    let s = &mut system;
    let _ = s.update();
    s.midi_nodes()
        .unwrap()
        .sync_controls
        .messages()
        .store(tutti_midi_runtime::SyncMessages {
            clock: true,
            ..tutti_midi_runtime::SyncMessages::OFF
        });
    let _sink = s.open_midi_sink(Arc::new(wire.clone())).open();
    let _ = s.update();
    s.play().expect("room");

    let tick = Ump::from(Message::System(MidiGroup::FIRST, System::TimingClock));
    tutti_fixtures::wait_until(
        "the clock's ticks to arrive",
        tutti_fixtures::HANG_GUARD,
        || {
            let _ = s.update();
            wire.events().iter().filter(|(_, ump)| *ump == tick).count() >= 3
        },
    );
    let terms = s
        .midi()
        .output_terms()
        .expect("the system clock times the out");
    assert_eq!(terms.audio_output, Some(tutti_units::Seconds::new(0.0)));
    assert!(terms.is_complete());
}
