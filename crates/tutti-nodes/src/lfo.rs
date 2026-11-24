//! The audio-rate LFO node.
//!
//! [`LfoNode`] owns everything *audio*: the `tutti_graph::Node` impl, the clock
//! position, the sample rate, the beat read from each block's `Env`. It calls
//! [`Waveform::raw_at`] for the one pure step `cycles -> value`, the same
//! function a control lane evaluates, so a beat-synced node and lane produce
//! one sequence (a random wave included). The wave is a pure function of its
//! position in unwrapped cycles: the cycle index addresses a stepped shape's
//! step, the fraction is the phase, and a negative position reads the end of
//! the cycle. A free-running node runs from its insert and does not restart at
//! play.
//!
//! `LfoShape` and `Waveform` are re-exported so `use tutti_nodes::LfoShape`
//! resolves here.

use tutti_units::UnitParam;

use tutti_buffers::ChannelLayout;
use tutti_buffers::Float;
use tutti_dsp::{Lfo, LfoWave};
use tutti_graph::{
    Continuity, Cx, Env, Io, Node, Offset, Prepare, Shape, Status, TransportPosition,
};
use tutti_rt::{AtomicCell, atomic_word};
use tutti_units::{BeatsPerCycle, Bipolar, Hz, PhaseIncrement, SampleRate, Samples, Seconds};
use tutti_units::{Latency, Tail};

use std::sync::Arc;

// The waveform vocabulary is tutti-modulation's floor, re-exported so `use
// tutti_nodes::LfoShape` resolves.
pub use tutti_modulation::{LfoShape, Waveform};

/// The longest time, after a switch to [`LfoSync::Synced`], that the output of an
/// [`LfoNode`] is not the stateless beat-locked LFO: the time the offset between the free
/// phase it left and the beat lock takes to run down to zero, linearly, from the first
/// frame of the block that reads the switch. From that long after the switch the output
/// is the wave at the transport beat of each frame divided by the span, as it is from the
/// start on a node that was always synced. It is converted to frames at the prepared rate
/// (rounded).
pub const LFO_SYNC_RAMP: Seconds = Seconds::new(0.05);

/// What a running [`LfoNode`]'s phase follows: its own rate, or the transport beat.
///
/// A setting, not a param: it has no port and is not modulated. The node reads the word
/// once per block, so a store lands at the next block's first frame, never mid-block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[repr(u8)]
pub enum LfoSync {
    /// The phase advances from the node's own frame count at the rate in its rate param
    /// ([`UnitParam::Rate`], in [`Hz`]), independent of the transport, and keeps running
    /// while the transport is stopped. A negative rate runs the phase backwards, and 0
    /// holds it: the value at a position is a pure function of it, so either is well
    /// defined. The beat span is not in use, and kept.
    #[default]
    Free,
    /// The phase is read from the transport beat of each frame of the block
    /// ([`tutti_graph::Env::for_each_beat`]), one cycle per [`beats_per_cycle`]
    /// (a larger span is slower). Sample-accurate, and the same offline. The rate
    /// param is not in use, and kept.
    ///
    /// [`beats_per_cycle`]: LfoNode::beats_per_cycle
    Synced,
}

atomic_word!(LfoSync { Free, Synced });

/// The sync's name (`Free Running` or `Beat Synced`), for a UI's selector: display text,
/// not a stable identifier, so do not parse or persist it.
impl core::fmt::Display for LfoSync {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Free => "Free Running",
            Self::Synced => "Beat Synced",
        })
    }
}

/// The offset of a node's phase from the beat lock after a switch to synced, running down
/// to zero over the ramp: the figure at the switch and the frames of the ramp still to
/// run. It is added to the locked position in cycles, `cycles * left / ramp` at a frame
/// with `left` frames still to run.
#[derive(Clone, Copy, Debug)]
struct Carry {
    cycles: f64,
    left: u32,
}

impl Carry {
    const NONE: Self = Self {
        cycles: 0.0,
        left: 0,
    };

    /// The offset in cycles `frames` frames after the frame this carry is at, with
    /// `ramp_inv` the reciprocal of the ramp's frames.
    #[inline]
    fn after(self, frames: u32, ramp_inv: f64) -> f64 {
        self.cycles * f64::from(self.left.saturating_sub(frames)) * ramp_inv
    }
}

/// `frames` as a `u32`, saturating: a block is far shorter.
fn frames_u32(frames: usize) -> u32 {
    u32::try_from(frames).unwrap_or(u32::MAX)
}

/// Whether `next` is where the transport that was `prev` (held for `len` frames at `rate`)
/// goes on to by playing or standing still: not a locate, a loop wrap or a start.
fn continues(
    prev: &(TransportPosition, Samples),
    rate: SampleRate,
    next: &TransportPosition,
) -> bool {
    match prev.0.continuity(prev.1, rate, next) {
        Continuity::Rolled => true,
        Continuity::Held => prev.0.playing == next.playing,
        Continuity::Wrapped | Continuity::Jumped => false,
    }
}

/// The audio-rate LFO: a graph node (`tutti_graph::Node`, no inputs, one
/// output) that computes a clock position in unwrapped cycles — free-running,
/// or from the transport beat of its block's `Env` — and reads its [`Waveform`]
/// there.
///
/// **The per-sample tier.** Reach for this when a control-rate value is too
/// coarse (audio-rate modulation): an audio generator whose output feeds a
/// node's own audio input. Beat-synced it reads the beat of every frame from
/// its block's `Env`, so it is sample-accurate and renders identically offline.
/// The control-rate alternative is a `tutti_modulation::Source::Lfo` route,
/// which a control lane evaluates into the param's port.
///
/// The depth is a live atomic the UI can write, multiplied onto the wave.
///
/// **Addressable.** The value at a position is a pure function of it, so a
/// random wave steps on the cycle index ([`Waveform::raw_at`]): beat-synced, it
/// replays after a seek; free-running, a fork (which starts at cycle zero)
/// replays the original's sequence from its start.
///
/// # In a graph
///
/// A graph node ([`IntoNode`](tutti_graph::IntoNode)): inserted, its controls are a [`ParamSet`](tutti_graph::ParamSet)
/// over the depth ([`UnitParam::Depth`]) and the rate ([`UnitParam::Rate`], in [`Hz`]),
/// whatever the node's [`LfoSync`], the one route to them. Both are read per frame (the
/// rate while free). It is a source with no ring-out
/// ([`Tail::None`]): what it emits is a function of the clock, so a render's end
/// ends it, and the executor never skips a node with no inputs.
///
/// **Arrival.** It has no inputs, so its compiled arrival is zero by
/// construction and the beat it reads is its block's own; a consumer behind a
/// latent path is aligned by the compiler, which delays this node's edge into
/// it like any other source's.
///
/// # Free or synced
///
/// A running LFO switches between the two: [`sync`](Self::sync) is the word it reads, once
/// per block, so a store lands at the next block's first frame. The rate is kept while
/// synced and the beat span ([`beats_per_cycle`](Self::beats_per_cycle), from the config,
/// not a param) while free. The output is continuous through a switch, and the synced
/// clock stays the stateless beat lock (the same offline, and equal to a control lane's):
///
/// - **Synced to free.** The free clock starts from the phase the output is at, the locked
///   phase plus any offset still running down, and goes on at the rate.
/// - **Free to synced.** The phase is the locked phase plus an offset, the free phase the
///   node was at less the locked phase of the block's first frame, taken the nearer way
///   round (within half a cycle). The offset runs down linearly to zero over
///   [`LFO_SYNC_RAMP`] and from then on the node is the stateless beat-locked LFO.
/// - **A locate, a loop wrap or a start while synced** lands on the locked phase at once,
///   also mid-ramp: the offset is dropped. The node tells them by the transport of each
///   piece of a block against the previous piece's rolled on
///   ([`TransportPosition::continuity`]).
///
/// **Fork, clone, reset.** A clone shares the sync word and carries the offset and its
/// remaining frames; a fork ([`ParamNode::fork_fresh`](tutti_graph::ParamNode::fork_fresh))
/// holds a word of its own at the value at the fork and is reset: the free clock at cycle
/// zero and no offset. A reset drops the offset.
#[derive(Clone, Debug)]
pub struct LfoNode {
    wave: Waveform,
    /// The rate and the depth, and the sync word.
    cells: LfoCells,
    /// The beat span of the synced clock, kept while free.
    beats_per_cycle: BeatsPerCycle,
    /// Added to the clock position before the wave reads it.
    phase_offset: PhaseIncrement,
    /// The free-running clock: only its phase, cycle count and step are read, its wave is
    /// not. Exact for as long as the node runs free; a switch from synced locates it.
    running: Lfo,
    /// The rate [`Node::prepare`] set; `None` until then, when a free-running
    /// clock has no step and the node renders silence.
    sample_rate: Option<SampleRate>,
    /// [`LFO_SYNC_RAMP`] in frames at the prepared rate; 0 until [`Node::prepare`], when a
    /// switch to synced is locked at once.
    ramp_frames: u32,
    /// The word the last block read: `None` before the first block and after a reset.
    active: Option<LfoSync>,
    /// The offset from the beat lock still running down.
    carry: Carry,
    /// The last piece of the previous block's transport and length, for telling a locate.
    prev: Option<(TransportPosition, Samples)>,
}

// The params an [`LfoNode`] lets the graph modulate, in port order: its depth and its
// rate, whatever its sync; and the [`LfoConfig`] it is made from: its shape, its starting
// depth, rate, beat span, phase and sync. Both params are read per frame. The sync is a
// setting a running node reads once per block (no port).
tutti_graph::declare_params!([] LfoNode,
    /// The clock is chosen by `sync`, and the rate and the beat span are kept whichever it
    /// is: a node keeps both, and a switch of the running node picks one. The default is a
    /// free node at 1 Hz, one cycle a beat when synced.
    #[derive(Copy, PartialEq)] LfoConfig, LfoCells {
    required { shape: LfoShape }
    params {
        Depth => depth: Bipolar = Bipolar::MAX,
        Rate => rate: Hz = Hz::new(1.0),
    }
    options {
        seed: u64 = 0,
        beats_per_cycle: BeatsPerCycle = BeatsPerCycle::of(1.0),
        phase_offset: PhaseIncrement = PhaseIncrement::new(0.0)
    }
    settings {
        sync: LfoSync = LfoSync::Free,
    }
});

impl LfoConfig {
    /// A node that starts synced to the transport beat at `span`: the span and
    /// [`LfoSync::Synced`] in one call. The rate is kept.
    #[must_use]
    pub fn beat_sync(self, span: BeatsPerCycle) -> Self {
        self.beats_per_cycle(span).sync(LfoSync::Synced)
    }
}

/// The config for a control-rate LFO's declaration: the same shape, seed,
/// rate and phase. Beat-synced, it produces the values a control lane
/// evaluates from that [`tutti_modulation::Lfo`], at audio rate. Free-running
/// (`Hz`), it runs at that rate from its insert and does not restart
/// at play, where the lane's `Hz` clock is the graph's play clock.
impl From<tutti_modulation::Lfo> for LfoConfig {
    fn from(lfo: tutti_modulation::Lfo) -> Self {
        let config = Self::new(lfo.shape).seed(lfo.seed).phase_offset(lfo.phase);
        match lfo.rate {
            tutti_modulation::LfoRate::Beats(per_cycle) => config.beat_sync(per_cycle),
            tutti_modulation::LfoRate::Hz(rate) => config.rate(rate),
        }
    }
}

impl From<LfoConfig> for LfoNode {
    fn from(config: LfoConfig) -> Self {
        Self {
            wave: Waveform::Shape {
                shape: config.shape,
                seed: config.seed,
            },
            cells: LfoCells::from_config(&config),
            beats_per_cycle: config.beats_per_cycle,
            phase_offset: config.phase_offset,
            running: Lfo::new(LfoWave::Sine, Hz::new(0.0), SampleRate::SR_48K),
            sample_rate: None,
            ramp_frames: 0,
            active: None,
            carry: Carry::NONE,
            prev: None,
        }
    }
}

impl LfoNode {
    /// The word that says whether the phase follows the rate or the beat, shared with the
    /// running node and its clones: `store` lands at the node's next block. Clone the `Arc`
    /// before the node is inserted; a fork holds a word of its own, at the value at the fork.
    #[must_use]
    pub fn sync(&self) -> &Arc<AtomicCell<LfoSync>> {
        &self.cells.sync
    }

    /// The beats one cycle of the synced clock takes: kept while the node is free.
    #[must_use]
    pub fn beats_per_cycle(&self) -> BeatsPerCycle {
        self.beats_per_cycle
    }

    /// The wave at `cycles` plus the phase offset, with the depth applied.
    #[inline]
    fn evaluate<F: Float>(&self, cycles: f64, offset: PhaseIncrement, depth: F) -> F {
        // The wave is `f64`; it crosses into the output's float once per frame.
        F::from_design(self.wave.raw_at(cycles + offset.get(), false)) * depth
    }

    /// The locked clock position, in cycles, at the first frame of `env`.
    fn locked_at_start(&self, env: &Env) -> f64 {
        env.transport_at(Offset::ZERO)
            .beat()
            .cycles_in(self.beats_per_cycle)
    }
}

impl<F: Float> Node<F> for LfoNode {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
            .cost(tutti_graph::Cost::Moderate)
            .tail(Tail::None)
            .param_addrs(self.declared_ports().as_slice())
    }

    fn prepare(&mut self, p: &Prepare) {
        self.sample_rate = Some(p.sample_rate());
        self.ramp_frames = frames_u32(LFO_SYNC_RAMP.to_samples(p.sample_rate()).get());
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        debug_assert_eq!(
            cx.arrival,
            Latency::ZERO,
            "a node with no inputs arrives at zero"
        );
        let size = io.frames();
        // A driven depth is clamped into `-1..=1` (a written one is within it). A
        // non-finite driven frame reads the cell instead: a NaN rate would
        // leave the phase NaN for good. The cell crosses into the output's float once
        // per block.
        let depth_cell = F::from_unit(self.cells.depth.load());
        let depth_signal = self
            .declared_param::<{ UnitParam::Depth as u16 }, _>(&io)
            .frames();
        let depth_at = |i: usize| {
            depth_signal
                .map(|l| l[i])
                .filter(|v| v.is_finite())
                .map_or(depth_cell, |v| v.clamp(F::lit(-1.0), F::lit(1.0)))
        };
        let offset = self.phase_offset;
        // The sync word is read once per block, never per frame: a store lands at the next
        // block's first frame.
        let sync = self.cells.sync.load();
        let previous = self.active.replace(sync);
        match sync {
            LfoSync::Free => {
                if previous == Some(LfoSync::Synced) {
                    // The free clock starts from where the output is: the locked phase at
                    // this block's first frame plus the offset still running down.
                    let at = self.locked_at_start(cx.env) + self.carry.after(0, self.ramp_inv());
                    self.running.locate(at);
                }
                self.carry = Carry::NONE;
                self.prev = cx
                    .env
                    .segments()
                    .last()
                    .map(|(_, piece)| (piece.transport, piece.block_len));
                let rate_cell = self.cells.rate.load().get();
                let rate_signal = self
                    .declared_param::<{ UnitParam::Rate as u16 }, _>(&io)
                    .frames();
                // The step is accumulated in `f64` for as long as the node runs: an
                // `f32`-rounded step drifts the cycle index off the frame.
                let out = io.output(0);
                let Some(sample_rate) = self.sample_rate else {
                    out[..size].fill(F::ZERO);
                    return Status::Modified;
                };
                // The step is rebuilt only when the rate moves.
                let mut current = rate_cell;
                self.running.retarget(Hz::new(current), sample_rate);
                for (i, o) in out[..size].iter_mut().enumerate() {
                    let rate = rate_signal
                        .map(|l| l[i].into())
                        .filter(|v| v.is_finite())
                        .unwrap_or(rate_cell);
                    if rate != current {
                        current = rate;
                        self.running.retarget(Hz::new(rate), sample_rate);
                    }
                    *o = self.evaluate(self.running.cycles(), offset, depth_at(i));
                    self.running.advance(Samples::new(1));
                }
            }
            LfoSync::Synced => {
                let span = self.beats_per_cycle;
                let rate = cx.env.sample_rate;
                let ramp_inv = self.ramp_inv();
                if previous == Some(LfoSync::Free) {
                    // The offset of the free phase from the lock, the nearer way round.
                    let miss = self.running.cycles() - self.locked_at_start(cx.env);
                    self.carry = Carry {
                        cycles: miss - miss.round(),
                        left: self.ramp_frames,
                    };
                }
                let out = io.output(0);
                for (start, piece) in cx.env.segments() {
                    if self.carry.left > 0
                        && self
                            .prev
                            .as_ref()
                            .is_some_and(|prev| !continues(prev, rate, &piece.transport))
                    {
                        self.carry = Carry::NONE;
                    }
                    let carry = self.carry;
                    let from = start.index();
                    if carry.left > 0 {
                        piece.for_each_piece_beat(start, |i, beat| {
                            let cycles =
                                beat.cycles_in(span) + carry.after(frames_u32(i - from), ramp_inv);
                            out[i] = self.evaluate(cycles, offset, depth_at(i));
                        });
                    } else {
                        piece.for_each_piece_beat(start, |i, beat| {
                            let cycles = beat.cycles_in(span);
                            out[i] = self.evaluate(cycles, offset, depth_at(i));
                        });
                    }
                    self.carry.left = carry.left.saturating_sub(frames_u32(piece.block_len.get()));
                    self.prev = Some((piece.transport, piece.block_len));
                }
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.running.reset();
        self.active = None;
        self.carry = Carry::NONE;
        self.prev = None;
    }

    fn param_base(&self, k: usize) -> Option<f64> {
        self.declared_base(k)
    }
}

impl LfoNode {
    /// The reciprocal of the ramp's frames (0 with no ramp, when no offset runs).
    fn ramp_inv(&self) -> f64 {
        1.0 / f64::from(self.ramp_frames.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_buffers::MaxBlock;
    use tutti_graph::ParamNode;
    use tutti_graph::contract::{Direct, assert_param_fork};
    use tutti_graph::{Env, PlayClock, Renderer, TransportChanges};
    use tutti_units::{Beat, Bpm, Frame, Samples};

    // The waveform math is `tutti-modulation`'s, and so are the tests that pin it.
    // What belongs here is the adapter around it: phase generation from
    // transport, depth, and the node surface.

    /// `n` frames of `node` at `sr` in one block, transport stopped at beat 0.
    fn render(node: LfoNode, sr: f64, n: usize) -> Vec<f32> {
        let mut d = Direct::new(node, SampleRate::of(sr), n);
        d.block();
        d.output().plane(0).samples().to_vec()
    }

    /// `n` frames of `node` through a graph at 48 kHz, each block handed
    /// `transport` as its own (advancing, when it rolls, by the block).
    fn render_at(node: LfoNode, transport: TransportPosition, n: usize) -> Vec<f32> {
        let rate = SampleRate::SR_48K;
        let (mut solo, ..) =
            Renderer::alone(node, Prepare::new(rate, MaxBlock::of(Samples::new(256))));
        solo.set_transport_fn(move |frame| {
            let mut clock = tutti_music::FrameClock::new(transport.beat(), transport.tempo, rate);
            if transport.playing {
                clock.advance(Samples::new(frame.get() as usize), None);
            }
            TransportPosition::counted(transport.playing, transport.tempo, clock.origin(), None)
        });
        solo.render(n).remove(0)
    }

    fn stopped_at(beat: f64) -> TransportPosition {
        TransportPosition::new(false, Bpm::of(120.0), Beat::new(beat), None)
    }

    /// A default config runs free at 1 Hz and keeps a beat span of one beat: at 100 Hz a
    /// quarter cycle is 25 frames, the sine's peak.
    #[test]
    fn a_default_config_runs_free_at_one_hertz_and_keeps_a_one_beat_span() {
        let node = LfoNode::from(LfoConfig::new(LfoShape::Sine));
        assert_eq!(node.sync().load(), LfoSync::Free);
        assert_eq!(node.cells.rate.load(), Hz::new(1.0));
        assert_eq!(node.beats_per_cycle(), BeatsPerCycle::of(1.0));
        let out = render(node, 100.0, 26);
        assert!((out[25] - 1.0).abs() < 1e-5, "{}", out[25]);
        assert!(out[0].abs() < 1e-6);
    }

    /// The span and the rate are independent options: a config holds both whichever order
    /// they are written in, and the sync is the one thing that says which the node starts on
    /// (`beat_sync` is the span and the word in one call).
    #[test]
    fn a_config_keeps_the_span_and_the_rate_and_the_sync_says_which_it_starts_on() {
        for config in [
            LfoConfig::new(LfoShape::Sine)
                .beat_sync(BeatsPerCycle::of(4.0))
                .rate(Hz::new(2.0)),
            LfoConfig::new(LfoShape::Sine)
                .rate(Hz::new(2.0))
                .beat_sync(BeatsPerCycle::of(4.0)),
        ] {
            let lfo = LfoNode::from(config);
            assert_eq!(lfo.sync().load(), LfoSync::Synced);
            assert_eq!(lfo.beats_per_cycle(), BeatsPerCycle::of(4.0));
            assert_eq!(lfo.cells.rate.load(), Hz::new(2.0));
        }
        let lfo = LfoNode::from(
            LfoConfig::new(LfoShape::Sine)
                .beats_per_cycle(BeatsPerCycle::of(4.0))
                .rate(Hz::new(2.0)),
        );
        assert_eq!(lfo.sync().load(), LfoSync::Free);
        assert_eq!(lfo.beats_per_cycle(), BeatsPerCycle::of(4.0));
        assert_eq!(lfo.cells.rate.load(), Hz::new(2.0));
    }

    /// A fork starts from the values last set through the node's
    /// `ParamSet` and shares no cell with it (see
    /// `tutti_graph::contract::assert_param_fork`). The declared params are the depth and
    /// the rate whatever the config's sync.
    #[test]
    fn the_declared_params_are_depth_and_rate_whatever_the_sync() {
        let free = LfoNode::from(LfoConfig::new(LfoShape::Sine).rate(Hz::new(2.0)));
        let synced =
            LfoNode::from(LfoConfig::new(LfoShape::Sine).beat_sync(BeatsPerCycle::of(4.0)));
        for node in [free, synced] {
            assert_eq!(
                ParamNode::<f32>::param_set(&node)
                    .params()
                    .collect::<Vec<_>>(),
                [UnitParam::Depth, UnitParam::Rate]
            );
            assert_param_fork(node);
        }
    }

    /// No inputs, one output, no ring-out: what a clock-driven source emits
    /// ends with the render, so a bounce adds nothing past its span for it.
    #[test]
    fn a_source_has_no_ring_out() {
        for lfo in [
            LfoNode::from(LfoConfig::new(LfoShape::Sine)),
            LfoNode::from(LfoConfig::new(LfoShape::Sine).beat_sync(BeatsPerCycle::of(1.0))),
        ] {
            let shape = Node::<f32>::shape(&lfo);
            assert_eq!(shape.audio_in().count(), 0);
            assert_eq!(shape.audio_out().count(), 1);
            assert_eq!(shape.tail, Tail::None);
        }
    }

    #[test]
    fn a_stopped_beat_reads_the_sine_of_its_phase_in_the_span() {
        let lfo =
            || LfoNode::from(LfoConfig::new(LfoShape::Sine).beat_sync(BeatsPerCycle::of(4.0)));
        // Beat 1.0 of a 4-beat cycle = quarter phase = sine peak.
        let out = render_at(lfo(), stopped_at(1.0), 1);
        assert!((out[0] - 1.0).abs() < 0.01, "Expected 1.0, got {}", out[0]);
        let out = render_at(lfo(), stopped_at(2.0), 1);
        assert!((out[0] - 0.0).abs() < 0.01, "Expected 0.0, got {}", out[0]);
    }

    /// While the transport rolls, each frame is evaluated at its own beat,
    /// not the block's first: a sine over one 4-beat cycle, frame by frame,
    /// across blocks.
    #[test]
    fn a_rolling_transport_is_read_frame_by_frame() {
        let rolling = TransportPosition::new(true, Bpm::of(120.0), Beat::new(0.0), None);
        // 120 BPM at 48 kHz: 24 000 frames a beat, 96 000 a cycle.
        let out = render_at(
            LfoNode::from(LfoConfig::new(LfoShape::Sine).beat_sync(BeatsPerCycle::of(4.0))),
            rolling,
            3_000,
        );
        for (i, &v) in out.iter().enumerate() {
            let beat = i as f64 / 24_000.0;
            let want = (core::f64::consts::TAU * beat / 4.0).sin() as f32;
            assert!((v - want).abs() < 1e-3, "frame {i}: {v} vs {want}");
        }
    }

    /// The beat is `f64` in the block's `Env`, so a phase far into a session
    /// keeps its sub-beat detail.
    #[test]
    fn beat_synced_lfo_keeps_sub_beat_precision_at_high_beats() {
        let lfo =
            || LfoNode::from(LfoConfig::new(LfoShape::Sine).beat_sync(BeatsPerCycle::of(4.0)));
        // Same fractional offset (0.5 beat) at beat 0 and at beat 20000.
        let at_edge = render_at(lfo(), stopped_at(0.5), 1)[0];
        let past_edge = render_at(lfo(), stopped_at(20_000.5), 1)[0];

        // 20000 is a multiple of the 4-beat cycle, so both are the same phase.
        assert!(
            (at_edge - past_edge).abs() < 0.01,
            "sub-beat precision lost at high beat count: {at_edge} vs {past_edge}"
        );
    }

    #[test]
    fn the_depth_scales_the_square_wave() {
        let lfo = LfoNode::from(LfoConfig::new(LfoShape::Square).depth(Bipolar::of(0.5)));
        let out = render(lfo, 48_000.0, 1);
        assert!((out[0] - 0.5).abs() < 0.01);
    }

    /// Negative depth inverts the wave rather than silencing it.
    ///
    /// The depth is signed: `LfoConfig::depth(-1.0)` stores `-1.0` and phase-flips the
    /// shape. Clamping to `0.0..=1.0` instead would store `0.0` and take the
    /// LFO flat, which is silent in both senses.
    #[test]
    fn negative_depth_inverts_instead_of_silencing() {
        let first = |depth: f64| {
            let lfo = LfoNode::from(LfoConfig::new(LfoShape::Square).depth(Bipolar::of(depth)));
            render(lfo, 48_000.0, 1)[0]
        };
        let (a, b) = (first(1.0), first(-1.0));

        assert!(a.abs() > 0.01, "the reference sample must be audible");
        assert!((b + a).abs() < 1e-6, "expected {b} to invert to {}", -a);
    }

    #[test]
    fn a_quarter_phase_offset_starts_a_sine_at_its_peak() {
        let lfo =
            LfoNode::from(LfoConfig::new(LfoShape::Sine).phase_offset(PhaseIncrement::new(0.25)));
        let out = render(lfo, 100.0, 1);
        assert!((out[0] - 1.0).abs() < 0.1, "Expected ~1.0, got {}", out[0]);
    }

    #[test]
    fn a_reset_restarts_the_cycle_at_phase_zero() {
        let mut d = Direct::new(
            LfoNode::from(LfoConfig::new(LfoShape::Sine)),
            SampleRate::of(100.0),
            25,
        );
        // A quarter cycle at 1 Hz and 100 Hz: the next frame would be the sine's peak.
        d.block();
        Node::<f32>::reset(&mut *d.node);
        d.block();
        assert!(
            d.output().plane(0).samples()[0].abs() < 0.1,
            "after a reset the first frame is the sine at phase zero"
        );
    }

    /// The unit roundoff of `f32`, `2^-24`: the bound on the relative error of one rounding,
    /// so on the absolute error of a value in `[-1, 1]`.
    const F32_ROUNDOFF: f64 = 5.960_464_477_539_063e-8;

    fn random(seed: u64) -> Waveform {
        Waveform::Shape {
            shape: LfoShape::Random,
            seed,
        }
    }

    /// **Seek.** A beat-synced random LFO's value at a beat is the wave's at
    /// that beat's cycle, whether the transport rolled there from zero or
    /// was placed there: the step is addressed by the cycle index, not
    /// counted from wraps the node happened to see.
    #[test]
    fn a_random_value_is_the_same_after_a_seek() {
        let seed = 11;
        let lfo = || {
            LfoNode::from(
                LfoConfig::new(LfoShape::Random)
                    .seed(seed)
                    .beat_sync(BeatsPerCycle::of(1.0)),
            )
        };
        let rolling = TransportPosition::new(true, Bpm::of(120.0), Beat::new(0.0), None);
        // 7.5 beats at 120 BPM and 48 kHz.
        let rolled = render_at(lfo(), rolling, 180_001);
        let want = random(seed).raw_at(7.5, false);
        assert_eq!(rolled[180_000], want as f32);
        for beat in [7.5, 7.25, 3.5, 1_000.5] {
            let sought = render_at(lfo(), stopped_at(beat), 1)[0];
            assert_eq!(
                sought,
                random(seed).raw_at(beat, false) as f32,
                "beat {beat}"
            );
        }
        assert_eq!(render_at(lfo(), stopped_at(7.25), 1)[0], want as f32);
    }

    /// **Fork.** A free-running fork starts at cycle zero and replays the
    /// wave's values from there, as the original did: both read the value
    /// at each frame's cycle, `i / 256` at 187.5 Hz and 48 kHz (an exact
    /// binary step, so the position is exact).
    #[test]
    fn a_random_value_is_the_same_in_a_fork() {
        let rate = SampleRate::SR_48K;
        let mut d = Direct::new(
            LfoNode::from(
                LfoConfig::new(LfoShape::Random)
                    .seed(5)
                    .rate(Hz::new(187.5)),
            ),
            rate,
            4096,
        );
        let check = |out: &[f32], who: &str| {
            for (i, &v) in out.iter().enumerate() {
                assert_eq!(
                    v,
                    random(5).raw_at(i as f64 / 256.0, false) as f32,
                    "{who} frame {i}"
                );
            }
        };
        d.block();
        check(d.output().plane(0).samples(), "original");
        let mut f = Direct::new(ParamNode::<f32>::fork_fresh(&*d.node), rate, 4096);
        f.block();
        check(f.output().plane(0).samples(), "fork");
    }

    /// **Step boundaries.** The step changes exactly where the rate puts a
    /// cycle boundary, and nowhere else: every 256 frames free-running at
    /// 187.5 Hz, and every 12 000 frames beat-synced at half a beat per cycle
    /// (120 BPM, 48 kHz), each step the wave's value for its index. The
    /// smooth wave reaches each step at its boundary.
    #[test]
    fn random_steps_fall_where_the_rate_says() {
        let w = random(0);
        let free = render(
            LfoNode::from(LfoConfig::new(LfoShape::Random).rate(Hz::new(187.5))),
            48_000.0,
            2048,
        );
        for (i, &v) in free.iter().enumerate() {
            assert_eq!(
                v,
                w.raw_at((i / 256) as f64, false) as f32,
                "free frame {i}"
            );
        }
        let rolling = TransportPosition::new(true, Bpm::of(120.0), Beat::new(0.0), None);
        let synced = render_at(
            LfoNode::from(LfoConfig::new(LfoShape::Random).beat_sync(BeatsPerCycle::of(0.5))),
            rolling,
            60_000,
        );
        for (i, &v) in synced.iter().enumerate() {
            assert_eq!(
                v,
                w.raw_at((i / 12_000) as f64, false) as f32,
                "synced frame {i}"
            );
        }
        // Consecutive steps differ here, so each boundary is visible.
        for k in 1..5 {
            assert_ne!(synced[k * 12_000 - 1], synced[k * 12_000], "boundary {k}");
        }
        let smooth = render(
            LfoNode::from(LfoConfig::new(LfoShape::RandomSmooth).rate(Hz::new(187.5))),
            48_000.0,
            2048,
        );
        for k in 1..8 {
            assert!(
                (f64::from(smooth[k * 256]) - w.raw_at(k as f64 - 1.0, false)).abs() < F32_ROUNDOFF
            );
        }
    }

    /// The node built from a control-rate [`tutti_modulation::Lfo`] evaluates the
    /// same wave: seed, phase and rate carried over. Beat-synced, on the same
    /// clock (the beat); free-running, at the same rate on the node's
    /// own clock, which counts from its insert (frame 0 here).
    #[test]
    fn a_node_from_a_control_lfo_reads_its_wave() {
        let lfo = tutti_modulation::Lfo::new(LfoShape::RandomSmooth)
            .seed(3)
            .rate(BeatsPerCycle::of(2.0))
            .phase(PhaseIncrement::new(0.25));
        let wave = Waveform::Shape {
            shape: LfoShape::RandomSmooth,
            seed: 3,
        };
        let synced = LfoNode::from(LfoConfig::from(lfo));
        assert_eq!(synced.sync().load(), LfoSync::Synced);
        assert_eq!(synced.beats_per_cycle(), BeatsPerCycle::of(2.0));
        let free = LfoNode::from(LfoConfig::from(lfo.rate(Hz::new(187.5))));
        assert_eq!(free.sync().load(), LfoSync::Free);
        assert_eq!(free.cells.rate.load(), Hz::new(187.5));
        for beat in [0.0, 1.5, 9.0, 33.75] {
            let got = render_at(LfoNode::from(LfoConfig::from(lfo)), stopped_at(beat), 1)[0];
            let want = wave.raw_at(beat / 2.0 + 0.25, false);
            assert!(
                (f64::from(got) - want).abs() < F32_ROUNDOFF,
                "beat {beat}: {got} vs {want}"
            );
        }
        // 187.5 Hz at 48 kHz: 256 frames a cycle.
        let free = render(
            LfoNode::from(LfoConfig::from(lfo.rate(Hz::new(187.5)))),
            48_000.0,
            2048,
        );
        for i in [0, 100, 256, 777, 2047] {
            let want = wave.raw_at(i as f64 / 256.0 + 0.25, false);
            let got = f64::from(free[i]);
            assert!(
                (got - want).abs() < F32_ROUNDOFF,
                "frame {i}: {got} vs {want}"
            );
        }
    }

    #[test]
    fn a_reverse_rate_walks_the_cycle_backwards_across_the_start() {
        // -1 Hz at 100 Hz: frame n is at -n / 100 cycles, whose ramp value is
        // `2 frac(-n / 100) - 1`; frame 0 is on the start, the ramp's -1.
        let out = render(
            LfoNode::from(LfoConfig::new(LfoShape::Sawtooth).rate(Hz::new(-1.0))),
            100.0,
            250,
        );
        for (n, &got) in out.iter().enumerate().filter(|(n, _)| n % 100 != 0) {
            let want = 2.0 * (1.0 - (n % 100) as f64 / 100.0) - 1.0;
            assert!((f64::from(got) - want).abs() < 1e-6, "frame {n}: {got}");
        }
    }

    /// The random wave `Random` of seed 0 at `cycles`, the closed form a free-running
    /// `LfoNode` is tested against.
    fn random_at(cycles: f64) -> f32 {
        Waveform::Shape {
            shape: LfoShape::Random,
            seed: 0,
        }
        .raw_at(cycles, false) as f32
    }

    /// At 100 Hz the clock's step is `rate / 100` cycles; frames whose position is not
    /// within 0.05 of a cycle boundary have an index no rounding can move.
    fn assert_random_follows(rate: f64, step: f64) {
        let out = render(
            LfoNode::from(LfoConfig::new(LfoShape::Random).rate(Hz::new(rate))),
            100.0,
            200,
        );
        let mut checked = 0;
        for (n, &got) in out.iter().enumerate() {
            let pos = n as f64 * step;
            if (pos - pos.round()).abs() < 0.05 {
                continue;
            }
            assert_eq!(got, random_at(pos), "rate {rate}, frame {n}");
            checked += 1;
        }
        assert!(checked > 80);
    }

    #[test]
    fn a_random_lfo_at_a_high_rate_below_nyquist_carries_forward_once_a_cycle() {
        // 40 Hz at 100 Hz: 0.4 cycles a frame.
        assert_random_follows(40.0, 0.4);
    }

    #[test]
    fn a_random_lfo_holds_one_value_a_cycle_and_steps_on_the_next() {
        // 1 Hz at 100 Hz: cycles are 100 frames; frames 10..=90 of each are one cycle.
        let out = render(LfoNode::from(LfoConfig::new(LfoShape::Random)), 100.0, 300);
        for c in 0..3 {
            let held = out[100 * c + 10];
            assert!(out[100 * c + 10..=100 * c + 90].iter().all(|&v| v == held));
        }
        assert!(out[10] != out[110] && out[110] != out[210]);
    }

    /// In an `f64` graph a free-running sine at a third of the rate is `sin(2 pi k / 3)`. The
    /// wave and the depth are `f64` (`Waveform::raw_at`, `Bipolar`), so every frame holds the
    /// closed form to `F64_TOLERANCE`, and a frame is not what an `f32` narrowing leaves (a
    /// non-dyadic value, which `f32` cannot hold to 1e-9).
    #[test]
    fn in_an_f64_graph_the_lfo_writes_its_wave_unnarrowed() {
        use crate::test_support::{F64_TOLERANCE, RATE, render_f64};
        let node = LfoNode::from(LfoConfig::new(LfoShape::Sine).rate(Hz::new(RATE.get() / 3.0)));
        let out = render_f64(node, RATE, &[], 64);
        let mut beyond_f32 = false;
        for (k, &got) in out[0].iter().enumerate() {
            let want = (core::f64::consts::TAU * k as f64 / 3.0).sin();
            assert!(
                (got - want).abs() <= F64_TOLERANCE,
                "frame {k}: {got} vs {want}"
            );
            beyond_f32 |= (got - f64::from(got as f32)).abs() > 1e-9;
        }
        assert!(beyond_f32, "no frame is finer than an f32");
    }

    // The switch figures. 4800 Hz at 120 bpm is 2400 frames a beat, so a 4-beat span is
    // 9600 frames a cycle; the ramp is 0.05 s, 240 frames. A sawtooth is `2 frac(c) - 1`, so
    // a value names its phase.

    const SR: f64 = 4800.0;
    /// [`LFO_SYNC_RAMP`] at [`SR`]: 0.05 s of 4800 frames.
    const RAMP: usize = 240;
    const BLOCK: usize = 100;
    /// The free rate of the switch tests, in Hz: `7 / 4800` of a cycle a frame.
    const FREE_HZ: f64 = 7.0;

    /// A sawtooth with a 4-beat span and a free rate of [`FREE_HZ`], starting on `sync`.
    fn saw(sync: LfoSync) -> Direct<LfoNode> {
        let config = LfoConfig::new(LfoShape::Sawtooth)
            .beats_per_cycle(BeatsPerCycle::of(4.0))
            .rate(Hz::new(FREE_HZ))
            .sync(sync);
        Direct::new(LfoNode::from(config), SampleRate::of(SR), BLOCK)
    }

    /// The transport rolling at 120 bpm from beat 0 at frame 0.
    fn rolling(frame: usize) -> TransportPosition {
        TransportPosition::new(
            true,
            Bpm::of(120.0),
            Beat::new(frame as f64 / 2_400.0),
            None,
        )
    }

    /// The locked position in cycles of `frame` under [`rolling`].
    fn locked(frame: usize) -> f64 {
        frame as f64 / 9_600.0
    }

    /// The free clock's position in cycles at `frame` frames of [`FREE_HZ`].
    fn free_at(frame: usize) -> f64 {
        frame as f64 * FREE_HZ / SR
    }

    /// `x` the nearer way round: within half a cycle of zero.
    fn nearer(x: f64) -> f64 {
        x - x.round()
    }

    /// The offset a switch to synced at `frame` takes, after `frame` frames of free running.
    fn carry0(frame: usize) -> f64 {
        nearer(free_at(frame) - locked(frame))
    }

    /// `[from, from + frames)` in blocks of [`BLOCK`], each under `transport` of its first
    /// frame.
    fn run(
        d: &mut Direct<LfoNode>,
        from: usize,
        frames: usize,
        transport: impl Fn(usize) -> TransportPosition,
    ) -> Vec<f32> {
        let mut out = Vec::with_capacity(frames);
        for start in (from..from + frames).step_by(BLOCK) {
            d.set_transport(transport(start));
            d.block();
            out.extend_from_slice(d.output().plane(0).samples());
        }
        out
    }

    /// The distance between the phase a sawtooth `value` is at and `cycles`, round the cycle.
    fn off_by(value: f32, cycles: f64) -> f64 {
        let d = ((f64::from(value) + 1.0) / 2.0 - cycles).rem_euclid(1.0);
        d.min(1.0 - d)
    }

    fn assert_at(out: &[f32], first: usize, want: impl Fn(usize) -> f64, what: &str) {
        for (k, &v) in out.iter().enumerate() {
            let frame = first + k;
            let d = off_by(v, want(frame));
            assert!(d < 1e-5, "{what}, frame {frame}: {v} is {d} cycles off");
        }
    }

    /// **Free to synced.** After 300 free frames the free phase is `300 * 7 / 4800 = 0.4375`
    /// and the lock is `300 / 9600 = 0.03125`: the offset is `0.40625` of a cycle, and
    /// every frame of the next 240 is the lock plus the offset times `1 - k / 240`, then the
    /// lock alone, across the blocks of 100 frames the ramp spans.
    #[test]
    fn a_switch_to_synced_slews_the_free_phase_onto_the_lock_over_the_ramp() {
        let mut d = saw(LfoSync::Free);
        run(&mut d, 0, 300, rolling);
        d.node.sync().store(LfoSync::Synced);
        let out = run(&mut d, 300, 600, rolling);
        let c0 = carry0(300);
        assert!((c0 - 0.40625).abs() < 1e-12);
        assert_at(
            &out,
            300,
            |frame| {
                let k = frame - 300;
                locked(frame)
                    + if k < RAMP {
                        c0 * (1.0 - k as f64 / RAMP as f64)
                    } else {
                        0.0
                    }
            },
            "ramp then lock",
        );
    }

    /// **Continuity.** The first frame after the switch is the free clock's next step
    /// (`0.4375`), a step of `7 / 4800` from the last free frame; the lock (`0.03125`) is
    /// 0.4 of a cycle away and a snap to it would be a jump of two hundred steps.
    #[test]
    fn the_first_frame_after_a_switch_to_synced_continues_the_free_phase() {
        let mut d = saw(LfoSync::Free);
        let free = run(&mut d, 0, 300, rolling);
        d.node.sync().store(LfoSync::Synced);
        let first = run(&mut d, 300, BLOCK, rolling)[0];
        let step = FREE_HZ / SR;
        assert!(
            off_by(first, free_at(300)) < 1e-5,
            "the free clock's next frame"
        );
        assert!(off_by(free[299], free_at(299)) < 1e-5);
        assert!(
            off_by(first, free_at(299)) < step + 1e-5,
            "within a frame's step"
        );
        assert!(
            off_by(first, locked(300)) > 100.0 * step,
            "the lock is far from the free phase"
        );
    }

    /// **Synced to free.** A node that is synced goes on from the phase it is at: the
    /// locked `300 / 9600` at the switch, plus `k * 7 / 4800` a frame on.
    #[test]
    fn a_switch_to_free_runs_on_from_the_phase_the_output_is_at() {
        let mut d = saw(LfoSync::Synced);
        run(&mut d, 0, 300, rolling);
        d.node.sync().store(LfoSync::Free);
        let out = run(&mut d, 300, 300, rolling);
        assert_at(
            &out,
            300,
            |f| locked(300) + free_at(f - 300),
            "free from the lock",
        );
    }

    /// **Synced to free, mid-ramp.** A switch back 100 frames into the ramp starts from the
    /// lock plus the offset still running down (140 of 240 frames left), not the lock.
    #[test]
    fn a_switch_to_free_mid_ramp_starts_from_the_offset_phase() {
        let mut d = saw(LfoSync::Free);
        run(&mut d, 0, 300, rolling);
        d.node.sync().store(LfoSync::Synced);
        run(&mut d, 300, 100, rolling);
        d.node.sync().store(LfoSync::Free);
        let out = run(&mut d, 400, 200, rolling);
        let start = locked(400) + carry0(300) * 140.0 / RAMP as f64;
        assert_at(
            &out,
            400,
            |f| start + free_at(f - 400),
            "free from the offset phase",
        );
    }

    /// **The export invariant.** A node that switched to synced at frame `F` is, from
    /// `F + 240` on, the closed form of the stateless lock `frame / 9600`: whichever block
    /// the switch falls in, and with the rate the free clock had.
    #[test]
    fn from_the_end_of_the_ramp_a_switched_node_is_the_stateless_lock() {
        for switch in [100, 500, 1_100] {
            let mut d = saw(LfoSync::Free);
            run(&mut d, 0, switch, rolling);
            d.node.sync().store(LfoSync::Synced);
            let out = run(&mut d, switch, RAMP + 400, rolling);
            assert_at(
                &out[RAMP..],
                switch + RAMP,
                locked,
                &format!("switched at {switch}"),
            );
        }
    }

    /// **The rate and the span are kept.** A rate written while synced changes no frame
    /// (each is the lock) and is the rate a switch to free runs at; the span the node was
    /// built with is the one it locks to after a switch back.
    #[test]
    fn a_rate_moved_while_synced_is_the_free_rate_and_the_span_is_kept_while_free() {
        let mut d = saw(LfoSync::Synced);
        let set = ParamNode::<f32>::param_set(&*d.node);
        let before = run(&mut d, 0, 300, rolling);
        assert_at(&before, 0, locked, "synced at 7 Hz");
        assert!(set.set(UnitParam::Rate, 9.0));
        let after = run(&mut d, 300, 300, rolling);
        assert_at(&after, 300, locked, "synced at 9 Hz");
        d.node.sync().store(LfoSync::Free);
        let free = run(&mut d, 600, 300, rolling);
        assert_at(
            &free,
            600,
            |f| locked(600) + (f - 600) as f64 * 9.0 / SR,
            "free at 9 Hz",
        );
        assert_eq!(d.node.beats_per_cycle(), BeatsPerCycle::of(4.0));
        d.node.sync().store(LfoSync::Synced);
        let back = run(&mut d, 900, RAMP + 300, rolling);
        assert_at(
            &back[RAMP..],
            900 + RAMP,
            locked,
            "locked to the 4-beat span again",
        );
    }

    /// **A locate, a start, mid-ramp.** The transport stands at beat 1 (lock `0.25`) while
    /// the node runs free at 30 Hz for 300 frames (phase `1.875`) and then switches: the
    /// offset is `-0.375`. A locate to beat 2.5 (lock `0.625`) between blocks, or a
    /// locate or a start inside one, puts every frame from its own on the lock exactly (the
    /// wave at `0.625`) or on the rolled lock from the start's frame: the offset is dropped,
    /// where the frames before it are still on the ramp.
    #[test]
    fn a_locate_or_a_start_mid_ramp_lands_on_the_lock_at_once() {
        let stopped =
            |beat: f64| TransportPosition::new(false, Bpm::of(120.0), Beat::new(beat), None);
        let ramped_node = || {
            let config = LfoConfig::new(LfoShape::Sawtooth)
                .beats_per_cycle(BeatsPerCycle::of(4.0))
                .rate(Hz::new(30.0));
            let mut d = Direct::new(LfoNode::from(config), SampleRate::of(SR), BLOCK);
            run(&mut d, 0, 300, |_| stopped(1.0));
            d.node.sync().store(LfoSync::Synced);
            d
        };
        let ramp = |k: usize| 0.25 - 0.375 * (1.0 - k as f64 / RAMP as f64);
        let exact = |cycles: f64| {
            Waveform::Shape {
                shape: LfoShape::Sawtooth,
                seed: 0,
            }
            .raw_at(cycles, false) as f32
        };
        // Between blocks: one block on the ramp, then a block at another beat.
        let mut d = ramped_node();
        let on = run(&mut d, 300, BLOCK, |_| stopped(1.0));
        assert_at(&on, 300, |f| ramp(f - 300), "on the ramp");
        let located = run(&mut d, 400, BLOCK, |_| stopped(2.5));
        assert!(located.iter().all(|&v| v == exact(0.625)), "{located:?}");
        // Inside a block: a locate at frame 40, then a start at frame 40.
        let block_with = |d: &mut Direct<LfoNode>, to: TransportPosition| {
            let mut changes = TransportChanges::NONE;
            changes
                .push(Offset::of(Samples::new(40), Samples::new(BLOCK)), to)
                .unwrap();
            let env = Env {
                frame: Frame::new(300),
                sample_rate: SampleRate::of(SR),
                block_len: Samples::new(BLOCK),
                transport: stopped(1.0),
                changes,
                play: PlayClock::ZERO,
            };
            d.block_in(&env);
            d.output().plane(0).samples().to_vec()
        };
        let mut d = ramped_node();
        let out = block_with(&mut d, stopped(2.5));
        assert_at(&out[..40], 300, |f| ramp(f - 300), "before the locate");
        assert!(
            out[40..].iter().all(|&v| v == exact(0.625)),
            "{:?}",
            &out[40..]
        );
        let mut d = ramped_node();
        let out = block_with(
            &mut d,
            TransportPosition::new(true, Bpm::of(120.0), Beat::new(1.0), None),
        );
        assert_at(&out[..40], 300, |f| ramp(f - 300), "before the start");
        assert_at(
            &out[40..],
            340,
            |f| (1.0 + (f - 340) as f64 / 2_400.0) / 4.0,
            "the rolled lock from the start",
        );
    }

    /// **Clone, fork, reset.** A clone taken 100 frames into the ramp carries it on (the
    /// frames of the 140 that remain are the formula's) and shares the word; a fork holds a
    /// word of its own at the value at the fork and is reset, so it is the lock; a reset
    /// drops the offset.
    #[test]
    fn a_clone_continues_the_ramp_a_fork_owns_its_word_and_a_reset_drops_the_offset() {
        let mut d = saw(LfoSync::Free);
        run(&mut d, 0, 300, rolling);
        d.node.sync().store(LfoSync::Synced);
        run(&mut d, 300, 100, rolling);
        let c0 = carry0(300);
        let (clone, fork) = ((*d.node).clone(), ParamNode::<f32>::fork_fresh(&*d.node));
        let mut clone = Direct::new(clone, SampleRate::of(SR), BLOCK);
        let mut fork = Direct::new(fork, SampleRate::of(SR), BLOCK);
        let on = run(&mut clone, 400, 300, rolling);
        assert_at(
            &on,
            400,
            |frame| {
                let k = frame - 300;
                locked(frame)
                    + if k < RAMP {
                        c0 * (1.0 - k as f64 / RAMP as f64)
                    } else {
                        0.0
                    }
            },
            "the clone's remaining steps",
        );
        assert!(
            Arc::ptr_eq(clone.node.sync(), d.node.sync()),
            "a clone shares the word"
        );
        // The fork is the lock at once, with a word of its own at the value at the fork.
        assert_eq!(fork.node.sync().load(), LfoSync::Synced);
        assert!(!Arc::ptr_eq(fork.node.sync(), d.node.sync()));
        d.node.sync().store(LfoSync::Free);
        assert_eq!(fork.node.sync().load(), LfoSync::Synced);
        fork.node.sync().store(LfoSync::Free);
        fork.node.sync().store(LfoSync::Synced);
        assert_eq!(d.node.sync().load(), LfoSync::Free);
        let locked_out = run(&mut fork, 400, 300, rolling);
        assert_at(&locked_out, 400, locked, "a fork has no offset");
        // A reset drops the offset of a node 100 frames into the ramp.
        let mut d = saw(LfoSync::Free);
        run(&mut d, 0, 300, rolling);
        d.node.sync().store(LfoSync::Synced);
        run(&mut d, 300, 100, rolling);
        Node::<f32>::reset(&mut *d.node);
        let after = run(&mut d, 400, 300, rolling);
        assert_at(&after, 400, locked, "no offset after a reset");
    }
}
