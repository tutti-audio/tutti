//! A caller's sink as an output: it receives the frames the render keeps, at the
//! output's width, and a failure of it fails the render before any file lands.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tutti_rt::Mutex;

use tutti_buffers::{ChannelLayout, ChannelSet, FloatWidth, Interleaved, Sink, SinkFailed};
use tutti_codec::{Encoding, Format, Pcm, ToFile};
use tutti_export::{
    Control, FileTarget, Job, Output, Outputs, RenderError, RenderGraph, Setup, SinkTarget,
    TailPolicy, TargetError, render,
};
use tutti_graph::{ForkByClone, GraphBuilder};
use tutti_nodes::testing::Osc;
use tutti_timeline::FrozenClock;
use tutti_units::{Amplitude, Hz, SampleRate, Samples};

const RATE: SampleRate = SampleRate::SR_48K;

/// What a [`Collect`] sink saw, readable after the render consumed it.
#[derive(Default)]
struct Seen {
    samples: Mutex<Vec<f32>>,
    finished: AtomicBool,
}

#[derive(thiserror::Error, Debug)]
#[error("the collector failed")]
struct Failed;

/// A sink that keeps what it is given, and fails on its `fail_at`th write.
struct Collect {
    layout: ChannelLayout,
    seen: Arc<Seen>,
    writes: usize,
    fail_at: Option<usize>,
}

impl Collect {
    fn new(layout: ChannelLayout, seen: &Arc<Seen>) -> Self {
        Self {
            layout,
            seen: Arc::clone(seen),
            writes: 0,
            fail_at: None,
        }
    }
}

impl Sink for Collect {
    type Sample = f32;
    type Error = Failed;

    fn layout(&self) -> ChannelLayout {
        self.layout
    }

    fn write(&mut self, frames: Interleaved<'_, f32>) -> Result<(), SinkFailed> {
        self.writes += 1;
        if self.fail_at == Some(self.writes) {
            return Err(SinkFailed);
        }
        self.seen.samples.lock().extend_from_slice(frames.samples());
        Ok(())
    }

    fn finalize(self) -> Result<(), Failed> {
        self.seen.finished.store(true, Ordering::SeqCst);
        match self.fail_at {
            Some(at) if at <= self.writes => Err(Failed),
            _ => Ok(()),
        }
    }
}

fn sine() -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let osc = g.add(ForkByClone(
        Osc::sine(Hz::new(440.0))
            .with_amplitude(Amplitude::new(0.4))
            .with_layout(ChannelLayout::STEREO),
    ));
    g.connect_output(osc, 0, 0).connect_output(osc, 1, 1);
    let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
    RenderGraph::assemble(editor, executor, Setup::new()).expect("built together")
}

fn job(length: usize, latency: usize) -> Job {
    Job::new(
        RATE,
        Samples::new(length),
        Samples::new(latency),
        TailPolicy::Frames(Samples::ZERO),
    )
    .expect("a render")
}

/// The sink receives exactly the audible span: the latency is trimmed off the head and the
/// length caps the tail, frame for frame the sine's value at its index.
#[test]
fn a_sink_receives_the_trimmed_and_capped_frames_of_the_analytic_signal() {
    let seen = Arc::new(Seen::default());
    let mut outputs = Outputs::<f32>::new();
    outputs.sink(Output::main_output(SinkTarget::new(Collect::new(
        ChannelLayout::STEREO,
        &seen,
    ))));
    render(
        &mut sine(),
        &job(2_000, 1_500),
        &FrozenClock,
        outputs,
        &mut Control::none(),
    )
    .expect("renders")
    .into_finished()
    .expect("not cancelled");
    let got = seen.samples.lock();
    assert_eq!(got.len(), 2_000 * 2);
    assert!(seen.finished.load(Ordering::SeqCst));
    for (i, frame) in got.chunks(2).enumerate() {
        let n = (1_500 + i) as f64;
        let want = 0.4 * (std::f64::consts::TAU * 440.0 * n / 48_000.0).sin();
        for s in frame {
            assert!(
                (f64::from(*s) - want).abs() < 2e-4,
                "frame {i}: {s} vs {want}"
            );
        }
    }
}

/// A sink of another width than the audio is refused before anything renders, never
/// folded or truncated.
#[test]
fn a_sink_of_another_width_is_refused_before_the_render() {
    let seen = Arc::new(Seen::default());
    let mut outputs = Outputs::<f32>::new();
    outputs.sink(Output::main_output(SinkTarget::new(Collect::new(
        ChannelLayout::MONO,
        &seen,
    ))));
    let err = render(
        &mut sine(),
        &job(1_000, 0),
        &FrozenClock,
        outputs,
        &mut Control::none(),
    )
    .map(|_| ())
    .expect_err("refused");
    assert!(
        matches!(
            err,
            RenderError::Target {
                output: 0,
                why: TargetError::SinkWidth { sink, feed },
            } if sink == ChannelLayout::MONO && feed == ChannelLayout::STEREO
        ),
        "{err:?}"
    );
    assert!(seen.samples.lock().is_empty());
    assert!(!seen.finished.load(Ordering::SeqCst));
}

/// A sink whose write fails mid-render fails the render with the sink's own error, and the
/// file output of the same render does not land.
#[test]
fn a_failing_sink_fails_the_render_and_no_file_lands() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("out.wav");
    let seen = Arc::new(Seen::default());
    let mut sink = Collect::new(ChannelLayout::STEREO, &seen);
    sink.fail_at = Some(2);
    let mut outputs = Outputs::<f32>::new();
    let _ = outputs.file(Output::main_output(FileTarget::new(
        ToFile::new(
            &path,
            Encoding::new(Format::Wav(Pcm::Float(FloatWidth::F32)))
                .channels(ChannelSet::default_for(ChannelLayout::STEREO)),
        )
        .replace(),
    )));
    outputs.sink(Output::main_output(SinkTarget::new(sink)));
    let err = render(
        &mut sine(),
        &job(10_000, 0),
        &FrozenClock,
        outputs,
        &mut Control::none(),
    )
    .map(|_| ())
    .expect_err("fails");
    let RenderError::Sink { output, source } = err else {
        panic!("not the sink's error: {err:?}");
    };
    assert_eq!(output, 1);
    assert!(source.is::<Failed>(), "{source:?}");
    assert!(!path.exists());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
