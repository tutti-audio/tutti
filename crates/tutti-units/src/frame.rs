//! [`Frame`] — an absolute position on the engine's frame clock — and [`At`],
//! the "when" every scheduled command has to state.
//!
//! # Why a position type beside `Samples`
//!
//! [`Samples`] is a **count**: a latency, a ring length, a block length. A
//! frame on the timeline is a **position**, and the two have different
//! algebras, exactly as `Beat` and `BeatDuration` do: a position plus a count
//! is a position, the distance between two positions is a count, and two
//! positions do not add. Carrying both as bare integers is how an
//! off-by-a-block bug gets written — a block-relative offset handed to code
//! that wanted an absolute frame, or the reverse — and nothing notices until
//! a note lands one block late.
//!
//! `u64` rather than `usize`: `Samples` is `usize`, which on a 32-bit target
//! wraps after about a day at 48 kHz. An engine left running is a position
//! that has to survive that.
//!
//! The block-relative half of the split (an *offset* into the current block,
//! valid only inside it) belongs to whatever owns the block; the only
//! conversion between the two goes through that block's environment.
//!
//! # Algebra
//!
//! - `Frame + Samples → Frame` ([`Add`]): advance a position.
//!   Saturating — the counter cannot overflow in practice, and wrapping a
//!   timeline position to zero would replay the session from the top.
//! - The distance between two frames is [`since`](Frame::since), a *checked*
//!   subtraction returning `Option<Samples>`: "how long ago" is only a count
//!   when it is not negative, and the caller has to say what a negative
//!   answer means.
//!
//! Omitted on purpose (the ledger is in the tests, and enforced by the
//! `compile_fail` examples on [`Frame`]):
//!
//! - `Frame + Frame` — two positions do not add.
//! - `Frame - Frame` — see [`since`](Frame::since).
//! - `Frame + u64` / `Frame + usize` — a bare operand could be a count, an
//!   offset or a channel; make the caller say `Samples::new(n)`.
//! - `Frame * k` — scaling a position depends on where zero is.

use core::ops::{Add, AddAssign};

use super::samples::Samples;
use super::units::Beat;

unit! {
    /// An absolute frame position on the engine's clock: frames rendered since it
    /// started. See the module docs.
    ///
    /// Two positions do not add, and do not subtract with `-`:
    ///
    /// ```compile_fail
    /// use tutti_units::Frame;
    /// let _ = Frame::new(10) + Frame::new(20);
    /// ```
    ///
    /// ```compile_fail
    /// use tutti_units::Frame;
    /// let _ = Frame::new(20) - Frame::new(10);
    /// ```
    ///
    /// A bare integer is not a frame count either:
    ///
    /// ```compile_fail
    /// use tutti_units::Frame;
    /// let _ = Frame::new(10) + 5u64;
    /// ```
    ///
    /// What does compile says what it means:
    ///
    /// ```
    /// use tutti_units::{Frame, Samples};
    /// let start = Frame::new(48_000);
    /// let later = start + Samples::new(512);
    /// assert_eq!(later.since(start), Some(Samples::new(512)));
    /// assert_eq!(start.since(later), None); // not a count: it is in the past
    /// ```
    pub struct Frame(u64): Display;
}

impl Frame {
    /// The first frame the clock renders.
    pub const ZERO: Frame = Frame::new(0);

    /// Wraps a raw position already denominated in frames.
    #[inline]
    #[must_use]
    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    /// Returns the raw position.
    #[inline]
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The frame at the same wall-clock time at a rate `ratio` times this
    /// one's (`new / old`), to the nearest frame, half away from zero. What the
    /// executor does to its clock when a re-prepare changes the rate; `f64`
    /// holds a frame exactly up to 2^53, about 5 900 years at 48 kHz.
    ///
    /// ```
    /// use tutti_units::Frame;
    /// assert_eq!(Frame::new(256).scaled(44_100.0 / 48_000.0), Frame::new(235));
    /// ```
    #[inline]
    #[must_use]
    pub fn scaled(self, ratio: f64) -> Frame {
        Frame::nearest(self.0 as f64 * ratio)
    }

    /// The frame nearest a fractional `position`, half away from zero:
    /// saturating, so a negative or NaN position is [`Frame::ZERO`]. A position
    /// that keeps its fraction as an order key, across a rate change, lands on
    /// the frame this rounds it to.
    ///
    /// ```
    /// use tutti_units::Frame;
    /// assert_eq!(Frame::nearest(2.5), Frame::new(3));
    /// assert_eq!(Frame::nearest(2.49), Frame::new(2));
    /// assert_eq!(Frame::nearest(-3.0), Frame::ZERO);
    /// ```
    #[inline]
    #[must_use]
    pub fn nearest(position: f64) -> Frame {
        Frame::new(libm::round(position) as u64)
    }

    /// The frame at the same wall-clock time at rate `to`, this one being a
    /// position at rate `from`: [`scaled`](Self::scaled) by `to / from`.
    ///
    /// ```
    /// use tutti_units::{Frame, SampleRate};
    /// let (cd, dat) = (SampleRate::of(44_100.0), SampleRate::of(48_000.0));
    /// assert_eq!(Frame::new(44_100).rescaled(cd, dat), Frame::new(48_000));
    /// // 7 frames at 44.1 kHz are 7.619 at 48 kHz: the nearest is 8.
    /// assert_eq!(Frame::new(7).rescaled(cd, dat), Frame::new(8));
    /// // 5 are 5.442: the nearest is 5.
    /// assert_eq!(Frame::new(5).rescaled(cd, dat), Frame::new(5));
    /// ```
    #[inline]
    #[must_use]
    pub fn rescaled(self, from: crate::SampleRate, to: crate::SampleRate) -> Frame {
        self.scaled(to.get() / from.get())
    }

    /// The time this frame is at, counted from frame 0 at `rate`.
    ///
    /// ```
    /// use tutti_units::{Frame, SampleRate, Seconds};
    /// assert_eq!(Frame::new(24_000).to_seconds(SampleRate::of(48_000.0)), Seconds::new(0.5));
    /// ```
    #[inline]
    #[must_use]
    pub fn to_seconds(self, rate: crate::SampleRate) -> crate::Seconds {
        crate::Seconds::new(self.0 as f64 / rate.get())
    }

    /// Returns how many frames `earlier` is before `self`, or `None` when it is not
    /// before (it is later) or the distance does not fit a `Samples` on this
    /// target.
    ///
    /// The named replacement for `Frame - Frame`, which the module docs omit:
    /// an unsigned subtraction either wraps or saturates, and both hide the
    /// ordering bug that produced a negative distance.
    #[inline]
    pub fn since(self, earlier: Frame) -> Option<Samples> {
        let d = self.0.checked_sub(earlier.0)?;
        usize::try_from(d).ok().map(Samples::new)
    }
}

impl From<Frame> for u64 {
    #[inline]
    fn from(frame: Frame) -> Self {
        frame.0
    }
}

/// A position advanced by a count. Saturating: see the module docs.
impl Add<Samples> for Frame {
    type Output = Frame;
    #[inline]
    fn add(self, rhs: Samples) -> Frame {
        // `usize` fits in `u64` on every target Rust supports; the fallback
        // is the saturation the operator promises anyway.
        Frame(
            self.0
                .saturating_add(u64::try_from(rhs.get()).unwrap_or(u64::MAX)),
        )
    }
}

impl AddAssign<Samples> for Frame {
    #[inline]
    fn add_assign(&mut self, rhs: Samples) {
        *self = *self + rhs;
    }
}

/// A position on the engine's clock and the rate it was counted at: an instant
/// that stays the same instant when the rate changes.
///
/// The engine's frame clock counts frames **at the current rate**, and a
/// re-prepare at another rate moves it to the same wall-clock time at the new
/// one ([`Frame::rescaled`]). A bare [`Frame`] read before such a change and
/// used after it names the wrong instant; a `Moment` carries the rate it was
/// read at, so whoever resolves it can rescale it.
///
/// ```
/// use tutti_units::{Frame, Moment, SampleRate, Samples};
/// let m = Moment::new(Frame::new(48_000), SampleRate::of(48_000.0));
/// let later = m.after(Samples::new(480));
/// assert_eq!((later.frame(), later.rate()), (Frame::new(48_480), SampleRate::of(48_000.0)));
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Moment {
    frame: Frame,
    rate: crate::SampleRate,
}

impl Moment {
    /// `frame`, counted at `rate`.
    #[inline]
    #[must_use]
    pub const fn new(frame: Frame, rate: crate::SampleRate) -> Self {
        Self { frame, rate }
    }

    /// The frame, at [`rate`](Self::rate).
    #[inline]
    #[must_use]
    pub const fn frame(self) -> Frame {
        self.frame
    }

    /// The rate the frame was counted at.
    #[inline]
    #[must_use]
    pub const fn rate(self) -> crate::SampleRate {
        self.rate
    }

    /// `frames` later, at the same rate.
    #[inline]
    #[must_use]
    pub fn after(self, frames: Samples) -> Self {
        Self {
            frame: self.frame + frames,
            rate: self.rate,
        }
    }
}

/// When a scheduled command takes effect.
///
/// Every control-thread command that is meant to happen *during playback* —
/// a scheduled event or parameter ramp, and play / stop / seek on a
/// transport — states its time as one of these. There is
/// **no untimed overload**: "whenever the next block starts" is spelled
/// [`NextBlock`](At::NextBlock), a visible, greppable choice rather than a
/// default nobody chose.
///
/// A time already in the past when the command reaches the audio thread is
/// not dropped: it lands at the start of the next block and is counted as
/// late by whoever resolves it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum At {
    /// An absolute position on the engine's frame clock.
    ///
    /// That clock counts frames at the **current** rate, and a re-prepare at
    /// another rate moves it to the same wall-clock time at the new one. It is
    /// not a count of frames rendered: a sender that counts those lands early
    /// or late after a rate change by what the rescale dropped. Read the clock
    /// where it is published (`EventInletWriter::clock` in `tutti-graph`).
    Frame(Frame),
    /// A musical position, resolved against the transport snapshot of the
    /// block it falls in — so a tempo change before it moves it.
    Beat(Beat),
    /// The first frame of the next block the engine renders.
    NextBlock,
}

impl From<Frame> for At {
    /// [`At::Frame`].
    fn from(frame: Frame) -> Self {
        Self::Frame(frame)
    }
}

impl From<Beat> for At {
    /// [`At::Beat`].
    fn from(beat: Beat) -> Self {
        Self::Beat(beat)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A frame or a beat is the time it names**: `Frame::new(n)` is `At::Frame`
    /// and `Beat::new(b)` is `At::Beat`. (`At` has no `Moment` variant: a moment
    /// carries its own rate and is a `Stamp::Moment`.)
    #[test]
    fn a_frame_or_a_beat_is_an_at() {
        assert_eq!(At::from(Frame::new(48_000)), At::Frame(Frame::new(48_000)));
        assert_eq!(At::from(Beat::new(2.5)), At::Beat(Beat::new(2.5)));
    }

    /// **A frame is its count over the rate**: 24 000 frames at 48 kHz is half a second.
    #[test]
    fn a_frame_is_the_time_its_count_names() {
        let rate = crate::SampleRate::of(48_000.0);
        assert_eq!(Frame::new(24_000).to_seconds(rate).get(), 0.5);
        assert_eq!(Frame::new(0).to_seconds(rate).get(), 0.0);
    }

    /// Advancing adds the count, and saturates instead of wrapping.
    #[test]
    fn advancing_adds_and_saturates() {
        assert_eq!(Frame::new(100) + Samples::new(28), Frame::new(128));
        let mut f = Frame::new(u64::MAX - 1);
        f += Samples::new(5);
        assert_eq!(f, Frame::new(u64::MAX));
    }

    /// `since` is a checked distance: a later frame is not "some frames ago".
    #[test]
    fn since_is_checked() {
        assert_eq!(
            Frame::new(600).since(Frame::new(88)),
            Some(Samples::new(512))
        );
        assert_eq!(Frame::new(88).since(Frame::new(88)), Some(Samples::new(0)));
        assert_eq!(Frame::new(88).since(Frame::new(600)), None);
    }

    /// What is deliberately *absent*: `Frame + Frame`, `Frame - Frame`,
    /// `Frame + u64`, `Frame * k` (see the module docs). The first three are
    /// enforced by the `compile_fail` examples on [`Frame`]; this entry keeps
    /// the ledger beside the operators, as `samples.rs` does. Adding an
    /// omitted operator means deleting its entry here and its example there.
    #[test]
    fn omitted_operators_are_documented() {}
}
