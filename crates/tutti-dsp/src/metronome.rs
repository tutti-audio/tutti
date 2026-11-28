//! [`Metronome`]: the two clicks of a metronome and the notated beat that triggers them.
//!
//! A click is a windowed sine, rendered once at the prepared rate and replayed from its
//! start at every notated beat; the downbeat of a bar sounds the accent, a higher and louder
//! one. The beat is the playhead's, read once per frame through [`tick`](Metronome::tick),
//! under the project's [`MeterMap`] (`None`: 4/4 from beat 0).

use tutti_buffers::Float;
use tutti_music::{Meter, MeterMap, TimeSignature};
use tutti_units::{
    Amplitude, Beat, Hz, Phase, PhaseIncrement, PhaseStep, RunningPhase, SampleRate, Samples,
    Seconds,
};

/// How far two beat onsets must differ to count as different beats.
///
/// The playhead is an accumulated `f64`, so an exact compare would retrigger on drift alone.
/// A thousandth of a quarter note is far below the shortest notated beat any legal meter can
/// express (a 64th note is 0.0625 quarters) and far above the drift of a realistic session.
const ONSET_EPSILON: f64 = 1e-3;

/// A metronome's clicks and the beat latch that triggers them.
///
/// Built unprepared: it sounds nothing until [`prepare`](Self::prepare) renders the clicks.
/// Beats are identified by the *onset* of the notated beat they fall in, so the latch needs
/// no global numbering and stays distinct across a meter change. The index counts *notated*
/// beats, not quarter notes: in 7/8 that is an eighth, so the metronome clicks seven times
/// per bar. The accent is the bar's downbeat straight from the meter, never an every-N count,
/// which drifts against the bar the moment the time signature is not 4/4.
#[derive(Clone)]
pub struct Metronome<F: Float> {
    /// The rate the clicks were rendered at; `None` until prepared.
    rate: Option<SampleRate>,
    /// The two clicks, designed in `f64` and crossed once per sample into `F` when prepared.
    normal: Vec<F>,
    accent: Vec<F>,
    /// The frame of the sounding click the next sample reads.
    position: usize,
    is_accent: bool,
    /// The onset of the notated beat currently sounding, or `None` when nothing has been
    /// clicked yet. `Option`, since every finite beat, negative pre-roll included, is a
    /// legitimate onset.
    onset: Option<Beat>,
    /// Where the notated beat latched in `onset` ends: the next onset, or an earlier meter
    /// change. While the playhead stays in `[onset, latched_until)` nothing can retrigger, so
    /// the per-frame check is two compares instead of a `bar_at`. A cache of the meter, so
    /// [`begin_block`](Self::begin_block) drops it. Never read while `onset` is `None`.
    latched_until: Beat,
}

impl<F: Float> core::fmt::Debug for Metronome<F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Metronome")
            .field("rate", &self.rate)
            .field("position", &self.position)
            .field("is_accent", &self.is_accent)
            .field("onset", &self.onset)
            .field("latched_until", &self.latched_until)
            .finish_non_exhaustive()
    }
}

impl<F: Float> Default for Metronome<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: Float> Metronome<F> {
    /// A silent metronome with no clicks rendered.
    #[must_use]
    pub fn new() -> Self {
        Self {
            rate: None,
            normal: Vec::new(),
            accent: Vec::new(),
            position: 0,
            is_accent: false,
            onset: None,
            latched_until: Beat::new(f64::NEG_INFINITY),
        }
    }

    /// Renders the two clicks at `rate` unless they already are (within 0.1 Hz), and
    /// silences it when it does. Allocates: control side.
    pub fn prepare(&mut self, rate: SampleRate) {
        if self
            .rate
            .is_some_and(|had| (had.get() - rate.get()).abs() <= 0.1)
        {
            return;
        }
        self.rate = Some(rate);
        self.normal = click(rate, false);
        self.accent = click(rate, true);
        self.silence();
    }

    /// The rendered click: the accent's when `accent`, else the normal one. Empty before
    /// [`prepare`](Self::prepare). Its first frame is exactly zero (`sin(0)` under a zero
    /// attack), so a click sounds one frame after its onset.
    #[must_use]
    pub fn click(&self, accent: bool) -> &[F] {
        if accent { &self.accent } else { &self.normal }
    }

    /// The onset of the notated beat latched, or `None` before the first.
    #[must_use]
    pub fn onset(&self) -> Option<Beat> {
        self.onset
    }

    /// Whether the click latched is the accent.
    #[must_use]
    pub fn is_accent(&self) -> bool {
        self.is_accent
    }

    /// Silences it and forgets the last beat, so resuming retriggers.
    pub fn silence(&mut self) {
        self.position = 0;
        self.is_accent = false;
        self.onset = None;
    }

    /// Drops the cached span of the latched beat, so the next frame consults the meter.
    ///
    /// Called at the top of every block, which is all it takes for a republished meter to
    /// reach it: the meter is lent per block, and this is the only thing derived from it that
    /// outlives one.
    pub fn begin_block(&mut self) {
        self.latched_until = Beat::new(f64::NEG_INFINITY);
    }

    /// The next sample at `volume`, with the playhead at `beat` under `meter`: retriggers the
    /// click when `beat` has crossed into a new notated beat, then reads one sample of the
    /// click sounding, or silence once it has run out. Once per frame, so the common case,
    /// still inside the latched beat, is two compares; `bar_at` runs about once per notated
    /// beat.
    #[inline]
    pub fn tick(&mut self, meter: Option<&MeterMap>, beat: Beat, volume: F) -> F {
        self.latch(meter, beat);
        let click = if self.is_accent {
            &self.accent
        } else {
            &self.normal
        };
        if let Some(&sample) = click.get(self.position) {
            self.position += 1;
            sample * volume
        } else {
            F::ZERO
        }
    }

    /// Retriggers if `beat` has crossed into a new notated beat under `meter`.
    #[inline]
    fn latch(&mut self, meter: Option<&MeterMap>, beat: Beat) {
        if let Some(onset) = self.onset
            && beat >= onset
            && beat < self.latched_until
        {
            return;
        }

        let position = match meter {
            Some(map) => map.bar_at(beat),
            None => TimeSignature::default().bar_at(beat),
        };

        let onset = position.beat_onset();

        // Inequality, not `>`: a backward jump from a loop wrap must retrigger too. The
        // epsilon is for float drift in the accumulated playhead.
        let changed = match self.onset {
            Some(previous) => (onset - previous).get().abs() > ONSET_EPSILON,
            None => true,
        };

        if changed {
            self.onset = Some(onset);
            self.position = 0;
            self.is_accent = position.is_downbeat();
        }

        // The latched beat ends at the next notated onset, or sooner at a meter change, which
        // may fall mid-beat (the bar before a change is simply short). Measured from the onset
        // actually latched, so a within-epsilon non-change keeps the span it already had.
        let latched = self.onset.unwrap_or(onset);
        let next_onset = latched + position.beat_length();
        let next_change = meter.and_then(|m| m.change_after(latched));
        self.latched_until = match next_change {
            Some(change) if change < next_onset => change,
            _ => next_onset,
        };
    }
}

/// One rendered click: a windowed sine, 1200 Hz at full level for the accent and 1000 Hz at
/// 0.7 for the normal one, 30 ms long. The envelope rises over 1 ms, holds until 20 ms and
/// falls to zero at the end.
fn click<F: Float>(rate: SampleRate, is_accent: bool) -> Vec<F> {
    /// Total click length.
    const CLICK_LEN: Seconds = Seconds::new(0.03);
    /// Fade-in, then full level until `HOLD_END`, then fade to zero.
    const ATTACK: Seconds = Seconds::new(0.001);
    const HOLD_END: Seconds = Seconds::new(0.02);

    // `to_samples_ceil`, not a truncating cast: this sizes a buffer.
    let len = CLICK_LEN.to_samples_ceil(rate).get();

    let freq = if is_accent {
        Hz::new(1200.0)
    } else {
        Hz::new(1000.0)
    };
    let level = if is_accent {
        Amplitude::new(1.0)
    } else {
        Amplitude::new(0.7)
    };
    let (attack, hold_end) = (ATTACK.get(), HOLD_END.get());
    let release_len = CLICK_LEN.get() - hold_end;
    let step = PhaseStep::from(PhaseIncrement::per_sample(freq, rate));

    let mut phase = RunningPhase::START;
    (0..len)
        .map(|i| {
            let t = Samples::new(i).to_seconds(rate).get();
            let env = if t < attack {
                t / attack
            } else if t < hold_end {
                1.0
            } else {
                1.0 - (t - hold_end) / release_len
            };
            let sample = Phase::new(f64::from_phase(phase)).sin() * env * level.get();
            phase = phase.advance(step);
            F::from_design(sample)
        })
        .collect()
}

#[cfg(test)]
mod tests;
