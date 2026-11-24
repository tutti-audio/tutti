//! An MTC follower: the slip rule, a source off its nominal speed, and a source
//! running backward.
//!
//! The oracle is arithmetic over the spec's rules (M1, MIDI Time Code, Quarter
//! Frame Message Implementation): a cycle names the frame that starts at the
//! arrival of its piece 0 and takes two frames to send; pieces `7` to `0` mean
//! time runs backward. Every figure below is a formula of `SLIP_FRAMES`,
//! `SETTLE` and the frame rate (25 fps, rate code 1).

use tutti_midi_runtime::{ClockEvent, MtcDecoder, MtcDirection, MtcEvent, MtcFollower};
use tutti_ump::{Message, MidiGroup, System, U7, Ump};
use tutti_units::Seconds;

const FPS: f64 = 25.0;

fn at(seconds: f64) -> Seconds {
    Seconds::new(seconds)
}

fn quarter(piece: u8, nibble: u8) -> Ump {
    Ump::from(Message::System(
        MidiGroup::FIRST,
        System::TimeCode(U7::of(piece << 4 | nibble)),
    ))
}

/// The nibbles of the cycle naming frame `n` of the first hours, 25 fps.
fn nibbles(n: u64) -> [u8; 8] {
    let (s, f) = ((n / 25) as u8, (n % 25) as u8);
    [f & 0xF, f >> 4, s & 0xF, s >> 4, 0, 0, 0, 1 << 1]
}

/// Cycle `c` (frames `2c`, `2c + 1`) as `(piece, arrival)` in the order a source
/// running at `speed` times nominal sends them: piece `k` is `k / 4` frames after
/// piece 0, and piece 0 is at `2c` frames of source time.
fn forward(c: u64, speed: f64) -> Vec<(Ump, f64)> {
    nibbles(2 * c)
        .iter()
        .enumerate()
        .map(|(k, n)| {
            (
                quarter(k as u8, *n),
                (2 * c) as f64 / (FPS * speed) + k as f64 / (4.0 * FPS * speed),
            )
        })
        .collect()
}

/// A follower, what it said, and a transport that obeys each locate at once and
/// then runs at nominal speed.
struct Rig {
    follower: MtcFollower,
    heard: Vec<(f64, MtcEvent)>,
    /// `(position, as of)` of the transport's last locate.
    transport: Option<(f64, f64)>,
}

impl Rig {
    fn new() -> Self {
        Self {
            follower: MtcFollower::new(),
            heard: Vec::new(),
            transport: None,
        }
    }

    fn take(&mut self, now: f64, events: Vec<MtcEvent>) {
        for e in events {
            if let MtcEvent::Clock(ClockEvent::Locate { position: p, at }) = e {
                self.transport = Some((p.get(), at.get()));
            }
            self.heard.push((now, e));
        }
    }

    fn feed(&mut self, ump: Ump, now: f64) {
        let mut events = Vec::new();
        self.follower.step(ump, at(now), &mut |e| events.push(e));
        self.take(now, events);
    }

    /// One control tick at `now`: a poll and a steer against the transport.
    fn tick(&mut self, now: f64) {
        let mut events = Vec::new();
        self.follower.poll(at(now), &mut |e| events.push(e));
        let position = self.transport.map(|(p, since)| at(p + now - since));
        self.follower
            .steer(position, at(now), &mut |e| events.push(e));
        self.take(now, events);
    }

    fn slips(&self) -> Vec<(f64, f64)> {
        self.heard
            .iter()
            .filter_map(|(now, e)| match e {
                MtcEvent::Clock(ClockEvent::Slipped { error }) => Some((*now, error.get())),
                _ => None,
            })
            .collect()
    }
}

/// Runs a source at `speed` for `seconds` against a transport at nominal speed,
/// a control tick every 10 ms; the largest error the follower read.
fn run_at_speed(rig: &mut Rig, speed: f64, seconds: f64) -> f64 {
    let mut pieces: Vec<(Ump, f64)> = (0..(seconds * FPS * speed / 2.0) as u64)
        .flat_map(|c| forward(c, speed))
        .collect();
    pieces.reverse();
    let mut worst = 0.0f64;
    for step in 0..(seconds * 100.0) as u64 {
        let now = step as f64 * 0.01;
        while pieces.last().is_some_and(|(_, t)| *t <= now) {
            let (ump, t) = pieces.pop().expect("one");
            rig.feed(ump, t);
        }
        rig.tick(now);
        if let Some(e) = rig.follower.error() {
            worst = worst.max(e.get().abs());
        }
    }
    worst
}

/// **A source off nominal speed is located back when it is `SLIP_FRAMES`
/// away.** The transport has no rate, so a source `d` fast leaves it `d` seconds
/// further behind each second: the error reaches the threshold `2 / 25 = 0.08 s`
/// at `0.08 / d` after the lock's locate (4 s for `d = 0.02`, 8 s for `d = -0.01`),
/// read at the next control tick (10 ms) and with the extrapolation from the last
/// cycle, which falls short by `d` times the time since its piece 0 (at most a
/// cycle, 80 ms). The error read never exceeds the threshold by more than `d` times
/// a cycle and a tick, and the next slip is a threshold's worth of time on.
#[test]
fn a_source_off_speed_is_located_back_at_the_slip_threshold() {
    let threshold = MtcFollower::SLIP_FRAMES as f64 / FPS;
    assert!((threshold - 0.08).abs() < 1e-12);
    for (speed_error, first) in [(0.02, 4.0), (-0.01, 8.0)] {
        let mut rig = Rig::new();
        let worst = run_at_speed(&mut rig, 1.0 + speed_error, 20.0);
        let slips = rig.slips();
        assert!(!slips.is_empty(), "{speed_error}: no slip");
        // The lock's locate is at the third cycle's piece 0, 0.16 s in, at the
        // source's own speed; the error is zero there.
        let lock = 4.0 / (FPS * (1.0 + speed_error));
        let expected = lock + first;
        let (now, error) = slips[0];
        assert!(
            now >= expected - 1e-9 && now <= expected + 0.1,
            "{speed_error}: first slip at {now}, expected near {expected}"
        );
        assert_eq!(error.signum(), speed_error.signum(), "{speed_error}");
        assert!(
            error.abs() > threshold && error.abs() < threshold + speed_error.abs() * 0.1,
            "{speed_error}: slipped by {error}"
        );
        assert!(
            worst <= threshold + speed_error.abs() * (0.08 + 0.01) + 1e-9,
            "{speed_error}: read {worst}"
        );
        // One locate per slip: the follower holds off SETTLE after each.
        let locates = rig
            .heard
            .iter()
            .filter(|(now, e)| {
                matches!(e, MtcEvent::Clock(ClockEvent::Locate { .. })) && *now > lock + 0.5
            })
            .count();
        assert_eq!(locates, slips.len(), "{speed_error}");
        for pair in slips.windows(2) {
            let spacing = pair[1].0 - pair[0].0;
            assert!(
                spacing >= MtcFollower::SETTLE.get() && (spacing - first).abs() < 0.2,
                "{speed_error}: slips {spacing} s apart"
            );
        }
    }
}

/// **A transport held 0.5 s behind slips once, then waits `SETTLE`.** The model
/// transport ignores locates (it stays behind), so each error read is the source
/// less the transport: 0.5 s at nominal speed. The first read is at the first
/// tick at or past the lock's locate plus `SETTLE`; the next, `SETTLE` after the
/// slip's own locate.
#[test]
fn a_slip_locates_once_and_holds_off_for_settle() {
    let mut rig = Rig::new();
    let mut pieces: Vec<(Ump, f64)> = (0..250).flat_map(|c| forward(c, 1.0)).collect();
    pieces.reverse();
    for step in 0..1000u64 {
        let now = step as f64 * 0.01;
        while pieces.last().is_some_and(|(_, t)| *t <= now) {
            let (ump, t) = pieces.pop().expect("one");
            rig.feed(ump, t);
        }
        let mut events = Vec::new();
        rig.follower.poll(at(now), &mut |e| events.push(e));
        // The transport is the source's nominal clock less 0.5 s: it never obeys
        // a locate.
        rig.follower.steer(Some(at(now - 0.5)), at(now), &mut |e| {
            events.push(e);
        });
        rig.take(now, events);
    }
    let slips = rig.slips();
    assert!(slips.len() >= 3, "{slips:?}");
    // The error is the source's time (frame 4 at 0.16 s of source time, so
    // `now`) less the transport's `now - 0.5`: 0.5 within the extrapolation.
    for (now, error) in &slips {
        assert!((error - 0.5).abs() < 1e-9, "{now}: {error}");
    }
    let lock = 4.0 / FPS;
    assert!(
        slips[0].0 >= lock + MtcFollower::SETTLE.get()
            && slips[0].0 < lock + MtcFollower::SETTLE.get() + 0.011,
        "first slip at {}",
        slips[0].0
    );
    for pair in slips.windows(2) {
        let spacing = pair[1].0 - pair[0].0;
        assert!(
            spacing >= MtcFollower::SETTLE.get() && spacing < MtcFollower::SETTLE.get() + 0.011,
            "slips {spacing} s apart"
        );
    }
}

/// **The decoder reads direction from `DIRECTION_STEPS` steps in a row**:
/// ascending is forward, descending is reverse (M1 MTC, Quarter Frame Message
/// Implementation), and one step the other way, or a gap, is neither. Three
/// steps are four pieces: `5, 4, 3` is two steps and says nothing, `5, 4, 3, 2`
/// is three.
#[test]
fn pieces_in_descending_order_are_reverse() {
    let mut decoder = MtcDecoder::new();
    assert_eq!(MtcDecoder::DIRECTION_STEPS, 3);
    assert_eq!(decoder.direction(), None);
    for (piece, expected) in [
        (5, None),
        (4, None),
        (3, None),
        (2, Some(MtcDirection::Reverse)),
        (1, Some(MtcDirection::Reverse)),
        // A gap, then a run up from where it lands.
        (6, None),
        (7, None),
        (0, None),
        (1, Some(MtcDirection::Forward)),
    ] {
        decoder.feed(&quarter(piece, 0));
        assert_eq!(decoder.direction(), expected, "after piece {piece}");
    }
}

/// Feeds `pieces` to a locked forward follower; what it said.
fn glitch(pieces: &[u8]) -> (Rig, Vec<MtcEvent>) {
    let mut rig = Rig::new();
    for c in 0..5 {
        for (ump, t) in forward(c, 1.0) {
            rig.feed(ump, t);
        }
    }
    assert!(rig.follower.locked());
    rig.heard.clear();
    let mut t = 0.4;
    for piece in pieces {
        rig.feed(quarter(*piece, 0), t);
        t += 1.0 / (4.0 * FPS);
    }
    let events = rig.heard.iter().map(|(_, e)| *e).collect();
    (rig, events)
}

/// **One glitch is not a reverse.** Pieces `2, 1, 3` have one step down, and a
/// burst loss that jumps `0` to `7` has one: neither makes `DIRECTION_STEPS`
/// steps down, so neither says `Reversed`, stops the transport or unlocks it.
#[test]
fn a_lone_out_of_order_piece_or_a_jump_does_not_reverse() {
    for pieces in [
        [0u8, 1, 2, 1, 3, 4].as_slice(),
        [6, 7, 0, 7, 0, 1].as_slice(),
    ] {
        let (rig, events) = glitch(pieces);
        assert!(rig.follower.locked(), "{pieces:?}");
        assert!(
            !events.iter().any(|e| matches!(
                e,
                MtcEvent::Reversed
                    | MtcEvent::Clock(ClockEvent::Stop { .. })
                    | MtcEvent::Clock(ClockEvent::Unlocked)
            )),
            "{pieces:?}: {events:?}"
        );
    }
}

/// **A descending run does reverse**: `7, 6, 5, 4` is three steps down, said at
/// the fourth piece.
#[test]
fn a_descending_run_reverses_at_its_third_step() {
    let (rig, events) = glitch(&[7, 6, 5, 4]);
    assert!(!rig.follower.locked());
    assert_eq!(events[0], MtcEvent::Reversed);
}

/// **A reverse cycle names the frame at its last piece, with no offset.** Pieces
/// `7` to `0` of frame 42 (`00:00:01:17`) assemble at piece 0, which "falls on
/// the boundary of the frame it represents": `timecode()` is frame 42, where a
/// forward cycle of the same pieces reads frame 44.
#[test]
fn a_reverse_cycle_is_frame_n_and_a_forward_one_is_two_on() {
    let pieces = nibbles(42);
    let mut reverse = MtcDecoder::new();
    for k in (0..8).rev() {
        reverse.feed(&quarter(k as u8, pieces[k]));
    }
    let cycle = reverse.take_cycle().expect("a cycle at piece 0");
    assert_eq!(cycle.direction, MtcDirection::Reverse);
    assert_eq!(cycle.frame.frame_count(), 42);
    assert_eq!(reverse.timecode().map(|t| t.frame_count()), Some(42));
    assert_eq!(reverse.take_cycle(), None, "taken once");

    let mut forward = MtcDecoder::new();
    for (k, n) in pieces.iter().enumerate() {
        forward.feed(&quarter(k as u8, *n));
    }
    assert_eq!(
        forward.take_cycle().map(|c| c.frame.frame_count()),
        Some(42)
    );
    assert_eq!(forward.timecode().map(|t| t.frame_count()), Some(44));
}

/// **A source that runs backward stops the transport, once, and a source that
/// runs forward again locks afresh.** Five forward cycles lock and play; pieces
/// 7, 6, 5, 4 of the next cycle are three steps down: `Reversed`, `Stop` and `Unlocked`, in
/// that order and at the piece that showed it (4, the third step down from 7).
/// The descending cycles that follow say nothing and locate nothing. Forward
/// cycles then lock after `LOCK_CYCLES`, with a locate and a play.
#[test]
fn a_reversed_source_stops_the_transport_and_forward_locks_again() {
    let mut rig = Rig::new();
    for c in 0..5 {
        for (ump, t) in forward(c, 1.0) {
            rig.feed(ump, t);
        }
    }
    assert!(rig.follower.locked());
    rig.heard.clear();
    // Backward from frame 40: pieces 7 to 0 of frames 40, 38, 36.
    let mut t = 0.4;
    let quarter_frame = 1.0 / (4.0 * FPS);
    let mut stopped_at = None;
    for frame in [40, 38, 36] {
        let n = nibbles(frame);
        for k in (0..8).rev() {
            rig.feed(quarter(k as u8, n[k]), t);
            if frame == 40 && k == 4 {
                stopped_at = Some(t);
            }
            t += quarter_frame;
        }
    }
    let events: Vec<MtcEvent> = rig.heard.iter().map(|(_, e)| *e).collect();
    assert_eq!(
        events,
        [
            MtcEvent::Reversed,
            MtcEvent::Clock(ClockEvent::Stop {
                at: at(stopped_at.expect("fed"))
            }),
            MtcEvent::Clock(ClockEvent::Unlocked)
        ]
    );
    assert!(!rig.follower.locked());
    rig.heard.clear();
    // Forward again, from frame 100: three cycles lock.
    let base = t + 1.0;
    for c in 50..50 + MtcFollower::LOCK_CYCLES as u64 {
        for (ump, at_) in forward(c, 1.0) {
            rig.feed(ump, base + at_);
        }
    }
    let events: Vec<MtcEvent> = rig.heard.iter().map(|(_, e)| *e).collect();
    assert_eq!(events.len(), 3, "{events:?}");
    assert_eq!(events[0], MtcEvent::Clock(ClockEvent::Locked));
    // The third cycle names frame 104 (`2 * 52`) at its piece 0, 4 / 25 s into
    // the run.
    assert_eq!(
        events[1],
        MtcEvent::Clock(ClockEvent::Locate {
            position: at(104.0 / FPS),
            at: at(base + 104.0 / FPS),
        })
    );
    assert_eq!(events[2], MtcEvent::Clock(ClockEvent::Play));
}

/// **A discarded cycle is no jump; a real jump still locates.** A locked, rolling
/// follower is fed cycles of frames 10, 12, then (the cycle at 14 lost to a gap)
/// 16: the step is 4 frames, two whole cycles, within `DROPOUT` (0.25 s is
/// 6.25 frames at 25 fps), so no `Locate`. A cycle at frame 17 is 1 frame on, not
/// whole cycles, and one at 200 is past the window: each locates.
#[test]
fn a_discarded_cycle_is_not_a_jump_and_a_real_jump_locates() {
    let locates = |rig: &Rig| {
        rig.heard
            .iter()
            .filter(|(_, e)| matches!(e, MtcEvent::Clock(ClockEvent::Locate { .. })))
            .count()
    };
    let feed_cycle = |rig: &mut Rig, frame: u64, start: f64| {
        for (k, n) in nibbles(frame).iter().enumerate() {
            rig.feed(quarter(k as u8, *n), start + k as f64 / (4.0 * FPS));
        }
    };
    let mut rig = Rig::new();
    for c in 0..5 {
        for (ump, t) in forward(c, 1.0) {
            rig.feed(ump, t);
        }
    }
    assert!(rig.follower.locked());
    rig.heard.clear();
    // Frames 8 was the last cycle (c = 4); 12 follows with 10 lost.
    feed_cycle(&mut rig, 12, 0.5);
    assert_eq!(locates(&rig), 0, "a discarded cycle is two whole cycles");
    feed_cycle(&mut rig, 14, 0.58);
    assert_eq!(locates(&rig), 0);
    feed_cycle(&mut rig, 17, 0.66);
    assert_eq!(locates(&rig), 1, "one frame is not whole cycles");
    feed_cycle(&mut rig, 200, 0.74);
    assert_eq!(locates(&rig), 2, "past the dropout window");
}
