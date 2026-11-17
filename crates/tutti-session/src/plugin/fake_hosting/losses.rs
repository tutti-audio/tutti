//! A plugin's cumulative [`Plugin::losses`](crate::Plugin::losses): each kind
//! of loss it sums, each exactly, and what a swap of the plugin or a rebuild of
//! its node does to the count. The counters are driven as the plugin's node and
//! handle drive them: the delivery's queue, the bridge's MIDI cell and the
//! control's dropped edits.

use std::sync::Arc;

use tutti_graph::{ForkByClone, Losses, NodeKey};
use tutti_nodes::testing::Const;
use tutti_plugin_host::{ParamDelivery, PluginControls, PluginHandle};
use tutti_plugin_types::PluginTail;
use tutti_plugin_types::fake::FakePlugin;
use tutti_types::ParamId;
use tutti_types::{ChannelLayout, SampleRate, Samples};

use super::{Inert, host};
use crate::{DetachedSession, Session};

/// A plugin over the fake backend, and the control whose dropped edits a test
/// sets.
fn handle() -> (PluginHandle, FakePlugin) {
    let (fake, control) = FakePlugin::new();
    let control: Arc<dyn tutti_plugin_host::MainThreadControl> = Arc::new(control);
    let handle = PluginHandle::from_backend(
        Arc::new(Inert::default()),
        Some(control),
        Default::default(),
        Default::default(),
        Default::default(),
    );
    (handle, fake)
}

fn plugin() -> tutti_plugin::catalog::PluginRef {
    tutti_plugin::catalog::PluginRef::file("fake.clap").expect("a plugin file")
}

/// What a test drives of one node: its controls' counters.
struct Counters {
    controls: PluginControls,
    /// The bridge thread's count of the MIDI events it did not send.
    bridge_midi: tutti_rt::Tally<u64>,
    delivery: ParamDelivery,
    /// The points a parameter's queue holds in the chunk the delivery is prepared for.
    queue_room: usize,
}

impl Counters {
    fn new() -> Self {
        let (controls, _) =
            PluginControls::new(Samples(0), PluginTail::default(), SampleRate::SR_48K);
        let delivery_counters = controls.delivery_counters();
        let bridge_midi = tutti_rt::tally();
        delivery_counters.attach_bridge_midi(bridge_midi.clone());
        let mut delivery = ParamDelivery::new([ParamId::new(0)], delivery_counters);
        let queue_room = delivery.prepare(64);
        delivery.begin();
        Self {
            controls,
            bridge_midi,
            delivery,
            queue_room,
        }
    }

    /// `n` MIDI events that never reached the plugin.
    fn lose_midi(&self, n: u64) {
        self.bridge_midi.writer().add(n);
    }

    /// One grid point lost to a full queue, one point that replaced the last
    /// of it, and one for a parameter with no slot.
    fn lose_one_point_of_each_kind(&mut self) {
        let value = |k: usize| if k.is_multiple_of(2) { 0.25 } else { 0.75 };
        let slots = self.delivery.room();
        // A queue of corners is full at its room.
        let queue = self.queue_room;
        for k in 0..queue {
            assert!(self.delivery.point(ParamId::new(0), k as i32, value(k)));
        }
        // A grid point at full is dropped.
        assert!(
            self.delivery
                .grid_point(ParamId::new(0), 1000, value(queue))
        );
        // A corner at full, in a queue of corners, replaces the last point.
        assert!(self.delivery.point(ParamId::new(0), 2000, value(queue)));
        // Every spare slot learns an address; the next one has none.
        for address in 1..slots as u32 {
            assert!(self.delivery.point(ParamId::new(address), 0, 0.5));
        }
        assert!(!self.delivery.point(ParamId::new(9999), 0, 0.5));
    }
}

/// A detached session with the fake plugin at a node, given a node's counters.
fn hosted() -> (DetachedSession, NodeKey, Counters, FakePlugin) {
    let (handle, fake) = handle();
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    let (node, _) = s.graph().add(ForkByClone(Const::mono(0.0)));
    host(&mut s, node, handle, plugin());
    s.commit();
    let node = node.erase();
    let counters = Counters::new();
    s.graph
        .plugins
        .get_mut(&node)
        .expect("hosted")
        .set_node((*counters.controls).clone());
    (s, node, counters, fake)
}

fn losses(s: &DetachedSession, node: NodeKey) -> Losses {
    s.plugin_view(node).expect("hosted").losses()
}

/// **Each kind of loss lands in `losses()`, exactly.** A plugin that lost no
/// event reads none; then 5 MIDI events (`lost`), 3 parameter edits the host
/// did not hear (`lost`), a grid point a full queue dropped (`lost`), a point
/// that replaced the last of a full queue (`evicted`) and a point for a
/// parameter with no slot (`refused`) read as lost 9, evicted 1, refused 1.
#[test]
fn each_kind_of_loss_lands_in_the_plugins_losses() {
    let (s, node, mut counters, fake) = hosted();
    assert_eq!(losses(&s, node), Losses::NONE, "nothing lost yet");

    counters.lose_midi(5);
    assert_eq!(losses(&s, node), Losses::lost(5), "MIDI past the plugin");
    fake.drop_reported_edits(3);
    assert_eq!(
        losses(&s, node),
        Losses::lost(5 + 3),
        "edits the host did not hear"
    );
    counters.lose_one_point_of_each_kind();
    assert_eq!(
        losses(&s, node),
        Losses::lost(5 + 3 + 1) + Losses::evicted(1) + Losses::refused(1)
    );
}

/// **The count only grows across a rebuild of the node and a swap of the
/// plugin at the same key.** A node rebuilt for new ports starts its counters
/// at zero, and a swap brings a handle that has dropped no edits: what the
/// outgoing ones counted stays, and what the incoming ones count adds to it.
#[test]
fn losses_keep_what_a_rebuilt_node_and_a_swapped_plugin_lost() {
    let (mut s, node, counters, fake) = hosted();
    counters.lose_midi(5);
    fake.drop_reported_edits(3);
    let before = Losses::lost(5 + 3);
    assert_eq!(losses(&s, node), before);

    // The node is rebuilt: its controls are new, and count from zero.
    let rebuilt = Counters::new();
    s.graph
        .plugins
        .get_mut(&node)
        .unwrap()
        .set_node((*rebuilt.controls).clone());
    assert_eq!(losses(&s, node), before, "the rebuild lost nothing");
    rebuilt.lose_midi(2);
    assert_eq!(losses(&s, node), before + Losses::lost(2));

    // The plugin is swapped: a new handle and new controls.
    let (incoming, incoming_fake) = handle();
    let swapped = Counters::new();
    let outgoing = s.graph.plugins.get_mut(&node).unwrap().replace(
        incoming,
        swapped.controls.clone(),
        plugin(),
    );
    drop(outgoing);
    assert_eq!(
        losses(&s, node),
        before + Losses::lost(2),
        "the swap lost nothing, and the outgoing plugin's count stays"
    );
    incoming_fake.drop_reported_edits(4);
    swapped.lose_midi(1);
    assert_eq!(
        losses(&s, node),
        before + Losses::lost(2 + 4 + 1),
        "the incoming plugin's losses add"
    );
}
