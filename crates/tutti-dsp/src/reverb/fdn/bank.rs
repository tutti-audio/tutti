//! [`FdnBank`]: the feedback delay network over buffers views, with the parameters that
//! sound as [`Ramp`]s ([`FdnRamps`]).
//!
//! `N` delay lines read with linear interpolation (so a length may move), a
//! three-band absorption in each loop, a mix `A = I - 2 m v v^T` (`m` is 1 for a
//! reflection and 0 for the identity of `Matrix::Diagonal`) and a stereo output.
//! The wet signal is the lines' own outputs (before their absorption), so
//! nothing sounds before the shortest line has gone round, and the loop gain is
//! exactly the network's.
//!
//! **Chunks.** The render counts [`CHUNK`](FdnBank::CHUNK) frames from the last reset, and
//! works once at each chunk's start: every ramp takes a step, and the chunk's constants are
//! read from them (the line lengths, the absorption gains, the reflection vector, the
//! modulation's two ends the lengths are interpolated between). The chunks are counted from
//! the first frame since the last reset, not from a call's edge, so the output does not
//! depend on how the input is cut into calls. The *design* is not the bank's: the caller retargets
//! the ramps ([`retarget`](FdnBank::retarget)) when its controls move, on a grid of its own.
//!
//! **Retargeting ramps.** `retarget` only sets targets. Every parameter that sounds (the loop
//! gains, the reflection vector (renormalised to its length every chunk) and its mix `m`,
//! the input and output gains, the wet gain, the width and the modulation depth) is a
//! [`Ramp`] that moves to it in a fixed 20 ms of whole chunks, landing on it exactly; a
//! length moves at a bounded half frame a chunk (about 1.6% of a frame a frame, a pitch bend
//! of 27 cents), and the pre-delay at two.
//!
//! **Lines in flight.** A gain is designed for the length its line is moving to. While a line
//! is still `L'` frames long and its design is for `L`, each band's gain is raised to `L' / L`
//! at the chunk's start, so the decay stays the designed RT60 at whatever length the line has
//! (`begin_chunk` derives it). An at-rest line runs the designed gains unchanged.
//!
//! **The mix.** The identity of a diagonal network and the reflection of a
//! mixing one have determinants +1 and -1, so no path of orthogonal matrices
//! joins them, and a straight line `I - 2 m v v^T` between them has eigenvalue
//! `1 - 2m` along `v`, a contraction that would lose a frozen tail's energy.
//! The matrix is therefore never between the two: `m` is 0 or 1, and it
//! switches at the end of a ramp that leaves a mixing matrix (when `v` has
//! reached line 0, where `I - 2 v v^T` flips one line's sign) and at the start
//! of one that reaches it. Every step of a ramp is then orthogonal, `v` moves
//! between unit vectors (a diagonal network points it at line 0), and a mixing
//! network is bit-identical to a recurrence without `m`. The switch is a step in
//! the sign of line 0's loop, as the model's own matrix steps between
//! `Diagonal` and the first `Reflection` near line 0.
//!
//! **The absorption** of a line is `y = g_high x + (g_mid - g_high) L2(x) +
//! (g_low - g_mid) L1(x)` with `L1` and `L2` two cascaded one-poles at the
//! network's two crossovers: `g_low` at DC, `g_high` far above the upper
//! crossover and `g_mid` between. It is linear in the gains, so a ramp of the
//! gains is a ramp of the filter; with every gain 1 it is the identity, and a
//! frozen loop loses nothing. A set of gains whose response peaks above the largest
//! of them between the crossovers (a mid band that rings longer than a neighbour) is
//! scaled by the design (`rescale_bands`) so no frequency decays slower than the longest band
//! set, and the loop cannot grow.
//!
//! **The shimmer.** A [`ShimmerLayout`](crate::ShimmerLayout) runs one mono fixed-rate
//! vocoder in the feedback ([`shimmer`](super::shimmer) has the loop and its gain): at each
//! frame the lines' feedback `f` is summed to its mean component `s`, the share `k` of it is
//! replaced by the shifted `s` of the chunk before (delayed by the chunk and the vocoder's
//! latency, [`shimmer_delay`](FdnBank::shimmer_delay)), and the chunk's `s` goes to the vocoder
//! at the chunk's end. The model's `Fdn` stage carries that delay as its `loop_delay` while the send
//! is above zero or its ramp has not reached zero, so the decay queries and the tail read it. A plain layout
//! runs the loop above unchanged.
//!
//! **Format.** The state and the views are the bank's float `F`. The loop gains and the
//! one-poles are designed in `f64` and cross to `F` once per design ([`FdnBank::retarget`]). A pole
//! is within `1 - p` of 1 at a low crossover (`p` of 200 Hz at 192 kHz is 6.5e-3), so a one-pole
//! stores its complement `c = 1 - p`, solved with `expm1`, and runs `q += c (x - q)`. A loop
//! gain is not stored as a complement: a line's gain is `g = 10^(-3 L / (fs T))` for its own
//! length `L`, and the shortest line (size 0, 11.7 ms or 2244 frames at 192 kHz) at 60 s has `1 - g
//! = 1.35e-3`, which `F`'s rounding of `g` (3e-8) moves by 2.2e-5 of itself: the decay time moves by
//! that share (a 1 ms line would move it by 2.6e-4, the bound). A loop gain multiplies a line's
//! sample, so the product rounds by about `u` a trip whatever the form: a complement buys nothing
//! there.

use crate::BlockMismatch;
use crate::block::expect_output_width;
use tutti_buffers::MAX_CHANNELS;
use tutti_buffers::{Float, InputOutput};
use tutti_units::{
    Gain, Hz, Quantity, ReadPosition, SampleRate, SampleSpan, Samples, Seconds, Tail,
};

use super::design::{CROSSOVER, FdnControls, set_loop_delay};
use super::kind::FdnKind;
use super::kind::sealed::{ShiftRamps, Shifter};
use crate::delay::DelayRing;
use crate::design::along;
use crate::onepole::{cutoff_complement, cutoff_pole};
use crate::reverb::network::{Absorption, Band, Matrix, Network, NetworkStage};
use crate::reverb::{CHUNK, ramp, tail_of};
use crate::{DecayTime, FftFloat, Lerp, Ramp, Steps, System, TAIL_FLOOR};

/// The shortest a line reads at: one frame.
const ONE_FRAME: ReadPosition = ReadPosition::from_frames(Samples::new(1));
/// How far a line's length moves in a chunk, in frames.
const LENGTH_STEP: f64 = 0.5;
/// How far the pre-delay moves in a chunk, in frames.
const PRE_STEP: f64 = 2.0;

/// The coefficient point a design asks for, as [`FdnBank::retarget`] writes it: the values
/// [`FdnRamps`] ramp to, before any rate is applied. Sized for the most lines the bank was
/// built for; its fields from line `n` on are not read but for the input and output gains,
/// which a smaller network closes.
#[derive(Clone, Debug)]
struct Target<F> {
    /// The lines in use.
    n: usize,
    rate: Hz,
    crossover: [Hz; 2],
    /// The crossovers' one-poles `exp(-2 pi fc / fs)`, for the response the gains are scaled by.
    pole: [f64; 2],
    /// The one-poles' complements `1 - pole`, the coefficients their updates run on.
    complement: [F; 2],

    len: Vec<ReadPosition>,
    pre_len: ReadPosition,
    depth: F,
    gain: Vec<[F; 3]>,
    v: Vec<F>,
    v_len: F,
    mix: F,
    input: Vec<F>,
    out: Vec<[F; 2]>,
    wet: F,
    width: F,
}

impl<F: Float> Target<F> {
    fn new(lines: usize) -> Self {
        Self {
            n: 0,
            rate: Hz::new(0.5),
            crossover: [Hz::ZERO; 2],
            pole: [0.0; 2],
            complement: [F::ONE; 2],
            len: vec![ONE_FRAME; lines],
            pre_len: ReadPosition::START,
            depth: F::ZERO,
            gain: vec![[F::ZERO; 3]; lines],
            v: vec![F::ZERO; lines],
            v_len: F::ZERO,
            mix: F::ZERO,
            input: vec![F::ZERO; lines],
            out: vec![[F::ZERO; 2]; lines],
            wet: F::ZERO,
            width: F::ONE,
        }
    }
}

/// What a [`FdnBank`] approaches: every sounding parameter a [`Ramp`] (each at its own
/// rate: a length and the pre-delay by a distance, the rest by 20 ms), and the few that jump
/// (the line count, the modulation rate, the absorption's poles).
///
/// Retargeted by [`FdnBank::retarget`], stepped once a chunk by [`FdnBank::process`]. A ramp steps
/// before its chunk runs: the first chunk after a `retarget` reads step 1.
#[derive(Clone, Debug)]
pub struct FdnRamps<F: Float + Lerp, L: FdnKind> {
    /// The steps of the 20 ms ramp of a gain, at the rate prepared for.
    ramp: Steps,
    /// The lines in use: 0 until the first `retarget`, and the bank is silent.
    n: usize,
    rate: Hz,
    /// The absorption one-poles' complements `1 - pole`.
    complement: [F; 2],
    crossover: [Hz; 2],

    len: Vec<Ramp<ReadPosition>>,
    /// The length the gains are designed for, ramped with them over the ramp: the gains in
    /// flight are a blend of two designs, each for its own length.
    design_len: Vec<Ramp<ReadPosition>>,
    pre_len: Ramp<ReadPosition>,
    depth: Ramp<F>,
    gain: Vec<[Ramp<F>; 3]>,
    v: Vec<Ramp<F>>,
    /// The length of `v` the network asks for (1 for every design here): the
    /// reflection is renormalised to it every chunk, so it is orthogonal at
    /// every step of a ramp and not only at its ends.
    v_len: Ramp<F>,
    /// `m` of `I - 2 m v v^T`: 0 for a diagonal network, 1 for a reflection. Only
    /// its target and whether it is moving are read (see the module docs): the
    /// bank's `m` is 1 while it ramps or targets 1, else 0.
    mix: Ramp<F>,
    input: Vec<Ramp<F>>,
    out: Vec<[Ramp<F>; 2]>,
    wet: Ramp<F>,
    width: Ramp<F>,
    /// What the kind adds: the shimmer's send and shift, nothing for a plain layout.
    shimmer: L::Ramps<F>,
}

impl<F: Float + Lerp, L: FdnKind> FdnRamps<F, L> {
    fn new(lines: usize, ramp: Steps, t: &Target<F>) -> Self {
        let rest = Ramp::new(F::ZERO);
        Self {
            ramp,
            n: 0,
            rate: Hz::new(0.5),
            complement: t.complement,
            crossover: t.crossover,
            len: vec![Ramp::new(ONE_FRAME); lines],
            design_len: vec![Ramp::new(ONE_FRAME); lines],
            pre_len: Ramp::new(ReadPosition::START),
            depth: rest,
            gain: vec![[rest; 3]; lines],
            v: vec![rest; lines],
            v_len: rest,
            mix: rest,
            input: vec![rest; lines],
            out: vec![[rest; 2]; lines],
            wet: rest,
            width: Ramp::new(F::ONE),
            shimmer: L::Ramps::new(),
        }
    }

    fn ramps(&mut self) -> impl Iterator<Item = &mut Ramp<F>> {
        self.gain
            .iter_mut()
            .flatten()
            .chain(&mut self.v)
            .chain(&mut self.input)
            .chain(self.out.iter_mut().flatten())
            .chain([
                &mut self.depth,
                &mut self.v_len,
                &mut self.mix,
                &mut self.wet,
                &mut self.width,
            ])
    }

    /// Jumps to the targets now.
    pub fn jump_to_target(&mut self) {
        self.ramps().for_each(Ramp::jump_to_target);
        self.lengths().for_each(Ramp::jump_to_target);
        self.shimmer.jump_to_target();
    }

    /// Takes one step of every ramp.
    fn step(&mut self) {
        self.ramps().for_each(|g| g.advance(1));
        self.lengths().for_each(|g| g.advance(1));
        self.shimmer.step();
    }

    /// The ramps of a length: the lines', the gains' design lengths and the pre-delay.
    fn lengths(&mut self) -> impl Iterator<Item = &mut Ramp<ReadPosition>> {
        self.len
            .iter_mut()
            .chain(&mut self.design_len)
            .chain([&mut self.pre_len])
    }

    /// Aims every ramp at the target's value with its own rate: a length by
    /// [`LENGTH_STEP`] a chunk, the pre-delay by [`PRE_STEP`], every other gain in the ramp.
    /// The lines from `n` on keep their gains and lengths; their input and output gains close.
    fn retarget(&mut self, t: &Target<F>, kind: &L::Controls) {
        let ramp = self.ramp;
        let n = t.n;
        self.n = n;
        self.crossover = t.crossover;
        self.complement = t.complement;
        for j in 0..n {
            self.len[j].retarget_position(t.len[j], Steps::Rate(LENGTH_STEP));
            self.design_len[j].retarget_position(t.len[j], ramp);
            for (s, &g) in self.gain[j].iter_mut().zip(&t.gain[j]) {
                s.retarget_by(g, ramp);
            }
            self.v[j].retarget_by(t.v[j], ramp);
        }
        for j in 0..self.input.len() {
            self.input[j].retarget_by(t.input[j], ramp);
            for (s, &g) in self.out[j].iter_mut().zip(&t.out[j]) {
                s.retarget_by(g, ramp);
            }
        }
        self.v_len.retarget_by(t.v_len, ramp);
        self.mix.retarget_by(t.mix, ramp);
        self.depth.retarget_by(t.depth, ramp);
        self.rate = t.rate;
        self.pre_len
            .retarget_position(t.pre_len, Steps::Rate(PRE_STEP));
        self.wet.retarget_by(t.wet, ramp);
        self.width.retarget_by(t.width, ramp);
        self.shimmer.retarget(kind, ramp);
    }
}

/// What a bank's chunks change: the lines, the pre-delay, the modulation phases, the
/// absorption's one-poles, and the constants of the chunk being run.
#[derive(Clone, Debug)]
struct State<F> {
    lines: Vec<DelayRing<F>>,
    pre: DelayRing<F>,
    phase: Vec<f64>,
    /// One-pole states a line: `[L1 first, L1 second, L2 first, L2 second]`.
    pole: Vec<[F; 4]>,
    /// Whether the modulation has not started: its first chunk reads the phase's
    /// own value as the previous one.
    fresh: bool,

    // The chunk's constants.
    c_len: Vec<ReadPosition>,
    c_mod_prev: Vec<F>,
    c_mod_next: Vec<F>,
    c_gain: Vec<[F; 3]>,
    c_v: Vec<F>,
    /// 0 or 1: the chunk's `m`.
    c_mix: F,
    c_in: Vec<F>,
    c_out: Vec<[F; 2]>,
    c_wet: F,
    c_width: F,
    c_pre: ReadPosition,
}

/// A feedback delay network of any layout: its lines, its pre-delay and what a chunk
/// changes of them.
///
/// Built on the control side for one rate, where it allocates everything it will hold (the
/// lines at their longest, the scratch a design is made in); a clone keeps their size, and
/// [`reset`](Self::reset) zeroes them without allocating. Mono in (the input channels are
/// averaged), stereo out: the **wet** signal alone, even lines left and odd right.
#[derive(Clone, Debug)]
pub struct FdnBank<F: FftFloat, L: FdnKind> {
    layout: L,
    rate: SampleRate,
    max_lines: usize,
    cap: usize,
    pre_cap: usize,
    /// How long it rings at its worst.
    ceiling: Tail,
    /// Frames of the current chunk already run: 0 at a chunk's start.
    into: usize,
    state: State<F>,
    /// What the kind keeps in the feedback path: the shifter of a shimmer layout, nothing
    /// for a plain layout.
    shimmer: L::State<F>,
    /// The controls the design in `network` is of: a design is made once however many of
    /// [`retarget`](Self::retarget) and [`tail_of`](Self::tail_of) ask for it.
    designed: Option<(FdnControls, L::Controls)>,
    /// Where a design is made and the coefficient point it asks for, so the audio thread
    /// builds neither.
    network: Box<Network>,
    target: Target<F>,
}

impl<F: FftFloat + Lerp, L: FdnKind> FdnBank<F, L> {
    /// The frames between a ramp's steps: the chunk. A caller that designs on a grid of
    /// its own can align it here, so a new target lands on a chunk's start.
    pub const CHUNK: usize = CHUNK;

    /// A silent bank of `layout` at `rate`. Allocates: build it on the control side.
    #[must_use]
    pub fn new(layout: &L, rate: SampleRate) -> Self {
        let shimmer = layout.state::<F>(rate);
        let mut bounds = layout.layout().capacity(rate, shimmer.delay());
        bounds.tail = layout.ceiling(bounds.tail, rate);
        let max_lines = bounds.lines;
        let cap = bounds.line.to_samples_ceil().get() + 4;
        let pre_cap = bounds.pre_delay.get() + 4;
        let mut target = Target::new(max_lines);
        set_poles(&mut target, CROSSOVER, rate);
        let mut bank = Self {
            layout: *layout,
            rate,
            max_lines,
            cap,
            pre_cap,
            ceiling: tail_of(bounds.tail, rate),
            into: 0,
            state: State {
                lines: (0..max_lines).map(|_| DelayRing::new(cap)).collect(),
                pre: DelayRing::new(pre_cap),
                phase: vec![0.0; max_lines],
                pole: vec![[F::ZERO; 4]; max_lines],
                fresh: true,
                c_len: vec![ONE_FRAME; max_lines],
                c_mod_prev: vec![F::ZERO; max_lines],
                c_mod_next: vec![F::ZERO; max_lines],
                c_gain: vec![[F::ZERO; 3]; max_lines],
                c_v: vec![F::ZERO; max_lines],
                c_mix: F::ZERO,
                c_in: vec![F::ZERO; max_lines],
                c_out: vec![[F::ZERO; 2]; max_lines],
                c_wet: F::ZERO,
                c_width: F::ZERO,
                c_pre: ReadPosition::START,
            },
            shimmer,
            designed: None,
            network: Box::new(Network::new(rate)),
            target,
        };
        bank.reset();
        bank
    }

    /// The ramps of this bank, at rest and set to nothing: its first [`retarget`](Self::retarget)
    /// is the caller's to land.
    #[must_use]
    pub fn ramps(&self) -> FdnRamps<F, L> {
        FdnRamps::new(self.max_lines, ramp(self.rate), &self.target)
    }

    /// How long it rings at its worst: unbounded for a plain layout, since a freeze holds the
    /// tail; a [`ShimmerLayout`](crate::ShimmerLayout) cannot freeze, and its ceiling is its longest decay.
    #[must_use]
    pub fn tail(&self) -> Tail {
        self.ceiling
    }

    /// Designs `controls` into the scratch network, unless it already holds that design.
    fn design(&mut self, network: &FdnControls, kind: &L::Controls) {
        if self.designed != Some((*network, *kind)) {
            self.layout
                .design_into(network, kind, self.rate, &mut self.network);
            self.designed = Some((*network, *kind));
        }
    }

    /// How long `controls` ring from now, with `ramps` as they are: the network's own tail
    /// at the floor plus the pre-delay still to fall, which is at most the ceiling
    /// ([`tail`](Self::tail)): the node clamps to it through its clock. No allocation; the
    /// audio state is not touched.
    ///
    /// The decay does not stretch while a line is in flight (the gains follow the length, see
    /// [`process`](Self::process)), so the design's tail is the tail. The pre-delay ramp adds
    /// what it still has to fall: a pre-delay at `P'` frames with the target `P` delays the
    /// input into the loop by up to `P' - P` more, and `P' <= ` the longest pre-delay, which
    /// the ceiling's worst design already has.
    pub fn tail_of(
        &mut self,
        ramps: &FdnRamps<F, L>,
        network: &FdnControls,
        kind: &L::Controls,
    ) -> Tail {
        self.design(network, kind);
        // The shifted path's delay is in the loop while the send is above zero, and until its
        // ramp has reached zero: the shifted share still circulating is read at the ramp's value.
        let delay = if ramps.shimmer.shifts(kind) {
            self.shimmer.delay()
        } else {
            Samples::ZERO
        };
        set_loop_delay(&mut self.network, delay);
        let DecayTime::Finite(t) = self.network.tail(TAIL_FLOOR) else {
            return tail_of(DecayTime::Unbounded, self.rate);
        };
        let pre = self.network.pre_delay.min(Samples::new(self.pre_cap - 4));
        let behind = ramps.pre_len.now().saturating_sub(pre);
        let seconds = t.get() + behind.span().to_seconds(self.rate).get();
        tail_of(DecayTime::Finite(Seconds::new(seconds)), self.rate)
    }

    /// Back to silence, the chunks counted from here. No allocation.
    pub fn reset(&mut self) {
        let s = &mut self.state;
        s.lines.iter_mut().for_each(DelayRing::clear);
        s.pre.clear();
        s.pole.fill([F::ZERO; 4]);
        for (j, p) in s.phase.iter_mut().enumerate() {
            *p = std::f64::consts::TAU * j as f64 / self.max_lines.max(1) as f64;
        }
        s.fresh = true;
        self.into = 0;
        self.shimmer.reset();
    }

    /// The frames the shifted feedback of a [`ShimmerLayout`](crate::ShimmerLayout) takes to come back into the
    /// lines, the chunk it is collected in and the vocoder's latency: zero for a plain layout.
    /// It is the delay of the reverb's onset and is added to every line's loop in the network
    /// the decay queries read, from a design with a [`ShimmerControls::send`](crate::ShimmerControls::send) above zero.
    #[must_use]
    pub fn shimmer_delay(&self) -> Samples {
        self.shimmer.delay()
    }

    /// Aims `ramps` at the design of `controls`: the first recirculating stage's lines,
    /// matrix, outputs, pre-delay, wet gain and width, each as the value its ramp moves to.
    /// No allocation.
    pub fn retarget(
        &mut self,
        ramps: &mut FdnRamps<F, L>,
        network: &FdnControls,
        kind: &L::Controls,
    ) {
        self.design(network, kind);
        if self.coefficients() {
            ramps.retarget(&self.target, kind);
        }
    }

    /// The coefficient point the design in `network` asks for, written into `target`: `false`
    /// for a network with no recirculating stage or no line, and then the target is as it was.
    fn coefficients(&mut self) -> bool {
        let Some(NetworkStage::Fdn(f)) = self
            .network
            .stages
            .iter()
            .find(|s| matches!(s, NetworkStage::Fdn(_)))
        else {
            return false;
        };
        let n = f.lines.len().min(self.max_lines);
        if n == 0 {
            return false;
        }
        let t = &mut self.target;
        t.n = n;
        let max_len = SampleSpan::from_samples(Samples::new(self.cap - 4));
        // A coefficient of the network is a unit, or zero when it is not finite. Each crosses to
        // `F` once, here.
        fn figure<F: Float>(q: impl Quantity) -> F {
            if q.value().is_finite() {
                F::from_unit(q)
            } else {
                F::ZERO
            }
        }

        if let Some(Absorption::Bands(b)) = f.lines.first().map(|l| l.absorption) {
            set_poles(t, b.crossover(), self.rate);
        }
        let flat = F::from_design(1.0 / (n as f64).sqrt());
        for j in 0..n {
            let line = &f.lines[j];
            t.len[j] = ReadPosition::from_span(
                line.length
                    .clamp(SampleSpan::from_samples(Samples::new(1)), max_len),
            );
            t.gain[j] = [Band::Low, Band::Mid, Band::High]
                .map(|b| line.absorption.gain(b))
                .map(figure::<F>);
            // A diagonal network is `m = 0`; its `v` is a unit vector (line 0)
            // like the rest, so a ramp to a mixing matrix moves between unit
            // vectors and `v_len` stays 1.
            t.v[j] = match &f.matrix {
                Matrix::Diagonal if j == 0 => F::ONE,
                Matrix::Diagonal => F::ZERO,
                Matrix::Householder => flat,
                Matrix::Reflection(v) if v.len() == n => figure(v[j]),
                _ => F::ZERO,
            };
            t.input[j] = figure(f.input.get(j).copied().unwrap_or(Gain::SILENT));
            let row = |r: usize| {
                figure(
                    f.output
                        .rows
                        .get(r)
                        .or_else(|| f.output.rows.first())
                        .and_then(|row| row.get(j))
                        .copied()
                        .unwrap_or(Gain::SILENT),
                )
            };
            t.out[j] = [row(0), row(1)];
        }
        t.v_len = t.v[..n].iter().map(|&v| v * v).sum::<F>().sqrt();
        t.mix = if matches!(f.matrix, Matrix::Diagonal) {
            F::ZERO
        } else {
            F::ONE
        };
        for j in n..self.max_lines {
            t.input[j] = F::ZERO;
            t.out[j] = [F::ZERO; 2];
        }
        let m = f.lines[0].modulation;
        t.depth = F::from_design(m.depth.get().min((self.cap as f64) * 0.25));
        let hz = m.rate.get();
        t.rate = Hz::new(if hz.is_finite() {
            hz.clamp(0.0, 20.0)
        } else {
            0.5
        });
        t.pre_len = ReadPosition::from_frames(self.network.pre_delay)
            .min(ReadPosition::from_frames(Samples::new(self.pre_cap - 4)));
        t.wet = figure(self.network.wet.to_amplitude());
        let width = self.network.width.get();
        t.width = if width.is_finite() {
            F::from_design(width.clamp(0.0, 4.0))
        } else {
            F::ONE
        };
        true
    }

    /// The constants of the chunk that starts now, from the ramps' step.
    fn begin_chunk(&mut self, c: &FdnRamps<F, L>) {
        let s = &mut self.state;
        let n = c.n;
        let depth: f64 = c.depth.now().into();
        if s.fresh {
            s.fresh = false;
            for j in 0..self.max_lines {
                s.c_mod_next[j] = F::from_design(depth * libm::sin(s.phase[j]));
            }
        }
        let dphi = c.rate.angle_per_frame(self.rate).get() * Self::CHUNK as f64;
        for j in 0..n {
            s.phase[j] = (s.phase[j] + dphi) % std::f64::consts::TAU;
            s.c_len[j] = c.len[j].now();
            s.c_mod_prev[j] = s.c_mod_next[j];
            s.c_mod_next[j] = F::from_design(depth * libm::sin(s.phase[j]));
            // The gains are designed for a length `L`: a loop of `L` frames loses 60 dB in `T`
            // seconds at `g = 10^(-3 L / (fs T))` a trip. The same `T` at the `L'` frames the
            // line is now takes `g^(L' / L)`, since `(g^(L'/L))^(fs T / L') = g^(fs T / L)`: each
            // band's gain is raised to `L' / L` while the length is in flight, and an at-rest line
            // runs the designed gains as they are. `L` is the design length the gains ramp
            // with (the gain in flight is a blend of two designs), so the ramp is continuous. The
            // power is taken in `f64` and each band crosses once.
            let gains = c.gain[j].map(|g| g.now());
            let (now, designed) = (c.len[j].now(), c.design_len[j].now());
            let [low, mid, high] = if now == designed {
                gains
            } else {
                let ratio = now.span().get() / designed.span().get();
                gains.map(|g| F::from_design(libm::pow(g.into(), ratio)))
            };
            s.c_gain[j] = [high, mid - high, low - mid];
            s.c_v[j] = c.v[j].now();
            s.c_in[j] = c.input[j].now();
            s.c_out[j] = c.out[j].map(|g| g.now());
        }
        // A straight line between two unit vectors is shorter than either, and
        // `I - 2 v v^T` of a short `v` is a contraction: bring it back to length.
        let len = s.c_v[..n].iter().map(|&x| x * x).sum::<F>().sqrt();
        if len > F::lit(1.0e-6) {
            let k = c.v_len.now() / len;
            s.c_v[..n].iter_mut().for_each(|x| *x *= k);
        }
        s.c_mix = if c.mix.target() > F::lit(0.5) || c.mix.moving() {
            F::ONE
        } else {
            F::ZERO
        };
        self.shimmer
            .begin_chunk(c.shimmer.now(), c.shimmer.pitch(), n);
        s.c_wet = c.wet.now();
        s.c_width = c.width.now();
        s.c_pre = c.pre_len.now();
    }

    /// Runs `input` into `output`, the same number of frames: the input channels averaged to
    /// one, the wet signal on the first two output channels. `ramps` steps at each chunk's
    /// start, so a call that ends inside a chunk goes on in the next. Before the first
    /// [`retarget`](Self::retarget) the output is silent. No allocation.
    ///
    /// # Errors
    ///
    /// [`BlockMismatch`] when the block's output is not two channels wide.
    pub fn process(
        &mut self,
        ramps: &mut FdnRamps<F, L>,
        block: &mut InputOutput<'_, F>,
    ) -> Result<(), BlockMismatch> {
        // Any input is mixed down to the one wet signal, which is a stereo pair.
        expect_output_width(block, 2)?;
        let frames = block.frames().get();
        let (input, mut output) = block.split();
        let width = input.channels();
        let xs: [&[F]; MAX_CHANNELS] = core::array::from_fn(|c| {
            if c < width {
                &input.plane(c).samples()[..frames]
            } else {
                &[]
            }
        });
        let mut planes = output.iter_mut();
        let mut left = planes.next().map(|p| &mut p.into_samples_mut()[..frames]);
        let mut right = planes.next().map(|p| &mut p.into_samples_mut()[..frames]);

        let n = ramps.n;
        if n == 0 {
            for y in left.iter_mut().chain(right.iter_mut()) {
                y.fill(F::ZERO);
            }
            return Ok(());
        }
        let scale = F::from_design(1.0 / width.max(1) as f64);
        let p = ramps.complement;
        let half = F::lit(0.5);
        let mut d = [F::ZERO; 16];
        for i in 0..frames {
            if self.into == 0 {
                ramps.step();
                self.begin_chunk(ramps);
            }
            let s = &mut self.state;
            let t = along::<F>(self.into as u32, Self::CHUNK as u32);
            let x: F = xs[..width].iter().map(|ch| ch[i]).sum::<F>() * scale;

            s.pre.put(x);
            let xd = s.pre.at(s.c_pre);
            s.pre.advance();

            let (mut out_l, mut out_r) = (F::ZERO, F::ZERO);
            for j in 0..n {
                let delay = F::offset_position(
                    s.c_len[j],
                    crate::lerp(s.c_mod_prev[j], s.c_mod_next[j], t),
                )
                .max(ONE_FRAME);
                d[j] = s.lines[j].at(delay);
                out_l += s.c_out[j][0] * d[j];
                out_r += s.c_out[j][1] * d[j];
            }

            let mut sum = F::ZERO;
            for j in 0..n {
                let q = &mut s.pole[j];
                q[0] += p[0] * (d[j] - q[0]);
                q[1] += p[0] * (q[0] - q[1]);
                q[2] += p[1] * (d[j] - q[2]);
                q[3] += p[1] * (q[2] - q[3]);
                let [high, c2, c1] = s.c_gain[j];
                d[j] = high * d[j] + c2 * q[3] + c1 * q[1];
                sum += s.c_v[j] * d[j];
            }
            // `m` scales the projection, so `m = 1` is the reflection to the bit.
            let sum = s.c_mix * sum;
            // Each line takes its share of the input and the reflected feedback, and for a
            // shimmer the shifted share of its mean.
            self.shimmer
                .feed(self.into, &mut d[..n], &s.c_v[..n], &s.c_in[..n], xd, sum);
            for j in 0..n {
                s.lines[j].put(d[j]);
            }
            // Every row moves on, the idle ones too: a line past `n` keeps the frames it held
            // and a later regrowth reads them at the right age.
            s.lines.iter_mut().for_each(DelayRing::advance);

            let (mid, side) = ((out_l + out_r) * half, (out_l - out_r) * half);
            let wet = s.c_wet;
            if let Some(y) = left.as_deref_mut() {
                y[i] = (mid + s.c_width * side) * wet;
            }
            if let Some(y) = right.as_deref_mut() {
                y[i] = (mid - s.c_width * side) * wet;
            }
            self.into = (self.into + 1) % Self::CHUNK;
            if self.into == 0 {
                self.shimmer.end_chunk();
            }
        }
        Ok(())
    }

    /// The energy the lines hold: the last `length` samples of each, which is the whole
    /// state of a frozen network (the absorption's one-poles are not read while every gain is
    /// 1). For the tests.
    #[cfg(test)]
    pub(crate) fn state_energy(&self, ramps: &FdnRamps<F, L>) -> f64 {
        let s = &self.state;
        (0..ramps.n)
            .map(|j| {
                let len = ramps.len[j].now().frames().get();
                (1..=len)
                    .map(|i| {
                        let x = s.lines[j].back(i);
                        x.to_f64() * x.to_f64()
                    })
                    .sum::<f64>()
            })
            .sum()
    }
}

/// Sets the one-poles of `t` to the crossovers, in Hz, at `rate`, when they moved.
fn set_poles<F: Float>(t: &mut Target<F>, crossover: [Hz; 2], rate: SampleRate) {
    if crossover == t.crossover {
        return;
    }
    t.crossover = crossover;
    for ((p, c), f) in t.pole.iter_mut().zip(&mut t.complement).zip(crossover) {
        *p = cutoff_pole(f, rate);
        *c = F::from_design(cutoff_complement(f, rate));
    }
}

#[cfg(test)]
mod tests;
