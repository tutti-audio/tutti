//! One activation, both halves held together: [`Active`] is the shared
//! `tutti_fixtures::Active`, and [`Render`] is what a case drives it with.

use tutti_buffers::FloatWidth;
use tutti_buffers::{Float, InputOutput};
use tutti_fixtures::Whole;
use tutti_plugin_types::{
    Loaded, MaxBlock, PluginIo, Prepare, ProcessContext, ProcessOutput, Processor, SampleRate,
    Samples,
};

/// The default rate.
pub const RATE: f64 = 48_000.0;
/// The default block size.
pub const BLOCK: usize = 64;
/// What an output holds before a plugin writes it, so a plugin or host that
/// leaves a channel unwritten shows.
const UNWRITTEN: f64 = 9.0;

/// A `Prepare` for realtime rendering.
///
/// # Panics
///
/// Panics when `block` is zero.
#[must_use]
pub fn prepare(rate: f64, block: usize, format: FloatWidth) -> Prepare {
    let block = MaxBlock::new(Samples(block)).expect("a case renders at least one frame");
    Prepare::new(SampleRate::of(rate), block).with_format(format)
}

/// A sample width a case can render at.
pub trait Width: Float + Into<f64> + From<f32> {}

impl Width for f32 {}

impl Width for f64 {}

/// One activation, both halves held together.
pub type Active<L> = tutti_fixtures::Active<Whole<L>>;

/// Renders blocks through an [`Active`]'s Processor.
pub trait Render {
    /// One block; returns the two output channels.
    ///
    /// # Panics
    ///
    /// When the block does not render.
    fn render<S: Width>(
        &mut self,
        inputs: &[Vec<S>],
        frames: usize,
        ctx: &ProcessContext<'_>,
    ) -> Vec<Vec<S>>;

    /// A block of silence in, the outputs back.
    fn silent(&mut self, frames: usize) -> Vec<Vec<f32>> {
        self.render(
            &[vec![0.0; frames], vec![0.0; frames]],
            frames,
            &ProcessContext::new(),
        )
    }
}

impl<L: Loaded> Render for Active<L> {
    fn render<S: Width>(
        &mut self,
        inputs: &[Vec<S>],
        frames: usize,
        ctx: &ProcessContext<'_>,
    ) -> Vec<Vec<S>> {
        let ins: Vec<&[S]> = inputs.iter().map(Vec::as_slice).collect();
        let mut outs = vec![vec![S::from(UNWRITTEN as f32); frames]; 2];
        let mut refs: Vec<&mut [S]> = outs.iter_mut().map(Vec::as_mut_slice).collect();
        let mut out = ProcessOutput::default();
        let mut io = PluginIo {
            buffer: InputOutput::from_slices(Samples(frames), &ins, &mut refs)
                .expect("equal channels"),
            ctx,
            out: &mut out,
        };
        self.processor()
            .process(&mut io)
            .expect("the block renders");
        outs
    }
}

pub(crate) use tutti_fixtures::main_thread;
