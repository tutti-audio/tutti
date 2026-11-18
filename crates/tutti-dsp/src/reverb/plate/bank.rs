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
//! with the pole `exp(-2 pi fc / fs)` of its cutoff `fc`.
//!
//! **Chunks.** The render counts [`CHUNK`](PlateBank::CHUNK) frames from the last reset and
//! works once at each chunk's start: every [`Glide`] takes a step and the chunk's constants are
//! read from them, the modulation's offsets at the chunk's start and end among them. The chunks
//! are counted from the first frame since the last reset, so the output does not depend on how
//! the input is cut into calls. The caller [aims](PlateBank::aim) the glides when its controls
//! move: each sounds through 20 ms of whole chunks, the pre-delay by two frames a chunk.
//!
//! **Format.** The state is `f32`; a view of another [`Float`] is converted at the edges.

use tutti_buffers::{Float, Planes, PlanesMut, Sample};
use tutti_types::{Gain, Hz, MAX_CHANNELS, SampleRate, SampleSpan, Samples, Seconds, Tail};

use super::design::{MIN_BANDWIDTH, OUTPUT_GAIN, OUTPUT_TAPS, PlateControls, Point, scaled};
use crate::reverb::network::{Absorption, Network, Stage};
use crate::reverb::{CHUNK, cutoff_pole, ramp, read, tail_of};
use crate::{Glide, Ring, Steps, System, TAIL_FLOOR};

const PRE_STEP: f32 = 2.0; // frames a chunk the pre-delay moves
const SLACK: usize = 4; // frames a line keeps beyond its length

/// One delay line: `put` writes the current frame and `read` takes it back `delay` frames;
/// `advance` moves to the next frame.
#[derive(Clone, Debug)]
struct Line {
    buf: Vec<f32>,
    w: usize,
}

impl Line {
    fn new(frames: Samples) -> Self {
        Self {
            buf: vec![0.0; frames.get() + SLACK],
            w: 0,
        }
    }

    fn clear(&mut self) {
        self.buf.fill(0.0);
        self.w = 0;
    }

    #[inline]
    fn put(&mut self, x: f32) {
        self.buf[self.w] = x;
    }

    #[inline]
    fn read(&self, delay: f32) -> f32 {
        read(&self.buf, self.w, delay)
    }

    #[inline]
    fn advance(&mut self) {
        self.w = (self.w + 1) % self.buf.len();
    }

    /// A lattice of coefficient `g` over this line at `delay` frames: `x` in, `y` out.
    #[inline]
    fn lattice(&mut self, x: f32, g: f32, delay: f32) -> f32 {
        let z = self.read(delay);
        let v = x + g * z;
        self.put(v);
        z - g * v
    }
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
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Coeffs {
    /// The decay coefficient.
    decay: f32,
    /// The damping pole.
    damping: f32,
    /// The second lattice's coefficient.
    lattice2: f32,
    /// The modulated lattice's coefficient.
    lattice1: f32,
    /// The input diffusers' coefficients.
    input: [f32; 4],
    /// The modulation excursion in frames.
    depth: f32,
    /// The pre-delay in frames.
    pre: f32,
    /// The bandwidth's pole.
    bandwidth: f32,
}

/// What a [`PlateBank`] approaches: every sounding control a [`Glide`], the pre-delay by a
/// distance a chunk and the rest in 20 ms.
///
/// Aimed by [`PlateBank::aim`], stepped once a chunk by [`PlateBank::process`]. A glide steps
/// before its chunk runs: the first chunk after an aim reads step 1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlateGlides {
    ramp: Steps,
    decay: Glide<f32>,
    damping: Glide<f32>,
    lattice2: Glide<f32>,
    lattice1: Glide<f32>,
    input: [Glide<f32>; 4],
    depth: Glide<f32>,
    pre: Glide<f32>,
    bandwidth: Glide<f32>,
}

impl PlateGlides {
    fn each(&mut self) -> impl Iterator<Item = &mut Glide<f32>> {
        [
            &mut self.decay,
            &mut self.damping,
            &mut self.lattice2,
            &mut self.lattice1,
            &mut self.depth,
            &mut self.pre,
            &mut self.bandwidth,
        ]
        .into_iter()
        .chain(&mut self.input)
    }

    /// Lands on the targets now.
    pub fn settle(&mut self) {
        self.each().for_each(Glide::settle);
    }

    fn step(&mut self) {
        self.each().for_each(|g| g.advance(1));
    }

    /// The coefficients now.
    fn now(&self) -> Coeffs {
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

    fn aim(&mut self, t: &Coeffs) {
        let ramp = self.ramp;
        self.decay.aim_by(t.decay, ramp);
        self.damping.aim_by(t.damping, ramp);
        self.lattice2.aim_by(t.lattice2, ramp);
        self.lattice1.aim_by(t.lattice1, ramp);
        for (g, &v) in self.input.iter_mut().zip(&t.input) {
            g.aim_by(v, ramp);
        }
        self.depth.aim_by(t.depth, ramp);
        self.pre.aim_by(t.pre, Steps::Rate(PRE_STEP));
        self.bandwidth.aim_by(t.bandwidth, ramp);
    }
}

/// An output tap with its offset scaled to the rate.
#[derive(Clone, Copy, Debug)]
struct TapAt {
    half: usize,
    point: Point,
    at: f32,
    sign: Gain,
}

/// The state of one tank half.
#[derive(Clone, Debug)]
struct Half {
    modulated: Line,
    delay1: Line,
    lattice2: Line,
    delay2: Line,
    /// The damping low-pass's output.
    damped: f32,
}

/// The constants of the chunk being run: its coefficients and the modulation offsets, per half,
/// at the chunk's start and end.
#[derive(Clone, Copy, Debug, Default)]
struct Chunk {
    k: Coeffs,
    prev: [f32; 2],
    next: [f32; 2],
}

/// A plate reverberator at one rate: mono in, stereo out.
///
/// Built on the control side for one rate, where it allocates the lines; a clone keeps their
/// size, and [`reset`](Self::reset) zeroes them without allocating.
#[derive(Clone, Debug)]
pub struct PlateBank {
    rate: SampleRate,
    shape: Shape,
    /// The output taps, left then right, at the rate's offsets.
    taps: [[TapAt; 7]; 2],
    /// How long it rings at its worst.
    ceiling: Tail,
    /// Frames of the current chunk already run: 0 at a chunk's start.
    into: Samples,
    pre: Line,
    /// The bandwidth low-pass's output.
    bandwidth: f32,
    input: [Line; 4],
    halves: [Half; 2],
    /// The phase of the modulation, in turns.
    phase: f64,
    /// Whether the modulation has not started: its first chunk reads the phase's own value as
    /// the previous one.
    fresh: bool,
    chunk: Chunk,
    /// The controls the design in `network` is of.
    designed: Option<PlateControls>,
    /// Where a design is made, so the audio thread builds none.
    network: Box<Network>,
}

impl PlateBank {
    /// The frames between a glide's steps: the chunk.
    pub const CHUNK: usize = CHUNK;

    /// A silent plate at `rate`. Allocates: build it on the control side.
    #[must_use]
    pub fn new(rate: SampleRate) -> Self {
        let mut network = Box::new(Network::new(rate));
        let worst = PlateControls {
            pre_delay: crate::reverb::fdn::design::MAX_PRE_DELAY,
            modulation: tutti_types::Depth::of(1.0),
            ..PlateControls::DEFAULT
        };
        worst.design_into(rate, &mut network);
        let shape = shape_of(&network);
        let half = |h: &[Samples; 4]| Half {
            modulated: Line::new(h[0] + shape.excursion + Samples(1)),
            delay1: Line::new(h[1]),
            lattice2: Line::new(h[2]),
            delay2: Line::new(h[3]),
            damped: 0.0,
        };
        let mut bank = Self {
            rate,
            taps: OUTPUT_TAPS.map(|side| {
                side.map(|k| TapAt {
                    half: k.half,
                    point: k.point,
                    at: scaled(k.offset, rate) as f32,
                    sign: k.sign,
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
            into: Samples::ZERO,
            pre: Line::new(shape.pre),
            bandwidth: 0.0,
            input: shape.input.map(Line::new),
            halves: [half(&shape.halves[0]), half(&shape.halves[1])],
            phase: 0.0,
            fresh: true,
            chunk: Chunk::default(),
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
    pub fn glides(&self) -> PlateGlides {
        let rest = Glide::landed(0.0);
        PlateGlides {
            ramp: ramp(self.rate),
            decay: rest,
            damping: rest,
            lattice2: rest,
            lattice1: rest,
            input: [rest; 4],
            depth: rest,
            pre: rest,
            bandwidth: rest,
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
    pub fn tail_of(&mut self, glides: &PlateGlides, controls: &PlateControls) -> Tail {
        self.design(controls);
        let Ring::Finite(t) = self.network.tail(TAIL_FLOOR) else {
            return tail_of(Ring::Unbounded, self.rate);
        };
        let pre = (self.network.pre_delay.get() as f32).clamp(0.0, self.shape.pre.get() as f32);
        let behind = f64::from((glides.pre.now() - pre).max(0.0));
        let input = bandwidth_ring(controls.bandwidth_at(self.rate), self.rate);
        tail_of(
            Ring::Finite(Seconds(t.get() + input.get() + behind / self.rate.get())),
            self.rate,
        )
    }

    /// Back to silence, the chunks counted from here. No allocation.
    pub fn reset(&mut self) {
        self.pre.clear();
        self.bandwidth = 0.0;
        self.input.iter_mut().for_each(Line::clear);
        for h in &mut self.halves {
            h.modulated.clear();
            h.delay1.clear();
            h.lattice2.clear();
            h.delay2.clear();
            h.damped = 0.0;
        }
        self.phase = 0.0;
        self.fresh = true;
        self.into = Samples::ZERO;
    }

    /// Aims `glides` at the design of `controls`. No allocation.
    pub fn aim(&mut self, glides: &mut PlateGlides, controls: &PlateControls) {
        self.design(controls);
        if let Some(mut t) = target_of(&self.network) {
            t.bandwidth = cutoff_pole(controls.bandwidth_at(self.rate), self.rate) as f32;
            t.pre = t.pre.clamp(0.0, self.shape.pre.get() as f32);
            glides.aim(&t);
        }
    }

    /// The constants of the chunk that starts now, from the glides' step.
    fn begin_chunk(&mut self, g: &PlateGlides) {
        let k = g.now();
        let depth = f64::from(k.depth);
        let at = |phase: f64| {
            let turn = std::f64::consts::TAU * phase;
            [
                (depth * libm::sin(turn)) as f32,
                (depth * libm::cos(turn)) as f32,
            ]
        };
        let c = &mut self.chunk;
        if self.fresh {
            self.fresh = false;
            c.next = at(self.phase);
        }
        self.phase = (self.phase
            + f64::from(self.shape.mod_rate.get()) * Self::CHUNK as f64 / self.rate.get())
            % 1.0;
        c.prev = c.next;
        c.next = at(self.phase);
        c.k = k;
    }

    /// Runs `input` into `output`, the same number of frames: the input channels averaged to
    /// one, the wet signal on the first two output channels. `glides` steps at each chunk's
    /// start, so a call that ends inside a chunk goes on in the next. A channel of `output`
    /// past the second is not touched; before the first [`aim`](Self::aim) the output is
    /// silent. No allocation.
    pub fn process<S: Float>(
        &mut self,
        glides: &mut PlateGlides,
        input: Planes<'_, S>,
        mut output: PlanesMut<'_, '_, S>,
    ) {
        let frames = input.frames().get().min(output.frames().get());
        let width = input.channels();
        let xs: [&[S]; MAX_CHANNELS] = core::array::from_fn(|c| {
            if c < width {
                &input.plane(c).samples()[..frames]
            } else {
                &[]
            }
        });
        let mut planes = output.iter_mut();
        let mut left = planes.next().map(|p| &mut p.into_samples_mut()[..frames]);
        let mut right = planes.next().map(|p| &mut p.into_samples_mut()[..frames]);

        if glides.decay.target() == 0.0 {
            for y in left.iter_mut().chain(right.iter_mut()) {
                y.fill(S::SILENCE);
            }
            return;
        }
        let scale = 1.0 / width.max(1) as f32;
        for i in 0..frames {
            if self.into == Samples::ZERO {
                glides.step();
                self.begin_chunk(glides);
            }
            let t = self.into.get() as f32 / Self::CHUNK as f32;
            let x: f32 = xs[..width]
                .iter()
                .map(|ch| ch[i].convert_to::<f32>())
                .sum::<f32>()
                * scale;
            let (l, r) = self.frame(x, t);
            if let Some(y) = left.as_deref_mut() {
                y[i] = l.convert_to::<S>();
            }
            if let Some(y) = right.as_deref_mut() {
                y[i] = r.convert_to::<S>();
            }
            self.into = (self.into + Samples(1)) % Samples(Self::CHUNK);
        }
    }

    /// One frame: `x` through the pre-delay, the bandwidth, the input diffusers and the tank;
    /// the left and right taps. `t` is how far into the chunk the frame is.
    fn frame(&mut self, x: f32, t: f32) -> (f32, f32) {
        let c = self.chunk;
        let k = c.k;
        self.pre.put(x);
        let delayed = self.pre.read(k.pre);
        self.pre.advance();
        self.bandwidth += (1.0 - k.bandwidth) * (delayed - self.bandwidth);
        let mut u = self.bandwidth;
        for ((line, &n), &g) in self.input.iter_mut().zip(&self.shape.input).zip(&k.input) {
            u = line.lattice(u, g, n.get() as f32);
            line.advance();
        }

        // Each half reads the other's last delay before either writes this frame.
        let ends = [0, 1].map(|h| {
            self.halves[h]
                .delay2
                .read(self.shape.halves[h][3].get() as f32)
        });
        for h in 0..2 {
            let [d1, l1, d2, _] = self.shape.halves[h].map(|n| n.get() as f32);
            let half = &mut self.halves[h];
            let input = u + k.decay * ends[1 - h];
            let sweep = crate::lerp(c.prev[h], c.next[h], t);
            let y = half
                .modulated
                .lattice(input, k.lattice1, (d1 + sweep).max(1.0));
            half.delay1.put(y);
            let q = half.delay1.read(l1);
            half.damped += (1.0 - k.damping) * (q - half.damped);
            let y = half.lattice2.lattice(k.decay * half.damped, k.lattice2, d2);
            half.delay2.put(y);
        }

        let [left, right] = self.taps.map(|side| {
            OUTPUT_GAIN.get()
                * side
                    .iter()
                    .map(|k| {
                        let half = &self.halves[k.half];
                        k.sign.get()
                            * match k.point {
                                Point::Delay1 => half.delay1.read(k.at),
                                Point::Lattice2 => half.lattice2.read(k.at),
                                Point::Delay2 => half.delay2.read(k.at),
                            }
                    })
                    .sum::<f32>()
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
    let floor = f64::from(TAIL_FLOOR.to_amplitude().get());
    if pole <= 0.0 {
        return Seconds::ZERO;
    }
    Seconds(libm::log(floor) / libm::log(pole) / rate.get())
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
fn target_of(network: &Network) -> Option<Coeffs> {
    let Stage::Fdn(f) = network.stages.iter().find(|s| matches!(s, Stage::Fdn(_)))? else {
        return None;
    };
    let (a, b) = (f.lines.first()?, f.lines.get(1)?);
    let mut input = [0.0f32; 4];
    for (slot, stage) in input.iter_mut().zip(network.stages.iter()) {
        if let Stage::AllPass(d) = stage {
            *slot = d[0].gain.get();
        }
    }
    let Absorption::OnePole { gain, damp } = a.absorption else {
        return None;
    };
    Some(Coeffs {
        decay: gain.get(),
        damping: damp,
        lattice2: b.diffuser?.gain.get(),
        lattice1: a.diffuser?.gain.get(),
        input,
        depth: a.diffuser?.modulation.depth.get() as f32,
        pre: network.pre_delay.get() as f32,
        bandwidth: 0.0,
    })
}

#[cfg(test)]
mod tests;
