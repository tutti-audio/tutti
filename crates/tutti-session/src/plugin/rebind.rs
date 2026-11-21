//! The node of a hosted plugin that keeps a rebinder (loaded, or inserted or
//! replaced through `Graph`) re-inserted when the plugin changes its ports.
//!
//! `Phase::Plugins` compares the ports such a plugin has now with the ones
//! its node declares. When they differ it asks the plugin's
//! [`PluginRebinder`](tutti_plugin::catalog::PluginRebinder) to **prepare** a
//! node over the same plugin, which changes nothing in the plugin, swaps it in
//! at the same key (`Graph::replace_rebound`), and only when the swap was made
//! **commits** the move of the plugin's Processor to it. A swap that is refused
//! leaves the plugin as it was and is reported
//! ([`PluginChange::PortsRefused`]), once: the ports count as handled.
//!
//! The wiring to the ports the new shape keeps stays, and the wiring to the
//! ports it lacks is reported as [`SessionEvent::Unwired`] by the update that
//! commits the swap. The plugin keeps running. Its Processor is recalled at the
//! commit, returned by the node it replaces at that node's next block (or when
//! the node is dropped, since the recall is raised), and lent to the new node at
//! the next update's pump: the plugin is silent from the swap to then, at least
//! one block and one update.
//!
//! A [`Bypassed`](crate::NodeMode::Bypassed) node is parked and not rendered,
//! but the executor still calls its
//! [`poll_parked`](tutti_graph::Node::poll_parked) each block, which answers the
//! recall, so a port change, a restart or a rate change completes while the
//! bypass lasts. A plugin with a bypass of its own keeps running and is not
//! affected.
//!
//! The record is `Settling` until the rebinder says
//! [`Moved`](tutti_plugin_host::Settlement::Moved), or
//! [`Lost`](tutti_plugin_host::Settlement::Lost) when the plugin died (which the
//! plugin's own path reports, and both nodes then render silence); no second
//! rebuild starts before. A plugin that is deactivated when the commit lands
//! keeps the record `Settling`, and holds off further port changes, until it is
//! activated again and lends its Processor to the new node.
//!
//! An in-process plugin changes its ports on the main thread and is ticked
//! there, so the rebuild runs in an update that runs on it: `plugins_phase` does
//! not tick an in-process plugin (only the editors' service does, for an open
//! one), so this phase does, for each hosted plugin that keeps a rebinder
//! (loaded, or inserted or replaced through `Graph`). An out-of-process plugin is
//! ticked the same way: it restarts into a new slab, pipeline and node for its
//! new buses.

use thiserror::Error;
use tutti_plugin_host::{RebindError, Settlement};
use tutti_plugin_types::MainThreadMarker;

use super::{PluginChange, PluginEvent, RebindState};
use crate::{Clock, CrossfadeRefused, ReplaceRefused, Session, SessionEvent};
use tutti_graph::NodeKey;

/// Why a plugin's ports were not re-inserted ([`PluginChange::PortsRefused`]).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum PortsRefusal {
    /// The plugin's ports cannot be a node: too wide for a block
    /// ([`TooWide`](RebindError::TooWide)). The plugin is untouched.
    /// (A plugin that is not running or restarting is asked again at the next
    /// update, not refused.)
    #[error(transparent)]
    Ports(#[from] RebindError),
    /// The graph refused the swap (a poisoned graph, a node that is gone, an
    /// editor that refuses it); the plugin is untouched, since it is touched only
    /// once the node is in.
    #[error("the graph refused the node over the new ports: {0}")]
    Swap(CrossfadeRefused),
}

/// `f32` because a plugin format is `f32` at its FFI.
impl<K: crate::AppKey, C: Clock<Float = f32>> Session<K, C> {
    /// Ticks each hosted plugin that keeps a rebinder (loaded, or inserted or
    /// replaced through `Graph`); the tick is what reads a change of ports.
    /// Re-inserts the node of each whose ports changed. Called at the end of
    /// `Phase::Plugins`.
    pub(crate) fn rebind_plugins(
        &mut self,
        main: Option<MainThreadMarker>,
        out: &mut Vec<SessionEvent>,
    ) {
        let nodes: Vec<NodeKey> = self
            .graph
            .plugins
            .iter()
            .filter(|(_, hosted)| hosted.rebind.is_some() && !hosted.silenced)
            .map(|(node, _)| *node)
            .collect();
        for node in nodes {
            let Some(hosted) = self.graph.plugins.get_mut(&node) else {
                continue;
            };
            if let Some(main) = main {
                hosted.handle.editor_idle(main);
            }
            let Some(state) = hosted.rebind.take() else {
                continue;
            };
            let state = self.rebind_step(node, state, main, out);
            if let Some(hosted) = self.graph.plugins.get_mut(&node) {
                hosted.rebind = Some(state);
            }
        }
    }

    /// One update of the rebuild of one hosted plugin that keeps a rebinder:
    /// the record it keeps next.
    fn rebind_step(
        &mut self,
        node: NodeKey,
        state: RebindState,
        main: Option<MainThreadMarker>,
        out: &mut Vec<SessionEvent>,
    ) -> RebindState {
        let (rebinder, laid_out) = match &state {
            RebindState::Settling { rebinder, .. } => match rebinder.settled() {
                Settlement::Pending => return state,
                // Moved: the Processor is in the new node. Lost: the plugin
                // died. Idle: nothing was moved.
                Settlement::Idle | Settlement::Moved | Settlement::Lost => {
                    let laid_out = state.laid_out().clone();
                    (rebinder.clone(), laid_out)
                }
            },
            RebindState::Idle { rebinder, laid_out } => (rebinder.clone(), laid_out.clone()),
        };
        if let Some(main) = main {
            rebinder.tick(main);
        }
        let Some(now) = rebinder.layout() else {
            return state.with(laid_out, false);
        };
        if now == laid_out {
            return state.with(laid_out, false);
        }
        match rebinder.prepare() {
            Ok(rebuilt) => {
                // The swap commits on the main thread; an update that is not on
                // it asks again, the unplaced node dropped.
                let Some(main) = main else {
                    return state.with(laid_out, false);
                };
                let (bound, controls, commit) = rebuilt.into_parts();
                if let Some(controls) = &controls {
                    controls.set_meter(self.transport.settings().project_meter().clone());
                }
                match self.graph.replace_rebound(node, bound) {
                    Ok(()) => {
                        // The plugin is touched only now the node is in place.
                        commit.commit(main);
                        if let (Some(hosted), Some(controls)) =
                            (self.graph.plugins.get_mut(&node), controls)
                        {
                            hosted.set_node(controls);
                        }
                        state.with(now, true)
                    }
                    // Between a re-prepare's two commits: asked again next update.
                    Err(ReplaceRefused::Busy(_)) => state.with(laid_out, false),
                    Err(ReplaceRefused::Failed(why)) => {
                        out.push(refused(node, PortsRefusal::Swap(why)));
                        state.with(now, false)
                    }
                }
            }
            // Its own restart is not over: asked again at the next update.
            Err(RebindError::Restarting | RebindError::NotRunning | RebindError::NoPorts) => {
                state.with(laid_out, false)
            }
            Err(why) => {
                out.push(refused(node, PortsRefusal::Ports(why)));
                state.with(now, false)
            }
        }
    }
}

fn refused(node: NodeKey, why: PortsRefusal) -> SessionEvent {
    SessionEvent::Plugin(PluginEvent {
        node,
        change: PluginChange::PortsRefused { why },
    })
}
