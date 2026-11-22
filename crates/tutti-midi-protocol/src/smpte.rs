//! The SMPTE frame-rate enum, shared by MTC and LTC.
//!
//! Its own module because the rate is the one piece of timecode vocabulary that
//! outlives a decoder: a project holds a frame rate whether or not any MTC is
//! arriving, and the MTC Full Message and the runtime's decoders read it rather
//! than owning it.

use tutti_units::{Seconds, TimecodeFrames};

tutti_units::ranged! {
    /// The hours of a timecode: `0` to `23`, the hour of a day (M1 p.53 gives it
    /// five bits, of which a clock reads 24 values).
    #[derive(Eq, Hash)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
    TimecodeHours(u8), NotTimecodeHours, "an hour of the day (0 to 23)", |raw| raw < 24
}

tutti_units::ranged! {
    /// The minutes of a timecode: `0` to `59`.
    #[derive(Eq, Hash)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
    TimecodeMinutes(u8), NotTimecodeMinutes, "a minute of the hour (0 to 59)", |raw| raw < 60
}

tutti_units::ranged! {
    /// The seconds of a timecode: `0` to `59`.
    #[derive(Eq, Hash)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
    TimecodeSeconds(u8), NotTimecodeSeconds, "a second of the minute (0 to 59)", |raw| raw < 60
}

/// SMPTE frame rate for MTC/LTC.
///
/// MTC's two-bit rate code in the last Quarter Frame and in a Full Message's
/// hours byte is [`MtcRate`], which has no 29.97 non-drop:
/// [`mtc_rate`](Self::mtc_rate) is `None` for it, and every `MtcRate` converts
/// to a `SmpteFrameRate`.
///
/// # Two rates are 29.97, and the difference is counting, not speed
///
/// [`Fps2997Df`](Self::Fps2997Df) and [`Fps2997Ndf`](Self::Fps2997Ndf) run at the
/// identical rate — [`ratio`](Self::ratio) returns the same fraction for both. They
/// differ in whether the *labels* skip: drop-frame omits two frame numbers a
/// minute so the timecode stays aligned with wall-clock time, non-drop counts
/// every frame and drifts about 3.6 seconds an hour. Treating one as the other
/// keeps playing at the right speed and reports the wrong position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SmpteFrameRate {
    /// 24 fps (film)
    Fps24,
    /// 25 fps (PAL video)
    Fps25,
    /// 29.97 fps drop-frame (NTSC video)
    Fps2997Df,
    /// 29.97 fps non-drop (rare)
    Fps2997Ndf,
    /// 30 fps (audio/music)
    Fps30,
}

/// The frame rate MTC names in its two-bit code: in the last Quarter Frame of a
/// cycle and in the hours byte of a Full Message (M1 p.53 and T-9). The
/// discriminants are the wire codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[repr(u8)]
pub enum MtcRate {
    /// Code 0: 24 frames per second.
    Fps24 = 0,
    /// Code 1: 25 frames per second.
    Fps25 = 1,
    /// Code 2: 29.97 frames per second, drop-frame (30 labels a second).
    Fps2997Df = 2,
    /// Code 3: 30 frames per second, non-drop.
    Fps30 = 3,
}

impl MtcRate {
    /// Every rate, in code order.
    pub const ALL: [Self; 4] = [Self::Fps24, Self::Fps25, Self::Fps2997Df, Self::Fps30];

    /// The rate the low two bits of `bits` name: the field is two bits wide on
    /// the wire, so the bits above it (the hours' or a piece's own) are masked
    /// off.
    #[must_use]
    pub const fn masked(bits: u8) -> Self {
        match bits & 0x03 {
            0 => Self::Fps24,
            1 => Self::Fps25,
            2 => Self::Fps2997Df,
            _ => Self::Fps30,
        }
    }

    /// The frame labels in a second at this rate: the exclusive bound of a
    /// frame field.
    #[must_use]
    pub const fn labels_per_second(self) -> u8 {
        // At most 30, so it fits a byte.
        self.frame_rate().labels_per_second() as u8
    }

    /// The hours byte `0rrhhhhh` of `hours` at this rate (M1 p.53): the rate
    /// code over the five hour bits. This pair is the one home of the layout.
    #[must_use]
    pub const fn hours_byte(self, hours: u8) -> u8 {
        (self.code() << 5) | (hours & 0x1F)
    }

    /// The rate and the hours (0..=31 as written) of a hours byte `0rrhhhhh`:
    /// the inverse of [`hours_byte`](Self::hours_byte). Bit 7 is not read; a
    /// caller that reads a wire byte refuses it first.
    #[must_use]
    pub const fn from_hours_byte(hours_byte: u8) -> (Self, u8) {
        (Self::masked(hours_byte >> 5), hours_byte & 0x1F)
    }

    /// The two-bit code on the wire.
    #[must_use]
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// The rate as a [`SmpteFrameRate`].
    #[must_use]
    pub const fn frame_rate(self) -> SmpteFrameRate {
        match self {
            Self::Fps24 => SmpteFrameRate::Fps24,
            Self::Fps25 => SmpteFrameRate::Fps25,
            Self::Fps2997Df => SmpteFrameRate::Fps2997Df,
            Self::Fps30 => SmpteFrameRate::Fps30,
        }
    }
}

impl From<MtcRate> for SmpteFrameRate {
    fn from(rate: MtcRate) -> Self {
        rate.frame_rate()
    }
}

impl SmpteFrameRate {
    /// Every rate, in discriminant order.
    pub const ALL: [Self; 5] = [
        Self::Fps24,
        Self::Fps25,
        Self::Fps2997Df,
        Self::Fps2997Ndf,
        Self::Fps30,
    ];

    /// The MTC rate this one is sent as, or `None` for
    /// [`Fps2997Ndf`](Self::Fps2997Ndf), which MTC has no code for.
    #[must_use]
    pub const fn mtc_rate(self) -> Option<MtcRate> {
        match self {
            Self::Fps24 => Some(MtcRate::Fps24),
            Self::Fps25 => Some(MtcRate::Fps25),
            Self::Fps2997Df => Some(MtcRate::Fps2997Df),
            Self::Fps30 => Some(MtcRate::Fps30),
            Self::Fps2997Ndf => None,
        }
    }

    /// The rate as an exact fraction of frames over seconds, `(num, den)`:
    /// `24/1`, `25/1`, `30000/1001` for both 29.97 rates, `30/1`. The 29.97
    /// rates are not a binary float, so arithmetic over a long timeline and
    /// integer frame arithmetic at a position in samples use this fraction.
    #[must_use]
    pub const fn ratio(self) -> (u64, u64) {
        match self {
            Self::Fps24 => (24, 1),
            Self::Fps25 => (25, 1),
            Self::Fps2997Df | Self::Fps2997Ndf => (30_000, 1001),
            Self::Fps30 => (30, 1),
        }
    }

    /// The frames elapsed at `seconds` at this rate: `seconds * num / den`
    /// over the exact [`ratio`](Self::ratio), unrounded, so the fraction of a
    /// frame is kept. The one place a time becomes a frame count; each caller
    /// rounds it as its message needs: [`SmpteTimecode::from_seconds`] to the
    /// nearest frame ([`TimecodeFrames::round`]), `MmcPosition::from_seconds`
    /// down ([`TimecodeFrames::floor`]) with the fraction as its subframes.
    ///
    /// [`SmpteTimecode::from_seconds`]: SmpteTimecode::from_seconds
    #[must_use]
    pub fn frames_at(self, seconds: Seconds) -> TimecodeFrames {
        let (num, den) = self.ratio();
        TimecodeFrames::new(seconds.get() * num as f64 / den as f64)
    }

    /// The time one frame lasts: `den / num` over the exact
    /// [`ratio`](Self::ratio); [`frames_at`](Self::frames_at) is its inverse.
    #[must_use]
    pub fn frame_period(self) -> Seconds {
        let (num, den) = self.ratio();
        Seconds::new(den as f64 / num as f64)
    }

    /// Frame labels per second: the count the `FF` field wraps at (24, 25, 30,
    /// 30, 30 for the two 29.97 rates).
    pub const fn labels_per_second(self) -> u64 {
        let (num, den) = self.ratio();
        num.div_ceil(den)
    }

    /// Reports whether frame *numbers* are skipped to track wall-clock time.
    ///
    /// True only for [`Fps2997Df`](Self::Fps2997Df). A renderer that ignores this
    /// produces timecode that is correct frame-by-frame and wrong by seconds an
    /// hour in.
    #[must_use]
    pub fn is_drop_frame(self) -> bool {
        matches!(self, Self::Fps2997Df)
    }
}

/// An assembled SMPTE position: `HH:MM:SS:FF` plus the rate that gives the frame
/// count meaning, at any [`SmpteFrameRate`] (29.97 non-drop included).
/// [`MtcTimeCode`](crate::MtcTimeCode) is the one at a rate MTC has a code for.
///
/// The fields are the timecode's own *labels*, not a duration — under
/// [`SmpteFrameRate::Fps2997Df`] some frame numbers never occur, so arithmetic on
/// these fields directly is wrong. Convert with [`to_seconds`](Self::to_seconds)
/// first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(try_from = "SmpteTimecodeRaw", into = "SmpteTimecodeRaw")
)]
pub struct SmpteTimecode {
    hours: TimecodeHours,
    minutes: TimecodeMinutes,
    seconds: TimecodeSeconds,
    frames: u8,
    frame_rate: SmpteFrameRate,
}

/// The persisted shape of a [`SmpteTimecode`]: its labels as written, the frames
/// checked against the rate by `TryFrom` on the way back in.
#[cfg(feature = "serde")]
#[derive(serde::Serialize, serde::Deserialize)]
struct SmpteTimecodeRaw {
    hours: TimecodeHours,
    minutes: TimecodeMinutes,
    seconds: TimecodeSeconds,
    frames: u8,
    frame_rate: SmpteFrameRate,
}

/// A persisted position with a frame label out of range for its rate.
#[cfg(feature = "serde")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[error("a timecode frame label is out of range for its frame rate")]
pub struct InvalidSmpteTimecode;

#[cfg(feature = "serde")]
impl From<SmpteTimecode> for SmpteTimecodeRaw {
    fn from(t: SmpteTimecode) -> Self {
        Self {
            hours: t.hours,
            minutes: t.minutes,
            seconds: t.seconds,
            frames: t.frames,
            frame_rate: t.frame_rate,
        }
    }
}

#[cfg(feature = "serde")]
impl TryFrom<SmpteTimecodeRaw> for SmpteTimecode {
    type Error = InvalidSmpteTimecode;

    fn try_from(raw: SmpteTimecodeRaw) -> Result<Self, Self::Error> {
        Self::new(
            raw.hours,
            raw.minutes,
            raw.seconds,
            raw.frames,
            raw.frame_rate,
        )
        .ok_or(InvalidSmpteTimecode)
    }
}

impl SmpteTimecode {
    /// `00:00:00:00` at `frame_rate`.
    #[must_use]
    pub const fn origin(frame_rate: SmpteFrameRate) -> Self {
        Self {
            hours: TimecodeHours::of(0),
            minutes: TimecodeMinutes::of(0),
            seconds: TimecodeSeconds::of(0),
            frames: 0,
            frame_rate,
        }
    }

    /// The position `hours:minutes:seconds:frames` counted at `frame_rate`, or
    /// `None` when `frames` is not below the rate's label count (24, 25 or 30:
    /// [`SmpteFrameRate::labels_per_second`]). The other fields are in range by
    /// their types.
    ///
    /// The labels a drop-frame minute omits (`;00` and `;01`) are accepted:
    /// they are in range, and [`frame_count`](Self::frame_count) reads each as
    /// the next label that occurs.
    #[must_use]
    pub const fn new(
        hours: TimecodeHours,
        minutes: TimecodeMinutes,
        seconds: TimecodeSeconds,
        frames: u8,
        frame_rate: SmpteFrameRate,
    ) -> Option<Self> {
        if (frames as u64) < frame_rate.labels_per_second() {
            Some(Self {
                hours,
                minutes,
                seconds,
                frames,
                frame_rate,
            })
        } else {
            None
        }
    }

    /// Hours, 0..=23. MTC carries this in 5 bits split across two nibbles.
    #[must_use]
    pub const fn hours(self) -> TimecodeHours {
        self.hours
    }

    /// Minutes, 0..=59.
    #[must_use]
    pub const fn minutes(self) -> TimecodeMinutes {
        self.minutes
    }

    /// Seconds, 0..=59.
    #[must_use]
    pub const fn seconds(self) -> TimecodeSeconds {
        self.seconds
    }

    /// Frames within the second, below [`frame_rate`](Self::frame_rate)'s label
    /// count.
    #[must_use]
    pub const fn frames(self) -> u8 {
        self.frames
    }

    /// The rate this position is counted at. Carried in the same quarter-frame as
    /// the top hours bit, so it arrives with the position rather than being
    /// configured out of band.
    #[must_use]
    pub const fn frame_rate(self) -> SmpteFrameRate {
        self.frame_rate
    }
}

impl core::fmt::Display for SmpteTimecode {
    /// Canonical `HH:MM:SS:FF` SMPTE form (colon-separated, zero-padded).
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{:02}:{:02}:{:02}:{:02}",
            self.hours.get(),
            self.minutes.get(),
            self.seconds.get(),
            self.frames
        )
    }
}

impl SmpteTimecode {
    /// Elapsed time from `00:00:00:00`, in seconds.
    ///
    /// Counts through [`frame_count`](Self::frame_count), so drop-frame labels
    /// are mapped to the frames that elapsed. Non-drop 29.97 labels are counted
    /// at face value, at 30000/1001 frames per second.
    #[must_use]
    pub fn to_seconds(self) -> Seconds {
        let (num, den) = self.frame_rate.ratio();
        Seconds::new(self.frame_count() as f64 * den as f64 / num as f64)
    }

    /// Frames elapsed since `00:00:00:00`.
    ///
    /// Under [`SmpteFrameRate::Fps2997Df`] this skips the labels `;00` and
    /// `;01` that each minute not divisible by ten omits (SMPTE 12M). Labels
    /// that cannot occur in drop-frame are counted as the next label that does.
    pub fn frame_count(self) -> u64 {
        let per_sec = self.frame_rate.labels_per_second();
        let minutes = u64::from(self.hours.get()) * 60 + u64::from(self.minutes.get());
        let mut frames = u64::from(self.frames);
        // `;00` and `;01` do not exist in a minute not divisible by ten: they
        // count as `;02`, the next label that does.
        let dropped_label =
            self.frame_rate.is_drop_frame() && self.seconds.get() == 0 && minutes % 10 != 0;
        if dropped_label {
            frames = frames.max(2);
        }
        let labelled = (minutes * 60 + u64::from(self.seconds.get())) * per_sec + frames;
        if self.frame_rate.is_drop_frame() {
            labelled.saturating_sub(2 * (minutes - minutes / 10))
        } else {
            labelled
        }
    }

    /// The label of the frame nearest `seconds` at `frame_rate`: rounding, not
    /// flooring, so a time a float's width short of a frame boundary reads as
    /// that frame. `None` for a time no label names: before zero, NaN or
    /// infinite, or past the last label of the day, whose hours field ends at
    /// 23 (a time that rounds up to the 24th hour included). That last label
    /// is the day's end at 24, 25 and 30 fps and at drop-frame 29.97; non-drop
    /// 29.97 counts its labels at face value, 30 a second, so its labels run on
    /// to 86 486.4 s.
    ///
    /// ```
    /// use tutti_midi_protocol::{SmpteFrameRate, SmpteTimecode};
    /// use tutti_units::Seconds;
    ///
    /// let rate = SmpteFrameRate::Fps30;
    /// let at = |s| SmpteTimecode::from_seconds(Seconds::new(s), rate);
    /// assert_eq!(at(3661.5).unwrap().to_string(), "01:01:01:15");
    /// assert_eq!(at(-1.0), None);
    /// assert_eq!(at(86_400.0), None);
    /// ```
    #[must_use]
    pub fn from_seconds(seconds: Seconds, frame_rate: SmpteFrameRate) -> Option<Self> {
        let count = nearest_frame(seconds, frame_rate)?;
        let label = Self::from_frame_count(count, frame_rate);
        // A time that rounds up to the 24th hour wrapped its hours: its label
        // does not count back to it.
        (label.frame_count() == count).then_some(label)
    }

    /// The label of the frame nearest `seconds` at `frame_rate`, the hours
    /// wrapping at 24: for a generator whose clock runs on past a day, whose
    /// hours field counts 0 to 23 and starts again (MIDI Time Code, M1 p.53).
    /// `None` before zero, for NaN and for a time no frame count holds.
    #[must_use]
    pub fn from_seconds_wrapping(seconds: Seconds, frame_rate: SmpteFrameRate) -> Option<Self> {
        let count = nearest_frame(seconds, frame_rate)?;
        Some(Self::from_frame_count(count, frame_rate))
    }

    /// The label of frame number `count` since `00:00:00:00` at `frame_rate`;
    /// the inverse of [`frame_count`](Self::frame_count). Hours wrap at 24.
    #[must_use]
    pub fn from_frame_count(count: u64, frame_rate: SmpteFrameRate) -> Self {
        let per_sec = frame_rate.labels_per_second();
        let labelled = if frame_rate.is_drop_frame() {
            // 17_982 frames per ten minutes: the first minute keeps all 1800
            // labels, the other nine drop two.
            let (tens, rem) = (count / 17_982, count % 17_982);
            let dropped = if rem < 2 { 0 } else { 2 * ((rem - 2) / 1798) };
            count + 18 * tens + dropped
        } else {
            count
        };
        let secs = labelled / per_sec;
        Self {
            // Each is reduced below its bound, so the casts are exact.
            hours: TimecodeHours::of(((secs / 3600) % 24) as u8),
            minutes: TimecodeMinutes::of(((secs / 60) % 60) as u8),
            seconds: TimecodeSeconds::of((secs % 60) as u8),
            frames: (labelled % per_sec) as u8,
            frame_rate,
        }
    }
}

/// The frame count of the frame nearest `seconds`: `None` before zero, for NaN
/// and the infinities, and past 2^53 frames (where a count is no longer exact).
fn nearest_frame(seconds: Seconds, frame_rate: SmpteFrameRate) -> Option<u64> {
    if !seconds.get().is_finite() || seconds.get() < 0.0 {
        return None;
    }
    let frames = frame_rate.frames_at(seconds).round();
    (frames < 1 << 53).then_some(frames)
}

#[cfg(test)]
mod tests;
