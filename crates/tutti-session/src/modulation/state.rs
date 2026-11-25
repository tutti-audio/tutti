//! What the session says of its modulation: [`Problem`], the names it gives a
//! target and a source, and [`ModState`], the derived nodes with what the last
//! apply left refused or unresolved.

use std::collections::BTreeMap;
use std::time::Duration;

use tutti_buffers::Float;
use tutti_graph::{EdgeId, EventOutputPort, NodeKey, PassError};
use tutti_modulation::{ProgramCompileError, RouteKey, SourceKey, TargetKey};
use tutti_nodes::automation::LaneControls;
use tutti_platform::Instant;

use super::derived::{Applied, Derived};
use super::lower::{Feed, Refusal};
use super::{ModRouteId, ModSourceId, ModTarget, OwnFeed, OwnMods, route_id, source_id};
use crate::graph::Graph;

/// A modulation target as a [`Problem`] names it: a node's param, the mix of
/// a node or an edge's gain, as the session's declarations
/// ([`Session::mods_mut`](crate::Session::mods_mut)) named it.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum TargetName {
    /// A target the session's declarations named.
    Own(ModTarget),
}

impl std::fmt::Display for TargetName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Own(target) => target.fmt(f),
        }
    }
}

/// A modulation source as a [`Problem`] names it.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum SourceName {
    /// A source the session declared, and the node that feeds it.
    Own {
        /// The source.
        id: ModSourceId,
        /// The node it follows (a follower's input, a note envelope's MIDI).
        fed_by: Option<NodeKey>,
    },
}

impl std::fmt::Display for SourceName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Own {
                fed_by: Some(node), ..
            } => write!(f, "the source fed by node {}", node.0),
            Self::Own { id, fed_by: None } => write!(f, "source {id}"),
        }
    }
}

/// One reason a modulation is not doing what was declared, as
/// [`ModState::problems`] lists it and
/// [`SessionEvent::Modulation`](crate::SessionEvent::Modulation) reports it.
/// Its `Display` names the node and the param (or the source). The first six
/// are refusals: nothing new applied, and the graph keeps what the last apply
/// that succeeded built.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Problem {
    /// The declarations do not compile.
    Uncompilable {
        /// Why.
        cause: ProgramCompileError,
        /// The target the cause names, or the one of the route it names.
        at: Option<TargetName>,
    },
    /// A route from a source whose input is downstream of the route's own
    /// target: its edge would close a cycle.
    Cycle {
        /// The route.
        route: ModRouteId,
        /// The route's target.
        at: Option<TargetName>,
    },
    /// A native target whose node does not declare the param.
    NoSuchParam {
        /// The target.
        target: TargetName,
    },
    /// A plugin target whose node has no event input at the port its lane
    /// feeds.
    NoSuchEventInput {
        /// The target.
        target: TargetName,
    },
    /// A mix target on a node with no pass: the dry side of the mix would be
    /// a guess.
    Unplaced {
        /// The target.
        target: TargetName,
        /// What the node would need to declare.
        error: PassError,
    },
    /// A note envelope fed from an event output its node does not have.
    WrongFeed {
        /// The source.
        source: SourceName,
    },
    /// A target the last apply could not resolve: no node, a node not
    /// committed yet, or a plugin's values not read yet.
    Unresolved {
        /// The target.
        target: TargetName,
        /// How long it has been unresolved, as of the last apply.
        waited: Duration,
    },
    /// A follower the last apply could not feed.
    Unfed {
        /// The source.
        source: SourceName,
    },
}

impl Problem {
    /// The route a refusal names: a route whose follower is downstream of its
    /// own target, or one the compile refused.
    #[must_use]
    pub fn route(&self) -> Option<ModRouteId> {
        match self {
            Self::Cycle { route, .. } => Some(*route),
            Self::Uncompilable { cause, .. } => compile_route(cause).map(route_id),
            _ => None,
        }
    }
}

/// The route a compile refusal names.
fn compile_route(cause: &ProgramCompileError) -> Option<RouteKey> {
    use ProgramCompileError as C;
    match cause {
        C::DuplicateRoute(route)
        | C::UnknownSource { route }
        | C::UndeclaredTarget { route }
        | C::DepthBySignal { route } => Some(*route),
        _ => None,
    }
}

/// The target a compile refusal names.
fn compile_target(cause: &ProgramCompileError) -> Option<TargetKey> {
    use ProgramCompileError as C;
    match cause {
        C::DuplicateTarget(t)
        | C::NanRange(t)
        | C::CrossedArc(t)
        | C::LiveCurve(t)
        | C::LayerIdCollision(t)
        | C::TooManyLayers { target: t, .. }
        | C::TooManyInputs { target: t, .. } => Some(*t),
        _ => None,
    }
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let refused = |f: &mut std::fmt::Formatter<'_>, at: &Option<TargetName>| match at {
            Some(at) => write!(f, "modulation refused at {at}: "),
            None => write!(f, "modulation refused: "),
        };
        match self {
            Self::Uncompilable { cause, at } => {
                refused(f, at)?;
                write!(f, "it does not compile: {cause}")
            }
            Self::Cycle { at, .. } => {
                refused(f, at)?;
                write!(f, "its source's input is downstream of its target")
            }
            Self::NoSuchParam { target } => {
                refused(f, &Some(*target))?;
                write!(f, "its node does not declare it")
            }
            Self::NoSuchEventInput { target } => {
                refused(f, &Some(*target))?;
                write!(f, "no such event input")
            }
            Self::Unplaced { target, error } => {
                refused(f, &Some(*target))?;
                write!(f, "no mix without a pass: {error}")
            }
            Self::WrongFeed { source } => {
                write!(f, "modulation refused: {source} has a feed it cannot take")
            }
            Self::Unresolved { target, waited } => write!(
                f,
                "{target} is unresolved after {waited:?}: no node is bound to it, or the \
                 plugin's values were never read"
            ),
            Self::Unfed { source } => write!(f, "{source} cannot be fed: its node is not there"),
        }
    }
}

impl std::error::Error for Problem {}

impl OwnMods {
    /// The target a compile key names, as a [`Problem`] reports it.
    fn target_name(&self, key: TargetKey) -> Option<TargetName> {
        self.target_of(key).map(TargetName::Own)
    }

    fn source_name(&self, key: SourceKey) -> SourceName {
        SourceName::Own {
            id: source_id(key),
            fed_by: self.sources.get(&key).and_then(|s| match s.feed {
                Some(OwnFeed::Audio(node) | OwnFeed::Events(node, _)) => Some(node),
                None => None,
            }),
        }
    }

    /// The target of `route`, as a [`Problem`] reports it.
    fn route_target_name(&self, route: RouteKey) -> Option<TargetName> {
        self.target_name(self.routes.get(&route)?.to)
    }

    /// `refusal` as the [`Problem`] that names it in the session's terms.
    fn problem(&self, refusal: Refusal) -> Problem {
        let name = |t: TargetKey| {
            self.target_name(t)
                .expect("a refusal names a declared target")
        };
        match refusal {
            Refusal::Uncompilable(cause) => {
                let at = compile_target(&cause)
                    .and_then(|t| self.target_name(t))
                    .or_else(|| self.route_target_name(compile_route(&cause)?));
                Problem::Uncompilable { cause, at }
            }
            Refusal::Cycle { route } => Problem::Cycle {
                route: route_id(route),
                at: self.route_target_name(route),
            },
            Refusal::NoSuchParam(t) => Problem::NoSuchParam { target: name(t) },
            Refusal::NoSuchEventInput(t) => Problem::NoSuchEventInput { target: name(t) },
            Refusal::Unplaced { target, error } => Problem::Unplaced {
                target: name(target),
                error,
            },
            Refusal::WrongFeed(s) => Problem::WrongFeed {
                source: self.source_name(s),
            },
        }
    }

    /// The edge a target key of an edge names.
    pub(super) fn edge_of(&self, key: u64) -> Option<EdgeId> {
        crate::Graph::<f32>::edge_id(self.edge_refs.get(&key).copied()?)
    }

    /// What feeds source `source`.
    pub(super) fn feed_of(&self, source: SourceKey) -> Option<Feed> {
        Some(match self.sources.get(&source)?.feed? {
            OwnFeed::Audio(node) => Feed::Audio(node),
            OwnFeed::Events(node, port) => Feed::Events(EventOutputPort::new(node, port)),
        })
    }
}

/// The session's modulation as applied: the derived nodes, and what the last
/// apply left refused or unresolved. Read it with
/// [`Session::mod_state`](crate::Session::mod_state); [`problems`](Self::problems)
/// is where to ask why nothing modulates.
#[derive(Debug)]
pub struct ModState {
    derived: Derived,
    /// Why the declarations are refused now: the refusal's [`Problem`].
    refusal: Option<Problem>,
    /// What the last apply left unresolved: [`Problem::Unresolved`] and
    /// [`Problem::Unfed`].
    unresolved: Vec<Problem>,
    /// Since when each unresolved target has been unresolved, and whether it
    /// was reported: the warning's clock.
    unresolved_since: BTreeMap<TargetKey, (Instant, bool)>,
    warn_after: Duration,
    /// A plugin target waits for its parameter's value to be read, which no
    /// edit of the editor announces: the next update applies again to find out.
    awaiting_values: bool,
    /// The last apply staged edits and the commit that sends them has not run.
    awaiting_commit: bool,
    /// The commit about to run is the last apply's own.
    commit_is_ours: bool,
    /// An apply has run: from then on every modulation phase applies, so
    /// taking every declaration away tears down what they built.
    pub(super) applied: bool,
}

impl ModState {
    pub(crate) fn new(warn_after: Duration) -> Self {
        Self {
            derived: Derived::default(),
            refusal: None,
            unresolved: Vec::new(),
            unresolved_since: BTreeMap::new(),
            warn_after,
            awaiting_values: false,
            awaiting_commit: false,
            commit_is_ours: false,
            applied: false,
        }
    }

    /// Applies `own`'s declarations when there is something to apply: they
    /// were rebuilt (`declared`), a plugin target waits for a value, or the
    /// editor is not where the last apply left it (a node or an edge that a
    /// target waited for is an edit, which moves it). Whether it did. What the
    /// apply stages is the update's commit's.
    pub(super) fn run<F: Float>(
        &mut self,
        graph: &mut Graph<F>,
        own: &OwnMods,
        declared: bool,
        now: Instant,
        out: &mut Vec<crate::SessionEvent>,
    ) -> bool {
        let clock = graph.modulation_clock();
        if !self.derived.stale(declared, self.awaiting_values, clock) {
            return false;
        }
        let built = own.built.as_ref().expect("built");
        let applied = graph.modulate(&mut self.derived, &built.borrow(), own, clock.1);
        let committed = applied.as_ref().is_ok_and(|a| a.commit);
        self.applied(applied, own, now, out);
        // The apply recorded what it left: the next update finds nothing to
        // apply unless something else moves the editor. The commit that
        // follows adds its own (`before_commit`, `after_commit`).
        self.awaiting_commit = committed;
        // Every lane this apply built or dropped is in `derived` now: the
        // plugins' driven parameters follow it in the same update.
        #[cfg(feature = "plugin")]
        graph.refresh_driven(self);
        true
    }

    /// Before `Phase::Commit` sends: whether the editor is as the last apply
    /// left it, so that the revision the commit moves it to is the apply's own.
    pub(super) fn before_commit(&mut self, revision: u64) {
        self.commit_is_ours =
            self.awaiting_commit && self.derived.seen_revision() == Some(revision);
        self.awaiting_commit = false;
    }

    /// After `Phase::Commit`: the commit of the apply's own edits moved the
    /// revision, which the next update does not take for a change.
    pub(super) fn after_commit(&mut self, revision: u64) {
        if std::mem::take(&mut self.commit_is_ours) {
            self.derived.adopt_revision(revision);
        }
    }

    /// Sends the owed resyncs, once nothing is left to commit.
    pub(super) fn resync<F: Float>(&mut self, graph: &mut Graph<F>) {
        if self.derived.resyncs_owed() > 0 && !graph.has_uncommitted_edits() {
            graph.send_mod_resyncs(&mut self.derived);
        }
    }

    /// The lanes' retired programs, freed (`Phase::Reclaim`).
    pub(crate) fn reclaim(&self) -> usize {
        self.derived.reclaim()
    }

    /// Everything the last apply found wrong: the refusal if the declarations
    /// are refused, then each target not yet resolved and each follower not
    /// fed. Empty while the declarations apply and every lane is built.
    pub fn problems(&self) -> impl Iterator<Item = &Problem> {
        self.refusal.iter().chain(&self.unresolved)
    }

    /// `target`'s lane: its node and its controls (the knob, the range, the
    /// counters). `None` while it has none.
    pub fn lane(&self, target: TargetKey) -> Option<(NodeKey, &LaneControls)> {
        self.derived.lane(target)
    }

    /// The lanes with a knob that drive `node`'s plugin parameters, by id.
    #[cfg(feature = "plugin")]
    pub(crate) fn plugin_lanes(&self, node: NodeKey) -> crate::plugin_params::Knobs {
        self.derived
            .plugin_lanes(node)
            .map(|(id, lane)| (id, lane.clone()))
            .collect()
    }

    /// Signal source `source`'s node (a follower's, a note envelope's).
    pub fn source_node(&self, source: ModSourceId) -> Option<NodeKey> {
        self.derived.source(SourceKey(source.get()))
    }

    /// Source resyncs owed to the next commit.
    pub fn pending_resyncs(&self) -> usize {
        self.derived.resyncs_owed()
    }

    /// Resyncs refused for good (a source node gone from the plan, a poisoned
    /// editor), counted. A full queue is not one: that resync stays owed.
    pub fn resyncs_dropped(&self) -> u64 {
        self.derived.resyncs_dropped()
    }

    /// Records an apply's outcome.
    fn applied(
        &mut self,
        applied: Result<Applied, Refusal>,
        names: &OwnMods,
        now: Instant,
        out: &mut Vec<crate::SessionEvent>,
    ) {
        self.applied = true;
        match applied {
            Ok(applied) => {
                self.refusal = None;
                self.awaiting_values = applied.awaiting_values;
                let mut since = std::mem::take(&mut self.unresolved_since);
                let mut unresolved = Vec::new();
                self.unresolved_since = applied
                    .unresolved_targets
                    .iter()
                    .filter_map(|&target| {
                        // A target whose declaration is gone has nothing to name.
                        let name = names.target_name(target)?;
                        let (from, warned) = since.remove(&target).unwrap_or((now, false));
                        unresolved.push(Problem::Unresolved {
                            target: name,
                            waited: now.saturating_duration_since(from),
                        });
                        Some((target, (from, warned)))
                    })
                    .collect();
                unresolved.extend(applied.unresolved_sources.iter().map(|&source| {
                    Problem::Unfed {
                        source: names.source_name(source),
                    }
                }));
                self.unresolved = unresolved;
                self.warn(names, now, out);
            }
            Err(error) => {
                self.awaiting_values = false;
                self.refuse(names.problem(error), out);
            }
        }
    }

    /// Reports, once, each target that has been unresolved for
    /// the builder's unresolved warning time. Every update runs it: the time
    /// passes while nothing else changes.
    pub(super) fn warn(
        &mut self,
        names: &OwnMods,
        now: Instant,
        out: &mut Vec<crate::SessionEvent>,
    ) {
        let warn_after = self.warn_after;
        for (&target, (from, warned)) in &mut self.unresolved_since {
            let waited = now.saturating_duration_since(*from);
            if *warned || waited < warn_after {
                continue;
            }
            let Some(name) = names.target_name(target) else {
                continue;
            };
            *warned = true;
            let problem = Problem::Unresolved {
                target: name,
                waited,
            };
            tracing::warn!("{problem}");
            // The list holds the reading as of now.
            if let Some(held) = self
                .unresolved
                .iter_mut()
                .find(|p| matches!(p, Problem::Unresolved { target, .. } if *target == name))
            {
                *held = problem.clone();
            }
            out.push(crate::SessionEvent::Modulation(problem));
        }
    }

    /// Holds `refusal`, reporting it when it starts
    /// or changes.
    fn refuse(&mut self, refusal: Problem, out: &mut Vec<crate::SessionEvent>) {
        if self.refusal.as_ref() != Some(&refusal) {
            tracing::error!("{refusal}");
            self.refusal = Some(refusal.clone());
            out.push(crate::SessionEvent::Modulation(refusal));
        }
    }
}
