use super::*;
use tutti_types::SampleRate;

struct Silent;
impl MidiOut for Silent {
    fn queue_timed(&self, events: &[tutti_midi_runtime::Outgoing]) -> tutti_midi_runtime::Taken {
        tutti_midi_runtime::Taken::within(events, events.len())
    }
}

/// **A sink's tap follows the graph to another rate at the next update.**
/// `Graph::set_sample_rate` moves the graph's clock to another scale and a tap
/// keeps the rate it was opened at; the `MidiOut` phase reopens the sink's, so
/// the frames it carries are read on the scale they are on.
#[test]
fn a_re_rated_graph_reopens_the_sinks_tap_at_the_next_update() {
    let mut s = Session::builder()
        .sample_rate(SampleRate::SR_48K)
        .build()
        .expect("builds");
    s.pool(midi_sink("out", Arc::new(Silent)))
        .expect("declares");
    let _ = s.update();
    assert_eq!(s.midi_state.sinks.ports[0].tap.rate(), SampleRate::SR_48K);
    s.graph().set_sample_rate(SampleRate::SR_44K1);
    let _ = s.update();
    assert_eq!(s.midi_state.sinks.ports[0].tap.rate(), SampleRate::SR_44K1);
}
