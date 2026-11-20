//! A VBAP mix as a test graph is wired: a panner per source, summed by fan-in
//! onto the output channels, and (where the bed has an LFE) a bass-managed
//! send, because a panner never feeds the LFE.

use tutti_buffers::{Bed, ChannelLayout, Speaker};
use tutti_graph::{GraphBuilder, NodeKey};
use tutti_units::{Azimuth, Hz, Q, UnitParam};

use crate::{SvfFilterConfig, SvfFilterNode, SvfType, VbapError, VbapPannerNode};

/// Wire a panner over `bed` per `(source, bearing)` into `g`'s outputs: the
/// source's ports 0 and 1 feed the panner (at ear level), and each
/// panner channel `c` feeds output `c`, so the graph sums the panners by
/// fan-in.
///
/// # Errors
///
/// [`VbapError`] for a bed with no speakers to pan between ([`Bed::Mono`]).
pub fn vbap_mix(
    g: &mut GraphBuilder,
    bed: Bed,
    sources: &[(NodeKey, Azimuth)],
) -> Result<(), VbapError> {
    for &(source, azimuth) in sources {
        let panner = VbapPannerNode::new(bed)?;
        let (pan, params) = g.add_with_controls(panner);
        params.set(UnitParam::Azimuth, azimuth.get());
        g.connect(source, 0, pan, 0).connect(source, 1, pan, 1);
        for c in 0..bed.width().count() as usize {
            g.connect_output(pan, c, c);
        }
    }
    Ok(())
}

/// A bass-managed send for the LFE of `bed`: each source's port 0, summed by
/// fan-in onto a 120 Hz low-pass, into the output the bed's topology names as
/// its LFE. Nothing is added for a bed without one.
pub fn bass_send(g: &mut GraphBuilder, bed: Bed, sources: &[NodeKey]) {
    let Some(lfe) = bed.topology().index_of(Speaker::LowFrequency) else {
        return;
    };
    let (low_pass, _) = g.add_with_controls(SvfFilterNode::<f32>::from(
        SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
            .freq(Hz::new(120.0))
            .q(Q::new(0.707)),
    ));
    for &source in sources {
        g.connect(source, 0, low_pass, 0);
    }
    g.connect_output(low_pass, 0, lfe);
}
