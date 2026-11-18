//! The update's steps: [`Phase`] and [`PHASES`] name them, [`PhaseCx`] and [`ModInput`] carry what
//! a phase reads besides the session, and `Session::update` and `run_phase` run them,
//! reporting what they found as [`SessionEvent`]s.

use std::collections::BTreeMap;
use std::fmt::{self, Debug, Formatter};
use std::time::Instant;

use tutti_engine::StreamFaultKind;
use tutti_graph::NodeKey;
use tutti_modulation::Declarations;
use tutti_rt::MainThreadMarker;

use crate::clock::sealed::Running;
use crate::format::Sealed;
use crate::graph::Retried;
use crate::{AppKey, Clock, Committed, Session, SessionEvent};

/// One step of [`Session::update`]. [`PHASES`] is their order.
///
/// A host that runs the steps itself calls [`Session::run_phase`] once per phase, in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Phase {
    /// What the session's jobs sent lands first (`SessionBuilder::spawn_with`:
    /// each message, then each end, in arrival order). Then the pool's
    /// landings are reported ([`SessionEvent::Pool`], in the order the app's
    /// set names the entries), then its releases (by key), then crossfades
    /// that waited out a re-prepare are retried, what bounces
    /// sent back is dropped (feature `export`; and a bounce in flight for
    /// an hour of `PhaseCx::now` warned about, once), then
    /// `Editor::collect` frees what the audio thread retired (here, on the
    /// main thread) and, mid re-prepare, sends the resumed plan.
    Collect,
    /// Hosted plugins' health, latency, parameter edits and state
    /// snapshots (feature `plugin`): a dead pooled plugin's node stays at its
    /// key, silent, and a host-inserted plugin's is removed
    /// (`PluginChange::Died`), a moved latency handed to the graph
    /// (`PluginChange::LatencyChanged`), a due snapshot, a parameter
    /// read and a reload submitted to the session's spawner (their results land in
    /// the next `Phase::Collect`).
    Plugins,
    /// The plugins' render mode (features `plugin` and `export`): Offline
    /// while an export is in flight, Realtime otherwise, each plugin told on
    /// a change or when it was never told.
    RenderMode,
    /// Modulation's apply: the declarations (the
    /// host's, `PhaseCx::mods`, or the session's own, `Session::mods_mut`)
    /// onto the graph, staging what is new for the commit. A refusal is
    /// `SessionEvent::Modulation`.
    Modulation,
    /// The output policy staged since the last update, applied
    /// ([`Session::policy`]); then the commit, iff the graph was edited since
    /// the last one, and the compensation of the plan it sent (or of a re-prepare's resumed plan)
    /// published.
    Commit,
    /// Modulation's re-apply and resyncs: while a
    /// target is unresolved (its node was new before the commit), the
    /// declarations apply again and what that built is committed, so the
    /// lane plays from the update its node does; then the note-envelope
    /// resyncs the apply owes are sent.
    AfterCommit,
    /// Every output's event tap drained to its device or sink (feature `midi`:
    /// the `midi_sink` entries, and with `midi-hardware` the `midi_output` ones).
    MidiOut,
    /// The stream's faults, as [`SessionEvent::Fault`] (a real-time clock),
    /// and the device's own events (feature `device`).
    Poll,
    /// Frees, here, what the audio thread let go of in every cell the
    /// session publishes to (`Publisher::reclaim`): the project meter and the
    /// plugin slots following it, the graph's param ranges, the modulation
    /// lanes' programs, the session's clips, the hardware input's MPE
    /// request and port set. A value a block still held when it was
    /// replaced stays retired until a scan after the block ends; a cell
    /// that is not published again gets that scan here. A lock and a
    /// length check per cell when nothing is retired. It also closes the
    /// graph's taps whose readers were dropped
    /// ([`Taps::open`](crate::Taps::open),
    /// [`Taps::open_event`](crate::Taps::open_event)), and emits what the
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
    Phase::AfterCommit,
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
    /// [`Modulation`](Phase::Modulation) and
    /// [`AfterCommit`](Phase::AfterCommit) phases.
    pub mods: Option<ModInput<'a>>,
    /// The proof the calling thread is the main thread, when it is: the work
    /// only that thread may do (the retired plugins' drops, the editors'
    /// service, the work other threads handed over, the pool's main stages)
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
    pub keys: &'a dyn crate::ModKeys,
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
    /// handed over, and the pool's main stages) runs when it is called on the
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
        // What a `Job::wait` updated through comes first: it happened first.
        let mut out = self.jobs.take_held();
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
                // First, so what a job landed is in the graph this update commits.
                self.land_jobs(out);
                // What those landings and the holds' ends changed, and the pool's
                // stages on this thread.
                self.pool_events(cx.main, out);
                // What the graph's own removals left declared.
                self.forget_removed();
                // Before the collect, as a frame's spawns run before its
                // commit: a crossfade lands on the update that first finds
                // the re-prepare resumed.
                for retried in self.graph.retry_crossfades() {
                    out.push(match retried {
                        Retried::Landed { node, id, controls } => SessionEvent::CrossfadeLanded {
                            node,
                            id,
                            controls: crate::CrossfadeControls::new(controls),
                        },
                        Retried::Failed { node, id, refused } => {
                            SessionEvent::CrossfadeFailed { node, id, refused }
                        }
                    });
                }
                // What an export left behind (a render's fork, an edit
                // that never ran) drops here, on the main thread.
                #[cfg(feature = "export")]
                {
                    self.bounce.drop_leftovers();
                    self.bounce.watch_stall(cx.now);
                }
                let repreparing = self.graph.is_repreparing();
                self.graph.collect();
                self.sync_root_refusal();
                // Closes a plugin that was dropped off its own thread (a pool thread at an
                // export's end, say): it handed the close to this session's collector, which
                // runs on the main thread (`PhaseCx::main` is `Some` only there).
                #[cfg(feature = "plugin")]
                if let Some(main) = cx.main {
                    self.main_work.run(main);
                }
                self.report_event_losses(out);
                self.report_silenced(out);
                // Last: the landings this update's own stages queued.
                self.jobs.report_landed(out);
                // A re-prepare resumed in that collect: it sent the resumed
                // plan, whose shapes are new (a lookahead is a time, so a
                // rate change moves latencies).
                self.resumed |= repreparing && !self.graph.is_repreparing();
            }
            Phase::Commit => {
                self.apply_output_policy();
                let sent = self.graph.is_edited() && self.commit_reporting(out);
                out.extend(
                    self.graph
                        .announce_reopened()
                        .into_iter()
                        .map(|(id, topology)| SessionEvent::RootReaderReopened { id, topology }),
                );
                for u in self.graph.take_unwired() {
                    out.push(SessionEvent::Unwired {
                        node: u.node,
                        edges: u.edges,
                        event_edges: u.event_edges,
                        bus_edges: u.bus_edges,
                    });
                }
                self.report_rewired(out);
                if !sent && self.resumed {
                    self.publish_sent();
                }
                self.resumed = false;
            }
            Phase::Modulation => self
                .mods
                .modulation_phase(&mut self.graph, cx.mods, cx.now, out),
            Phase::AfterCommit => {
                // The re-apply's commit, as the update's own: published
                // on `Sent`.
                if self.mods.reapply(&mut self.graph, cx.mods, cx.now, out) {
                    self.commit_reporting(out);
                }
                self.mods.resync(&mut self.graph);
                self.report_rewired(out);
            }
            Phase::MidiOut => {
                #[cfg(feature = "midi")]
                {
                    if self.midi_state.midi_out_phase(&mut self.graph, cx.now) {
                        self.commit();
                    }
                }
            }
            Phase::Plugins => C::Float::serve_plugins(self, cx, out),
            Phase::RenderMode => C::Float::announce_render_mode(self),
            Phase::Poll => {
                // Who the session is on the wire is in force, and the devices
                // and the tempo the session follows are read, before the
                // stream's own poll.
                // Who the session is on the wire is in force before the devices
                // are stepped.
                #[cfg(feature = "midi-hardware")]
                self.apply_midi_policy();
                // A device that connected is heard from the plan that holds
                // its inlet: committed now, not an update later.
                #[cfg(feature = "midi-hardware")]
                if self.midi_state.midi_input_phase(
                    self.midi_io.as_mut(),
                    self.midi.as_ref(),
                    &mut self.graph,
                    cx.now,
                    out,
                ) {
                    self.commit();
                }
                // The tempo the session follows is read after the devices:
                // what a device sent in the block before is in its tap.
                #[cfg(feature = "midi")]
                {
                    let follow = crate::midi_follow::FollowCx {
                        settings: self.transport.settings(),
                        meter: self.transport.settings().project_meter(),
                        playhead: self.playhead(),
                    };
                    let tempo_edited = self.midi_state.follow_phase(&follow, &mut self.graph, out);
                    let (actions, clock_edited) =
                        self.midi_state.clock_phase(&follow, &mut self.graph, out);
                    let (machine_actions, machine_edited) =
                        self.midi_state.machine_phase(&follow, &mut self.graph, out);
                    if tempo_edited || clock_edited || machine_edited {
                        self.commit();
                    }
                    let refused: u64 = actions
                        .into_iter()
                        .chain(machine_actions)
                        .map(|action| u64::from(!self.send_clock_action(action)))
                        .sum();
                    if refused > 0 {
                        out.push(crate::MidiReport::ClockCommandsRefused(refused).into());
                    }
                }
                out.append(&mut self.carried_events);
                let lost = self.fault_poll(out);
                #[cfg(feature = "device")]
                self.listing_poll(cx.now, out);
                Running::poll(&mut self.running, cx.now, lost, out);
            }
            Phase::Reclaim => {
                // Any thread that owns the session: what it frees is
                // tables, meters, programs and clips, never a unit. A tap
                // or meter whose reader was dropped goes out of the next plan.
                self.graph.reap_taps();
                self.graph.reap_event_taps();
                self.graph.reap_meters();
                if let Some((taps, _)) = &self.heard {
                    taps.reclaim();
                }
                self.reclaim();
                self.graph.drain_rt_log();
                // What the main thread owns of the retired and the open.
                #[cfg(feature = "plugin")]
                if let Some(main) = cx.main {
                    self.graph.graveyard.exhume(main);
                    self.graph.graveyard.service(main);
                }
            }
        }
    }

    /// The blocks the engine silenced since the last update, as one
    /// [`SessionEvent::BlocksSilenced`]. The meter's count is cumulative
    /// (and zeroed by a reset), so the rise is read against the last
    /// reading; a reset restarts from the new count.
    fn report_silenced(&mut self, out: &mut Vec<SessionEvent>) {
        let Some(load) = &self.load else { return };
        let now = load.peek().silenced;
        let rise = now.saturating_sub(self.silenced_seen);
        self.silenced_seen = now;
        if rise > 0 {
            out.push(SessionEvent::BlocksSilenced(rise));
        }
    }

    /// The events the graph's ports lost since the last update, as
    /// [`SessionEvent::EventsDropped`], one per port.
    fn report_event_losses(&mut self, out: &mut Vec<SessionEvent>) {
        let mut sums: BTreeMap<(NodeKey, Option<u16>), (u64, u64)> = BTreeMap::new();
        for loss in self.graph.take_event_losses() {
            let sum = sums.entry((loss.node, loss.port)).or_default();
            sum.0 += u64::from(loss.count);
            sum.1 += u64::from(loss.recovered);
        }
        out.extend(
            sums.into_iter()
                .filter(|(_, (count, recovered))| *count > 0 || *recovered > 0)
                .map(
                    |((node, port), (count, recovered))| SessionEvent::EventsDropped {
                        node,
                        port,
                        count,
                        recovered,
                    },
                ),
        );
        let elsewhere = self.graph.take_unreported_event_losses();
        if elsewhere > 0 {
            out.push(SessionEvent::EventsDroppedElsewhere(elsewhere));
        }
    }

    /// Pushes the nodes the commits since the last report rewired.
    fn report_rewired(&mut self, out: &mut Vec<SessionEvent>) {
        out.extend(
            self.graph
                .take_rewired()
                .into_iter()
                .map(|r| SessionEvent::Rewired {
                    node: r.node,
                    was: r.was,
                    now: r.now,
                }),
        );
    }

    /// Commits every edit since the last commit, now, and on `Sent`
    /// publishes the plan's compensation. For a host that must send an edit
    /// before the next update; [`update`](Self::update) commits iff the
    /// graph was edited.
    pub fn commit(&mut self) -> Committed {
        let committed = self.graph.commit();
        if committed == Committed::Sent {
            self.publish_sent();
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

    /// Publish the compensation of the plan sent last: the per-channel
    /// pre-roll to [`compensation`](Self::compensation), the total to
    /// [`latency`](Self::latency). Nothing before the first plan.
    pub(crate) fn publish_sent(&mut self) {
        if let Some(sent) = self.graph.sent_compensation() {
            self.latency = sent.total();
            self.compensation.set(sent.channels());
        }
    }

    /// Reports the newest fault the stream raised since the last poll, if the
    /// clock has a stream. Whether the stream was lost: its device left.
    ///
    /// The message is *taken* from the record: a fault is reported once, by
    /// its own kind, and a later fault that only bumps the count (an overrun
    /// the record keeps no message for beside a latched panic) is not read as
    /// the old message again.
    pub(crate) fn fault_poll(&mut self, out: &mut Vec<SessionEvent>) -> bool {
        let Some(stream) = Running::stream(&self.running) else {
            return false;
        };
        let faults = stream.faults();
        let count = faults.count();
        if count == self.faults_seen {
            return false;
        }
        // A take that lags (a record mid-write) retries on the next poll; an
        // empty one (a counted fault whose text was lost) is seen.
        let Ok(taken) = faults.take_last() else {
            return false;
        };
        self.faults_seen = count;
        let Some(fault) = taken else {
            return false;
        };
        let lost = fault.kind == StreamFaultKind::Disconnected;
        out.push(SessionEvent::Fault(fault));
        lost
    }
}
