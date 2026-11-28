//! A memory source's config is a `NodeConfig`: its initials are the gain the built node
//! starts at, a change of gain keeps the structure, and a change of anything else does not.

use std::sync::Arc;

use tutti_buffers::{ChannelLayout, Interleaved, Wave};
use tutti_graph::{HostValues, IntoNode, NodeConfig};
use tutti_sampler::testing::loop_frames;
use tutti_sampler::{MemorySourceConfig, VoiceWindow};
use tutti_units::{Amplitude, Beat, PlaybackRate, SampleRate, UnitAddr, UnitParam};

/// Four stereo frames: a new allocation each call, so two calls are two identities.
fn wave() -> Arc<Wave> {
    let mut wave = Wave::new(2, SampleRate::SR_48K);
    for _ in 0..4 {
        wave.append(Interleaved::new(&[0.25, 0.25], wave.layout()).unwrap())
            .unwrap();
    }
    Arc::new(wave)
}

fn config(wave: &Arc<Wave>) -> MemorySourceConfig<f32> {
    MemorySourceConfig::new(Arc::clone(wave), ChannelLayout::STEREO).expect("a stereo wave places")
}

#[test]
fn a_source_config_lists_its_gain_and_compares_its_structure() {
    let shared = wave();
    let base = config(&shared).gain(Amplitude::new(0.75));
    let values = config(&shared).gain(Amplitude::new(0.5));

    // The initials are what the built node starts at.
    for config in [&base, &values] {
        let initials = config.initials();
        let (_, controls) = config.build(&HostValues::new()).into_node();
        let volume = UnitAddr::first(UnitParam::Volume);
        assert_eq!(initials.params().len(), 1, "the gain is the one param");
        assert_eq!(initials.params()[0].0, volume);
        assert_eq!(controls.params().count(), 1);
        assert_eq!(controls.get(volume), Some(initials.params()[0].1));
    }
    assert_eq!(base.initials().params()[0].1, 0.75);
    assert_eq!(values.initials().params()[0].1, 0.5);

    // A change of gain is the same node, written.
    assert!(base.same_structure(&values) && values.same_structure(&base));

    // Anything else is a new node.
    let structures = [
        ("speed", base.clone().speed(PlaybackRate::of(2.0))),
        ("loop", base.clone().looped(loop_frames(1, 3, 0))),
        (
            "window",
            base.clone().window(VoiceWindow::from(Beat::new(1.0))),
        ),
        (
            "channels",
            MemorySourceConfig::new(Arc::clone(&shared), ChannelLayout::MONO)
                .expect("a stereo wave folds to mono")
                .gain(Amplitude::new(0.75)),
        ),
        (
            "the same wave behind another Arc",
            config(&wave()).gain(Amplitude::new(0.75)),
        ),
    ];
    for (what, other) in &structures {
        assert!(
            !base.same_structure(other) && !other.same_structure(&base),
            "{what} is a new node"
        );
    }
}
