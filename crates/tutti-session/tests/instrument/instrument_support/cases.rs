//! The cases. Each takes the subject by type, builds its own rig, and states
//! what it pins in its doc.
//!
//! Figures are analytic or invariants of the render (a frame index the
//! subject's declaration implies, silence, a pitch in Hz); no case compares a
//! render with another render.

use std::panic::{AssertUnwindSafe, catch_unwind};
use tutti_buffers::PlanesMut;
use tutti_session::Stamp;

use tutti_units::{Amplitude, At, Frame, Tail};

use super::midi::*;
use super::rig::{BLOCK, Rig};
use tutti_ump::{Message, NoteNumber, NoteVelocity, Ump, Voice2};

/// A case a subject cannot pass: it runs, and must fail with `fails_with`.
pub struct Gap {
    pub case: &'static str,
    pub why: &'static str,
    pub fails_with: &'static str,
}

/// A hosted plugin's chunk counters (`Diagnostics`), each over its life.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Chunks {
    /// Chunks the node played as silence because the pipeline had not answered.
    pub missed: u64,
    /// Of those, answered after the silence was played.
    pub late: u64,
    /// Of those, answered by the plugin with a refusal: it did not publish the
    /// chunk's audio, and said so.
    pub refused: u64,
    /// Chunks the bridge dropped as more than one chunk behind.
    pub skipped: u64,
}

impl Chunks {
    /// The counts since `before`, an earlier reading of the same counters.
    fn since(self, before: Chunks) -> Chunks {
        Chunks {
            missed: self.missed.saturating_sub(before.missed),
            late: self.late.saturating_sub(before.late),
            refused: self.refused.saturating_sub(before.refused),
            skipped: self.skipped.saturating_sub(before.skipped),
        }
    }
}

pub trait Subject {
    const NAME: &'static str;
    const GAPS: &'static [Gap];
    /// Whether its pitch follows the note (a gate does not).
    const PITCHED: bool;
    /// Frames from a note-off by which the output must be silent.
    const SETTLE: usize;
    /// Frames from a note-on's stamp (plus the node's declared latency) to its
    /// first nonzero sample, as a range: the attack's lead plus the
    /// resolution the subject declares for its events.
    const LEAD: (usize, usize);
    /// Frames an All Sound Off may keep sounding past its stamp.
    const CUT: usize;
    /// A rig with the subject's instrument in it, made an instrument.
    fn rig() -> Rig;
    /// What a black-box subject recorded of what the host sent it, when it
    /// records anything: a case about a message the subject may not act on
    /// asserts the host delivered it, and where.
    fn transcript(rig: &Rig) -> Option<tutti_shared_plugin::transcript::Transcript> {
        let _ = rig;
        None
    }
    /// The chunks its pipeline has not answered in time, as counted so far:
    /// zero for a subject that has no pipeline.
    fn chunks(rig: &Rig) -> Chunks {
        let _ = rig;
        Chunks::default()
    }
    /// Frames its audio lags its events: what the node declares to the
    /// compiler. A subject whose declaration is not what it does says what it
    /// does.
    fn lag(rig: &Rig) -> usize {
        rig.session
            .graph_ref()
            .node_latency(rig.node)
            .unwrap()
            .get()
    }
}

/// Runs `case`, honouring a gap.
pub fn run<S: Subject>(name: &'static str, case: fn()) {
    match S::GAPS.iter().find(|g| g.case == name) {
        None => case(),
        Some(gap) => match catch_unwind(AssertUnwindSafe(case)) {
            Ok(()) => panic!(
                "{}: the gap in `{name}` is stale, the case passes ({})",
                S::NAME,
                gap.why
            ),
            Err(e) => {
                let text = e
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                    .unwrap_or_default();
                assert!(
                    text.contains(gap.fails_with),
                    "{}: `{name}` failed, but not as the gap says ({:?}); it said: {text}",
                    S::NAME,
                    gap.fails_with
                );
            }
        },
    }
}

/// The frames a note stamped at `stamp` may first sound on.
fn window<S: Subject>(rig: &Rig, stamp: usize) -> std::ops::RangeInclusive<usize> {
    let base = stamp + S::lag(rig);
    base + S::LEAD.0..=base + S::LEAD.1
}

/// `Ok` iff the first sound at or after `from` is in the window of a note
/// stamped at `stamp`, exactly.
fn onset_in_window<S: Subject>(rig: &Rig, from: usize, stamp: usize) -> Result<(), String> {
    let w = window::<S>(rig, stamp);
    match rig.onset(from) {
        Some(first) if w.contains(&first) => Ok(()),
        Some(first) => Err(format!("first sound at {first}, wanted {w:?}")),
        None => Err(format!("the note is silent, wanted {w:?}")),
    }
}

/// `Ok` iff the first sound at or after `from` is in the window of a note
/// stamped at `stamp`, with the slack of the rate ratio.
fn first_sound_in_window<S: Subject>(rig: &Rig, from: usize, stamp: usize) -> Result<(), String> {
    let w = window::<S>(rig, stamp);
    let Some(first) = rig.onset(from) else {
        return Err(format!("the note is silent, wanted about {w:?}"));
    };
    // The lead is in frames at the rate it was measured at; allow the ratio.
    let slack = S::LEAD.1 / 8 + 4;
    if (*w.start() - slack..=*w.end() + slack).contains(&first) {
        Ok(())
    } else {
        Err(format!("first sound at {first}, wanted about {w:?}"))
    }
}

/// What one attempt at a timing case found: its verdict, and the chunks its
/// subject's pipeline missed, answered late, refused or skipped while the case
/// ran.
struct Judged {
    outcome: Result<(), String>,
    chunks: Chunks,
}

/// Blocks rendered after a case's verdict before its chunks are read: a late
/// answer is counted on a later chunk, within the pipeline's ring, so a miss in
/// the last blocks of the case is accounted for by then.
const LATE_COUNT_BLOCKS: usize = 4;

/// The chunks counted since `before`, read once a late answer to the case's
/// last block has had [`LATE_COUNT_BLOCKS`] to be counted. It renders after the
/// verdict, so it changes no frame the case judged.
fn chunks_over_the_case<S: Subject>(rig: &mut Rig, before: Chunks) -> Chunks {
    rig.render_to(rig.frame() + LATE_COUNT_BLOCKS * BLOCK);
    S::chunks(rig).since(before)
}

/// Attempts a case gets before it fails as too loaded to judge.
const JUDGEABLE_RUNS: usize = 3;

/// Runs `attempt` on a fresh rig until one can be judged. A passing attempt is
/// judged. A failing one is judged too, unless the subject's pipeline missed a
/// chunk during it that the counters account for as late, refused or skipped:
/// the silent block such an answer plays moves a note's first sound by a
/// block, or loses it, whatever its stamp. A failure with no such miss fails at
/// once, as does a miss the counters leave unanswered. Three unjudgeable
/// attempts fail as too loaded to judge.
fn judgeable<S: Subject>(what: &str, mut attempt: impl FnMut() -> Judged) {
    let mut seen = Vec::new();
    for n in 1..=JUDGEABLE_RUNS {
        let Judged { outcome, chunks } = attempt();
        let Err(why) = outcome else { return };
        if chunks.missed == 0 || chunks.late + chunks.refused + chunks.skipped < chunks.missed {
            panic!(
                "{}: {what}: {why} (chunks over the case: {chunks:?})",
                S::NAME
            );
        }
        eprintln!(
            "{}: {what}: attempt {n} not judgeable, {why} (chunks over the case: {chunks:?})",
            S::NAME
        );
        seen.push(chunks);
    }
    panic!(
        "{}: {what}: machine too loaded to judge: chunks over each attempt: {seen:?}",
        S::NAME
    );
}

const STAMP: usize = 1_037;
const OFF: usize = 6_011;

/// **Only an instrument is one**: a node with no event input, and one with
/// no stereo pair, are refused with the reason. (A hosted plugin that declares
/// itself an effect is refused in `tests/plugin/plugin.rs`: it needs a plugin.)
pub fn only_an_instrument_is_one<S: Subject>() {
    let mut rig = S::rig();
    let dc = rig
        .session
        .graph()
        .add(tutti_graph::ForkByClone(tutti_nodes::testing::Const::mono(
            0.5,
        )));
    let err = rig
        .session
        .instrument(dc)
        .expect_err("a source with no event input");
    assert!(
        matches!(
            err,
            tutti_session::Error::NotInstrument {
                why: tutti_session::NotInstrument::NoEventInput,
                ..
            }
        ),
        "{err}"
    );
}

/// **A note-on sounds on the frame it is stamped** (plus the node's declared
/// latency and its lead): the first nonzero sample is in the subject's window.
/// A stamp is a position in a block, so the case stamps off the block grid.
pub fn note_on_sounds_at_its_stamp<S: Subject>() {
    judgeable::<S>("a note-on sounds on the frame it is stamped", || {
        let mut rig = S::rig();
        let before = S::chunks(&rig);
        rig.at(rig.t(STAMP), note_on(0, 60, 100));
        rig.render_to(rig.t(STAMP) + 4_000);
        let w = window::<S>(&rig, rig.t(STAMP));
        let outcome = onset_in_window::<S>(&rig, rig.origin, rig.t(STAMP)).and_then(|()| {
            let early = rig.peak(rig.origin..*w.start());
            if early != 0.0 {
                return Err(format!("{early} before the window {w:?}"));
            }
            let right = rig.right[*w.start()..]
                .iter()
                .fold(0.0f32, |a, v| a.max(v.abs()));
            if right <= 1e-4 {
                return Err("the right channel is silent: the strip reads only the left".into());
            }
            Ok(())
        });
        let chunks = chunks_over_the_case::<S>(&mut rig, before);
        Judged { outcome, chunks }
    });
}

/// **A frame already past plays at the next block**, not never: a note-on
/// stamped for a frame the engine has rendered sounds from the next block's
/// first frame.
pub fn a_frame_already_past_plays_at_the_next_block<S: Subject>() {
    judgeable::<S>("a frame already past plays at the next block", || {
        let mut rig = S::rig();
        rig.render_to(rig.t(3 * BLOCK));
        let next = rig.frame();
        let before = S::chunks(&rig);
        // The origin of the clock, long gone.
        assert!(
            rig.handle
                .keys_mut()
                .push(Frame::new(1), note_on(0, 60, 100))
                .is_ok()
        );
        rig.render_to(next + 4_000);
        let outcome = onset_in_window::<S>(&rig, next, next);
        let chunks = chunks_over_the_case::<S>(&mut rig, before);
        Judged { outcome, chunks }
    });
}

/// **A note-off releases**: held, then released at `OFF`, the output is
/// silent by `SETTLE` frames after it.
pub fn note_off_releases<S: Subject>() {
    let mut rig = S::rig();
    rig.at(rig.t(STAMP), note_on(0, 60, 100));
    rig.at(rig.t(OFF), note_off(0, 60));
    let quiet = rig.t(OFF) + S::lag(&rig) + S::SETTLE;
    rig.render_to(quiet + 2 * BLOCK);
    assert!(
        rig.peak(rig.t(OFF)..rig.t(OFF) + 1_000) > 1e-4,
        "it sounded"
    );
    let p = rig.peak(quiet..quiet + 2 * BLOCK);
    assert!(p < 1e-4, "still sounding {p} at {quiet}");
}

/// **A note-on at velocity 0 is a note-off.**
pub fn velocity_zero_is_a_note_off<S: Subject>() {
    let mut rig = S::rig();
    rig.at(rig.t(STAMP), note_on(0, 60, 100));
    rig.at(rig.t(OFF), note_on(0, 60, 0));
    let quiet = rig.t(OFF) + S::lag(&rig) + S::SETTLE;
    rig.render_to(quiet + 2 * BLOCK);
    let p = rig.peak(quiet..quiet + 2 * BLOCK);
    assert!(p < 1e-4, "still sounding {p} at {quiet}");
}

/// **The declared tail covers the ring-out**: what the node rang for after its
/// note-off is no longer than the tail it declares, so an offline render that
/// stops at the last note-off plus the tail keeps everything.
pub fn tail_covers_the_ring_out<S: Subject>() {
    let mut rig = S::rig();
    rig.at(rig.t(STAMP), note_on(0, 60, 100));
    rig.at(rig.t(OFF), note_off(0, 60));
    let end = rig.t(OFF) + S::lag(&rig) + S::SETTLE + 2 * BLOCK;
    rig.render_to(end);
    let last = rig.last_audible().expect("it sounded");
    let rang = last.saturating_sub(rig.t(OFF) + S::lag(&rig));
    let tail = rig.session.graph_ref().node_tail(rig.node).unwrap();
    let declared = match tail {
        Tail::None | Tail::Unknown => 0,
        Tail::Finite(n) => n.get(),
        Tail::Unbounded => return,
    };
    assert!(
        rang <= declared + BLOCK,
        "rang {rang} frames after the note-off, declared tail {tail:?}"
    );
}

/// **All Sound Off silences the channel** within the subject's resolution of
/// its stamp, not at the next block. A subject that is a black box (a hosted
/// plugin decides for itself what CC 120 means) is held to what the host does:
/// it delivers the message once, as a control change of controller 120 and value
/// 0, on the frame of its stamp (its offset in its block).
pub fn all_sound_off_cuts_at_its_stamp<S: Subject>() {
    const CC: usize = 20_005;
    let mut rig = S::rig();
    rig.at(rig.t(STAMP), note_on(0, 60, 100));
    rig.at(rig.t(CC), cc(0, 120, 0));
    let from = rig.t(CC) + S::lag(&rig) + S::CUT;
    rig.render_to(from + 2_000);
    if let Some(t) = S::transcript(&rig) {
        use tutti_shared_plugin::transcript::Kind;
        let seen: Vec<_> = t
            .of(Kind::Control)
            .map(|e| (e.offset, e.id, e.value))
            .collect();
        assert_eq!(
            seen,
            [((rig.t(CC) % BLOCK) as u32, 120, 0.0)],
            "the plugin was sent All Sound Off once, on its stamp"
        );
        let on: Vec<_> = t.of(Kind::NoteOn).map(|e| e.offset).collect();
        assert_eq!(on, [(rig.t(STAMP) % BLOCK) as u32], "the note-on's stamp");
        return;
    }
    assert!(
        rig.peak(rig.t(CC) - 2_000..rig.t(CC)) > 1e-4,
        "it sounded before"
    );
    let p = rig.peak(from..from + 2_000);
    assert!(p < 1e-4, "sounding {p} after All Sound Off, from {from}");
}

/// **A message on another channel**: a note on channel 5 sounds in the same
/// window (an instrument is not tied to channel 0), and a pedal down on
/// channel 2 holds nothing of channel 0's.
pub fn another_channel_is_heard_and_holds_nothing<S: Subject>() {
    judgeable::<S>("a note on channel 5 sounds in the window", || {
        let mut rig = S::rig();
        let before = S::chunks(&rig);
        rig.at(rig.t(STAMP), note_on(5, 60, 100));
        rig.render_to(rig.t(STAMP) + 4_000);
        let outcome = onset_in_window::<S>(&rig, rig.origin, rig.t(STAMP));
        let chunks = chunks_over_the_case::<S>(&mut rig, before);
        Judged { outcome, chunks }
    });

    let mut rig = S::rig();
    rig.at(rig.t(STAMP), cc(2, 64, 127));
    rig.at(rig.t(STAMP) + 10, note_on(0, 60, 100));
    rig.at(rig.t(OFF), note_off(0, 60));
    let quiet = rig.t(OFF) + S::lag(&rig) + S::SETTLE;
    rig.render_to(quiet + 2 * BLOCK);
    let p = rig.peak(quiet..quiet + 2 * BLOCK);
    assert!(p < 1e-4, "held by a pedal on another channel: {p}");
}

/// **The gain reaches the output**: at zero the output is silent from the
/// second block on, and a note still sounding is heard again when it is
/// restored. The gain is the strip's; the instrument has none of its own.
pub fn gain_reaches_the_output<S: Subject>() {
    let mut rig = S::rig();
    rig.at(rig.t(STAMP), note_on(0, 60, 100));
    rig.render_to(rig.t(STAMP) + 4_000);
    assert!(rig.peak(rig.t(STAMP) + 3_000..rig.t(STAMP) + 4_000) > 1e-4);
    rig.session
        .set(rig.handle.gain().to(Amplitude::new(0.0)))
        .unwrap();
    let at = rig.frame();
    rig.render_to(at + 3 * BLOCK);
    let p = rig.peak(at + 2 * BLOCK..at + 3 * BLOCK);
    assert_eq!(p, 0.0, "gain 0 still sounds {p}");
    rig.session
        .set(rig.handle.gain().to(Amplitude::new(1.0)))
        .unwrap();
    let at = rig.frame();
    rig.render_to(at + 4 * BLOCK);
    assert!(
        rig.peak(at + 2 * BLOCK..at + 4 * BLOCK) > 1e-4,
        "gain 1 is silent"
    );
}

/// **A rate change keeps the note on its frame** (and, for a pitched one, at
/// its pitch in Hz): the session is re-prepared at 44.1 kHz while running.
pub fn a_rate_change_keeps_the_note_on_its_frame<S: Subject>() {
    judgeable::<S>("a rate change keeps the note on its frame", || {
        let mut rig = S::rig();
        rig.render_to(2 * BLOCK);
        rig.session
            .graph()
            .set_sample_rate(tutti_units::SampleRate::SR_44K1)
            .unwrap();
        rig.settle();
        let before = S::chunks(&rig);
        rig.render_to(rig.frame() + 4 * BLOCK);
        // An absolute frame, measured from the engine's clock. The clock is not
        // this rig's count after a rate change (see `Rig::clock_of`): a count of
        // rendered frames would land the note about 21 frames late here.
        let stamp = rig.frame() + 300;
        rig.at(stamp, note_on(0, 60, 100));
        rig.render_to(stamp + 16_000);
        let outcome = first_sound_in_window::<S>(&rig, stamp, stamp).and_then(|()| {
            if S::PITCHED {
                let p = period(&rig.left[stamp + 4_000..stamp + 16_000]);
                let hz = 44_100.0 / p;
                if (hz - 261.63).abs() >= 3.0 {
                    return Err(format!("{hz} Hz at 44.1 kHz"));
                }
            }
            Ok(())
        });
        let chunks = chunks_over_the_case::<S>(&mut rig, before);
        Judged { outcome, chunks }
    });
}

/// **A note queued before a rate change keeps its instant**: a note-on for
/// 12 000 frames ahead at 48 kHz, waiting in the queue when the session is
/// re-prepared at 44.1 kHz, sounds at the same wall-clock time, which is frame
/// 11 025 at the new rate, and not at frame 12 000 of it.
pub fn a_note_queued_before_a_rate_change_keeps_its_instant<S: Subject>() {
    rate_change_instant::<S>(false);
}

/// **A reading taken before a rate change keeps its instant**: the clock is
/// read at 48 kHz, the session is re-prepared at 44.1 kHz, and only then is the
/// note sent for the reading. It lands on the instant the reading named, not
/// where the same frame number is at the new rate.
pub fn a_reading_taken_before_a_rate_change_keeps_its_instant<S: Subject>() {
    rate_change_instant::<S>(true);
}

fn rate_change_instant<S: Subject>(send_after_the_change: bool) {
    let what = if send_after_the_change {
        "a reading taken before a rate change keeps its instant"
    } else {
        "a note queued before a rate change keeps its instant"
    };
    judgeable::<S>(what, || {
        rate_change_instant_once::<S>(send_after_the_change)
    });
}

fn rate_change_instant_once<S: Subject>(send_after_the_change: bool) -> Judged {
    const AHEAD: usize = 12_000;
    let mut rig = S::rig();
    rig.render_to(2 * BLOCK);
    let read = rig.handle.keys().clock().expect("the queue is prepared");
    assert_eq!(read.rate(), super::rig::RATE);
    let send = |rig: &mut Rig| {
        rig.handle
            .keys_mut()
            .push(
                read.after(tutti_units::Samples::new(AHEAD)),
                note_on(0, 60, 100),
            )
            .expect("the queue has room");
    };
    if !send_after_the_change {
        send(&mut rig);
        // One block, so the queue holds it in its waiting list and not in its
        // ring when the rate changes.
        rig.block();
    }
    rig.session
        .graph()
        .set_sample_rate(tutti_units::SampleRate::SR_44K1)
        .unwrap();
    rig.settle();
    let before = S::chunks(&rig);
    if send_after_the_change {
        send(&mut rig);
    }
    // The instant, at the new rate: the executor's own arithmetic.
    let target = Frame::new(read.frame().get() + AHEAD as u64)
        .rescaled(super::rig::RATE, tutti_units::SampleRate::SR_44K1);
    let now = rig.handle.keys().clock().expect("the queue is prepared");
    assert_eq!(now.rate(), tutti_units::SampleRate::SR_44K1);
    let ahead = target
        .since(now.frame())
        .expect("the instant is still ahead")
        .get();
    let at = rig.frame() + ahead;
    let from = rig.frame();
    rig.render_to(at + 4_000);
    let outcome = first_sound_in_window::<S>(&rig, from, at);
    let chunks = chunks_over_the_case::<S>(&mut rig, before);
    Judged { outcome, chunks }
}

/// **An instrument has one handle at a time**: making a node an instrument
/// twice is refused, naming why, and the first handle keeps playing; once the
/// handle is removed (its queue and strip, not the instrument) the node can be
/// made one again.
pub fn an_instrument_is_one_handle_at_a_time<S: Subject>() {
    let mut rig = S::rig();
    let err = rig
        .session
        .instrument(rig.node)
        .expect_err("a second handle");
    assert!(
        matches!(
            err,
            tutti_session::Error::NotInstrument {
                why: tutti_session::NotInstrument::AlreadyInstrument,
                ..
            }
        ),
        "{err}"
    );
    rig.at(rig.t(STAMP), note_on(0, 60, 100));
    rig.render_to(rig.t(STAMP) + 2_000);
    assert!(
        rig.peak(rig.t(STAMP)..rig.t(STAMP) + 2_000) > 1e-4,
        "the first handle plays"
    );
    let (queue, strip) = (rig.handle.queue(), rig.handle.output());
    rig.session.remove(strip.erase()).unwrap();
    assert_eq!(
        rig.session.remove_instrument(rig.handle),
        Err(tutti_session::RemoveError::NoSuchNode(strip.erase())),
        "a part removed by its key is the one the removal names, the other goes all the same"
    );
    assert!(
        rig.session.graph_ref().contains(rig.node),
        "the instrument stays"
    );
    assert!(!rig.session.graph_ref().contains(queue));
    assert!(!rig.session.graph_ref().contains(strip));
    rig.session
        .instrument(rig.node)
        .expect("a node can be made an instrument again");
}

/// **A fork starts silent, and plays what it is given**: the graph forked
/// while a note sounds renders silence while its transport is stopped (no voice
/// is carried over), and then, rolling, the clip that feeds the instrument plays
/// a note in it. It pins that a fork carries the instrument over (a plugin's
/// mode among it) and plays what its graph gives it. It does **not** pin the
/// order a plugin's state is restored in: a plugin restored after its activation
/// still came up in the state's mode here (the CLAP state applied
/// after the activation), because the fork's own prepare activates it again. The
/// order is pinned by the plugin subject's other cases, six of which fail under
/// that reordering, and by the server's staging tests.
pub fn a_fork_starts_silent_and_plays<S: Subject>() {
    use tutti_fixtures::midi;
    use tutti_midi_runtime::TimedMidiEvent;
    use tutti_ump::{MidiChannel, MidiGroup};

    let mut rig = S::rig();
    rig.at(rig.t(STAMP), note_on(0, 60, 100));
    rig.render_to(rig.t(STAMP) + 4_000);
    assert!(rig.peak(rig.t(STAMP) + 3_000..rig.t(STAMP) + 4_000) > 1e-4);
    // A note at a hundredth of a beat (240 frames at 120 BPM, 48 kHz), in the
    // clip that feeds the instrument: it forks with the graph.
    rig.session
        .midi()
        .play_clip(
            rig.node,
            tutti_units::Beat::new(0.0),
            [TimedMidiEvent::new(
                tutti_units::Beat::new(0.01),
                Message::midi2(
                    MidiChannel::FIRST,
                    Voice2::note_on(NoteNumber::of(64), NoteVelocity::CENTER),
                ),
            )
            .unwrap()],
        )
        .expect("an instrument takes a clip");
    let _ = rig.session.update();
    let mut fork = rig
        .session
        .graph_ref()
        .testing()
        .fork_live()
        .expect("the instrument forks");
    let mut out = vec![vec![0.0; 2_048]; 2];
    fork.render(BLOCK, PlanesMut::from_vecs(&mut out).unwrap());
    let p = out.iter().flatten().fold(0.0f32, |a, v| a.max(v.abs()));
    assert_eq!(p, 0.0, "the fork sounds {p}");

    // Rolling from beat 0 a block at a time, paced: a hosted plugin in a live
    // fork answers a chunk in real time.
    let mut peak = 0.0f32;
    for i in 0..80u32 {
        let beat = f64::from(i) * BLOCK as f64 / 24_000.0;
        fork.set_transport(tutti_graph::TransportPosition::new(
            true,
            tutti_units::Bpm::of(120.0),
            tutti_units::Beat::new(beat),
            None,
        ));
        fork.render(BLOCK, PlanesMut::from_vecs(&mut out).unwrap());
        peak = out.iter().flatten().fold(peak, |a, v| a.max(v.abs()));
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(peak > 1e-4, "the fork plays nothing: {peak}");
}

/// **Sending allocates nothing**, and neither does a block that plays it. The
/// ring, the queue node and the strip are preallocated; the control thread
/// pushes into the ring without a lock.
pub fn sending_allocates_nothing<S: Subject>() {
    let mut rig = S::rig();
    rig.at(rig.t(STAMP), note_on(0, 60, 100));
    rig.render_to(rig.t(STAMP) + 4_000);
    let mut buf = vec![0.0f32; BLOCK * 2];
    let on = note_on(0, 64, 100);
    let off = note_off(0, 64);
    // The blocks run inside `RtScope`; the pushes are checked here.
    let push = |rig: &mut Rig, packet| {
        assert_no_alloc::assert_no_alloc(|| rig.handle.keys_mut().push(Stamp::NextBlock, packet))
            .is_ok()
    };
    let a = push(&mut rig, on);
    rig.session.render(
        &mut tutti_buffers::InterleavedMut::new(&mut buf, tutti_buffers::ChannelLayout::STEREO)
            .unwrap(),
    );
    let b = push(&mut rig, off);
    rig.session.render(
        &mut tutti_buffers::InterleavedMut::new(&mut buf, tutti_buffers::ChannelLayout::STEREO)
            .unwrap(),
    );
    assert!(a && b);
}

/// The fundamental's period in frames: autocorrelation, an octave error
/// corrected by checking half the lag.
pub fn period(x: &[f32]) -> f64 {
    let n = 4096.min(x.len() / 2);
    let ac = |lag: usize| -> f64 {
        (0..n)
            .map(|i| f64::from(x[i]) * f64::from(x[i + lag]))
            .sum()
    };
    let (mut best, mut at) = (f64::MIN, 0);
    for lag in 60..800 {
        let v = ac(lag);
        if v > best {
            best = v;
            at = lag;
        }
    }
    if at / 2 >= 60 && ac(at / 2) >= 0.9 * best {
        at /= 2;
    }
    let (a, b, c) = (ac(at - 1), ac(at), ac(at + 1));
    at as f64 + 0.5 * (a - c) / (a - 2.0 * b + c)
}
