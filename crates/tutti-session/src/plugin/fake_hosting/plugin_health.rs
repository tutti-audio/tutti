//! What the session's `Phase::Plugins` does with a hosted plugin's health: the
//! engine's latched cause reaches the death report verbatim, a crash is not a
//! death until the debounce window elapses, a death removes the node and the
//! record, a live plugin is never written off, and a graph with no hosted
//! plugin is never marked edited by the phase.
//!
//! The plugin is a fake backend (`PluginHandle::from_backend`) hosted at a
//! node over `host`, so no `plugin-server` is needed; what the
//! plugin reports (`SessionEvent::Plugin`'s `ParamChanged` and the rest) is driven through the same fake's listeners. The real
//! plugin's death is `plugin.rs`'s
//! `a_plugin_dead_for_the_window_is_removed_and_reported_once`.

use super::host;
use std::sync::Arc;
use std::time::Duration;
use tutti_buffers::ChannelLayout;
use tutti_platform::Instant;

use crate::{DetachedSession, Phase, PhaseCx, PluginHealth, Session, SessionEvent};
use crate::{PluginChange, PluginEvent};
use tutti_graph::ForkByClone;
use tutti_graph::NodeKey;
use tutti_nodes::testing::Const;
use tutti_plugin_host::Liveness;
use tutti_plugin_host::PluginHandle;
use tutti_plugin_types::{IpcError, ParameterInfo, PluginFailure};
use tutti_units::Normalized;
use tutti_units::ParamId;
use tutti_units::SampleRate;

/// A backend whose liveness is whatever the test says it is.
struct FakeBackend {
    cause: Option<PluginFailure>,
}

impl tutti_plugin_types::AnyThread for FakeBackend {
    fn parameter_descriptors(&self) -> tutti_plugin_types::Answer<Option<Vec<ParameterInfo>>> {
        tutti_plugin_types::Answer::ready(None)
    }
    fn parameter_value(&self, _id: ParamId) -> tutti_plugin_types::Answer<Option<Normalized>> {
        tutti_plugin_types::Answer::ready(None)
    }

    fn set_parameter_value(
        &self,
        _id: ParamId,
        _value: Normalized,
    ) -> Result<(), tutti_plugin_types::UnknownParam> {
        Ok(())
    }
    fn bypassed(&self, _site: tutti_plugin_types::OwnBypass) -> Option<bool> {
        None
    }
    fn parameter_plain(&self, _id: ParamId, _value: Normalized) -> Option<f64> {
        None
    }
    fn parameter_normalized(
        &self,
        _id: ParamId,
        _plain: f64,
    ) -> tutti_plugin_types::Answer<Option<Normalized>> {
        tutti_plugin_types::Answer::ready(None)
    }
    fn parameter_text(&self, _id: ParamId, _value: Normalized) -> Option<String> {
        None
    }
    fn parameter_value_from_text(&self, _id: ParamId, _text: &str) -> Option<Normalized> {
        None
    }
    fn save_state(
        &self,
        _: tutti_plugin_types::StateUse,
    ) -> Result<Vec<u8>, tutti_plugin_types::StateError> {
        Err(tutti_plugin_types::StateError::NoStateRoute)
    }
    fn set_render_mode(&self, _mode: tutti_plugin_types::RenderMode) -> bool {
        false
    }
    fn set_automation_mode(
        &self,
        _mode: tutti_plugin_types::AutomationMode,
    ) -> Result<(), tutti_plugin_types::AutomationModeError> {
        Err(tutti_plugin_types::AutomationModeError::PluginDead)
    }
}

impl tutti_plugin_types::Health for FakeBackend {
    fn is_crashed(&self) -> bool {
        self.cause.is_some()
    }
    fn crash_cause(&self) -> Option<PluginFailure> {
        self.cause.clone()
    }
    fn rate_refusal(&self) -> Option<tutti_plugin_types::RateRefusal> {
        None
    }
    fn block_counts(&self) -> tutti_plugin_types::PipelineCounts {
        tutti_plugin_types::PipelineCounts::default()
    }
    fn terminate(&self, _reason: tutti_plugin_types::TerminationReason) {}
}

fn handle_with(cause: Option<PluginFailure>) -> PluginHandle {
    PluginHandle::from_backend(
        Arc::new(FakeBackend { cause }),
        None,
        tutti_plugin_types::PluginDescriptor::new("", tutti_plugin_types::PluginClass::Unknown),
        Default::default(),
        Default::default(),
    )
}

/// A detached session with a fake plugin (reporting `cause`) hosted at a dc
/// node.
fn session_with(cause: Option<PluginFailure>) -> (DetachedSession, NodeKey) {
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    let node = s.graph().add(ForkByClone(Const::mono(0.0)));
    host(
        &mut s,
        node,
        handle_with(cause),
        tutti_plugin_types::PluginRef::file("fake.clap"),
    );
    // The insert is an edit: send it, so the flag starts clear.
    s.commit();
    (s, node.erase())
}

/// `Phase::Plugins` at `t0 + ms`: the deaths it reported.
fn deaths_at(s: &mut DetachedSession, t0: Instant, ms: u64) -> Vec<(NodeKey, PluginFailure)> {
    let mut out = Vec::new();
    s.run_phase(
        Phase::Plugins,
        PhaseCx::at(t0 + Duration::from_millis(ms)),
        &mut out,
    );
    out.into_iter()
        .filter_map(|e| match e {
            SessionEvent::Plugin(PluginEvent {
                node,
                change: PluginChange::Died { cause },
                ..
            }) => Some((node, cause)),
            _ => None,
        })
        .collect()
}

/// One observation every half window, as frames at 60 Hz span it: `frames`
/// of them. The deaths each reported.
fn observe(s: &mut DetachedSession, frames: usize) -> Vec<(NodeKey, PluginFailure)> {
    let _ = tutti_fixtures::main_thread();
    let t0 = Instant::now();
    let half = Liveness::DEFAULT_WINDOW.as_millis() as u64 / 2;
    let mut died = Vec::new();
    for frame in 0..frames {
        died.extend(deaths_at(s, t0, frame as u64 * half));
    }
    died
}

/// **A graph with no hosted plugin is not edited by the phase**: a dc node
/// that is no plugin, and the phase alone (no commit, so an edit the phase
/// made would stay on the session's flag). Every node is a graph node, so a
/// phase that treated each as a plugin would mark the graph edited every
/// update and re-plan compensation forever.
#[test]
fn a_non_plugin_node_never_marks_the_graph_edited() {
    let _ = tutti_fixtures::main_thread();
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    s.graph().add(ForkByClone(Const::mono(0.0)));
    s.commit();
    assert!(
        !s.graph_ref().has_uncommitted_edits(),
        "precondition: committed"
    );
    let t0 = Instant::now();
    for ms in [0, 10, 20] {
        deaths_at(&mut s, t0, ms);
    }
    assert!(
        !s.graph_ref().has_uncommitted_edits(),
        "a dc node is not a plugin and has no latency to compensate"
    );
}

/// **The engine's latched reason reaches the death report verbatim.**
/// Asserting on the content is what distinguishes reading the engine's cause
/// from inventing one: a fixed placeholder would pass a check for a death
/// alone.
#[test]
fn a_dead_plugin_reports_the_engines_cause_not_a_placeholder() {
    let latched = PluginFailure::Connect {
        source: Arc::new(IpcError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No such file or directory",
        ))),
    };
    let (mut s, node) = session_with(Some(latched));
    let died = observe(&mut s, Liveness::DEATHS as usize);
    assert_eq!(died.len(), 1, "{died:?}");
    assert_eq!(died[0].0, node);
    let cause = &died[0].1;
    assert!(
        matches!(cause, PluginFailure::Connect { .. }),
        "the cause must be the engine's latched failure, carried through as its kind: {cause:?}"
    );
    // The `IpcError` behind it is still the `source`, not flattened to text.
    let source = std::error::Error::source(cause).expect("a connect failure has a source");
    assert!(
        matches!(source.downcast_ref::<IpcError>(), Some(IpcError::Io(e))
            if e.kind() == std::io::ErrorKind::NotFound),
        "{source:?}"
    );
}

/// **A plugin that runs in this process and panicked is reported `Died` with
/// the `Panicked` cause its lifecycle latched.** The handle half (a panic
/// out of a call makes the in-process handle read `Dead { Panicked }`) is
/// `tutti-plugin-host`'s `a_panicked_in_process_plugin_reads_dead_with_a_panicked_cause`;
/// this is the session's half, with a backend that reads dead the way that
/// handle does: the phase reports the death once, carrying the cause as it is.
#[test]
fn a_panicked_in_process_plugin_is_reported_died_with_its_panicked_cause() {
    let latched = PluginFailure::Panicked {
        source: Arc::new(tutti_plugin_types::PluginError::Other(
            "a call into the plugin panicked".into(),
        )),
    };
    let (mut s, node) = session_with(Some(latched));
    let died = observe(&mut s, Liveness::DEATHS as usize);
    assert_eq!(died.len(), 1, "{died:?}");
    assert_eq!(died[0].0, node);
    assert!(
        matches!(died[0].1, PluginFailure::Panicked { .. }),
        "{:?}",
        died[0].1
    );
    assert!(
        std::error::Error::source(&died[0].1).is_some(),
        "the lifecycle's reason is the source"
    );
}

/// **A plugin is not written off on the first bad observation.** Pinned at
/// every count below the threshold, because an off-by-one here would unwire a
/// live plugin: it reads Died (not yet written off), and
/// its node stays.
#[test]
fn a_crash_is_not_declared_until_the_debounce_elapses() {
    for frames in 1..Liveness::DEATHS as usize {
        let (mut s, node) = session_with(Some(PluginFailure::StreamClosed));
        let died = observe(&mut s, frames);
        assert!(died.is_empty(), "at {frames} observation(s): {died:?}");
        assert!(
            matches!(
                s.plugin_view(node).expect("hosted").health(),
                PluginHealth::Died { .. }
            ),
            "at {frames} observation(s) the plugin is failing, not dead"
        );
        assert!(
            s.graph_ref().contains(node),
            "a failing plugin must stay wired: unwiring it early silences a live plugin"
        );
    }
}

/// **Declaring a plugin dead removes its node and its record.**
#[test]
fn declaring_death_removes_the_node_and_the_record() {
    let (mut s, node) = session_with(Some(PluginFailure::StreamClosed));
    let died = observe(&mut s, Liveness::DEATHS as usize);
    assert_eq!(died.len(), 1, "{died:?}");
    assert!(
        !s.graph_ref().contains(node),
        "a dead plugin's node is removed"
    );
    assert_eq!(s.plugins().count(), 0, "and the session no longer hosts it");
}

/// **A plugin the engine reports as alive is never written off**, however
/// long it runs: the negative case, which none of the above would notice a
/// phase declaring death unconditionally fail.
#[test]
fn a_live_plugin_is_never_declared_dead() {
    let (mut s, node) = session_with(None);
    let died = observe(&mut s, Liveness::DEATHS as usize + 5);
    assert!(died.is_empty(), "{died:?}");
    assert!(matches!(
        s.plugin_view(node).expect("hosted").health(),
        PluginHealth::Running
    ));
    assert!(s.graph_ref().contains(node), "a healthy plugin stays wired");
}

/// A detached session with a fake plugin hosted at a dc node, and the
/// listeners its backend reports through (firing one is the plugin
/// reporting).
fn session_reporting() -> (
    DetachedSession,
    NodeKey,
    tutti_plugin_host::ParameterChangeSink,
    tutti_plugin_host::RefreshSink,
    tutti_plugin_host::InvalidateSink,
) {
    let (params, refresh, invalidate) = (
        tutti_plugin_host::ParameterChangeSink::new(),
        tutti_plugin_host::RefreshSink::new(),
        tutti_plugin_host::InvalidateSink::new(),
    );
    let handle = PluginHandle::from_backend(
        Arc::new(FakeBackend { cause: None }),
        None,
        tutti_plugin_types::PluginDescriptor::new("", tutti_plugin_types::PluginClass::Unknown),
        Default::default(),
        params.clone(),
    )
    .with_refresh_sink(refresh.clone())
    .with_invalidate_sink(invalidate.clone());
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    let node = s.graph().add(ForkByClone(Const::mono(0.0)));
    host(
        &mut s,
        node,
        handle,
        tutti_plugin_types::PluginRef::file("fake.clap"),
    );
    s.commit();
    (s, node.erase(), params, refresh, invalidate)
}

/// The plugin events one `Phase::Plugins` reported.
fn plugin_events(s: &mut DetachedSession) -> Vec<PluginEvent> {
    let mut out = Vec::new();
    s.run_phase(Phase::Plugins, PhaseCx::at(Instant::now()), &mut out);
    out.into_iter()
        .filter_map(|e| match e {
            SessionEvent::Plugin(event) => Some(event),
            _ => None,
        })
        .collect()
}

/// **A plugin's own edit of a parameter arrives as `ParamChanged`**, in the
/// order the plugin made it, with its gesture's ends, at the plugin's node: a
/// gesture is a `GestureBegan`, a change with its value and its
/// recording flag, a `GestureEnded`. A later update reports nothing again.
#[test]
fn a_plugins_own_edit_arrives_as_param_changed() {
    use tutti_plugin_types::ParamEdit;
    let _ = tutti_fixtures::main_thread();
    let (mut s, node, params, _, _) = session_reporting();
    let id = ParamId::new(7);
    let value = Normalized::clamped(0.25);
    params.fire(ParamEdit::Begin(id));
    params.fire(ParamEdit::change(id, value));
    params.fire(ParamEdit::change(id, value).unrecorded());
    params.fire(ParamEdit::End(id));
    let events = plugin_events(&mut s);
    let changes: Vec<_> = events
        .iter()
        .inspect(|e| assert_eq!(e.node, node))
        .map(|e| e.change.clone())
        .collect();
    assert!(
        matches!(
            changes.as_slice(),
            [
                PluginChange::GestureBegan { id: a },
                PluginChange::ParamChanged { id: b, value: v1, recorded: true },
                PluginChange::ParamChanged { id: c, value: v2, recorded: false },
                PluginChange::GestureEnded { id: d },
            ] if [*a, *b, *c, *d] == [id; 4] && *v1 == value && *v2 == value
        ),
        "{changes:?}"
    );
    assert!(plugin_events(&mut s).is_empty(), "each is reported once");
}

/// **A reload that left the plugin at its defaults reaches the stream** as
/// `StateNotRestored`, with the reason, and a reload that restored is not an
/// event.
#[test]
fn a_reload_that_restored_nothing_arrives_in_the_stream() {
    use tutti_plugin_types::PluginInvalidation;
    let _ = tutti_fixtures::main_thread();
    let (mut s, _, _, _, invalidate) = session_reporting();
    invalidate.fire(PluginInvalidation::Reloaded);
    invalidate.fire(PluginInvalidation::StateNotRestored {
        reason: "setState refused".into(),
    });
    let changes: Vec<_> = plugin_events(&mut s)
        .into_iter()
        .map(|e| e.change)
        .collect();
    assert!(
        matches!(
            changes.as_slice(),
            [PluginChange::StateNotRestored { reason }] if reason == "setState refused"
        ),
        "{changes:?}"
    );
}

/// **What a plugin reports besides edits reaches the same stream**: a refresh
/// is `ParamsRescanned` or `ParamTitlesChanged`, a tail and a bus change are
/// `TailChanged` and `IoChanged`. A latency it reports is not repeated here
/// (the update that gives it to the graph reports it once, as
/// `LatencyChanged`), and a rate refusal is the plugin's health.
#[test]
fn a_plugins_refreshes_and_invalidations_arrive_in_the_stream() {
    use tutti_plugin_types::{PluginInvalidation, PluginRefresh, PluginTail};
    let _ = tutti_fixtures::main_thread();
    let (mut s, _, _, refresh, invalidate) = session_reporting();
    refresh.fire(PluginRefresh::ParamValues);
    refresh.fire(PluginRefresh::ParamTitles);
    invalidate.fire(PluginInvalidation::Latency {
        samples: tutti_units::Samples::new(64),
    });
    invalidate.fire(PluginInvalidation::Tail {
        tail: PluginTail::Unbounded,
    });
    invalidate.fire(PluginInvalidation::Io);
    invalidate.fire(PluginInvalidation::RateRestored);
    let changes: Vec<_> = plugin_events(&mut s)
        .into_iter()
        .map(|e| e.change)
        .collect();
    assert!(
        matches!(
            changes.as_slice(),
            [
                PluginChange::ParamsRescanned,
                PluginChange::ParamTitlesChanged,
                PluginChange::TailChanged {
                    tail: PluginTail::Unbounded
                },
                PluginChange::IoChanged,
            ]
        ),
        "{changes:?}"
    );
}

/// **A render mode a prepare off the main thread could not apply reaches the
/// app** as `RenderModeNotApplied` with the mode the prepare carried, once, at
/// the plugin's node.
#[test]
fn a_render_mode_not_applied_arrives_in_the_stream() {
    use tutti_plugin_types::{PluginInvalidation, RenderMode};
    let _ = tutti_fixtures::main_thread();
    let (mut s, node, _, _, invalidate) = session_reporting();
    invalidate.fire(PluginInvalidation::RenderModeNotApplied {
        mode: RenderMode::Offline,
    });
    let events = plugin_events(&mut s);
    assert!(
        matches!(
            events.as_slice(),
            [PluginEvent {
                node: at,
                change: PluginChange::RenderModeNotApplied {
                    mode: RenderMode::Offline
                }
            }] if *at == node
        ),
        "{events:?}"
    );
    assert!(plugin_events(&mut s).is_empty(), "reported once");
}

/// **A graph that refuses the silent stand-in is heard, once, and the node is
/// not counted silenced.** The node is not in the graph, so the swap is refused
/// ([`ReplaceRefused::NoSuchNode`]): the session reports it as
/// `NodeNotSilenced`, and the node is no longer pending, which is what the
/// update phase asks again.
#[test]
fn a_graph_that_refuses_the_silent_stand_in_is_reported_and_the_node_is_not_silenced() {
    use crate::ReplaceRefused;
    use crate::plugin::Silence;

    let (mut s, _hosted) = session_with(None);
    let gone = s.graph().add(ForkByClone(Const::mono(0.0))).erase();
    s.remove(gone).expect("a plain node is removed");
    host(
        &mut s,
        gone,
        handle_with(None),
        tutti_plugin_types::PluginRef::file("fake.clap"),
    );

    let mut out = Vec::new();
    s.silence_node(gone, &mut out);

    assert!(
        matches!(
            out.as_slice(),
            [SessionEvent::Plugin(PluginEvent {
                node,
                change: PluginChange::NodeNotSilenced { why: ReplaceRefused::NoSuchNode(missing) },
                ..
            })] if *node == gone && *missing == gone
        ),
        "{out:?}"
    );
    assert_eq!(
        s.graph.plugins[&gone].silence,
        Silence::Refused,
        "refused is not silenced: the update phase asks only a pending node again"
    );
}

/// **A snapshot the plugin failed is counted for the app**
/// ([`Diagnostics::failed_snapshots`](crate::Diagnostics::failed_snapshots)); one
/// it took is not.
#[test]
fn a_failed_periodic_snapshot_is_counted() {
    let (mut s, node) = session_with(None);
    let token = Arc::clone(&s.graph.plugins[&node].snapshot_token);
    let diagnostics = |s: &DetachedSession| {
        s.plugin_view(node)
            .expect("hosted")
            .diagnostics()
            .failed_snapshots()
    };
    assert_eq!(diagnostics(&s), 0);
    assert!(s.land_snapshot(node, &token, Instant::now(), Ok(vec![1, 2, 3]),));
    assert_eq!(diagnostics(&s), 0, "a taken snapshot is not a failure");
    assert!(!s.land_snapshot(
        node,
        &token,
        Instant::now(),
        Err(tutti_plugin_types::StateError::Lost),
    ));
    assert_eq!(diagnostics(&s), 1);
}

/// **A round trip that panicked is counted for the app**
/// ([`Diagnostics::panicked_round_trips`](crate::Diagnostics::panicked_round_trips)),
/// and the round trip that did not panic is not.
#[test]
fn a_panicked_round_trip_is_counted() {
    let (s, node) = session_with(None);
    let panics = s.graph.plugins[&node].panicked_round_trips.writer();
    let count = |s: &DetachedSession| {
        s.plugin_view(node)
            .expect("hosted")
            .diagnostics()
            .panicked_round_trips()
    };
    assert_eq!(
        super::super::contained("fine", node, &panics, || 7),
        Some(7)
    );
    assert_eq!(count(&s), 0);
    let lost: Option<()> =
        super::super::contained("state snapshot", node, &panics, || panic!("a plugin panic"));
    assert!(lost.is_none());
    assert_eq!(count(&s), 1);
}
