//! Both panners fork like every param node: a fork starts from the values
//! last set through the node's `ParamSet` and shares no cell with the live
//! node, in either direction (`assert_param_fork`).

use tutti_fixtures::hrir::synthetic_hrir_sphere;
use tutti_graph::contract::{SAMPLE_RATE, assert_param_fork};
use tutti_graph::{Node, ParamNode};
use tutti_nodes::HrtfBinauralNode;

fn hrtf() -> HrtfBinauralNode<f32> {
    let rate = SAMPLE_RATE.get() as u32;
    let bytes = synthetic_hrir_sphere(rate, 64);
    hrtf_from::<f32>(&bytes).expect("the sphere parses")
}

#[test]
fn vbap_forks_from_its_params() {
    assert_param_fork(vbap::<f32>(tutti_buffers::Bed::Surround714));
}

#[test]
fn hrtf_forks_from_its_params() {
    assert_param_fork(hrtf());
}

/// Every param a panner lets a control signal drive is one its `ParamSet` addresses,
/// so a host reaches the cell a control signal rides on.
#[test]
fn a_panner_declares_every_param_it_reads() {
    let vbap = vbap::<f32>(tutti_buffers::Bed::Stereo);
    assert_eq!(vbap.shape().params.as_slice().len(), 4);
    for p in vbap.shape().params.as_slice() {
        assert!(
            vbap.param_set().get(*p).is_some(),
            "VBAP: {p:?} is not in its ParamSet"
        );
    }
    let hrtf = hrtf();
    assert_eq!(hrtf.shape().params.as_slice().len(), 3);
    for p in hrtf.shape().params.as_slice() {
        assert!(
            hrtf.param_set().get(*p).is_some(),
            "HRTF: {p:?} is not in its ParamSet"
        );
    }
}

/// A panner over `bed`.
fn vbap<F: tutti_buffers::Float>(bed: tutti_buffers::Bed) -> tutti_nodes::VbapPannerNode<F> {
    tutti_nodes::VbapPannerNode::try_from(tutti_nodes::VbapPannerConfig::new(bed))
        .expect("a bed with speakers to pan between")
}

/// A binaural node over the sphere in `bytes`, prepared at `rate`.
fn hrtf_from<F: tutti_dsp::FftFloat>(
    bytes: &[u8],
) -> Result<tutti_nodes::HrtfBinauralNode<F>, tutti_dsp::HrtfBinauralError> {
    Ok(tutti_nodes::HrtfBinauralNode::from(
        tutti_nodes::HrtfBinauralConfig::new(std::sync::Arc::new(
            tutti_dsp::HrirSphere::from_bytes(bytes)?,
        )),
    ))
}
