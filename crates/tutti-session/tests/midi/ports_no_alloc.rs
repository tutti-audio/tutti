//! A session with an input port and an output port renders a block without
//! allocating, events flowing: a device's notes through its input node to a
//! routed synth, and the session's clock through the output port's node.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tutti_platform::Instant;

use crate::common::OpenPorts as _;
use crate::common::{MF, play};
use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_fixtures::midi;
use tutti_graph::{Cx, ForkByClone, Io, Node, Prepare, Shape, Status};
use tutti_midi_device::EndpointId;
use tutti_midi_hardware::test_support::{FakeBackend, persistent_endpoint};
use tutti_session::{MidiRouteRule, Session};
use tutti_ump::MidiChannel;

/// A stand-in synth that counts the events it takes and keeps none (the common
/// `Tap` logs them, which allocates).
#[derive(Clone)]
struct Sink(Arc<AtomicUsize>);

impl Node for Sink {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, io: Io<'_>) -> Status {
        self.0.fetch_add(io.events(0).len(), Ordering::Relaxed);
        Status::Modified
    }
    fn reset(&mut self) {}
}

#[test]
fn an_input_and_an_output_port_render_without_allocating() {
    let (backend, fake) = FakeBackend::build_with(
        vec![persistent_endpoint(1, "Keys")],
        vec![persistent_endpoint(10, "Synth")],
        usize::MAX,
    );
    let mut s = Session::builder()
        .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareConfig::new())
        .build()
        .expect("builds");
    let _ = s.update();
    let heard = Arc::new(AtomicUsize::new(0));
    let synth = s.graph().add(ForkByClone(Sink(Arc::clone(&heard))));
    let synth = synth.erase();
    s.midi()
        .route(MidiRouteRule::new().to(synth))
        .expect("routes");
    let list = s.midi().devices();
    let input = s.open_input(list[0].clone()).expect("opens");
    let output = s.open_output(list[1].clone()).expect("opens");
    let _ = s.update();
    s.midi_nodes()
        .unwrap()
        .sync_controls
        .messages()
        .store(tutti_midi_runtime::SyncMessages {
            clock: true,
            ..tutti_midi_runtime::SyncMessages::OFF
        });
    play(&s);
    let mut buf = vec![0.0f32; 512 * 2];
    let mut block = |s: &Session| {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
    };
    // Warm up outside the gate: the first blocks publish the clock the devices'
    // threads place arrivals by.
    for _ in 0..4 {
        block(&s);
        let _ = s.update();
    }

    // The device's thread pushes outside the gate (the fake's own locking is its
    // own); the blocks that read the events are the gate's.
    for n in 0..200u8 {
        let note = midi::note_on(MidiChannel::of(n % 4), 40 + n % 40, MF);
        assert!(fake.send_input(EndpointId::from_raw(1), note, Instant::now()));
    }
    for _ in 0..200 {
        block(&s);
    }
    let _ = s.update();
    assert!(
        heard.load(Ordering::Relaxed) >= 200,
        "the device's notes reached the routed synth through the input node: {}",
        heard.load(Ordering::Relaxed)
    );
    assert!(
        !fake.take_sent(EndpointId::from_raw(10)).is_empty(),
        "the output node forwarded the clock to the device"
    );
    assert!(input.node() != output.node());
}
