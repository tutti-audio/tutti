//! The live analyzer: a tap with a meter's state, drained by the app.
//!
//! Every figure is analytic: a constant of an exactly representable value, or a
//! sine of 48 samples a cycle with a sample on every crest, and the frame
//! counts the render and the tap's capacity fix.

use std::num::NonZeroUsize;

use tutti_buffers::{ChannelLayout, ChannelTopology, InterleavedMut, Mono, Wave};
use tutti_graph::{ForkByClone, TapAlign, TapOptions};
use tutti_measure::{
    Error as MeasureError, PeakBlocks, PeakConfig, VectorscopeConfig, WindowedLoudnessConfig,
};
use tutti_nodes::testing::{Const, Osc, Through};
use tutti_session::{Analyzer, AnalyzerError, BindError, Session};
use tutti_units::{Amplitude, Hz, SampleRate, Samples};

const RATE: SampleRate = SampleRate::SR_48K;
const BLOCK: usize = 64;

fn offline() -> Session {
    Session::builder()
        .sample_rate(RATE)
        .build()
        .expect("builds")
}

/// A tap that keeps the source's own time and holds `capacity` frames.
fn tap(capacity: usize) -> TapOptions {
    TapOptions::new(Samples::new(capacity)).align(TapAlign::Raw)
}

/// Renders `blocks` 64-frame blocks.
fn render(s: &mut Session, blocks: usize) {
    let mut out = vec![0.0f32; BLOCK * 2];
    for _ in 0..blocks {
        s.render(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());
    }
}

/// A peak meter over blocks of `frames`.
fn peak_blocks(frames: usize) -> PeakConfig {
    PeakConfig::new(Samples::new(frames))
}

/// Every block's peak, in order.
fn peaks(blocks: &PeakBlocks) -> Vec<f64> {
    blocks.as_flat().iter().map(|b| b.peak().get()).collect()
}

/// A constant sine of amplitude 0.5 through a peak meter yields 0.5 for every
/// block, as each 48-sample block holds a whole cycle.
#[test]
fn a_sine_of_half_amplitude_yields_a_peak_of_half_per_item() {
    let mut s = offline();
    let osc = s.graph().add(ForkByClone(
        Osc::sine(Hz::new(1000.0)).with_amplitude(Amplitude::new(0.5)),
    ));
    let mut analyzer = s
        .graph()
        .taps()
        .open_analyzer(&[osc.out(0)], peak_blocks(48), tap(4096))
        .expect("the source exists");
    let _ = s.update();
    render(&mut s, 10);

    let items: Vec<_> = analyzer.try_iter().collect();
    // 640 frames: thirteen whole blocks of 48.
    assert_eq!(items.len(), 13);
    for item in &items {
        assert!(
            (item.block.peak().get() - 0.5).abs() < 1e-6,
            "{:?}",
            item.block
        );
    }
    assert_eq!(analyzer.lost(), Samples::ZERO);
}

/// A constant 0.25 has a peak of 0.25 in every block, live and offline.
#[test]
fn live_and_offline_finish_at_the_constants_peak() {
    let mut s = offline();
    let src = s.graph().add(ForkByClone(Const::mono(0.25)));
    let analyzer: Analyzer<_> = s
        .graph()
        .taps()
        .open_analyzer(&[src.out(0)], peak_blocks(BLOCK), tap(4096))
        .expect("the source exists");
    let _ = s.update();
    render(&mut s, 10);
    let live = analyzer.finish();

    let wave = Wave::from_mono(RATE, Mono::new(&[0.25f32; 10 * BLOCK]));
    let offline = s
        .analyze(wave, peak_blocks(BLOCK))
        .run()
        .expect("the wave analyzes");

    assert_eq!(peaks(&live), [0.25; 10]);
    assert_eq!(peaks(&offline), [0.25; 10]);
}

#[test]
fn a_source_missing_at_open_is_missing_source() {
    let mut s = offline();
    let src = s.graph().add(ForkByClone(Const::mono(0.25)));
    s.remove(src).unwrap();
    let refused = s
        .graph()
        .taps()
        .open_analyzer(&[src.out(0)], peak_blocks(BLOCK), tap(64))
        .unwrap_err();
    assert!(
        matches!(refused, AnalyzerError::MissingSource(_)),
        "{refused:?}"
    );
}

/// A config the graph's rate and the tap's width refuse is the meter's error,
/// and the tap is not opened: the next tap is the first the session names.
#[test]
fn a_config_the_context_refuses_opens_no_tap() {
    let mut s = offline();
    let src = s.graph().add(ForkByClone(Const::mono(0.25)));
    let refused = s
        .graph()
        .taps()
        .open_analyzer(&[src.out(0)], peak_blocks(0), tap(64))
        .unwrap_err();
    assert!(
        matches!(refused, AnalyzerError::Measure(MeasureError::ZeroBlock)),
        "{refused:?}"
    );
    let next = s.graph().taps().open_root(Samples::new(64)).unwrap().id();

    let mut fresh = offline();
    let first = fresh
        .graph()
        .taps()
        .open_root(Samples::new(64))
        .unwrap()
        .id();
    assert_eq!(next, first, "the refused open minted no tap");
}

/// A stereo analysis of a mono tap has no view of it, and the tap is not
/// opened.
#[test]
fn a_view_the_tap_width_cannot_make_opens_no_tap() {
    let mut s = offline();
    let src = s.graph().add(ForkByClone(Const::mono(0.25)));
    let scope = VectorscopeConfig::new(NonZeroUsize::MIN);
    let refused = s
        .graph()
        .taps()
        .open_analyzer(&[src.out(0)], scope, tap(64))
        .unwrap_err();
    assert!(
        matches!(refused, AnalyzerError::Bind(BindError::Width { .. })),
        "{refused:?}"
    );
    let next = s.graph().taps().open_root(Samples::new(64)).unwrap().id();
    let mut fresh = offline();
    let first = fresh
        .graph()
        .taps()
        .open_root(Samples::new(64))
        .unwrap()
        .id();
    assert_eq!(next, first);
}

/// A NaN block is refused whole and counted in frames, the meter is untouched
/// by it, and the blocks after it still yield items: every rendered frame is
/// either refused or fed.
///
/// The NaN source's removal fades out, and the fade finishes at an update, so
/// the NaN reaches the tap for a few blocks after it is taken out.
#[test]
fn a_nan_block_is_counted_refused_and_the_next_blocks_items_arrive() {
    let mut s = offline();
    let nan = s.graph().add(ForkByClone(Const::mono(f32::NAN)));
    let finite = s.graph().add(ForkByClone(Const::mono(0.25)));
    let through = s.graph().add(ForkByClone(Through::mono()));
    s.graph().connect(nan, through).unwrap().unfaded();
    let loudness = WindowedLoudnessConfig::new(ChannelTopology::default_for(ChannelLayout::MONO));
    let mut analyzer = s
        .graph()
        .taps()
        .open_analyzer(&[through.out(0)], loudness, tap(4096))
        .expect("the source exists");
    let _ = s.update();
    render(&mut s, 1);
    assert_eq!(analyzer.try_iter().count(), 0, "a NaN block yields no item");
    assert_eq!(analyzer.refused(), Samples::new(BLOCK));

    s.remove(nan).unwrap();
    s.graph().connect(finite, through).unwrap().unfaded();
    let _ = s.update();
    let mut last = None;
    for _ in 0..200 {
        let _ = s.update();
        render(&mut s, 1);
        last = analyzer.try_iter().last().or(last);
    }
    let last = last.expect("the blocks after the NaN yield items");
    assert_eq!(
        last.frame + analyzer.refused(),
        Samples::new(201 * BLOCK),
        "every frame rendered was fed to the meter or counted refused"
    );
    assert!(analyzer.refused() >= Samples::new(BLOCK));
}

/// An app that does not drain loses what the ring cannot hold: the frames
/// rendered minus the capacity.
#[test]
fn lost_counts_the_frames_rendered_past_the_capacity() {
    let mut s = offline();
    let src = s.graph().add(ForkByClone(Const::mono(0.25)));
    let mut analyzer = s
        .graph()
        .taps()
        .open_analyzer(&[src.out(0)], peak_blocks(BLOCK), tap(BLOCK))
        .expect("the source exists");
    let _ = s.update();
    render(&mut s, 10);

    assert_eq!(analyzer.lost(), Samples::new(10 * BLOCK - BLOCK));
    // The capacity is what was kept: one block.
    assert_eq!(analyzer.try_iter().count(), 1);
    // Cumulative: a read does not clear it.
    assert_eq!(analyzer.lost(), Samples::new(10 * BLOCK - BLOCK));
}

/// Once every source is removed and the tap drained, the analyzer ends and
/// yields nothing more. The removal fades out, and the fade finishes at an update.
#[test]
fn the_analyzer_ends_after_its_source_is_removed() {
    let mut s = offline();
    let src = s.graph().add(ForkByClone(Const::mono(0.25)));
    let mut analyzer = s
        .graph()
        .taps()
        .open_analyzer(&[src.out(0)], peak_blocks(BLOCK), tap(4096))
        .expect("the source exists");
    let _ = s.update();
    render(&mut s, 2);
    assert_eq!(analyzer.try_iter().count(), 2);
    assert!(!analyzer.is_finished());

    s.remove(src).unwrap();
    let _ = s.update();
    for _ in 0..200 {
        let _ = s.update();
        render(&mut s, 1);
        let _ = analyzer.try_iter().count();
    }
    assert!(analyzer.is_finished());
    render(&mut s, 1);
    assert_eq!(analyzer.try_iter().count(), 0);
}
