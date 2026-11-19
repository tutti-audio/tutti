//! [`OutputPlan`]: when a hardware MIDI output should send an event, as a pure
//! function.
//!
//! The graph's MIDI is on frames. A device wants it at a time, and the OS's
//! sequencer or CoreMIDI will hold an event until a time it is given, so the
//! only question left on the host is *which* time. The answer is a sum, and
//! every term is a value the caller reads and passes in; the plan reads no
//! clock and keeps no thread:
//!
//! ```text
//! send time = the instant the frame is heard      ClockMap::os_at(frame)
//!           + the audio output's latency          OutputTerms::audio_output
//!           - the MIDI device's latency           OutputLatency::device
//!           + the user's offset                   OutputLatency::offset
//! ```
//!
//! The instant a frame is rendered is [`ClockMap::os_at`], and the audio
//! output's latency (the device's, from callback to speaker) delays when it is
//! heard; the MIDI device takes its own time to sound a note, so it is sent that
//! much earlier; the offset is the user's trim of what the two measurements miss.
//!
//! **Every send time is an [`Instant`]**, the OS's one monotonic clock; a
//! backend converts it to its own clock in one function (CoreMIDI's mach host
//! time, an ALSA queue's real time).
//!
//! **A term nobody has reported is `None`, never zero.** The audio output's
//! latency is not known until the stream's first callback and can change on a
//! restart ([`OutputTerms::is_complete`]): while it is `None` the plan sends
//! with the terms it has, which is **early by the audio latency**, and says so.
//!
//! **Bounds.** A send time at or before `now` is [`SendAt::Now`]. One more than
//! [`MAX_OUTPUT_AHEAD`] ahead is refused ([`Placement::TooFar`]): the backend's
//! queue must not hold an unbounded run of events (a stalled clock or a bad
//! latency would otherwise schedule minutes ahead), and the caller counts what
//! was refused. A frame the clock cannot place is sent [`Now`](SendAt::Now).

use std::time::Duration;

use tutti_platform::Instant;

use tutti_graph::Offset;
use tutti_ump::{Join, Spans, Ump, spans_joined};
use tutti_units::{Frame, Seconds};

use crate::ClockMap;

/// How far ahead of `now` an event may be scheduled: 2 s. A send time further
/// out is refused and counted ([`Placement::TooFar`]).
pub const MAX_OUTPUT_AHEAD: Duration = Duration::from_secs(2);

/// The largest magnitude of an [`OutputOffset`] and of a device's latency in an
/// [`OutputLatency`]: 1 s.
pub const MAX_OUTPUT_LATENCY: Seconds = Seconds::new(1.0);

/// When a backend should put an event on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendAt {
    /// At once.
    Now,
    /// At this instant, on the OS's monotonic clock; the backend schedules it
    /// natively (a queue time, a host time).
    At(Instant),
}

/// A packet for a sink that schedules: the offset the JR stamper and a
/// block-oriented sink read, and when to send it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outgoing {
    /// Its position in the batch, from the batch's first event (the spacing a
    /// JR Timestamp is made from; a block sink's offset).
    pub at: Offset,
    /// When to send it.
    pub send: SendAt,
    /// The packet.
    pub ump: Ump,
    /// Whether the packet is a message of its own or goes with the one before
    /// it.
    join: Join,
}

impl Outgoing {
    /// A packet that is a message of its own.
    #[must_use]
    pub const fn new(at: Offset, send: SendAt, ump: Ump) -> Self {
        Self {
            at,
            send,
            ump,
            join: Join::Alone,
        }
    }

    /// This packet as part of the message of the packet before it ([`Join`]):
    /// a cut never falls between them.
    #[must_use]
    pub const fn joined_to_previous(self) -> Self {
        Self {
            join: Join::ToPrevious,
            ..self
        }
    }

    /// Whether the packet goes with the one before it.
    #[must_use]
    pub const fn join(&self) -> Join {
        self.join
    }

    /// The messages of `events`, in order: what a cut keeps together
    /// ([`spans_joined`]).
    pub fn messages(events: &[Self]) -> Spans<'_, Self, fn(&Self) -> Ump, fn(&Self) -> Join> {
        spans_joined(events, |event| event.ump, Self::join)
    }

    /// `events`, each sent at once.
    #[must_use]
    pub fn now_batch(events: &[tutti_graph::UmpAt]) -> Vec<Self> {
        events
            .iter()
            .map(|&(at, ump)| Self::new(at, SendAt::Now, ump))
            .collect()
    }
}

tutti_units::ranged! {
    /// A user's trim of when an output sends, in seconds, signed and at most
    /// [`MAX_OUTPUT_LATENCY`] (1 s) either way: positive sends later.
    ///
    /// `new` refuses what it cannot hold (NaN, infinity, beyond a second) and
    /// never clamps; `get` returns the seconds.
    OutputOffset(Seconds), OffsetOutOfRange, "finite and at most one second either way",
    |raw| raw.get().is_finite() && raw.get().abs() <= MAX_OUTPUT_LATENCY.get()
}

impl OutputOffset {
    /// No trim.
    pub const ZERO: Self = Self::of(Seconds::new(0.0));
}

impl Default for OutputOffset {
    fn default() -> Self {
        Self::ZERO
    }
}

/// One output's own timing: what its MIDI device takes to sound an event, and
/// the user's trim on top. Both default to nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct OutputLatency {
    device: Seconds,
    offset: OutputOffset,
}

impl OutputLatency {
    /// A device latency (measured, or what its maker declares) and a user
    /// offset; `None` when the device latency is negative, not finite, or over
    /// [`MAX_OUTPUT_LATENCY`].
    #[must_use]
    pub fn new(device: Seconds, offset: OutputOffset) -> Option<Self> {
        let d = device.get();
        (d.is_finite() && (0.0..=MAX_OUTPUT_LATENCY.get()).contains(&d))
            .then_some(Self { device, offset })
    }

    /// What the device takes to sound an event: it is sent that much earlier.
    #[must_use]
    pub const fn device(self) -> Seconds {
        self.device
    }

    /// The user's trim.
    #[must_use]
    pub const fn offset(self) -> OutputOffset {
        self.offset
    }
}

/// Every term of an output's send time but the frame's own: what an output is
/// planned with.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct OutputTerms {
    /// The audio output's latency, from the render callback to the sound
    /// leaving: `None` while nobody has reported it (before the stream's first
    /// callback, again after a restart), and when there is no audio output.
    pub audio_output: Option<Seconds>,
    /// The output's own.
    pub latency: OutputLatency,
}

impl OutputTerms {
    /// Whether every term is known. While it is not, events go out **early by
    /// the audio output's latency**, which is unreported, and not by a zero
    /// that stands in for it.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.audio_output.is_some()
    }
}

/// What the plan decided for one frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Send it, now or at an instant.
    Send(SendAt),
    /// Its send time is more than [`MAX_OUTPUT_AHEAD`] ahead: not sent, and the
    /// caller counts it.
    TooFar,
}

/// The pure core that turns a frame into a send time: see the module docs.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct OutputPlan {
    terms: OutputTerms,
}

impl OutputPlan {
    /// A plan over `terms`.
    #[must_use]
    pub const fn new(terms: OutputTerms) -> Self {
        Self { terms }
    }

    /// The terms it plans with.
    #[must_use]
    pub const fn terms(&self) -> OutputTerms {
        self.terms
    }

    /// When `frame` is sent. `map` places a frame on the OS clock (`None`, or a
    /// frame it cannot place: sent now); `now` is the caller's reading of the
    /// same clock.
    #[must_use]
    pub fn place(&self, frame: Frame, map: Option<&ClockMap>, now: Instant) -> Placement {
        let Some(heard) = map.and_then(|map| map.os_at(frame)) else {
            return Placement::Send(SendAt::Now);
        };
        // One signed sum in seconds, then one step on the clock: later by the
        // audio output and the trim, earlier by the device.
        let net = self.terms.audio_output.unwrap_or(Seconds::new(0.0))
            + self.terms.latency.offset.get()
            - self.terms.latency.device;
        let step = Duration::try_from_secs_f64(net.get().abs()).ok();
        let Some(at) = step.and_then(|step| {
            if net.get() >= 0.0 {
                heard.checked_add(step)
            } else {
                heard.checked_sub(step)
            }
        }) else {
            return Placement::Send(SendAt::Now);
        };
        match at.checked_duration_since(now) {
            None | Some(Duration::ZERO) => Placement::Send(SendAt::Now),
            Some(ahead) if ahead > MAX_OUTPUT_AHEAD => Placement::TooFar,
            Some(_) => Placement::Send(SendAt::At(at)),
        }
    }
}

#[cfg(test)]
mod tests;
