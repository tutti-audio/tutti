//! The graph's sample-accuracy contract for the binaural
//! renderer, a graph node, on the audio-impulse path: an impulse at
//! frame `F` leaves at exactly `F + arrival + STEP` on every path
//! (direct, behind PDC, across a recompile, across ragged blocks — see
//! `tutti_graph::contract`).
//!
//! A node that declared no latency while every sample left a frame late would
//! leave PDC blind to the delay, and binaural tracks would arrive late against
//! the mix. The row pins
//! the declared figure (128, the panner's convolution step) *and* that the
//! output leaves on it, wherever in the step and in the graph's block the
//! impulse falls.

use tutti_fixtures::hrir::synthetic_hrir_sphere;
use tutti_graph::ParamNode;
use tutti_graph::contract::{Detect, Excite, Row, SAMPLE_RATE};
use tutti_graph::contract_tests;
use tutti_nodes::HrtfBinauralNode;
use tutti_nodes::testing::aim;
use tutti_units::Azimuth;
use tutti_units::Elevation;
use tutti_units::Samples;

/// The panner's convolution step (`tutti_dsp::HRTF_STEP`).
const LATENCY: usize = 128;

fn node() -> HrtfBinauralNode<f32> {
    let rate = SAMPLE_RATE.get() as u32;
    let bytes = synthetic_hrir_sphere(rate, 64);
    // Born at the contract's rate; the graph prepares it at that rate too.
    let node = hrtf::<f32>(&bytes).expect("the sphere parses");
    aim(&node.param_set(), Azimuth::new(90.0), Elevation::new(0.0));
    node
}

/// The impulse on the left input; each output is a fold of both inputs, so
/// either ear hears it on the same frame. Checked on each ear.
fn hrtf_row(ear: u16) -> Row {
    Row::new(
        &format!("HrtfBinauralNode (output {ear})"),
        || Box::new(node()),
        Excite::Impulse {
            port: 0,
            amplitude: 0.5,
        },
        Detect::Threshold(1e-4),
    )
    .output(ear)
    .with_expected_latency(Samples::new(LATENCY))
}

contract_tests!(audio hrtf_left => hrtf_row(0));
contract_tests!(audio hrtf_right => hrtf_row(1));

/// A binaural node over the sphere in `bytes`, prepared at `rate`.
fn hrtf<F: tutti_dsp::FftFloat>(
    bytes: &[u8],
) -> Result<tutti_nodes::HrtfBinauralNode<F>, tutti_dsp::HrtfBinauralError> {
    Ok(tutti_nodes::HrtfBinauralNode::from(
        tutti_nodes::HrtfBinauralConfig::new(std::sync::Arc::new(
            tutti_dsp::HrirSphere::from_bytes(bytes)?,
        )),
    ))
}
