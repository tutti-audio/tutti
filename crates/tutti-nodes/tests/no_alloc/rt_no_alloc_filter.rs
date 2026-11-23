//! The SVF with its param ports fed per frame allocates nothing: the path that designs
//! coefficients on the audio thread and cuts a block at grid points and corners. The held and
//! cell-moving filters are rows of the table.

use tutti_graph::contract::Direct;
use tutti_nodes::{SvfFilterConfig, SvfFilterNode, SvfType};
use tutti_units::{Hz, Q, SampleRate};

/// The SVF with its cutoff and Q ports fed per frame, at widths 1, 2, 6, 9 and 16: every
/// grid point designs and retargets, every block is cut, and the bank runs its blended
/// and held frames in one lane group and in several. Through `Direct`, which feeds a
/// node's param ports (it copies the control signal, so the same 64 frames play each block).
#[test]
fn an_svf_with_its_ports_fed_is_allocation_free() {
    for width in [1u16, 2, 6, 9, 16] {
        let node = SvfFilterNode::<f32>::from(
            SvfFilterConfig::new(width, SvfType::Bell)
                .freq(Hz::new(1_000.0))
                .q(Q::new(1.0)),
        );
        let mut rig = Direct::new(node, SampleRate::SR_48K, 64);
        for c in rig.inputs_mut().iter_mut() {
            let c = c.into_samples_mut();
            for (i, x) in c.iter_mut().enumerate() {
                *x = if (i / 16) % 2 == 0 { 0.5 } else { -0.5 };
            }
        }
        let tau = std::f32::consts::TAU / 64.0;
        let cutoff: Vec<f32> = (0..64)
            .map(|i| 1_000.0 + 800.0 * (tau * i as f32).sin())
            .collect();
        let q: Vec<f32> = (0..64)
            .map(|i| 1.0 + 0.5 * (tau * 3.0 * i as f32).sin())
            .collect();
        rig.feed(0, Some(&cutoff));
        rig.feed(1, Some(&q));
        for _ in 0..32 {
            rig.block();
        }
        assert!(
            rig.output()
                .plane(0)
                .samples()
                .iter()
                .any(|s| s.abs() > 1e-6),
            "the SVF rendered silence; the gate would walk no DSP"
        );
        for _ in 0..256 {
            rig.block();
        }
    }
}
