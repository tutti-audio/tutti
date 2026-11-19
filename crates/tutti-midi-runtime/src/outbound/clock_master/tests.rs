use super::*;
use crate::{ClockTransportState, MidiClockDecoder, MtcDecoder};
use atomic_float::AtomicF64;
use core::num::NonZeroU8;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool as StdAtomicBool, Ordering};
use tutti_graph::UmpAt;
use tutti_midi_protocol::{QuarterFramePiece, SmpteFrameRate};
use tutti_music::LoopRange;
use tutti_music::MeterChange;
use tutti_transport::ProjectMeter;
use tutti_ump::{BarAccents, Flex, Message, Metronome, MidiGroup, System};
use tutti_units::{Bpm, Seconds};

/// The System message a packet carries.
fn system(ump: Ump) -> Option<System> {
    match ump.decode() {
        Ok(Message::System(_, system)) => Some(system),
        _ => None,
    }
}

/// Test transport: tempo + beat + playing under a switch.
struct TestTransport {
    beat: AtomicF64,
    tempo: Bpm,
    playing: StdAtomicBool,
    looping: std::sync::Mutex<Option<tutti_music::LoopRange>>,
}

impl TestTransport {
    fn new(tempo: Bpm) -> Self {
        Self {
            beat: AtomicF64::new(0.0),
            tempo,
            playing: StdAtomicBool::new(false),
            looping: std::sync::Mutex::new(None),
        }
    }
    fn set_loop(&self, start: f64, end: f64) {
        *self.looping.lock().unwrap() =
            Some(LoopRange::new(Beat::new(start), Beat::new(end)).expect("a loop"));
    }
    fn set_beat(&self, b: f64) {
        self.beat.store(b, Ordering::Release);
    }
    fn set_playing(&self, p: bool) {
        self.playing.store(p, Ordering::Release);
    }
    fn now(&self) -> TransportPosition {
        TransportPosition::new(
            self.playing.load(Ordering::Acquire),
            self.tempo,
            Beat::new(self.beat.load(Ordering::Acquire)),
            *self.looping.lock().unwrap(),
        )
    }
}

/// A master ticked from a [`TestTransport`], emitting into a mailbox, sending
/// what `messages` says.
struct Rig {
    cm: ClockMaster,
    messages: SyncMessages,
    meter: tutti_rt::Reader<tutti_music::MeterMap>,
    transport: Arc<TestTransport>,
    out: Collected,
}

impl Rig {
    fn set_sample_rate(&mut self, rate: SampleRate) {
        self.cm.set_sample_rate(rate);
    }

    /// The Beat Clock on or off, the rest of what is sent as it was.
    fn set_enabled(&mut self, on: bool) {
        self.messages.clock = on;
    }

    /// MTC on or off, the rest of what is sent as it was.
    fn set_send_mtc(&mut self, on: bool) {
        self.messages.mtc = on;
    }

    /// The frame rate time codes are stated at.
    fn set_mtc_fps(&mut self, rate: MtcRate) {
        self.messages.rate = rate;
    }

    fn tick(&mut self, block_size: usize) {
        let loan = self.messages.flex.then(|| self.meter.lend());
        self.cm.tick(
            self.messages,
            loan.as_deref(),
            &self.transport.now(),
            Samples::new(block_size),
            &mut |e| {
                self.out.0.lock().unwrap().push(e);
            },
        );
    }
}

fn master(tempo: Bpm, sample_rate: impl Into<SampleRate>) -> (Rig, Arc<TestTransport>, Collected) {
    let transport = Arc::new(TestTransport::new(tempo));
    let receiver = Collected::default();
    let out = receiver.clone();
    let cm = ClockMaster::new(MidiGroup::FIRST, sample_rate.into());
    let rig = Rig {
        cm,
        messages: SyncMessages {
            clock: true,
            ..SyncMessages::OFF
        },
        meter: ProjectMeter::default().reader(),
        transport: Arc::clone(&transport),
        out,
    };
    (rig, transport, receiver)
}

/// What the rig's master emitted, in order.
#[derive(Clone, Default)]
struct Collected(Arc<std::sync::Mutex<Vec<UmpAt>>>);

/// Takes everything emitted so far.
fn drain_all(receiver: &Collected) -> Vec<UmpAt> {
    std::mem::take(&mut *receiver.0.lock().unwrap())
}

fn is_status(ev: &UmpAt, status: u8) -> bool {
    let w0 = ev.1.0[0];
    (w0 >> 28) & 0x0F == 0x1 && ((w0 >> 16) & 0xFF) as u8 == status
}

#[test]
fn emits_24_ticks_per_beat_at_120bpm() {
    let sr = 48_000.0;
    let (mut cm, transport, cons) = master(Bpm::of(120.0), SampleRate::of(sr));
    transport.set_playing(true);
    // 120 BPM → 0.5s/beat → 24000 samples/beat. Process one beat's worth of
    // audio in blocks of 512 and count 0xF8 ticks.
    let samples_per_beat = sr * 0.5;
    let block = 512usize;
    let blocks = (samples_per_beat / block as f64).ceil() as usize;

    let mut clock_ticks = 0;
    let mut beat = 0.0f64;
    let beats_per_sample = 120.0 / 60.0 / sr;
    for _ in 0..blocks {
        transport.set_beat(beat);
        cm.tick(block);
        beat += block as f64 * beats_per_sample;
    }
    let events = drain_all(&cons);
    for ev in &events {
        if is_status(ev, 0xF8) {
            clock_ticks += 1;
        }
    }
    // Over ~1 beat, 24 ticks are expected (allow ±1 for boundary rounding).
    assert!(
        (clock_ticks as i64 - 24).abs() <= 1,
        "expected ~24 clock ticks over one beat, got {clock_ticks}"
    );
}

#[test]
fn play_at_zero_emits_start_not_continue() {
    let (mut cm, transport, cons) = master(Bpm::of(120.0), SampleRate::SR_48K);
    transport.set_beat(0.0);
    transport.set_playing(true);
    cm.tick(512);
    let events = drain_all(&cons);
    assert!(
        events.iter().any(|e| is_status(e, 0xFA)),
        "expected Start (0xFA)"
    );
    assert!(
        !events.iter().any(|e| is_status(e, 0xFB)),
        "no Continue at beat 0"
    );
}

#[test]
fn play_mid_song_emits_continue_and_song_position() {
    let (mut cm, transport, cons) = master(Bpm::of(120.0), SampleRate::SR_48K);
    transport.set_beat(8.0); // 2 bars in
    transport.set_playing(true);
    cm.tick(512);
    let events = drain_all(&cons);
    assert!(
        events.iter().any(|e| is_status(e, 0xFB)),
        "expected Continue (0xFB)"
    );
    assert!(
        events.iter().any(|e| is_status(e, 0xF2)),
        "expected Song Position (0xF2)"
    );
    assert!(
        !events.iter().any(|e| is_status(e, 0xFA)),
        "no Start mid-song"
    );
}

#[test]
fn stop_emits_stop_message() {
    let (mut cm, transport, cons) = master(Bpm::of(120.0), SampleRate::SR_48K);
    transport.set_playing(true);
    cm.tick(512);
    let _ = drain_all(&cons);
    transport.set_playing(false);
    cm.tick(512);
    let events = drain_all(&cons);
    assert!(
        events.iter().any(|e| is_status(e, 0xFC)),
        "expected Stop (0xFC)"
    );
}

#[test]
fn disabled_master_emits_nothing() {
    let (mut cm, transport, cons) = master(Bpm::of(120.0), SampleRate::SR_48K);
    cm.set_enabled(false);
    transport.set_playing(true);
    cm.tick(512);
    assert!(
        drain_all(&cons).is_empty(),
        "disabled master must be silent"
    );
}

/// Ticks land on their frames, in order, and a start on a tick boundary
/// sends that boundary's tick on frame 0.
///
/// This pinned two ticks (1000, 2000) until the start tick was fixed:
/// MIDI 1.0 (MMA, System Real Time Messages, Start) has a receiver
/// begin on the first Timing Clock after Start, so the clock of beat 0
/// must be sent, or every receiver runs one tick behind.
#[test]
fn ticks_are_sample_accurate_within_block() {
    // A block that spans three clock ticks, the first on its first
    // frame, places them at distinct, ordered frame offsets.
    let sr = 48_000.0;
    let (mut cm, transport, cons) = master(Bpm::of(120.0), SampleRate::of(sr));
    transport.set_playing(true);
    transport.set_beat(0.0);
    // One beat = 24000 samples; 1/24 beat = 1000 samples. A 2500-sample
    // block starting at beat 0 covers tick boundaries at 0, 1000 and 2000.
    cm.tick(2500);
    let events: Vec<_> = drain_all(&cons)
        .into_iter()
        .filter(|e| is_status(e, 0xF8))
        .collect();
    assert_eq!(events.len(), 3, "expected 3 ticks in the block");
    assert!(
        events[0].0.get() < events[1].0.get() && events[1].0.get() < events[2].0.get(),
        "ticks ordered"
    );
    assert_eq!(events[0].0.get(), 0, "the start beat's tick");
    assert!((events[1].0.get() as i64 - 1000).abs() < 4);
    assert!((events[2].0.get() as i64 - 2000).abs() < 4);
}

/// Where the first Timing Clock goes, and in what order, for a block that
/// does not continue the previous one: the transport message(s), then an
/// F8 on frame 0 when the beat sits on a tick boundary, else the first F8
/// on the next boundary.
fn first_clock(events: &[UmpAt]) -> (usize, u32) {
    let i = events
        .iter()
        .position(|e| is_status(e, 0xF8))
        .expect("a timing clock");
    (i, events[i].0.get())
}

/// Start at beat 0, Continue at an on-tick beat, and a locate while
/// playing to an on-tick beat: each sends an F8 on frame 0, queued after
/// its Start / Continue / Song Position. MIDI 1.0 (MMA, System Real
/// Time Messages, Start, Continue): a receiver begins on the first
/// Timing Clock after Start / Continue.
#[test]
fn an_on_tick_start_continue_or_locate_clocks_its_first_frame() {
    let sr = 48_000.0;
    // Start at beat 0.
    let (mut cm, transport, cons) = master(Bpm::of(120.0), SampleRate::of(sr));
    transport.set_beat(0.0);
    transport.set_playing(true);
    cm.tick(512);
    let events = drain_all(&cons);
    let start = events
        .iter()
        .position(|e| is_status(e, 0xFA))
        .expect("Start");
    let (clock, offset) = first_clock(&events);
    assert_eq!(offset, 0, "Start at beat 0: F8 on frame 0");
    assert!(start < clock, "the F8 follows Start in the queue");

    // Continue at beat 8 (tick 192).
    let (mut cm, transport, cons) = master(Bpm::of(120.0), SampleRate::of(sr));
    transport.set_beat(8.0);
    transport.set_playing(true);
    cm.tick(512);
    let events = drain_all(&cons);
    let spp = events.iter().position(|e| is_status(e, 0xF2)).expect("SPP");
    let cont = events
        .iter()
        .position(|e| is_status(e, 0xFB))
        .expect("Continue");
    let (clock, offset) = first_clock(&events);
    assert_eq!(offset, 0, "Continue on a tick: F8 on frame 0");
    assert!(
        spp < clock && cont < clock,
        "the F8 follows SPP and Continue"
    );

    // A locate while playing to beat 4 (tick 96).
    let (mut cm, transport, cons) = master(Bpm::of(120.0), SampleRate::of(sr));
    transport.set_beat(0.0);
    transport.set_playing(true);
    cm.tick(512);
    let _ = drain_all(&cons);
    transport.set_beat(4.0);
    cm.tick(512);
    let events = drain_all(&cons);
    let spp = events.iter().position(|e| is_status(e, 0xF2)).expect("SPP");
    let (clock, offset) = first_clock(&events);
    assert_eq!(offset, 0, "a locate on a tick: F8 on frame 0");
    assert!(spp < clock, "the F8 follows Song Position");
}

/// Off a tick boundary, the first F8 after a locate or a Continue is the
/// next boundary's, on its frame; nothing goes out on frame 0.
#[test]
fn an_off_tick_locate_clocks_the_next_boundary() {
    let sr = 48_000.0;
    // Half a tick past beat 4: 500 frames to the next boundary at 120
    // BPM / 48 kHz (a tick is 1000 frames).
    let off = 4.0 + 0.5 / f64::from(System::TIMING_CLOCKS_PER_QUARTER);
    let (mut cm, transport, cons) = master(Bpm::of(120.0), SampleRate::of(sr));
    transport.set_beat(0.0);
    transport.set_playing(true);
    cm.tick(512);
    let _ = drain_all(&cons);
    transport.set_beat(off);
    cm.tick(2048);
    let events = drain_all(&cons);
    assert!(events.iter().any(|e| is_status(e, 0xF2)), "a locate");
    let (_, offset) = first_clock(&events);
    assert_eq!(offset, 500, "the next boundary's frame");

    // The same off-tick beat as a Continue.
    let (mut cm, transport, cons) = master(Bpm::of(120.0), SampleRate::of(sr));
    transport.set_beat(off);
    transport.set_playing(true);
    cm.tick(2048);
    let (_, offset) = first_clock(&drain_all(&cons));
    assert_eq!(offset, 500, "Continue off a tick: the next boundary");
}

/// Run `blocks` blocks of `block` frames at `rate`, the transport rolling
/// at 120 BPM from `*beat`, and return every event with its absolute
/// frame (counted from `*frame`). Advances both.
fn run_at(
    cm: &mut Rig,
    transport: &TestTransport,
    cons: &Collected,
    rate: f64,
    (block, blocks): (usize, usize),
    (beat, frame): (&mut f64, &mut u64),
) -> Vec<(UmpAt, u64)> {
    let mut at = Vec::new();
    for _ in 0..blocks {
        transport.set_beat(*beat);
        cm.tick(block);
        at.extend(
            drain_all(cons)
                .into_iter()
                .map(|e| (e, *frame + u64::from(e.0.get()))),
        );
        *beat += block as f64 * 2.0 / rate;
        *frame += block as u64;
    }
    at
}

/// The frames of the events in `events` with `status`.
fn of(events: &[(UmpAt, u64)], status: u8) -> Vec<u64> {
    events
        .iter()
        .filter(|(e, _)| is_status(e, status))
        .map(|&(_, at)| at)
        .collect()
}

/// Mean spacing, in frames, of the events at `at`.
fn spacing(at: &[u64]) -> f64 {
    assert!(at.len() > 10, "events to measure, got {}", at.len());
    (at[at.len() - 1] - at[0]) as f64 / (at.len() - 1) as f64
}

/// **A rate change moves the tick spacing to the new rate's frames.** At
/// 120 BPM a 24-PPQN tick is 1/48 s: 918.75 frames at 44.1 kHz, 1 000 at
/// 48 kHz. A master built at 44.1 kHz and re-rated to 48 kHz (a device
/// restart) must space its ticks 1 000 frames apart from the next block,
/// or the receiving gear runs ~8.8% fast. (Offsets are whole frames, so
/// each tick sits up to one frame early; the mean over ~48 ticks is within
/// 0.05 of the exact spacing.)
#[test]
fn a_rate_change_moves_the_tick_spacing() {
    let (mut cm, transport, cons) = master(Bpm::of(120.0), SampleRate::SR_44K1);
    transport.set_playing(true);
    let (mut beat, mut frame) = (0.0, 0);
    let run = (&mut beat, &mut frame);
    let before = of(
        &run_at(&mut cm, &transport, &cons, 44_100.0, (441, 100), run),
        0xF8,
    );
    assert!(
        (spacing(&before) - 918.75).abs() < 0.05,
        "{}",
        spacing(&before)
    );

    cm.set_sample_rate(SampleRate::SR_48K);
    assert_eq!(cm.cm.sample_rate, SampleRate::SR_48K);
    let run = (&mut beat, &mut frame);
    let after = of(
        &run_at(&mut cm, &transport, &cons, 48_000.0, (480, 100), run),
        0xF8,
    );
    assert!(
        (spacing(&after) - 1_000.0).abs() < 0.05,
        "{}",
        spacing(&after)
    );
}

/// **The MTC quarter-frame due across a rate change lands on its
/// wall-clock time.** At 25 fps a quarter-frame is 1/100 s: 441 frames at
/// 44.1 kHz, 480 at 48 kHz. After 20 blocks of 512 at 44.1 kHz (10 240
/// frames) the last quarter-frame was at 10 143 and the next is due 441
/// frames later, 344 old frames after the restart: 374.4 new frames, and
/// 480 apart from there. And the restart is not a locate: no Song
/// Position goes out, and the phase is not reset.
#[test]
fn a_rate_change_keeps_the_mtc_quarter_frame_on_time() {
    let (mut cm, transport, cons) = master(Bpm::of(120.0), SampleRate::SR_44K1);
    cm.set_send_mtc(true);
    cm.set_mtc_fps(MtcRate::Fps25);
    transport.set_playing(true);
    let (mut beat, mut frame) = (0.0, 0);
    let run = (&mut beat, &mut frame);
    let before = of(
        &run_at(&mut cm, &transport, &cons, 44_100.0, (512, 20), run),
        0xF1,
    );
    assert_eq!(
        before.last(),
        Some(&10_143),
        "setup: every 441 frames from 0"
    );

    cm.set_sample_rate(SampleRate::SR_48K);
    let run = (&mut beat, &mut frame);
    let events = run_at(&mut cm, &transport, &cons, 48_000.0, (512, 20), run);
    assert_eq!(
        of(&events, 0xF2),
        Vec::<u64>::new(),
        "the restart is not a seek"
    );
    let after = of(&events, 0xF1);
    assert_eq!(
        after[0],
        10_240 + 375,
        "344 old frames are 374.4 new ones, which is reached on frame 375"
    );
    assert!(
        (spacing(&after) - 480.0).abs() < 0.05,
        "{}",
        spacing(&after)
    );
}

/// **The MTC grid is closed form, and a quarter-frame is never early.**
/// Quarter-frame `k` goes out on the first frame at or after
/// `k × rate / (4 × fps)`, however many blocks it took to get there: at 29.97
/// fps and 44.1 kHz a quarter-frame is 367.867… frames,
/// which binary cannot represent, and a carried phase that adds it
/// block after block drifts off the grid.
#[test]
fn the_mtc_grid_is_closed_form_and_never_early() {
    let rate = 44_100.0;
    let fps = MtcRate::Fps2997Df;
    let (mut cm, transport, cons) = master(Bpm::of(120.0), SampleRate::of(rate));
    cm.set_send_mtc(true);
    cm.set_mtc_fps(fps);
    transport.set_playing(true);
    let (mut beat, mut frame) = (0.0, 0);
    let events = run_at(
        &mut cm,
        &transport,
        &cons,
        rate,
        (512, 6_000),
        (&mut beat, &mut frame),
    );
    let quarters = of(&events, 0xF1);
    assert!(quarters.len() > 8_000, "{} quarter-frames", quarters.len());
    for (k, &at) in quarters.iter().enumerate() {
        let (num, den) = SmpteFrameRate::from(fps).ratio();
        let time = (k as f64 * rate * den as f64) / (num as f64 * 4.0);
        let at = at as f64;
        assert!(
            at >= time - 1e-6 && at - time < 1.0 + 1e-6,
            "quarter-frame {k} due at {time} placed at {at}: not the first frame at or after"
        );
    }
}

#[test]
fn generated_clock_round_trips_through_decoder() {
    // The strongest check: feed this master's own output into the decoder and confirm
    // it recovers the tempo and beat — the in-repo loopback from the plan.
    let sr = 48_000.0;
    let (mut cm, transport, cons) = master(Bpm::of(120.0), SampleRate::of(sr));
    transport.set_playing(true);

    let block = 256usize;
    let beats_per_sample = 120.0 / 60.0 / sr;
    let mut beat = 0.0f64;
    let mut sample_clock = 0u64;

    let mut decoder = MidiClockDecoder::new();
    // Run ~4 beats so the decoder's tempo window fills.
    for _ in 0..((sr * 2.0 / block as f64) as usize) {
        transport.set_beat(beat);
        cm.tick(block);
        for ev in drain_all(&cons) {
            let at = Seconds::new((sample_clock + ev.0.get() as u64) as f64 / sr);
            decoder.feed(&ev.1, at);
        }
        beat += block as f64 * beats_per_sample;
        sample_clock += block as u64;
    }

    assert_eq!(decoder.transport_state(), ClockTransportState::Playing);
    let tempo = decoder.tempo_bpm().expect("decoder derived a tempo");
    assert!(
        !tempo.differs_from(Bpm::of(120.0), Bpm::of(2.0)),
        "round-trip tempo ~120, got {tempo:?}"
    );
}

#[test]
fn mtc_quarter_frames_round_trip_to_a_timecode() {
    let sr = 48_000.0;
    let (mut cm, transport, cons) = master(Bpm::of(120.0), SampleRate::of(sr));
    cm.set_send_mtc(true);
    cm.set_mtc_fps(MtcRate::Fps25);
    transport.set_playing(true);

    // 25 fps → 100 quarter-frames/sec → one every 480 samples. Run enough
    // blocks to emit at least 8 consecutive quarter-frames (2 full frames).
    let block = 480usize;
    let beats_per_sample = 120.0 / 60.0 / sr;
    let mut beat = 0.0f64;
    let mut decoder = MtcDecoder::new();
    let mut got_tc = false;
    for _ in 0..40 {
        transport.set_beat(beat);
        cm.tick(block);
        for ev in drain_all(&cons) {
            if is_status(&ev, 0xF1) {
                assert!(decoder.feed(&ev.1), "a quarter-frame is recognised");
                if decoder.timecode().is_some() {
                    got_tc = true;
                }
            }
        }
        beat += block as f64 * beats_per_sample;
    }
    assert!(got_tc, "MTC quarter-frames should assemble into a timecode");
}

/// Plays 5 s of MTC from 58 s plus `offset` samples (so 1:00 is crossed) into a decoder, and
/// checks every assembled timecode against the clock the master read.
fn mtc_loopback(rate: f64, fps: MtcRate, offset: f64) {
    let (mut cm, transport, cons) = master(Bpm::of(120.0), SampleRate::of(rate));
    cm.set_send_mtc(true);
    cm.set_mtc_fps(fps);
    transport.set_playing(true);
    let start = 58.0 * rate + offset;
    let (mut beat, mut frame) = (start * 2.0 / rate, 0);
    let events = run_at(
        &mut cm,
        &transport,
        &cons,
        rate,
        (480, (rate * 5.0 / 480.0) as usize),
        (&mut beat, &mut frame),
    );
    let mut decoder = MtcDecoder::new();
    let (mut piece0_at, mut last_count, mut cycles) = (0.0, None, 0u32);
    for (ev, at) in events.iter().filter(|(e, _)| is_status(e, 0xF1)) {
        let Some(System::TimeCode(code)) = system(ev.1) else {
            panic!("not a quarter-frame");
        };
        let code = code.get();
        if code >> 4 == 0 {
            piece0_at = (start + *at as f64) / rate;
        }
        assert!(decoder.feed(&ev.1));
        if code >> 4 != 7 {
            continue;
        }
        let tc = decoder.timecode().expect("a full cycle");
        assert!(
            u64::from(tc.frames()) < tc.frame_rate().labels_per_second(),
            "{rate} Hz {fps:?}: {tc}"
        );
        // Piece 0 sits on an even frame of the timeline, and the label the
        // cycle spells is that frame: the decoder reports it two frames on
        // (MTC: a receiver keeps an internal offset of +2 frames), and the two differ by under the sample the event is
        // placed on.
        let (num, den) = SmpteFrameRate::from(fps).ratio();
        let lag = piece0_at - (tc.to_seconds().get() - 2.0 * den as f64 / num as f64);
        assert!(
            lag.abs() < 2.0 / rate,
            "{rate} Hz {fps:?}: {tc} is {lag} s from piece 0"
        );
        let count = tc.frame_count();
        if let Some(prev) = last_count {
            assert_eq!(count, prev + 2, "{rate} Hz {fps:?}: {tc}");
        }
        last_count = Some(count);
        cycles += 1;
    }
    let (num, den) = SmpteFrameRate::from(fps).ratio();
    assert!(
        f64::from(cycles) > 5.0 * (num as f64 / den as f64) / 2.0 - 3.0,
        "{cycles} cycles"
    );
    assert_eq!(
        decoder.timecode().map(|t| t.minutes()),
        Some(1),
        "crossed 1:00"
    );
}

/// **One cycle carries one position.** Each assembled timecode is valid
/// (frames below the rate), monotonic, and equal to the even frame piece 0
/// went out on, across the minute rollover, at every rate and start offset.
#[test]
fn outbound_mtc_assembles_whole_valid_timecodes_across_a_minute() {
    for (rate, fps) in [
        (48_000.0, MtcRate::Fps24),
        (48_000.0, MtcRate::Fps25),
        (48_000.0, MtcRate::Fps2997Df),
        (48_000.0, MtcRate::Fps30),
        (44_100.0, MtcRate::Fps2997Df),
        (44_100.0, MtcRate::Fps24),
    ] {
        // Offsets that put 1:00 on, and in the middle of, a cycle.
        for offset in [0.0, 1_000.0, 5_000.0] {
            mtc_loopback(rate, fps, offset);
        }
    }
}

// A position on a frame boundary, or a float's width short of it, reads as
// that frame, floored to an even one.
#[test]
fn timecode_rounds_to_the_nearest_even_frame() {
    let at = |pos: f64, fps| {
        let cycle = mtc_cycle(Seconds::new(pos / SampleRate::SR_48K.get()), fps);
        let nibbles =
            std::array::from_fn(|p| cycle.frame(QuarterFramePiece::masked(p as u8)).nibble.get());
        MtcTimeCode::from_nibbles(nibbles)
            .expect("a position the grid sends is one a clock reads")
            .timecode()
            .to_string()
    };
    let f25 = MtcRate::Fps25;
    assert_eq!(at(3_840.0, f25), "00:00:00:02");
    assert_eq!(at(3_840.0 - 1e-7, f25), "00:00:00:02");
    assert_eq!(at(1_920.0, f25), "00:00:00:00");
    assert_eq!(at(7_680.0 - 1e-7, f25), "00:00:00:04");
    // Drop-frame count 1800 starts at sample 2_882_880 (60.06 s).
    let df = MtcRate::Fps2997Df;
    assert_eq!(at(2_882_880.0, df), "00:01:00:02");
    assert_eq!(at(2_882_880.0 - 1e-7, df), "00:01:00:02");
    assert_eq!(at(2_882_880.0 - 1_601.6, df), "00:00:59:28");
    let t30 = MtcRate::Fps30;
    assert_eq!(at(2_880_000.0, t30), "00:01:00:00");
    assert_eq!(at(2_880_000.0 - 1e-7, t30), "00:01:00:00");
}

/// A rig at 120 BPM and 48 kHz (24 000 frames a beat, a tick every 1 000)
/// looping beats 1.0..1.1, so a block starting on beat 1.0 wraps on frame
/// 2 400. Returns the rig, its transport and its receiver, rolling.
fn looping_rig() -> (Rig, Arc<TestTransport>, Collected) {
    let (cm, transport, cons) = master(Bpm::of(120.0), SampleRate::SR_48K);
    transport.set_loop(1.0, 1.1);
    transport.set_beat(1.0);
    transport.set_playing(true);
    (cm, transport, cons)
}

/// The `(offset, ...)` of the events with `status`.
fn offsets(events: &[UmpAt], status: u8) -> Vec<u32> {
    events
        .iter()
        .filter(|e| is_status(e, status))
        .map(|e| e.0.get())
        .collect()
}

/// **A loop wrap inside a block moves the ticks onto the next pass.** A
/// 4 000-frame block starting on the loop's start (beat 1.0, which is on
/// tick 24) wraps on frame 2 400. Its first pass holds the ticks on frames
/// 0, 1 000 and 2 000; the second starts again on the loop's start beat, so
/// its ticks are on 2 400 (the wrap's own frame) and 3 400. Ticks that ran
/// on linearly past the loop's end would be on 0, 1 000, 2 000 and 3 000.
/// The next block starts where this one ended (1 600 frames into the loop),
/// which is no locate: no Song Position, and its tick on frame 400.
#[test]
fn a_loop_wrap_inside_a_block_moves_the_ticks_onto_the_next_pass() {
    let (mut cm, transport, cons) = looping_rig();
    cm.tick(4_000);
    let first = drain_all(&cons);
    assert_eq!(offsets(&first, 0xF8), [0, 1_000, 2_000, 2_400, 3_400]);

    // The block ended 1 600 frames into the loop: beat 1 + 1600 / 24000. The
    // next one starts there, short of the wrap: no locate, and the tick on
    // beat 1 + 2000 / 24000.
    transport.set_beat(1.0 + 1_600.0 / 24_000.0);
    cm.tick(700);
    let second = drain_all(&cons);
    assert_eq!(offsets(&second, 0xF8), [400]);
    assert!(
        offsets(&second, 0xF2).is_empty(),
        "the block start is no locate"
    );
}

/// **The Song Position goes out on the wrap's frame**, carrying the loop's
/// start (beat 1.0 is MIDI beat 4; the end, 1.5, is 6) and before the tick
/// on that frame. A loop of 1.0..1.5 wraps on frame 12 000; the play edge's
/// own Song Position is on frame 0.
#[test]
fn the_song_position_lands_on_the_wraps_frame() {
    let (mut cm, transport, cons) = looping_rig();
    transport.set_loop(1.0, 1.5);
    cm.tick(13_000);
    let events = drain_all(&cons);
    let positions: Vec<_> = events
        .iter()
        .filter_map(|e| match system(e.1) {
            Some(System::SongPosition(position)) => Some((e.0.get(), position.get())),
            _ => None,
        })
        .collect();
    assert_eq!(positions, [(0, 4), (12_000, 4)]);
    let at_wrap: Vec<_> = events.iter().filter(|e| e.0.get() == 12_000).collect();
    assert!(
        is_status(at_wrap[0], 0xF2) && is_status(at_wrap[1], 0xF8),
        "the Song Position, then the wrapped pass's first tick"
    );
}

/// **The MTC grid restarts on the wrap's frame**, as a locate restarts it:
/// piece 0 goes out on the wrap, the pieces before it on the first pass's
/// grid. At 25 fps and 48 kHz a quarter-frame is 480 frames, so the first
/// pass sends pieces 0..4 on 0, 480, ..., 1 920 and the second pieces 0..3
/// on 2 400, 2 880, 3 360, 3 840. Unrestarted, the grid runs on and piece 5
/// is on 2 400.
#[test]
fn the_mtc_grid_restarts_on_the_wrap() {
    // The loop starts on beat 0, which is on an even frame, so a restarted
    // grid puts piece 0 on the wrap's own frame.
    let (mut cm, transport, cons) = master(Bpm::of(120.0), SampleRate::SR_48K);
    transport.set_loop(0.0, 0.1);
    transport.set_playing(true);
    cm.set_send_mtc(true);
    cm.tick(4_000);
    let events = drain_all(&cons);
    let qf: Vec<_> = events
        .iter()
        .filter_map(|e| match system(e.1) {
            Some(System::TimeCode(code)) => Some((e.0.get(), code.get() >> 4)),
            _ => None,
        })
        .collect();
    assert_eq!(
        qf,
        [
            (0, 0),
            (480, 1),
            (960, 2),
            (1_440, 3),
            (1_920, 4),
            (2_400, 0),
            (2_880, 1),
            (3_360, 2),
            (3_840, 3)
        ]
    );
}

/// Plays the transport (120 BPM, 48 kHz, rolling from `from`, looping
/// `region`) through blocks of `lens`, the host's clock giving each block its
/// start beat, and returns every event with its absolute frame, in the order
/// sent. `mtc` turns the quarter-frames on.
fn play_layout(from: f64, region: (f64, f64), mtc: bool, lens: &[usize]) -> Vec<(u64, UmpAt)> {
    let (mut cm, transport, cons) = master(Bpm::of(120.0), SampleRate::SR_48K);
    transport.set_loop(region.0, region.1);
    transport.set_playing(true);
    cm.set_send_mtc(mtc);
    let looping = tutti_music::LoopRange::new(
        tutti_units::Beat::new(region.0),
        tutti_units::Beat::new(region.1),
    );
    let mut host =
        tutti_music::FrameClock::new(Beat::new(from), Bpm::of(120.0), SampleRate::SR_48K);
    let mut frame = 0u64;
    let mut out = Vec::new();
    for &len in lens {
        transport.set_beat(host.beat().get());
        cm.tick(len);
        out.extend(
            drain_all(&cons)
                .into_iter()
                .map(|e| (frame + u64::from(e.0.get()), e)),
        );
        host.advance(Samples::new(len), looping);
        frame += len as u64;
    }
    out
}

/// Block lengths covering `total` frames in blocks of `size`, the last short.
fn fixed(total: usize, size: usize) -> Vec<usize> {
    let mut lens = vec![size; total / size];
    if !total.is_multiple_of(size) {
        lens.push(total % size);
    }
    lens
}

/// Block lengths cut at the frames in `cuts` (and `total`).
fn cut_at(total: usize, cuts: &[usize]) -> Vec<usize> {
    let mut at = 0;
    let mut lens = Vec::new();
    for &c in cuts.iter().chain(std::iter::once(&total)) {
        lens.push(c - at);
        at = c;
    }
    lens
}

/// **The stream does not depend on where the block boundaries fall.** The
/// loop is beat 0 to 11 999.3 frames on, at 120 BPM and 48 kHz (24 000 frames
/// a beat), so the first wrap is on frame 12 000 and, a fraction of a frame
/// over each time, the next on 23 999, 35 998, 47 998 and 59 997: the wrap frames are
/// fixed by the loop. The same 60 000 frames through one block, blocks of 512
/// and of 1 000, blocks cut exactly on each wrap (the wrap on a block's first
/// frame) and blocks cut one frame after each wrap, must send the same ticks,
/// Song Positions and MTC pieces on the same absolute frames; a Song Position
/// is on every wrap frame and none other, and so is a tick on the loop's
/// start.
#[test]
fn the_stream_does_not_depend_on_the_block_boundaries() {
    const TOTAL: usize = 60_000;
    let region = (0.0, 11_999.3 / 24_000.0);
    let whole = play_layout(0.0, region, true, &[TOTAL]);

    let wraps: Vec<u64> = whole
        .iter()
        .filter(|(_, e)| is_status(e, 0xF2))
        .map(|&(at, _)| at)
        .collect();
    assert_eq!(wraps, [12_000, 23_999, 35_998, 47_998, 59_997]);
    for &w in &wraps {
        assert!(
            whole.iter().any(|(at, e)| *at == w && is_status(e, 0xF8)),
            "a tick on the loop's start at {w}"
        );
    }
    assert!(whole.iter().any(|(_, e)| is_status(e, 0xF1)), "MTC is on");

    let cuts: Vec<usize> = wraps.iter().map(|&w| w as usize).collect();
    let after: Vec<usize> = cuts.iter().map(|c| c + 1).collect();
    for (name, lens) in [
        ("512", fixed(TOTAL, 512)),
        ("1000", fixed(TOTAL, 1_000)),
        ("on the wraps", cut_at(TOTAL, &cuts)),
        ("one past the wraps", cut_at(TOTAL, &after)),
    ] {
        assert_eq!(lens.iter().sum::<usize>(), TOTAL);
        let got = play_layout(0.0, region, true, &lens);
        let key = |v: &[(u64, UmpAt)]| {
            let mut v: Vec<_> = v.iter().map(|&(at, e)| (at, e.1.0[0])).collect();
            v.sort_by_key(|&(at, _)| at);
            v
        };
        assert_eq!(key(&got), key(&whole), "blocks {name}");
    }
}

/// **A bar-aligned loop on power-of-two blocks sends its Song Position.**
/// A loop of beats 0..8 at 120 BPM and 48 kHz is 192 000 frames, 375 blocks
/// of 512, so every wrap is on a block's first frame. Three cycles send a
/// Song Position (to beat 0) on each of the first two wraps, frames 192 000
/// and 384 000, and Start on frame 0 for the play edge.
#[test]
fn a_loop_wrapping_on_every_block_edge_sends_a_song_position_each_time() {
    let events = play_layout(0.0, (0.0, 8.0), false, &fixed(3 * 192_000, 512));
    let positions: Vec<_> = events
        .iter()
        .filter_map(|(at, e)| match system(e.1) {
            Some(System::SongPosition(position)) => Some((*at, position.get())),
            _ => None,
        })
        .collect();
    assert_eq!(positions, [(192_000, 0), (384_000, 0)]);
}

/// **A real locate on the frame of a wrap sends one Song Position**, to
/// where it locates; and a re-prepare between the blocks (`prepare` re-rates
/// the master) keeps the wrap that ended the first.
#[test]
fn a_wrap_on_the_block_edge_and_a_locate_are_one_song_position() {
    let (mut cm, transport, cons) = looping_rig();
    cm.tick(2_400);
    let _ = drain_all(&cons);
    transport.set_beat(1.5);
    cm.tick(100);
    let positions: Vec<_> = drain_all(&cons)
        .iter()
        .filter_map(|e| match system(e.1) {
            Some(System::SongPosition(position)) => Some((e.0.get(), position.get())),
            _ => None,
        })
        .collect();
    assert_eq!(positions, [(0, 6)], "one, to the locate");

    let (mut cm, transport, cons) = looping_rig();
    cm.tick(2_400);
    let _ = drain_all(&cons);
    cm.set_sample_rate(SampleRate::SR_48K);
    transport.set_beat(1.0);
    cm.tick(2_400);
    let second = drain_all(&cons);
    assert_eq!(offsets(&second, 0xF2), [0], "the wrap that ended block one");
    assert_eq!(offsets(&second, 0xF8), [0, 1_000, 2_000]);
}

/// **A master sends on the Group it was built with**: Start, Timing Clocks,
/// Song Position and MTC quarter-frames all carry that Group, not the first.
#[test]
fn every_event_goes_out_on_the_masters_group() {
    let group = MidiGroup::masked(5);
    let mut cm = ClockMaster::new(group, SampleRate::SR_48K);
    let messages = SyncMessages {
        clock: true,
        mtc: true,
        ..SyncMessages::OFF
    };
    let transport = TestTransport::new(Bpm::of(120.0));
    transport.set_beat(8.0);
    transport.set_playing(true);
    let mut events = Vec::new();
    cm.tick(
        messages,
        None,
        &transport.now(),
        Samples::new(4096),
        &mut |e| events.push(e),
    );
    let statuses: std::collections::BTreeSet<u8> = events
        .iter()
        .map(|(_, ump)| ((ump.0[0] >> 16) & 0xFF) as u8)
        .collect();
    for status in [0xF1, 0xF2, 0xF8, 0xFB] {
        assert!(
            statuses.contains(&status),
            "{status:#x} not sent: {statuses:x?}"
        );
    }
    assert_eq!(cm.group, group);
    for (_, ump) in &events {
        assert_eq!(ump.group(), Some(group), "{ump:?}");
    }
}

/// **A frame rate set from the control side reaches the audio side at its next
/// tick, with the pending quarter-frame where it was due.** At 48 kHz and 25
/// fps a quarter-frame is 480 frames; after 20 blocks of 512 the last was at
/// 10 080 and the next is due at 10 560. Set to 30 fps (400 frames) the next
/// one still goes out at 10 560 and the ones after it every 400, with no Song
/// Position.
#[test]
fn a_frame_rate_set_between_blocks_keeps_the_pending_quarter_frame() {
    let (mut cm, transport, cons) = master(Bpm::of(120.0), SampleRate::SR_48K);
    cm.set_send_mtc(true);
    cm.set_mtc_fps(MtcRate::Fps25);
    transport.set_playing(true);
    let (mut beat, mut frame) = (0.0, 0);
    let run = (&mut beat, &mut frame);
    let before = of(
        &run_at(&mut cm, &transport, &cons, 48_000.0, (512, 20), run),
        0xF1,
    );
    assert_eq!(before.last(), Some(&10_080), "setup: every 480 frames");

    cm.set_mtc_fps(MtcRate::Fps30);
    assert_eq!(cm.messages.rate, MtcRate::Fps30);
    let run = (&mut beat, &mut frame);
    let events = run_at(&mut cm, &transport, &cons, 48_000.0, (512, 20), run);
    assert_eq!(of(&events, 0xF2), Vec::<u64>::new(), "not a seek");
    let after = of(&events, 0xF1);
    assert_eq!(after[0], 10_560, "the pending quarter-frame keeps its time");
    assert_eq!(after[1], 10_960, "the next is 400 frames on, at 30 fps");
}

/// The MTC Full Messages in `events`: each one's offset and the payload its
/// two SysEx7 packets carry.
fn full_messages(events: &[UmpAt]) -> Vec<(u32, Vec<u8>)> {
    let mut found = Vec::new();
    let mut open: Option<(u32, Vec<u8>)> = None;
    for (offset, ump) in events {
        let Ok(Message::Sysex7(_, packet)) = ump.decode() else {
            continue;
        };
        let (at, mut bytes) = open.take().unwrap_or((offset.get(), Vec::new()));
        bytes.extend_from_slice(packet.data());
        match packet.status() {
            tutti_ump::SysexStatus::Start | tutti_ump::SysexStatus::Continue => {
                open = Some((at, bytes));
            }
            _ => found.push((at, bytes)),
        }
    }
    found
}

/// A rig at 120 BPM and 48 kHz sending MTC at `fps`, stopped at `beat`.
fn mtc_rig(fps: MtcRate, beat: f64) -> (Rig, Arc<TestTransport>, Collected) {
    let (mut rig, transport, cons) = master(Bpm::of(120.0), SampleRate::SR_48K);
    rig.set_send_mtc(true);
    rig.set_mtc_fps(fps);
    transport.set_beat(beat);
    (rig, transport, cons)
}

/// M1 p.53 (Full Message): a locate while stopped is a Full Message of the
/// position, `7F 7F 01 01 hr mn sc fr`. At 120 bpm beat 10 is 5 s, which at 25
/// fps is `00:00:05:00` (hours byte `0 01 00000`), and beat 3.5 is 1.75 s, frame
/// 19.
#[test]
fn a_cue_while_stopped_is_an_mtc_full_message_of_the_position() {
    let (mut rig, transport, cons) = mtc_rig(MtcRate::Fps25, 10.0);
    rig.tick(512);
    assert_eq!(
        full_messages(&drain_all(&cons)),
        [(0, vec![0x7F, 0x7F, 0x01, 0x01, 0x20, 0x00, 0x05, 0x00])]
    );
    // Standing still states nothing more.
    rig.tick(512);
    assert!(full_messages(&drain_all(&cons)).is_empty());
    transport.set_beat(3.5);
    rig.tick(512);
    assert_eq!(
        full_messages(&drain_all(&cons)),
        [(0, vec![0x7F, 0x7F, 0x01, 0x01, 0x20, 0x00, 0x01, 19])]
    );
}

/// The Full Message states the rate in its hours byte: beat 120.14 is 60.07 s,
/// frame 1802 at 30 fps and frame 1800 at 29.97 drop-frame; both are
/// `00:01:00:02`, the drop-frame label skipping `;00` and `;01`, and the hours
/// byte is `0 11 00000` for 30 and `0 10 00000` for 29.97.
#[test]
fn the_full_message_carries_the_rate_code_and_drop_frame_labels() {
    for (fps, hr, fr) in [(MtcRate::Fps30, 0x60, 2), (MtcRate::Fps2997Df, 0x40, 2)] {
        let (mut rig, _transport, cons) = mtc_rig(fps, 120.14);
        rig.tick(512);
        let sent = full_messages(&drain_all(&cons));
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1[4..], [hr, 1, 0, fr], "{fps:?}");
    }
}

/// A jump while playing is a Full Message at its block's first frame, after
/// which the Quarter Frames restart at piece 0; ordinary playing sends none.
#[test]
fn a_jump_while_playing_sends_a_full_message_and_restarts_the_pieces() {
    let (mut rig, transport, cons) = mtc_rig(MtcRate::Fps25, 0.0);
    rig.tick(512);
    drain_all(&cons);
    transport.set_playing(true);
    rig.tick(512);
    let started = drain_all(&cons);
    // A start from the position the cue stated adds no second Full Message.
    assert!(full_messages(&started).is_empty());
    transport.set_beat(512.0 / 24_000.0);
    rig.tick(512);
    assert!(full_messages(&drain_all(&cons)).is_empty());
    transport.set_beat(40.0);
    rig.tick(512);
    let jumped = drain_all(&cons);
    // Beat 40 is 20 s: 00:00:20:00.
    assert_eq!(
        full_messages(&jumped),
        [(0, vec![0x7F, 0x7F, 0x01, 0x01, 0x20, 0x00, 0x14, 0x00])]
    );
    let first_piece = jumped
        .iter()
        .find_map(|e| match system(e.1) {
            Some(System::TimeCode(code)) => Some(code.get() >> 4),
            _ => None,
        })
        .expect("a quarter frame follows");
    assert_eq!(first_piece, 0);
}

/// A loop wrap is a locate on its own frame: the Full Message is at the wrap's
/// frame and states the loop start (beat 1.0 at 120 bpm is 0.5 s: frame 15 at
/// 30 fps).
#[test]
fn a_loop_wrap_sends_a_full_message_on_its_frame() {
    let (mut rig, _transport, cons) = looping_rig();
    rig.set_send_mtc(true);
    rig.set_mtc_fps(MtcRate::Fps30);
    rig.tick(4_096);
    let events = drain_all(&cons);
    // The wrap is on frame 2 400; the start on frame 0 is a Full Message too.
    let wrap = *offsets(&events, 0xF2).last().unwrap();
    assert_eq!(wrap, 2_400);
    let fulls = full_messages(&events);
    assert_eq!(
        fulls.iter().map(|(at, _)| *at).collect::<Vec<_>>(),
        [0, wrap]
    );
    for (_, bytes) in &fulls {
        assert_eq!(bytes[4..], [0x60, 0, 0, 15]);
    }
}

/// With MTC off, no Full Message is sent, whatever the transport does.
#[test]
fn no_full_message_is_sent_with_mtc_off() {
    let (mut rig, transport, cons) = master(Bpm::of(120.0), SampleRate::SR_48K);
    transport.set_beat(10.0);
    rig.tick(512);
    transport.set_playing(true);
    rig.tick(512);
    transport.set_beat(80.0);
    rig.tick(512);
    assert!(full_messages(&drain_all(&cons)).is_empty());
}

// --- Flex Set Tempo and Set Time Signature ----------------------------------

/// A master with the project meter's reader it ticks over, and what it sends.
struct FlexMaster {
    cm: ClockMaster,
    reader: tutti_rt::Reader<tutti_music::MeterMap>,
    messages: SyncMessages,
}

impl FlexMaster {
    fn tick(
        &mut self,
        transport: &TransportPosition,
        block: Samples,
        emit: &mut impl FnMut(UmpAt),
    ) {
        self.cm.tick(
            self.messages,
            Some(&*self.reader.lend()),
            transport,
            block,
            emit,
        );
    }
}

/// A master on the first Group at 48 kHz, its clock bytes on or off, Flex on,
/// reading the meter of the returned project meter (4/4 until set).
fn flex_master(clock: bool) -> (FlexMaster, ProjectMeter, Collected) {
    let meter = ProjectMeter::default();
    let cm = FlexMaster {
        cm: ClockMaster::new(MidiGroup::FIRST, SampleRate::SR_48K),
        reader: meter.reader(),
        messages: SyncMessages {
            clock,
            flex: true,
            ..SyncMessages::OFF
        },
    };
    (cm, meter, Collected::default())
}

/// One block of `frames` frames at `beat` and `tempo`, playing.
fn play(cm: &mut FlexMaster, out: &Collected, tempo: f64, beat: f64, frames: usize) {
    let transport = TransportPosition::new(true, Bpm::of(tempo), Beat::new(beat), None);
    cm.tick(&transport, Samples::new(frames), &mut |e| {
        out.0.lock().unwrap().push(e);
    });
}

/// The Flex messages of `events` as `(frame, message)`, in order.
fn flexes(events: &[UmpAt]) -> Vec<(u32, Flex)> {
    events
        .iter()
        .filter_map(|(at, ump)| match ump.decode() {
            Ok(Message::Flex(group, flex)) => {
                assert_eq!(group, MidiGroup::FIRST, "addressed to the group");
                Some((at.get(), flex))
            }
            _ => None,
        })
        .collect()
}

fn time_signature(numerator: u8, denominator: u8) -> Flex {
    Flex::SetTimeSignature(tutti_ump::FlexTimeSignature {
        numerator,
        denominator,
        thirty_seconds: 8,
    })
}

/// Tempo 120 for 2 500 frames (ticks every 1 000 frames: 24 000 frames a beat
/// over 24), then 150 from beat 2 500 / 24 000 (800 frames a tick; the next
/// tick, 3 / 24 beat, is (3 / 24 - 2 500 / 24 000) x 19 200 = 400 frames in).
/// Set Tempo goes out for 120 on the start beat's tick and for 150 on frame
/// 400, with the Timing Clock of that tick, and on no frame between ticks.
/// Spec: m2-104/7.5.3#2
#[test]
fn set_tempo_with_clock_goes_out_on_a_tick() {
    let (mut cm, _meter, out) = flex_master(true);
    play(&mut cm, &out, 120.0, 0.0, 2_500);
    let first = drain_all(&out);
    assert_eq!(flexes(&first)[0], (0, Flex::SetTempo(50_000_000)));
    play(&mut cm, &out, 150.0, 2_500.0 / 24_000.0, 2_000);
    let second = drain_all(&out);
    let tempo: Vec<_> = flexes(&second)
        .into_iter()
        .filter(|(_, f)| matches!(f, Flex::SetTempo(_)))
        .collect();
    assert_eq!(tempo, [(400, Flex::SetTempo(40_000_000))]);
    let ticks: Vec<u32> = second
        .iter()
        .filter(|e| is_status(e, 0xF8))
        .map(|e| e.0.get())
        .collect();
    assert_eq!(ticks, [400, 1_200]);
}

/// The same with the clock bytes off: no Timing Clock is sent, and Set Tempo
/// still waits for the 24-PPQN grid (frame 400), not the segment's first frame.
/// Spec: m2-104/7.5.3#3
#[test]
fn set_tempo_without_clock_goes_out_on_the_tick_grid() {
    let (mut cm, _meter, out) = flex_master(false);
    play(&mut cm, &out, 120.0, 0.0, 2_500);
    let first = drain_all(&out);
    assert_eq!(flexes(&first)[0], (0, Flex::SetTempo(50_000_000)));
    play(&mut cm, &out, 150.0, 2_500.0 / 24_000.0, 2_000);
    let second = drain_all(&out);
    assert!(
        second.iter().all(|e| !is_status(e, 0xF8)),
        "no clock bytes with the clock off"
    );
    let tempo: Vec<_> = flexes(&second)
        .into_iter()
        .filter(|(_, f)| matches!(f, Flex::SetTempo(_)))
        .collect();
    assert_eq!(tempo, [(400, Flex::SetTempo(40_000_000))]);
}

/// A tempo that stays is stated once, not on every tick or block.
/// Spec: m2-104/7.5.3#3
#[test]
fn an_unchanged_tempo_is_not_restated() {
    let (mut cm, _meter, out) = flex_master(false);
    play(&mut cm, &out, 120.0, 0.0, 2_400);
    play(&mut cm, &out, 120.0, 0.1, 2_400);
    let tempo = flexes(&drain_all(&out))
        .into_iter()
        .filter(|(_, f)| matches!(f, Flex::SetTempo(_)))
        .count();
    assert_eq!(tempo, 1);
}

/// 4/4 from the top is stated on frame 0 (a bar boundary); a 3/4 meter
/// published at beat 1.5 (36 000 frames at 120 bpm, 48 kHz) is stated on the
/// next bar boundary of the new meter, beat 3, frame (3 - 1.5) x 24 000 =
/// 36 000 of the block that starts at 1.5, and on no frame before it.
/// Spec: m2-104/7.5.4#1
#[test]
fn set_time_signature_with_clock_goes_out_on_a_bar_boundary() {
    let (mut cm, meter, out) = flex_master(true);
    play(&mut cm, &out, 120.0, 0.0, 36_000);
    let first = flexes(&drain_all(&out));
    assert_eq!(
        first
            .iter()
            .filter(|(_, f)| *f == time_signature(4, 2))
            .count(),
        1
    );
    meter.set_meter(MeterMap::new([MeterChange::new(
        Beat::new(0.0),
        TimeSignature::of(3, 4),
    )]));
    play(&mut cm, &out, 120.0, 1.5, 48_000);
    let second: Vec<_> = flexes(&drain_all(&out))
        .into_iter()
        .filter(|(_, f)| matches!(f, Flex::SetTimeSignature(_)))
        .collect();
    assert_eq!(second, [(36_000, time_signature(3, 2))]);
}

/// The same with the clock bytes off.
/// Spec: m2-104/7.5.4#2
#[test]
fn set_time_signature_without_clock_goes_out_on_a_bar_boundary() {
    let (mut cm, meter, out) = flex_master(false);
    play(&mut cm, &out, 120.0, 0.0, 36_000);
    let first = flexes(&drain_all(&out));
    assert!(first.contains(&(0, time_signature(4, 2))));
    meter.set_meter(MeterMap::new([MeterChange::new(
        Beat::new(0.0),
        TimeSignature::of(7, 8),
    )]));
    // 7/8 is 3.5 beats a bar: the boundary after beat 1.5 is beat 3.5, frame
    // (3.5 - 1.5) x 24 000 = 48 000 of the block; 7/8 wire is 7 over 2^3.
    play(&mut cm, &out, 120.0, 1.5, 60_000);
    let second: Vec<_> = flexes(&drain_all(&out))
        .into_iter()
        .filter(|(_, f)| matches!(f, Flex::SetTimeSignature(_)))
        .collect();
    assert_eq!(second, [(48_000, time_signature(7, 3))]);
}

/// A Continue mid-bar states the tempo on its first tick (beat 1 is tick 24,
/// frame 0) and the meter on the next bar boundary, beat 4, frame
/// (4 - 1) x 24 000 = 72 000, never off a bar boundary.
/// Spec: m2-104/7.5.4#1
#[test]
fn a_start_mid_bar_states_the_meter_at_the_next_bar() {
    let (mut cm, _meter, out) = flex_master(true);
    play(&mut cm, &out, 120.0, 1.0, 80_000);
    let sent = flexes(&drain_all(&out));
    assert_eq!(
        sent,
        [
            (0, Flex::SetTempo(50_000_000)),
            (72_000, time_signature(4, 2)),
        ]
    );
}

/// Flex is off until set: a clock with the tempo changing and a meter set
/// sends no Flex Data.
#[test]
fn no_flex_is_sent_unless_enabled() {
    let (mut cm, _meter, out) = flex_master(true);
    cm.messages.flex = false;
    assert!(cm.messages.clock);
    play(&mut cm, &out, 120.0, 0.0, 48_000);
    play(&mut cm, &out, 150.0, 2.0, 48_000);
    let events = drain_all(&out);
    assert!(flexes(&events).is_empty());
    assert!(events.iter().any(|e| is_status(e, 0xF8)), "the clock ran");
}

fn metronomes(events: &[UmpAt]) -> Vec<(u32, Flex)> {
    flexes(events)
        .into_iter()
        .filter(|(_, f)| matches!(f, Flex::SetMetronome(_)))
        .collect()
}

/// The Set Metronome of a bar of `beats` beats, undivided, clicking every
/// `clocks` MIDI clocks with no subdivision clicks.
fn metronome(clocks: u8, beats: u8) -> Flex {
    Flex::SetMetronome(Metronome {
        clocks_per_click: clocks,
        accents: BarAccents::Undivided(NonZeroU8::new(beats).unwrap()),
        subdivision_1: 0,
        subdivision_2: 0,
    })
}

/// A master sending the Set Metronome alone: Flex tempo and meter off.
fn metronome_master() -> (FlexMaster, ProjectMeter, Collected) {
    let (mut cm, meter, out) = flex_master(false);
    cm.messages.flex = false;
    cm.messages.metronome = true;
    (cm, meter, out)
}

/// A start from the top states a click on every quarter-note beat of 4/4 (24
/// clocks), the bar undivided (Part 1 the 4 beats, Parts 2 and 3 zero) with no
/// subdivision clicks, on the bar line's frame and with no Set Tempo or Set
/// Time Signature; a 3/4 meter published at beat 1.5 is stated on its first bar
/// boundary, beat 3, frame (3 - 1.5) x 24 000 = 36 000; a 6/8 meter has an eighth
/// note for a beat, 12 clocks, and 6 beats in the bar.
/// Spec: m2-104/7.5.5#2, m2-104/7.5.5#5, m2-104/7.5.5#9
#[test]
fn set_metronome_states_the_click_of_each_bar_boundary_meter() {
    let (mut cm, meter, out) = metronome_master();
    play(&mut cm, &out, 120.0, 0.0, 36_000);
    let first = drain_all(&out);
    assert_eq!(flexes(&first), [(0, metronome(24, 4))]);
    meter.set_meter(map(&[(0.0, 3, 4)]));
    play(&mut cm, &out, 120.0, 1.5, 48_000);
    assert_eq!(metronomes(&drain_all(&out)), [(36_000, metronome(24, 3))]);
    meter.set_meter(map(&[(0.0, 6, 8)]));
    // The boundary after beat 3.0 + 1.5 = 4.5 of 6/8 (3 beats a bar) is beat 6.
    play(&mut cm, &out, 120.0, 4.5, 48_000);
    assert_eq!(metronomes(&drain_all(&out)), [(36_000, metronome(12, 6))]);
}

/// The Set Metronome follows the meter on the Set Time Signature's frame, after
/// it, when both are on.
/// Spec: m2-104/7.5.5#2
#[test]
fn set_metronome_follows_the_set_time_signature_on_its_frame() {
    let (mut cm, _meter, out) = flex_master(false);
    cm.messages.metronome = true;
    play(&mut cm, &out, 120.0, 0.0, 1_000);
    assert_eq!(
        flexes(&drain_all(&out)),
        [
            (0, Flex::SetTempo(50_000_000)),
            (0, time_signature(4, 2)),
            (0, metronome(24, 4)),
        ]
    );
}

/// An unchanged meter is stated once; a stop and a start state it again.
/// Spec: m2-104/7.5.5#2
#[test]
fn set_metronome_is_stated_once_and_again_on_a_start() {
    let (mut cm, _meter, out) = metronome_master();
    play(&mut cm, &out, 120.0, 0.0, 30_000);
    // Crosses the bar line at beat 4: the same meter is not restated.
    play(&mut cm, &out, 120.0, 1.25, 90_000);
    assert_eq!(metronomes(&drain_all(&out)), [(0, metronome(24, 4))]);
    let stopped = TransportPosition::new(false, Bpm::of(120.0), Beat::new(2.5), None);
    cm.tick(&stopped, Samples::new(1_000), &mut |_| {});
    play(&mut cm, &out, 120.0, 4.0, 1_000);
    assert_eq!(metronomes(&drain_all(&out)), [(0, metronome(24, 4))]);
}

/// A note value whose beat is not whole clocks (a 64th, a clock and a half) has
/// no Set Metronome.
#[test]
fn a_meter_with_a_fractional_clock_beat_states_no_metronome() {
    let (mut cm, meter, out) = metronome_master();
    meter.set_meter(map(&[(0.0, 4, 64)]));
    play(&mut cm, &out, 120.0, 0.0, 1_000);
    assert!(drain_all(&out).is_empty());
}

/// The metronome is off until set: tempo and meter alone send no Set Metronome.
#[test]
fn no_metronome_is_sent_unless_enabled() {
    let (mut cm, _meter, out) = flex_master(true);
    assert!(!cm.messages.metronome);
    play(&mut cm, &out, 120.0, 0.0, 48_000);
    let events = drain_all(&out);
    assert!(metronomes(&events).is_empty());
    assert!(!meters(&events).is_empty(), "the meter was sent");
}

fn meters(events: &[UmpAt]) -> Vec<(u32, Flex)> {
    flexes(events)
        .into_iter()
        .filter(|(_, f)| matches!(f, Flex::SetTimeSignature(_)))
        .collect()
}

fn tempos(events: &[UmpAt]) -> Vec<(u32, Flex)> {
    flexes(events)
        .into_iter()
        .filter(|(_, f)| matches!(f, Flex::SetTempo(_)))
        .collect()
}

fn map(changes: &[(f64, u32, u32)]) -> MeterMap {
    MeterMap::new(
        changes
            .iter()
            .map(|&(beat, n, d)| MeterChange::new(Beat::new(beat), TimeSignature::of(n, d))),
    )
}

/// A meter change off the dyadic beats puts bar lines where `bar_at` of a line
/// can name the bar before it (4/4 from 0.1 has bar 3 at 4.1, which `bar_at`
/// puts in bar 2; 1/64 from 0.1 has bar 5 at 0.2875). The master walks bars by
/// number, so it ends, and states each new meter on the frame of the first bar
/// line of it: at 120 bpm and 48 kHz a beat is 24 000 frames, the lines here
/// are whole frames, and the expected frames are those of `MeterMap::bar_start`.
/// Spec: m2-104/7.5.4#1
#[test]
fn bar_lines_off_the_dyadic_beats_are_walked_by_number_and_end() {
    for changes in [
        &[(0.0, 4, 4), (0.1, 4, 4)][..],
        &[(0.0, 4, 4), (0.1, 3, 4)],
        &[(0.0, 4, 4), (0.1, 1, 64)],
    ] {
        let meter_map = map(changes);
        let (mut cm, meter, out) = flex_master(true);
        meter.set_meter(meter_map.clone());
        let frames = 480_000;
        play(&mut cm, &out, 120.0, 0.0, frames);
        let mut expected = Vec::new();
        let mut stated = None;
        for n in 1.. {
            let line = meter_map.bar_start(tutti_units::BarNumber::new(n));
            let frame = (line.get() * 24_000.0).round() as usize;
            if frame >= frames {
                break;
            }
            let signature = meter_map.at(line);
            if stated != Some(signature) {
                stated = Some(signature);
                expected.push((
                    frame as u32,
                    Flex::SetTimeSignature(flex_time_signature_wire(signature).unwrap()),
                ));
            }
        }
        assert_eq!(meters(&drain_all(&out)), expected, "{changes:?}");
    }
}

/// A tick and a bar line on the first frame of a block are sent there once,
/// not also at the end of the block before: 1 000 frames a tick at 120 bpm,
/// and 4/4 then 3/4 from beat 4 (frame 96 000).
/// Spec: m2-104/7.5.3#2, m2-104/7.5.4#1
#[test]
fn a_tick_and_a_bar_line_on_a_block_edge_are_sent_once() {
    let (mut cm, meter, out) = flex_master(true);
    meter.set_meter(map(&[(0.0, 4, 4), (4.0, 3, 4)]));
    play(&mut cm, &out, 120.0, 0.0, 96_000);
    let first = drain_all(&out);
    assert_eq!(meters(&first), [(0, time_signature(4, 2))]);
    // 96 ticks, the last on frame 95 000: beat 4's tick is the next block's.
    let clocks = offsets(&first, 0xF8);
    assert_eq!((clocks.len(), clocks.last()), (96, Some(&95_000)));
    play(&mut cm, &out, 120.0, 4.0, 48_000);
    let second = drain_all(&out);
    assert_eq!(meters(&second), [(0, time_signature(3, 2))]);
    assert_eq!(offsets(&second, 0xF8)[..2], [0, 1_000]);

    // Tempo 150 from the block that starts on tick 1 (frame 1 000 of 120).
    let (mut cm, _meter, out) = flex_master(true);
    play(&mut cm, &out, 120.0, 0.0, 1_000);
    let block = drain_all(&out);
    assert_eq!(tempos(&block), [(0, Flex::SetTempo(50_000_000))]);
    assert_eq!(offsets(&block, 0xF8), [0]);
    play(&mut cm, &out, 150.0, 1_000.0 / 24_000.0, 1_000);
    let block = drain_all(&out);
    assert_eq!(tempos(&block), [(0, Flex::SetTempo(40_000_000))]);
    // 800 frames a tick at 150 bpm.
    assert_eq!(offsets(&block, 0xF8), [0, 800]);
    play(
        &mut cm,
        &out,
        150.0,
        1_000.0 / 24_000.0 + 1_000.0 / 19_200.0,
        1_000,
    );
    let block = drain_all(&out);
    assert!(tempos(&block).is_empty());
    // Tick 3 (beat 0.125) is (0.125 - 0.09375) x 19 200 = 600 frames in.
    assert_eq!(offsets(&block, 0xF8), [600]);
}

/// A locate while playing restates nothing that did not change, and the new
/// meter waits for a bar line: 4/4 from 0 and 3/4 from 4 have bars at 0, 4, 7
/// and 10, so a locate to 8.5 states 3/4 on frame (10 - 8.5) x 24 000 = 36 000.
/// Spec: m2-104/7.5.4#1
#[test]
fn a_locate_while_playing_states_the_meter_at_the_next_bar_line() {
    let (mut cm, meter, out) = flex_master(true);
    play(&mut cm, &out, 120.0, 0.0, 2_400);
    drain_all(&out);
    meter.set_meter(map(&[(0.0, 4, 4), (4.0, 3, 4)]));
    play(&mut cm, &out, 120.0, 8.5, 60_000);
    let second = drain_all(&out);
    assert_eq!(meters(&second), [(36_000, time_signature(3, 2))]);
    assert!(tempos(&second).is_empty());
}

/// The passes of a looped block (here three of 24 000 frames, wrapping at
/// 24 000 and 48 000) restate nothing: the tempo and the meter are each
/// stated once, on the first frame.
/// Spec: m2-104/7.5.3#2, m2-104/7.5.4#1
#[test]
fn the_passes_of_a_looped_block_restate_nothing() {
    let (mut cm, _meter, out) = flex_master(true);
    let looping = LoopRange::new(Beat::new(0.0), Beat::new(1.0)).expect("a loop");
    let transport = TransportPosition::new(true, Bpm::of(120.0), Beat::new(0.0), Some(looping));
    cm.tick(&transport, Samples::new(72_000), &mut |e| {
        out.0.lock().unwrap().push(e);
    });
    let events = drain_all(&out);
    assert_eq!(offsets(&events, 0xF2), [24_000, 48_000], "two wraps");
    assert_eq!(
        flexes(&events),
        [(0, Flex::SetTempo(50_000_000)), (0, time_signature(4, 2)),]
    );
}

/// A meter whose change falls inside the block is stated on the first bar line
/// of the new one: 4/4 then 3/4 from beat 8 over 12 beats (288 000 frames) puts
/// 3/4 on frame 8 x 24 000 = 192 000.
/// Spec: m2-104/7.5.4#1
#[test]
fn a_meter_change_inside_the_block_is_stated_on_its_bar_line() {
    let (mut cm, meter, out) = flex_master(true);
    meter.set_meter(map(&[(0.0, 4, 4), (8.0, 3, 4)]));
    play(&mut cm, &out, 120.0, 0.0, 288_000);
    assert_eq!(
        flexes(&drain_all(&out)),
        [
            (0, Flex::SetTempo(50_000_000)),
            (0, time_signature(4, 2)),
            (192_000, time_signature(3, 2)),
        ]
    );
}

/// A whole-note meter (4/1, 16 beats a bar) has no standard Set Time
/// Signature: nothing is sent for it, and nothing is resent at each of its bar
/// lines (beats 4 and 20 of 24).
/// Spec: m2-104/7.5.4#1
#[test]
fn a_whole_note_meter_sends_nothing_and_4_4_is_sent_once() {
    let (mut cm, meter, out) = flex_master(true);
    meter.set_meter(map(&[(0.0, 4, 4), (4.0, 4, 1)]));
    play(&mut cm, &out, 120.0, 0.0, 576_000);
    assert_eq!(meters(&drain_all(&out)), [(0, time_signature(4, 2))]);
}
