//! A pooled plugin's node re-inserted when the plugin changes its ports.
//!
//! `Phase::Plugins` compares the ports a pooled plugin has now with the ones
//! its node declares. When they differ it asks the plugin's
//! [`PluginRebinder`](tutti_plugin::catalog::PluginRebinder) for a node over the
//! same plugin and swaps it in at the same key
//! ([`Graph::replace_rebound`]): the wiring to ports the new shape keeps stays,
//! and the wiring to ports it lacks is reported as
//! [`SessionEvent::Unwired`](crate::SessionEvent::Unwired) by the update that
//! commits the swap. The plugin keeps running; its Processor moves to the new
//! node at a later tick.
//!
//! The node the swap retires may still hold the Processor. The rebuild raised a
//! recall, and a node dropped while it holds the Processor and a recall is
//! raised hands it back to the control side (`InProcClient`'s `Drop`), so the
//! retirement loses nothing. The record keeps the rebuild `settling` until the
//! rebinder says [`Moved`](tutti_plugin_host::Settlement::Moved), or
//! [`Lost`](tutti_plugin_host::Settlement::Lost) when the plugin died (which the
//! plugin's own path reports), and starts no second rebuild before.
//!
//! An in-process plugin changes its ports on the main thread and is ticked
//! there, so the rebuild runs in an update that runs on it. A served
//! plugin's ports are fixed by its slab: a change of width is
//! [`PluginChange::PortsRefused`] and its node stays.

use tutti_plugin::catalog::Rebuilt;
use tutti_plugin_host::{RebindError, Settlement};
use tutti_plugin_types::MainThreadMarker;

use super::{PluginChange, PluginEvent};
use crate::{Clock, Session, SessionEvent};
use tutti_graph::NodeKey;

impl<C: Clock> Session<C> {
    /// Ticks the in-process pooled plugins (their pump is what reads a change
    /// of ports) and re-inserts the node of each whose ports changed. Called at
    /// the end of `Phase::Plugins`.
    pub(crate) fn rebind_pooled_plugins(&mut self, out: &mut Vec<SessionEvent>) {
        let main = MainThreadMarker::new();
        let nodes: Vec<NodeKey> = self
            .graph
            .plugins
            .iter()
            .filter(|(_, hosted)| {
                hosted.pooled.is_some() && hosted.rebind.is_some() && !hosted.silenced
            })
            .map(|(node, _)| *node)
            .collect();
        for node in nodes {
            let Some(hosted) = self.graph.plugins.get_mut(&node) else {
                continue;
            };
            // The pump reads what the plugin changed about itself, and runs the
            // restart that follows; nothing else ticks an in-process plugin.
            if let (Some(main), None) = (main, hosted.handle.server_pid()) {
                hosted.handle.editor_idle(main);
            }
            let Some(state) = hosted.rebind.as_mut() else {
                continue;
            };
            if state.settling {
                match state.rebinder.settled() {
                    Settlement::Pending => continue,
                    // Moved: the Processor is in the new node. Lost: the plugin
                    // died, which its own path reports; the nodes stay silent.
                    Settlement::Moved | Settlement::Lost => state.settling = false,
                }
            }
            let Some(now) = state.rebinder.layout() else {
                continue;
            };
            if state.laid_out.as_ref() == Some(&now) {
                continue;
            }
            let rebinder = state.rebinder.clone();
            let Some(main) = main else {
                continue;
            };
            // Nothing refuses the swap once the Processor is on its way to the
            // node, so it is asked first.
            if !self.graph.can_swap(node) {
                continue;
            }
            let done = match rebinder.rebind(main) {
                Ok(Some(Rebuilt { bound, controls })) => {
                    controls.set_meter(self.click_settings.project_meter());
                    if let Err(refused) = self.graph.replace_rebound(node, bound) {
                        tracing::error!("the rebuilt node of {node:?} was refused: {refused}");
                        continue;
                    }
                    Done::Rebuilt(controls)
                }
                // A served plugin whose ports still fit its slab: its node stays.
                Ok(None) => Done::Kept,
                // Its own restart is not over: asked again at the next update.
                Err(RebindError::Restarting | RebindError::NotRunning | RebindError::NoPorts) => {
                    continue;
                }
                Err(why @ (RebindError::OutsideSlab { .. } | RebindError::TooWide(_))) => {
                    out.push(SessionEvent::Plugin(PluginEvent {
                        node,
                        change: PluginChange::PortsRefused { why },
                    }));
                    Done::Kept
                }
            };
            let Some(hosted) = self.graph.plugins.get_mut(&node) else {
                continue;
            };
            if let Some(state) = hosted.rebind.as_mut() {
                state.laid_out = Some(now);
                state.settling = matches!(done, Done::Rebuilt(_));
            }
            if let Done::Rebuilt(controls) = done {
                hosted.node = Some(controls);
            }
        }
    }
}

/// What a rebind did.
enum Done {
    /// A node over the new ports was swapped in, with its controls.
    Rebuilt(tutti_plugin_host::NodeControls),
    /// The node stays.
    Kept,
}
