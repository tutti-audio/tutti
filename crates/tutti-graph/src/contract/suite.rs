//! [`Row`]: a node, how to excite it and what to expect, checked on every
//! [`Path`] the graph can put it on, and the `contract_tests!` macro that writes
//! one test per path.

use crate::NodeKey;
use tutti_buffers::ChannelLayout;
use tutti_buffers::MaxBlock;
use tutti_ump::{Message, NoteKeyEvent, NoteNumber, Ump, Voice2};
use tutti_units::{At, Beat, Bpm, Frame, Latency, SampleRate, Samples, UnitAddr};

use crate::contract::nodes::{Emitter, EventLog, Latent, ParamPulse, Recorder, Sum};
use crate::edit::builder::GraphBuilder;
use crate::edit::editor::Editor;
use crate::event::types::EventKind;
use crate::exec::Executor;
use crate::model::spec::{EventInputPort, EventOutputPort};
use crate::node::fork::Unforkable;
use crate::node::param::ParamIn;
use crate::node::time::{TransportChanges, TransportPosition};
use crate::node::{Node, Prepare, Resolution};

/// The rate every contract graph runs at.
pub const SAMPLE_RATE: SampleRate = SampleRate::SR_48K;

/// The `MaxBlock` every contract graph is prepared for. Above 65, so the
/// ragged schedule's 65-frame block fits, and not a multiple of anything a
/// node under test chunks by except 64.
pub const MAX_BLOCK: usize = 128;

/// The latent sibling's declared latency on the PDC paths, and so the
/// node's arrival there. Longer than [`MAX_BLOCK`], so at least one block
/// boundary falls while an excitation is in the PDC delay (the recompile
/// paths commit on it). Not a multiple of 64 or of any block size a
/// schedule uses, so the delay moves an excitation's offset: the harness
/// therefore puts excitations both where the *source* sees each of
/// [`OFFSETS`] and where the *node* does.
pub const SIBLING_LATENCY: usize = 141;

/// Where, inside its block, each excitation is put (under whole
/// [`MAX_BLOCK`] blocks; a ragged schedule moves them).
pub const OFFSETS: [usize; 6] = [0, 1, 63, 64, 100, MAX_BLOCK - 1];

/// Frames rendered before the earliest excitation, so a node's start-up
/// (a limiter's envelope, a convolver's first block) is behind it.
const WARMUP: u64 = 4 * MAX_BLOCK as u64;

/// Frames per beat at 120 BPM and [`SAMPLE_RATE`].
const FRAMES_PER_BEAT: u64 = 24_000;

/// How far after the timed start each [`Path::ScheduledBeat`] command's
/// beat is, in frames: on the start's own frame (the hardest case: the beat
/// the transport begins on, reachable only through the change inside the
/// block), a dozen frames on (usually in the start's block), and a quarter
/// beat on (well past it).
const BEAT_LEADS: [u64; 3] = [0, 12, FRAMES_PER_BEAT / 4];

/// A key no contract graph uses, for the unrelated node the
/// [`Path::RecompileUnrelated`] edit inserts.
const UNRELATED: NodeKey = NodeKey(u64::MAX - 1);

/// How a row excites its node.
#[derive(Clone, Copy, Debug)]
pub enum Excite {
    /// Deliver `kind` into event input `port`. Every path that has an event
    /// source applies: an upstream node's output, a fan-in merge, a
    /// scheduled command at a frame or a beat.
    Event {
        /// The node's event input.
        port: u16,
        /// What to deliver.
        kind: EventKind,
    },
    /// Put one sample of `amplitude` on audio input `port` (the others read
    /// silence).
    Impulse {
        /// The node's audio input.
        port: u16,
        /// The impulse's height.
        amplitude: f32,
    },
    /// Put one frame of `amplitude` on the node's param `param` through its
    /// event source (a [`ParamPulse`]: a step to `amplitude` at frame `F`
    /// and back to 0 at `F + 1`, unclamped): the compiler-owned param
    /// port's sample-accuracy case. A step at frame `F` must reach the
    /// node's param at `F + arrival`, exactly. The source is connected from
    /// the start, so its declick is over long before the excitation. Behind
    /// PDC the latent sibling feeds the node's audio input 0 and the param
    /// source is the early path, delayed to the node's arrival by the
    /// compiler. Runs the audio paths.
    Param {
        /// The node's declared param (a bare `UnitParam` is its index-0
        /// address: `param.into()`).
        param: UnitAddr,
        /// The pulse's height, in the param's raw `f64`.
        amplitude: f64,
    },
}

/// How a row finds its node's response in the watched output.
#[derive(Clone, Debug)]
pub enum Detect {
    /// The response starts at the first sample whose magnitude is above
    /// this. For a node whose response is not known sample for sample (a
    /// limiter's gain, an HRIR), and whose output before it is silence.
    Threshold(f32),
    /// The response is exactly these samples, and everything before it is
    /// exactly zero.
    Exact(Vec<f32>),
}

/// One path through the graph. Each is a separate case (see
/// [`contract_tests!`](crate::contract_tests)); a node's own rows are in the
/// node crates' `tests/executor/contract.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Path {
    /// The excitation straight in: an upstream event source, or the main
    /// input. Whole [`MAX_BLOCK`] blocks.
    Direct,
    /// Behind PDC: a latent sibling ([`Latent`], [`SIBLING_LATENCY`])
    /// merges with the excitation's path upstream of the node, so the node's
    /// arrival is [`SIBLING_LATENCY`] and the excitation is delayed to match.
    /// Excitations are put both where the source and where the node sees
    /// each of [`OFFSETS`].
    BehindPdc,
    /// Through an event fan-in merge: two sources on the node's port, the
    /// exciting one second in source order (the higher key), the first
    /// sending its event a few frames **later**: the same event, except that
    /// a note is on another key (so no retrigger policy decides the case, see
    /// `Row::other_source_kind`). Usually in the same
    /// block, so a merge that is not by offset hands the node the two out of
    /// order. Direct, and behind PDC: there the latent sibling is a third
    /// source on the same port, so both emitters' events are delayed
    /// [`SIBLING_LATENCY`] frames on their way into the merge — across one
    /// or two block boundaries — and still arrive in offset order.
    ///
    /// Both responses are expected, the second at `f + gap`, and nothing
    /// else.
    ///
    /// What this case does not catch: a merge that drops the **first**
    /// source's events, on an instrument row. An instrument's sustained first
    /// response hides the second onset, so only the ImpulseSource rows (and
    /// `a_fan_in_tie_goes_by_source_order`) hold that property; the
    /// instrument rows did not hold it with the same key either.
    ///
    /// Ties are `tests/executor/contract.rs`'s `a_fan_in_tie_goes_by_source_order`.
    EventFanIn,
    /// Behind PDC, with an unrelated node inserted and committed while the
    /// excitation is in flight (between its frame and its response). After
    /// the next block the running plan must hold the new node, so the path
    /// cannot pass having recompiled nothing.
    RecompileUnrelated,
    /// Behind PDC, with the node that feeds this one (the summing node on
    /// the audio path, the exciting source on the event path) re-inserted
    /// (a new generation) while the excitation is in flight in the delay
    /// into it. After the next block the running plan must carry the
    /// feeder's new generation.
    ///
    /// The executor closes the keys a replaced source holds (no note is left
    /// stuck), and the close travels the same delay as the note-on: it
    /// reaches the sink on the onset frame or later. A row marked
    /// [`Row::onset_release_is_silent`] (a SoundFont, whose envelope releases
    /// from 0 at the onset) cannot be held to a sound there, so the path
    /// asserts from evidence what it is about, with a recording wrapper around the
    /// node: the node was handed the note-on on exactly the expected frame and
    /// then the close of that key, and its render is silent when the close
    /// came within the row's delay stage.
    RecompileUpstreamGeneration,
    /// Direct and behind PDC, in blocks of one frame.
    Blocks1,
    /// Direct and behind PDC, in blocks of 63 frames.
    Blocks63,
    /// Direct and behind PDC, in blocks of 64 frames.
    Blocks64,
    /// Direct and behind PDC, in blocks of 65 frames.
    Blocks65,
    /// Direct and behind PDC, in [`MAX_BLOCK`] blocks.
    BlocksMax,
    /// Direct and behind PDC, in blocks of random lengths from 1 to
    /// [`MAX_BLOCK`] (a fixed seed, so a failure reproduces).
    BlocksRandom,
    /// Direct and behind PDC, the excitation delivered by
    /// [`Editor::schedule`] at `At::Frame(F)`.
    ScheduledFrame,
    /// Direct and behind PDC, the excitation delivered at `At::Beat(b)`
    /// under a transport started by a timed start inside a block (a change
    /// in [`TransportChanges`]), for a beat on the start's own frame, a
    /// dozen frames after it and a quarter beat after it.
    ScheduledBeat,
}

impl Path {
    /// Every path.
    pub const ALL: [Path; 13] = [
        Path::Direct,
        Path::BehindPdc,
        Path::EventFanIn,
        Path::RecompileUnrelated,
        Path::RecompileUpstreamGeneration,
        Path::Blocks1,
        Path::Blocks63,
        Path::Blocks64,
        Path::Blocks65,
        Path::BlocksMax,
        Path::BlocksRandom,
        Path::ScheduledFrame,
        Path::ScheduledBeat,
    ];

    /// Whether the path needs an event excitation. The audio-impulse rows
    /// run every other one.
    #[must_use]
    pub const fn needs_events(self) -> bool {
        matches!(
            self,
            Path::EventFanIn | Path::ScheduledFrame | Path::ScheduledBeat
        )
    }
}

/// The graph around the node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Topo {
    Direct,
    Pdc,
}

/// Where the excitation comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Feed {
    /// An [`Emitter`] upstream, or the main input for an impulse.
    Source,
    /// Two emitters on one port.
    FanIn,
    /// `Editor::schedule(At::Frame(F))`.
    AtFrame,
    /// `Editor::schedule(At::Beat(..))` after a timed start.
    AtBeat,
}

/// A mid-stream recompile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edit {
    None,
    Unrelated,
    Regenerate,
}

/// A node, how to excite it, and how to find its response — one row of the
/// contract suite. See the [module docs](crate::contract).
pub struct Row {
    name: String,
    make: Box<dyn Fn() -> Box<dyn Node>>,
    excite: Excite,
    detect: Detect,
    output: u16,
    latency: Option<Samples>,
    lead: u64,
    /// `Some(n)`: a note closed within `n` frames of its onset is silent.
    onset_release_silent: Option<u64>,
}

impl std::fmt::Debug for Row {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Row")
            .field("name", &self.name)
            .field("excite", &self.excite)
            .field("detect", &self.detect)
            .field("output", &self.output)
            .field("latency", &self.latency)
            .field("lead", &self.lead)
            .finish_non_exhaustive()
    }
}

impl Row {
    /// A row for the node `make` builds (a fresh one per run).
    ///
    /// # Panics
    ///
    /// If `detect` is [`Detect::Exact`] with a response that does not start
    /// with a non-zero sample (the response is found by its first sample).
    pub fn new(
        name: &str,
        make: impl Fn() -> Box<dyn Node> + 'static,
        excite: Excite,
        detect: Detect,
    ) -> Self {
        if let Detect::Exact(p) = &detect {
            assert!(
                p.first().is_some_and(|&x| x != 0.0),
                "{name}: an exact response starts with a non-zero sample (it is found by it)"
            );
        }
        Self {
            name: name.to_string(),
            make: Box::new(make),
            excite,
            detect,
            output: 0,
            latency: None,
            lead: 0,
            onset_release_silent: None,
        }
    }

    /// The node is silent for a note that ends on the frame it starts: its
    /// envelope releases from its value at the onset, which is 0 (SF2 2.04
    /// section 9.1.7 for a SoundFont) and stays 0 through its delay stage, so a
    /// note closed within `within` frames of its onset is silent. Set it only
    /// for such a node; one that still sounds (a synth with a minimum envelope)
    /// keeps requiring its response.
    ///
    /// It changes one path, [`Path::RecompileUpstreamGeneration`] on an event
    /// row. There the replaced source's held notes are closed by the executor
    /// on the frame the delayed note-on comes out, which is right (no stuck
    /// note), and a node of this kind makes no sound. The path then asserts
    /// what it is about from evidence instead of from sound: the new
    /// generation is installed (the delay ring carried), a recording wrapper
    /// around the node saw the delayed note-on on exactly the expected frame
    /// followed by the close of the same key, and the render is silent when the
    /// close is within `within` frames of the onset (so a stray onset still
    /// fails) or has its usual response when it is later. Every other path
    /// is unchanged.
    ///
    /// `within` is derived, not measured: the default delayVolEnv is -12000
    /// timecents, 1 ms, 46.9 frames at 48 kHz, during which a volume envelope is
    /// 0; a node applies an event on its control chunk (8 frames for a
    /// SoundFont), so a close up to 8 frames into the chunk reaches the voice
    /// after the envelope has been computed once more. The SoundFont row sets
    /// 48. The closes this path sees are 12, 13, 28, 41, 64, 65, 77, 78, 127
    /// and 128 frames after the onset: those up to 41 are in the delay stage
    /// and silent, those from 64 are audible. A close between 42 and 63 frames
    /// after the onset has not been seen; the delay stage ends at 46.9 frames
    /// and the chunk adds up to 8 more, so the answer there depends on the
    /// chunk, and `within` sits at 48 in that gap. Such a close would have to
    /// be looked at, not assumed.
    #[must_use]
    pub fn onset_release_is_silent(mut self, within: Samples) -> Self {
        self.onset_release_silent = Some(within.get() as u64);
        self
    }

    /// Watch output channel `channel` (the first by default).
    #[must_use]
    pub fn output(mut self, channel: u16) -> Self {
        self.output = channel;
        self
    }

    /// The node's response begins `lead` frames after the frame the contract
    /// puts its excitation's effect on, by its own DSP rather than by its
    /// timing: an instrument whose note starts from an exactly-zero sample
    /// (an envelope or oscillator starting at zero), found by its first
    /// non-zero one. Pinned as a constant, so a response that is late on
    /// some paths and not others still fails. Zero by default.
    ///
    /// Not a latency: nothing downstream is compensated for it (declare one
    /// in the node's `Shape` for that).
    #[must_use]
    pub fn with_lead(mut self, lead: Samples) -> Self {
        self.lead = lead.get() as u64;
        self
    }

    /// Also pin the node's declared latency, once prepared, to `latency`:
    /// for a node whose figure is known independently (an HRTF frame, a
    /// lookahead), so a declaration and a DSP that drift *together* still
    /// fail.
    #[must_use]
    pub fn with_expected_latency(mut self, latency: Samples) -> Self {
        self.latency = Some(latency);
        self
    }

    /// Runs `path` over every offset in [`OFFSETS`], and panics, naming the
    /// row, the path, the excitation frame and both frames, at the first
    /// response that is not exactly where the contract puts it.
    ///
    /// # Panics
    ///
    /// On a contract failure, and when `path` needs an event excitation and
    /// this row has an audio one ([`Path::needs_events`]): a row cannot pass
    /// a path it does not run.
    pub fn check(&self, path: Path) {
        let is_event = matches!(self.excite, Excite::Event { .. });
        assert!(
            is_event || !path.needs_events(),
            "{}: {path:?} needs an event excitation; this row's is an audio impulse",
            self.name
        );
        let (topos, feed, edit, blocks): (&[Topo], Feed, Edit, Option<Schedule>) = match path {
            Path::Direct => (&[Topo::Direct], Feed::Source, Edit::None, None),
            Path::BehindPdc => (&[Topo::Pdc], Feed::Source, Edit::None, None),
            Path::EventFanIn => (BOTH, Feed::FanIn, Edit::None, None),
            Path::RecompileUnrelated => (&[Topo::Pdc], Feed::Source, Edit::Unrelated, None),
            Path::RecompileUpstreamGeneration => {
                (&[Topo::Pdc], Feed::Source, Edit::Regenerate, None)
            }
            Path::Blocks1 => (BOTH, Feed::Source, Edit::None, Some(Schedule::Fixed(1))),
            Path::Blocks63 => (BOTH, Feed::Source, Edit::None, Some(Schedule::Fixed(63))),
            Path::Blocks64 => (BOTH, Feed::Source, Edit::None, Some(Schedule::Fixed(64))),
            Path::Blocks65 => (BOTH, Feed::Source, Edit::None, Some(Schedule::Fixed(65))),
            Path::BlocksMax => (
                BOTH,
                Feed::Source,
                Edit::None,
                Some(Schedule::Fixed(MAX_BLOCK)),
            ),
            Path::BlocksRandom => (BOTH, Feed::Source, Edit::None, Some(Schedule::Random)),
            Path::ScheduledFrame => (BOTH, Feed::AtFrame, Edit::None, None),
            Path::ScheduledBeat => (BOTH, Feed::AtBeat, Edit::None, None),
        };
        let schedule = blocks.unwrap_or(Schedule::Fixed(MAX_BLOCK));
        let leads: &[u64] = if feed == Feed::AtBeat {
            &BEAT_LEADS
        } else {
            &[0]
        };
        // Room before the first excitation for the longest beat lead, in
        // whole blocks, so every timed start is past the warm-up too.
        let max = MAX_BLOCK as u64;
        let lead_room = leads.iter().copied().max().unwrap_or(0).div_ceil(max) * max;
        let base = WARMUP + lead_room;
        for &topo in topos {
            let arrival = match topo {
                Topo::Direct => 0,
                Topo::Pdc => SIBLING_LATENCY as u64,
            };
            for &k in &OFFSETS {
                let k = k as u64;
                // Each offset where the excitation *starts* (its source's
                // block, under whole blocks) and, behind PDC, also where the
                // *node* sees it: `SIBLING_LATENCY` moves every offset, so
                // without the second the node would never see a block's
                // first or last frame, or a 64-frame seam.
                let mut frames = vec![base + k];
                let at_node = base + (k + max - arrival % max) % max;
                if at_node != base + k {
                    frames.push(at_node);
                }
                for f in frames {
                    for &lead in leads {
                        self.run(path, topo, feed, edit, schedule, f, lead);
                    }
                }
            }
        }
    }

    /// One graph, one excitation (two on the fan-in path), one assertion
    /// over the whole render.
    #[allow(clippy::too_many_arguments)]
    fn run(
        &self,
        path: Path,
        topo: Topo,
        feed: Feed,
        edit: Edit,
        schedule: Schedule,
        f: u64,
        lead: u64,
    ) {
        let node_shape = (self.make)().shape();
        let gap = self.fan_in_gap(node_shape.event_resolution);
        // On the fan-in path both events share one block when the gap
        // allows: the exciting one moves back rather than the later one
        // spilling into the next block.
        let max = MAX_BLOCK as u64;
        let f = if feed == Feed::FanIn && f % max + gap >= max && gap < max {
            f - gap
        } else {
            f
        };
        // On the beat path `f` is the command's beat, `lead` frames after a
        // timed start.
        let start = (feed == Feed::AtBeat).then(|| f - lead);
        let mut rig = self.build(topo, feed, f, gap);
        let want_arrival = match topo {
            Topo::Direct => 0,
            Topo::Pdc => SIBLING_LATENCY,
        };
        let ctx = format!(
            "{}: {path:?} ({topo:?}), excitation at frame {f} (the node sees offset {} in \
             whole blocks{})",
            self.name,
            (f + want_arrival as u64) % max,
            start.map_or(String::new(), |s| format!(", timed start at {s}"))
        );
        assert_eq!(
            rig.arrival.get(),
            want_arrival,
            "{ctx}: the harness built the wrong path (the node's arrival)"
        );
        if let Some(l) = self.latency {
            assert_eq!(
                rig.latency, l,
                "{ctx}: the node declares a latency other than the row pins"
            );
        }

        // Schedule the command now, when there is one: the plan is in.
        let to = EventInputPort::new(rig.node, self.event_port());
        match feed {
            Feed::AtFrame => {
                rig.ed
                    .schedule(At::Frame(Frame::new(f)), to, self.event_kind())
                    .expect("room for one command");
            }
            Feed::AtBeat => {
                rig.ed
                    .schedule(
                        At::Beat(Beat::new(lead as f64 / FRAMES_PER_BEAT as f64)),
                        to,
                        self.event_kind(),
                    )
                    .expect("room for one command");
            }
            Feed::Source | Feed::FanIn => {}
        }

        // Every frame the node is handed an excitation on.
        let arrival = rig.arrival.get() as u64;
        let mut delivered = vec![f + arrival];
        if feed == Feed::FanIn {
            delivered.push(f + gap + arrival);
        }
        let pattern_len = match &self.detect {
            Detect::Exact(p) => p.len() as u64,
            Detect::Threshold(_) => 1,
        };
        let lat = rig.latency.get() as u64;
        let last = *delivered.last().expect("one at least");
        let total = last + lat + pattern_len + 3 * max;
        let blocks = schedule.blocks(total);

        // Where each response may start: `delivered + latency`, within what
        // the node's resolution promises (`Frames(n)` within
        // `n - 1` frames either way, `Block` within the block it lands in).
        let expect: Vec<(u64, u64)> = delivered
            .iter()
            .map(|&d| {
                let tolerance = match (self.excite, rig.resolution) {
                    (Excite::Impulse { .. } | Excite::Param { .. }, _)
                    | (_, Resolution::Sample) => 0,
                    (_, Resolution::Frames(n)) => u64::from(n.max(1)) - 1,
                    (_, Resolution::Block) => {
                        let (bs, be) = block_of(&blocks, d);
                        be - bs - 1
                    }
                };
                (d + lat, tolerance)
            })
            .collect();

        let impulse_port = match self.excite {
            Excite::Impulse { amplitude, .. } => Some(amplitude),
            Excite::Event { .. } | Excite::Param { .. } => None,
        };
        let mut out = Vec::with_capacity(total as usize);
        let mut done = 0u64;
        // `Some(generation before)` from the edit until the block after it
        // has proved the recompile installed.
        let mut pending_edit: Option<u32> = None;
        let mut edited = false;
        for &n in &blocks {
            // The edit lands on the first block after the excitation's.
            if edit != Edit::None && !edited && done > f {
                assert!(
                    done < expect[0].0,
                    "{ctx}: no block boundary while the excitation is in flight"
                );
                pending_edit = Some(rig.edit(edit, self));
                edited = true;
            }
            let input: Vec<f32> = (done..done + n as u64)
                .map(|i| match impulse_port {
                    Some(a) if i == f => a,
                    _ => 0.0,
                })
                .collect();
            let (transport, changes) = transport_for(start, done, n);
            let mut block = vec![vec![0.0f32; n]; 1];
            {
                let mut refs: Vec<&mut [f32]> = block.iter_mut().map(|c| &mut c[..]).collect();
                let ins: Vec<&[f32]> = if rig.has_input {
                    vec![&input[..]]
                } else {
                    vec![]
                };
                rig.exec
                    .process_with_changes(n, &transport, &changes, &ins, &mut refs);
            }
            if let Some(before) = pending_edit.take() {
                rig.assert_installed(edit, before, &ctx);
            }
            rig.ed.collect();
            out.extend_from_slice(&block[0]);
            done += n as u64;
        }
        assert!(
            edit == Edit::None || edited,
            "{ctx}: the edit never happened"
        );
        if matches!(feed, Feed::AtFrame | Feed::AtBeat) {
            assert_eq!(
                rig.exec.late_command_count(),
                0,
                "{ctx}: the command was late"
            );
            assert_eq!(
                rig.ed.commands_outstanding(),
                0,
                "{ctx}: the command never landed"
            );
        }
        if let (Some(within), Edit::Regenerate) = (self.onset_release_silent, edit) {
            self.assert_closed_at_onset(
                &rig,
                &out,
                &expect,
                (expect[0].0 - rig.latency.get() as u64, within),
                &ctx,
            );
        } else {
            self.assert_responses(&out, &expect, &ctx);
        }
    }

    /// The recompile path of a [`Row::onset_release_is_silent`] row, from
    /// evidence. The node was handed the delayed note-on on exactly frame
    /// `onset`, then the executor's close of the same key, and nothing else
    /// (the installed generation was checked right after the edit). Where the
    /// close travels the carried delay it lands on the onset frame or a block
    /// later, depending on where the edit fell: on the onset frame the node
    /// must render silence (a stray onset fails); a block later it has sounded
    /// in between, and the usual response is expected.
    fn assert_closed_at_onset(
        &self,
        rig: &Rig,
        out: &[f32],
        expect: &[(u64, u64)],
        (onset, within): (u64, u64),
        ctx: &str,
    ) {
        let log = rig.log.as_ref().expect("the row records its node's events");
        assert_eq!(log.lost(), 0, "{ctx}: the recorder's ring overflowed");
        let seen: Vec<(u64, EventKind)> = log.iter().copied().collect();
        let kind = self.event_kind();
        let EventKind::Midi(on) = kind else {
            panic!("{ctx}: an onset-release row is excited by a note");
        };
        // The held key a note-on or off names (protocol, group, channel, note).
        let key = |u: Ump| match u.note_key_event() {
            Some(NoteKeyEvent::On(k) | NoteKeyEvent::Off(k)) => Some(k),
            _ => None,
        };
        assert_eq!(
            seen.len(),
            2,
            "{ctx}: the node should be handed the note-on and the close, and was handed {seen:?}"
        );
        assert_eq!(
            seen[0],
            (onset, kind),
            "{ctx}: the delayed note-on should reach the node on frame {onset}"
        );
        let (frame, closed) = seen[1];
        assert!(
            matches!(closed, EventKind::Midi(off) if off.is_note_off() && key(off) == key(on)),
            "{ctx}: the second event should close the key: {closed:?}"
        );
        if frame < onset + within {
            assert!(
                out.iter().all(|&x| x == 0.0),
                "{ctx}: a note closed {} frames after its onset should be silent, and the render has a sample at frame {:?}",
                frame - onset,
                out.iter().position(|&x| x != 0.0)
            );
        } else {
            assert!(
                frame > onset,
                "{ctx}: the close at {frame} is before the note-on at {onset}"
            );
            self.assert_responses(out, expect, ctx);
        }
    }

    /// Check `out` against the expected responses, each `(exact frame,
    /// tolerance)` in time order.
    ///
    /// A [`Detect::Threshold`] row is held on its first response's start:
    /// what follows (a limiter's release, an HRIR) is not known sample for
    /// sample. A [`Detect::Exact`] row is held on the **whole render**: exact
    /// silence except each expected response, so a duplicate, a missing
    /// second response or a stray sample anywhere fails.
    fn assert_responses(&self, out: &[f32], expect: &[(u64, u64)], ctx: &str) {
        let place = |e: u64, tol: u64| {
            if tol == 0 {
                format!("{e}")
            } else {
                format!("{e} (within {tol})")
            }
        };
        let lead = self.lead;
        let check_start = |got: u64, (e, tol): (u64, u64)| {
            let e = e + lead;
            assert!(
                got.abs_diff(e) <= tol,
                "{ctx}: the response starts at frame {got}; the contract puts it at {}",
                place(e, tol)
            );
        };
        match &self.detect {
            Detect::Threshold(th) => {
                let first = out.iter().position(|x| x.abs() > *th);
                let Some(first) = first else {
                    panic!(
                        "{ctx}: no response at all; expected one at frame {}",
                        place(expect[0].0 + lead, expect[0].1)
                    );
                };
                check_start(first as u64, expect[0]);
            }
            Detect::Exact(p) => {
                let mut cursor = 0usize;
                for &(e, tol) in expect {
                    let Some(at) = out[cursor..].iter().position(|&x| x != 0.0) else {
                        panic!(
                            "{ctx}: a response is missing; expected one at frame {}",
                            place(e + lead, tol)
                        );
                    };
                    let at = cursor + at;
                    check_start(at as u64, (e, tol));
                    let got = &out[at..(at + p.len()).min(out.len())];
                    assert_eq!(
                        got,
                        &p[..],
                        "{ctx}: the response at frame {at} is not the expected one"
                    );
                    cursor = at + p.len();
                }
                if let Some(extra) = out[cursor..].iter().position(|&x| x != 0.0) {
                    panic!(
                        "{ctx}: an extra response at frame {} (value {}); the contract \
                         expects silence after the last one",
                        cursor + extra,
                        out[cursor + extra]
                    );
                }
            }
        }
    }

    fn event_port(&self) -> u16 {
        match self.excite {
            Excite::Event { port, .. } | Excite::Impulse { port, .. } => port,
            Excite::Param { .. } => 0,
        }
    }

    /// The event the fan-in's first, later source sends: `kind` itself, except
    /// that a note's key is another one. Two sources sending the same key
    /// would test the node's retrigger policy (an instrument that releases a
    /// key struck again is right to drop the earlier strike), and this case
    /// is about merge order and carry, so each source's note is its own and
    /// each response is expected under any retrigger rule. Anything that is
    /// not a MIDI 2.0 note (a controller, a ramp, and a MIDI 1.0 message: rows
    /// are excited with MIDI 2.0) is sent as it is: two identical events
    /// merged are what the rows of an echoing node pin.
    fn other_source_kind(kind: EventKind) -> EventKind {
        let EventKind::Midi(ump) = kind else {
            return kind;
        };
        // The neighbouring key: one up, or one down at 127.
        let other = |note: NoteNumber| {
            NoteNumber::new(note.get() + 1).unwrap_or_else(|| NoteNumber::of(note.get() - 1))
        };
        let message = match ump.decode() {
            Ok(Message::Midi2(
                a,
                Voice2::NoteOn {
                    note,
                    velocity,
                    attribute,
                },
            )) => Message::Midi2(
                a,
                Voice2::NoteOn {
                    note: other(note),
                    velocity,
                    attribute,
                },
            ),
            Ok(Message::Midi2(
                a,
                Voice2::NoteOff {
                    note,
                    velocity,
                    attribute,
                },
            )) => Message::Midi2(
                a,
                Voice2::NoteOff {
                    note: other(note),
                    velocity,
                    attribute,
                },
            ),
            _ => return kind,
        };
        EventKind::Midi(Ump::from(message))
    }

    fn event_kind(&self) -> EventKind {
        match self.excite {
            Excite::Event { kind, .. } => kind,
            Excite::Impulse { .. } | Excite::Param { .. } => unreachable!("checked in `check`"),
        }
    }

    /// How much later the fan-in's first source sends its event: far
    /// enough that the two responses cannot overlap or swap even with each
    /// at the far edge of its resolution's tolerance, and near enough to
    /// share a block (see `run`), so a merge that is not by offset hands the
    /// node the two out of order.
    fn fan_in_gap(&self, resolution: Resolution) -> u64 {
        let p = match &self.detect {
            Detect::Exact(p) => p.len() as u64,
            Detect::Threshold(_) => 1,
        };
        let tolerance = match resolution {
            Resolution::Sample => 0,
            Resolution::Frames(n) => u64::from(n.max(1)) - 1,
            Resolution::Block => MAX_BLOCK as u64 - 1,
        };
        p + 2 * tolerance + 1
    }

    fn build(&self, topo: Topo, feed: Feed, f: u64, gap: u64) -> Rig {
        if let Excite::Param { param, amplitude } = self.excite {
            return self.build_param(topo, param, Frame::new(f), amplitude);
        }
        let node = (self.make)();
        let shape = node.shape();
        let audio = matches!(self.excite, Excite::Impulse { .. });
        let port = self.event_port();
        let (node, log) = if self.onset_release_silent.is_some() && !audio {
            let port = shape
                .event_input(port)
                .expect("the node declares the event input the row excites");
            let (recorder, log) = Recorder::wrap(node, port);
            (Box::new(recorder) as Box<dyn Node>, Some(log))
        } else {
            (node, None)
        };
        if audio {
            assert!(
                port < shape.audio_in().count(),
                "{}: audio input {port} out of range",
                self.name
            );
        } else {
            assert!(
                port < shape.event_in,
                "{}: event input {port} out of range",
                self.name
            );
        }
        let ins = if audio {
            ChannelLayout::MONO
        } else {
            ChannelLayout::EMPTY
        };
        let mut g = GraphBuilder::new(ins, ChannelLayout::MONO);
        let n = g.add(Unforkable(node));
        let mut feeder = None;
        let p = port as usize;
        if audio {
            match topo {
                Topo::Direct => {
                    g.connect_input(0, n, p);
                }
                Topo::Pdc => {
                    let lat = g.add(Unforkable(Latent::new(Latency::new(Samples::new(
                        SIBLING_LATENCY,
                    )))));
                    let sum = g.add(Unforkable(Sum));
                    g.connect(lat, 0, sum, 0).connect_input(0, sum, 1);
                    g.connect(sum, 0, n, p);
                    feeder = Some((sum, FeederKind::Sum));
                }
            }
        } else {
            let kind = self.event_kind();
            if topo == Topo::Pdc {
                let lat = g.add(Unforkable(Latent::new(Latency::new(Samples::new(
                    SIBLING_LATENCY,
                )))));
                g.connect_events(lat, 0, n, p);
            }
            match feed {
                Feed::Source => {
                    let e = g.add(Unforkable(Emitter::new(Frame::new(f), kind)));
                    g.connect_events(e, 0, n, p);
                    feeder = Some((e, FeederKind::Emitter(Frame::new(f), kind)));
                }
                Feed::FanIn => {
                    let late = g.add(Unforkable(Emitter::new(
                        Frame::new(f + gap),
                        Self::other_source_kind(kind),
                    )));
                    let e = g.add(Unforkable(Emitter::new(Frame::new(f), kind)));
                    g.connect_events(late, 0, n, p).connect_events(e, 0, n, p);
                }
                Feed::AtFrame | Feed::AtBeat => {}
            }
        }
        g.connect_output(n, self.output as usize, 0);
        let (ed, exec) = g
            .build(Prepare::new(
                SAMPLE_RATE,
                MaxBlock::of(Samples::new(MAX_BLOCK)),
            ))
            .unwrap_or_else(|e| panic!("{}: the contract graph does not build: {e}", self.name));
        let unit = exec
            .plan()
            .and_then(|p| p.unit(n))
            .expect("the node is in the plan");
        let (arrival, prepared) = (unit.arrival.samples(), unit.shape);
        Rig {
            ed,
            exec,
            node: n,
            feeder,
            arrival,
            latency: prepared.latency.samples(),
            resolution: prepared.event_resolution,
            has_input: audio,
            log,
        }
    }
}

impl Row {
    /// The [`Excite::Param`] graph: a [`ParamPulse`] armed for `at` driving
    /// `param`, and behind PDC a [`Latent`] sibling on the node's audio
    /// input 0.
    fn build_param(&self, topo: Topo, param: UnitAddr, at: Frame, amplitude: f64) -> Rig {
        let node = (self.make)();
        let shape = node.shape();
        assert!(
            shape.params.index_of(param).is_some(),
            "{}: the node does not declare {param:?}",
            self.name
        );
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
        let n = g.add(Unforkable(node));
        let pulse = ParamPulse::new(at, param, amplitude);
        let src = g.add(Unforkable(pulse));
        if topo == Topo::Pdc {
            assert!(
                shape.audio_in().count() > 0,
                "{}: behind PDC the latent sibling feeds audio input 0",
                self.name
            );
            let lat = g.add(Unforkable(Latent::new(Latency::new(Samples::new(
                SIBLING_LATENCY,
            )))));
            g.connect(lat, 0, n, 0);
        }
        g.connect_param(ParamIn { node: n, param }, EventOutputPort::new(src, 0));
        g.connect_output(n, self.output as usize, 0);
        let (ed, exec) = g
            .build(Prepare::new(
                SAMPLE_RATE,
                MaxBlock::of(Samples::new(MAX_BLOCK)),
            ))
            .unwrap_or_else(|e| panic!("{}: the contract graph does not build: {e}", self.name));
        let unit = exec
            .plan()
            .and_then(|p| p.unit(n))
            .expect("the node is in the plan");
        let (arrival, prepared) = (unit.arrival.samples(), unit.shape);
        Rig {
            ed,
            exec,
            node: n,
            feeder: Some((src, FeederKind::ParamPulse(pulse))),
            arrival,
            latency: prepared.latency.samples(),
            resolution: prepared.event_resolution,
            has_input: false,
            log: None,
        }
    }
}

const BOTH: &[Topo] = &[Topo::Direct, Topo::Pdc];

/// What feeds the node, to re-insert for [`Path::RecompileUpstreamGeneration`].
#[derive(Clone, Copy)]
enum FeederKind {
    Sum,
    Emitter(Frame, EventKind),
    ParamPulse(ParamPulse),
}

struct Rig {
    ed: Editor,
    exec: Executor,
    node: NodeKey,
    feeder: Option<(NodeKey, FeederKind)>,
    arrival: Samples,
    latency: Samples,
    resolution: Resolution,
    has_input: bool,
    /// What the node under test was handed on its event input, when the row
    /// records it (see [`Row::onset_release_is_silent`]).
    log: Option<EventLog>,
}

impl Rig {
    /// Make the edit and commit it. Returns the generation the node it
    /// proves itself on had before: the feeder's, or 0 for the unrelated
    /// insert (which had none).
    fn edit(&mut self, edit: Edit, row: &Row) -> u32 {
        let before = match (edit, self.feeder) {
            (Edit::Regenerate, Some((key, _))) => self.ed.spec().generation(key),
            _ => 0,
        };
        match edit {
            Edit::None => {}
            Edit::Unrelated => {
                // A node on no path: it touches neither the node under test
                // nor anything feeding it, so the recompile must not either.
                self.ed.insert(
                    UNRELATED,
                    "unrelated",
                    Unforkable(Latent::new(Latency::new(Samples::new(7)))),
                );
            }
            Edit::Regenerate => {
                let (key, kind) = self
                    .feeder
                    .unwrap_or_else(|| panic!("{}: no upstream node to regenerate", row.name));
                match kind {
                    FeederKind::Sum => {
                        self.ed.insert(key, "sum", Unforkable(Sum));
                    }
                    // Armed for a frame already past, so it sends nothing
                    // more: the one event in flight is the old unit's.
                    FeederKind::Emitter(at, kind) => {
                        self.ed
                            .insert(key, "emitter", Unforkable(Emitter::new(at, kind)));
                    }
                    // Armed as the old one was: it sends only the steps
                    // still ahead of it, so none is sent twice or lost.
                    FeederKind::ParamPulse(pulse) => {
                        self.ed.insert(key, "param pulse", Unforkable(pulse));
                    }
                }
            }
        }
        self.ed
            .commit()
            .unwrap_or_else(|e| panic!("{}: the mid-stream commit failed: {e}", row.name));
        before
    }

    /// After the first block past an edit: the executor runs the edited
    /// plan, so a recompile path cannot pass having recompiled nothing.
    fn assert_installed(&self, edit: Edit, before: u32, ctx: &str) {
        let plan = self.exec.plan().expect("a plan");
        match edit {
            Edit::None => {}
            Edit::Unrelated => assert!(
                plan.unit(UNRELATED).is_some(),
                "{ctx}: the unrelated insert is not in the running plan"
            ),
            Edit::Regenerate => {
                let (key, _) = self.feeder.expect("checked at the edit");
                let now = plan
                    .unit(key)
                    .expect("the feeder is in the plan")
                    .generation;
                assert!(
                    now > before,
                    "{ctx}: the feeder is still generation {now} in the running plan"
                );
            }
        }
    }
}

/// A block schedule.
#[derive(Clone, Copy, Debug)]
enum Schedule {
    Fixed(usize),
    Random,
}

impl Schedule {
    /// Blocks covering at least `total` frames.
    fn blocks(self, total: u64) -> Vec<usize> {
        let mut out = Vec::new();
        let mut done = 0u64;
        // A fixed seed: the same schedule every run.
        let mut rng = tutti_rt::SplitMix64::new(0);
        while done < total {
            let n = match self {
                Schedule::Fixed(n) => n,
                Schedule::Random => 1 + (rng.next_u64() % MAX_BLOCK as u64) as usize,
            };
            out.push(n);
            done += n as u64;
        }
        out
    }
}

/// The `[start, end)` frames of the block holding `frame`.
fn block_of(blocks: &[usize], frame: u64) -> (u64, u64) {
    let mut bs = 0u64;
    for &n in blocks {
        let be = bs + n as u64;
        if frame < be {
            return (bs, be);
        }
        bs = be;
    }
    panic!("frame {frame} is past the render");
}

/// The transport for the block `[bs, bs + n)`: stopped until `start` (when
/// there is one), then rolling from beat 0 at 120 BPM — a timed start, as a
/// change inside its block when it falls inside one.
fn transport_for(start: Option<u64>, bs: u64, n: usize) -> (TransportPosition, TransportChanges) {
    let stopped = TransportPosition::default();
    let rolling = |at: u64| {
        TransportPosition::new(
            true,
            Bpm::of(120.0),
            Beat::new((at - start.unwrap_or(0)) as f64 / FRAMES_PER_BEAT as f64),
            None,
        )
    };
    let mut changes = TransportChanges::NONE;
    let Some(s) = start else {
        return (stopped, changes);
    };
    if bs >= s {
        return (rolling(bs), changes);
    }
    if s < bs + n as u64 {
        let at = crate::node::time::Offset::of(
            Samples::new(usize::try_from(s - bs).unwrap()),
            Samples::new(n),
        );
        changes.push(at, rolling(s)).expect("one change");
    }
    (stopped, changes)
}

/// One `#[test]` per [`Path`] for a row, in a module named for it, so each
/// path is its own case and fails on its own.
///
/// `event` rows run every path; `audio` rows (an [`Excite::Impulse`]) run
/// every path but the three that need an event
/// ([`Path::needs_events`]). The row expression is evaluated once per test,
/// in a child module of the caller (so it can name the caller's items).
///
/// ```
/// use tutti_graph::contract::{Detect, Excite, Lookahead, ImpulseSource, Row};
/// use tutti_graph::EventKind;
/// use tutti_fixtures::midi;
/// use tutti_ump::{MidiChannel, NoteVelocity};
/// use tutti_units::{Latency, Samples};
///
/// fn pulse_row() -> Row {
///     Row::new(
///         "ImpulseSource",
///         || Box::new(ImpulseSource::new(Latency::ZERO)),
///         Excite::Event {
///             port: 0,
///             kind: EventKind::Midi(midi::note_on(MidiChannel::FIRST, 60, 0xC800)),
///         },
///         Detect::Exact(vec![1.0]),
///     )
/// }
///
/// fn lookahead_row() -> Row {
///     Row::new(
///         "Lookahead",
///         || Box::new(Lookahead::new(Latency::new(Samples::new(45)))),
///         Excite::Impulse { port: 0, amplitude: 1.0 },
///         Detect::Exact(vec![1.0]),
///     )
/// }
///
/// // `pulse::direct`, `pulse::behind_pdc`, … `pulse::scheduled_beat`.
/// tutti_graph::contract_tests!(event pulse => pulse_row());
/// // `lookahead::direct`, … `lookahead::blocks_random`: no event paths.
/// tutti_graph::contract_tests!(audio lookahead => lookahead_row());
/// # fn main() {}
/// ```
#[macro_export]
macro_rules! contract_tests {
    (event $name:ident => $row:expr) => {
        mod $name {
            #[allow(unused_imports)]
            use super::*;
            $crate::contract_tests!(@path $row; direct Direct, behind_pdc BehindPdc,
                recompile_unrelated RecompileUnrelated,
                recompile_upstream_generation RecompileUpstreamGeneration,
                blocks_1 Blocks1, blocks_63 Blocks63, blocks_64 Blocks64, blocks_65 Blocks65,
                blocks_max BlocksMax, blocks_random BlocksRandom,
                event_fan_in EventFanIn, scheduled_frame ScheduledFrame,
                scheduled_beat ScheduledBeat);
        }
    };
    (audio $name:ident => $row:expr) => {
        mod $name {
            #[allow(unused_imports)]
            use super::*;
            $crate::contract_tests!(@path $row; direct Direct, behind_pdc BehindPdc,
                recompile_unrelated RecompileUnrelated,
                recompile_upstream_generation RecompileUpstreamGeneration,
                blocks_1 Blocks1, blocks_63 Blocks63, blocks_64 Blocks64, blocks_65 Blocks65,
                blocks_max BlocksMax, blocks_random BlocksRandom);
        }
    };
    (@path $row:expr; $($test:ident $path:ident),* $(,)?) => {
        $(
            #[test]
            fn $test() {
                $crate::contract::Row::check(&$row, $crate::contract::Path::$path);
            }
        )*
    };
}
