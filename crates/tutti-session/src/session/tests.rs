use std::sync::Arc;

use tutti_music::{MeterChange, MeterMap, TimeSignature};
use tutti_transport::MeterSlot;
use tutti_types::ChannelLayout;
use tutti_units::{Beat, SampleRate};

use crate::Session;

/// **The update's `Reclaim` phase frees what a block held across a
/// replacement**, in a cell nothing publishes to again: the meter a
/// following plugin slot (a `MeterSlot`, as a plugin node holds) read when
/// the project meter changed. A held `Loan` stands in for a block in
/// progress across the publish.
///
/// The nodes' cells are the graph's table, pinned by
/// `the_update_sweeps_each_nodes_reclaimer_while_it_is_in_the_graph`; the
/// modulation lanes by `tests/modulation/modulation.rs`, the hardware ports by
/// `tests/midi/midi.rs` (`midi-hardware`), a plugin's meter by `plugin.rs`.
#[test]
fn an_update_frees_what_a_block_held_across_a_replacement() {
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .build_detached(SampleRate::SR_48K)
        .expect("builds");
    let _ = s.update();

    let slot = MeterSlot::following_meter(Some(s.transport.settings().project_meter().clone()));
    let meter = s.transport_handle().meter();
    let old = Arc::new(MeterMap::new([MeterChange::new(
        Beat(0.0),
        TimeSignature::of(3, 4),
    )]));
    let gone = Arc::downgrade(&old);
    s.set(meter.to(old)).unwrap();

    let mut node = slot.reader();
    {
        // A block in progress across the publish.
        let _held = node.lend();
        s.set(meter.to(MeterMap::default())).unwrap();
        // Still held: this update's own reclaim leaves it.
        let _ = s.update();
    }
    assert!(gone.upgrade().is_some(), "the meter is retired");

    let _ = s.update();
    assert!(gone.upgrade().is_none(), "the meter is freed");
}
