use super::*;
use tutti_rt::SplitMix64;

const MS: Duration = Duration::from_millis(1);

/// The jitter of an arrival: a unit value from the stream.
fn unit(rng: &mut SplitMix64) -> f64 {
    (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64
}

/// A sender whose clock runs `1 + drift` times the host's, started at
/// `phase` ticks, with a connection that delays every message by `latency`
/// plus up to `jitter`.
struct Link {
    base: Instant,
    drift: f64,
    phase: f64,
    latency: Duration,
    jitter: Duration,
    rng: SplitMix64,
}

impl Link {
    fn new(drift: f64, phase: f64, jitter: Duration) -> Self {
        Self {
            base: Instant::now(),
            drift,
            phase,
            latency: 3 * MS,
            jitter,
            rng: SplitMix64::new(0x5EED),
        }
    }

    /// The sender's 16 bits at `t` seconds after the base.
    fn ticks(&self, t: f64) -> JrTicks {
        let ticks = self.phase + t * (1.0 + self.drift) * JrTicks::RATE.get();
        JrTicks::masked(ticks.round() as u64)
    }

    fn send(&self, t: f64) -> Instant {
        self.base + Duration::from_secs_f64(t)
    }

    /// When a message sent at `t` arrives.
    fn arrival(&mut self, t: f64) -> Instant {
        self.send(t) + self.latency + self.jitter.mul_f64(unit(&mut self.rng))
    }

    /// Sends a JR Clock at `t`.
    fn clock(&mut self, rx: &mut JrReceiver, t: f64) -> ClockHeard {
        let arrival = self.arrival(t);
        rx.clock(self.ticks(t), arrival)
    }

    /// The error of the send time the receiver gives a message sent at `t`,
    /// against the send time plus the least latency (which is what a
    /// receiver can know), and the outcome.
    fn error(&mut self, rx: &JrReceiver, t: f64) -> (f64, Instant, JrTime) {
        let arrival = self.arrival(t);
        let time = rx.resolve(self.ticks(t), arrival);
        let JrTime::Sent { at, .. } = time else {
            panic!("a locked receiver maps a timestamp: {time:?}");
        };
        let truth = self.send(t) + self.latency;
        let error = if at >= truth {
            at.duration_since(truth)
        } else {
            truth.duration_since(at)
        };
        (error.as_secs_f64(), arrival, time)
    }
}

/// Send times are recovered over a sender clock 200 ppm fast, across five
/// wraps of its 16 bits, with each arrival up to 2 ms late: within a quarter of
/// the jitter, and never after the message arrived.
/// Spec: m2-104/7.2.2.1#3, m2-104/7.2.2.2#2
#[test]
fn send_times_recovered_within_a_quarter_of_the_jitter_over_200_ppm_drift() {
    let jitter = 2 * MS;
    let mut link = Link::new(200e-6, 40_000.0, jitter);
    let mut rx = JrReceiver::new();
    let mut t = 0.0;
    while t < 10.0 {
        assert_eq!(link.clock(&mut rx, t), ClockHeard::Taken);
        t += 0.1;
    }
    let mut worst = 0.0f64;
    while t < 13.0 {
        let _ = link.clock(&mut rx, t);
        for k in 0..14 {
            let sent = t + f64::from(k) * 0.007;
            let (error, arrival, time) = link.error(&rx, sent);
            worst = worst.max(error);
            let JrTime::Sent { at, jitter: spread } = time else {
                unreachable!()
            };
            let spread = Duration::from_secs_f64(spread.get());
            assert!(at <= arrival, "a message is never sent after it arrives");
            assert!(spread <= jitter, "the spread is the jitter's at most");
        }
        t += 0.1;
    }
    assert!(
        worst <= jitter.as_secs_f64() / 4.0,
        "worst error {worst} s over a jitter of {jitter:?}"
    );
}

/// A timestamp whose 16 bits wrapped after the last clock is the sender time
/// the wrap makes it, to the tick: clocks at 65 000 and 65 500 ticks, then a
/// message at tick 1 000 (a wrap later), with no jitter and no drift.
#[test]
fn a_timestamp_past_the_wrap_is_the_next_wrap() {
    let mut link = Link::new(0.0, 65_000.0, Duration::ZERO);
    link.latency = Duration::ZERO;
    let mut rx = JrReceiver::new();
    let tick = 1.0 / JrTicks::RATE.get();
    assert_eq!(link.clock(&mut rx, 0.0), ClockHeard::Taken);
    assert_eq!(link.clock(&mut rx, 500.0 * tick), ClockHeard::Taken);
    // 1 536 ticks after the first clock: tick 65 000 + 1 536 = 66 536 = 1 000.
    let sent = 1_536.0 * tick;
    let arrival = link.arrival(sent);
    assert_eq!(link.ticks(sent), JrTicks::new(1_000));
    let JrTime::Sent { at, .. } = rx.resolve(JrTicks::new(1_000), arrival) else {
        panic!("locked")
    };
    let error = at.duration_since(link.send(sent));
    assert!(error < Duration::from_micros(1), "{error:?}");
}

/// A timestamp slightly behind the last clock (a message sent before it) is
/// before it, not a wrap ahead.
#[test]
fn a_timestamp_behind_the_last_clock_is_not_a_wrap_ahead() {
    let mut link = Link::new(0.0, 100.0, Duration::ZERO);
    link.latency = Duration::ZERO;
    let mut rx = JrReceiver::new();
    assert_eq!(link.clock(&mut rx, 0.2), ClockHeard::Taken);
    let sent = 0.19;
    let arrival = link.arrival(0.2);
    let JrTime::Sent { at, .. } = rx.resolve(link.ticks(sent), arrival) else {
        panic!("locked")
    };
    let error = if at > link.send(sent) {
        at.duration_since(link.send(sent))
    } else {
        link.send(sent).duration_since(at)
    };
    assert!(error < Duration::from_micros(40), "{error:?}");
}

/// Three clocks lost (a 900 ms gap, under the half wrap) leave the
/// receiver locked, and a timestamp after the gap is mapped to the tick.
#[test]
fn a_gap_of_lost_clocks_under_the_limit_stays_locked() {
    let mut link = Link::new(0.0, 0.0, Duration::ZERO);
    link.latency = Duration::ZERO;
    let mut rx = JrReceiver::new();
    for k in 0..5 {
        assert_eq!(link.clock(&mut rx, f64::from(k) * 0.25), ClockHeard::Taken);
    }
    // Clocks at 1.25, 1.5 and 1.75 are lost; the next is 0.9 s after the last.
    assert_eq!(link.clock(&mut rx, 1.9), ClockHeard::Taken);
    let (error, ..) = link.error(&rx, 2.0);
    assert!(error < 40e-6, "{error}");
}

/// A silence of more than [`LOST_AFTER`] loses the clock: timestamps keep
/// their arrival, the next clock starts the window over, and it is mapped
/// again to the tick, with the wraps of the silence unknown and unneeded.
#[test]
fn a_silence_loses_the_clock_and_the_next_clock_relocks() {
    let mut link = Link::new(0.0, 7.0, Duration::ZERO);
    link.latency = Duration::ZERO;
    let mut rx = JrReceiver::new();
    for k in 0..8 {
        let _ = link.clock(&mut rx, f64::from(k) * 0.1);
    }
    let silent = link.arrival(0.7 + 1.5);
    assert_eq!(rx.lock(silent), JrLock::Lost);
    assert_eq!(
        rx.resolve(link.ticks(2.2), silent),
        JrTime::Arrival(Unmapped::ClockLost)
    );
    // 5.0 s later, a clock of an unknown wrap.
    assert_eq!(link.clock(&mut rx, 5.0), ClockHeard::Restarted);
    assert_eq!(rx.lock(link.arrival(5.0)), JrLock::Locked);
    let (error, ..) = link.error(&rx, 5.05);
    assert!(error < 40e-6, "{error}");
}

/// A timestamp before any clock keeps the message's arrival, typed as such
/// (M2-104 §7.2.2.3: render as soon as possible).
#[test]
fn a_timestamp_before_any_clock_falls_back_to_arrival() {
    let rx = JrReceiver::new();
    let now = Instant::now();
    assert_eq!(rx.lock(now), JrLock::Waiting);
    let time = rx.resolve(JrTicks::new(1_234), now);
    assert_eq!(time, JrTime::Arrival(Unmapped::NoClock));
    assert_eq!(time.render_at(now), now);
}

/// A clock that is not after the last (reordered or repeated) is ignored and
/// leaves the mapping as it was.
#[test]
fn a_late_or_repeated_clock_is_ignored() {
    let mut link = Link::new(0.0, 0.0, Duration::ZERO);
    link.latency = Duration::ZERO;
    let mut rx = JrReceiver::new();
    let _ = link.clock(&mut rx, 0.2);
    let _ = link.clock(&mut rx, 0.3);
    let before = rx.resolve(link.ticks(0.35), link.arrival(0.35));
    assert_eq!(link.clock(&mut rx, 0.3), ClockHeard::Late);
    // Earlier by 50 ms, arriving now.
    let stale = rx.clock(link.ticks(0.25), link.arrival(0.31));
    assert_eq!(stale, ClockHeard::Late);
    assert_eq!(rx.resolve(link.ticks(0.35), link.arrival(0.35)), before);
}

/// On a link whose delay is 0 or 2 ms, both seen in the window, the render time
/// (send time plus jitter) is never before the arrival of a message whose delay
/// is either, and never more than the spread and a tick after it.
/// Spec: m2-104/7.2.2.1#4
#[test]
fn render_time_is_after_every_arrival_within_the_observed_delays() {
    let jitter = 2 * MS;
    let tick = Duration::from_secs_f64(1.0 / JrTicks::RATE.get());
    let link = Link::new(0.0, 0.0, jitter);
    let delayed = |k: u32| {
        if k.is_multiple_of(2) {
            Duration::ZERO
        } else {
            jitter
        }
    };
    let mut rx = JrReceiver::new();
    let mut t = 0.0;
    for k in 0..30 {
        let arrival = link.send(t) + link.latency + delayed(k);
        let _ = rx.clock(link.ticks(t), arrival);
        t += 0.1;
    }
    for k in 0..50 {
        let sent = t + f64::from(k) * 0.001;
        let arrival = link.send(sent) + link.latency + delayed(k);
        let time = rx.resolve(link.ticks(sent), arrival);
        let JrTime::Sent { jitter: spread, .. } = time else {
            panic!("locked: {time:?}")
        };
        let spread = Duration::from_secs_f64(spread.get());
        // The window holds both delays; a fitted slope adds a little.
        assert!(
            spread + tick >= jitter && spread <= jitter.mul_f64(1.25),
            "{spread:?}"
        );
        let render = time.render_at(arrival);
        assert!(render + tick >= arrival, "rendered before it arrived");
        assert!(render <= arrival + spread + tick, "rendered too late");
    }
}

/// A slope fitted over a short span with heavy jitter (here 30%, far beyond any
/// sender's drift) is not taken as drift: the line keeps the nominal rate and a
/// message after the span is mapped to its send time.
#[test]
fn a_slope_beyond_any_crystals_drift_is_not_used() {
    let mut link = Link::new(0.0, 0.0, Duration::ZERO);
    link.latency = Duration::ZERO;
    let mut rx = JrReceiver::new();
    // Five clocks over one second, the later ones increasingly late.
    for (k, late_ms) in [0u64, 0, 100, 200, 300].into_iter().enumerate() {
        let t = k as f64 * 0.25;
        let arrival = link.send(t) + Duration::from_millis(late_ms);
        let _ = rx.clock(link.ticks(t), arrival);
    }
    let sent = 1.1;
    let JrTime::Sent { at, .. } = rx.resolve(link.ticks(sent), link.send(sent) + 300 * MS) else {
        panic!("locked")
    };
    let error = if at > link.send(sent) {
        at.duration_since(link.send(sent))
    } else {
        link.send(sent).duration_since(at)
    };
    assert!(error < Duration::from_micros(40), "{error:?}");
}

/// The held JR Timestamp times the messages after it, and is not used once the
/// clock restarted or it is older than a second: it is `NoTimestamp`, not a wrap
/// later.
#[test]
fn a_held_timestamp_is_dropped_by_a_restart_and_by_age() {
    let mut link = Link::new(0.0, 7.0, Duration::ZERO);
    link.latency = Duration::ZERO;
    let mut rx = JrReceiver::new();
    let none = JrTime::Arrival(Unmapped::NoTimestamp);
    assert_eq!(rx.resolve_held(link.send(0.0)), none);
    for k in 0..20 {
        let _ = link.clock(&mut rx, f64::from(k) * 0.1);
    }
    rx.timestamp(link.ticks(0.45), link.arrival(0.45));
    assert!(matches!(
        rx.resolve_held(link.arrival(1.4)),
        JrTime::Sent { .. }
    ));
    assert_eq!(
        rx.resolve_held(link.arrival(1.6)),
        none,
        "older than a second"
    );
    // A restart takes the stamp with the window.
    rx.timestamp(link.ticks(1.7), link.arrival(1.7));
    link.phase += 0.5 * JrTicks::RATE.get();
    assert_eq!(link.clock(&mut rx, 1.9), ClockHeard::Restarted);
    assert_eq!(rx.resolve_held(link.arrival(1.9)), none);
}

/// After a reset the receiver is as one that never heard a clock.
#[test]
fn reset_forgets_the_clock() {
    let mut link = Link::new(0.0, 0.0, Duration::ZERO);
    let mut rx = JrReceiver::new();
    let _ = link.clock(&mut rx, 0.0);
    let now = link.arrival(0.0);
    assert_eq!(rx.lock(now), JrLock::Locked);
    rx.reset();
    assert_eq!(rx.lock(now), JrLock::Waiting);
}

/// A clock that jumps by `jump` seconds (the sender restarted, or was set)
/// restarts the window at the first clock after it, and a timestamp after the
/// next few clocks is mapped to the new clock, with no silence between.
fn assert_relocks_after_a_jump(jump: f64) {
    let mut link = Link::new(0.0, 40_000.0, Duration::ZERO);
    link.latency = 3 * MS;
    let mut rx = JrReceiver::new();
    let mut t = 0.0;
    while t < 3.0 {
        assert_eq!(link.clock(&mut rx, t), ClockHeard::Taken);
        t += 0.1;
    }
    link.phase += jump * JrTicks::RATE.get();
    assert_eq!(
        link.clock(&mut rx, t),
        ClockHeard::Restarted,
        "a jump of {jump} s"
    );
    t += 0.1;
    for _ in 0..WINDOW {
        assert_eq!(link.clock(&mut rx, t), ClockHeard::Taken);
        t += 0.1;
    }
    let (error, ..) = link.error(&rx, t + 0.05);
    assert!(error < 40e-6, "{error} s after a jump of {jump} s");
}

/// A sender clock that jumps forward maps after its arrival, which no line of
/// a live sender does: the window restarts and the clock is mapped again.
#[test]
fn a_sender_clock_that_jumps_forward_is_relocked() {
    assert_relocks_after_a_jump(0.5);
}

/// One that jumps back by more than the silence that loses a clock maps more
/// than [`LOST_AFTER`] before its arrival, and is relocked the same way.
#[test]
fn a_sender_clock_that_jumps_back_is_relocked() {
    assert_relocks_after_a_jump(-1.02);
}

/// A connection whose delay shrinks by a few hundred microseconds over its first
/// clocks (the least latency is found as clocks arrive) is not a clock that
/// jumped: a young window has seen almost no jitter and must not read this as one.
#[test]
fn early_readings_whose_delay_shrinks_do_not_restart() {
    let mut link = Link::new(0.0, 0.0, Duration::ZERO);
    let mut rx = JrReceiver::new();
    for (k, delay_us) in [1_000u64, 700, 400, 250, 250].into_iter().enumerate() {
        let t = k as f64 * 0.1;
        let arrival = link.send(t) + Duration::from_micros(delay_us);
        assert_eq!(rx.clock(link.ticks(t), arrival), ClockHeard::Taken, "{k}");
    }
    link.latency = Duration::from_micros(250);
    let (error, ..) = link.error(&rx, 0.55);
    assert!(error < 40e-6, "{error}");
}

/// A sender may send extra JR Clocks at a shorter period (M2-104 §7.2.2.1), so
/// the receiver draws no conclusion from the interval between them: clocks
/// 10 ms, 240 ms, 10 ms and 250 ms apart over a link with no latency, jitter or
/// drift are all taken, report only the tick's rounding as jitter (two ticks,
/// 64 us), and a message sent at 0.4 s maps to 0.4 s after the base within the
/// same.
/// Spec: m2-104/7.2.2.1#6
#[test]
fn the_interval_between_clocks_says_nothing_of_the_link() {
    let mut link = Link::new(0.0, 100.0, Duration::ZERO);
    link.latency = Duration::ZERO;
    let mut rx = JrReceiver::new();
    for t in [0.0, 0.010, 0.250, 0.260, 0.510] {
        assert_eq!(link.clock(&mut rx, t), ClockHeard::Taken);
    }
    let sent = 0.4;
    let arrival = link.arrival(sent);
    let JrTime::Sent { at, jitter } = rx.resolve(link.ticks(sent), arrival) else {
        panic!("locked");
    };
    let bound = Duration::from_micros(64);
    assert!(jitter.get() <= bound.as_secs_f64(), "{jitter:?}");
    let error = at
        .duration_since(link.send(sent))
        .max(link.send(sent).duration_since(at));
    assert!(error <= bound, "{error:?}");
}

/// M2-104 §7.2.2.1: a JR tick is 1/31250 s, so 31250 ticks are one second and
/// the 16-bit wrap, 65536 ticks, is 2.097152 s.
#[test]
fn a_tick_at_the_spec_rate_is_its_time_in_seconds() {
    assert_eq!(JrTicks::RATE.get(), 31_250.0);
    assert_eq!(
        JrTicks::new(31_250).seconds(),
        tutti_units::Seconds::new(1.0)
    );
    let near = |a: f64, b: f64| (a - b).abs() < 1e-12;
    assert!(near(JrTicks::new(1).seconds().get(), 32e-6));
    assert!(near(
        JrTicks::new(u16::MAX).seconds().get() + JrTicks::new(1).seconds().get(),
        2.097152
    ));
}
