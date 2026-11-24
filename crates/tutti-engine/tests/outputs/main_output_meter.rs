//! The main output meter is an op in the plan, so whatever renders the graph feeds
//! it: the level of each graph output channel, before the engine's fold to the
//! device and its declick. These moved here from `tutti-cpal`'s output-block
//! tests, where the callback fed a stereo-folded meter; the fold tests became
//! per-channel ones (the meter does not fold).

use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_buffers::MaxBlock;
use tutti_engine::Engine;
use tutti_graph::{Editor, MeterReader, Prepare};
use tutti_graph::{MeterId, MeterSpec, NodeKey};
use tutti_graph::{OutputPort, PortSource};
use tutti_timeline::Transport;
use tutti_units::{SampleRate, Samples};

const RATE: f64 = 48_000.0;

/// A mono source writing a constant.
struct Dc(f32);

impl tutti_graph::Node for Dc {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        io.output(0).fill(self.0);
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// An engine over a constant `level` on output `channel` of `outputs` (silence
/// on the rest), or on every output when `channel` is `None`, and the main output
/// meter declared on its outputs (off until enabled).
fn engine(
    level: f32,
    channel: Option<usize>,
    outputs: usize,
    capacity: Samples,
) -> (Engine, MeterReader) {
    let transport = Transport::new(SampleRate::of(RATE));
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(RATE),
        MaxBlock::of(Samples::new(512)),
    ));
    ed.insert(NodeKey(1), "dc", tutti_graph::Unforkable(Dc(level)));
    ed.wire_outputs((0..outputs).map(|c| {
        if channel.is_none_or(|only| only == c) {
            PortSource::Node(OutputPort::new(NodeKey(1), 0))
        } else {
            PortSource::Zero
        }
    }))
    .unwrap();
    let meter = ed
        .open_meter(MeterId::of(1), MeterSpec::Outputs)
        .expect("the sources exist");
    ed.commit().expect("commits");
    let engine = Engine::with_capacity(&transport, &mut ed, exec, capacity).expect("within limits");
    // The engine's editor half outlives the test; nothing commits again.
    Box::leak(Box::new((ed, transport)));
    (engine, meter)
}

/// `(peak, rms)` of every channel, taking what was held.
fn levels(meter: &mut MeterReader) -> Vec<(f64, f64)> {
    meter
        .read()
        .iter()
        .map(|l| (l.peak.get(), l.rms.get()))
        .collect()
}

fn render(engine: &Engine, frames: usize, width: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; frames * width];
    engine.process(
        &mut InterleavedMut::new(
            &mut out,
            ChannelLayout::try_from(width).expect("a test width is small"),
        )
        .unwrap(),
    );
    out
}

/// **A wide device meters every channel, not the first two.**
///
/// A 5.1 mix whose dialogue (the centre) never appears on the meter would be
/// the bug. The signal is put on the **centre** channel with the front pair
/// silent, which is the only arrangement that separates a per-channel meter
/// from a `[..2]` truncation: under a uniform-DC graph the two agree exactly.
/// (This was a stereo fold of the device buffer, `MINUS_3_DB` on each side; the
/// meter now reads the graph's outputs one by one and folds nothing.)
#[test]
fn a_wide_device_meters_each_channel_not_the_first_two() {
    // 5.1 order: L, R, C, LFE, Ls, Rs. Channel 2 is the centre.
    let (engine, mut meter) = engine(1.0, Some(2), 6, Samples::new(512));
    meter.enable();
    let out = render(&engine, 64, 6);

    // The device buffer itself: silent front pair, loud centre.
    assert_eq!(out[0], 0.0, "left must be silent");
    assert_eq!(out[1], 0.0, "right must be silent");
    assert_eq!(out[2], 1.0, "the centre carries the signal");

    assert_eq!(
        levels(&mut meter),
        vec![
            (0.0, 0.0),
            (0.0, 0.0),
            (1.0, 1.0),
            (0.0, 0.0),
            (0.0, 0.0),
            (0.0, 0.0)
        ],
        "the centre reads on its own channel; the rest are silent"
    );
}

/// **The meter measures only while enabled.** A disabled meter reads nothing
/// of a rendered block; once enabled it reads the next one. A stereo device
/// and a wide one are both checked.
#[test]
fn an_enabled_meter_reads_the_block() {
    for width in [2usize, 6] {
        let (engine, mut meter) = engine(0.5, None, width, Samples::new(512));
        render(&engine, 64, width);
        assert_eq!(
            meter.read()[0].peak.get(),
            0.0,
            "width {width}: disabled reads nothing"
        );

        meter.enable();
        render(&engine, 64, width);
        let peak = meter.read()[0].peak.get();
        assert!(peak > 0.0, "width {width}: the enabled meter read {peak}");
    }
}

/// **A stereo block at the `MAX_FRAMES` ceiling (8192) reads its analytic
/// level**: a constant 0.5 reads 0.5 as both peak and RMS, on each channel.
/// The engine runs it as sixteen graph blocks of 512, and the meter reads
/// them as one.
#[test]
fn a_full_block_reads_its_analytic_level() {
    let (engine, mut meter) = engine(0.5, None, 2, Samples::new(8192));
    meter.enable();
    render(&engine, 8192, 2);
    let r = meter.read();
    for (what, channel) in [("left", 0), ("right", 1)] {
        assert_eq!(r[channel].peak.get(), 0.5, "peak {what}");
        assert_eq!(r[channel].rms.get(), 0.5, "rms {what}");
        assert_eq!(r[channel].frames, Samples::new(8192), "frames {what}");
    }
}

/// **The pure render feeds the meter too**: `process_segment` has no declick
/// but is a render a host pulls, and an offline session's handle must not
/// read silence for it.
#[test]
fn the_pure_render_feeds_the_meter() {
    let (engine, mut meter) = engine(0.25, None, 2, Samples::new(512));
    meter.enable();
    let mut out = vec![0.0f32; 64 * 2];
    engine.process_segment(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());
    assert_eq!(meter.read()[0].peak.get(), 0.25);
}

/// **The main output reads before the declick.** A stop that fades the output
/// does not lower the reading of the block in which it fades: the meter sees
/// the graph's signal at its full level while the device buffer ramps to
/// silence.
///
/// The RMS tells them apart (the peak does not: the fade rises back to the
/// source's level within the block).
#[test]
fn the_main_output_reads_before_the_declick() {
    use tutti_timeline::MotionCommand;
    let transport = Transport::new(SampleRate::of(RATE));
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(RATE),
        MaxBlock::of(Samples::new(512)),
    ));
    ed.insert(NodeKey(1), "dc", tutti_graph::Unforkable(Dc(0.5)));
    ed.wire_outputs(vec![PortSource::Node(OutputPort::new(NodeKey(1), 0))])
        .unwrap();
    let mut meter = ed
        .open_meter(MeterId::of(1), MeterSpec::Outputs)
        .expect("the sources exist");
    meter.enable();
    ed.commit().expect("commits");
    let engine =
        Engine::with_capacity(&transport, &mut ed, exec, Samples::new(512)).expect("limits");
    let _ = transport.motion().send(MotionCommand::Play);
    render(&engine, 512, 1);
    assert_eq!(meter.read()[0].peak.get(), 0.5);

    // A declicked stop with no lead time: the output falls to zero over the
    // fade, the graph's own signal does not.
    let _ = transport.motion().send(MotionCommand::stop());
    let out = render(&engine, 512, 1);
    assert_eq!(out[0], 0.0, "the device buffer starts the stop at zero");
    let faded = (out.iter().map(|s| s * s).sum::<f32>() / out.len() as f32).sqrt();
    assert!(faded < 0.45, "the device buffer is faded: rms {faded}");
    let l = meter.read()[0];
    assert_eq!(l.peak.get(), 0.5);
    assert_eq!(
        l.rms.get(),
        0.5,
        "the main output reads the graph, not the faded buffer (rms {faded})"
    );
    Box::leak(Box::new((ed, transport)));
}

/// A source that is loud for its first `loud` frames and quiet after.
struct Step {
    loud: usize,
    at: usize,
}

impl tutti_graph::Node for Step {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        for (k, s) in io.output(0).iter_mut().enumerate() {
            *s = if self.at + k < self.loud { 1.0 } else { 0.1 };
        }
        self.at += io.output(0).len();
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// A block longer than the engine's capacity is rendered in pieces but read
/// as one block: the peak and RMS cover the whole output, a loud start in the
/// first piece included. The capacity is the larger of 512, the prepared
/// block, and what is asked for, so the block is longer than that. 100 frames
/// at 1.0 then 1400 at 0.1 on the centre: the peak is 1.0, the RMS
/// `sqrt((100 + 1400 * 0.01) / 1500)`, and the silent channels read zero.
#[test]
fn a_block_longer_than_the_capacity_is_measured_whole() {
    let transport = Transport::new(SampleRate::of(RATE));
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(RATE),
        MaxBlock::of(Samples::new(512)),
    ));
    ed.insert(
        NodeKey(1),
        "step",
        tutti_graph::Unforkable(Step { loud: 100, at: 0 }),
    );
    ed.wire_outputs((0..6).map(|c| {
        if c == 2 {
            PortSource::Node(OutputPort::new(NodeKey(1), 0))
        } else {
            PortSource::Zero
        }
    }))
    .unwrap();
    let mut meter = ed
        .open_meter(MeterId::of(1), MeterSpec::Outputs)
        .expect("the sources exist");
    meter.enable();
    ed.commit().expect("commits");
    let engine =
        Engine::with_capacity(&transport, &mut ed, exec, Samples::new(64)).expect("within limits");
    render(&engine, 1500, 6);
    let r = meter.read();
    let rms = ((100.0 + 1400.0 * 0.01) / 1500.0f64).sqrt();
    assert!((r[2].peak.get() - 1.0).abs() < 1e-5, "{r:?}");
    assert!((r[2].rms.get() - rms).abs() < 1e-4, "{r:?} want rms {rms}");
    assert_eq!(r[2].frames, Samples::new(1500));
    for c in [0, 1, 3, 4, 5] {
        assert_eq!((r[c].peak.get(), r[c].rms.get()), (0.0, 0.0), "channel {c}");
    }
}

/// **A stereo main output reaches a stereo device unchanged.**
///
/// The engine folds the graph's outputs to the device's width with
/// `fold_to_width` over the engine's order for the main output's width; a stereo main output to a stereo device is the identity, which
/// must stay a passthrough. (This pinned the premise of the meter's
/// stereo shortcut, which went with the fold; the passthrough is still the
/// engine's own.)
#[test]
fn a_stereo_main_output_folds_to_a_stereo_device_unchanged() {
    let mut folded = [0.0f32; 2];
    let stereo = tutti_buffers::ChannelLayout::STEREO;
    let fold = tutti_graph::fold_to_width(
        &tutti_buffers::ChannelTopology::default_for(stereo).unwrap(),
        stereo,
    )
    .unwrap();
    fold.frame(&[0.25, -0.75], &mut folded);
    assert!(fold.is_identity());
    assert_eq!(
        folded,
        [0.25, -0.75],
        "a stereo main output plays unchanged on a stereo device"
    );
}
