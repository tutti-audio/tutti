//! A hosted plugin's diagnostics and its buses: the counts and figures a host shows beside a
//! plugin, and the graph's
//! bus handles for wiring it.
//!
//! Neither depends on where the plugin runs: a figure that exists only out of
//! process (a server's pid) is `None`, a count that cannot happen in process
//! (a pipeline's missed chunks) is zero, and everything else reads the same.

use tutti_graph::{BusRef, BusSource, MAX_BUSES};
use tutti_plugin_types::{BlockCounts, BusRole, PluginBus, TerminationReason};
use tutti_units::{Samples, Tail};

use crate::plugin::HostedPlugin;

/// What the graph holds of a plugin's node: the figures a view is made with.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Figures {
    pub(crate) latency: Samples,
    pub(crate) tail: Tail,
}

/// A hosted plugin's counts and figures, lent by
/// [`Plugin::diagnostics`](crate::Plugin::diagnostics).
///
/// What a host shows in a plugin's corner (its server, its dropouts, its
/// latency), kept off the main surface. What it lost, in events, is
/// [`Plugin::losses`](crate::Plugin::losses).
#[derive(Debug, Clone, Copy)]
pub struct Diagnostics<'a> {
    hosted: &'a HostedPlugin,
    figures: Figures,
}

impl<'a> Diagnostics<'a> {
    pub(crate) fn new(hosted: &'a HostedPlugin, figures: Figures) -> Self {
        Self { hosted, figures }
    }

    /// The OS process id of the `plugin-server` hosting the plugin, or `None`
    /// for a plugin that runs in this process.
    pub fn server_pid(&self) -> Option<u32> {
        self.hosted.handle.server_pid()
    }

    /// The blocks the audio path gave up waiting for, and the replies it took,
    /// over the plugin's life. What the node itself did not render is
    /// [`block_losses`](Self::block_losses). A plugin that runs in this process has no
    /// replies to wait for: both are zero.
    pub fn block_counts(&self) -> BlockCounts {
        self.hosted.handle.block_counts()
    }

    /// What the node did not render, or the plugin did not take, over the
    /// life of the plugin at this node: blocks with no Processor in hand,
    /// blocks the plugin failed, blocks it wrote a NaN or an infinity in, and
    /// the entries and input messages it refused. Cumulative, and carried
    /// across a rebuild of the node and a swap of the plugin, as
    /// [`Plugin::losses`](crate::Plugin::losses) is. Counted in blocks and
    /// entries, not events (what such a block costs in MIDI is in `losses`),
    /// and in pieces of a block for a plugin that renders in chunks.
    ///
    /// Not [`block_counts`](Self::block_counts), which are the replies a
    /// served plugin's pipeline waited for: those are the pipeline's, these
    /// are the node's own renders, whichever hosting runs the plugin.
    pub fn block_losses(&self) -> tutti_plugin_host::BlockLosses {
        self.hosted.block_losses()
    }

    /// The chunks the plugin's pipeline had not answered by the time the node
    /// played them (each played as silence: a dropout), since the plugin was
    /// bound. Zero for a plugin that runs in this process, which has no
    /// pipeline to be late.
    pub fn missed_chunks(&self) -> u64 {
        self.hosted
            .controls
            .as_ref()
            .map_or(0, tutti_plugin_host::PluginControls::missed_chunks)
    }

    /// Of the [`missed_chunks`](Self::missed_chunks), those the plugin's
    /// pipeline answered after the node had played the silence: a late answer
    /// (an overloaded plugin or machine: raise the buffer size), not a lost
    /// one. Every missed chunk ends as exactly one of late,
    /// [refused](Self::refused_chunks), [skipped](Self::skipped_chunks) or
    /// [unanswered](Self::unanswered_chunks). Zero for a plugin that runs in
    /// this process.
    pub fn late_chunks(&self) -> u64 {
        self.hosted
            .controls
            .as_ref()
            .map_or(0, tutti_plugin_host::PluginControls::late_chunks)
    }

    /// Of the [`missed_chunks`](Self::missed_chunks), those the plugin's
    /// server refused: it did not publish the audio (the chunk's ring slot
    /// recycled before it ran, a block that did not fit, an output channel not
    /// written, or a refused rate). Zero for a plugin that runs in this
    /// process.
    pub fn refused_chunks(&self) -> u64 {
        self.hosted
            .controls
            .as_ref()
            .map_or(0, tutti_plugin_host::PluginControls::refused_chunks)
    }

    /// Of the [`missed_chunks`](Self::missed_chunks), those that are neither
    /// late, refused nor skipped: the server never said what became of them. A
    /// loss no counter explains. Zero for a plugin that runs in this process.
    pub fn unanswered_chunks(&self) -> u64 {
        self.hosted
            .controls
            .as_ref()
            .map_or(0, tutti_plugin_host::PluginControls::unanswered_chunks)
    }

    /// Chunks dropped because the plugin fell more than one chunk behind: the
    /// pipeline never sent them, so they played as silence. Raise the buffer
    /// size if it grows. Zero for a plugin that runs in this process.
    pub fn skipped_chunks(&self) -> u64 {
        self.hosted
            .controls
            .as_ref()
            .map_or(0, tutti_plugin_host::PluginControls::skipped_chunks)
    }

    /// Commands the audio side could not hand to the plugin's bridge (a full
    /// command queue, or no payload for a block): lost, and a refused block
    /// plays as silence. Zero for a plugin that runs in this process.
    pub fn refused_commands(&self) -> u64 {
        self.hosted
            .controls
            .as_ref()
            .map_or(0, tutti_plugin_host::PluginControls::refused_commands)
    }

    /// Audio responses the plugin's bridge could not queue for the audio thread: lost. Zero for a plugin that runs in this process.
    pub fn lost_responses(&self) -> u64 {
        self.hosted
            .controls
            .as_ref()
            .map_or(0, tutti_plugin_host::PluginControls::lost_responses)
    }

    /// Parameter-change buffers the audio thread leaked rather than free (a broken sizing bound). Zero for a plugin that runs in this process.
    pub fn leaked_buffers(&self) -> u64 {
        self.hosted
            .controls
            .as_ref()
            .map_or(0, tutti_plugin_host::PluginControls::leaked_buffers)
    }

    /// Transfers between the plugin and its editor instance that were refused: a parameter write the mirror dropped (the plugin still got it; the open editor's display missed it), an edit or a state one side did not take (an editor opened at the plugin's defaults among them). Zero for a plugin that runs in this process.
    pub fn mirror_writes_refused(&self) -> u64 {
        self.hosted
            .controls
            .as_ref()
            .map_or(0, tutti_plugin_host::PluginControls::mirror_writes_refused)
    }

    /// Requests the plugin's editor instance made that were dropped: every request but a transport request (a restart, an I/O change, a parameter rescan, a latency or tail change, a reload, a callback, a MIDI-CC or note-expression change, a timer or a file descriptor), which the plugin's audio instance cannot be told. Zero for a plugin that runs in this process.
    pub fn editor_requests_dropped(&self) -> u64 {
        self.hosted.controls.as_ref().map_or(
            0,
            tutti_plugin_host::PluginControls::editor_requests_dropped,
        )
    }

    /// The latency the graph compensates for this plugin's node, as of the last
    /// update: the plugin's own, and out of process one chunk of the pipeline
    /// that carries its audio.
    pub fn latency(&self) -> Samples {
        self.figures.latency
    }

    /// The latency the plugin itself reports, before the graph takes it and
    /// without the pipeline's chunk: what [`latency`](Self::latency) is made of.
    /// Out of process it is the figure the bridge last heard from the plugin,
    /// so it moves the moment the plugin says so (the graph's moves at the next
    /// update); in process there is no pipeline and the node declares the
    /// plugin's figure, so it is [`latency`](Self::latency).
    pub fn reported_latency(&self) -> Samples {
        self.hosted.controls.as_ref().map_or(
            self.figures.latency,
            tutti_plugin_host::PluginControls::latency,
        )
    }

    /// Whether the chunk the node submitted last, and has not played yet, has
    /// been answered (`true` with none in flight, and always in process).
    ///
    /// For a host driving the graph by hand, block by block, with no device
    /// clock: it waits on this before the next block, so that block plays the
    /// answer and not a dropout ([`missed_chunks`](Self::missed_chunks)). A
    /// live host does not wait: the device's callback period is the deadline.
    pub fn chunk_in_flight_answered(&self) -> bool {
        self.hosted
            .controls
            .as_ref()
            .is_none_or(tutti_plugin_host::PluginControls::chunk_in_flight_answered)
    }

    /// The tail the graph holds for the node, as of the last update: what the
    /// plugin reports, which may be unbounded or not yet reported.
    pub fn tail(&self) -> Tail {
        self.figures.tail
    }

    /// Whether the node follows the project's meter (the time signature and bar
    /// it tells the plugin). True for every plugin the session added; false for
    /// one a host inserted itself
    /// ([`Graph::insert_plugin`](crate::Graph::insert_plugin)), whose node the
    /// session did not give a meter.
    pub fn follows_project_meter(&self) -> bool {
        self.hosted.node.has_meter()
    }

    /// Ends the plugin's server process: the death follows as it would for any
    /// crash ([`PluginHealth::Died`](crate::PluginHealth::Died), with
    /// `reason` as its cause), and recovery, if on, reloads it. Nothing for a
    /// plugin that runs in this process. Not for the audio thread.
    pub fn terminate(&self, reason: TerminationReason) {
        self.hosted.handle.terminate(reason);
    }
}

/// A plugin's buses as the graph's bus handles, lent by
/// [`Plugin::buses`](crate::Plugin::buses): what to wire with
/// `Graph::connect` ([`BusSource`] into [`BusRef`]).
///
/// `connect(kick.bus_out(0), comp.buses().input(1).unwrap())` feeds a plugin's
/// second input bus: no loader marks a sidechain (CLAP and AU have no marker and
/// VST3 says aux), so an app wires a key input by index. A bus index here is the graph's bus index, the plugin's own bus
/// order, so each handle is the one `key.bus_in(i)` and `key.bus_out(i)` name.
/// The graph tracks [`MAX_BUSES`] of a side: a plugin with more has its later
/// buses reached by port.
#[derive(Debug, Clone, Copy)]
pub struct PluginBuses<'a> {
    node: tutti_graph::NodeKey,
    inputs: &'a [PluginBus],
    outputs: &'a [PluginBus],
}

impl<'a> PluginBuses<'a> {
    pub(crate) fn new(node: tutti_graph::NodeKey, hosted: &'a HostedPlugin) -> Self {
        Self::over(
            node,
            hosted.handle.input_buses(),
            hosted.handle.output_buses(),
        )
    }

    fn over(node: tutti_graph::NodeKey, inputs: &'a [PluginBus], outputs: &'a [PluginBus]) -> Self {
        Self {
            node,
            inputs,
            outputs,
        }
    }

    /// Bus `index` of this node (below [`MAX_BUSES`], so it fits the `u8`).
    fn bus(&self, index: usize) -> BusRef {
        BusRef::new(self.node, index as u8)
    }

    /// The main input bus, if the plugin has an input.
    pub fn main_input(&self) -> Option<BusRef> {
        let index = self
            .inputs
            .iter()
            .position(|bus| bus.role == BusRole::Main)?;
        self.input(index)
    }

    /// Input bus `index`, in the plugin's bus order.
    pub fn input(&self, index: usize) -> Option<BusRef> {
        (index < self.inputs.len().min(MAX_BUSES)).then(|| self.bus(index))
    }

    /// Output bus `index`, in the plugin's bus order: the main bus is the one
    /// the plugin marks main, and the rest are its extra outputs.
    pub fn output(&self, index: usize) -> Option<BusSource> {
        (index < self.outputs.len().min(MAX_BUSES)).then(|| BusSource::Node(self.bus(index)))
    }

    /// How many input buses the graph tracks.
    pub fn input_count(&self) -> usize {
        self.inputs.len().min(MAX_BUSES)
    }

    /// How many output buses the graph tracks.
    pub fn output_count(&self) -> usize {
        self.outputs.len().min(MAX_BUSES)
    }
}
