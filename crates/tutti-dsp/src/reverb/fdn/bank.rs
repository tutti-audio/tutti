//! [`FdnBank`]: the feedback delay network over buffers views, with the parameters that
//! sound as [`Glide`]s ([`FdnGlides`]).
//!
//! `N` delay lines read with linear interpolation (so a length may move), a
//! three-band absorption in each loop, a mix `A = I - 2 m v v^T` (`m` is 1 for a
//! reflection and 0 for the identity of `Matrix::Diagonal`) and a stereo output.
//! The wet signal is the lines' own outputs (before their absorption), so
//! nothing sounds before the shortest line has gone round, and the loop gain is
//! exactly the network's.
//!
//! **Chunks.** The render counts [`CHUNK`](FdnBank::CHUNK) frames from the last reset, and
//! works once at each chunk's start: every glide takes a step, and the chunk's constants are
//! read from them (the line lengths, the absorption gains, the reflection vector, the
//! modulation's two ends the lengths are interpolated between). The chunks are counted from
//! the first frame since the last reset, not from a call's edge, so the output does not
//! depend on how the input is cut into calls. The *design* is not the bank's: the caller aims
//! the glides ([`aim`](FdnBank::aim)) when its controls move, on a grid of its own.
//!
//! **Retargeting ramps.** `aim` only sets targets. Every parameter that sounds (the loop
//! gains, the reflection vector (renormalised to its length every chunk) and its mix `m`,
//! the input and output gains, the wet gain, the width and the modulation depth) is a
//! [`Glide`] that moves to it in a fixed 20 ms of whole chunks, landing on it exactly; a
//! length moves at a bounded half frame a chunk (about 1.6% of a frame a frame, a pitch bend
//! of 27 cents), and the pre-delay at two.
//!
//! **Lines in flight.** A gain is designed for the length its line is moving to. While a line
//! is still `L'` frames long and its design is for `L`, each band's gain is raised to `L' / L`
//! at the chunk's start, so the decay stays the designed RT60 at whatever length the line has
//! (`begin_chunk` derives it). A landed line runs the designed gains unchanged.
//!
//! **The mix.** The identity of a diagonal network and the reflection of a
//! mixing one have determinants +1 and -1, so no path of orthogonal matrices
//! joins them, and a straight line `I - 2 m v v^T` between them has eigenvalue
//! `1 - 2m` along `v`, a contraction that would lose a frozen tail's energy.
//! The matrix is therefore never between the two: `m` is 0 or 1, and it
//! switches at the end of a ramp that leaves a mixing matrix (when `v` has
//! reached line 0, where `I - 2 v v^T` flips one line's sign) and at the start
//! of one that reaches it. Every step of a ramp is then orthogonal, `v` moves
//! between unit vectors (a diagonal network aims it at line 0), and a mixing
//! network is bit-identical to a recurrence without `m`. The switch is a step in
//! the sign of line 0's loop, as the model's own matrix steps between
//! `Diagonal` and the first `Reflection` near line 0.
//!
//! **The absorption** of a line is `y = g_high x + (g_mid - g_high) L2(x) +
//! (g_low - g_mid) L1(x)` with `L1` and `L2` two cascaded one-poles at the
//! network's two crossovers: `g_low` at DC, `g_high` far above the upper
//! crossover and `g_mid` between. It is linear in the gains, so a ramp of the
//! gains is a ramp of the filter; with every gain 1 it is the identity, and a
//! frozen loop loses nothing. A set of gains whose response would exceed 1
//! anywhere (a mid band that rings longer than both its neighbours, by a lot)
//! is scaled to peak at 0.9999 at most, so the loop cannot grow.
//!
//! **Format.** The state is `f32`; a view of another [`Float`] is converted at the edges.

use tutti_buffers::{Float, Planes, PlanesMut, Sample};
use tutti_types::{Hz, MAX_CHANNELS, SampleRate, Seconds, Tail};

use super::design::{CROSSOVER, FdnControls, FdnLayout};
use crate::delay::DelayRing;
use crate::onepole::cutoff_pole;
use crate::reverb::network::{Absorption, Band, Matrix, Network, Stage};
use crate::reverb::{CHUNK, ramp, tail_of};
use crate::{Glide, Ring, Steps, System, TAIL_FLOOR};

/// How far a line's length moves in a chunk, in frames.
const LENGTH_STEP: f32 = 0.5;
/// How far the pre-delay moves in a chunk, in frames.
const PRE_STEP: f32 = 2.0;

/// The coefficient point a design asks for, as [`FdnBank::aim`] writes it: the values
/// [`FdnGlides`] glide to, before any rate is applied. Sized for the most lines the bank was
/// built for; its fields from line `n` on are not read but for the input and output gains,
/// which a smaller network closes.
#[derive(Clone, Debug)]
struct Target {
    /// The lines in use.
    n: usize,
    rate: Hz,
    crossover: [f32; 2],
    pole: [f32; 2],

    len: Vec<f32>,
    pre_len: f32,
    depth: f32,
    gain: Vec<[f32; 3]>,
    v: Vec<f32>,
    v_len: f32,
    mix: f32,
    input: Vec<f32>,
    out: Vec<[f32; 2]>,
    wet: f32,
    width: f32,
}

impl Target {
    fn new(lines: usize) -> Self {
        Self {
            n: 0,
            rate: Hz(0.5),
            crossover: [0.0; 2],
            pole: [0.0; 2],
            len: vec![1.0; lines],
            pre_len: 0.0,
            depth: 0.0,
            gain: vec![[0.0; 3]; lines],
            v: vec![0.0; lines],
            v_len: 0.0,
            mix: 0.0,
            input: vec![0.0; lines],
            out: vec![[0.0; 2]; lines],
            wet: 0.0,
            width: 1.0,
        }
    }
}

/// What a [`FdnBank`] approaches: every sounding parameter a [`Glide`] (each at its own
/// rate: a length and the pre-delay by a distance, the rest by 20 ms), and the few that jump
/// (the line count, the modulation rate, the absorption's poles).
///
/// Aimed by [`FdnBank::aim`], stepped once a chunk by [`FdnBank::process`]. A glide steps
/// before its chunk runs: the first chunk after an aim reads step 1.
#[derive(Clone, Debug)]
pub struct FdnGlides {
    /// The steps of the 20 ms ramp of a gain, at the rate prepared for.
    ramp: Steps,
    /// The lines in use: 0 until the first aim, and the bank is silent.
    n: usize,
    rate: Hz,
    pole: [f32; 2],
    crossover: [f32; 2],

    len: Vec<Glide<f32>>,
    /// The length the gains are designed for, glided with them over the ramp: the gains in
    /// flight are a blend of two designs, each for its own length.
    design_len: Vec<Glide<f32>>,
    pre_len: Glide<f32>,
    depth: Glide<f32>,
    gain: Vec<[Glide<f32>; 3]>,
    v: Vec<Glide<f32>>,
    /// The length of `v` the network asks for (1 for every design here): the
    /// reflection is renormalised to it every chunk, so it is orthogonal at
    /// every step of a ramp and not only at its ends.
    v_len: Glide<f32>,
    /// `m` of `I - 2 m v v^T`: 0 for a diagonal network, 1 for a reflection. Only
    /// its target and whether it is moving are read (see the module docs): the
    /// bank's `m` is 1 while it ramps or aims at 1, else 0.
    mix: Glide<f32>,
    input: Vec<Glide<f32>>,
    out: Vec<[Glide<f32>; 2]>,
    wet: Glide<f32>,
    width: Glide<f32>,
}

impl FdnGlides {
    fn new(lines: usize, ramp: Steps, t: &Target) -> Self {
        let rest = Glide::landed(0.0);
        Self {
            ramp,
            n: 0,
            rate: Hz(0.5),
            pole: t.pole,
            crossover: t.crossover,
            len: vec![Glide::landed(1.0); lines],
            design_len: vec![Glide::landed(1.0); lines],
            pre_len: rest,
            depth: rest,
            gain: vec![[rest; 3]; lines],
            v: vec![rest; lines],
            v_len: rest,
            mix: rest,
            input: vec![rest; lines],
            out: vec![[rest; 2]; lines],
            wet: rest,
            width: Glide::landed(1.0),
        }
    }

    fn glides(&mut self) -> impl Iterator<Item = &mut Glide<f32>> {
        self.len
            .iter_mut()
            .chain(&mut self.design_len)
            .chain(self.gain.iter_mut().flatten())
            .chain(&mut self.v)
            .chain(&mut self.input)
            .chain(self.out.iter_mut().flatten())
            .chain([
                &mut self.pre_len,
                &mut self.depth,
                &mut self.v_len,
                &mut self.mix,
                &mut self.wet,
                &mut self.width,
            ])
    }

    /// Lands on the targets now.
    pub fn settle(&mut self) {
        self.glides().for_each(Glide::settle);
    }

    /// Takes one step of every glide.
    fn step(&mut self) {
        self.glides().for_each(|g| g.advance(1));
    }

    /// Aims every glide at the target's value with its own rate: a length by
    /// [`LENGTH_STEP`] a chunk, the pre-delay by [`PRE_STEP`], every other gain in the ramp.
    /// The lines from `n` on keep their gains and lengths; their input and output gains close.
    fn aim(&mut self, t: &Target) {
        let ramp = self.ramp;
        let n = t.n;
        self.n = n;
        self.crossover = t.crossover;
        self.pole = t.pole;
        for j in 0..n {
            self.len[j].aim_by(t.len[j], Steps::Rate(LENGTH_STEP));
            self.design_len[j].aim_by(t.len[j], ramp);
            for (s, &g) in self.gain[j].iter_mut().zip(&t.gain[j]) {
                s.aim_by(g, ramp);
            }
            self.v[j].aim_by(t.v[j], ramp);
        }
        for j in 0..self.input.len() {
            self.input[j].aim_by(t.input[j], ramp);
            for (s, &g) in self.out[j].iter_mut().zip(&t.out[j]) {
                s.aim_by(g, ramp);
            }
        }
        self.v_len.aim_by(t.v_len, ramp);
        self.mix.aim_by(t.mix, ramp);
        self.depth.aim_by(t.depth, ramp);
        self.rate = t.rate;
        self.pre_len.aim_by(t.pre_len, Steps::Rate(PRE_STEP));
        self.wet.aim_by(t.wet, ramp);
        self.width.aim_by(t.width, ramp);
    }
}

/// What a bank's chunks change: the lines, the pre-delay, the modulation phases, the
/// absorption's one-poles, and the constants of the chunk being run.
#[derive(Clone, Debug)]
struct State {
    lines: Vec<DelayRing>,
    pre: DelayRing,
    phase: Vec<f64>,
    /// One-pole states a line: `[L1 first, L1 second, L2 first, L2 second]`.
    pole: Vec<[f32; 4]>,
    /// Whether the modulation has not started: its first chunk reads the phase's
    /// own value as the previous one.
    fresh: bool,

    // The chunk's constants.
    c_len: Vec<f32>,
    c_mod_prev: Vec<f32>,
    c_mod_next: Vec<f32>,
    c_gain: Vec<[f32; 3]>,
    c_v: Vec<f32>,
    /// 0 or 1: the chunk's `m`.
    c_mix: f32,
    c_in: Vec<f32>,
    c_out: Vec<[f32; 2]>,
    c_wet: f32,
    c_width: f32,
    c_pre: f32,
}

/// A feedback delay network of any layout: its lines, its pre-delay and what a chunk
/// changes of them.
///
/// Built on the control side for one rate, where it allocates everything it will hold (the
/// lines at their longest, the scratch a design is made in); a clone keeps their size, and
/// [`reset`](Self::reset) zeroes them without allocating. Mono in (the input channels are
/// averaged), stereo out: the **wet** signal alone, even lines left and odd right.
#[derive(Clone, Debug)]
pub struct FdnBank {
    layout: FdnLayout,
    rate: SampleRate,
    max_lines: usize,
    cap: usize,
    pre_cap: usize,
    /// How long it rings at its worst.
    ceiling: Tail,
    /// Frames of the current chunk already run: 0 at a chunk's start.
    into: usize,
    state: State,
    /// The controls the design in `network` is of: a design is made once however many of
    /// [`aim`](Self::aim) and [`tail_of`](Self::tail_of) ask for it.
    designed: Option<FdnControls>,
    /// Where a design is made and the coefficient point it asks for, so the audio thread
    /// builds neither.
    network: Box<Network>,
    target: Target,
}

impl FdnBank {
    /// The frames between a glide's steps: the chunk. A caller that designs on a grid of
    /// its own can align it here, so a retarget lands on a chunk's start.
    pub const CHUNK: usize = CHUNK;

    /// A silent bank of `layout` at `rate`. Allocates: build it on the control side.
    #[must_use]
    pub fn new(layout: &FdnLayout, rate: SampleRate) -> Self {
        let bounds = layout.capacity(rate);
        let max_lines = bounds.lines;
        let cap = bounds.line.to_samples_ceil().get() + 4;
        let pre_cap = bounds.pre_delay.get() + 4;
        let mut target = Target::new(max_lines);
        set_poles(&mut target, CROSSOVER.map(|f| f.get()), rate);
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
                pole: vec![[0.0; 4]; max_lines],
                fresh: true,
                c_len: vec![1.0; max_lines],
                c_mod_prev: vec![0.0; max_lines],
                c_mod_next: vec![0.0; max_lines],
                c_gain: vec![[0.0; 3]; max_lines],
                c_v: vec![0.0; max_lines],
                c_mix: 0.0,
                c_in: vec![0.0; max_lines],
                c_out: vec![[0.0; 2]; max_lines],
                c_wet: 0.0,
                c_width: 0.0,
                c_pre: 0.0,
            },
            designed: None,
            network: Box::new(Network::new(rate)),
            target,
        };
        bank.reset();
        bank
    }

    /// The glides of this bank, at rest and aimed at nothing: its first [`aim`](Self::aim)
    /// is the caller's to land.
    #[must_use]
    pub fn glides(&self) -> FdnGlides {
        FdnGlides::new(self.max_lines, ramp(self.rate), &self.target)
    }

    /// How long it rings at its worst: the layout's longest decay, or unbounded for a layout
    /// that can freeze.
    #[must_use]
    pub fn tail(&self) -> Tail {
        self.ceiling
    }

    /// Designs `controls` into the scratch network, unless it already holds that design.
    fn design(&mut self, controls: &FdnControls) {
        if self.designed != Some(*controls) {
            self.layout
                .design_into(controls, self.rate, &mut self.network);
            self.designed = Some(*controls);
        }
    }

    /// How long `controls` ring from now, with `glides` as they are: the network's own tail
    /// at the floor plus the pre-delay still to fall, which is at most the ceiling
    /// ([`tail`](Self::tail)): the node clamps to it through its clock. No allocation; the
    /// audio state is not touched.
    ///
    /// The decay does not stretch while a line is in flight (the gains follow the length, see
    /// [`process`](Self::process)), so the design's tail is the tail. The pre-delay glide adds
    /// what it still has to fall: a pre-delay at `P'` frames with the target `P` delays the
    /// input into the loop by up to `P' - P` more, and `P' <= ` the longest pre-delay, which
    /// the ceiling's worst design already has.
    pub fn tail_of(&mut self, glides: &FdnGlides, controls: &FdnControls) -> Tail {
        self.design(controls);
        let Ring::Finite(t) = self.network.tail(TAIL_FLOOR) else {
            return tail_of(Ring::Unbounded, self.rate);
        };
        let pre = (self.network.pre_delay.get() as f32).clamp(0.0, (self.pre_cap - 4) as f32);
        let behind = f64::from((glides.pre_len.now() - pre).max(0.0));
        let seconds = t.get() + behind / self.rate.get();
        tail_of(Ring::Finite(Seconds(seconds)), self.rate)
    }

    /// Back to silence, the chunks counted from here. No allocation.
    pub fn reset(&mut self) {
        let s = &mut self.state;
        s.lines.iter_mut().for_each(DelayRing::clear);
        s.pre.clear();
        s.pole.fill([0.0; 4]);
        for (j, p) in s.phase.iter_mut().enumerate() {
            *p = std::f64::consts::TAU * j as f64 / self.max_lines.max(1) as f64;
        }
        s.fresh = true;
        self.into = 0;
    }

    /// Aims `glides` at the design of `controls`: the first recirculating stage's lines,
    /// matrix, outputs, pre-delay, wet gain and width, each as the value its glide moves to.
    /// No allocation.
    pub fn aim(&mut self, glides: &mut FdnGlides, controls: &FdnControls) {
        self.design(controls);
        if self.coefficients() {
            glides.aim(&self.target);
        }
    }

    /// The coefficient point the design in `network` asks for, written into `target`: `false`
    /// for a network with no recirculating stage or no line, and then the target is as it was.
    fn coefficients(&mut self) -> bool {
        let Some(Stage::Fdn(f)) = self
            .network
            .stages
            .iter()
            .find(|s| matches!(s, Stage::Fdn(_)))
        else {
            return false;
        };
        let n = f.lines.len().min(self.max_lines);
        if n == 0 {
            return false;
        }
        let t = &mut self.target;
        t.n = n;
        let max_len = (self.cap - 4) as f32;

        if let Some(Absorption::Bands { crossover, .. }) = f.lines.first().map(|l| l.absorption) {
            set_poles(t, [crossover[0].get(), crossover[1].get()], self.rate);
        }
        let flat = 1.0 / (n as f32).sqrt();
        for j in 0..n {
            let line = &f.lines[j];
            // The kernel's coefficient point is `f32`: a span narrows once, here.
            t.len[j] = (line.length.get() as f32).clamp(1.0, max_len);
            t.gain[j] = absorption_gains(line.absorption, t.pole);
            // A diagonal network is `m = 0`; its `v` is a unit vector (line 0)
            // like the rest, so a ramp to a mixing matrix moves between unit
            // vectors and `v_len` stays 1.
            let v = match &f.matrix {
                Matrix::Diagonal => f32::from(u8::from(j == 0)),
                Matrix::Householder => flat,
                Matrix::Reflection(v) if v.len() == n => v[j].get(),
                _ => 0.0,
            };
            t.v[j] = if v.is_finite() { v } else { 0.0 };
            let input = f.input.get(j).map_or(0.0, |g| g.get());
            t.input[j] = if input.is_finite() { input } else { 0.0 };
            let row = |r: usize| {
                let x = f
                    .output
                    .rows
                    .get(r)
                    .or_else(|| f.output.rows.first())
                    .and_then(|row| row.get(j))
                    .map_or(0.0, |g| g.get());
                if x.is_finite() { x } else { 0.0 }
            };
            t.out[j] = [row(0), row(1)];
        }
        t.v_len = t.v[..n].iter().map(|v| v * v).sum::<f32>().sqrt();
        t.mix = f32::from(u8::from(!matches!(f.matrix, Matrix::Diagonal)));
        for j in n..self.max_lines {
            t.input[j] = 0.0;
            t.out[j] = [0.0; 2];
        }
        let m = f.lines[0].modulation;
        t.depth = (m.depth.get() as f32).min((self.cap as f32) * 0.25);
        let hz = m.rate.get();
        t.rate = Hz(if hz.is_finite() {
            hz.clamp(0.0, 20.0)
        } else {
            0.5
        });
        let pre = self.network.pre_delay.get() as f32;
        t.pre_len = pre.clamp(0.0, (self.pre_cap - 4) as f32);
        let wet = self.network.wet.to_amplitude().get();
        t.wet = if wet.is_finite() { wet } else { 0.0 };
        let width = self.network.width.get();
        t.width = if width.is_finite() {
            width.clamp(0.0, 4.0)
        } else {
            1.0
        };
        true
    }

    /// The constants of the chunk that starts now, from the glides' step.
    fn begin_chunk(&mut self, c: &FdnGlides) {
        let s = &mut self.state;
        let n = c.n;
        let depth = f64::from(c.depth.now());
        if s.fresh {
            s.fresh = false;
            for j in 0..self.max_lines {
                s.c_mod_next[j] = (depth * libm::sin(s.phase[j])) as f32;
            }
        }
        let dphi =
            std::f64::consts::TAU * f64::from(c.rate.get()) * Self::CHUNK as f64 / self.rate.get();
        for j in 0..n {
            s.phase[j] = (s.phase[j] + dphi) % std::f64::consts::TAU;
            s.c_len[j] = c.len[j].now();
            s.c_mod_prev[j] = s.c_mod_next[j];
            s.c_mod_next[j] = (depth * libm::sin(s.phase[j])) as f32;
            // The gains are designed for a length `L`: a loop of `L` frames loses 60 dB in `T`
            // seconds at `g = 10^(-3 L / (fs T))` a trip. The same `T` at the `L'` frames the
            // line is now takes `g^(L' / L)`, since `(g^(L'/L))^(fs T / L') = g^(fs T / L)`: each
            // band's gain is raised to `L' / L` while the length is in flight, and a landed line
            // runs the designed gains as they are. `L` is the design length the gains glide
            // with (the gain in flight is a blend of two designs), so the ramp is continuous.
            let gains = c.gain[j].map(|g| g.now());
            let (now, designed) = (c.len[j].now(), c.design_len[j].now());
            let [low, mid, high] = if now == designed {
                gains
            } else {
                gains.map(|g| libm::powf(g, now / designed))
            };
            s.c_gain[j] = [high, mid - high, low - mid];
            s.c_v[j] = c.v[j].now();
            s.c_in[j] = c.input[j].now();
            s.c_out[j] = c.out[j].map(|g| g.now());
        }
        // A straight line between two unit vectors is shorter than either, and
        // `I - 2 v v^T` of a short `v` is a contraction: bring it back to length.
        let len: f32 = s.c_v[..n].iter().map(|x| x * x).sum::<f32>().sqrt();
        if len > 1.0e-6 {
            let k = c.v_len.now() / len;
            s.c_v[..n].iter_mut().for_each(|x| *x *= k);
        }
        s.c_mix = f32::from(u8::from(c.mix.target() > 0.5 || c.mix.moving()));
        s.c_wet = c.wet.now();
        s.c_width = c.width.now();
        s.c_pre = c.pre_len.now();
    }

    /// Runs `input` into `output`, the same number of frames: the input channels averaged to
    /// one, the wet signal on the first two output channels. `glides` steps at each chunk's
    /// start, so a call that ends inside a chunk goes on in the next. A channel of `output`
    /// past the second is not touched; before the first [`aim`](Self::aim) the output is
    /// silent. No allocation.
    #[expect(
        clippy::needless_range_loop,
        reason = "the per-line arrays (`c_len`, `c_out`, `pole`, `buf` rows) are indexed by one \
                  line number with the loop's scratch `d`; an iterator over any one of them \
                  would only hide the others"
    )]
    pub fn process<S: Float>(
        &mut self,
        glides: &mut FdnGlides,
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

        let n = glides.n;
        if n == 0 {
            for y in left.iter_mut().chain(right.iter_mut()) {
                y.fill(S::SILENCE);
            }
            return;
        }
        let scale = 1.0 / width.max(1) as f32;
        let p = glides.pole.map(|x| 1.0 - x);
        let mut d = [0.0f32; 16];
        for i in 0..frames {
            if self.into == 0 {
                glides.step();
                self.begin_chunk(glides);
            }
            let s = &mut self.state;
            let t = self.into as f32 / Self::CHUNK as f32;
            let x: f32 = xs[..width]
                .iter()
                .map(|ch| ch[i].convert_to::<f32>())
                .sum::<f32>()
                * scale;

            s.pre.put(x);
            let xd = s.pre.at(s.c_pre);
            s.pre.advance();

            let (mut out_l, mut out_r) = (0.0f32, 0.0f32);
            for j in 0..n {
                let delay =
                    (s.c_len[j] + crate::lerp(s.c_mod_prev[j], s.c_mod_next[j], t)).max(1.0);
                d[j] = s.lines[j].at(delay);
                out_l += s.c_out[j][0] * d[j];
                out_r += s.c_out[j][1] * d[j];
            }

            let mut sum = 0.0f32;
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
            for j in 0..n {
                let line = &mut s.lines[j];
                line.put(s.c_in[j] * xd + d[j] - 2.0 * s.c_v[j] * sum);
                line.advance();
            }

            let (mid, side) = ((out_l + out_r) * 0.5, (out_l - out_r) * 0.5);
            let wet = s.c_wet;
            if let Some(y) = left.as_deref_mut() {
                y[i] = ((mid + s.c_width * side) * wet).convert_to::<S>();
            }
            if let Some(y) = right.as_deref_mut() {
                y[i] = ((mid - s.c_width * side) * wet).convert_to::<S>();
            }
            self.into = (self.into + 1) % Self::CHUNK;
        }
    }

    /// The energy the lines hold: the last `length` samples of each, which is the whole
    /// state of a frozen network (the absorption's one-poles are not read while every gain is
    /// 1). For the tests.
    #[cfg(test)]
    pub(crate) fn state_energy(&self, glides: &FdnGlides) -> f64 {
        let s = &self.state;
        (0..glides.n)
            .map(|j| {
                let len = glides.len[j].now() as usize;
                (1..=len)
                    .map(|i| {
                        let x = s.lines[j].back(i);
                        f64::from(x) * f64::from(x)
                    })
                    .sum::<f64>()
            })
            .sum()
    }
}

/// Sets the one-poles of `t` to the crossovers, in Hz, at `rate`, when they moved.
fn set_poles(t: &mut Target, crossover: [f32; 2], rate: SampleRate) {
    if crossover == t.crossover {
        return;
    }
    t.crossover = crossover;
    for (p, f) in t.pole.iter_mut().zip(crossover) {
        *p = cutoff_pole(Hz(f), rate) as f32;
    }
}

/// The gains of a line's absorption, scaled if their response would peak above 1.
fn absorption_gains(a: Absorption, pole: [f32; 2]) -> [f32; 3] {
    let g = [Band::Low, Band::Mid, Band::High].map(|b| a.gain(b).get());
    rescale_bands(g, pole)
}

/// The gains `g = [low, mid, high]` of a three-band absorption whose one-poles are
/// `pole` (`exp(-2 pi fc / fs)` for the two crossovers), scaled to peak at 0.9999
/// if their response would exceed 1 anywhere; NaN, infinite or negative gains
/// are silence. The model's design applies it too, so the gains a network
/// carries are the ones the bank runs.
pub(crate) fn rescale_bands(g: [f32; 3], pole: [f32; 2]) -> [f32; 3] {
    if !g.iter().all(|x| x.is_finite() && *x >= 0.0) {
        return [0.0; 3];
    }
    let peak = peak_response(g, pole);
    if peak > 1.0 {
        g.map(|x| (f64::from(x) * 0.9999 / peak) as f32)
    } else {
        g
    }
}

/// The largest `|y(w)|` of the absorption with gains `g = [low, mid, high]`,
/// over a grid of frequencies.
pub(crate) fn peak_response(g: [f32; 3], pole: [f32; 2]) -> f64 {
    let (low, mid, high) = (f64::from(g[0]), f64::from(g[1]), f64::from(g[2]));
    let (c2, c1) = (mid - high, low - mid);
    // |L1|, |L2| <= 1, so the response is at most the sum of the magnitudes
    // of its three terms: when that is under 1 (every gain falling with
    // frequency, the usual case) there is nothing to measure.
    let bound = high.abs() + c2.abs() + c1.abs();
    if bound <= 1.0 {
        return bound;
    }
    let pole = pole.map(f64::from);
    // |L(w)|: ((1 - p) / (1 - p e^-jw))^2.
    let l = |p: f64, w: f64| -> (f64, f64) {
        let (re, im) = (1.0 - p * libm::cos(w), p * libm::sin(w));
        let den = re * re + im * im;
        let (hr, hi) = ((1.0 - p) * re / den, -(1.0 - p) * im / den);
        (hr * hr - hi * hi, 2.0 * hr * hi)
    };
    let mut peak = 0.0f64;
    for k in 0..=96 {
        let w = std::f64::consts::PI * (f64::from(k) / 96.0).powi(2);
        let (a, b) = l(pole[0], w);
        let (c, d) = l(pole[1], w);
        let (re, im) = (high + c2 * c + c1 * a, c2 * d + c1 * b);
        peak = peak.max(libm::hypot(re, im));
    }
    peak
}

#[cfg(test)]
mod tests;
