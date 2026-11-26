//! Behavioural tests for the render + encode path.
//!
//! The crate had 45 tests and none of them rendered anything through a real
//! encoder end to end, which is how three of four output formats came to be
//! broken and an upmix export came to panic, both with a green suite. Every test
//! here fails against the code as it was.
//!
//! The cases below write WAV, FLAC and AIFF (the dev-dependency on
//! `tutti-codec` turns every encoder on). `hound` decodes the results back.
//!
//! Every graph here is built with `tutti_graph::GraphBuilder` and rendered
//! as a `RenderGraph`.

use crate::support;
use crate::support::{EncodeConfig, ExportConfig, RenderConfig};
use tutti_buffers::{Bed, ChannelLayout, ChannelTopology, Mono};

use crate::support::{render_to_buffers, render_to_file};
use tutti_buffers::FloatWidth;
use tutti_codec::IntPcm;
use tutti_codec::{FlacLevel, Format, IntDepth, Pcm};
use tutti_export::{Control, Normalize, Progress, RenderGraph, Resample, Setup, Stage, TailPolicy};
use tutti_graph::{Cx, ForkByClone, GraphBuilder, Io, Node, Prepare, Shape, Status};
use tutti_nodes::testing::{Const, Osc};
use tutti_timeline::{FrozenClock, RenderClock};
use tutti_units::{Amplitude, Drive, Hz, SampleRate};
use tutti_units::{Db, Samples, Seconds, Tail};

/// The rate [`config`] renders at.
const RATE: SampleRate = SampleRate::SR_44K1;

/// `g`, built for an export at `rate` — which must be the config's render
/// rate: a graph prepared at another is refused, not re-rated.
fn built(g: GraphBuilder, rate: SampleRate) -> RenderGraph {
    built_with(g, rate, Setup::new())
}

/// [`built`] with the main output declared (or not) as `setup` says.
fn built_with(g: GraphBuilder, rate: SampleRate, setup: Setup) -> RenderGraph {
    let (editor, executor) = g.build(RenderGraph::prepare(rate)).expect("builds");
    RenderGraph::assemble(editor, executor, setup).expect("built together")
}

/// A stereo DC at 0.5, built for an export at `rate`.
fn dc(rate: SampleRate) -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let id = g.add(Const::frame(&[0.5, 0.5]));
    g.pipe_output(id);
    built(g, rate)
}

/// FLAC at 24 bit and the default level.
fn flac() -> Format {
    Format::Flac {
        pcm: IntPcm::new(IntDepth::Int24),
        level: FlacLevel::default(),
    }
}

fn config(format: Format, layout: ChannelLayout) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: RATE,
            duration_seconds: Seconds::new(0.2),
            ..Default::default()
        },
        encode: EncodeConfig {
            format,
            channels: layout,
            topology: None,
        },
        ..Default::default()
    }
}

#[test]
fn all_three_formats_export() {
    let d = tempfile::tempdir().unwrap();
    for (f, ext) in [
        (Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int24))), "wav"),
        (flac(), "flac"),
        (Format::Aiff(Pcm::Int(IntPcm::new(IntDepth::Int24))), "aiff"),
    ] {
        let p = d.path().join(format!("a.{ext}"));
        let r = render_to_file(
            dc(RATE),
            &config(f, ChannelLayout::STEREO),
            &FrozenClock,
            &p,
        );
        match &r {
            Ok(w) => println!("{ext}: OK {} bytes", w.bytes),
            Err(e) => println!("{ext}: ERR {e}"),
        }
        assert!(r.is_ok(), "{ext} failed: {r:?}");
        assert!(
            r.unwrap().bytes > 100,
            "{ext} wrote a suspiciously small file"
        );
    }
}

/// A mono graph exported to quad is the front pair, and the rears stay silent.
#[test]
fn upmix_does_not_panic_and_leaves_extras_silent() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("q.wav");
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let id = g.add(Const::mono(0.5));
    g.pipe_output(id);
    render_to_file(
        built(g, RATE),
        &config(
            Format::Wav(Pcm::Float(FloatWidth::F32)),
            ChannelLayout::QUAD,
        ),
        &FrozenClock,
        &p,
    )
    .unwrap();
    let rd = hound::WavReader::open(&p).unwrap();
    assert_eq!(rd.spec().channels, 4);
    let s: Vec<f32> = rd.into_samples::<f32>().map(|x| x.unwrap()).collect();
    let mut ch = [0.0f32; 4];
    for f in s.as_chunks::<4>().0 {
        for (a, &v) in ch.iter_mut().zip(f) {
            *a = a.max(v.abs());
        }
    }
    println!("mono->quad channel peaks: {ch:?}");
    assert!(
        ch[0] > 0.4 && ch[1] > 0.4,
        "the front pair carries the signal"
    );
    for (c, &peak) in ch.iter().enumerate().skip(2) {
        assert!(peak < 1e-6, "ch{c} must be silent, got {peak}");
    }
}

/// The clock is advanced once per block, by exactly the frames produced.
///
/// A clock that is never advanced leaves every placed voice at beat 0, and
/// the failure mode is total silence.
#[test]
fn the_clock_advances_by_exactly_the_frames_rendered() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingClock(AtomicUsize);
    impl RenderClock for CountingClock {
        fn advance(&self, frames: tutti_units::Samples) {
            self.0.fetch_add(frames.get(), Ordering::Relaxed);
        }
        // A stopped transport at beat 0: nothing in this graph reads it.
        // That the graph is handed the clock's transport block by block is
        // `graph_source.rs`'s `the_graph_reads_the_render_clocks_transport`.
        fn graph_block(
            &self,
        ) -> (
            tutti_graph::TransportPosition,
            tutti_graph::TransportChanges,
        ) {
            Default::default()
        }
    }

    let clock = Arc::new(CountingClock(AtomicUsize::new(0)));
    let out = render_to_buffers(
        dc(RATE),
        &config(
            Format::Wav(Pcm::Float(FloatWidth::F32)),
            ChannelLayout::STEREO,
        ),
        clock.as_ref(),
    )
    .unwrap();

    let frames = out.frames().get();
    assert_eq!(frames, 8820, "0.2 s at 44.1 kHz");
    assert_eq!(
        clock.0.load(Ordering::Relaxed),
        frames,
        "the clock must advance exactly once per rendered frame"
    );
}

/// A latency trim renders extra frames and drops them from the head, so the
/// output is still the requested length — not short by the trim.
#[test]
fn a_latency_trim_preserves_the_output_length() {
    let mut s = config(
        Format::Wav(Pcm::Float(FloatWidth::F32)),
        ChannelLayout::STEREO,
    );
    let untrimmed = render_to_buffers(dc(RATE), &s, &FrozenClock).unwrap();

    s.render.latency = tutti_units::Samples::new(512);
    let trimmed = render_to_buffers(dc(RATE), &s, &FrozenClock).unwrap();

    assert_eq!(
        untrimmed.frames(),
        trimmed.frames(),
        "trimming must not shorten the output"
    );
}

/// A tail lengthens the output by exactly the tail.
///
/// The mirror of `a_latency_trim_preserves_the_output_length`, and the opposite
/// direction: a trim must not shorten the file, a tail must lengthen it. The
/// decay is audio the graph produced, so keeping it is the point — rendering it
/// and then discarding it at the sink's cap would leave this assertion at the
/// untailed length.
#[test]
fn a_tail_lengthens_the_output_by_exactly_the_tail() {
    let mut s = config(
        Format::Wav(Pcm::Float(FloatWidth::F32)),
        ChannelLayout::STEREO,
    );
    let untailed = render_to_buffers(dc(RATE), &s, &FrozenClock).unwrap();

    let tail = tutti_units::Samples::new(4096);
    s.render.tail = TailPolicy::Frames(tail);
    let tailed = render_to_buffers(dc(RATE), &s, &FrozenClock).unwrap();

    assert_eq!(
        tailed.frames().get(),
        untailed.frames().get() + tail.get(),
        "the rendered tail must reach the output, not be capped away"
    );
}

/// A convolver's tail is its impulse response's ring-out, exactly.
///
/// The expected value here comes from the DSP, not from the implementation: an
/// FIR of length `L` answers an impulse at frame 0 through frame `L - 1`, so
/// `L - 1` frames land after the input stops. This is the one node whose tail is
/// known a priori, which is what makes it worth reaching for a dev-dependency.
///
/// Read through `samples()`: every node here reports, so the graph's tail is a
/// number the caller can spend without a decision.
#[test]
fn a_convolver_reports_its_ir_ring_out() {
    let ir = vec![0.5f32; 4096];
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let src = g.add(Const::mono(0.5));
    let conv = g.add_with_controls(convolver(&ir)).0;
    g.connect(src, 0, conv, 0).pipe_output(conv);

    assert_eq!(
        built(g, RATE).declared_tail().samples(),
        Some(tutti_units::Samples::new(4095)),
    );
}

/// Cascaded convolvers sum their ring-outs, because cascading convolves the two
/// responses and a ring-out is defined so that supports add without a
/// correction term.
#[test]
fn cascaded_convolvers_sum_their_tails() {
    let a = vec![0.5f32; 1024];
    let b = vec![0.5f32; 2048];
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let src = g.add(Const::mono(0.5));
    let first = g.add_with_controls(convolver(&a)).0;
    let second = g.add_with_controls(convolver(&b)).0;
    g.connect(src, 0, first, 0)
        .connect(first, 0, second, 0)
        .pipe_output(second);

    // 1023 + 2047, which is the ring-out of the 3071-sample cascaded response.
    assert_eq!(
        built(g, RATE).declared_tail().samples(),
        Some(tutti_units::Samples::new(3070)),
    );
}

/// One un-reporting node is enough to make the graph's figure a partial one.
///
/// `known()` still gives the sum over what spoke, so the two accessors disagree
/// — which is the whole reason there are two. The unreporting node has to be
/// constructed deliberately now that the engine's stock nodes all answer.
#[test]
fn one_silent_node_makes_the_figure_partial_without_losing_it() {
    /// A node that does not know its tail (`Tail::Unknown`): what a node that
    /// has not measured its own ring-out declares.
    #[derive(Clone, Default)]
    struct Unreporting;

    impl Node for Unreporting {
        fn shape(&self) -> Shape {
            Shape::through_unassigned(ChannelLayout::MONO).tail(tutti_units::Tail::Unknown)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
            let (ins, mut outs) = io.split();
            outs.get(0).copy_from_slice(ins.get(0));
            Status::Modified
        }
        fn reset(&mut self) {}
    }

    let ir = vec![0.5f32; 4096];
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let src = g.add(Const::mono(0.5));
    let quiet = g.add(ForkByClone(Unreporting));
    let conv = g.add_with_controls(convolver(&ir)).0;
    g.connect(src, 0, quiet, 0)
        .connect(quiet, 0, conv, 0)
        .pipe_output(conv);

    let reported = built(g, RATE).declared_tail();
    assert_eq!(reported.unknown_nodes(), 1, "one node never answered");
    assert_eq!(
        reported.samples(),
        None,
        "so the graph's tail cannot be spent as a number"
    );
    assert_eq!(
        reported.known(),
        tutti_units::Samples::new(4095),
        "but what the convolver reported is still available"
    );
}

/// The stock nodes now answer, so an ordinary graph reports a spendable tail.
///
/// This is what teaching `AudioNode` to report bought: before it, a single
/// `dc` source left `samples()` refusing for every real project, because one
/// unreporting node on the output path makes the whole figure unspendable.
#[test]
fn a_graph_of_stock_nodes_reports_a_spendable_tail() {
    let reported = dc(RATE).declared_tail();
    assert_eq!(reported.unknown_nodes(), 0);
    assert_eq!(reported.samples(), Some(tutti_units::Samples::new(0)));
}

/// A stereo integrator: `y[n] = y[n-1] + x[n]`, per channel.
///
/// The smallest node that genuinely never decays — a feedback loop with a gain
/// of exactly one, the limit of an FDN reverb. The engine ships no node that
/// reports
/// [`Tail::Unbounded`]: `ConvolverNode`, its reverb, is an FIR and reports a
/// finite ring-out (the cases above). So the property under test — that
/// `resolve` spends exactly the caller's cap on a graph that never decays —
/// needs a node that says so, and this one is honest about it.
///
/// [`Tail::Unbounded`]: tutti_units::Tail::Unbounded
#[derive(Clone, Default)]
struct Integrator([f32; 2]);

impl Node for Integrator {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::STEREO).tail(tutti_units::Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        for (c, sum) in self.0[..2].iter_mut().enumerate() {
            for (y, &x) in outs.get(c).iter_mut().zip(ins.get(c)) {
                *sum += x;
                *y = *sum;
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {
        self.0 = [0.0; 2];
    }
}

/// A graph that never decays resolves to exactly the caller's bound.
///
/// A feedback loop re-enters its own output, so it has no frame count of its
/// own — the cap is the whole answer, and it is the caller's choice rather than
/// a figure the engine invented.
#[test]
fn resolving_an_unbounded_graph_spends_the_cap() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let src = g.add(Const::frame(&[0.5, 0.5]));
    let rev = g.add(ForkByClone(Integrator::default()));
    g.connect(src, 0, rev, 0)
        .connect(src, 1, rev, 1)
        .pipe_output(rev);

    let reported = built(g, RATE).declared_tail();
    let cap = tutti_units::Samples::new(384_000);
    assert!(
        reported.is_unbounded(),
        "a unity-gain feedback loop never decays on its own"
    );
    assert_eq!(reported.samples(), None, "so there is no count to spend");
    assert_eq!(reported.resolve(cap), cap);
}

/// A fully-reported graph resolves to its own figure, not the cap.
///
/// The cap bounds; it does not replace. Spending it on a graph that answered
/// would append silence — 4095 frames is 0.09 s, and the cap here is eight
/// seconds.
#[test]
fn resolving_a_reported_graph_keeps_its_own_figure() {
    let ir = vec![0.5f32; 4096];
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let src = g.add(Const::mono(0.5));
    let conv = g.add_with_controls(convolver(&ir)).0;
    g.connect(src, 0, conv, 0).pipe_output(conv);

    let reported = built(g, RATE).declared_tail();
    assert_eq!(
        reported.resolve(tutti_units::Samples::new(384_000)),
        tutti_units::Samples::new(4095),
        "the convolver's figure, not the cap"
    );
}

/// A stateless node declares it has no tail, rather than staying silent about
/// it.
///
/// This is what makes the mechanism usable: one unreporting node on the output
/// path makes the whole graph's tail unspendable, so declaring `None` on the
/// nodes that genuinely have none is load-bearing, not cosmetic.
#[test]
fn a_stateless_node_reports_no_tail_rather_than_an_unknown_one() {
    use tutti_buffers::ChannelLayout;
    use tutti_graph::Node;
    use tutti_nodes::{DistortionConfig, ShapeKind, Waveshape};

    let dist = tutti_nodes::DistortionNode::<f32>::from(
        DistortionConfig::new(ChannelLayout::STEREO, Waveshape::new(ShapeKind::Tanh))
            .drive(Drive::new(1.0)),
    );
    // A kernel node's tail is unknown until it has a rate.
    let dist = tutti_graph::contract::prepared(dist, RATE, 64);
    assert_eq!(dist.shape().tail, tutti_units::Tail::None);
}

/// `render_to_buffers` reports the rate its samples are actually at, and gives
/// back one plane per channel rather than a stereo pair.
#[test]
fn buffers_report_their_own_shape() {
    let out = render_to_buffers(
        dc(RATE),
        &config(
            Format::Wav(Pcm::Float(FloatWidth::F32)),
            ChannelLayout::QUAD,
        ),
        &FrozenClock,
    )
    .unwrap();
    assert_eq!(out.channels(), 4, "a quad request yields four planes");
    assert_eq!(out.sample_rate().get(), 44100.0);
    assert!(out.planes().iter().all(|p| p.len() == out.frames().get()));
}

/// Normalization composes: measure with tutti-measure, apply the gain it
/// reports. The crate does not hide this, which is why it can stream.
#[test]
fn a_caller_can_compose_normalization() {
    use tutti_buffers::Interleaved;
    use tutti_measure::{LoudnessConfig, measure_loudness};
    use tutti_units::Db;

    // Longer than R128's 400 ms gating block, or the meter reports nothing
    // passed the gate and there is no loudness to normalize toward.
    let mut long = config(
        Format::Wav(Pcm::Float(FloatWidth::F32)),
        ChannelLayout::STEREO,
    );
    long.render.duration_seconds = Seconds::new(2.0);
    let mut out = render_to_buffers(dc(RATE), &long, &FrozenClock).unwrap();

    let cfg = LoudnessConfig::new(Bed::Stereo.topology());
    let rate = out.sample_rate();
    let flat = out.interleaved();
    let before = measure_loudness(
        &cfg,
        Interleaved::new(&flat, ChannelLayout::STEREO).unwrap(),
        rate,
    )
    .unwrap();
    let gain = before.gain_to(Db::new(-14.0), Db::new(-1.0));
    out.apply_gain(gain);
    let flat = out.interleaved();
    let after = measure_loudness(
        &cfg,
        Interleaved::new(&flat, ChannelLayout::STEREO).unwrap(),
        rate,
    )
    .unwrap();

    // Assert what `gain_to` actually promises, not merely that `apply_gain` is
    // linear. The earlier form checked `after ≈ before + gain`, which is
    // algebraically true for ANY gain — `gain_to` could return `Db(0.0)` and it
    // still passed, so the measure half of measure-then-apply was unverified.
    //
    // The promise is two-sided: reach the target, UNLESS the true-peak ceiling
    // binds first. This signal is a DC-ish 0.5 constant at -29.3 LUFS with a
    // -4.99 dBTP peak, so +15.3 dB would be needed for -14 LUFS — and that
    // would put the peak at +10 dBTP. The ceiling wins, and the peak is what
    // must land exactly.
    assert!(
        after.lufs.get() <= -14.0 + 0.5,
        "must never overshoot the loudness target: {before:?} -> {after:?}"
    );
    assert!(
        (after.true_peak.get() - (-1.0)).abs() < 0.5,
        "the ceiling binds here, so the peak must land on it: {after:?}"
    );

    // And the ceiling must not be an excuse to do nothing: a gain was applied.
    assert!(
        after.lufs.get() > before.lufs.get() + 1.0,
        "the reachable part of the gain must still be applied: \
         {before:?} + {gain:?} -> {after:?}"
    );
}

/// A requested resample reaches the file: the header carries the target rate,
/// and the frame count matches the converted duration.
///
/// The file path must honour the resample the config asks for.
#[test]
fn a_resample_request_reaches_the_file() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("r.wav");
    let mut s = config(
        Format::Wav(Pcm::Float(FloatWidth::F32)),
        ChannelLayout::STEREO,
    );
    s.render.duration_seconds = Seconds::new(1.0);
    s.resample = Some(Resample::to(SampleRate::SR_48K));

    render_to_file(dc(RATE), &s, &FrozenClock, &p).unwrap();

    let rd = hound::WavReader::open(&p).unwrap();
    assert_eq!(
        rd.spec().sample_rate,
        48_000,
        "header must carry the target"
    );
    let frames = rd.into_samples::<f32>().count() / 2;
    let err = (frames as i64 - 48_000).abs();
    assert!(
        err < 512,
        "expected ~48000 frames at the new rate, got {frames}"
    );
}

/// `write_buffers` completes the measure-then-apply cycle: render, normalize,
/// write. Without it a caller who normalized would have nowhere to put the
/// result — and it shares every encoder with the streaming path rather than
/// growing a second writer per format.
#[test]
fn normalized_audio_can_be_written_to_every_format() {
    let d = tempfile::tempdir().unwrap();
    let mut s = config(
        Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int24))),
        ChannelLayout::STEREO,
    );
    s.render.duration_seconds = Seconds::new(1.0);

    let mut audio = render_to_buffers(dc(RATE), &s, &FrozenClock).unwrap();
    audio.apply_gain(tutti_units::Db::new(-6.0));

    for (format, ext) in [
        (Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int24))), "wav"),
        (flac(), "flac"),
        (Format::Aiff(Pcm::Int(IntPcm::new(IntDepth::Int24))), "aiff"),
    ] {
        s.encode.format = format;
        let p = d.path().join(format!("n.{ext}"));
        let w = support::write_buffers(&audio, &s, &p).expect("write");
        assert!(w.bytes > 100, "{ext} wrote {} bytes", w.bytes);
    }

    // The written WAV must carry the gain: 0.5 at -6 dB is ~0.25. (The WAV was
    // written in the loop above; this only re-reads it.)
    let p = d.path().join("n.wav");
    let rd = hound::WavReader::open(&p).unwrap();
    let peak = rd
        .into_samples::<i32>()
        .map(|x| (x.unwrap() as f32 / 8_388_608.0).abs())
        .fold(0.0f32, f32::max);
    assert!(
        (peak - 0.25).abs() < 0.01,
        "expected ~0.25 after -6 dB, got {peak}"
    );
}

/// A resample must reach every format, not just the ones that happened to
/// route through the shared pump.
///
/// An encoder that pulled from `drive` directly while taking its header rate
/// from `encoder_rate` would write un-resampled audio under a header claiming
/// the target (a 1 s render playing back 8.8% fast), which a WAV-only test
/// cannot see.
#[test]
fn a_resample_reaches_every_format_not_just_wav() {
    let d = tempfile::tempdir().unwrap();
    let mut s = config(
        Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int24))),
        ChannelLayout::STEREO,
    );
    s.render.duration_seconds = Seconds::new(1.0); // 44100 frames in, 48000 expected out
    s.resample = Some(Resample::to(tutti_units::SampleRate::SR_48K));

    // WAV is the control: it was already correct.
    let p = d.path().join("r.wav");
    render_to_file(dc(RATE), &s, &FrozenClock, &p).unwrap();
    let rd = hound::WavReader::open(&p).unwrap();
    assert_eq!(rd.spec().sample_rate, 48_000);
    let wav_frames = rd.into_samples::<i32>().count() / 2;
    assert!(
        (wav_frames as i64 - 48_000).abs() < 512,
        "wav: expected ~48000 frames, got {wav_frames}"
    );

    // FLAC: the header claims 48k, so the payload must actually BE 48k. If the
    // resample were skipped the file would hold 44100 frames.
    s.encode.format = flac();
    let pf = d.path().join("r.flac");
    render_to_file(dc(RATE), &s, &FrozenClock, &pf).unwrap();
    let flac_frames = flac_frame_count(&pf);
    assert!(
        (flac_frames as i64 - 48_000).abs() < 512,
        "flac: header says 48000 but payload holds {flac_frames} frames \
         (44100 means the resample was skipped)"
    );
}

/// `write_buffers` must resample from the rate the FRAMES are at, not from
/// whatever `config.render.sample_rate` happens to hold.
///
/// The frames handed to `write_buffers` already exist; `config.render` describes
/// a render that already happened, and may be a different rate entirely. Reading
/// it made the output 2.18x too long and pitched down, under a header that said
/// otherwise.
#[test]
fn write_buffers_resamples_from_the_frames_own_rate() {
    let d = tempfile::tempdir().unwrap();

    // Render at 96k...
    let mut render_cfg = config(
        Format::Wav(Pcm::Float(FloatWidth::F32)),
        ChannelLayout::STEREO,
    );
    render_cfg.render.sample_rate = tutti_units::SampleRate::SR_96K;
    render_cfg.render.duration_seconds = Seconds::new(1.0);
    let audio =
        render_to_buffers(dc(render_cfg.render.sample_rate), &render_cfg, &FrozenClock).unwrap();
    assert_eq!(audio.sample_rate().get(), 96_000.0);

    // ...then write with a config whose `render` half is left at its DEFAULT
    // 44100 — the case the doc tells a caller is fine to ignore.
    let mut write_cfg = config(
        Format::Wav(Pcm::Float(FloatWidth::F32)),
        ChannelLayout::STEREO,
    );
    write_cfg.resample = Some(Resample::to(tutti_units::SampleRate::SR_48K));
    assert_eq!(
        write_cfg.render.sample_rate.get(),
        44_100.0,
        "this test is only meaningful while the config's render rate differs"
    );

    let p = d.path().join("w.wav");
    support::write_buffers(&audio, &write_cfg, &p).unwrap();

    let rd = hound::WavReader::open(&p).unwrap();
    assert_eq!(rd.spec().sample_rate, 48_000);
    let frames = rd.into_samples::<f32>().count() / 2;
    // 96k -> 48k halves the length. Reading 44100 as the source instead would
    // give ~104_490 frames.
    assert!(
        (frames as i64 - 48_000).abs() < 512,
        "expected ~48000 frames (96k halved), got {frames} \
         (~104490 means it resampled from the config's rate, not the audio's)"
    );
}

/// Peak sample of a written float WAV.
fn file_peak(path: &std::path::Path) -> f32 {
    hound::WavReader::open(path)
        .unwrap()
        .into_samples::<f32>()
        .map(|s| s.unwrap().abs())
        .fold(0.0f32, f32::max)
}

/// `render_normalized_to_file` reaches the requested loudness — the whole point
/// of the two-pass path. The un-normalized render of the same graph is the
/// control: equal peaks would mean the normalization did nothing.
#[test]
fn normalized_export_lifts_the_level_toward_the_target() {
    use crate::support::render_normalized_to_file;
    use tutti_export::Normalize;
    use tutti_units::Db;

    let d = tempfile::tempdir().unwrap();
    // Longer than R128's 400 ms gating block, or nothing passes the gate.
    let mut cfg = config(
        Format::Wav(Pcm::Float(FloatWidth::F32)),
        ChannelLayout::STEREO,
    );
    cfg.render.duration_seconds = Seconds::new(2.0);

    let plain = d.path().join("plain.wav");
    render_to_file(dc(RATE), &cfg, &FrozenClock, &plain).unwrap();

    let normalized = d.path().join("normalized.wav");
    render_normalized_to_file(
        dc(RATE),
        &cfg,
        &FrozenClock,
        Normalize::lufs(Db::new(-14.0)),
        &normalized,
    )
    .unwrap();

    let (before, after) = (file_peak(&plain), file_peak(&normalized));
    assert!(
        after > before,
        "normalization must lift this signal: {before} -> {after}"
    );
    assert!(after <= 1.0, "and must never clip past full scale: {after}");
}

/// `Normalize::Peak` targets TRUE peak (4x oversampled), which is why it routes
/// through the R128 meter rather than a `fold(max, abs)` over the planes.
///
/// So the file must be re-measured, not merely re-read: for this signal the
/// two readings differ by ~1 dB. A 0.5 constant has a −6.02 dB *sample* peak but
/// a −4.99 dBTP *true* peak — the oversampling filter rings at the buffer's hard
/// leading edge. Asserting the sample peak against a dBTP target would be
/// comparing two different quantities, and would have to be loosened by exactly
/// the amount the feature is supposed to catch.
#[test]
fn peak_normalization_lands_on_the_requested_dbtp() {
    use crate::support::render_normalized_to_file;
    use tutti_buffers::Interleaved;
    use tutti_export::Normalize;
    use tutti_measure::{LoudnessConfig, measure_loudness};
    use tutti_units::Db;

    let d = tempfile::tempdir().unwrap();
    let mut cfg = config(
        Format::Wav(Pcm::Float(FloatWidth::F32)),
        ChannelLayout::STEREO,
    );
    cfg.render.duration_seconds = Seconds::new(2.0);

    let path = d.path().join("peak.wav");
    render_normalized_to_file(
        dc(RATE),
        &cfg,
        &FrozenClock,
        Normalize::peak(Db::new(-1.0)),
        &path,
    )
    .unwrap();

    let samples: Vec<f32> = hound::WavReader::open(&path)
        .unwrap()
        .into_samples::<f32>()
        .map(|s| s.unwrap())
        .collect();
    let meter = LoudnessConfig::new(Bed::Stereo.topology());
    let measured = measure_loudness(
        &meter,
        Interleaved::new(&samples, ChannelLayout::STEREO).unwrap(),
        cfg.render.sample_rate,
    )
    .unwrap();

    assert!(
        (measured.true_peak.get() - (-1.0)).abs() < 0.2,
        "the written file's TRUE peak must land on the -1 dBTP target, got {:?}",
        measured.true_peak
    );
    // And the sample peak sits BELOW it — the gap is the oversampling headroom
    // a sample-peak normalizer would have handed to the clipper instead.
    assert!(
        file_peak(&path) < 0.891,
        "a true-peak target must leave sample-peak headroom, got {}",
        file_peak(&path)
    );
}

/// True peak, in dBTP, of a stereo WAV on disk (float or integer), measured
/// with the same R128 meter the export normalizes by.
fn file_true_peak(path: &std::path::Path) -> f64 {
    use tutti_buffers::Interleaved;
    use tutti_measure::{LoudnessConfig, measure_loudness};

    let reader = hound::WavReader::open(path).unwrap();
    let spec = reader.spec();
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.into_samples::<f32>().map(|s| s.unwrap()).collect(),
        hound::SampleFormat::Int => {
            let full = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .into_samples::<i32>()
                .map(|s| s.unwrap() as f32 / full)
                .collect()
        }
    };
    let meter = LoudnessConfig::new(Bed::Stereo.topology());
    measure_loudness(
        &meter,
        Interleaved::new(&samples, ChannelLayout::STEREO).unwrap(),
        SampleRate::of(f64::from(spec.sample_rate)),
    )
    .expect("stereo is measurable")
    .true_peak
    .get()
}

/// The trap `loudness.rs::a_reading_is_always_finite_even_below_the_gate` names,
/// reached through the export path: a render SHORTER than R128's 400 ms gating
/// block reports `-inf` LUFS, which the meter clamps to the -70 LUFS gate to
/// keep every gain finite. Policy (owner decision, lane E1): that floor is not
/// a measurement, and normalizing to a loudness from it would apply about
/// +47 dB to whatever the render holds. So a LUFS normalization of such a
/// render is `Unmeasurable { BelowGate }` and writes nothing, where it used to
/// write a finite file at that invented gain. A PEAK normalization of the same
/// render is measurable (a true peak needs no gate) and stays finite.
#[test]
fn a_sub_gating_block_render_cannot_be_loudness_normalized() {
    use crate::support::render_normalized_to_file;
    use tutti_export::{Normalize, Unmeasured};
    use tutti_units::Db;

    let d = tempfile::tempdir().unwrap();
    // 0.2 s — half the 400 ms gating block.
    let cfg = config(
        Format::Wav(Pcm::Float(FloatWidth::F32)),
        ChannelLayout::STEREO,
    );
    assert!(
        cfg.render.duration_seconds < Seconds::new(0.4),
        "this test is only meaningful under the gating block"
    );

    let path = d.path().join("short.wav");
    let err = render_normalized_to_file(
        dc(RATE),
        &cfg,
        &FrozenClock,
        Normalize::lufs(Db::new(-14.0)),
        &path,
    )
    .expect_err("nothing passed the gate");
    assert!(
        matches!(
            err,
            tutti_export::RenderError::Unmeasurable {
                reason: Unmeasured::BelowGate,
                output: Some(0),
                channels,
                sample_rate,
            } if channels == ChannelLayout::STEREO && sample_rate == RATE
        ),
        "{err:?}"
    );
    assert!(
        !path.exists(),
        "nothing is written for an unmeasurable render"
    );

    // The same render has a true peak, so a peak target is measurable, and
    // the gain it picks is finite.
    render_normalized_to_file(
        dc(RATE),
        &cfg,
        &FrozenClock,
        Normalize::peak(Db::new(-1.0)),
        &path,
    )
    .unwrap();
    let samples: Vec<f32> = hound::WavReader::open(&path)
        .unwrap()
        .into_samples::<f32>()
        .map(|s| s.unwrap())
        .collect();
    assert!(!samples.is_empty(), "a short render still writes frames");
    assert!(samples.iter().all(|s| s.is_finite()));
    let peak = file_true_peak(&path);
    assert!(
        (peak + 1.0).abs() < 0.05,
        "the short render lands on its -1 dBTP target, got {peak}"
    );
}

/// R128 meters any channel count, so a surround export normalizes against its
/// own loudness: it must not error, and must not silently switch to a
/// different metric.
#[test]
fn surround_normalizes_rather_than_falling_back_to_peak() {
    use crate::support::render_normalized_to_file;
    use tutti_export::Normalize;
    use tutti_units::Db;

    let d = tempfile::tempdir().unwrap();
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::from(6u16));
    let id = g.add(Const::frame(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6]));
    g.pipe_output(id);

    let mut cfg = config(
        Format::Wav(Pcm::Float(FloatWidth::F32)),
        ChannelLayout::from(6u16),
    );
    cfg.render.duration_seconds = Seconds::new(2.0);

    let path = d.path().join("surround.wav");
    render_normalized_to_file(
        built(g, cfg.render.sample_rate),
        &cfg,
        &FrozenClock,
        Normalize::lufs(Db::new(-14.0)),
        &path,
    )
    .expect("5.1 normalization must not be rejected");

    let reader = hound::WavReader::open(&path).unwrap();
    assert_eq!(reader.spec().channels, 6, "all six channels must survive");
}

/// Frame count from a FLAC STREAMINFO block (bytes 21..27, 36 bits).
fn flac_frame_count(path: &std::path::Path) -> u64 {
    let b = std::fs::read(path).unwrap();
    assert_eq!(&b[0..4], b"fLaC", "not a FLAC file");
    // 4 magic + 4 block header, then STREAMINFO; total-samples is 36 bits
    // starting 13 bytes into it.
    let si = &b[8..];
    ((si[13] as u64 & 0x0F) << 32)
        | (si[14] as u64) << 24
        | (si[15] as u64) << 16
        | (si[16] as u64) << 8
        | (si[17] as u64)
}

/// Silence cannot be normalized. Owner decision (lane E1): exact silence is
/// unmeasurable, so it is `Unmeasurable` (`Silent` for a peak target,
/// `BelowGate` for a loudness one) and nothing is written. It used to be
/// written as silence, which this test asserted (`peak <= 2 LSB`): the policy
/// moved from "safe by multiplying zero" to "refused", because the gain it was
/// multiplying (+144 dB, from the true-peak floor) was invented.
///
/// It still guards what the old test did, that `render_to_buffers` hands the
/// meter planes with no dither in them: dither added there makes the silence
/// read as ±1 LSB of signal, which is measurable, and the render succeeds.
///
/// (The planes `render_to_buffers` returns for silence are checked directly by
/// `silence_planes_carry_no_dither`.)
#[test]
fn normalizing_silence_is_refused() {
    use crate::support::render_normalized_to_file;
    use tutti_export::{Normalize, Unmeasured};
    use tutti_units::Db;

    let silence = || {
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
        let id = g.add(Const::frame(&[0.0, 0.0]));
        g.pipe_output(id);
        built(g, RATE)
    };

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("silence.wav");

    let cfg = config(
        Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int16))),
        ChannelLayout::STEREO,
    );

    for (normalize, want) in [
        (Normalize::peak(Db::new(-1.0)), Unmeasured::Silent),
        (Normalize::lufs(Db::new(-14.0)), Unmeasured::BelowGate),
    ] {
        let err = render_normalized_to_file(silence(), &cfg, &FrozenClock, normalize, &path)
            .expect_err("silence has no reading");
        assert!(
            matches!(&err, tutti_export::RenderError::Unmeasurable { reason, .. } if *reason == want),
            "{err:?}"
        );
        assert!(!path.exists());
    }
}

/// The dBTP target must hold in the written file, resample included.
///
/// Sample-rate conversion moves the true peak — its interpolation overshoots
/// between the original samples — so a gain measured at the render rate and
/// applied to audio written at another lands off target.
///
/// The shift is small (~0.16 dB on this signal, measured) but systematic and
/// always upward, which is what makes it dangerous: it eats the safety margin a
/// ceiling exists to provide, and at a 0 dBTP target it clips outright. The
/// tolerance below is deliberately TIGHTER than the shift — a looser one passes
/// whether or not the conversion happens before the measurement, which is
/// exactly how this bug survived its first test.
#[test]
fn a_resampled_normalized_export_still_lands_on_its_dbtp_target() {
    use crate::support::render_normalized_to_file;
    use tutti_buffers::Interleaved;
    use tutti_export::Normalize;
    use tutti_measure::{LoudnessConfig, measure_loudness};
    use tutti_units::Db;

    // Energy near Nyquist, with its true peak *between* samples, is what SRC
    // overshoots on; a DC constant barely moves and would hide the bug entirely.
    //
    // A sine at a quarter of the 44.1 kHz render rate, started at 0.546 turns
    // (a band-limited square at fs/4 keeps no harmonic below Nyquist but its
    // fundamental, so it renders the same). At that phase the samples are ±0.285 and ±0.958 of the peak
    // (±0.279 / ±0.939 at this amplitude of 0.98).
    //
    // The property that matters is where the meter looks. It estimates true
    // peak on a 4× oversampled grid, one point every 1/16 turn of this tone,
    // and from 0.546 turns the nearest grid point misses the crest by 0.0165
    // turns — so the render-rate estimate reads low, and a gain chosen from it
    // lands high once the resampled file puts samples closer to the crest.
    // At a start of 1/8 turn the grid lands exactly on the crest, the estimate
    // is already right, and the test passes with the measure-before-resample
    // bug put back. So the phase is load-bearing; the amplitude is not, since
    // peak normalization divides it out.
    let square = || {
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
        let tone = Osc::sine(Hz::new(11025.0))
            .with_phase(tutti_units::Phase::new(0.546))
            .with_amplitude(Amplitude::new(0.98))
            .with_layout(ChannelLayout::STEREO);
        let id = g.add(tone);
        g.pipe_output(id);
        built(g, RATE)
    };

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("resampled.wav");

    let mut cfg = config(
        Format::Wav(Pcm::Float(FloatWidth::F32)),
        ChannelLayout::STEREO,
    );
    cfg.resample = Some(Resample::to(tutti_units::SampleRate::SR_48K));

    render_normalized_to_file(
        square(),
        &cfg,
        &FrozenClock,
        Normalize::peak(Db::new(-1.0)),
        &path,
    )
    .unwrap();

    // Re-measure the FILE, at the rate it was actually written at.
    let reader = hound::WavReader::open(&path).unwrap();
    let rate = reader.spec().sample_rate;
    assert_eq!(rate, 48_000, "the resample must have reached the file");
    let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.unwrap()).collect();

    let meter = LoudnessConfig::new(Bed::Stereo.topology());
    let measured = measure_loudness(
        &meter,
        Interleaved::new(&samples, ChannelLayout::STEREO).unwrap(),
        tutti_units::SampleRate::of(rate as f64),
    )
    .expect("stereo at 48k is measurable");

    assert!(
        (measured.true_peak.get() - (-1.0)).abs() < 0.05,
        "written file must sit at the -1 dBTP target, measured {:?} — a gain \
         chosen before the resample lands ~0.16 dB high",
        measured.true_peak
    );
}

/// A signal the meter cannot read is an error, not a silently un-normalized
/// file. `Written` carries no field saying the gain was skipped, so reporting
/// success would lose the fact entirely.
#[test]
fn an_unmeasurable_rate_fails_rather_than_writing_un_normalized_audio() {
    use crate::support::render_normalized_to_file;
    use tutti_export::Normalize;
    use tutti_units::Db;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unmeasurable.wav");

    // R128 accepts 16 Hz - 2.8 MHz; 4 MHz is outside it.
    let mut cfg = config(
        Format::Wav(Pcm::Float(FloatWidth::F32)),
        ChannelLayout::STEREO,
    );
    cfg.render.sample_rate = tutti_units::SampleRate::of(4_000_000.0);
    cfg.render.duration_seconds = Seconds::new(0.0005);

    let err = render_normalized_to_file(
        dc(cfg.render.sample_rate),
        &cfg,
        &FrozenClock,
        Normalize::peak(Db::new(-1.0)),
        &path,
    )
    .expect_err("an unmeasurable rate must not report success");

    assert!(
        matches!(err, tutti_export::RenderError::Unmeasurable { .. }),
        "expected Unmeasurable, got {err:?}"
    );
}

/// A main output whose channel count is not one of 1/2/4/6/8/12 (3, 5, 7 or 9
/// wide) exports at that width: the width is runtime, and a caller passes
/// `ChannelLayout::from(main_output_width)` straight through.
///
/// Asserted through the file header and the samples, not the return value: "no
/// error" would also pass if the export quietly wrote a stereo file.
#[test]
fn an_unnamed_main_output_width_exports_at_that_width() {
    let d = tempfile::tempdir().unwrap();

    for width in [3u16, 5, 7, 9] {
        let layout = ChannelLayout::from(width);
        assert!(
            !matches!(
                layout,
                ChannelLayout::MONO | ChannelLayout::STEREO | ChannelLayout::QUAD
            ),
            "width {width} should be an unnamed layout"
        );

        // A graph as wide as the file, carrying a distinct constant per channel so
        // a dropped or duplicated channel is visible.
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, layout);
        for c in 0..width as usize {
            let id = g.add(Const::mono(0.1 + 0.05 * c as f32));
            g.connect_output(id, 0, c);
        }

        let p = d.path().join(format!("w{width}.wav"));
        render_to_file(
            built(g, RATE),
            &config(Format::Wav(Pcm::Float(FloatWidth::F32)), layout),
            &FrozenClock,
            &p,
        )
        .unwrap_or_else(|e| panic!("width {width} failed to export: {e}"));

        let reader = hound::WavReader::open(&p).unwrap();
        assert_eq!(
            reader.spec().channels,
            width,
            "the file must carry {width} channels"
        );

        let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.unwrap()).collect();
        assert!(!samples.is_empty(), "width {width} wrote no audio");
        assert_eq!(
            samples.len() % width as usize,
            0,
            "width {width}: not a whole number of frames"
        );

        // Every channel carries its OWN constant — proof the frames are
        // interleaved at the right stride, which is the failure mode a
        // frame-index-vs-sample-index slip at an odd width would produce.
        let mid = samples.len() / width as usize / 2;
        for c in 0..width as usize {
            let got = samples[mid * width as usize + c];
            let want = 0.1 + 0.05 * c as f32;
            assert!(
                (got - want).abs() < 1e-3,
                "width {width} channel {c}: got {got}, expected {want}"
            );
        }
    }
}

/// The same widths through the in-memory path and back out, so `write_buffers`
/// and `render_to_buffers` are covered too — all three entry points went through
/// the same rejecting dispatch.
#[test]
fn an_odd_width_round_trips_through_buffers() {
    use support::write_buffers;

    let d = tempfile::tempdir().unwrap();
    for width in [3u16, 5] {
        let layout = ChannelLayout::from(width);
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, layout);
        for c in 0..width as usize {
            let id = g.add(Const::mono(0.25));
            g.connect_output(id, 0, c);
        }

        let cfg = config(Format::Wav(Pcm::Float(FloatWidth::F32)), layout);
        let rendered = render_to_buffers(built(g, RATE), &cfg, &FrozenClock)
            .unwrap_or_else(|e| panic!("width {width}: render_to_buffers failed: {e}"));
        assert_eq!(rendered.channels(), width as usize);
        assert_eq!(rendered.layout(), layout);

        let p = d.path().join(format!("buf{width}.wav"));
        write_buffers(&rendered, &cfg, &p)
            .unwrap_or_else(|e| panic!("width {width}: write_buffers failed: {e}"));
        assert_eq!(hound::WavReader::open(&p).unwrap().spec().channels, width);
    }
}

/// An odd width must survive a resample too — that path deinterleaves into
/// planes and re-interleaves them, so it is where a stride mistake at a width
/// that is rarely exercised would surface.
#[test]
fn an_odd_width_survives_a_resample() {
    let d = tempfile::tempdir().unwrap();
    let width = 5u16;
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::from(width));
    for c in 0..width as usize {
        let id = g.add(Const::mono(0.1 + 0.05 * c as f32));
        g.connect_output(id, 0, c);
    }

    let mut cfg = config(
        Format::Wav(Pcm::Float(FloatWidth::F32)),
        ChannelLayout::from(width),
    );
    cfg.resample = Some(Resample::to(SampleRate::SR_48K));

    let p = d.path().join("resampled5.wav");
    render_to_file(built(g, RATE), &cfg, &FrozenClock, &p).expect("a 5-wide resampled export");

    let reader = hound::WavReader::open(&p).unwrap();
    assert_eq!(reader.spec().channels, width);
    assert_eq!(reader.spec().sample_rate, 48_000);

    let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.unwrap()).collect();
    let frames = samples.len() / width as usize;
    assert!(frames > 0, "resampled export wrote no audio");
    // Mid-file, past the resampler's transient: each channel still carries its
    // own constant, so the re-interleave used the right stride.
    let mid = frames / 2;
    for c in 0..width as usize {
        let got = samples[mid * width as usize + c];
        let want = 0.1 + 0.05 * c as f32;
        assert!(
            (got - want).abs() < 1e-2,
            "resampled channel {c}: got {got}, expected {want}"
        );
    }
}

/// Zero channels is the one width a render still refuses — the re-purposed
/// `UnsupportedChannels`. `Multi(0)` is a representable `ChannelLayout` (the
/// empty bus), so it has to be rejected somewhere rather than dividing by zero
/// in the frame stride.
#[test]
fn a_zero_width_export_is_rejected_not_a_divide_by_zero() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("zero.wav");
    let err = render_to_file(
        dc(RATE),
        &config(
            Format::Wav(Pcm::Float(FloatWidth::F32)),
            ChannelLayout::EMPTY,
        ),
        &FrozenClock,
        &p,
    )
    .expect_err("a zero-channel render must not report success");
    assert!(
        matches!(err, tutti_export::RenderError::UnsupportedChannels),
        "expected UnsupportedChannels, got {err:?}"
    );
}

/// A five-wide graph exported to a file narrower than it, where either width has
/// no channel order, is refused with `ConfigError::Placement` rather than cut to
/// its first channels; widened (or into an ordered file it folds into) it
/// renders.
#[test]
fn a_graph_with_no_place_in_the_file_is_refused() {
    use tutti_export::{ConfigError, RenderError};
    let five = || {
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::from(5u16));
        let id = g.add(Const::new(0.25, ChannelLayout::from(5u16)));
        g.pipe_output(id);
        built(g, RATE)
    };
    for width in [1u16, 2, 3, 4] {
        let err = render_to_buffers(
            five(),
            &config(
                Format::Wav(Pcm::Float(FloatWidth::F32)),
                ChannelLayout::from(width),
            ),
            &FrozenClock,
        );
        assert!(
            matches!(
                err,
                Err(RenderError::InvalidConfig(ConfigError::Placement(
                    tutti_buffers::FoldError::Unplaced { channel, .. }
                ))) if channel == usize::from(width)
            ),
            "five into {width}: {:?}",
            err.map(|_| ())
        );
    }
    for width in [5u16, 6, 8] {
        let ok = render_to_buffers(
            five(),
            &config(
                Format::Wav(Pcm::Float(FloatWidth::F32)),
                ChannelLayout::from(width),
            ),
            &FrozenClock,
        );
        assert!(ok.is_ok(), "five into {width}");
    }
}

/// A four-wide graph whose channels are 0.1, 0.2, 0.3, 0.4.
fn four_buses(main_output: Option<ChannelTopology>) -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::QUAD);
    let id = g.add(Const::frame(&[0.1, 0.2, 0.3, 0.4]));
    g.pipe_output(id);
    let setup = match main_output {
        Some(main_output) => Setup::new().main_output(main_output),
        None => Setup::new(),
    };
    built_with(g, RATE, setup)
}

/// The first frame of every plane of a render.
fn first_frame(graph: &mut RenderGraph, width: u16) -> Result<Vec<f32>, tutti_export::RenderError> {
    let cfg = config(
        Format::Wav(Pcm::Float(FloatWidth::F32)),
        ChannelLayout::from(width),
    );
    let out = render_to_buffers(graph, &cfg, &FrozenClock)?;
    Ok(out.planes().iter().map(|p| p[0]).collect())
}

/// A placement given to the setup is what the file is placed from: four `Unassigned` buses into
/// a six-channel file keep channels 2 and 3 at 2 and 3 (without it, the graph's
/// declared quad puts its rears on the side pair at 4 and 5), a given 5.1 folds
/// into stereo by the 5.1 rules, and a placement the file cannot take is
/// `ConfigError::Placement`, and one that is not as wide as the graph
/// `RenderError::TopologyWidth`.
#[test]
fn a_declared_main_output_is_what_the_file_is_placed_from() {
    use tutti_buffers::FoldError;
    use tutti_buffers::{ChannelTopology, Speaker};
    use tutti_export::ConfigError;
    let mut graph = four_buses(None);
    assert_eq!(
        first_frame(&mut graph, 6).unwrap(),
        [0.1, 0.2, 0.0, 0.0, 0.3, 0.4]
    );
    let mut graph = four_buses(Some(
        ChannelTopology::unassigned(ChannelLayout::QUAD).unwrap(),
    ));
    assert_eq!(
        first_frame(&mut graph, 6).unwrap(),
        [0.1, 0.2, 0.3, 0.4, 0.0, 0.0]
    );
    // Four buses cannot be placed in a stereo file.
    assert!(matches!(
        first_frame(&mut graph, 2),
        Err(tutti_export::RenderError::InvalidConfig(
            ConfigError::Placement(FoldError::Unplaced { channel: 2, .. })
        ))
    ));
    // A declaration of another width than the graph is refused.
    let mut graph = four_buses(Some(
        ChannelTopology::unassigned(ChannelLayout::from(6u16)).unwrap(),
    ));
    assert!(matches!(
        first_frame(&mut graph, 6),
        Err(tutti_export::RenderError::TopologyWidth { .. })
    ));
    // Declared as a 5.1 (six wide), a six-channel graph folds to stereo by it.
    let six_channels = |main_output: ChannelTopology| {
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::from(6u16));
        let id = g.add(Const::frame(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]));
        g.pipe_output(id);
        built_with(g, RATE, Setup::new().main_output(main_output))
    };
    let swapped = tutti_buffers::Bed::Surround51.topology();
    assert!(swapped.index_of(Speaker::FrontLeft).is_some());
    let mut six = six_channels(swapped);
    let m = <f32 as tutti_buffers::Float>::MINUS_3_DB;
    let stereo = first_frame(&mut six, 2).unwrap();
    assert_eq!(stereo, [1.0 + m * 3.0 + m * 5.0, 2.0 + m * 3.0 + m * 6.0]);
    // The same six, the front pair declared the other way round.
    let swapped = ChannelTopology::new([
        Speaker::FrontRight,
        Speaker::FrontLeft,
        Speaker::FrontCenter,
        Speaker::LowFrequency,
        Speaker::SideLeft,
        Speaker::SideRight,
    ])
    .unwrap();
    let mut six = six_channels(swapped);
    assert_eq!(
        first_frame(&mut six, 2).unwrap(),
        [2.0 + m * 3.0 + m * 5.0, 1.0 + m * 3.0 + m * 6.0]
    );
}

/// `render_to_buffers` of silence gives exactly 0.0 at an integer depth with
/// dither on: dither belongs to the encode boundary, not to the planes a meter
/// reads.
#[test]
fn silence_planes_carry_no_dither() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let id = g.add(Const::frame(&[0.0, 0.0]));
    g.pipe_output(id);
    let cfg = config(
        Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int16))),
        ChannelLayout::STEREO,
    );
    let out = render_to_buffers(built(g, RATE), &cfg, &FrozenClock).unwrap();
    assert!(out.frames().get() > 0);
    assert!(out.planes().iter().flatten().all(|s| s.to_bits() == 0));
}

/// Rings `ceiling` frames at most, and reports `report` each block (nothing
/// when `None`).
#[derive(Clone)]
struct Narrowing {
    ceiling: Tail,
    report: Option<Tail>,
}

impl Node for Narrowing {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO).tail(self.ceiling)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        if let Some(report) = self.report {
            io.report_tail(report);
        }
        let (ins, mut outs) = io.split();
        if ins.get(0).iter().all(|&x| x == 0.0) {
            return Status::Silent;
        }
        outs.get(0).copy_from_slice(ins.get(0));
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A mono source that is silent, and says so.
#[derive(Clone)]
struct Silence;

impl Node for Silence {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }
    fn reset(&mut self) {}
}

fn frames_tail(frames: usize) -> Tail {
    Tail::Finite(Samples::new(frames))
}

/// A mono constant of `level` (silence, for 0) through `nodes` in series, each
/// with its declared ceiling and what it reports.
fn narrowing_chain(level: f32, nodes: &[(Tail, Option<Tail>)]) -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let source = if level == 0.0 {
        g.add(ForkByClone(Silence))
    } else {
        g.add(Const::mono(level))
    };
    through(g, source, nodes)
}

/// `source` through `nodes` in series, to the output.
fn through(
    mut g: GraphBuilder,
    source: tutti_graph::NodeKey,
    nodes: &[(Tail, Option<Tail>)],
) -> RenderGraph {
    let mut at = source;
    for &(ceiling, report) in nodes {
        let node = g.add(ForkByClone(Narrowing { ceiling, report }));
        g.connect(at, 0, node, 0);
        at = node;
    }
    g.pipe_output(at);
    built(g, RATE)
}

/// A mono source of 0.5 for its first `loud` frames and silence after.
#[derive(Clone)]
struct Burst {
    loud: usize,
    at: usize,
}

impl Node for Burst {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let out = io.output(0);
        for (i, x) in out.iter_mut().enumerate() {
            *x = if self.at + i < self.loud { 0.5 } else { 0.0 };
        }
        let silent = self.at >= self.loud;
        self.at += out.len();
        if silent {
            Status::Silent
        } else {
            Status::Modified
        }
    }
    fn reset(&mut self) {
        self.at = 0;
    }
}

/// The frames of `graph`'s 0.2 s render (8,820 at 44.1 kHz) with `tail`.
fn tailed_frames(graph: RenderGraph, tail: TailPolicy) -> usize {
    let mut cfg = config(
        Format::Wav(Pcm::Float(FloatWidth::F32)),
        ChannelLayout::MONO,
    );
    cfg.render.tail = tail;
    render_to_buffers(graph, &cfg, &FrozenClock)
        .unwrap()
        .frames()
        .get()
}

/// 0.2 s at 44.1 kHz.
const PROGRAMME: usize = 8_820;

fn reported(ceiling: usize) -> TailPolicy {
    TailPolicy::Reported {
        ceiling: Samples::new(ceiling),
        kept_latency: Samples::ZERO,
    }
}

/// **An export spends the tail its nodes report at the programme's end**, not
/// the ceiling they declare: the node declares 4,000 frames and reports 100.
/// The source is a constant that keeps sounding past the programme, which
/// does not lengthen the tail: every later ask bounds it by more than the first.
#[test]
fn export_spends_the_tail_its_nodes_report_at_the_programmes_end() {
    let graph = narrowing_chain(0.5, &[(frames_tail(4_000), Some(frames_tail(100)))]);
    assert_eq!(graph.declared_tail().samples(), Some(Samples::new(4_000)));
    assert_eq!(tailed_frames(graph, reported(4_000)), PROGRAMME + 100);
}

/// A node fed by one that still owes rings out its whole ceiling after it: the
/// reported figure says what its own last input left, not what the upstream's
/// remaining output will excite. Upstream reports 300 (ceiling 5,000), the
/// node after it declares 4,000 and reports 100: 300 + 4,000.
#[test]
fn a_node_fed_by_one_that_still_owes_spends_its_ceiling_after_it() {
    let graph = narrowing_chain(
        0.5,
        &[
            (frames_tail(5_000), Some(frames_tail(300))),
            (frames_tail(4_000), Some(frames_tail(100))),
        ],
    );
    assert_eq!(graph.declared_tail().samples(), Some(Samples::new(9_000)));
    assert_eq!(
        tailed_frames(graph, reported(9_000)),
        PROGRAMME + 300 + 4_000
    );
}

/// A node whose input went quiet owes what its report has left: its input is
/// silent from frame 5,120 (five 1,024-frame blocks) and the programme ends at
/// frame 8,820, so three whole quiet blocks and the last block's 628 frames
/// have passed (3,700) of the 5,000 it reports: 1,300 are left.
#[test]
fn a_node_whose_input_went_quiet_spends_what_is_left_of_its_report() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let source = g.add(ForkByClone(Burst { loud: 5_120, at: 0 }));
    let graph = through(g, source, &[(frames_tail(6_000), Some(frames_tail(5_000)))]);
    assert_eq!(tailed_frames(graph, reported(6_000)), PROGRAMME + 1_300);
}

/// Passes its input, then rings `T` frames of 0.25 after the input stops, and
/// reports `T` under a larger ceiling.
#[derive(Clone)]
struct Rings {
    ring: usize,
    left: usize,
}

impl Node for Rings {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO).tail(frames_tail(self.ring + 5_000))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.report_tail(frames_tail(self.ring));
        let (ins, mut outs) = io.split();
        for (o, &i) in outs.get(0).iter_mut().zip(ins.get(0)) {
            *o = if i != 0.0 {
                self.left = self.ring;
                i
            } else if self.left > 0 {
                self.left -= 1;
                0.25
            } else {
                0.0
            };
        }
        Status::Modified
    }
    fn reset(&mut self) {
        self.left = 0;
    }
}

/// Delays its input by `D` frames and declares that latency.
#[derive(Clone)]
struct Delay {
    line: Vec<f32>,
    at: usize,
}

impl Node for Delay {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO)
            .latency(tutti_units::Latency::new(Samples::new(self.line.len())))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        for (o, &i) in outs.get(0).iter_mut().zip(ins.get(0)) {
            *o = std::mem::replace(&mut self.line[self.at], i);
            self.at = (self.at + 1) % self.line.len();
        }
        Status::Modified
    }
    fn reset(&mut self) {
        self.line.fill(0.0);
        self.at = 0;
    }
}

/// The ring-out of a node reaches the output through the latency after it: a
/// source ends exactly at the programme's end, a node rings 3,000 frames
/// after it and reports so, and a node of 2,048 frames' latency follows. The
/// ring leaves the graph 2,048 frames later than it is made, so the export
/// pulls 2,048 + 3,000 frames past the programme, keeps the 3,000 (the
/// latency is trimmed) and its last frame is the ring's last. A query taken
/// after the latency's flush would find the ring node 1,652 quiet frames in
/// and cut the end of it.
#[test]
fn a_ring_out_reaches_the_output_through_the_latency_after_it() {
    const RING: usize = 3_000;
    const DELAY: usize = 2_048;
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let source = g.add(ForkByClone(Burst {
        loud: PROGRAMME,
        at: 0,
    }));
    let rings = g.add(ForkByClone(Rings {
        ring: RING,
        left: 0,
    }));
    let delay = g.add(ForkByClone(Delay {
        line: vec![0.0; DELAY],
        at: 0,
    }));
    g.connect(source, 0, rings, 0);
    g.connect(rings, 0, delay, 0);
    g.pipe_output(delay);
    let graph = built(g, RATE);
    assert_eq!(graph.reported_latency(), Samples::new(DELAY));

    let mut cfg = config(
        Format::Wav(Pcm::Float(FloatWidth::F32)),
        ChannelLayout::MONO,
    );
    cfg.render.latency = graph.reported_latency();
    cfg.render.tail = reported(9_000);
    let out = render_to_buffers(graph, &cfg, &FrozenClock).unwrap();
    let plane = &out.planes()[0];
    assert_eq!(plane.len(), PROGRAMME + RING);
    assert_eq!(plane[PROGRAMME + RING - 1], 0.25, "the last ring frame");
    assert_eq!(plane[PROGRAMME - 1], 0.5, "the last frame of the programme");
}

/// A node whose input has been silent for its whole tail by the programme's
/// end is skipped by the executor and owes nothing: the render ends with the
/// programme, though the node declares 4,000 frames and reports 100.
#[test]
fn a_node_at_rest_at_the_programmes_end_adds_no_tail() {
    let graph = narrowing_chain(0.0, &[(frames_tail(4_000), Some(frames_tail(100)))]);
    assert_eq!(graph.declared_tail().samples(), Some(Samples::new(4_000)));
    assert_eq!(tailed_frames(graph, reported(4_000)), PROGRAMME);
}

/// A report that never decays is spent at the job's ceiling, and a report
/// above a finite declared ceiling is clamped to that ceiling when the job's
/// own is larger.
#[test]
fn an_unbounded_report_spends_the_ceiling_it_is_under() {
    let graph = narrowing_chain(0.5, &[(Tail::Unbounded, Some(Tail::Unbounded))]);
    assert!(graph.reported_tail().is_unbounded());
    assert_eq!(tailed_frames(graph, reported(2_000)), PROGRAMME + 2_000);

    let graph = narrowing_chain(0.5, &[(frames_tail(4_000), Some(Tail::Unbounded))]);
    assert_eq!(tailed_frames(graph, reported(10_000)), PROGRAMME + 4_000);
}

/// A node that never reports is spent at its declared ceiling, and the job's
/// ceiling bounds it.
#[test]
fn a_node_that_never_reports_spends_its_ceiling() {
    let graph = narrowing_chain(0.5, &[(frames_tail(4_000), None)]);
    assert_eq!(tailed_frames(graph, reported(10_000)), PROGRAMME + 4_000);

    let graph = narrowing_chain(0.5, &[(frames_tail(4_000), None)]);
    assert_eq!(tailed_frames(graph, reported(1_500)), PROGRAMME + 1_500);
}

/// An instrument with an event input: it sounds (0.5) through the programme
/// up to frame `sounding`, then is silent and says [`Status::Idle`] (the block
/// after its last sound), under a declared tail of 20,000 frames.
#[derive(Clone)]
struct Release {
    sounding: usize,
    at: usize,
}

impl Node for Release {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
            .events(1, 0)
            .tail(frames_tail(20_000))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let out = io.output(0);
        let idle = self.at >= self.sounding;
        for (i, x) in out.iter_mut().enumerate() {
            *x = if self.at + i < self.sounding {
                0.5
            } else {
                0.0
            };
        }
        self.at += out.len();
        if idle { Status::Idle } else { Status::Modified }
    }
    fn reset(&mut self) {
        self.at = 0;
    }
}

/// **The export asks again as the tail renders.** An instrument (declared
/// tail 20,000) sounds 1,024 frames past the programme and is idle from the
/// block after; a node after it rings 300 frames and reports so (declared
/// tail 5,300). One ask at the programme's end charges 20,000 + 5,300 (the
/// instrument's ceiling, then the node's, fed by one that owes): the render
/// would be 8,820 + 25,300. The tail is the smallest bound seen, each ask
/// bounding it by what is spent plus what is owed: the first gives 25,300; one
/// block (1,024) later the instrument has just sounded its last, so the bound
/// is 1,024 + 25,300 and the first stands; after a second block (2,048 spent)
/// the instrument has said idle and the node after it, no longer fed, owes
/// its report of 300 (its input has been quiet 1,024 frames, past its report,
/// but its last block was not flagged silent, so it owes the whole report):
/// the bound is 2,048 + 300 = 2,348. The render is 8,820 + 2,348.
#[test]
fn a_bounce_after_an_instrument_in_release_ends_at_the_release_and_the_reported_tail() {
    const RELEASE: usize = 1_024;
    const RING: usize = 300;
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let instrument = g.add(ForkByClone(Release {
        sounding: PROGRAMME + RELEASE,
        at: 0,
    }));
    let rings = g.add(ForkByClone(Rings {
        ring: RING,
        left: 0,
    }));
    g.connect(instrument, 0, rings, 0);
    g.pipe_output(rings);
    let graph = built(g, RATE);
    assert_eq!(
        graph.declared_tail().samples(),
        Some(Samples::new(20_000 + RING + 5_000))
    );
    assert_eq!(
        tailed_frames(graph, reported(40_000)),
        PROGRAMME + 2 * RELEASE + RING
    );
}

/// A node that never says idle spends the ceiling and no more: the ceiling
/// (2,500 frames, two blocks and 452) is not a multiple of the block, and the
/// node declares 20,000. (The unbounded report below is the same rule for a
/// node whose figure is the cap; this one is a finite figure that never falls.)
#[test]
fn a_node_that_never_rests_spends_the_ceiling_and_no_more() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let instrument = g.add(ForkByClone(Release {
        sounding: usize::MAX,
        at: 0,
    }));
    g.pipe_output(instrument);
    assert_eq!(
        tailed_frames(built(g, RATE), reported(2_500)),
        PROGRAMME + 2_500
    );
}

/// A normalized export renders once and measures and writes what it spilled:
/// both passes cover the frames the render kept (programme and reported tail),
/// as does the file, and the rendering stage ends at that true total.
#[test]
fn the_passes_of_a_normalized_export_agree_on_the_reported_length() {
    let graph = narrowing_chain(0.5, &[(frames_tail(4_000), Some(frames_tail(100)))]);
    let mut cfg = config(
        Format::Wav(Pcm::Float(FloatWidth::F32)),
        ChannelLayout::MONO,
    );
    cfg.render.tail = reported(4_000);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("n.wav");
    let mut seen = Vec::new();
    let mut watch = |p: &Progress| seen.push(*p);
    let token = tutti_export::CancelToken::new();
    let mut control = Control::new(&token).on_progress(&mut watch);
    let written = support::render_file_under(
        graph,
        &cfg,
        &FrozenClock,
        Some(Normalize::peak(Db::new(-6.0))),
        &path,
        &mut control,
    )
    .unwrap()
    .into_finished()
    .expect("nothing cancels it");
    let kept = Samples::new(PROGRAMME + 100);
    let last = |stage| {
        seen.iter()
            .rfind(|p| p.stage() == stage)
            .and_then(Progress::frames)
    };
    assert_eq!(last(Stage::Rendering), Some((kept, kept)), "the render");
    assert_eq!(last(Stage::Measuring), Some((kept, kept)), "the measure");
    assert_eq!(last(Stage::Writing), Some((kept, kept)), "the write");
    assert_eq!(written.frames, kept);
    assert_eq!(
        hound::WavReader::open(&path).unwrap().duration(),
        kept.get() as u32
    );
}

/// A one-channel convolver over `ir`, with the default head partition.
fn convolver(ir: &[f32]) -> tutti_nodes::ConvolverNode<f32> {
    tutti_nodes::ConvolverNode::try_from(tutti_nodes::ConvolverConfig::new(
        tutti_buffers::ChannelTopology::MONO,
        &[Mono::new(ir)],
    ))
    .expect("one response on a mono bus")
}
