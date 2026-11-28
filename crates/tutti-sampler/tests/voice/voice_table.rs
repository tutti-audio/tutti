//! A pool's config holds its voices as rows keyed by slot: a row is refused when its
//! key is taken or the voices are full, the rows are kept in key order, and a row
//! matches another by its structure alone.

use std::sync::Arc;

use tutti_buffers::{ChannelLayout, Interleaved, Wave};
use tutti_fixtures::{RangeContract, check_range_contract};
use tutti_sampler::testing::{free_config, loop_frames, placed_config};
use tutti_sampler::{
    MemorySourceConfig, PlaceError, Placed, PlayDirection, Row, SlotId, TableError, VoiceCount,
    VoiceNodeConfig, VoiceNodeError, VoicePoolConfig, VoiceWindow,
};
use tutti_units::{Amplitude, Beat, Cents, PlaybackRate, SampleRate, StretchFactor};

use super::node_config::stream_over;

/// Four stereo frames: a new allocation each call, so two calls are two identities.
fn wave() -> Arc<Wave> {
    let mut wave = Wave::new(2, SampleRate::SR_48K);
    for _ in 0..4 {
        wave.append(Interleaved::new(&[0.25, 0.25], wave.layout()).unwrap())
            .unwrap();
    }
    Arc::new(wave)
}

fn at(beat: f64) -> VoiceWindow {
    VoiceWindow::from(Beat::new(beat))
}

fn free(wave: &Arc<Wave>) -> VoiceNodeConfig<f32> {
    VoiceNodeConfig::free(free_config(Arc::clone(wave))).expect("free")
}

fn placed(wave: &Arc<Wave>, beat: f64) -> VoiceNodeConfig<f32, Placed> {
    VoiceNodeConfig::placed(placed_config(Arc::clone(wave), at(beat))).expect("placed")
}

fn pool() -> VoicePoolConfig<f32> {
    VoicePoolConfig::new(ChannelLayout::STEREO).expect("stereo is a width a pool runs")
}

fn voices(count: u16) -> VoiceCount {
    VoiceCount::of(count)
}

#[test]
fn a_duplicate_key_is_refused_and_a_free_key_is_taken() {
    let shared = wave();
    let held = pool().row(SlotId(3), free(&shared)).expect("a free key");
    assert_eq!(
        held.clone().row(SlotId(3), free(&shared)).err(),
        Some(TableError::DuplicateKey(SlotId(3))),
        "a second row under key 3"
    );
    let two = held.row(SlotId(4), free(&shared)).expect("another key");
    assert_eq!(two.table().rows().len(), 2);
}

#[test]
fn rows_past_the_voices_are_refused_with_their_figures() {
    let shared = wave();
    let full = pool()
        .voices(voices(2))
        .unwrap()
        .row(SlotId(1), free(&shared))
        .unwrap()
        .row(SlotId(2), free(&shared))
        .unwrap();
    assert_eq!(
        full.row(SlotId(3), free(&shared)).err(),
        Some(TableError::TooMany {
            rows: 3,
            capacity: voices(2)
        }),
        "a third row in two voices"
    );
}

#[test]
fn voices_below_the_rows_are_refused_whatever_the_order() {
    let shared = wave();
    let three = pool()
        .row(SlotId(1), free(&shared))
        .unwrap()
        .row(SlotId(2), free(&shared))
        .unwrap()
        .row(SlotId(3), free(&shared))
        .unwrap();
    assert_eq!(
        three.clone().voices(voices(2)).err(),
        Some(TableError::TooMany {
            rows: 3,
            capacity: voices(2)
        }),
        "voices lowered under the rows"
    );
    assert!(
        three.voices(voices(3)).is_ok(),
        "voices equal to the rows are enough"
    );
}

#[test]
fn a_voice_count_is_at_least_one() {
    check_range_contract(&RangeContract {
        new: VoiceCount::new,
        get: VoiceCount::get,
        of: Some(VoiceCount::of),
        valid: &[1, 64, u16::MAX],
        invalid: &[0],
    });
}

#[test]
fn a_pool_runs_a_width_from_one_channel_to_the_samplers_widest() {
    assert_eq!(
        VoicePoolConfig::<f32>::new(ChannelLayout::EMPTY).err(),
        Some(PlaceError::EmptyWidth)
    );
    assert!(matches!(
        VoicePoolConfig::<f32>::new(ChannelLayout::from_count(u16::MAX)).err(),
        Some(PlaceError::WidthTooWide { .. })
    ));
}

#[test]
fn configs_built_in_different_key_orders_are_equal_and_sorted() {
    let shared = wave();
    let build = |keys: [u128; 3]| {
        keys.into_iter().fold(pool(), |config, key| {
            config.row(SlotId(key), free(&shared)).expect("a free key")
        })
    };
    let ascending = build([1, 2, 3]);
    let shuffled = build([3, 1, 2]);
    assert_eq!(ascending, shuffled);
    let keys: Vec<u128> = shuffled
        .table()
        .rows()
        .iter()
        .map(|(key, _)| key.0)
        .collect();
    assert_eq!(keys, [1, 2, 3], "the rows are in key order");

    let other = pool()
        .row(SlotId(1), free(&shared))
        .unwrap()
        .row(SlotId(2), free(&shared))
        .unwrap();
    assert_ne!(ascending, other, "a row fewer is another config");
    assert_ne!(
        ascending,
        build([1, 2, 3]).voices(voices(3)).unwrap(),
        "another voice count is another config"
    );
}

#[test]
fn rows_over_one_shared_wave_are_equal_and_another_arc_is_not() {
    let shared = wave();
    let row = |wave: &Arc<Wave>| Row::from(free(wave));
    assert_eq!(row(&shared), row(&shared));
    assert!(row(&shared).same_structure(&row(&shared)));

    // The same samples behind another `Arc`.
    let copy = Arc::new((*shared).clone());
    assert_ne!(row(&shared), row(&copy));
    assert!(!row(&shared).same_structure(&row(&copy)));
}

#[test]
fn a_row_value_is_the_same_structure_and_a_structure_is_not() {
    let shared = wave();
    let moved = |source: MemorySourceConfig<f32>| {
        VoiceNodeConfig::free(
            source
                .gain(Amplitude::new(0.5))
                .speed(PlaybackRate::of(2.0))
                .looped(loop_frames(1, 3, 0)),
        )
        .expect("free")
        .direction(PlayDirection::Reverse)
        .stopped()
    };

    // Free rows: every value moved is the same structure, both ways.
    let base = Row::from(free(&shared));
    let values = Row::from(moved(free_config(Arc::clone(&shared))));
    assert_ne!(base, values);
    assert!(base.same_structure(&values) && values.same_structure(&base));

    // Placed rows: a moved window is a value too.
    let placed_base = Row::from(placed(&shared, 1.0));
    let placed_moved = Row::from(placed(&shared, 3.0).speed(PlaybackRate::of(2.0)));
    assert_ne!(placed_base, placed_moved);
    assert!(placed_base.same_structure(&placed_moved) && placed_moved.same_structure(&placed_base));

    // A stretching row's factor and pitch are values.
    let stretched = Row::from(free(&shared)).stretching(StretchFactor::new(2.0), Cents::new(0.0));
    let warped = Row::from(free(&shared)).stretching(StretchFactor::new(1.25), Cents::new(100.0));
    let unity = Row::from(free(&shared)).stretching(StretchFactor::UNITY, Cents::new(0.0));
    assert_ne!(stretched, warped);
    for (what, other) in [("another factor and pitch", &warped), ("unity", &unity)] {
        assert!(
            stretched.same_structure(other) && other.same_structure(&stretched),
            "{what} is a value of a stretching row"
        );
    }

    // Streamed rows: the stream is the identity, the values move.
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("tone.wav");
    tutti_fixtures::signal::write_wav_f32(&path, SampleRate::SR_48K, 1, &vec![0.25f32; 4_800]);
    let (_first_streamer, first, first_voice) = stream_over(&path, 2.0);
    let (_second_streamer, second, second_voice) = stream_over(&path, 2.0);
    let streamed = |voice, stream, gain| {
        VoiceNodeConfig::streaming(
            voice,
            stream,
            Amplitude::new(gain),
            StretchFactor::UNITY,
            Cents::new(0.0),
        )
        .expect("streamed")
    };
    let first_config = streamed(first_voice, first, 1.0);
    let first_row = Row::from(first_config.clone());
    let first_moved = Row::from(
        first_config
            .speed(PlaybackRate::of(2.0))
            .direction(PlayDirection::Reverse),
    );
    assert_ne!(first_row, first_moved);
    assert!(
        first_row.same_structure(&first_moved) && first_moved.same_structure(&first_row),
        "the same stream, its values moved"
    );
    let second_row = Row::from(streamed(second_voice, second, 1.0));
    assert!(
        !first_row.same_structure(&second_row),
        "another stream, over the same file"
    );

    // Each structure is another voice, both ways.
    let structures = [
        ("another wave", Row::from(free(&wave()))),
        (
            "another drive over the same wave",
            Row::from(placed(&shared, 0.0)),
        ),
        (
            "another width",
            Row::from(
                VoiceNodeConfig::free(
                    MemorySourceConfig::new(Arc::clone(&shared), ChannelLayout::MONO)
                        .expect("a stereo wave folds to mono"),
                )
                .expect("free"),
            ),
        ),
        ("a stretching row", stretched.clone()),
        ("a stream", first_row.clone()),
    ];
    for (what, other) in &structures {
        assert!(
            !base.same_structure(other) && !other.same_structure(&base),
            "{what} is another structure than the free row"
        );
    }
    assert!(
        !stretched.same_structure(&base) && !base.same_structure(&stretched),
        "a row that stretches and one that does not"
    );
    assert!(
        !first_row.same_structure(&Row::from(free(&shared))),
        "a stream is not a wave"
    );
}

#[test]
fn a_config_that_asks_for_a_stretch_is_a_stretching_row() {
    let shared = wave();
    let asked = Row::from(free(&shared).stretch(StretchFactor::new(2.0)));
    let declared = Row::from(free(&shared)).stretching(StretchFactor::new(2.0), Cents::new(0.0));
    assert_eq!(asked, declared);
    let pitched = Row::from(free(&shared).pitch(Cents::new(100.0)));
    assert!(
        asked.same_structure(&pitched),
        "a pitch shift stretches too"
    );
    assert!(
        !asked.same_structure(&Row::from(free(&shared))),
        "no stretch asked, no vocoder"
    );
}

#[test]
fn a_non_finite_gain_is_refused_by_the_row_source_config() {
    let shared = wave();
    for gain in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let source = free_config(Arc::clone(&shared)).gain(Amplitude::new(gain));
        assert!(
            matches!(
                VoiceNodeConfig::free(source).err(),
                Some(VoiceNodeError::Gain(_))
            ),
            "gain {gain}"
        );
    }
}
