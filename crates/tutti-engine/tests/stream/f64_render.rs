//! An engine built at `f64` renders end to end in `f64`, and the device edge
//! converts the graph's format to the device's by the rule in `Sample`.

use crate::fixtures::{SAMPLE_RATE, spec};
use std::sync::Arc;
use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_buffers::{Float, Sample, SampleFormat};
use tutti_engine::{AudioCallbackState, Engine, ManualStreamDriver, OutputBlock, OutputStream};
use tutti_graph::{
    Cx, Editor, ForkByClone, Io, Node, NodeKey, OutputPort, PortSource, Prepare, Shape, Status,
};
use tutti_timeline::Transport;
use tutti_units::{SampleRate, Samples};

/// One part in 2^40 above one: past `f32`'s 24-bit significand, inside `f64`'s 53.
const FINE: f64 = 1.0 + 1.0 / 1_099_511_627_776.0;

/// A constant source in any sample format.
#[derive(Clone)]
struct Dc(f64);

impl<F: Float> Node<F> for Dc {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        io.output(0).fill(F::from_f64(self.0));
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// An `f64` engine whose graph holds `level` on each of `outputs` channels.
fn dc_state(level: f64, outputs: usize) -> Arc<AudioCallbackState<f64>> {
    let transport = Transport::new(SampleRate::of(SAMPLE_RATE));
    let (mut ed, exec) = Editor::<f64>::with_format(Prepare::new(
        SampleRate::of(SAMPLE_RATE),
        MaxBlock::of(Samples::new(512)),
    ));
    ed.insert(NodeKey(1), "dc", ForkByClone(Dc(level)));
    ed.set_outputs(vec![
        PortSource::Node(OutputPort::new(NodeKey(1), 0));
        outputs
    ])
    .expect("the sources exist");
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    Box::leak(Box::new(ed));
    Arc::new(AudioCallbackState::new(engine))
}

/// **A stream over an `f64` engine hands back the figure `f32` rounds away.**
///
/// The constant is one part in 2^40 above one: through the executor, the fold
/// into the device, the declick fader and the output stage it comes out of the
/// manual stream exactly, in `f64`.
#[test]
fn an_f64_stream_renders_a_dc_figure_f32_cannot_hold() {
    // Setup: `f32` loses the figure, so a pass is not a rounded one.
    assert_ne!(f64::from(FINE as f32), FINE, "FINE is not an f32");
    let mut output = OutputStream::new(spec(2, SampleFormat::F64));
    let (driver, stream) = ManualStreamDriver::new();
    output
        .start_with(dc_state(FINE, 2), driver)
        .expect("the manual driver opens");

    let block: Vec<f64> = stream
        .render_block(Samples::new(64))
        .expect("the stream is open");
    assert_eq!(block.len(), 64 * 2);
    assert!(
        block.iter().all(|&x| x == FINE),
        "every sample is FINE exactly: {:?}",
        &block[..4]
    );
}

/// **The device edge converts the graph's format by the rule in `Sample`.**
///
/// A constant 0.5 is `0.5 * 2^15` in `i16` and `0.5 * 2^31` in `i32`; a level
/// past full scale holds at the format's maximum; `f32` is the rounded
/// figure and `f64` the exact one.
#[test]
fn an_f64_block_converts_to_the_device_format_by_the_sample_rule() {
    let mut block = OutputBlock::new(dc_state(0.5, 2), ChannelLayout::STEREO);
    let mut i16s = [0i16; 8];
    block.render(&mut i16s);
    assert_eq!(i16s, [1 << 14; 8], "0.5 * 2^15");
    let mut i32s = [0i32; 8];
    block.render(&mut i32s);
    assert_eq!(i32s, [1 << 30; 8], "0.5 * 2^31");

    let mut loud = OutputBlock::new(dc_state(2.0, 2), ChannelLayout::STEREO);
    let mut held = [0i16; 8];
    loud.render(&mut held);
    assert_eq!(held, [i16::MAX; 8], "past full scale holds at the maximum");

    let mut fine = OutputBlock::new(dc_state(FINE, 2), ChannelLayout::STEREO);
    let mut f32s = [0.0f32; 4];
    fine.render(&mut f32s);
    assert_eq!(f32s, [1.0f32; 4], "an f32 device holds the rounded figure");
    let mut f64s = [0.0f64; 4];
    fine.render(&mut f64s);
    assert_eq!(f64s, [FINE; 4], "an f64 device holds it exactly");
}

/// **A driver's own conversion maps each rendered `f64` and silences what the
/// graph did not render.** An offset-binary `u16` device, the `Sample` rule on
/// `i16` with the sign bit flipped: `0.5` is `32768 + 2^14`, and the samples of
/// a callback that is not whole frames past the last frame are the format's
/// silence (mid-scale).
#[test]
fn a_driver_conversion_maps_the_rendered_samples_and_silences_a_partial_frame() {
    let mut block = OutputBlock::new(dc_state(0.5, 2), ChannelLayout::STEREO);
    let offset_binary = |x: f64| (x.convert_to::<i16>() as u16) ^ 0x8000;
    let mut out = [0u16; 7];
    block.render_with(&mut out, offset_binary);
    assert_eq!(out[..6], [49152; 6], "three frames of 0.5");
    assert_eq!(out[6], 32768, "the half frame is silence, mid-scale");
}
