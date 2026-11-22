use super::*;
use std::sync::Arc;
use tutti_graph::Harmony;

/// **Lanes a block still held when a publish replaced them are freed by the
/// next `reclaim`**, with no further publish.
#[test]
fn reclaim_frees_lanes_a_block_held_across_their_replacement() {
    let c_major = Harmony::chord(60, 60, 0b1001_0001);
    let mut node = HarmonyNode::new(
        HarmonyChanges::new([TimedHarmony::new(Beat::new(0.0), c_major)]).expect("one change"),
    );
    let controls = HarmonyControls {
        cell: node.cell.clone(),
        serial: Arc::default(),
    };
    let gone = Arc::downgrade(&node.cell.current());
    {
        // A block in progress across the replacement.
        let _held = node.lanes.lend();
        controls.set(HarmonyChanges::default());
    }
    assert!(gone.upgrade().is_some(), "retired, pinned until a scan");
    assert_eq!(tutti_rt::Reclaim::reclaim(&controls), 0);
    assert!(gone.upgrade().is_none(), "freed by the reclaim");
}
