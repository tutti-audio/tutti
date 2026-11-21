//! [`PlateControls`]: what a plate reverberator is told, and the [`Network`] it makes.
//!
//! The plate is J. Dattorro's "simplified plate-class reverberation topology" (Effect
//! Design Part 1, JAES 45(9), 1997, figure 1 and tables 1 and 2). The input, the mean of
//! the channels, goes through a pre-delay, a one-pole low-pass (the *bandwidth*) and four
//! lattice all-passes (the input diffusers) into a *tank* of two halves wired in a figure
//! eight. A half is a modulated lattice (decay diffusion 1), a delay, a one-pole damping
//! low-pass, the *decay* gain, a lattice (decay diffusion 2), a delay and the decay gain
//! again; each half is fed the diffused input and the other half's output.
//!
//! **The network.** The tank is one [`Fdn`] of four lines in a ring, `A_L -> B_L -> A_R ->
//! B_R -> A_L` (a permutation matrix): line `A` is the modulated lattice, the first delay and
//! the damping with its decay gain, line `B` the second lattice, the second delay and the decay
//! gain. The four input diffusers are four [`NetworkStage::AllPass`] stages. The output taps (table 2,
//! read inside the tank) are the bank's alone: the network's output map is empty, so only its
//! `tail` and `rt60` queries are meaningful for the plate; its frequency-response and density
//! queries describe no output.
//!
//! **Decay.** The control is the RT60 of the tank's loop, mapped to the paper's decay
//! coefficient `c`. A trip round the tank passes the decay gain four times, so the loop gain
//! is `g = c^4`, and the loop's length is the sum `T` of the eight delays: the RT60 is
//! `-3 T / log10 g` ([`loop_gain`] is the inverse) and `c = g^(1/4)`. The rendered decay is
//! that figure **at diffusion 0, damping 0 and modulation 0**, where every lattice is a plain
//! delay and the loop is exactly `T` frames at the gain `g`. Each damping one-pole adds
//! `p / (1 - p)` frames of group delay at DC to its half, and a modulated delay moves around
//! its centre. At higher diffusion an all-pass's mean group delay is still its delay, but each
//! frequency decays at its own loop time: the decay is a mixture, faster early and slower
//! late, and a T30 fit reads it 8 to 10% longer at the paper's diffusion.
//!

use tutti_units::{Amplitude, Depth, Gain, Hz, Mix, SampleRate, SampleSpan, Seconds};

use crate::reverb::fdn::design::MAX_PRE_DELAY;
use crate::reverb::network::{
    Absorption, AllPass, AllPassForm, Fdn, Gains, Inline, Line, LineModulation, MAX_DENSE,
    MAX_LINES, MAX_STAGES, Matrix, Network, NetworkCapacity, NetworkOutputMap, NetworkStage,
    pre_delay_frames,
};
use crate::{MIN_DECAY, TAIL_FLOOR, loop_gain};

/// The sample rate the paper's lengths are at.
const PAPER_RATE: SampleRate = SampleRate::of(29761.0);

/// The input diffusers, in series: the delay in frames at 29761 Hz and the coefficient
/// (input diffusion 1, then 2).
pub(crate) const INPUT_DIFFUSERS: [(u32, Gain); 4] = [
    (142, Gain::new(0.75)),
    (107, Gain::new(0.75)),
    (379, Gain::new(0.625)),
    (277, Gain::new(0.625)),
];

/// One tank half's lengths at 29761 Hz.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Half {
    /// The modulated lattice (decay diffusion 1).
    pub(crate) modulated: u32,
    /// The delay after it.
    pub(crate) delay1: u32,
    /// The lattice after the damping (decay diffusion 2).
    pub(crate) lattice2: u32,
    /// The delay after it.
    pub(crate) delay2: u32,
}

/// The two tank halves, left then right.
pub(crate) const HALVES: [Half; 2] = [
    Half {
        modulated: 672,
        delay1: 4453,
        lattice2: 1800,
        delay2: 3720,
    },
    Half {
        modulated: 908,
        delay1: 4217,
        lattice2: 2656,
        delay2: 3163,
    },
];

/// Where inside a tank half an output tap reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Point {
    /// The first delay's line.
    Delay1,
    /// The second lattice's delay line.
    Lattice2,
    /// The second delay's line.
    Delay2,
}

/// One output tap: `sign` times the signal `offset` frames (at 29761 Hz) into `point` of
/// tank half `half`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OutTap {
    pub(crate) half: usize,
    pub(crate) point: Point,
    pub(crate) offset: u32,
    pub(crate) sign: Gain,
}

const fn tap(half: usize, point: Point, offset: u32, sign: Gain) -> OutTap {
    OutTap {
        half,
        point,
        offset,
        sign,
    }
}

/// The sign of an output tap that adds, and of one that subtracts.
const PLUS: Gain = Gain::new(1.0);
const MINUS: Gain = Gain::new(-1.0);

/// The left and right output taps (the paper's table 2).
pub(crate) const OUTPUT_TAPS: [[OutTap; 7]; 2] = [
    [
        tap(1, Point::Delay1, 266, PLUS),
        tap(1, Point::Delay1, 2974, PLUS),
        tap(1, Point::Lattice2, 1913, MINUS),
        tap(1, Point::Delay2, 1996, PLUS),
        tap(0, Point::Delay1, 1990, MINUS),
        tap(0, Point::Lattice2, 187, MINUS),
        tap(0, Point::Delay2, 1066, MINUS),
    ],
    [
        tap(0, Point::Delay1, 353, PLUS),
        tap(0, Point::Delay1, 3627, PLUS),
        tap(0, Point::Lattice2, 1228, MINUS),
        tap(0, Point::Delay2, 2673, PLUS),
        tap(1, Point::Delay1, 2111, MINUS),
        tap(1, Point::Lattice2, 335, MINUS),
        tap(1, Point::Delay2, 121, MINUS),
    ],
];

/// The gain of every output tap (the paper's 0.6).
pub(crate) const OUTPUT_GAIN: Gain = Gain::new(0.6);
/// Decay diffusion 1: negative, as the paper's figure notes.
pub(crate) const DECAY_DIFFUSION_1: Gain = Gain::new(-0.70);
/// The deepest modulation excursion, in frames at 29761 Hz (the paper's EXCURSION).
const EXCURSION: f64 = 16.0;
/// The rate of the modulation (the paper: of the order of 1 Hz).
const MODULATION_RATE: Hz = Hz::new(1.0);
/// The longest decay a control may ask for: 8 s.
pub const MAX_PLATE_DECAY: Seconds = Seconds::new(8.0);
/// The lowest bandwidth a control reaches.
pub(crate) const MIN_BANDWIDTH: Hz = Hz::new(10.0);
/// The share of Nyquist the input bandwidth is held under.
const BANDWIDTH_CEILING: f64 = 0.9;
/// The largest damping pole: one short of a filter that never settles.
const MAX_DAMPING: f64 = 0.999;

const _: () = assert!(MAX_STAGES >= 5 && MAX_LINES >= 4 && MAX_DENSE >= 16);

/// A paper length scaled to `rate`, `max(1, round(n * rate / 29761))` frames.
pub(crate) fn scaled(paper: u32, rate: SampleRate) -> f64 {
    libm::round(f64::from(paper) * rate.get() / PAPER_RATE.get()).max(1.0)
}

/// The loop length `T` of the tank at `rate`, in frames: the sum of its eight scaled
/// delays.
pub(crate) fn loop_frames(rate: SampleRate) -> f64 {
    HALVES
        .iter()
        .flat_map(|h| [h.modulated, h.delay1, h.lattice2, h.delay2])
        .map(|n| scaled(n, rate))
        .sum()
}

/// The controls of a plate reverberator, in tutti units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlateControls {
    /// The RT60 of the tank's loop, from [`MIN_DECAY`] to [`MAX_PLATE_DECAY`]. The rendered
    /// decay is exactly this only at `diffusion` 0 with no damping and no modulation (the
    /// module docs give the figure and why); with the paper's diffusion a T30 fit reads it
    /// 8 to 10% longer.
    pub decay: Seconds,
    /// The tank's damping: the pole of the one-pole low-pass in each half, `0` (none) to
    /// `0.999`. The paper's own coefficient.
    pub damping: Mix,
    /// The input bandwidth: the cutoff of the one-pole low-pass the input goes through, from
    /// 10 Hz up to 0.9 of Nyquist.
    pub bandwidth: Hz,
    /// The delay before the first sound.
    pub pre_delay: Seconds,
    /// The depth of the modulation of the two decay-diffusion-1 delays, `0..1`: the paper's
    /// excursion of 16 frames at 29761 Hz (scaled to the rate) at 1.
    pub modulation: Depth,
    /// How diffuse the lattices are, `0..1`: every lattice coefficient is the paper's times
    /// this (decay diffusion 2 after its `c + 0.15` rule), so `1` is table 1's and `0` makes
    /// every lattice a plain delay.
    pub diffusion: Mix,
}

impl PlateControls {
    /// The paper's table 1 where it names a figure: the RT60 of its decay of 0.5 (1.8 s),
    /// damping 0.0005, a bandwidth of 20 kHz (the paper's 0.9995 is nearly transparent), no
    /// pre-delay and half the modulation depth.
    pub const DEFAULT: PlateControls = PlateControls {
        decay: Seconds::new(1.8),
        damping: Mix::new(0.0005),
        bandwidth: Hz::new(20_000.0),
        pre_delay: Seconds::ZERO,
        modulation: Depth::of(0.5),
        diffusion: Mix::new(1.0),
    };
}

impl Default for PlateControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl PlateControls {
    /// The bandwidth in force at `rate`: finite, within 10 Hz and 0.9 of Nyquist.
    pub(crate) fn bandwidth_at(&self, rate: SampleRate) -> Hz {
        self.bandwidth
            .finite_or(Self::DEFAULT.bandwidth)
            .clamp(MIN_BANDWIDTH, rate.nyquist_scaled(BANDWIDTH_CEILING))
    }

    /// Designs the controls into `out` in place, at `rate`: the network the bank runs (the
    /// input bandwidth, which a network does not describe, is the bank's). A control outside
    /// its range is clamped and a non-finite one takes its [`DEFAULT`](Self::DEFAULT) value.
    /// A network is about 111 KB, so it is built where it lives; no allocation.
    ///
    /// # Panics
    ///
    /// Never: what it writes fits the network's capacities (asserted at compile time).
    pub fn design_into(&self, rate: SampleRate, out: &mut Network) {
        let d = Self::DEFAULT;
        let rt60 = if self.decay.get().is_finite() {
            self.decay
        } else {
            d.decay
        }
        .clamp(MIN_DECAY, MAX_PLATE_DECAY);
        let damping = self
            .damping
            .finite_or(d.damping)
            .get()
            .clamp(0.0, MAX_DAMPING);
        let pre = if self.pre_delay.get().is_finite() {
            self.pre_delay
        } else {
            Seconds::ZERO
        }
        .clamp(Seconds::ZERO, MAX_PRE_DELAY);
        let depth = self.modulation.get().abs().min(1.0);
        // `diffusion` is an `f32` unit: it widens once into the `f64` design.
        let diffusion = self.diffusion.finite_or(d.diffusion).get().clamp(0.0, 1.0);

        let total = SampleSpan::clamped(loop_frames(rate));
        let loop_gain = loop_gain(total, rt60, rate).get();
        let c = libm::pow(loop_gain, 0.25);
        let lattice2 = Gain::new(diffusion * (c + 0.15).clamp(0.25, 0.5));
        let scale = |g: Gain| Gain::new(diffusion * g.get());
        let excursion = SampleSpan::clamped(EXCURSION * depth * rate.get() / PAPER_RATE.get());

        let span = |n: u32| SampleSpan::clamped(scaled(n, rate));
        let lattice = |delay: SampleSpan, gain: Gain, modulation: LineModulation| AllPass {
            delay,
            gain,
            modulation,
            form: AllPassForm::Lattice,
        };

        out.clear(rate);
        out.pre_delay = pre_delay_frames(pre, rate);
        for (n, g) in INPUT_DIFFUSERS {
            let stage = Inline::from_slice(&[lattice(span(n), scale(g), LineModulation::NONE)])
                .expect("one all-pass fits a stage");
            out.stages
                .push(NetworkStage::AllPass(stage))
                .expect("five stages fit MAX_STAGES");
        }

        let mut lines: Inline<Line, MAX_LINES> = Inline::new();
        for h in HALVES {
            lines
                .push(Line {
                    length: span(h.delay1),
                    modulation: LineModulation::NONE,
                    absorption: Absorption::OnePole {
                        gain: Amplitude::new(c),
                        damp: damping,
                    },
                    diffuser: Some(lattice(
                        span(h.modulated),
                        scale(DECAY_DIFFUSION_1),
                        LineModulation {
                            depth: excursion,
                            rate: MODULATION_RATE,
                        },
                    )),
                })
                .expect("four lines fit MAX_LINES");
            lines
                .push(Line {
                    length: span(h.delay2),
                    modulation: LineModulation::NONE,
                    absorption: Absorption::Gain(Amplitude::new(c)),
                    diffuser: Some(lattice(span(h.lattice2), lattice2, LineModulation::NONE)),
                })
                .expect("four lines fit MAX_LINES");
        }

        // Line `i` is fed by line `i - 1`, round the ring.
        let mut ring = Inline::filled(Gain::SILENT, 16).expect("a 4 by 4 matrix fits MAX_DENSE");
        for i in 0..4 {
            ring.as_mut_slice()[i * 4 + (i + 3) % 4] = Gain::new(1.0);
        }
        // The diffused input goes to the first line of each half.
        let mut input = Gains::filled(Gain::SILENT, 4).expect("four gains fit MAX_LINES");
        input.as_mut_slice()[0] = Gain::new(1.0);
        input.as_mut_slice()[2] = Gain::new(1.0);
        out.stages
            .push(NetworkStage::Fdn(Fdn {
                lines,
                matrix: Matrix::Dense(ring),
                input,
                output: NetworkOutputMap::default(),
                loop_delay: SampleSpan::ZERO,
            }))
            .expect("five stages fit MAX_STAGES");
    }

    /// The worst case over every control: the longest decay, the longest pre-delay, the
    /// deepest modulation, the most damping and the paper's diffusion (the most group
    /// delay). Control side.
    pub(crate) fn capacity(rate: SampleRate) -> NetworkCapacity {
        let worst = PlateControls {
            decay: MAX_PLATE_DECAY,
            pre_delay: MAX_PRE_DELAY,
            modulation: Depth::of(1.0),
            damping: Mix::new(MAX_DAMPING),
            ..Self::DEFAULT
        };
        let mut network = Box::new(Network::new(rate));
        worst.design_into(rate, &mut network);
        NetworkCapacity::need(&network, TAIL_FLOOR)
    }
}

#[cfg(test)]
mod tests;
