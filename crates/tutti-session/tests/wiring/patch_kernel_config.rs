//! A kernel node declared through `Session::patch`: its config's values are written to
//! the running node, which is kept, and its structure swaps the node.

use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_nodes::{DelayConfig, DelayControls, DelayLayout, DelaySync, MaxDelay};
use tutti_session::{NodeDecl, Patched, Session};
use tutti_units::{Feedback, SampleRate, Seconds};

const MAX: MaxDelay = MaxDelay::of(Seconds::new(1.0));

fn session() -> Session {
    Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .outputs(ChannelLayout::MONO)
        .sample_rate(SampleRate::SR_48K)
        .block_dates(tutti_session::BlockDates::Frames)
        .build()
        .expect("builds")
}

fn delay(feedback: f64, sync: DelaySync) -> DelayConfig {
    DelayConfig::new(DelayLayout::new(ChannelTopology::MONO, MAX)).controls(DelayControls {
        sync,
        time: Seconds::new(0.25),
        feedback: Feedback::new(feedback),
        cross_feedback: Feedback::new(0.0),
    })
}

fn declare(s: &mut Session, config: DelayConfig) -> Patched<&'static str> {
    s.patch(NodeDecl::new("delay", config)).expect("patches")
}

/// **A delay declared again with new values keeps its node**: the feedback and the
/// kinded sync are written to it, nothing is rebuilt, and the key is the same.
#[test]
fn a_delays_changed_values_are_written_to_the_kept_node() {
    let mut s = session();
    declare(&mut s, delay(0.25, DelaySync::Free));
    let key = s.key::<DelayConfig>(&"delay").expect("declared");

    let patched = declare(&mut s, delay(0.5, DelaySync::Synced));

    assert!(patched.rebuilt.is_empty(), "values are no rebuild");
    assert!(patched.readded.is_empty() && patched.unconverged.is_empty());
    assert_eq!(s.key::<DelayConfig>(&"delay").unwrap().erase(), key.erase());
    assert_eq!(s.get(key.feedback()).unwrap(), Feedback::new(0.5));
    assert_eq!(s.get(key.sync()).unwrap(), DelaySync::Synced);
}

/// **A delay declared again with another layout swaps its node** under the same key.
#[test]
fn a_delays_changed_layout_rebuilds_it() {
    let mut s = session();
    declare(&mut s, delay(0.25, DelaySync::Free));

    let wider = DelayConfig::new(DelayLayout::new(
        ChannelTopology::MONO,
        MaxDelay::of(Seconds::new(2.0)),
    ));
    let patched = declare(&mut s, wider);

    assert_eq!(patched.rebuilt, ["delay"]);
}
