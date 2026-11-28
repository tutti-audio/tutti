//! A memory source's and a voice node's configs are `NodeConfig`s: their initials are the
//! values the built node starts at, a change of a value keeps the structure, and a change of
//! anything else does not.

use std::sync::Arc;

use tutti_buffers::{ChannelLayout, Interleaved, Wave};
use tutti_graph::{HostValues, Initials, IntoNode, NodeConfig, PublishedKind};
use tutti_sampler::testing::{free_config, loop_frames, placed_config, wait_for_open};
use tutti_sampler::{
    DirectionKind, DiskStreamer, DiskVoice, Free, LoopingKind, Manual, MemorySourceConfig, Placed,
    PlayDirection, SpeedKind, StreamHandle, StreamStart, VoiceNodeConfig, VoiceNodeControls,
    VoiceWindow, WindowKind,
};
use tutti_units::{
    Amplitude, Beat, Cents, PlaybackRate, SampleRate, StretchFactor, UnitAddr, UnitParam,
};

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

fn volume() -> UnitAddr {
    UnitAddr::first(UnitParam::Volume)
}

fn at(beat: f64) -> VoiceWindow {
    VoiceWindow::from(Beat::new(beat))
}

/// `source` with every value a source config carries moved: gain, speed and loop.
fn moved_source(source: MemorySourceConfig<f32>) -> MemorySourceConfig<f32> {
    source
        .gain(Amplitude::new(0.5))
        .speed(PlaybackRate::of(2.0))
        .looped(loop_frames(1, 3, 0))
}

fn free_moved(source: MemorySourceConfig<f32>) -> VoiceNodeConfig<f32, Free> {
    VoiceNodeConfig::free(moved_source(source))
        .expect("free")
        .direction(PlayDirection::Reverse)
}

fn placed_moved(source: MemorySourceConfig<f32>) -> VoiceNodeConfig<f32, Placed> {
    VoiceNodeConfig::placed(moved_source(source))
        .expect("placed")
        .direction(PlayDirection::Reverse)
}

/// The start `initials` carry for the published kind `K`.
fn initial<K: PublishedKind>(initials: &Initials) -> Option<K::Value> {
    initials
        .published_values()
        .iter()
        .find_map(|p| p.value::<K>())
}

/// The initials are what the built node starts at: the gain through the controls'
/// params, and each published value through the controls' cells.
fn assert_initials_are_the_built_nodes(
    what: &str,
    initials: &Initials,
    controls: &VoiceNodeControls<f32>,
    window: Option<VoiceWindow>,
    playing: Option<bool>,
) {
    assert_eq!(
        initials.params().len(),
        1,
        "{what}: the gain is the one param"
    );
    assert_eq!(initials.params()[0].0, volume(), "{what}");
    assert_eq!(
        controls.params().get(volume()),
        Some(initials.params()[0].1),
        "{what}"
    );
    assert_eq!(
        initials.published_values().len(),
        4,
        "{what}: loop, direction, speed and one of window or steering"
    );
    assert_eq!(
        initial::<LoopingKind>(initials),
        Some(*controls.looping_cell().current()),
        "{what}: loop"
    );
    assert_eq!(
        initial::<DirectionKind>(initials),
        Some(*controls.direction_cell().current()),
        "{what}: direction"
    );
    assert_eq!(
        initial::<SpeedKind>(initials),
        Some(*controls.speed_cell().current()),
        "{what}: speed"
    );
    assert_eq!(initial::<WindowKind>(initials), window, "{what}: window");
    assert_eq!(
        controls.window_cell().map(|cell| *cell.current()),
        window,
        "{what}: the window cell"
    );
    assert_eq!(
        controls.steering().map(|s| s.is_playing()),
        playing,
        "{what}: steering exists exactly for a free voice, at its start"
    );
    assert_eq!(
        window.is_some(),
        controls.steering().is_none(),
        "{what}: a voice has a window or a steering, never both or neither"
    );
}

/// A file's stream: its handle and the live voice over it, at `beat`.
fn stream_over(
    path: &std::path::Path,
    beat: f64,
) -> (
    DiskStreamer<Manual<f32>, f32>,
    StreamHandle<f32>,
    DiskVoice<f32>,
) {
    let mut streamer =
        DiskStreamer::manual(SampleRate::SR_48K, Default::default()).expect("a streamer builds");
    let opening = streamer.open_stream(path, StreamStart::HEAD);
    let (stream, reader) = wait_for_open(&mut streamer, opening).expect("the file opens");
    (streamer, stream, DiskVoice::from_reader(reader, at(beat)))
}

#[test]
fn a_free_voice_config_lists_its_values_and_compares_its_structure() {
    let shared = wave();
    let base = free_moved(free_config(Arc::clone(&shared)));
    let values = VoiceNodeConfig::free(free_config(Arc::clone(&shared)))
        .expect("free")
        .stopped();

    assert_initials_are_the_built_nodes(
        "base",
        &base.initials(),
        &base.build(&HostValues::new()).into_node().1,
        None,
        Some(true),
    );
    assert_initials_are_the_built_nodes(
        "values",
        &values.initials(),
        &values.build(&HostValues::new()).into_node().1,
        None,
        Some(false),
    );
    assert_eq!(base.initials().params()[0].1, 0.5);
    assert_eq!(values.initials().params()[0].1, 1.0);
    assert_ne!(
        base.initials(),
        values.initials(),
        "every value moves an initial"
    );

    // Every value changes and the structure holds, both ways.
    assert!(base.same_structure(&values) && values.same_structure(&base));
    for (what, other) in [
        ("speed", base.clone().speed(PlaybackRate::UNITY)),
        ("loop", values.clone().looped(loop_frames(2, 4, 0))),
        (
            "direction",
            values.clone().direction(PlayDirection::Reverse),
        ),
    ] {
        assert!(
            base.same_structure(&other) && other.same_structure(&base),
            "{what} is a value"
        );
        assert_ne!(base.initials(), other.initials(), "{what} moves an initial");
    }

    // Anything else is a new node.
    let structures = [
        ("another wave", free_moved(free_config(wave()))),
        (
            "width",
            free_moved(
                MemorySourceConfig::new(Arc::clone(&shared), ChannelLayout::MONO)
                    .expect("a stereo wave folds to mono"),
            ),
        ),
        (
            "stretch",
            free_moved(free_config(Arc::clone(&shared))).stretch(StretchFactor::new(2.0)),
        ),
        (
            "pitch",
            free_moved(free_config(Arc::clone(&shared))).pitch(Cents::new(100.0)),
        ),
    ];
    for (what, other) in &structures {
        assert!(
            !base.same_structure(other) && !other.same_structure(&base),
            "{what} is a new node"
        );
    }
}

#[test]
fn a_placed_voice_config_lists_its_values_and_compares_its_structure() {
    let shared = wave();
    let base = placed_moved(placed_config(Arc::clone(&shared), at(1.0)));
    let values = VoiceNodeConfig::placed(placed_config(Arc::clone(&shared), at(1.0)))
        .expect("placed")
        .window(at(3.0));

    assert_initials_are_the_built_nodes(
        "base",
        &base.initials(),
        &base.build(&HostValues::new()).into_node().1,
        Some(at(1.0)),
        None,
    );
    assert_initials_are_the_built_nodes(
        "values",
        &values.initials(),
        &values.build(&HostValues::new()).into_node().1,
        Some(at(3.0)),
        None,
    );

    // The window is a value: a moved one is the same node.
    assert!(base.same_structure(&values) && values.same_structure(&base));
    assert_ne!(base.initials(), values.initials());

    let structures = [
        ("another wave", placed_moved(placed_config(wave(), at(1.0)))),
        (
            "width",
            placed_moved(
                MemorySourceConfig::new(Arc::clone(&shared), ChannelLayout::MONO)
                    .expect("a stereo wave folds to mono")
                    .window(at(1.0)),
            ),
        ),
        (
            "stretch",
            placed_moved(placed_config(Arc::clone(&shared), at(1.0)))
                .stretch(StretchFactor::new(2.0)),
        ),
    ];
    for (what, other) in &structures {
        assert!(
            !base.same_structure(other) && !other.same_structure(&base),
            "{what} is a new node"
        );
    }
}

#[test]
fn a_streamed_voice_config_lists_its_values_and_compares_its_structure() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("tone.wav");
    tutti_fixtures::signal::write_wav_f32(&path, SampleRate::SR_48K, 1, &vec![0.25f32; 4_800]);
    let (_first_streamer, first, first_voice) = stream_over(&path, 2.0);
    let (_second_streamer, second, second_voice) = stream_over(&path, 2.0);

    let moved = |voice, stream| {
        VoiceNodeConfig::streaming(
            voice,
            stream,
            Amplitude::new(0.5),
            StretchFactor::UNITY,
            Cents::new(0.0),
        )
        .expect("streamed")
        .speed(PlaybackRate::of(2.0))
        .looped(loop_frames(1, 3, 0))
        .direction(PlayDirection::Reverse)
    };
    let base = moved(first_voice, first);
    let values = base
        .clone()
        .speed(PlaybackRate::UNITY)
        .direction(PlayDirection::Forward);

    assert_initials_are_the_built_nodes(
        "base",
        &base.initials(),
        &base.build(&HostValues::new()).into_node().1,
        Some(at(2.0)),
        None,
    );
    assert_eq!(base.initials().params()[0].1, 0.5);
    assert_ne!(base.initials(), values.initials());

    // The same stream, its values moved, is the same node.
    assert!(base.same_structure(&values) && values.same_structure(&base));

    // Another stream is another node.
    let other = moved(second_voice, second);
    assert!(!base.same_structure(&other) && !other.same_structure(&base));
}

/// **A source config's gain, speed and loop are what the built voice starts at**,
/// read through its controls.
#[test]
fn a_source_configs_values_are_the_built_voices() {
    for (what, source, placed) in [
        ("free", moved_source(free_config(wave())), false),
        ("placed", moved_source(placed_config(wave(), at(1.0))), true),
    ] {
        let (_, controls) = if placed {
            VoiceNodeConfig::placed(source)
                .expect("placed")
                .build(&HostValues::new())
                .into_node()
        } else {
            VoiceNodeConfig::free(source)
                .expect("free")
                .build(&HostValues::new())
                .into_node()
        };
        assert_eq!(controls.gain(), Amplitude::new(0.5), "{what}: gain");
        assert_eq!(
            *controls.speed_cell().current(),
            PlaybackRate::of(2.0),
            "{what}: speed"
        );
        assert_eq!(
            *controls.looping_cell().current(),
            Some(loop_frames(1, 3, 0)),
            "{what}: loop"
        );
    }
}
