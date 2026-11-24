//! A VBAP mix is a panner per source, wired to the output channels: the graph
//! sums the fan-in. These cases were the `build_vbap_mix` builder's tests and
//! pin what the panners do through that wiring.

use tutti_buffers::{Bed, ChannelLayout, MaxBlock};
use tutti_graph::{GraphBuilder, Prepare};
use tutti_nodes::testing::{Const, vbap_mix};
use tutti_nodes::{VbapError, VbapPannerConfig, VbapPannerNode};
use tutti_units::{Azimuth, SampleRate, Samples};

/// A panner per bearing (a DC `[1.0, 1.0]` source each), wired by
/// [`vbap_mix`]. Renders 49 blocks of 256 frames, past the panners' ~0.05 s
/// position smoother, and returns each output channel's energy over the last
/// block.
fn settled_energy(bed: Bed, bearings: &[f64]) -> Vec<f32> {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, bed.width());
    let sources: Vec<_> = bearings
        .iter()
        .map(|&az| (g.add(Const::frame(&[1.0, 1.0])), Azimuth::new(az)))
        .collect();
    vbap_mix(&mut g, bed, &sources).expect("a bed with speakers");
    let mut r = g
        .renderer(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(256)),
        ))
        .expect("builds");
    let mut last = Vec::new();
    for _ in 0..49 {
        last = r.render(256);
    }
    last.iter().map(|c| c.iter().map(|s| s * s).sum()).collect()
}

/// Two sources, one at a front and one at a rear speaker of a quad field: the
/// fan-in keeps each in its own channel, and the render does not collapse to
/// the front pair. Quad: ch0 FL 45, ch1 FR -45, ch2 RL 135, ch3 RR -135.
#[test]
fn fanned_in_panners_place_front_and_rear_sources() {
    let energy = settled_energy(Bed::Quad, &[45.0, 135.0]);
    let total: f32 = energy.iter().sum();
    assert!(total > 0.0, "silent");
    assert!(
        energy[0] > total * 0.2,
        "front-left should be lit ({energy:?})"
    );
    assert!(
        energy[2] > total * 0.2,
        "rear-left should be lit: no collapse to stereo ({energy:?})"
    );
}

/// A dead-centre source lands in the centre channel (2 of 5.1 file order), not
/// the surrounds, and a left-surround source lands in channel 4 with the LFE
/// channel (3) exactly silent: the panner never writes it.
#[test]
fn a_centre_source_lands_in_the_centre_and_never_in_the_lfe() {
    let energy = settled_energy(Bed::Surround51, &[0.0]);
    let total: f32 = energy.iter().sum();
    assert!(energy[2] > total * 0.4, "centre dominates ({energy:?})");
    assert!(
        energy[4] < total * 0.05 && energy[5] < total * 0.05,
        "surrounds stay quiet ({energy:?})"
    );
    assert_eq!(
        energy[3], 0.0,
        "the panner leaves the LFE silent ({energy:?})"
    );

    let surround = settled_energy(Bed::Surround51, &[110.0]);
    assert!(surround[4] > 0.0, "left surround is lit ({surround:?})");
    assert_eq!(surround[3], 0.0, "the LFE stays silent ({surround:?})");
}

/// Each bed with two or more speakers builds a panner of its width, up to the
/// widest; the one-speaker mono bed has nothing to pan between and is refused
/// by name rather than falling back.
#[test]
fn every_bed_builds_a_panner_of_its_width_and_mono_is_refused() {
    for bed in Bed::ALL {
        match VbapPannerNode::<f32>::try_from(VbapPannerConfig::new(bed.into())) {
            Ok(panner) => assert_eq!(panner.channels(), bed.width(), "{bed:?}"),
            Err(error) => {
                assert_eq!(bed, Bed::Mono);
                assert!(matches!(error, VbapError::Geometry(_)), "{error:?}");
            }
        }
    }
}

/// A bus that places no bed (unassigned channels, or an ambisonic set) has no speakers to pan
/// between and is refused by name, whatever its width.
#[test]
fn a_bus_that_is_not_a_bed_is_refused_by_name() {
    let foa = tutti_buffers::AmbisonicFormat::ambix(tutti_buffers::AmbisonicOrder::of(1));
    let unassigned =
        tutti_buffers::ChannelTopology::unassigned(tutti_buffers::ChannelLayout::from(6u16))
            .expect("six channels fit a bus");
    for bus in [unassigned, tutti_buffers::ChannelTopology::Ambisonic(foa)] {
        let error = VbapPannerNode::<f32>::try_from(VbapPannerConfig::new(bus)).unwrap_err();
        assert!(matches!(error, VbapError::NotABed), "{bus:?}: {error:?}");
    }
}
