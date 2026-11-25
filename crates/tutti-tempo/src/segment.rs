//! One segment's closed forms. Rates are beats per second (`bpm / 60`).
//!
//! * Step: `beats = r0 * s`.
//! * Ramp, tempo linear in time: `r(s) = r0 + a s`, `beats = r0 s + a s^2 / 2`.
//!   Over `db` beats from `r0` to `r1` it lasts `2 db / (r0 + r1)` seconds.
//!   Polynomial one way, one `sqrt` (correctly rounded) the other.
//! * Ramp, tempo linear in beats: `r(b) = r0 + g b`, `s = ln(1 + g b / r0) / g`,
//!   `beats = r0 (e^{g s} - 1) / g`.
//!
//! A negative offset is before the segment: only the first segment is ever
//! asked, and it extrapolates at its own tempo.
//!
//! Positions and spans are units (`Beat`, `BeatDuration`, `Seconds`); each
//! method takes its offset as one and reads the raw `f64` once at its edge,
//! because the closed forms mix beats, seconds and rates in one expression.
//! Two quantities stay bare `f64` because no unit covers them: a rate in
//! beats per second (`r0`, `r1`; a `Bpm` over 60, kept apart so the forms
//! read as written above) and a ramp's coefficient (`a`: beats per second
//! per second for a time ramp; `g`: beats per second per beat for a beat
//! ramp), which lives in its [`Law`].

use crate::point::Transition;
use tutti_units::{Beat, BeatDuration, Bpm, Seconds};

/// How the tempo moves across a segment, with the coefficient of its form.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Law {
    /// Constant tempo.
    Step,
    /// Tempo linear in time: `r(s) = r0 + a s`.
    Time { a: f64 },
    /// Tempo linear in beats: `r(b) = r0 + g b`.
    Beats { g: f64 },
}

impl Law {
    /// Whether the coefficient is finite (a step has none).
    fn is_finite(self) -> bool {
        match self {
            Self::Step => true,
            Self::Time { a } => a.is_finite(),
            Self::Beats { g } => g.is_finite(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Seg {
    /// Where the segment starts.
    pub beat0: Beat,
    /// Time from beat 0 to `beat0`.
    pub sec0: Seconds,
    /// Tempo at `beat0`, as given.
    pub bpm0: Bpm,
    /// `bpm0 / 60`, in beats per second.
    pub r0: f64,
    pub law: Law,
    /// How long the segment lasts (`INFINITY` for the last).
    pub dsec: Seconds,
}

impl Seg {
    /// The segment from `beat0`, spanning `dbeats` to a next tempo of `r1`
    /// (`INFINITY` beats for the last segment).
    pub(crate) fn new(
        beat0: Beat,
        sec0: Seconds,
        bpm0: Bpm,
        r1: f64,
        transition: Transition,
        dbeats: BeatDuration,
    ) -> Self {
        let dbeats = dbeats.get();
        let r0 = bpm0.get() / 60.0;
        let transition = if r0 == r1 {
            Transition::Step
        } else {
            transition
        };
        let (law, dsec) = match transition {
            Transition::Step => (Law::Step, dbeats / r0),
            Transition::RampTime { .. } => {
                let t = 2.0 * dbeats / (r0 + r1);
                (Law::Time { a: (r1 - r0) / t }, t)
            }
            Transition::RampBeats { .. } => {
                let g = (r1 - r0) / dbeats;
                (Law::Beats { g }, ln_ratio(g, dbeats, r0))
            }
        };
        Self {
            beat0,
            sec0,
            bpm0,
            r0,
            law,
            dsec: Seconds::new(dsec),
        }
    }

    /// Whether the closed forms agree with themselves over the whole segment:
    /// the duration is finite and positive and the forward and inverse maps
    /// meet at the far end. An overflowing square or quotient fails it.
    pub(crate) fn is_sound(&self, dbeats: BeatDuration) -> bool {
        let back = self.sec_in(dbeats).get();
        let dsec = self.dsec.get();
        dsec.is_finite() && dsec > 0.0 && self.law.is_finite() && (back - dsec).abs() <= 1e-9 * dsec
    }

    /// Time into the segment at `db` beats in.
    pub(crate) fn sec_in(&self, db: BeatDuration) -> Seconds {
        Seconds::new(self.raw_sec_in(db.get()))
    }

    fn raw_sec_in(&self, db: f64) -> f64 {
        if db < 0.0 {
            return db / self.r0;
        }
        match self.law {
            Law::Step => db / self.r0,
            Law::Time { a } => 2.0 * db / (self.r0 + libm::sqrt(self.r0 * self.r0 + 2.0 * a * db)),
            Law::Beats { g } => ln_ratio(g, db, self.r0),
        }
    }

    /// Beats into the segment at `ds` in.
    pub(crate) fn beat_in(&self, ds: Seconds) -> BeatDuration {
        BeatDuration::new(self.raw_beat_in(ds.get()))
    }

    fn raw_beat_in(&self, ds: f64) -> f64 {
        if ds < 0.0 {
            return self.r0 * ds;
        }
        match self.law {
            Law::Step => self.r0 * ds,
            Law::Time { a } => ds * (self.r0 + 0.5 * a * ds),
            Law::Beats { g } => {
                let y = g * ds;
                if y == 0.0 {
                    self.r0 * ds
                } else {
                    self.r0 * ds * (libm::expm1(y) / y)
                }
            }
        }
    }

    /// Tempo `ds` in. Written as the given tempo plus a change, so offset 0 is
    /// that tempo to the bit.
    pub(crate) fn bpm_at_sec(&self, ds: Seconds) -> Bpm {
        let ds = ds.get();
        if ds < 0.0 {
            return self.bpm0;
        }
        let bpm0 = self.bpm0.get();
        self.in_range(match self.law {
            Law::Step => bpm0,
            Law::Time { a } => bpm0 + 60.0 * a * ds,
            Law::Beats { g } => bpm0 * libm::exp(g * ds),
        })
    }

    /// Tempo in BPM `db` beats in. As [`bpm_at_sec`](Self::bpm_at_sec); the
    /// time ramp's `sqrt(r0^2 + 2 a db) - r0` is `2 a db / (sqrt(..) + r0)`,
    /// which does not cancel.
    pub(crate) fn bpm_at_beat(&self, db: BeatDuration) -> Bpm {
        let db = db.get();
        if db < 0.0 {
            return self.bpm0;
        }
        let bpm0 = self.bpm0.get();
        self.in_range(match self.law {
            Law::Step => bpm0,
            Law::Time { a } => {
                let root = libm::sqrt(self.r0 * self.r0 + 2.0 * a * db);
                bpm0 + 60.0 * (2.0 * a * db) / (root + self.r0)
            }
            Law::Beats { g } => bpm0 + 60.0 * g * db,
        })
    }

    /// `raw` as a tempo; the segment's own where the closed form leaves the
    /// range (a ramp read far past its end).
    fn in_range(&self, raw: f64) -> Bpm {
        Bpm::new(raw).unwrap_or(self.bpm0)
    }
}

/// `ln(1 + g db / r0) / g`, and `db / r0` as `g -> 0`.
fn ln_ratio(g: f64, db: f64, r0: f64) -> f64 {
    let x = g * db / r0;
    if x == 0.0 {
        db / r0
    } else {
        (db / r0) * (libm::log1p(x) / x)
    }
}
