//! Offline render throughput — how much faster than realtime a bounce runs.
//!
//! The number to read is × realtime: a case rendering 1 second of audio
//! in 10 ms runs at 100× realtime, so an hour-long project bounces in about
//! 36 seconds. Criterion's `Throughput::Elements(frames)` prints elem/s;
//! divide by the sample rate.
//!
//! # What is measured, and what is deliberately not
//!
//! `render/` drives a render into a `BufferTarget` — no file, no encoder, no
//! filesystem. That is the part this crate owns and the part a change here
//! can regress.
//!
//! `encode/` writes real files into a `tempfile::TempDir`, and is
//! track-only: never gate it. Its cost is dominated by the page cache and
//! by third-party encoders (`hound`, `flacenc`), so a regression
//! in it usually says something about the machine. It is here because the
//! render/encode split is the useful diagnostic: when an export is slow,
//! `encode` minus `render` at matching settings says which half to look at.
//!
//! Durations are kept short on purpose, except in `resample-normalized/`,
//! whose point is how the cost grows with the duration. Once the rendered planes get large
//! the working set leaves criterion's domain (wall-clock spread of 81× has
//! been measured at that boundary), and the honest instrument becomes
//! `--profile-time` plus samply.

use std::hint::black_box;
use tutti_buffers::ChannelSet;
use tutti_codec::Encoding;
use tutti_codec::ToFile;
use tutti_units::Seconds;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tutti_buffers::ChannelLayout;
use tutti_buffers::FloatWidth;
use tutti_codec::IntDepth;
use tutti_codec::IntPcm;
use tutti_codec::{Format, Pcm, Resample};
use tutti_export::{
    BufferTarget, Control, FileTarget, Gain, Job, Normalize, Output, Outputs, RenderGraph,
    Rendered, Setup, TailPolicy, Written, render,
};
use tutti_graph::GraphBuilder;
use tutti_nodes::testing::{Const, Osc};
use tutti_timeline::FrozenClock;
use tutti_units::{Amplitude, Db, Hz, SampleRate, Samples};

const SR: f64 = 48_000.0;

/// `g`, built for an export at `rate` — the config's render rate, which the
/// graph must be prepared at.
///
/// Built per iteration: an export consumes its graph, so the build (preparing every unit, compiling the plan) is part of
/// what a bounce costs.
fn built(g: GraphBuilder, rate: f64) -> RenderGraph {
    let (editor, executor) = g
        .build(RenderGraph::prepare(SampleRate::of(rate)))
        .expect("builds");
    RenderGraph::assemble(editor, executor, Setup::new()).expect("built together")
}

fn tone_graph(rate: f64) -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let id = g.add(
        Osc::sine(Hz::new(440.0))
            .with_amplitude(Amplitude::new(0.5))
            .with_layout(ChannelLayout::STEREO),
    );
    g.pipe_output(id);
    built(g, rate)
}

fn dc_graph(rate: f64) -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let id = g.add(Const::frame(&[0.25, 0.25]));
    g.pipe_output(id);
    built(g, rate)
}

/// One render and one file: what these cases vary.
#[derive(Clone)]
struct Case {
    job: Job,
    format: Format,
    channels: ChannelLayout,
    resample: Option<Resample>,
}

impl Case {
    fn job(&self) -> Job {
        self.job
    }

    fn file_target(
        &self,
        path: impl Into<std::path::PathBuf>,
        normalize: Option<Normalize>,
    ) -> FileTarget {
        let encoding = Encoding::new(self.format).channels(ChannelSet::default_for(self.channels));
        let encoding = match self.resample {
            Some(resample) => encoding.resample(resample),
            None => encoding,
        };
        let target = FileTarget::new(ToFile::new(path, encoding));
        match normalize {
            Some(normalize) => target.normalize(Gain::Own(normalize)),
            None => target,
        }
    }

    fn buffer_target(&self) -> BufferTarget {
        BufferTarget::new(self.channels)
    }
}

fn config(seconds: f64, rate: f64, channels: ChannelLayout, format: Format) -> Case {
    let rate = SampleRate::of(rate);
    Case {
        job: Job::new(
            rate,
            Seconds::new(seconds).to_samples(rate),
            Samples::new(0),
            TailPolicy::Frames(Samples::ZERO),
        )
        .expect("a render"),
        format,
        channels,
        resample: None,
    }
}

/// `cfg`'s render into memory.
fn to_buffers(mut graph: RenderGraph, cfg: &Case) -> Rendered {
    let mut outputs = Outputs::new();
    let planes = outputs.buffers(Output::main_output(cfg.buffer_target()));
    let done = render(
        &mut graph,
        &cfg.job(),
        &FrozenClock,
        outputs,
        &mut Control::none(),
    )
    .expect("renders")
    .into_finished()
    .expect("nothing cancels a control with no token");
    done.buffers(&planes)
        .expect("a key of these outputs")
        .clone()
}

/// `cfg`'s render into the file at `path`, normalized by `normalize` if given.
fn to_file(
    mut graph: RenderGraph,
    cfg: &Case,
    normalize: Option<Normalize>,
    path: &std::path::Path,
) -> Written {
    let mut outputs = Outputs::new();
    let file = outputs.file(Output::main_output(cfg.file_target(path, normalize)));
    let done = render(
        &mut graph,
        &cfg.job(),
        &FrozenClock,
        outputs,
        &mut Control::none(),
    )
    .expect("writes")
    .into_finished()
    .expect("nothing cancels a control with no token");
    done.file(&file).expect("a key of these outputs").clone()
}

/// Render throughput by duration, width and rate. No file touched.
fn bench_render(c: &mut Criterion) {
    let mut group = c.benchmark_group("render");

    for seconds in [0.25f64, 1.0] {
        let cfg = config(
            seconds,
            SR,
            ChannelLayout::STEREO,
            Format::Wav(Pcm::Float(FloatWidth::F32)),
        );
        let frames = (seconds * SR) as u64;
        group.throughput(Throughput::Elements(frames));
        group.bench_with_input(
            BenchmarkId::new("stereo-48k", format!("{seconds}s")),
            &cfg,
            |b, cfg| b.iter(|| black_box(to_buffers(tone_graph(SR), cfg))),
        );
    }

    // Width: the graph work is constant, the planes are not.
    for (name, layout) in [
        ("mono", ChannelLayout::MONO),
        ("stereo", ChannelLayout::STEREO),
        ("quad", ChannelLayout::from(4u16)),
        ("5.1", ChannelLayout::from(6u16)),
    ] {
        let cfg = config(0.25, SR, layout, Format::Wav(Pcm::Float(FloatWidth::F32)));
        group.throughput(Throughput::Elements((0.25 * SR) as u64));
        group.bench_with_input(BenchmarkId::new("width", name), &cfg, |b, cfg| {
            b.iter(|| black_box(to_buffers(dc_graph(SR), cfg)))
        });
    }

    // Rate: twice the samples for the same wall-clock duration.
    for rate in [48_000.0f64, 96_000.0] {
        let cfg = config(
            0.25,
            rate,
            ChannelLayout::STEREO,
            Format::Wav(Pcm::Float(FloatWidth::F32)),
        );
        group.throughput(Throughput::Elements((0.25 * rate) as u64));
        group.bench_with_input(BenchmarkId::new("rate", rate as u64), &cfg, |b, cfg| {
            b.iter(|| black_box(to_buffers(tone_graph(rate), cfg)))
        });
    }

    group.finish();
}

/// Encoder cost, by format. Track-only — never gate this.
fn bench_encode(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut group = c.benchmark_group("encode");
    group.throughput(Throughput::Elements((0.25 * SR) as u64));

    let formats: [(&str, Format); 2] = [
        (
            "wav",
            Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int24).no_dither())),
        ),
        (
            "flac",
            Format::Flac {
                pcm: IntPcm::new(IntDepth::Int24).no_dither(),
                level: tutti_codec::FlacLevel::default(),
            },
        ),
    ];

    for (name, format) in formats {
        // Int24 throughout, FLAC being an integer codec: one depth across the
        // group keeps the formats comparable.
        let cfg = config(0.25, SR, ChannelLayout::STEREO, format);
        let path = dir.path().join(format!("bench.{name}"));
        group.bench_with_input(BenchmarkId::from_parameter(name), &cfg, |b, cfg| {
            b.iter(|| black_box(to_file(tone_graph(SR), cfg, None, &path)))
        });
    }
    group.finish();
}

/// The two-pass path's sample-rate conversion over a long render: a
/// normalizing output that converts 44.1 kHz to 48 kHz and measures the peak
/// at the file's rate. That path reads the render back through the resampler
/// twice (measured, then written), so its cost over the duration is what
/// shows whether the conversion is linear. Long durations on purpose, unlike the rest of this
/// file: the scaling is the measurement. Writes a WAV like `encode/`, so read
/// it for the trend across durations, not as a gate.
fn bench_resample(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut group = c.benchmark_group("resample-normalized");
    group.sample_size(10);
    let from = 44_100.0f64;
    for seconds in [30.0f64, 60.0, 180.0] {
        let mut cfg = config(
            seconds,
            from,
            ChannelLayout::STEREO,
            Format::Wav(Pcm::Float(FloatWidth::F32)),
        );
        cfg.resample = Some(Resample::to(SampleRate::of(SR)));
        let path = dir.path().join("resampled.wav");
        group.throughput(Throughput::Elements((seconds * from) as u64));
        group.bench_with_input(
            BenchmarkId::new("44k1-to-48k", format!("{seconds}s")),
            &cfg,
            |b, cfg| {
                b.iter(|| {
                    black_box(to_file(
                        dc_graph(from),
                        cfg,
                        Some(Normalize::Peak {
                            target: Db::new(-1.0),
                        }),
                        &path,
                    ))
                })
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_render, bench_encode, bench_resample);
criterion_main!(benches);
