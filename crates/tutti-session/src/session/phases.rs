//! The update's steps: [`Phase`] and [`PHASES`] name them, [`PhaseCx`] and [`ModInput`] carry what
//! a phase reads besides the session, and `Session::update` and `run_phase` run them,
//! reporting what they found as [`SessionEvent`]s.

use std::fmt::{self, Debug, Formatter};
use tutti_platform::Instant;

use tutti_engine::OutputQuantum;
use tutti_modulation::Declarations;
use tutti_platform::MainThreadMarker;

use super::parts::MidiPart;
use crate::clock::sealed::Running;
use crate::format::Sealed;
use crate::{AppKey, Clock, Committed, Session, SessionEvent};

/// One step of [`Session::update`]. [`PHASES`] is their order.
///
/// A host that runs the steps itself calls [`Session::run_phase`] once per phase, in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Phase {
    /// The bells tick, then what the session's takes reported lands
    /// ([`TakeEvent`](crate::TakeEvent)s, in arrival order). Then crossfades
    /// that waited out a re-prepare are retried, what bounces
    /// sent back is dropped (feature `export`; and a bounce in flight for
    /// an hour of `PhaseCx::now` warned about, once), then
    /// `Editor::collect` frees what the audio thread retired (here, on the
    /// main thread) and, mid re-prepare, sends the resumed plan.
    Collect,
    /// Hosted plugins' health, latency, parameter edits and state
    /// snapshots (feature `plugin`): what the plugin tasks sent lands first; a dead
    /// loaded plugin's node stays at its
    /// key, silent, and a host-inserted plugin's is removed
    /// (`PluginChange::Died`), a moved latency handed to the graph
    /// (`PluginChange::LatencyChanged`), a due snapshot, a parameter
    /// read and a reload started on the session's executor (their results land in
    /// the next `Phase::Plugins`).
    Plugins,
    /// The plugins' render mode (features `plugin` and `export`): Offline
    /// while an export is in flight, Realtime otherwise, each plugin told on
    /// a change or when it was never told.
    RenderMode,
    /// The values written since the last update that wait for it published
    /// (the tempo map, the meter, the loop region, a clip's events, a
    /// harmony node's changes, a modulation route's polarity and curve, an
    /// LFO's rate, a macro's value), together and in a fixed order: the
    /// tempo, the meter, the loop, then the rest by what they are on. Then
    /// modulation's apply: the declarations (the host's, `PhaseCx::mods`,
    /// or the session's own, `Session::mods_mut`, with the published values
    /// in them) onto the graph, staging what is new for the commit, so a
    /// program value lands as one republish in the update that wrote it. A
    /// refusal is `SessionEvent::Modulation`.
    Modulation,
    /// The output policy staged since the last update, applied
    /// ([`Session::policy`]); then the commit, iff the graph was edited since
    /// the last one, and the compensation of the plan it sent (or of a
    /// re-prepare's resumed plan) published, then the note-envelope resyncs
    /// the modulation apply owes. The update's one plan carries what the
    /// apply staged, a lane with its target.
    Commit,
    /// Every output's event tap drained to its device or sink (feature `midi`:
    /// the sinks, and with `midi-hardware` the output ports).
    MidiOut,
    /// The stream's faults, as [`SessionEvent::Fault`] (a real-time clock),
    /// and the device's own events (feature `device`).
    Poll,
    /// Frees, here, what the audio thread let go of in every cell the
    /// session publishes to (`Publisher::reclaim`): the project meter and the
    /// plugin slots following it, the graph's param ranges, the modulation
    /// lanes' programs, the session's clips, the session's MPE mode, the
    /// hardware input's Profile zones and port set. A value a block still held when it was
    /// replaced stays retired until a scan after the block ends; a cell
    /// that is not published again gets that scan here. A lock and a
    /// length check per cell when nothing is retired. It also closes the
    /// graph's taps whose readers were dropped
    /// ([`Taps::open_ports`](crate::Taps::open_ports),
    /// [`Taps::open_event`](crate::Taps::open_event)), closes the heard tap
    /// whose reader was dropped, pushes the heard tap's pending install
    /// ([`Session::open_heard_tap`](crate::Session::open_heard_tap)), and emits what the
    /// graph's nodes logged on the audio thread as `tracing` events.
    Reclaim,
}

/// Every [`Phase`], in the order [`Session::update`] runs them.
pub const PHASES: &[Phase] = &[
    Phase::Collect,
    Phase::Plugins,
    Phase::RenderMode,
    Phase::Modulation,
    Phase::Commit,
    Phase::MidiOut,
    Phase::Poll,
    Phase::Reclaim,
];

/// What one phase reads besides the session.
#[derive(Clone, Copy, Debug)]
pub struct PhaseCx<'a> {
    /// The time thresholds are measured on (a test passes its own). On the OS
    /// clock: it is also the time the MIDI outputs are drained at, which their
    /// sends are timed against, so it may be stale by the phases that ran
    /// before the drain.
    pub now: Instant,
    /// The host's modulation declarations and the resolver for its keys;
    /// `None`: the session's own ([`Session::mods_mut`]). Read by the
    /// [`Modulation`](Phase::Modulation) phase.
    pub mods: Option<ModInput<'a>>,
    /// The proof the calling thread is the main thread, when it is: the work
    /// only that thread may do (the retired plugins' drops, the editors'
    /// service, the work other threads handed over)
    /// runs in a phase that holds one and waits otherwise. [`PhaseCx::at`]
    /// asks the OS ([`MainThreadMarker::new`](MainThreadMarker::new)).
    pub main: Option<MainThreadMarker>,
}

impl PhaseCx<'_> {
    /// A context at `now`, with the session's own declarations, on the thread
    /// the OS says is the main one, if this is it.
    pub fn at(now: Instant) -> Self {
        Self {
            now,
            mods: None,
            main: MainThreadMarker::new(),
        }
    }
}

impl<'a> PhaseCx<'a> {
    /// The same context with the host's declarations.
    #[must_use]
    pub fn with_mods(mut self, mods: ModInput<'a>) -> Self {
        self.mods = Some(mods);
        self
    }
}

/// A host's modulation declarations, borrowed for one phase, and how its
/// keys resolve to nodes.
#[derive(Clone, Copy)]
pub struct ModInput<'a> {
    /// The declarations, as `tutti_modulation` takes them.
    pub declarations: Declarations<'a>,
    /// The host's keys, resolved.
    pub keys: &'a dyn tutti_nodes::ModHost,
}

impl Debug for ModInput<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModInput").finish_non_exhaustive()
    }
}

impl<K: AppKey, C: Clock> Session<K, C> {
    /// Runs every phase in [`PHASES`] order at `Instant::now()` with the
    /// session's own declarations, and returns what it found.
    ///
    /// This is the one update. What only the main thread may do (dropping the
    /// plugins the session retired, closing the editors of those, servicing
    /// the open `Editor`s (feature `plugin`), running the work other threads
    /// handed over) runs when it is called on the
    /// thread the OS says is the main one; called elsewhere that work waits
    /// for an update on that thread. A host whose main thread is not the OS's
    /// calls [`update_on`](Self::update_on).
    #[must_use = "the events carry faults and dropouts (a refused commit, a plugin that died, a lost block): handle them, or bind `let _events` with a reason"]
    pub fn update(&mut self) -> Vec<SessionEvent> {
        self.update_in(PhaseCx::at(Instant::now()))
    }

    /// [`update`](Self::update) on a thread that stands in for the main thread:
    /// a host whose UI thread is not the process's main one, or a test.
    #[must_use = "the events carry faults and dropouts (a refused commit, a plugin that died, a lost block): handle them, or bind `let _events` with a reason"]
    pub fn update_on(&mut self, main: MainThreadMarker) -> Vec<SessionEvent> {
        let mut cx = PhaseCx::at(Instant::now());
        cx.main = Some(main);
        self.update_in(cx)
    }

    pub(super) fn update_in(&mut self, cx: PhaseCx<'_>) -> Vec<SessionEvent> {
        let mut out = Vec::new();
        for &phase in PHASES {
            self.run_phase(phase, cx, &mut out);
        }
        out
    }

    /// Runs one phase, pushing what it found onto `out`. A host running the
    /// phases itself runs them in [`PHASES`] order, once each per update.
    pub fn run_phase(&mut self, phase: Phase, cx: PhaseCx<'_>, out: &mut Vec<SessionEvent>) {
        match phase {
            Phase::Collect => {
                // The bells wake what the audio thread rang for, and the polled
                // executor runs what was woken, so what those futures sent lands now.
                self.sampler.wake();
                self.background.poll();
                // First of the landings, so what a take reported is in this update's
                // events.
                self.inputs.land_takes(out);
                // What the graph's own removals left declared.
                self.forget_removed();
                // Before the collect, as a frame's spawns run before its
                // commit: a replacement lands on the update that first finds
                // the re-prepare resumed.
                self.graph.report_replacements(out);
                // What an export left behind (a render's fork, an edit
                // that never ran) drops here, on the main thread.
                self.bounce.collect(cx.now);
                // A re-prepare resumed in this collect sent the resumed plan, whose
                // shapes are new (a lookahead is a time, so a rate change moves
                // latencies): its figures are published at the commit.
                self.graph.collect_noting_resume();
                self.sync_main_output_refusal();
                // Closes a plugin that was dropped off its own thread (a pool thread at an
                // export's end, say): it handed the close to this session's collector, which
                // runs on the main thread (`PhaseCx::main` is `Some` only there).
                self.plugins.run_main_work(cx.main);
                self.graph.report_event_losses(out);
                self.output.report_silenced(out);
            }
            Phase::Commit => {
                self.apply_output_policy(out);
                let sent = self.graph.has_uncommitted_edits() && self.commit_reporting(out);
                self.graph.report_commit(out);
                self.graph.report_rewired(out);
                if self.graph.take_resumed() && !sent {
                    self.graph.publish_sent();
                }
                // A scheduled command waits for the plan sent before it, so
                // the resyncs follow it here.
                self.mods.resync(&mut self.graph);
            }
            Phase::Modulation => {
                // First: the apply reads the program values this update
                // publishes, and the plan the commit sends and the blocks
                // after it read the rest.
                self.values.publish_pending();
                self.graph.apply_controls();
                self.mods
                    .modulation_phase(&mut self.graph, cx.mods, cx.now, out);
            }
            Phase::MidiOut => {
                if self.midi.out_phase(&mut self.graph, cx.now) {
                    self.commit();
                }
            }
            Phase::Plugins => C::Float::serve_plugins(self, cx, out),
            Phase::RenderMode => C::Float::announce_render_mode(self),
            Phase::Poll => {
                // Who the session is on the wire is in force and the devices are
                // read before the stream's own poll; a device that connected is
                // heard from the plan that holds its inlet: committed now, not an
                // update later.
                if self.midi.poll_devices(&mut self.graph, cx.now, out) {
                    self.commit();
                }
                // The tempo the session follows is read after the devices:
                // what a device sent in the block before is in its tap.
                let followed = self.midi.poll_follow(
                    &self.transport,
                    self.transport.handle.playhead(),
                    &mut self.graph,
                    out,
                );
                if followed.edited() {
                    self.commit();
                }
                MidiPart::send_follow(followed, &self.transport.handle, out);
                self.reports.drain_carried(out);
                let lost = self.fault_poll(out);
                self.devices.poll(cx.now, out);
                // A prepared quantum past `OutputQuantum`'s range cannot have
                // come from a stream: it counts as no callback size.
                let prepared = self
                    .graph
                    .prepared()
                    .quantum()
                    .and_then(|block| OutputQuantum::try_from(block).ok());
                Running::poll(&mut self.running, cx.now, lost, prepared, out);
            }
            Phase::Reclaim => {
                // Any thread that owns the session: what it frees is
                // tables, meters, programs and clips, never a unit. A tap
                // or meter whose reader was dropped goes out of the next plan.
                self.graph.prune_taps();
                self.graph.prune_event_taps();
                self.graph.prune_meters();
                self.output.reclaim();
                self.reclaim();
                self.graph.drain_rt_log();
                // What the main thread owns of the retired and the open.
                self.plugins.service_retired(&mut self.graph, cx.main);
            }
        }
    }

    /// Commits every edit since the last commit, now, and on `Sent`
    /// publishes the plan's compensation. For a host that must send an edit
    /// before the next update; [`update`](Self::update) commits iff the
    /// graph was edited.
    pub fn commit(&mut self) -> Committed {
        self.graph.apply_controls();
        let committed = self.graph.commit();
        if committed == Committed::Sent {
            self.graph.publish_sent();
            // After the send: the nodes that gained a receiver in this
            // commit are told, and send it their setup from the next block.
            let gained = self.graph.take_gained_receivers();
            self.midi.receivers_gained(&self.graph, &gained);
        }
        committed
    }

    /// [`commit`](Self::commit), a refusal pushed onto `out`: whether it sent.
    fn commit_reporting(&mut self, out: &mut Vec<SessionEvent>) -> bool {
        match self.commit() {
            Committed::Sent => true,
            Committed::Refused(e) => {
                if self.graph.newly_refused(&e) {
                    out.push(SessionEvent::CommitRefused(e));
                }
                false
            }
            Committed::Retry => false,
        }
    }

    /// Reports the newest fault the stream raised since the last poll, if the
    /// clock has a stream. Whether the stream was lost: its device left.
    pub(crate) fn fault_poll(&mut self, out: &mut Vec<SessionEvent>) -> bool {
        self.reports.faults(Running::stream(&self.running), out)
    }
}
