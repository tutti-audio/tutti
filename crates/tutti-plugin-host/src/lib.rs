#![doc = include_str!("../README.md")]
//! The format-generic half of plugin hosting.
//!
//! What a loaded plugin's host side needs whatever process the plugin runs in,
//! and whatever its format: the main-thread control surface
//! ([`PluginHandle`], over a hosting's [`AnyThread`](tutti_plugin_types::AnyThread),
//! [`Health`](tutti_plugin_types::Health) and [`MainThreadControl`]), the
//! node's controls and meter, the transport it tells the plugin and the
//! parameter changes it delivers, the host policy written once over a handle
//! ([`Liveness`], [`PluginParamValues`], [`needs_notify`]), and the
//! in-process node ([`PluginNode`], [`load_client`]) over any
//! [`InProcess`](tutti_plugin_types::InProcess) format.
//!
//! It names no format host, no wire and no server: the process edge is
//! `tutti-plugin-ipc`'s, and `tutti-plugin` puts a served plugin behind these
//! types. Nothing here is a dependency of either.

#![forbid(unsafe_code)]

mod control_handle;
mod controls;
mod fork_state;
mod in_process_erased;
mod listeners;
mod liveness;
mod node;
mod node_losses;
mod param_delivery;
mod param_input;
mod param_values;
mod render_mode;
mod segment_points;
mod time_signature;
mod transport_source;
mod transport_walk;
mod wakeups;
#[cfg(test)]
mod walk_tests;

pub use control_handle::{MainThreadControl, PluginHandle, PluginStatus};
pub use controls::{ForkedSources, NodeControls, PluginControls, PluginReports};
pub use fork_state::ForkState;
pub use in_process_erased::{
    AnyInProcessClient, Commit, InProcessNode, InProcessRebinder, InProcessRebuilt,
};
pub use listeners::{
    InvalidateSink, Listener, ParameterChangeSink, PluginSignal, RefreshSink, Subscription,
    TransportSink,
};
pub use liveness::{Liveness, LivenessVerdict};
pub use node::{
    ClientOverError, InProcessClient, LoadClientError, Pending, PluginNode, RebindError, Rebinder,
    Settlement, WidthError, client_over, load_client, load_client_restoring,
};
pub use node_losses::BlockLosses;
pub use param_delivery::{DeliveryCounters, ParamDelivery, ParamDeliveryStats};
pub use param_values::{ParamRead, ParamReporter, ParamSnapshot, PluginParamValues};
pub use render_mode::needs_notify;
pub(crate) use segment_points::SegmentPoints;
pub use time_signature::PluginTimeSignature;
pub(crate) use transport_source::TransportSource;
pub(crate) use transport_source::{BlockTransport, Snapshot, transport_info};
pub(crate) use transport_walk::Call;
pub(crate) use transport_walk::TransportWalk;
pub use wakeups::service_now;
