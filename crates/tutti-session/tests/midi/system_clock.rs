//! The session's hardware MIDI out on the system clock: with no audio device,
//! the timer's blocks carry the MIDI clock to the output, and the output's
//! terms are complete from the first block (the audio output's latency is zero,
//! not unknown).

use std::sync::Arc;
use std::time::Duration;
use tutti_platform::Instant;
use tutti_rt::Mutex;

use tutti_buffers::MaxBlock;
use tutti_graph::UmpAt;
use tutti_midi_runtime::{MidiOut, Outgoing, Taken};
use tutti_session::Session;
use tutti_ump::{Message, MidiGroup, System, Ump};
use tutti_units::{SampleRate, Samples};

#[derive(Clone, Default)]
struct Wire(Arc<Mutex<Vec<UmpAt>>>);

impl MidiOut for Wire {
    fn queue_timed(&self, events: &[Outgoing]) -> Taken {
        self.0.lock().extend(events.iter().map(|e| (e.at, e.ump)));
        Taken::within(events, events.len())
    }
}

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
    let limit = Instant::now() + Duration::from_secs(10);
    loop {
        let _ = s.update();
        let ticks = wire.0.lock().iter().filter(|(_, ump)| *ump == tick).count();
        if ticks >= 3 {
            break;
        }
        assert!(Instant::now() < limit, "the clock's ticks never arrived");
        std::thread::sleep(Duration::from_millis(5));
    }
    let terms = s
        .midi()
        .output_terms()
        .expect("the system clock times the out");
    assert_eq!(terms.audio_output, Some(tutti_units::Seconds::new(0.0)));
    assert!(terms.is_complete());
}
