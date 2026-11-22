//! [`AutomationLaneNode`]: a curve evaluated at the transport's beat, as a
//! graph node.

use std::sync::Arc;

use tutti_buffers::ChannelLayout;
use tutti_buffers::Float;
use tutti_graph::{
    Cx, ForkCause, ForkSource, Forked, IntoNode, Io, Node, NodeParts, Prepare, RenderMode, Shape,
    Status, control_in,
};
use tutti_units::{Beat, Latency, Tail};

use super::Curve;

/// An automation lane that evaluates a [`Curve`] against musical time.
///
/// A generator: no inputs, one output, the curve's value at the transport
/// beat of every frame, read from the block's `Env`
/// ([`tutti_graph::Env::for_each_beat`]). The lane holds no transport and
/// needs no edge: it is a pure function of the beat its block hands it, which
/// makes it per-sample accurate and lets an offline render drive it from its
/// own transport without any special casing. The beat is already
/// loop-wrapped by the transport, so the lane does not consult a loop range;
/// while the transport is stopped the lane holds the value at its beat.
///
/// **Arrival.** It has no inputs, so its compiled arrival is zero by
/// construction and the beat it reads is its block's own; a consumer behind
/// a latent path is aligned by the compiler, which delays this node's edge
/// into it like any other source's.
///
/// The curve is held behind `Arc<dyn Curve>` so the node stays cheap to clone
/// and agnostic to how the curve is stored — an [`Trajectory`], a
/// constant, an LFO shape.
///
/// **Audio, not a param source.** It renders the curve as audio, one value
/// per frame, for anything that reads a signal (a node's audio input, a
/// meter). A param port is driven by a [`ControlLaneNode`](super::ControlLaneNode) instead
/// (`UnitLaneConfig` over
/// [`ControlProgram::from_curve`](tutti_modulation::ControlProgram::from_curve)),
/// which sends the curve as curve-segment events
/// ([`ParamSegment`](tutti_graph::ParamSegment)).
///
/// # In a graph
///
/// A graph node ([`IntoNode`]) with no controls: a live curve swap is a
/// respawn. A fork reads
/// the curve's [`frozen`](Curve::frozen) copy when it has one (a curve that
/// reads state written after it was built), and shares the `Arc` otherwise
/// (an trajectory, read-only once shared). It is a source with no ring-out
/// ([`Tail::None`]): its output is a function of the beat, so a render's end
/// ends it.
///
/// [`Trajectory`]: tutti_curve::Trajectory
///
/// # Example
///
/// A four-beat ramp from silence to unity, read at beat 2 (the transport
/// stopped there), which is halfway along it.
///
/// ```
/// use tutti_buffers::MaxBlock;
/// use tutti_units::{Amplitude, Beat, Bpm, SampleRate, Samples};
/// use tutti_graph::{Prepare, Renderer, TransportPosition};
/// use tutti_nodes::automation::{AutomationLaneConfig, AutomationLaneNode, Breakpoint, Trajectory};
///
/// let mut trajectory: Trajectory<Amplitude> = Trajectory::new();
/// trajectory.add_breakpoint(Breakpoint::new(Beat::new(0.0), Amplitude::new(0.0)));
/// trajectory.add_breakpoint(Breakpoint::new(Beat::new(4.0), Amplitude::new(1.0)));
///
/// // No inputs, one output (the curve's value).
/// let lane = AutomationLaneNode::from(AutomationLaneConfig::new(trajectory));
/// let (mut renderer, ..) = Renderer::<f32>::alone(lane, Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(64))));
/// renderer.set_transport(TransportPosition::new(false, Bpm::of(120.0), Beat::new(2.0), None));
/// let out = renderer.render(1);
/// assert!((out[0][0] - 0.5).abs() < 1e-3, "midpoint of a 0..1 ramp, got {}", out[0][0]);
/// ```
pub struct AutomationLaneNode {
    curve: Arc<dyn Curve>,
    last_value: f64,
}

impl std::fmt::Debug for AutomationLaneNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutomationLaneNode")
            .field("last_value", &self.last_value)
            .finish_non_exhaustive()
    }
}

/// Another name for [`AutomationLaneNode`], kept for consumers that spell
/// it this way. The lane is not generic over the trajectory's label.
pub type LiveAutomationLane = AutomationLaneNode;

/// What an [`AutomationLaneNode`] is made from: the curve it evaluates at the transport beat.
///
/// The curve is held behind an `Arc` and read on the audio thread, so it must be cheap to
/// evaluate and must not allocate in `value_at`.
#[derive(Clone)]
pub struct AutomationLaneConfig {
    curve: Arc<dyn Curve>,
}

impl std::fmt::Debug for AutomationLaneConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutomationLaneConfig")
            .finish_non_exhaustive()
    }
}

impl AutomationLaneConfig {
    /// A lane that evaluates `curve` at the transport beat.
    #[must_use]
    pub fn new(curve: impl Curve + 'static) -> Self {
        Self {
            curve: Arc::new(curve),
        }
    }
}

impl From<AutomationLaneConfig> for AutomationLaneNode {
    fn from(config: AutomationLaneConfig) -> Self {
        Self {
            curve: config.curve,
            last_value: 0.0,
        }
    }
}

impl AutomationLaneNode {
    /// Evaluates the curve at `beat`.
    ///
    /// Returns `0.0` where the curve has no value — before its first point, or
    /// on an empty trajectory.
    pub fn value_at(&self, beat: Beat) -> f64 {
        self.curve.value_at(beat).unwrap_or(0.0)
    }

    /// A copy of this lane for a fork: the curve's
    /// [`frozen`](Curve::frozen) copy when it has one, the shared `Arc`
    /// otherwise (read-only once shared), and no last value.
    #[must_use]
    pub fn fork_fresh(&self) -> Self {
        Self {
            curve: self
                .curve
                .frozen()
                .unwrap_or_else(|| Arc::clone(&self.curve)),
            last_value: 0.0,
        }
    }
}

impl<F: Float> Node<F> for AutomationLaneNode {
    /// One audio output.
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
            .cost(tutti_graph::Cost::Moderate)
            .tail(Tail::None)
    }

    /// Nothing is derived from the rate: the beat arrives in each block's
    /// `Env`.
    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        debug_assert_eq!(
            cx.arrival,
            Latency::ZERO,
            "a node with no inputs arrives at zero"
        );
        let out = io.output(0);
        let curve = &*self.curve;
        let mut last = self.last_value;
        // A curve's value is the target's `f64` quantity, written in the
        // graph's float.
        cx.env.for_each_beat(|i, beat| {
            last = curve.value_at(beat).unwrap_or(0.0);
            out[i] = control_in(last);
        });
        self.last_value = last;
        Status::Modified
    }

    fn reset(&mut self) {
        self.last_value = 0.0;
    }
}

/// The fork source of a lane: [`AutomationLaneNode::fork_fresh`] of the lane
/// as inserted.
struct LaneFork(AutomationLaneNode);

impl<F: Float> ForkSource<F> for LaneFork {
    fn fork(&self, _mode: RenderMode) -> Result<Forked<F>, ForkCause> {
        Ok(Forked::new(Box::new(self.0.fork_fresh())))
    }
}

/// Inserted with no controls and a fork that freezes a live curve.
impl<F: Float> IntoNode<F> for AutomationLaneNode {
    type Controls = ();

    fn into_parts(self) -> NodeParts<(), F> {
        let fork = LaneFork(self.clone());
        NodeParts::new(Box::new(self), (), Some(Box::new(fork)))
    }
}

impl Clone for AutomationLaneNode {
    fn clone(&self) -> Self {
        Self {
            curve: Arc::clone(&self.curve),
            last_value: self.last_value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_float::AtomicF64;
    use std::sync::atomic::Ordering;
    use tutti_buffers::MaxBlock;
    use tutti_curve::{Breakpoint, Trajectory};
    use tutti_graph::{Renderer, TransportPosition};
    use tutti_units::{Bpm, Hz, SampleRate, Samples};

    fn ramp_trajectory() -> Trajectory<Hz> {
        let mut env: Trajectory<Hz> = Trajectory::new();
        env.add_breakpoint(Breakpoint::new(Beat::new(0.0), Hz::new(0.0)));
        env.add_breakpoint(Breakpoint::new(Beat::new(4.0), Hz::new(1.0)));
        env.add_breakpoint(Breakpoint::new(Beat::new(8.0), Hz::new(0.5)));
        env
    }

    /// `frames` of `lane` through a graph at 48 kHz in one block, the
    /// transport `transport` at the block's start.
    fn render(lane: AutomationLaneNode, transport: TransportPosition, frames: usize) -> Vec<f32> {
        let (mut solo, ..) = Renderer::alone(
            lane,
            Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(frames))),
        );
        solo.set_transport(transport);
        solo.render(frames).remove(0)
    }

    fn stopped_at(beat: f64) -> TransportPosition {
        TransportPosition::new(false, Bpm::of(120.0), Beat::new(beat), None)
    }

    #[test]
    fn declares_no_inputs_and_one_output() {
        let shape = Node::<f32>::shape(&AutomationLaneNode::from(AutomationLaneConfig::new(
            ramp_trajectory(),
        )));
        assert_eq!(shape.audio_in().count(), 0);
        assert_eq!(shape.audio_out().count(), 1);
        assert_eq!(
            shape.tail,
            Tail::None,
            "a beat-driven source has no ring-out"
        );
    }

    #[test]
    fn test_update_tracks_beat_position() {
        let lane = AutomationLaneNode::from(AutomationLaneConfig::new(ramp_trajectory()));

        assert!((lane.value_at(Beat::new(0.0)) - 0.0).abs() < 0.01);
        assert!((lane.value_at(Beat::new(2.0)) - 0.5).abs() < 0.01);
        assert!((lane.value_at(Beat::new(4.0)) - 1.0).abs() < 0.01);
        assert!((lane.value_at(Beat::new(6.0)) - 0.75).abs() < 0.01);
    }

    /// The transport hands an already-wrapped beat, so a lane at beat 6
    /// behaves identically whether or not a loop produced it.
    #[test]
    fn wrapped_beat_needs_no_loop_handling_in_the_lane() {
        let lane = AutomationLaneNode::from(AutomationLaneConfig::new(ramp_trajectory()));
        // A 4..8 loop wraps beat 10 to beat 6 in the transport.
        let wrapped = lane.value_at(Beat::new(6.0));
        assert!(
            (wrapped - 0.75).abs() < 0.01,
            "expected ~0.75, got {wrapped}"
        );
    }

    /// The node renders the curve at its block's beat.
    #[test]
    fn a_block_reads_the_curve_at_the_transport_beat() {
        let at = |beat| {
            render(
                AutomationLaneNode::from(AutomationLaneConfig::new(ramp_trajectory())),
                stopped_at(beat),
                1,
            )[0]
        };
        assert!((at(4.0) - 1.0).abs() < 0.01);
        assert!((at(0.0) - 0.0).abs() < 0.01);
        assert!((at(2.0) - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_reset_clears_last_value() {
        let mut flat: Trajectory<Hz> = Trajectory::new();
        flat.add_breakpoint(Breakpoint::new(Beat::new(0.0), Hz::new(0.5)));
        flat.add_breakpoint(Breakpoint::new(Beat::new(8.0), Hz::new(0.5)));
        let mut d = tutti_graph::contract::Direct::new(
            AutomationLaneNode::from(AutomationLaneConfig::new(flat)),
            SampleRate::SR_48K,
            64,
        );
        d.block();
        assert!((d.node.last_value - 0.5).abs() < 0.01);

        Node::<f32>::reset(&mut *d.node);
        assert_eq!(d.node.last_value, 0.0);
    }

    #[test]
    fn test_empty_trajectory_returns_zero() {
        let empty: Trajectory<Hz> = Trajectory::new();
        let lane = AutomationLaneNode::from(AutomationLaneConfig::new(empty));
        assert_eq!(lane.value_at(Beat::new(5.0)), 0.0);
        let empty: Trajectory<Hz> = Trajectory::new();
        let out = render(
            AutomationLaneNode::from(AutomationLaneConfig::new(empty)),
            stopped_at(5.0),
            1,
        );
        assert_eq!(out[0], 0.0);
    }

    /// A held beat holds the value for the whole block.
    #[test]
    fn test_process_fills_block_from_beat_input() {
        let out = render(
            AutomationLaneNode::from(AutomationLaneConfig::new(ramp_trajectory())),
            stopped_at(4.0),
            32,
        );
        for (i, val) in out.iter().enumerate() {
            assert!(
                (val - 1.0).abs() < 0.01,
                "Sample {i} expected ~1.0, got {val}"
            );
        }
    }

    /// After a block, `last_value` is the block's last frame.
    #[test]
    fn test_process_updates_last_value() {
        let mut flat: Trajectory<Hz> = Trajectory::new();
        flat.add_breakpoint(Breakpoint::new(Beat::new(0.0), Hz::new(0.5)));
        flat.add_breakpoint(Breakpoint::new(Beat::new(8.0), Hz::new(0.5)));
        let mut d = tutti_graph::contract::Direct::new(
            AutomationLaneNode::from(AutomationLaneConfig::new(flat)),
            SampleRate::SR_48K,
            64,
        );
        assert_eq!(d.node.last_value, 0.0);
        d.block();
        assert!(
            (d.node.last_value - 0.5).abs() < 0.01,
            "Expected ~0.5, got {}",
            d.node.last_value
        );
    }

    /// The whole point of the per-frame beat: the value moves WITHIN a
    /// block, following the transport, rather than being held
    /// block-constant.
    #[test]
    fn test_process_per_sample_varies() {
        // 120 BPM at 48 kHz, rolling from beat 0.
        let rolling = TransportPosition::new(true, Bpm::of(120.0), Beat::new(0.0), None);
        let out = render(
            AutomationLaneNode::from(AutomationLaneConfig::new(ramp_trajectory())),
            rolling,
            32,
        );

        assert!((out[0] - 0.0).abs() < 0.001);
        let last = out[31];
        assert!(last > 0.0, "Expected > 0.0, got {last}");
        assert!(last < 0.01, "Expected small value, got {last}");
        for i in 1..32 {
            assert!(
                out[i] >= out[i - 1],
                "Sample {i} should be >= sample {}",
                i - 1
            );
        }
    }

    /// A curve that reads state written after it was built, and freezes it.
    struct Live(Arc<AtomicF64>);

    struct Fixed(f64);

    impl Curve for Fixed {
        fn value_at(&self, _: Beat) -> Option<f64> {
            Some(self.0)
        }
    }

    impl Curve for Live {
        fn value_at(&self, _: Beat) -> Option<f64> {
            Some(self.0.load(Ordering::Acquire))
        }

        fn frozen(&self) -> Option<Arc<dyn Curve>> {
            Some(Arc::new(Fixed(self.0.load(Ordering::Acquire))))
        }
    }

    /// A fork reads the curve's frozen copy: a write the live curve sees
    /// after the fork does not reach it.
    #[test]
    fn a_fork_freezes_a_live_curve() {
        let cell = Arc::new(AtomicF64::new(0.25));
        let lane = AutomationLaneNode::from(AutomationLaneConfig::new(Live(Arc::clone(&cell))));
        let fork = lane.fork_fresh();
        cell.store(0.75, Ordering::Release);
        assert_eq!(render(fork, stopped_at(1.0), 1)[0], 0.25);
        assert_eq!(render(lane, stopped_at(1.0), 1)[0], 0.75);
    }

    /// The lane writes the curve's `f64` value as it is in an `f64` graph, and
    /// narrowed once in an `f32` one.
    #[test]
    fn the_lane_writes_its_curve_value_at_the_graphs_width() {
        use crate::test_support::{RATE, render_f64};
        let out = render_f64(
            AutomationLaneNode::from(AutomationLaneConfig::new(Fixed(0.1))),
            RATE,
            &[],
            32,
        );
        assert!(out[0].iter().all(|&v| v == 0.1f64));
        assert_eq!(control_in::<f32>(0.1), 0.1f32);
    }
}
