//! The inlet: audio from outside the graph, copied in by a node.
//!
//! The inverse of a [tap](crate::TapReader): a tap copies a graph signal out
//! into a ring a thread of your own drains, and an inlet drains a ring a
//! thread of your own fills (a capture device's input callback, a network
//! receiver) into the graph. [`InletNode`] is the node: no inputs, one output
//! a channel of its ring, nothing else. It is device-free, so it sits anywhere
//! in the graph, and wiring it through effects makes the outside signal heard
//! live and effected.
//!
//! # The ring
//!
//! The ring is an [`InletRing`] built by [`InletRing::pair`], with the
//! [`TapWriter`] the producer keeps: one lane a channel, frames written and
//! read whole, so a frame cannot tear across channels, and a frame the ring
//! has no room for is refused whole and counted by the writer.
//!
//! # No resampling, no clock
//!
//! **An inlet does not resample.** Its producer must run at the graph's rate:
//! `tutti-session`'s `Graph::add_inlet` refuses one that does not, once, when
//! it opens: the ring carries the rate its producer fills it at ([`InletRing::rate`]),
//! stated where the ring is made. After that two clocks are two clocks: the producer's and the graph's cannot be locked, so they
//! drift, however slowly. Nothing here corrects it; what fills or empties the
//! ring is counted, never blocked on:
//!
//! - a producer faster than the graph finds the ring full and the writer
//!   refuses whole frames, counting them lost;
//! - a producer slower than the graph leaves the ring empty and the node
//!   writes silence for the frames it lacks.
//!
//! A ring a few blocks deep keeps both rare; it adds that depth of latency.
//!
//! # One consumer
//!
//! Popping advances the read index, so exactly one party may pop. The ring is
//! popped only from [`Node::process`], and there is one node:
//!
//! - the node is **not `Clone`**, so no second consumer can be made from it:
//!
//!   ```compile_fail
//!   fn second_consumer(node: &tutti_graph::InletNode) -> tutti_graph::InletNode {
//!       node.clone()
//!   }
//!   ```
//!
//! - it is **unforkable**: its [`IntoNode`] hands the editor no fork source, so
//!   a fork of a graph that needs it (an export of the live input) is
//!   [`ForkError::NotForkable`](crate::ForkError::NotForkable) naming its key,
//!   never a copy that would take live frames beside it;
//!
//! - [`reset`](Node::reset) pops nothing: the frames in the ring are the
//!   producer's, not state of the node's.
//!
//! There is no gain: a level belongs in a node downstream of this one.

use tutti_buffers::ChannelLayout;
use tutti_buffers::Float;
use tutti_units::{SampleRate, Samples, Tail};

use crate::edges::tap::{TapReader, TapWriter};
use crate::node::cost::Cost;
use crate::node::io::Io;
use crate::node::rates::SampleRates;
use crate::node::shape::Shape;
use crate::node::{Cx, IntoNode, Node, NodeParts, Prepare, Status};

/// The reader half of an inlet's ring, with the rate its producer fills it at.
///
/// An inlet does not resample, so the rate the producer runs at is what the
/// graph must run at. It is stated once, where the ring is made, and travels
/// with the ring to `tutti-session`'s `Graph::add_inlet`, which refuses a rate
/// that is not the graph's: it cannot be given apart from the ring, and so
/// cannot be given wrong.
///
/// ```
/// use tutti_buffers::Interleaved;
/// use tutti_graph::InletRing;
/// use tutti_units::{SampleRate, Samples};
/// use tutti_buffers::ChannelLayout;
///
/// let (ring, mut producer) =
///     InletRing::pair(ChannelLayout::STEREO, Samples::new(64), SampleRate::SR_48K);
/// producer.push_interleaved(Interleaved::new(&[0.5, -0.5], ChannelLayout::STEREO)?);
/// assert_eq!(ring.rate(), SampleRate::SR_48K);
/// assert_eq!(ring.layout(), ChannelLayout::STEREO);
/// # Ok::<(), tutti_buffers::BufferError>(())
/// ```
#[derive(Debug)]
pub struct InletRing<F: Float = f32> {
    reader: TapReader<F>,
    rate: SampleRate,
}

impl<F: Float> InletRing<F> {
    /// A ring of `width` channels and `capacity` frames, its producer running
    /// at `rate`, and the writer the producer fills it with. Allocates; control
    /// side.
    #[must_use = "the writer is the only way to fill the ring"]
    pub fn pair(width: ChannelLayout, capacity: Samples, rate: SampleRate) -> (Self, TapWriter<F>) {
        let (reader, writer) = TapReader::pair(usize::from(width.count()), capacity);
        (Self { reader, rate }, writer)
    }

    /// The rate the producer fills the ring at.
    #[must_use]
    pub fn rate(&self) -> SampleRate {
        self.rate
    }

    /// The ring's width: one channel a lane.
    #[must_use]
    pub fn layout(&self) -> ChannelLayout {
        self.reader.layout()
    }
}

/// A node that copies a ring of its own into the graph: no inputs, one output
/// a channel of the ring.
///
/// Each block it pops one frame per output frame from its ring; an empty ring
/// (the producer has not pushed yet, or the graph outran it) yields silence
/// for the frames it lacks rather than stalling. If the node is handed a
/// number of outputs other than the ring's width (unreachable through a
/// graph, which gives it the outputs it declared) it writes silence on all of
/// them and leaves the ring alone, so no captured frame is thrown away
/// unheard.
///
/// It does not resample and has no clock of its own: see the
/// module docs. Its [`shape`](Node::shape) allows only its ring's rate, so a
/// graph re-prepared to another rate is refused naming it
/// ([`SetRateError::RateUnsupported`](crate::SetRateError::RateUnsupported)). Real-time safe: `process` does not allocate or lock.
/// It reports an unbounded tail, so the executor never skips it as silent (a
/// skipped block would leave its frames to pile up in the ring).
///
/// Not `Clone`, and inserted **unforkable**.
///
/// # Examples
///
/// ```
/// use tutti_buffers::Interleaved;
/// use tutti_graph::{InletNode, InletRing};
/// use tutti_units::{SampleRate, Samples};
/// use tutti_buffers::ChannelLayout;
///
/// let (ring, mut producer) =
///     InletRing::pair(ChannelLayout::STEREO, Samples::new(64), SampleRate::SR_48K);
/// let node = InletNode::new(ring);
/// let frame = Interleaved::new(&[0.5, -0.5], ChannelLayout::STEREO)?;
/// producer.push_interleaved(frame); // one stereo frame, from any thread
/// assert_eq!(node.layout(), ChannelLayout::STEREO);
/// # Ok::<(), tutti_buffers::BufferError>(())
/// ```
#[derive(Debug)]
pub struct InletNode<F: Float = f32> {
    ring: InletRing<F>,
}

impl<F: Float> InletNode<F> {
    /// An inlet over `ring`, as wide as the ring and running at its rate.
    #[must_use]
    pub fn new(ring: InletRing<F>) -> Self {
        Self { ring }
    }

    /// The ring's width: one output a channel.
    #[must_use]
    pub fn layout(&self) -> ChannelLayout {
        self.ring.layout()
    }
}

impl<F: Float> Node<F> for InletNode<F> {
    /// As wide as the ring and running at its rate only: the ring is filled at
    /// that rate and nothing resamples. A live source: [`Tail::Unbounded`], so
    /// the executor never skips it as silent.
    fn shape(&self) -> Shape {
        let rate = self.ring.rate();
        Shape::source_unassigned(self.ring.layout())
            .with_rates(SampleRates::of(rate, rate))
            .tail(Tail::Unbounded)
            // A block copy out of the ring: measured 0.4-0.6 ns per frame and
            // channel, against a gain's 0.05 and `SvfFilterNode`'s 4.0 on the
            // same box, under the boundary.
            .cost(Cost::Light)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let (_, mut outputs) = io.split();
        // Width check BEFORE any pop: popping consumes from the ring, so
        // popping and then declining to write would throw frames away. A
        // block this node cannot fill is silence, written, not left as the
        // arena held it.
        if outputs.len() != usize::from(self.ring.layout().count()) || outputs.is_empty() {
            outputs.iter_mut().for_each(|plane| plane.fill(F::ZERO));
            return Status::Modified;
        }
        // The frames the ring holds, then silence for the rest.
        let popped = outputs
            .planes_mut()
            .and_then(|planes| self.ring.reader.pop_planes(planes))
            .map_or(0, Samples::get);
        for plane in outputs.iter_mut() {
            plane[popped..].fill(F::ZERO);
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}

/// Inserted **unforkable**: no fork source, so a fork of a graph that needs
/// the inlet fails naming it (see the module docs). No controls.
impl<F: Float> IntoNode<F> for InletNode<F> {
    type Controls = ();

    fn into_parts(self) -> NodeParts<(), F> {
        NodeParts::new(Box::new(self), (), None, None)
    }
}

#[cfg(test)]
mod tests;
