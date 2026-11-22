//! MIDI 2.0 Jitter Reduction (JR) timestamps (M2-104 §2.1.4).
//!
//! JR timestamps let a receiver reconstruct the *original* spacing of a UMP
//! stream that a transport may have bunched up or spread out. The sender stamps
//! the stream with a monotonic 16-bit counter running at the JR reference clock
//! (31.25 kHz — one tick every 32 µs); the receiver reads the deltas between
//! successive stamps to recover inter-event intervals.
//!
//! The `Message::Utility(Utility::JrTimestamp(..))` of `tutti_ump` is the wire message. This
//! module is the *engine* around it:
//! - [`JrClock`] — the shared tick reference (samples ↔ 16-bit ticks).
//! - [`JrStamper`] — outbound: interleave a JR Timestamp before each event,
//!   derived from the event's sample-accurate [`Offset`]. Pure.
//! - [`JrClockEmitter`] — outbound: the periodic JR *Clock* (§7.2.2.1), which
//!   is what makes the stamps usable at all. §7.2.2.3: a receiver that has seen
//!   no JR Clock renders messages as soon as possible, ignoring every stamp.
//! - [`JrStream`] — a stamper plus the running sample origin one outbound wire
//!   stamps against, and optionally the clock cadence. This is what a pump
//!   holds; see its doc for why the origin is per-wire rather than per-caller.
//!
//! The inbound side, reading a sender's JR Clock and JR Timestamp messages, is
//! `tutti_midi_device::JrReceiver`.
//!
//! The clock and stamper are pure (no interior transport, no I/O).

use std::time::Duration;
use tutti_graph::UmpAt;

use tutti_graph::Offset;
use tutti_ump::{JrTicks, Message, Ump, Utility};
use tutti_units::{Frame, SampleRate, Samples, Seconds};

/// Converts sample positions to JR ticks at a fixed sample rate. Cheap,
/// `Copy`, and stateless — the shared reference both `JrStamper` reasons in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JrClock {
    sample_rate: SampleRate,
}

impl JrClock {
    /// A clock for a stream running at `sample_rate` Hz.
    pub fn new(sample_rate: SampleRate) -> Self {
        Self { sample_rate }
    }

    /// The JR tick a frame maps to (wrapping at 0x1_0000). A whole
    /// stream is stamped from one origin, so pass frames counted from it.
    #[inline]
    pub(crate) fn ticks_at(&self, frame: Frame) -> JrTicks {
        // ticks = frames * (JR_ticks/sec) / (frames/sec)
        let ticks = (frame.get() as f64) * JrTicks::RATE.get() / self.sample_rate.get();
        JrTicks::masked(ticks as u64)
    }
}

/// Outbound JR stamping: turns a bare UMP stream into a JR-timestamped one by
/// prefixing each event with a JR Timestamp derived from the
/// event's [`Offset`]. Stateless across calls (each `stamp_block` is relative
/// to the block it's given); carry a running sample origin in the caller if you
/// need cross-block continuity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct JrStamper {
    clock: JrClock,
}

impl JrStamper {
    /// A stamper for `sample_rate` Hz.
    ///
    /// There is no group: JR Timestamps are utility messages, which M2-104-UM
    /// §2.1.2 defines as groupless — a stamp applies to the stream, not to one
    /// group within it.
    pub fn new(sample_rate: SampleRate) -> Self {
        Self {
            clock: JrClock::new(sample_rate),
        }
    }

    /// Writes each input event into `out`, preceded by a JR Timestamp for its
    /// [`Offset`]. `origin` is the absolute frame of this
    /// block's offset zero, so stamps stay monotonic across blocks.
    ///
    /// Prefer [`JrStream`] over calling this directly: the origin has to advance
    /// by exactly the right amount between blocks, and that is the part a caller
    /// gets wrong.
    ///
    /// **Appends**; it does not clear. [`JrStream::stamp_span`] writes a due JR
    /// Clock ahead of the events and then calls this, so clearing here would
    /// drop it — the caller that owns the buffer is the one that clears it.
    ///
    /// Takes a buffer rather than returning a `Vec`: stamping runs once per
    /// block and its consumers hand the result straight to `MidiOut::queue` as
    /// a slice, so a returned `Vec` would allocate every block for a value
    /// nobody keeps.
    pub(crate) fn stamp_block(&self, events: &[UmpAt], origin: Frame, out: &mut Vec<UmpAt>) {
        out.reserve(events.len() * 2);
        for ev in events {
            let ticks = self
                .clock
                .ticks_at(origin + Samples::new(ev.0.get() as usize));
            out.push((ev.0, Message::Utility(Utility::JrTimestamp(ticks)).into()));
            out.push(*ev);
        }
    }

    /// How far the origin must advance after stamping `events` — one past the
    /// furthest frame offset in the block, or zero for an empty block.
    ///
    /// Split out from [`JrStream::stamp`] so the arithmetic that keeps stamps
    /// monotonic is stated once and testable on its own.
    #[inline]
    pub(crate) fn block_span(events: &[UmpAt]) -> Samples {
        events
            .iter()
            .map(|e| e.0.get() as usize)
            .max()
            .map_or(Samples::ZERO, |m| Samples::new(m + 1))
    }
}

/// The spec's hard ceiling on the JR Clock interval: M2-104 §7.2.2.1: a Sender sends a JR Clock
/// message at least once every 250 milliseconds.
pub(crate) const JR_CLOCK_MAX_INTERVAL: Duration = Duration::from_millis(250);

/// The interval [`JrClockEmitter::new`] actually uses.
///
/// Well inside the 250 ms ceiling, so a late block cannot push an interval
/// past it. §7.2.2.1 ties the bound to the 16-bit wrap (2.09712 seconds) and
/// also allows a shorter period, which helps the Receiver analyze jitter.
pub(crate) const JR_CLOCK_INTERVAL: Duration = Duration::from_millis(100);

/// Emits JR Clock messages on a cadence (M2-104 §7.2.2.1).
///
/// This is what makes JR Timestamps mean anything. §7.2.2.3: a receiver that has
/// seen no JR Clock renders those messages as soon as possible, so it
/// discards the sender's timestamps entirely. A stream that stamps but never
/// clocks has done the work and gets none of the benefit.
///
/// Driven by sample position rather than wall time, so it stays in step with the
/// stream it clocks and is deterministic under test. The emitter is pure: ask it
/// [`due`](Self::due) for each block and it tells you whether one is owed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct JrClockEmitter {
    clock: JrClock,
    sample_rate: SampleRate,
    /// The configured period, which [`interval`](Self::interval) counts in
    /// frames at the current rate.
    period: Duration,
    interval: Samples,
    /// Absolute frame of the last emitted clock. `None` until the
    /// first, which is due immediately — a receiver needs a reference before it
    /// can use any stamp.
    last_emit: Option<Frame>,
}

impl JrClockEmitter {
    /// An emitter at `sample_rate` Hz using [`JR_CLOCK_INTERVAL`].
    pub fn new(sample_rate: SampleRate) -> Self {
        Self::with_interval(sample_rate, JR_CLOCK_INTERVAL)
    }

    /// An emitter with an explicit interval.
    ///
    /// # Panics
    ///
    /// Panics if `interval` exceeds [`JR_CLOCK_MAX_INTERVAL`]: M2-104 §7.2.2.1 makes
    /// that bound a `shall`, so a longer interval is not a tuning choice, it is
    /// a non-conformant stream. Catching it here beats shipping one.
    pub(crate) fn with_interval(sample_rate: SampleRate, interval: Duration) -> Self {
        assert!(
            interval <= JR_CLOCK_MAX_INTERVAL,
            "JR Clock interval {interval:?} exceeds the §7.2.2.1 maximum of {JR_CLOCK_MAX_INTERVAL:?}"
        );
        Self {
            clock: JrClock::new(sample_rate),
            sample_rate,
            period: interval,
            interval: Seconds::new(interval.as_secs_f64()).to_samples(sample_rate),
            last_emit: None,
        }
    }

    /// The JR Clock owed at `origin`, or `None` if one is not yet due.
    ///
    /// Call once per block with the block's starting frame. The first call
    /// always yields a clock, and so does one whose `origin` is before the last
    /// (the stream restarted).
    pub fn due(&mut self, origin: Frame) -> Option<Ump> {
        let owed = match self.last_emit {
            None => true,
            Some(last) => origin.since(last).is_none_or(|gap| gap >= self.interval),
        };
        if !owed {
            return None;
        }
        self.last_emit = Some(origin);
        Some(Message::Utility(Utility::JrClock(self.clock.ticks_at(origin))).into())
    }
}

/// One outbound JR-stamped stream: a `JrStamper` plus the running sample origin
/// its stamps are relative to.
///
/// `JrStamper` is deliberately pure, so the origin has to live *somewhere*, and
/// "somewhere" is per **wire**, not per caller. A single endpoint fed by two
/// pumps — the clock master and the track MIDI-out both reach the same hardware
/// port — must share one origin, or the two interleave stamps that walk
/// backwards and a receiver reconstructs the wrong spacing. Owning it here is
/// what makes that structural rather than a convention each pump has to keep.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JrStream {
    stamper: JrStamper,
    /// Absolute frame of the next block's offset zero.
    origin: Frame,
    /// The JR Clock cadence, when this stream clocks as well as stamps.
    emitter: Option<JrClockEmitter>,
}

impl JrStream {
    /// A stream stamping at `sample_rate` Hz, starting at origin 0.
    ///
    /// Stamps only. Use [`clock`](Self::clock) for a conformant
    /// sender: §7.2.2.3 makes a receiver ignore timestamps it has no clock for.
    pub fn new(sample_rate: SampleRate) -> Self {
        Self::with_stamper(JrStamper::new(sample_rate))
    }

    /// A stream over an existing stamper.
    #[must_use]
    pub(crate) fn with_stamper(stamper: JrStamper) -> Self {
        Self {
            stamper,
            origin: Frame::ZERO,
            emitter: None,
        }
    }

    /// Adds the JR Clock cadence (M2-104 §7.2.2.1) at `JR_CLOCK_INTERVAL`.
    ///
    /// Once set, [`stamp_span`](Self::stamp_span) prefixes a clock to each block
    /// where one is due — including blocks with no events at all, which is why
    /// the cadence cannot ride on [`stamp`](Self::stamp) alone.
    #[must_use]
    pub fn clock(mut self, sample_rate: SampleRate) -> Self {
        self.emitter = Some(JrClockEmitter::new(sample_rate));
        self
    }

    /// Stamps one block and advances the origin past it, so the next call
    /// continues monotonically.
    ///
    /// The origin advances by the *events'* span, which is zero for an empty
    /// block. That is fine for stamping — an empty block stamps nothing — but it
    /// means a clock cadence driven from here would freeze on a silent stream.
    /// Use [`stamp_span`](Self::stamp_span) when clocking.
    pub fn stamp(&mut self, events: &[UmpAt], out: &mut Vec<UmpAt>) {
        self.stamp_span(events, JrStamper::block_span(events), out);
    }

    /// Stamps one block of a known `block` length, emitting a JR Clock
    /// first if the cadence owes one, and advances the origin by the *block*
    /// rather than by the events within it.
    ///
    /// This is the entry point a pump wants. §7.2.2.1 makes JR Clocks
    /// independent of any other message, so the cadence has to
    /// keep running through silence — and it only can if time advances on empty
    /// blocks, which the true block length provides and `block_span` does not.
    /// `out` is cleared first, so it is a destination and not an accumulator.
    /// Reusing one buffer across blocks is the point: this path runs once per
    /// block, and its callers only borrow the result to hand it to
    /// `MidiOut::queue`.
    pub fn stamp_span(&mut self, events: &[UmpAt], block: Samples, out: &mut Vec<UmpAt>) {
        out.clear();
        if let Some(emitter) = self.emitter.as_mut()
            && let Some(clock) = emitter.due(self.origin)
        {
            out.push((Offset::ZERO, clock));
        }
        self.stamper.stamp_block(events, self.origin, out);
        self.origin += block;
    }

    /// This stream at another sample rate: the stamper and the clock count
    /// ticks at `sample_rate`, the origin and the last clock's frame are
    /// converted to its frames (rounded to the nearest frame), and the clock's
    /// interval is its period counted again at `sample_rate`. JR time carries on
    /// from where it stood, within half a frame, when the graph is re-rated.
    #[must_use]
    pub fn at_rate(self, sample_rate: SampleRate) -> Self {
        let old = self.stamper.clock.sample_rate;
        let convert = |frame: Frame| {
            Frame::new((frame.get() as f64 * sample_rate.get() / old.get()).round() as u64)
        };
        Self {
            stamper: JrStamper::new(sample_rate),
            origin: convert(self.origin),
            emitter: self.emitter.map(|emitter| JrClockEmitter {
                clock: JrClock::new(sample_rate),
                sample_rate,
                period: emitter.period,
                interval: Seconds::new(emitter.period.as_secs_f64()).to_samples(sample_rate),
                last_emit: emitter.last_emit.map(convert),
            }),
        }
    }

    /// The absolute frame the next [`stamp`](Self::stamp) will start from.
    /// Exposed for tests and diagnostics.
    pub fn origin(&self) -> Frame {
        self.origin
    }
}

#[cfg(test)]
mod tests;
