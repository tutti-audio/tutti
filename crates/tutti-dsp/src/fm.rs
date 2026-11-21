//! Phase-modulation synthesis: operators that modulate one another's phase, run oversampled.
//!
//! An operator is a sine at a frequency whose phase a modulation `m`, in radians, moves:
//! `y = sin(2π φ + m)`, the form of Chowning's frequency modulation and of the DX7. A
//! modulator `j` of an operator `k` adds `I·level_j·y_j` to `m_k`, for the index of modulation
//! `I` ([`FmControls::index`]) and the modulator's `level` ([`OperatorSpec::level`]); an
//! operator that modulates itself adds its feedback `β·(y[n-1] + y[n-2])/2`, the average of
//! its last two outputs ([`FmControls::feedback`]), which damps the period-2 oscillation a
//! one-sample feedback falls into. The output is the sum of the carriers, each at
//! `level·y`, scaled by the voice's own level.
//!
//! A two-operator voice with a carrier at `fc` and a sine modulator at `fm` of unit level
//! has the line spectrum `Σ J_k(I) sin(2π (fc + k fm) t)` (Bessel functions of the first
//! kind), so the index is the modulation depth in radians and the sidebands' extent follows
//! it. The sidebands reach well past the carrier, and the voice runs at `factor` times the
//! rate through an [`Oversampler`], so what folds back at the base rate is what the
//! half-band filters leave of it.
//!
//! The [`Algorithm`] fixes which operator modulates which and which are carriers. Operators
//! are evaluated in index order and an operator is modulated only by those below it (or by
//! itself), so a loop between operators cannot be written.

use tutti_buffers::MaxBlock;
use tutti_buffers::{Float, MonoMut};
use tutti_units::{
    Amplitude, FrequencyRatio, Hz, ModulationIndex, PhaseIncrement, PhaseStep, RunningPhase,
    SampleRate, SampleSpan,
};

use crate::{FirError, Oversampler, Oversampling, OversamplingPlan};

/// The most operators an algorithm has.
pub const MAX_OPERATORS: usize = 6;

tutti_units::ranged! {
    /// How many operators an [`Algorithm`] has: `1..=6`.
    #[derive(Eq, Hash, PartialOrd, Ord)]
    OperatorCount(u8), OperatorCountError, "an operator count within 1..=6",
    |raw| raw >= 1 && raw as usize <= MAX_OPERATORS
}

tutti_units::ranged! {
    /// An operator's place in an [`Algorithm`]: `0..6`. Operators are evaluated in this order.
    #[derive(Eq, Hash, PartialOrd, Ord)]
    Operator(u8), OperatorError, "an operator index below 6",
    |raw| (raw as usize) < MAX_OPERATORS
}

/// One operator modulating another: the output of `modulator` moves the phase of `target`. A
/// route from an operator to itself is its feedback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Route {
    /// The operator whose output modulates.
    pub modulator: Operator,
    /// The operator whose phase it moves; not below the modulator.
    pub target: Operator,
}

/// Why an [`Algorithm`] was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AlgorithmError {
    /// A route or a carrier names an operator the algorithm does not have.
    #[error("operator {} is not one of the {} operators", .operator.get(), .count.get())]
    NoSuchOperator {
        /// The operator named.
        operator: Operator,
        /// How many operators the algorithm has.
        count: OperatorCount,
    },
    /// A route runs from an operator to one evaluated before it.
    #[error("operator {} cannot modulate operator {}, which is evaluated first", .route.modulator.get(), .route.target.get())]
    Backwards {
        /// The route.
        route: Route,
    },
    /// No operator is a carrier, so the voice would be silent.
    #[error("an algorithm needs a carrier")]
    NoCarrier,
}

/// Which operator modulates which, and which are carriers.
///
/// A modulator reaches only operators at its own index or above (the same index is feedback),
/// so evaluating the operators in index order evaluates every modulator before its target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Algorithm {
    count: OperatorCount,
    /// Bit `j` of entry `k` is set when operator `j` modulates operator `k`.
    modulated_by: [u8; MAX_OPERATORS],
    /// Bit `k` is set when operator `k` is a carrier.
    carriers: u8,
}

impl Algorithm {
    /// Operator 0 modulates 1, which modulates 2, and so on; the last is the only carrier.
    #[must_use]
    pub fn chain(count: OperatorCount) -> Self {
        let mut modulated_by = [0u8; MAX_OPERATORS];
        for (k, routed) in modulated_by
            .iter_mut()
            .enumerate()
            .take(usize::from(count.get()))
            .skip(1)
        {
            *routed = 1 << (k - 1);
        }
        Self {
            count,
            modulated_by,
            carriers: 1 << (count.get() - 1),
        }
    }

    /// No operator modulates another and every one is a carrier: an additive sum of sines.
    #[must_use]
    pub fn parallel(count: OperatorCount) -> Self {
        Self {
            count,
            modulated_by: [0; MAX_OPERATORS],
            carriers: ((1u16 << count.get()) - 1) as u8,
        }
    }

    /// How many operators it has.
    #[must_use]
    pub const fn count(&self) -> OperatorCount {
        self.count
    }

    /// Whether `modulator` modulates `target` (the same operator is its feedback).
    #[must_use]
    pub fn modulates(&self, modulator: Operator, target: Operator) -> bool {
        self.modulated_by[usize::from(target.get())] >> modulator.get() & 1 == 1
    }

    /// Whether `operator` is heard.
    #[must_use]
    pub fn is_carrier(&self, operator: Operator) -> bool {
        self.carriers >> operator.get() & 1 == 1
    }
}

/// An algorithm of `count` operators with `routes`, the operators in `carriers` heard.
///
/// # Errors
///
/// A route or carrier naming an operator at or past `count`, a route running to an operator
/// below its modulator, or no carrier.
impl TryFrom<(OperatorCount, &[Route], &[Operator])> for Algorithm {
    type Error = AlgorithmError;

    fn try_from(
        (count, routes, carriers): (OperatorCount, &[Route], &[Operator]),
    ) -> Result<Self, AlgorithmError> {
        let check = |operator: Operator| {
            if usize::from(operator.get()) < usize::from(count.get()) {
                Ok(())
            } else {
                Err(AlgorithmError::NoSuchOperator { operator, count })
            }
        };
        let mut modulated_by = [0u8; MAX_OPERATORS];
        for &route in routes {
            check(route.modulator)?;
            check(route.target)?;
            if route.modulator > route.target {
                return Err(AlgorithmError::Backwards { route });
            }
            modulated_by[usize::from(route.target.get())] |= 1 << route.modulator.get();
        }
        let mut heard = 0u8;
        for &carrier in carriers {
            check(carrier)?;
            heard |= 1 << carrier.get();
        }
        if heard == 0 {
            return Err(AlgorithmError::NoCarrier);
        }
        Ok(Self {
            count,
            modulated_by,
            carriers: heard,
        })
    }
}

/// How an operator's frequency follows the voice's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OperatorFrequency {
    /// A multiple of the voice's frequency.
    Ratio(FrequencyRatio),
    /// A frequency of its own, whatever the voice plays.
    Fixed(Hz),
}

/// One operator's frequency and level.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OperatorSpec {
    frequency: OperatorFrequency,
    level: Amplitude,
}

impl OperatorSpec {
    /// An operator at `frequency`, unity level.
    #[must_use]
    pub const fn new(frequency: OperatorFrequency) -> Self {
        Self {
            frequency,
            level: Amplitude::UNITY,
        }
    }

    /// The level: a carrier's gain in the output, and a modulator's share of the index.
    #[must_use]
    pub const fn level(mut self, level: Amplitude) -> Self {
        self.level = level;
        self
    }
}

/// An [`Algorithm`] and each of its operators' [`OperatorSpec`]: what a voice is made from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FmPatch {
    algorithm: Algorithm,
    operators: [OperatorSpec; MAX_OPERATORS],
}

impl FmPatch {
    /// A patch of `algorithm`, every operator at the voice's frequency (a ratio of 1) and unity
    /// level.
    #[must_use]
    pub const fn new(algorithm: Algorithm) -> Self {
        Self {
            algorithm,
            operators: [OperatorSpec::new(OperatorFrequency::Ratio(FrequencyRatio::UNISON));
                MAX_OPERATORS],
        }
    }

    /// The first operators' specs, in order: `operators[k]` is operator `k`'s. Specs past the
    /// algorithm's count are not used, and an operator without one keeps its default.
    #[must_use]
    pub fn operators(mut self, operators: &[OperatorSpec]) -> Self {
        let count = usize::from(self.algorithm.count.get());
        for (slot, spec) in self.operators[..count].iter_mut().zip(operators) {
            *slot = *spec;
        }
        self
    }

    /// A voice of this patch at `rate`, oversampled as `oversampling` says, for blocks of at
    /// most `max_block` frames, at the default controls with every phase at 0.
    ///
    /// # Errors
    ///
    /// When no half-band filter fits the rate ([`Oversampling::plan`]).
    pub fn voice<F: Float>(
        &self,
        oversampling: Oversampling,
        rate: SampleRate,
        max_block: MaxBlock,
    ) -> Result<FmVoice<F>, FirError> {
        FmVoice::new(*self, oversampling, rate, max_block)
    }

    /// The algorithm.
    #[must_use]
    pub const fn algorithm(&self) -> Algorithm {
        self.algorithm
    }
}

/// The live controls of an [`FmVoice`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FmControls {
    /// The voice's frequency, which an operator's [`Ratio`](OperatorFrequency::Ratio)
    /// multiplies. One that is not finite, or past the oversampled Nyquist, runs the phase
    /// at the Nyquist.
    pub frequency: Hz,
    /// The output's gain.
    pub level: Amplitude,
    /// The index of modulation `I`, in radians, of a modulator at unity level.
    pub index: ModulationIndex,
    /// The feedback `β`, in radians, of every operator the algorithm routes to itself.
    pub feedback: ModulationIndex,
}

impl FmControls {
    /// 440 Hz at unity level, with no modulation and no feedback.
    pub const DEFAULT: Self = Self {
        frequency: Hz::new(440.0),
        level: Amplitude::UNITY,
        index: ModulationIndex::ZERO,
        feedback: ModulationIndex::ZERO,
    };
}

/// The operators at the oversampled rate, one sample at a time.
#[derive(Clone, Debug)]
struct Stack<F: Float> {
    patch: FmPatch,
    /// The phase step of each operator at the oversampled rate.
    steps: [PhaseStep; MAX_OPERATORS],
    phases: [RunningPhase; MAX_OPERATORS],
    /// Each operator's last two outputs, newest first.
    history: [[F; 2]; MAX_OPERATORS],
    /// What an operator's output adds to the phase of those it modulates, in radians.
    swing: [F; MAX_OPERATORS],
    /// The gain of each carrier's output; 0 for a modulator.
    gain: [F; MAX_OPERATORS],
    feedback: F,
}

impl<F: Float> Stack<F> {
    fn new(patch: FmPatch) -> Self {
        Self {
            patch,
            steps: [PhaseStep::from_count(0); MAX_OPERATORS],
            phases: [RunningPhase::START; MAX_OPERATORS],
            history: [[F::ZERO; 2]; MAX_OPERATORS],
            swing: [F::ZERO; MAX_OPERATORS],
            gain: [F::ZERO; MAX_OPERATORS],
            feedback: F::ZERO,
        }
    }

    fn retarget(&mut self, controls: &FmControls, rate: SampleRate) {
        let algorithm = self.patch.algorithm;
        for k in 0..usize::from(algorithm.count.get()) {
            let spec = self.patch.operators[k];
            let frequency = match spec.frequency {
                OperatorFrequency::Ratio(ratio) => controls.frequency * ratio,
                OperatorFrequency::Fixed(hz) => hz,
            };
            let cycles = PhaseIncrement::new(
                (frequency.finite_or(Hz::ZERO).get() / rate.get()).clamp(0.0, 0.5),
            );
            self.steps[k] = PhaseStep::from(cycles);
            let level = F::from_unit(spec.level);
            self.swing[k] = level * F::from_unit(controls.index);
            let heard = algorithm.carriers >> k & 1 == 1;
            self.gain[k] = if heard {
                level * F::from_unit(controls.level)
            } else {
                F::ZERO
            };
        }
        self.feedback = F::from_unit(controls.feedback);
    }

    fn reset(&mut self) {
        self.phases = [RunningPhase::START; MAX_OPERATORS];
        self.history = [[F::ZERO; 2]; MAX_OPERATORS];
    }

    /// The next output sample.
    #[inline]
    fn step(&mut self) -> F {
        let algorithm = self.patch.algorithm;
        let half = F::lit(0.5);
        let mut out = F::ZERO;
        let mut y = [F::ZERO; MAX_OPERATORS];
        for k in 0..usize::from(algorithm.count.get()) {
            let routed = algorithm.modulated_by[k];
            let mut modulation = F::ZERO;
            for (j, &yj) in y.iter().enumerate().take(k) {
                if routed >> j & 1 == 1 {
                    modulation += self.swing[j] * yj;
                }
            }
            if routed >> k & 1 == 1 {
                let [y1, y2] = self.history[k];
                modulation += self.feedback * half * (y1 + y2);
            }
            let angle = F::TAU * F::from_phase(self.phases[k]) + modulation;
            y[k] = angle.sin();
            self.history[k] = [y[k], self.history[k][0]];
            self.phases[k] = self.phases[k].advance(self.steps[k]);
            out += self.gain[k] * y[k];
        }
        out
    }
}

/// A phase-modulation voice: an [`FmPatch`] run at `factor` times the rate through an
/// [`Oversampler`] and brought back to the base rate.
///
/// Built on the control side for one rate and block size; nothing it holds grows. The block
/// it renders is delayed by [`delay`](Self::delay) base frames, the half-band filters'
/// linear-phase delay.
#[derive(Clone, Debug)]
pub struct FmVoice<F: Float> {
    stack: Stack<F>,
    oversampler: Oversampler<F>,
    high_rate: SampleRate,
}

impl<F: Float> FmVoice<F> {
    fn new(
        patch: FmPatch,
        oversampling: Oversampling,
        rate: SampleRate,
        max_block: MaxBlock,
    ) -> Result<Self, FirError> {
        let plan: OversamplingPlan = oversampling.plan(rate)?;
        let mut voice = Self {
            stack: Stack::new(patch),
            high_rate: plan.high_rate(),
            oversampler: plan.state(max_block),
        };
        voice.retarget(&FmControls::DEFAULT);
        Ok(voice)
    }

    /// The delay of the output behind the operators' own phase, in base frames: the
    /// half-band filters' linear-phase delay ([`OversamplingPlan::down_delay`]).
    #[must_use]
    pub fn delay(&self) -> SampleSpan {
        self.oversampler.plan().down_delay()
    }

    /// Sets the controls the next block renders from. No allocation.
    pub fn retarget(&mut self, controls: &FmControls) {
        self.stack.retarget(controls, self.high_rate);
    }

    /// Back to the start: every phase at 0 and every history silent. No allocation.
    pub fn reset(&mut self) {
        self.stack.reset();
        self.oversampler.reset();
    }

    /// Renders `output.frames()` frames (at most the prepared block). No allocation.
    pub fn process(&mut self, mut output: MonoMut<'_, F>) {
        let mut high = self.oversampler.source(output.frames());
        for y in high.samples_mut() {
            *y = self.stack.step();
        }
        self.oversampler.downsample(output.reborrow());
    }
}

#[cfg(test)]
mod tests;
