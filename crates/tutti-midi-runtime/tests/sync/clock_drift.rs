//! A Beat Clock follower steered by the transport's beat (`BeatClockFollower::steer`).
//!
//! The oracle is arithmetic. A source at `T(t)` bpm has counted `B(t) = ∫T/60` beats
//! at time `t`. Start "does not start the sequence until the next Timing Clock"
//! (M1 p.30), so the first Timing Clock is beat 0 and tick `k` is sent when `B`
//! reaches `(k - 1) / 24`, 24 to a quarter note (M1 p.30). A model
//! transport integrates the tempo the follower announces, so its beat error is
//! `B(t)` less that integral, and every bound below is a formula of the
//! constants.

use tutti_midi_runtime::{BeatClockEvent, BeatClockFollower, MidiClockDecoder};
use tutti_ump::{Message, MidiGroup, System, U14, Ump};
use tutti_units::{Beat, Seconds};

fn at(seconds: f64) -> Seconds {
    Seconds::new(seconds)
}

fn system(message: System) -> Ump {
    Message::System(MidiGroup::FIRST, message).into()
}

/// A source: the beat it has counted at a time, and the time it reaches a beat.
struct Source {
    beat_at: fn(f64) -> f64,
    time_at: fn(f64) -> f64,
}

/// The transport the follower drives: it integrates the announced tempo.
struct Model {
    since: f64,
    beat: f64,
    tempo: f64,
}

impl Model {
    fn beat_at(&self, t: f64) -> f64 {
        self.beat + self.tempo * (t - self.since) / 60.0
    }

    fn take(
        &mut self,
        event: BeatClockEvent,
        now: f64,
        slips: &mut Vec<f64>,
        locates: &mut Vec<f64>,
    ) {
        match event {
            BeatClockEvent::Tempo { tempo, .. } => {
                self.beat = self.beat_at(now);
                self.since = now;
                self.tempo = tempo.get();
            }
            BeatClockEvent::Locate { position: beat, .. } => {
                self.beat = beat.get();
                self.since = now;
                locates.push(beat.get());
            }
            BeatClockEvent::Slipped { error } => slips.push(error.get()),
            _ => {}
        }
    }
}

/// What a run saw.
struct Run {
    /// The error, source less transport, at each steer after `from`.
    errors: Vec<f64>,
    /// The error carried by each `Slipped`.
    slips: Vec<f64>,
    /// The beat of each `Locate`.
    locates: Vec<f64>,
    follower: BeatClockFollower,
    model: Model,
}

/// Runs a follower over `seconds` of `source`, the transport starting `behind`
/// beats behind after Start, steering every 10 ms (one block) when `steer`.
fn run(source: &Source, seconds: f64, from: f64, behind: f64, steer: bool) -> Run {
    let mut follower = BeatClockFollower::new();
    let mut model = Model {
        since: 0.0,
        beat: 0.0,
        tempo: 120.0,
    };
    let (mut slips, mut locates, mut errors) = (Vec::new(), Vec::new(), Vec::new());
    let mut events = Vec::new();
    follower.step(system(System::Start), at(0.0), &mut |e| events.push(e));
    for e in events.drain(..) {
        model.take(e, 0.0, &mut slips, &mut locates);
    }
    model.beat -= behind;
    let (mut k, mut j) = (1u32, 1u32);
    loop {
        let tick = (source.time_at)((f64::from(k) - 1.0) / 24.0);
        let block = f64::from(j) * 0.01;
        if tick.min(block) > seconds {
            break;
        }
        if tick <= block {
            follower.step(system(System::TimingClock), at(tick), &mut |e| {
                events.push(e)
            });
            for e in events.drain(..) {
                model.take(e, tick, &mut slips, &mut locates);
            }
            k += 1;
        } else {
            if steer {
                let here = Some(Beat::new(model.beat_at(block)));
                follower.steer(here, at(block), &mut |e| events.push(e));
                for e in events.drain(..) {
                    model.take(e, block, &mut slips, &mut locates);
                }
            }
            if block >= from {
                errors.push((source.beat_at)(block) - model.beat_at(block));
            }
            j += 1;
        }
    }
    Run {
        errors,
        slips,
        locates,
        follower,
        model,
    }
}

fn worst(errors: &[f64]) -> f64 {
    errors.iter().fold(0.0, |m, e| m.max(e.abs()))
}

/// 120 bpm creeping to 120.09 over 600 s: `T(t) = 120 + a t`, `a = 0.09 / 600`.
const WANDER: f64 = 0.09 / 600.0;

fn wander() -> Source {
    Source {
        beat_at: |t| (120.0 * t + WANDER * t * t / 2.0) / 60.0,
        // The positive root of `WANDER/2 t^2 + 120 t = 60 beat`.
        time_at: |b| {
            let c = 60.0 * b;
            (-120.0 + (120.0f64 * 120.0 + 2.0 * WANDER * c).sqrt()) / WANDER
        },
    }
}

/// The least error a tempo step corrects, and the growth at that step over
/// `LOCK_TICKS` ticks (one beat) of the 120 bpm sources here, in beats (the
/// module docs of `follow`).
fn bound() -> f64 {
    let step = BeatClockFollower::TEMPO_STEP.get();
    let beat_seconds = MidiClockDecoder::LOCK_TICKS as f64 / 24.0 * 60.0 / 120.0;
    step * BeatClockFollower::CATCH_UP.get() / 60.0 + step / 60.0 * beat_seconds
}

/// **A tempo that wanders under `TEMPO_STEP` is followed.** The source ends 0.09
/// bpm over its start, under the step of 0.1, so an unsteered transport keeps
/// the tempo it locked on and falls behind by `∫(T - 120) / 60 = a t^2 / 120`
/// beats: 0.45 at 600 s. Steered, the error stays within `TEMPO_STEP *
/// CATCH_UP / 60` plus one beat of ticks of growth at that step.
#[test]
fn a_tempo_wander_under_the_step_is_followed() {
    let source = wander();
    let open = run(&source, 600.0, 5.0, 0.0, false);
    let predicted = WANDER * 600.0 * 600.0 / 120.0;
    let drift = *open.errors.last().unwrap();
    assert!((drift - predicted).abs() < 0.002, "{drift} vs {predicted}");
    let closed = run(&source, 600.0, 5.0, 0.0, true);
    let worst = worst(&closed.errors);
    assert!(worst <= bound(), "{worst} > {}", bound());
    assert!(closed.slips.is_empty());
}

/// **A source 500 ppm fast from the 30th second is followed.** 500 ppm of
/// 120 bpm is 0.06 bpm, under the step: unsteered, the transport falls behind
/// by `0.06 / 60` beats a second, 0.57 at 600 s. Steered it stays in bound.
#[test]
fn a_source_500_ppm_fast_is_followed() {
    // 2 beats a second, then 2.001 from the 30th.
    let source = Source {
        beat_at: |t| {
            if t <= 30.0 {
                2.0 * t
            } else {
                60.0 + 2.001 * (t - 30.0)
            }
        },
        time_at: |b| {
            if b <= 60.0 {
                b / 2.0
            } else {
                30.0 + (b - 60.0) / 2.001
            }
        },
    };
    let open = run(&source, 600.0, 5.0, 0.0, false);
    let predicted = 0.001 * (600.0 - 30.0);
    let drift = *open.errors.last().unwrap();
    assert!((drift - predicted).abs() < 0.002, "{drift} vs {predicted}");
    let closed = run(&source, 600.0, 5.0, 0.0, true);
    let worst = worst(&closed.errors);
    assert!(worst <= bound(), "{worst} > {}", bound());
    assert!(closed.slips.is_empty());
}

/// A steady 120 bpm: 2 beats a second, a tick every 1/48 s.
fn steady() -> Source {
    Source {
        beat_at: |t| 2.0 * t,
        time_at: |b| b / 2.0,
    }
}

/// **An error past `SLIP` locates once to the source's beat and says so.** The
/// transport starts 0.5 beat behind; once the locate after Start has landed
/// (the follower holds off `SETTLE`), the first steer reads 0.5, which is past
/// a quarter, and relocates to the source's beat at that instant, `2 now`,
/// then reads no more slips.
#[test]
fn an_error_past_slip_locates_once_and_reports() {
    let slipped = run(&steady(), 20.0, 5.0, 0.5, true);
    assert_eq!(slipped.slips.len(), 1, "{:?}", slipped.slips);
    assert!((slipped.slips[0] - 0.5).abs() < 1e-9, "{:?}", slipped.slips);
    // The Start's locate to beat 0, then the slip's, at the first 10 ms block
    // after `SETTLE` (the follower is locked by 0.5 s), at 2 beats a second.
    assert_eq!(slipped.locates.len(), 2, "{:?}", slipped.locates);
    let settle = BeatClockFollower::SETTLE.get();
    let at_slip = slipped.locates[1] / 2.0;
    assert!(
        (settle..settle + 0.01 + 1e-9).contains(&at_slip),
        "{at_slip}"
    );
    // On the source's beat, so the error stays in bound afterwards.
    assert!(worst(&slipped.errors) <= bound());
}

/// **The gauge reads source less transport.** 120 bpm locked, the transport
/// 0.01 beat behind (under `SLIP`): `error` is 0.01, a trim of `0.01 * 60 / 4 =
/// 0.15` bpm follows, and a follower not steered, not locked or in hold-off
/// reads none.
#[test]
fn the_gauge_reads_the_error_and_the_trim_follows() {
    let source = steady();
    let held = run(&source, 0.5, 0.0, 0.01, true);
    // 0.5 s is inside the hold-off the Start's locate began.
    assert_eq!(held.follower.error(), None);
    let steered = run(&source, 3.0, 0.0, 0.01, true);
    let Some(error) = steered.follower.error() else {
        panic!("a steered, locked follower reads a beat error");
    };
    let error = error.get();
    // The transport is still 0.01 behind at the first steer, then closes.
    assert!(error < 0.01 && error > 0.0, "{error}");
    // The trim moved the tempo the model integrates above the fit by a step.
    assert!(steered.model.tempo > 120.0 + BeatClockFollower::TEMPO_STEP.get() - 1e-9);
}

/// **The source's beat is the last tick's, not the next's.** Start "does not
/// start the sequence until the next Timing Clock" (M1 p.30): after Start, the
/// `k`-th Timing Clock is beat `(k - 1) / 24`. The error at the first read is
/// the last tick's beat plus the time since it at the fitted tempo, less the
/// transport's.
#[test]
fn the_error_at_the_first_read_is_the_beat_difference() {
    let mut follower = BeatClockFollower::new();
    let p = 60.0 / (120.0 * 24.0);
    follower.step(system(System::Start), at(0.0), &mut |_| {});
    for k in 1..=60u32 {
        follower.step(
            system(System::TimingClock),
            at(f64::from(k - 1) * p),
            &mut |_| {},
        );
    }
    // The 60th tick is beat 59 / 24 at 59 p; 5 ms later, past `SETTLE`, the
    // source is 0.01 beat on.
    let now = 59.0 * p + 0.005;
    assert!(now > BeatClockFollower::SETTLE.get());
    follower.steer(Some(Beat::new(2.4)), at(now), &mut |_| {});
    let Some(error) = follower.error() else {
        panic!("a beat error");
    };
    let error = error.get();
    assert!((error - (59.0 / 24.0 + 0.01 - 2.4)).abs() < 1e-9, "{error}");
}

/// **The origin of the source's beat**: the first Timing Clock after a Start is
/// beat 0 and the `k`-th is `(k - 1) / 24` (M1 p.30); after a Song Position
/// Pointer of `n` sixteenths and a Continue the first is `n / 4` (M1 p.28).
#[test]
fn the_decoder_counts_the_first_tick_after_start_and_continue_as_its_own_beat() {
    let tick = system(System::TimingClock);
    let mut decoder = MidiClockDecoder::new();
    assert_eq!(decoder.last_tick_beat(), None);
    decoder.feed(&system(System::Start), at(0.0));
    assert_eq!(decoder.last_tick_beat(), None);
    for k in 1..=30u32 {
        decoder.feed(&tick, at(f64::from(k) * 0.02));
        assert_eq!(
            decoder.last_tick_beat(),
            Some(Beat::new(f64::from(k - 1) / 24.0)),
            "tick {k}"
        );
    }
    decoder.feed(&system(System::SongPosition(U14::of(22))), at(1.0));
    decoder.feed(&system(System::Continue), at(1.0));
    assert_eq!(decoder.last_tick_beat(), None);
    decoder.feed(&tick, at(1.02));
    assert_eq!(decoder.last_tick_beat(), Some(Beat::new(22.0 / 4.0)));
    decoder.feed(&tick, at(1.04));
    assert_eq!(
        decoder.last_tick_beat(),
        Some(Beat::new(22.0 / 4.0 + 1.0 / 24.0))
    );
}

/// **A follower with nothing to steer reads no error**: after a Stop, and while
/// the transport is not rolling though the source is (`steer` given `None`).
#[test]
fn the_error_is_none_after_a_stop_and_while_the_transport_is_stopped() {
    let mut follower = BeatClockFollower::new();
    let p = 60.0 / (120.0 * 24.0);
    follower.step(system(System::Start), at(0.0), &mut |_| {});
    for k in 1..=60u32 {
        follower.step(
            system(System::TimingClock),
            at(f64::from(k - 1) * p),
            &mut |_| {},
        );
    }
    let now = 59.0 * p + 0.005;
    follower.steer(Some(Beat::new(2.4)), at(now), &mut |_| {});
    assert!(follower.error().is_some());
    // The transport stops while the source rolls.
    follower.steer(None, at(now + 0.01), &mut |_| {});
    assert_eq!(follower.error(), None);
    follower.steer(Some(Beat::new(2.4)), at(now + 0.02), &mut |_| {});
    assert!(follower.error().is_some());
    // The source stops.
    follower.step(system(System::Stop), at(now + 0.03), &mut |_| {});
    assert_eq!(follower.error(), None);
    follower.steer(Some(Beat::new(2.4)), at(now + 0.04), &mut |_| {});
    assert_eq!(follower.error(), None);
}

/// **A transport beat that is not a number is unreadable**: steering by NaN or
/// infinity reads no error, and the tempo the follower goes on announcing is the
/// fit, 120 bpm, untrimmed.
#[test]
fn a_non_finite_transport_beat_reads_no_error_and_leaves_the_tempo_untrimmed() {
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut follower = BeatClockFollower::new();
        let p = 60.0 / (120.0 * 24.0);
        let mut tempos = Vec::new();
        follower.step(system(System::Start), at(0.0), &mut |_| {});
        for k in 1..=60u32 {
            follower.step(
                system(System::TimingClock),
                at(f64::from(k - 1) * p),
                &mut |e| tempos.push(e),
            );
        }
        let now = 59.0 * p + 0.005;
        assert!(now > BeatClockFollower::SETTLE.get());
        follower.steer(Some(Beat::new(bad)), at(now), &mut |_| {});
        assert_eq!(follower.error(), None, "{bad}");
        for k in 61..=200u32 {
            follower.step(
                system(System::TimingClock),
                at(f64::from(k - 1) * p),
                &mut |e| tempos.push(e),
            );
        }
        let announced: Vec<f64> = tempos
            .iter()
            .filter_map(|e| match e {
                BeatClockEvent::Tempo { tempo, .. } => Some(tempo.get()),
                _ => None,
            })
            .collect();
        assert!(!announced.is_empty());
        assert!(
            announced.iter().all(|t| (t - 120.0).abs() < 1e-6),
            "{announced:?}"
        );
    }
}
