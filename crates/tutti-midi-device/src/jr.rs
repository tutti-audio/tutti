//! The receiver side of JR Timestamps: [`JrReceiver`] turns a sender's JR Clock
//! messages, with the instants they arrived at, into the sender-to-host time
//! map, and a JR Timestamp into the host time of the message it precedes
//! (M2-104 §7.2.2).
//!
//! A sender's clock counts [`JrTicks::RATE`] ticks a second in 16 bits, so it wraps
//! every 2.097 s, and a clock message arrives at least every 250 ms with the
//! jitter of the connection. The receiver keeps the last [`WINDOW`] clock
//! messages as pairs of (unwrapped sender time, arrival) and fits one line
//! through them:
//!
//! - **Drift** is the slope of a least-squares fit, used once the window spans
//!   [`DRIFT_SPAN`] and while it stays within [`MAX_DRIFT`] of 1; otherwise the
//!   sender's clock is taken as exactly [`JrTicks::RATE`], since a short span or heavy
//!   jitter would read jitter as drift.
//! - **Offset** is the line's lowest point: a message is never sent after it
//!   arrives, so the arrival nearest the line is the one with the least delay,
//!   and the line through it is the sender's clock on the host's.
//! - **Jitter** is how far the window's arrivals lie above that line, at most.
//!   A message arrives within it of its [`JrTime::Sent`] time, so a caller that
//!   renders at the send time plus the jitter renders each message at the same
//!   delay.
//!
//! The wrap is resolved against the line itself: a timestamp's 16 bits are
//! taken to be the sender time nearest the one the line gives at the arrival
//! of the message it times, which is right for a timestamp within half a wrap
//! (1.048 s) of it. A sender silent for [`LOST_AFTER`] is [`JrLock::Lost`]: its
//! timestamps are not mapped, and the next clock message starts the window
//! over.

use std::time::Duration;

use tutti_platform::Instant;

use tutti_ump::JrTicks;
use tutti_units::Seconds;

use crate::slope;

fn ticks_to_seconds(ticks: f64) -> Seconds {
    Seconds::new(ticks / JrTicks::RATE.get())
}

fn seconds_to_ticks(seconds: Seconds) -> f64 {
    seconds.get() * JrTicks::RATE.get()
}

/// How many JR Clock messages the fit keeps: 3.2 s of them at the 100 ms
/// cadence a sender may use, and 8 s at the 250 ms ceiling.
const WINDOW: usize = 32;

/// How long the window must span before its slope is taken as the sender's
/// drift.
const DRIFT_SPAN: Duration = Duration::from_secs(1);

/// How long without a JR Clock message before the sender's clock is lost: just
/// under half a wrap, the longest silence after which a timestamp is still
/// unambiguous. The spec's ceiling on the interval is 250 ms
/// (M2-104 §7.2.2.1), so a sender that keeps it never gets here.
const LOST_AFTER: Duration = Duration::from_secs(1);

/// How far from 1 a fitted slope may be (host seconds per sender second): 1%,
/// 10 000 ppm, thirty to a hundred times what a crystal drifts (tens to a few
/// hundred ppm), so a slope outside it is jitter over too short a span, not a
/// sender's drift. A sender beyond it is taken at slope 1: the window's spread
/// absorbs its drift, and the delay the jitter adds is bounded by about the
/// window's length times the drift.
const MAX_DRIFT: f64 = 0.01;

/// How long before its arrival a JR Clock may map: a clock older than the
/// silence that loses the clock is not live, so the line does not describe its
/// sender.
const STALE_AFTER: Duration = LOST_AFTER;

/// The least spread a window is taken to have when it judges a JR Clock off its
/// line: 1 ms, an allowance for a transport's arrival quantization (a USB MIDI
/// frame), not a figure of the specification. A window of one or two
/// readings has seen almost no jitter, so without it the ordinary shrinking of
/// a connection's delay reads as a clock that jumped, and restarts keep the
/// window too short to fit a drift.
const MIN_JITTER_TOLERANCE: Seconds = Seconds::new(0.001);

/// A sender clock's wrap, in ticks.
const WRAP: f64 = 65_536.0;

/// Where a receiver is with a sender's clock, at an instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum JrLock {
    /// No JR Clock message has been heard: a message is rendered as soon as it
    /// arrives (M2-104 §7.2.2.3).
    Waiting,
    /// The last JR Clock message is recent enough that a timestamp is mapped.
    Locked,
    /// The sender has been silent for a second (just under half a wrap): a
    /// timestamp is not mapped until the clock is heard again.
    Lost,
}

/// Why a JR Timestamp has no host time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Unmapped {
    /// The sender's clock was never heard.
    NoClock,
    /// The sender's clock was heard, and not lately ([`JrLock::Lost`]).
    ClockLost,
    /// There is no JR Timestamp to map: none was heard, or the last is older
    /// than a second, which is too old for its wrap to be told.
    NoTimestamp,
    /// The mapped time cannot be an `Instant`.
    OutOfRange,
}

/// The host time of a message a JR Timestamp precedes.
#[derive(Clone, Copy, Debug, PartialEq)]
#[must_use = "an `Arrival` outcome means the message keeps its arrival time"]
#[non_exhaustive]
pub enum JrTime {
    /// The instant the sender sent the message at, on the host's clock, never
    /// after the message arrived.
    Sent {
        /// When the message was sent.
        at: Instant,
        /// How far above the line the window's arrivals lie, at most: the
        /// delay that renders every message after its arrival, if the jitter
        /// holds.
        jitter: Seconds,
    },
    /// The timestamp cannot be mapped: the message is placed at its arrival.
    Arrival(Unmapped),
}

impl JrTime {
    /// When to render the message: the send time plus the jitter, or `arrival`
    /// where the timestamp was not mapped.
    #[must_use]
    pub fn render_at(self, arrival: Instant) -> Instant {
        match self {
            // `jitter` is a spread of arrivals over the window, seconds at
            // most, so the sum is past `Instant`'s range only for an `at` that
            // is already at its end, where `at` is kept.
            Self::Sent { at, jitter } => Duration::try_from_secs_f64(jitter.get())
                .ok()
                .and_then(|jitter| at.checked_add(jitter))
                .unwrap_or(at),
            Self::Arrival(_) => arrival,
        }
    }
}

/// What a JR Clock message did to the receiver.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[must_use]
#[non_exhaustive]
pub enum ClockHeard {
    /// It joined the window.
    Taken,
    /// It came after a silence of more than a second, or does not fit the line
    /// the window holds (it maps after its arrival by more than the window's
    /// jitter, or more than a second before it, as a sender whose clock
    /// restarted or jumped does): the window started over with it.
    Restarted,
    /// Its time is not after the last one's (it was reordered, or sent twice):
    /// it is ignored. A clock is monotonic but for the wrap (M2-104 §7.2.2.1).
    /// A sender whose clock steps back by under a second keeps being ignored
    /// until its clock passes the last one's or the silence reaches a second,
    /// when the window restarts.
    Late,
}

/// One JR Clock message: the sender's time, unwrapped, and when it arrived,
/// in seconds from the receiver's origin.
#[derive(Clone, Copy, Debug)]
struct Reading {
    ticks: i64,
    at: Seconds,
}

/// A sender's clock on the host's; see the module.
#[derive(Clone, Debug)]
pub struct JrReceiver {
    window: [Reading; WINDOW],
    len: usize,
    /// Where the oldest reading is.
    start: usize,
    /// The instant the readings' seconds are counted from: the first
    /// arrival.
    origin: Option<Instant>,
    /// The last clock message's 16 bits and arrival, for the wrap and the
    /// silence.
    last: Option<(JrTicks, Instant)>,
    /// The line the window fits, kept until the window changes.
    fit: Option<Line>,
    /// The last JR Timestamp and its arrival: it times every message after it
    /// until the next.
    stamp: Option<(JrTicks, Instant)>,
}

impl Default for JrReceiver {
    fn default() -> Self {
        Self::new()
    }
}

/// The line the window fits: host seconds = `slope` * sender seconds from
/// `ticks0` + `offset`.
#[derive(Clone, Copy, Debug)]
struct Line {
    ticks0: i64,
    slope: f64,
    offset: Seconds,
    jitter: Seconds,
}

impl JrReceiver {
    /// A receiver that has heard no clock.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            window: [Reading {
                ticks: 0,
                at: Seconds::ZERO,
            }; WINDOW],
            len: 0,
            start: 0,
            origin: None,
            last: None,
            fit: None,
            stamp: None,
        }
    }

    /// Forgets the sender's clock: the state of one never heard.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Where the receiver is with the sender's clock at `now`.
    pub fn lock(&self, now: Instant) -> JrLock {
        match self.last {
            None => JrLock::Waiting,
            Some((_, at)) if now.saturating_duration_since(at) > LOST_AFTER => JrLock::Lost,
            Some(_) => JrLock::Locked,
        }
    }

    /// Takes a JR Clock message of sender time `ticks` that arrived at
    /// `arrival`.
    pub fn clock(&mut self, ticks: JrTicks, arrival: Instant) -> ClockHeard {
        let mut heard = ClockHeard::Taken;
        let mut unwrapped = i64::from(ticks.get());
        let mut late = false;
        match self.last {
            None => {}
            Some((_, at)) if arrival.saturating_duration_since(at) > LOST_AFTER => {
                // The wrap count of a clock heard after this long is unknown.
                self.restart();
                heard = ClockHeard::Restarted;
            }
            Some((last, _)) => {
                // The sender's step, a signed 16-bit difference: under half a
                // wrap, because the silence is.
                let step = i64::from(ticks.since(last));
                unwrapped = self.newest().map_or(unwrapped, |r| r.ticks + step);
                if self.off_line(unwrapped, arrival) {
                    self.restart();
                    unwrapped = i64::from(ticks.get());
                    heard = ClockHeard::Restarted;
                } else {
                    late = step <= 0;
                }
            }
        }
        if late {
            return ClockHeard::Late;
        }
        let origin = *self.origin.get_or_insert(arrival);
        let reading = Reading {
            ticks: unwrapped,
            at: seconds_from(origin, arrival),
        };
        if self.len == WINDOW {
            self.start = (self.start + 1) % WINDOW;
            self.len -= 1;
        }
        self.window[(self.start + self.len) % WINDOW] = reading;
        self.len += 1;
        self.last = Some((ticks, arrival));
        self.fit = self.fit_window();
        heard
    }

    /// Empties the window, keeping the origin of its seconds.
    fn restart(&mut self) {
        self.len = 0;
        self.start = 0;
        self.fit = None;
        self.stamp = None;
    }

    /// Whether a clock of unwrapped sender time `ticks` that arrived at
    /// `arrival` cannot come from the sender the window describes: its send
    /// time on the line is after its arrival by more than the window's jitter
    /// (at least [`MIN_JITTER_TOLERANCE`]) and a tick (a message is not heard
    /// before it is sent, the jitter bounds how much earlier than the line an
    /// arrival has been, and a tick is the sender's rounding), or is more than
    /// [`STALE_AFTER`] before it (older than a live clock).
    fn off_line(&self, ticks: i64, arrival: Instant) -> bool {
        let (Some(origin), Some(line)) = (self.origin, self.fit) else {
            return false;
        };
        let sent = ticks_to_seconds((ticks - line.ticks0) as f64) * line.slope + line.offset;
        let age = seconds_from(origin, arrival) - sent;
        age + line.jitter.max(MIN_JITTER_TOLERANCE) + ticks_to_seconds(1.0) < Seconds::ZERO
            || age > Seconds::new(STALE_AFTER.as_secs_f64())
    }

    /// Takes a JR Timestamp of sender time `ticks` that arrived at `arrival`: it
    /// times the messages after it, until the next ([`resolve_held`](Self::resolve_held)).
    pub fn timestamp(&mut self, ticks: JrTicks, arrival: Instant) {
        self.stamp = Some((ticks, arrival));
    }

    /// The host time of a message that arrived at `arrival` under the last JR
    /// Timestamp taken. A timestamp older than `LOST_AFTER` is not used: its
    /// wrap cannot be told, as a clock's cannot. One taken before the clock
    /// restarted is dropped with the window.
    pub fn resolve_held(&self, arrival: Instant) -> JrTime {
        match self.stamp {
            Some((ticks, at)) if arrival.saturating_duration_since(at) <= LOST_AFTER => {
                self.resolve(ticks, arrival)
            }
            _ => JrTime::Arrival(Unmapped::NoTimestamp),
        }
    }

    /// The host time of a message that a JR Timestamp of sender time `ticks`
    /// precedes and that arrived at `arrival`.
    pub fn resolve(&self, ticks: JrTicks, arrival: Instant) -> JrTime {
        match self.lock(arrival) {
            JrLock::Waiting => return JrTime::Arrival(Unmapped::NoClock),
            JrLock::Lost => return JrTime::Arrival(Unmapped::ClockLost),
            JrLock::Locked => {}
        }
        let (Some(origin), Some(line)) = (self.origin, self.fit) else {
            return JrTime::Arrival(Unmapped::NoClock);
        };
        // The sender time the line gives at the arrival, and the timestamp's
        // wrap nearest it.
        let seen = seconds_to_ticks((seconds_from(origin, arrival) - line.offset) / line.slope)
            + line.ticks0 as f64;
        let wraps = ((seen - f64::from(ticks.get())) / WRAP).round();
        let sender = f64::from(ticks.get()) + wraps * WRAP;
        let host = ticks_to_seconds(sender - line.ticks0 as f64) * line.slope + line.offset;
        let sent = instant_at(origin, host).map(|at| at.min(arrival));
        match sent {
            Some(at) => JrTime::Sent {
                at,
                jitter: line.jitter,
            },
            None => JrTime::Arrival(Unmapped::OutOfRange),
        }
    }

    fn newest(&self) -> Option<Reading> {
        (self.len > 0).then(|| self.window[(self.start + self.len - 1) % WINDOW])
    }

    fn readings(&self) -> impl Iterator<Item = Reading> + Clone + '_ {
        (0..self.len).map(|i| self.window[(self.start + i) % WINDOW])
    }

    /// The line through the window: `None` while it is empty.
    fn fit_window(&self) -> Option<Line> {
        let first = self.readings().next()?;
        let x = |r: Reading| ticks_to_seconds((r.ticks - first.ticks) as f64);
        let span = self.readings().last().map_or(Seconds::ZERO, x);
        let fitted = slope(self.readings().map(|r| (x(r).get(), r.at)));
        let slope = match fitted {
            Some(fitted)
                if span.get() >= DRIFT_SPAN.as_secs_f64() && (fitted - 1.0).abs() <= MAX_DRIFT =>
            {
                fitted
            }
            _ => 1.0,
        };
        let mut low = Seconds::new(f64::INFINITY);
        let mut high = Seconds::new(f64::NEG_INFINITY);
        for r in self.readings() {
            let offset = r.at - x(r) * slope;
            low = low.min(offset);
            high = high.max(offset);
        }
        Some(Line {
            ticks0: first.ticks,
            slope,
            offset: low,
            jitter: high - low,
        })
    }
}

/// Seconds from `origin` to `at`, negative before it.
fn seconds_from(origin: Instant, at: Instant) -> Seconds {
    match at.checked_duration_since(origin) {
        Some(after) => Seconds::new(after.as_secs_f64()),
        None => Seconds::new(-origin.duration_since(at).as_secs_f64()),
    }
}

/// The instant `seconds` after `origin`, `None` where it is not one.
fn instant_at(origin: Instant, seconds: Seconds) -> Option<Instant> {
    if seconds.get() >= 0.0 {
        origin.checked_add(Duration::try_from_secs_f64(seconds.get()).ok()?)
    } else {
        origin.checked_sub(Duration::try_from_secs_f64(-seconds.get()).ok()?)
    }
}

#[cfg(test)]
mod tests;
