//! [`PlateBank`]: the Dattorro plate over buffers views, with the controls that sound as
//! [`Glide`]s ([`PlateGlides`]).
//!
//! The design is [`PlateControls::design_into`]: the bank reads the lengths and coefficients
//! the [`Network`] holds and runs the topology of [`design`](super::design) over them. Mono in
//! (the input channels are averaged), stereo out: the wet signal alone, the paper's seven
//! output taps a side (table 2) at 0.6 each.
//!
//! A **lattice** of delay `D` and coefficient `g` is `v[n] = x[n] + g v[n - D]`,
//! `y[n] = v[n - D] - g v[n]`: `H(z) = (z^-D - g) / (1 - g z^-D)`. The two modulated ones read
//! `v` at `D` plus a slow sinusoid, by linear interpolation: the left half's offset is
//! `A sin(2 pi f t)` and the right half's `A cos(2 pi f t)`, a quarter turn ahead of the left's,
//! with `t` the time since the last reset, `f` the modulation's rate and `A` its depth in
//! frames. A damping low-pass is `y[n] = (1 - p) x[n] + p y[n-1]`; the bandwidth's is the same
//! with the pole `exp(-2 pi fc / fs)` of its cutoff `fc`. A pole near 1 (damping up to 0.999, a
//! bandwidth of 10 Hz at 192 kHz is `1 - 3.3e-4`) is stored as its complement `c = 1 - p`,
//! designed in `f64`, and the low-pass runs `y += c (x - y)`: the complement keeps its relative
//! precision where `1 - p` formed in `F` would lose `u / c` of it (9e-5 at 192 kHz in `f32`).
//!
//! **Chunks.** The render counts [`CHUNK`](PlateBank::CHUNK) frames from the last reset and
//! works once at each chunk's start: every [`Glide`] takes a step and the chunk's constants are
//! read from them, the modulation's offsets at the chunk's start and end among them. The chunks
//! are counted from the first frame since the last reset, so the output does not depend on how
//! the input is cut into calls. The caller [aims](PlateBank::aim) the glides when its controls
//! move: each sounds through 20 ms of whole chunks, the pre-delay by two frames a chunk.
//!
//! **Format.** The state and the views are the bank's float `F`. The coefficients are designed
//! in `f64` and cross to `F` once per design (`target_of`, [`aim`](PlateBank::aim)). The decay
//! gain is the loop gain's fourth root, `c = g^(1/4)`, applied four times a trip. The longest
//! decay (8 s at 192 kHz, `T` of 1.39e5 frames) has `|ln g| = 0.63` and `1 - c = 0.145`, so `F`'s
//! rounding of `c` (3e-8) moves the RT60 by about 2e-7 of itself in `f32`. A loop gain multiplies a
//! delay line's sample, so the product rounds by about `u` a trip whatever the form: it is not
//! stored as a complement.

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_types::MAX_CHANNELS;
use tutti_units::{Hz, ReadPosition, SampleRate, SampleSpan, Samples, Seconds, Tail};

use super::design::{MIN_BANDWIDTH, OUTPUT_GAIN, OUTPUT_TAPS, PlateControls, Point, scaled};
use crate::delay::DelayRing;
use crate::design::along;
use crate::onepole::{cutoff_complement, cutoff_pole, ring_frames};
use crate::reverb::network::{Absorption, Network, Stage};
use crate::reverb::{CHUNK, ramp, tail_of};
use crate::{Glide, Lerp, Ring, Steps, System, TAIL_FLOOR};

const PRE_STEP: f32 = 2.0; // frames a chunk the pre-delay moves
const SLACK: usize = 4; // frames a line keeps beyond its length

/// A delay line of `frames` and the slack that keeps a modulated read inside it.
fn line<F: Float>(frames: Samples) -> DelayRing<F> {
    DelayRing::new(frames.get() + SLACK)
}

/// A lattice of coefficient `g` over `line` at `delay` frames: `x` in, `y` out.
#[inline]
fn lattice<F: Float>(line: &mut DelayRing<F>, x: F, g: F, delay: ReadPosition) -> F {
    let z = line.at(delay);
    let v = x + g * z;
    line.put(v);
    z - g * v
}

/// The lengths and fixed coefficients the bank was built to, read from a design.
#[derive(Clone, Debug)]
struct Shape {
    /// The input diffusers' lengths, in frames.
    input: [Samples; 4],
    /// Per half: the modulated lattice, delay 1, lattice 2 and delay 2.
    halves: [[Samples; 4]; 2],
    mod_rate: Hz,
    /// The deepest excursion and the longest pre-delay the design can ask for.
    excursion: Samples,
    pre: Samples,
}

/// A coefficient point: what a design asks for (the values [`PlateGlides`] glide to) and
/// what a chunk runs.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Coeffs<F> {
    /// The decay coefficient.
    decay: F,
    /// The damping pole's complement `1 - p`.
    damping: F,
    /// The second lattice's coefficient.
    lattice2: F,
    /// The modulated lattice's coefficient.
    lattice1: F,
    /// The input diffusers' coefficients.
    input: [F; 4],
    /// The modulation excursion in frames.
    depth: F,
    /// The pre-delay.
    pre: ReadPosition,
    /// The bandwidth pole's complement `1 - p`.
    bandwidth: F,
}

impl<F: Float> Default for Coeffs<F> {
    fn default() -> Self {
        Self {
            decay: F::ZERO,
            damping: F::ONE,
            lattice2: F::ZERO,
            lattice1: F::ZERO,
            input: [F::ZERO; 4],
            depth: F::ZERO,
            pre: ReadPosition::START,
            bandwidth: F::ONE,
        }
    }
}

/// What a [`PlateBank`] approaches: every sounding control a [`Glide`], the pre-delay by a
/// distance a chunk and the rest in 20 ms.
///
/// Aimed by [`PlateBank::aim`], stepped once a chunk by [`PlateBank::process`]. A glide steps
/// before its chunk runs: the first chunk after an aim reads step 1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlateGlides<F: Float> {
    ramp: Steps,
    decay: Glide<F>,
    damping: Glide<F>,
    lattice2: Glide<F>,
    lattice1: Glide<F>,
    input: [Glide<F>; 4],
    depth: Glide<F>,
    pre: Glide<ReadPosition>,
    bandwidth: Glide<F>,
}

impl<F: Float + Lerp> PlateGlides<F> {
    fn each(&mut self) -> impl Iterator<Item = &mut Glide<F>> {
        [
            &mut self.decay,
            &mut self.damping,
            &mut self.lattice2,
            &mut self.lattice1,
            &mut self.depth,
            &mut self.bandwidth,
        ]
        .into_iter()
        .chain(&mut self.input)
    }

    /// Lands on the targets now.
    pub fn settle(&mut self) {
        self.each().for_each(Glide::settle);
        self.pre.settle();
    }

    fn step(&mut self) {
        self.each().for_each(|g| g.advance(1));
        self.pre.advance(1);
    }

    /// The coefficients now.
    fn now(&self) -> Coeffs<F> {
        Coeffs {
            decay: self.decay.now(),
            damping: self.damping.now(),
            lattice2: self.lattice2.now(),
            lattice1: self.lattice1.now(),
            input: self.input.map(|g| g.now()),
            depth: self.depth.now(),
            pre: self.pre.now(),
            bandwidth: self.bandwidth.now(),
        }
    }

    fn aim(&mut self, t: &Coeffs<F>) {
        let ramp = self.ramp;
        self.decay.aim_by(t.decay, ramp);
        self.damping.aim_by(t.damping, ramp);
        self.lattice2.aim_by(t.lattice2, ramp);
        self.lattice1.aim_by(t.lattice1, ramp);
        for (g, &v) in self.input.iter_mut().zip(&t.input) {
            g.aim_by(v, ramp);
        }
        self.depth.aim_by(t.depth, ramp);
        self.pre.aim_position(t.pre, Steps::Rate(PRE_STEP));
        self.bandwidth.aim_by(t.bandwidth, ramp);
    }
}

/// An output tap with its offset scaled to the rate.
#[derive(Clone, Copy, Debug)]
struct TapAt<F> {
    half: usize,
    point: Point,
    at: ReadPosition,
    sign: F,
}

/// The state of one tank half.
#[derive(Clone, Debug)]
struct Half<F> {
    modulated: DelayRing<F>,
    delay1: DelayRing<F>,
    lattice2: DelayRing<F>,
    delay2: DelayRing<F>,
    /// The damping low-pass's output.
    damped: F,
}

/// The constants of the chunk being run: its coefficients and the modulation offsets, per half,
/// at the chunk's start and end.
#[derive(Clone, Copy, Debug)]
struct Chunk<F> {
    k: Coeffs<F>,
    prev: [F; 2],
    next: [F; 2],
}

/// A plate reverberator at one rate: mono in, stereo out.
///
/// Built on the control side for one rate, where it allocates the lines; a clone keeps their
/// size, and [`reset`](Self::reset) zeroes them without allocating.
#[derive(Clone, Debug)]
pub struct PlateBank<F: Float> {
    rate: SampleRate,
    shape: Shape,
    /// The output taps, left then right, at the rate's offsets.
    taps: [[TapAt<F>; 7]; 2],
    /// The output gain, narrowed once.
    output_gain: F,
    /// The input diffusers' lengths and each half's, as positions.
    input_len: [ReadPosition; 4],
    half_len: [[ReadPosition; 4]; 2],
    /// How long it rings at its worst.
    ceiling: Tail,
    /// Frames of the current chunk already run: 0 at a chunk's start.
    into: Samples,
    pre: DelayRing<F>,
    /// The bandwidth low-pass's output.
    bandwidth: F,
    input: [DelayRing<F>; 4],
    halves: [Half<F>; 2],
    /// The phase of the modulation, in turns.
    phase: f64,
    /// Whether the modulation has not started: its first chunk reads the phase's own value as
    /// the previous one.
    fresh: bool,
    chunk: Chunk<F>,
    /// The controls the design in `network` is of.
    designed: Option<PlateControls>,
    /// Where a design is made, so the audio thread builds none.
    network: Box<Network>,
}

impl<F: Float + Lerp> PlateBank<F> {
    /// The frames between a glide's steps: the chunk.
    pub const CHUNK: usize = CHUNK;

    /// A silent plate at `rate`. Allocates: build it on the control side.
    #[must_use]
    pub fn new(rate: SampleRate) -> Self {
        let mut network = Box::new(Network::new(rate));
        let worst = PlateControls {
            pre_delay: crate::reverb::fdn::design::MAX_PRE_DELAY,
            modulation: tutti_units::Depth::of(1.0),
            ..PlateControls::DEFAULT
        };
        worst.design_into(rate, &mut network);
        let shape = shape_of(&network);
        let half = |h: &[Samples; 4]| Half::<F> {
            modulated: line(h[0] + shape.excursion + Samples(1)),
            delay1: line(h[1]),
            lattice2: line(h[2]),
            delay2: line(h[3]),
            damped: F::ZERO,
        };
        let mut bank = Self {
            rate,
            taps: OUTPUT_TAPS.map(|side| {
                side.map(|k| TapAt {
                    half: k.half,
                    point: k.point,
                    at: ReadPosition::from_span(SampleSpan::clamped(scaled(k.offset, rate))),
                    sign: F::from_unit(k.sign),
                })
            }),
            ceiling: tail_of(
                match PlateControls::capacity(rate).tail {
                    Ring::Finite(t) => {
                        Ring::Finite(Seconds(t.get() + bandwidth_ring(MIN_BANDWIDTH, rate).get()))
                    }
                    other => other,
                },
                rate,
            ),
            output_gain: F::from_unit(OUTPUT_GAIN),
            input_len: shape.input.map(ReadPosition::from_frames),
            half_len: shape.halves.map(|h| h.map(ReadPosition::from_frames)),
            into: Samples::ZERO,
            pre: line(shape.pre),
            bandwidth: F::ZERO,
            input: shape.input.map(line),
            halves: [half(&shape.halves[0]), half(&shape.halves[1])],
            phase: 0.0,
            fresh: true,
            chunk: Chunk {
                k: Coeffs::default(),
                prev: [F::ZERO; 2],
                next: [F::ZERO; 2],
            },
            designed: None,
            network,
            shape,
        };
        bank.reset();
        bank
    }

    /// The glides of this bank, at rest and aimed at nothing: its first [`aim`](Self::aim)
    /// is the caller's to land. Until then the bank is silent.
    #[must_use]
    pub fn glides(&self) -> PlateGlides<F> {
        let rest = Glide::landed(F::ZERO);
        PlateGlides {
            ramp: ramp(self.rate),
            decay: rest,
            damping: Glide::landed(F::ONE),
            lattice2: rest,
            lattice1: rest,
            input: [rest; 4],
            depth: rest,
            pre: Glide::landed(ReadPosition::START),
            bandwidth: Glide::landed(F::ONE),
        }
    }

    /// How long it rings at its worst: the longest decay, the longest pre-delay.
    #[must_use]
    pub fn tail(&self) -> Tail {
        self.ceiling
    }

    /// Designs `controls` into the scratch network, unless it already holds that design.
    fn design(&mut self, controls: &PlateControls) {
        if self.designed != Some(*controls) {
            controls.design_into(self.rate, &mut self.network);
            self.designed = Some(*controls);
        }
    }

    /// How long `controls` ring from now, with `glides` as they are: the network's own tail at
    /// the floor (it counts each damping one-pole's peak delay, so the loop it bounds is the
    /// real one), plus the ring of the bandwidth one-pole (`ln(floor) / ln(pole)` frames), plus
    /// the pre-delay still to fall; at most the ceiling ([`tail`](Self::tail)). No allocation.
    pub fn tail_of(&mut self, glides: &PlateGlides<F>, controls: &PlateControls) -> Tail {
        self.design(controls);
        let Ring::Finite(t) = self.network.tail(TAIL_FLOOR) else {
            return tail_of(Ring::Unbounded, self.rate);
        };
        let pre = self.network.pre_delay.min(self.shape.pre);
        let behind = glides
            .pre
            .now()
            .saturating_sub(pre)
            .span()
            .to_seconds(self.rate);
        let input = bandwidth_ring(controls.bandwidth_at(self.rate), self.rate);
        tail_of(
            Ring::Finite(Seconds(t.get() + input.get() + behind.get())),
            self.rate,
        )
    }

    /// Back to silence, the chunks counted from here. No allocation.
    pub fn reset(&mut self) {
        self.pre.clear();
        self.bandwidth = F::ZERO;
        self.input.iter_mut().for_each(DelayRing::clear);
        for h in &mut self.halves {
            h.modulated.clear();
            h.delay1.clear();
            h.lattice2.clear();
            h.delay2.clear();
            h.damped = F::ZERO;
        }
        self.phase = 0.0;
        self.fresh = true;
        self.into = Samples::ZERO;
    }

    /// Aims `glides` at the design of `controls`. No allocation.
    pub fn aim(&mut self, glides: &mut PlateGlides<F>, controls: &PlateControls) {
        self.design(controls);
        if let Some(mut t) = target_of::<F>(&self.network) {
            t.bandwidth = F::from_design(cutoff_complement(
                controls.bandwidth_at(self.rate),
                self.rate,
            ));
            t.pre = t.pre.min(ReadPosition::from_frames(self.shape.pre));
            glides.aim(&t);
        }
    }

    /// The constants of the chunk that starts now, from the glides' step.
    fn begin_chunk(&mut self, g: &PlateGlides<F>) {
        let k = g.now();
        let depth: f64 = k.depth.into();
        let at = |phase: f64| {
            let turn = std::f64::consts::TAU * phase;
            [
                F::from_design(depth * libm::sin(turn)),
                F::from_design(depth * libm::cos(turn)),
            ]
        };
        let c = &mut self.chunk;
        if self.fresh {
            self.fresh = false;
            c.next = at(self.phase);
        }
        self.phase =
            (self.phase + self.shape.mod_rate.get() * Self::CHUNK as f64 / self.rate.get()) % 1.0;
        c.prev = c.next;
        c.next = at(self.phase);
        c.k = k;
    }

    /// Runs `input` into `output`, the same number of frames: the input channels averaged to
    /// one, the wet signal on the first two output channels. `glides` steps at each chunk's
    /// start, so a call that ends inside a chunk goes on in the next. A channel of `output`
    /// past the second is not touched; before the first [`aim`](Self::aim) the output is
    /// silent. No allocation.
    pub fn process(
        &mut self,
        glides: &mut PlateGlides<F>,
        input: Planes<'_, F>,
        mut output: PlanesMut<'_, '_, F>,
    ) {
        let frames = input.frames().get().min(output.frames().get());
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

        if glides.decay.target() == F::ZERO {
            for y in left.iter_mut().chain(right.iter_mut()) {
                y.fill(F::ZERO);
            }
            return;
        }
        let scale = F::from_design(1.0 / width.max(1) as f64);
        for i in 0..frames {
            if self.into == Samples::ZERO {
                glides.step();
                self.begin_chunk(glides);
            }
            let t = along::<F>(self.into.get() as u32, Self::CHUNK as u32);
            let x: F = xs[..width].iter().map(|ch| ch[i]).sum::<F>() * scale;
            let (l, r) = self.frame(x, t);
            if let Some(y) = left.as_deref_mut() {
                y[i] = l;
            }
            if let Some(y) = right.as_deref_mut() {
                y[i] = r;
            }
            self.into = (self.into + Samples(1)) % Samples(Self::CHUNK);
        }
    }

    /// One frame: `x` through the pre-delay, the bandwidth, the input diffusers and the tank;
    /// the left and right taps. `t` is how far into the chunk the frame is.
    fn frame(&mut self, x: F, t: F) -> (F, F) {
        let c = self.chunk;
        let k = c.k;
        self.pre.put(x);
        let delayed = self.pre.at(k.pre);
        self.pre.advance();
        self.bandwidth += k.bandwidth * (delayed - self.bandwidth);
        let mut u = self.bandwidth;
        for ((line, &n), &g) in self.input.iter_mut().zip(&self.input_len).zip(&k.input) {
            u = lattice(line, u, g, n);
            line.advance();
        }

        // Each half reads the other's last delay before either writes this frame.
        let ends = [0, 1].map(|h| self.halves[h].delay2.at(self.half_len[h][3]));
        for h in 0..2 {
            let [d1, l1, d2, _] = self.half_len[h];
            let half = &mut self.halves[h];
            let input = u + k.decay * ends[1 - h];
            let sweep = crate::lerp(c.prev[h], c.next[h], t);
            let y = lattice(
                &mut half.modulated,
                input,
                k.lattice1,
                F::offset_position(d1, sweep).max(ReadPosition::from_frames(Samples::new(1))),
            );
            half.delay1.put(y);
            let q = half.delay1.at(l1);
            half.damped += k.damping * (q - half.damped);
            let y = lattice(&mut half.lattice2, k.decay * half.damped, k.lattice2, d2);
            half.delay2.put(y);
        }

        let [left, right] = self.taps.map(|side| {
            self.output_gain
                * side
                    .iter()
                    .map(|k| {
                        let half = &self.halves[k.half];
                        k.sign
                            * match k.point {
                                Point::Delay1 => half.delay1.at(k.at),
                                Point::Lattice2 => half.lattice2.at(k.at),
                                Point::Delay2 => half.delay2.at(k.at),
                            }
                    })
                    .sum::<F>()
        });

        for h in &mut self.halves {
            h.modulated.advance();
            h.delay1.advance();
            h.lattice2.advance();
            h.delay2.advance();
        }
        (left, right)
    }
}

/// How long the bandwidth one-pole of cutoff `cutoff` rings after its input stops, to
/// [`TAIL_FLOOR`]: its pole `q = exp(-2 pi fc / fs)` leaves `q^n`, which is under the floor
/// after `ln(floor) / ln(q)` frames. Added to the tank's ring: the tank hears the input that
/// long after it stops.
fn bandwidth_ring(cutoff: Hz, rate: SampleRate) -> Seconds {
    let pole = cutoff_pole(cutoff, rate);
    Seconds(ring_frames(pole, TAIL_FLOOR) / rate.get())
}

/// The lengths and fixed coefficients of the design in `network`.
fn shape_of(network: &Network) -> Shape {
    let frames = SampleSpan::to_samples_floor;
    let mut input = [Samples(1); 4];
    for (slot, stage) in input.iter_mut().zip(network.stages.iter()) {
        if let Stage::AllPass(a) = stage {
            *slot = frames(a[0].delay);
        }
    }
    let mut shape = Shape {
        input,
        halves: [[Samples(1); 4]; 2],
        mod_rate: Hz::ZERO,
        excursion: Samples::ZERO,
        pre: network.pre_delay,
    };
    if let Some(Stage::Fdn(f)) = network.stages.iter().find(|s| matches!(s, Stage::Fdn(_))) {
        for (h, lines) in shape.halves.iter_mut().zip(f.lines.chunks(2)) {
            let (Some(a), Some(b)) = (lines[0].diffuser, lines[1].diffuser) else {
                continue;
            };
            *h = [
                frames(a.delay),
                frames(lines[0].length),
                frames(b.delay),
                frames(lines[1].length),
            ];
            shape.mod_rate = a.modulation.rate;
            shape.excursion = shape.excursion.max(a.modulation.depth.to_samples_ceil());
        }
    }
    shape
}

/// The coefficient point the design in `network` asks for, `None` for a network that is not a
/// plate's. The bandwidth is the caller's.
fn target_of<F: Float>(network: &Network) -> Option<Coeffs<F>> {
    let Stage::Fdn(f) = network.stages.iter().find(|s| matches!(s, Stage::Fdn(_)))? else {
        return None;
    };
    let (a, b) = (f.lines.first()?, f.lines.get(1)?);
    let mut input = [F::ZERO; 4];
    for (slot, stage) in input.iter_mut().zip(network.stages.iter()) {
        if let Stage::AllPass(d) = stage {
            *slot = F::from_unit(d[0].gain);
        }
    }
    let Absorption::OnePole { gain, damp } = a.absorption else {
        return None;
    };
    Some(Coeffs {
        decay: F::from_unit(gain),
        damping: F::from_design(1.0 - damp),
        lattice2: F::from_unit(b.diffuser?.gain),
        lattice1: F::from_unit(a.diffuser?.gain),
        input,
        depth: F::from_unit(a.diffuser?.modulation.depth),
        pre: ReadPosition::from_frames(network.pre_delay),
        bandwidth: F::ZERO,
    })
}

#[cfg(test)]
mod tests;
