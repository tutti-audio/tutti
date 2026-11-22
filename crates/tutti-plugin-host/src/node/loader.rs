//! Construct the `(InProcessClient<L, F>, PluginHandle)` pair for an in-process
//! plugin of format `L`.

use std::sync::Arc;
use tutti_buffers::{Float, FloatWidth};
use tutti_plugin_types::{MainThreadMarker, MainWorkHandle};
use tutti_rt::PendingSampleRate;

use tutti_rt::Mutex;

use super::client::{Declared, InProcessClient, Reload};
use super::control_backend::{ControlBackend, Reporting};
use super::instance::Instance;
use super::width_error::WidthError;
use crate::PluginHandle;
use crate::{ParameterChangeSink, PluginControls};
use tutti_plugin_types::{
    Features, InProcess, Loaded, PluginDescriptor, PluginRef, PluginTooWide, PluginWiring, Prepare,
};
use tutti_plugin_types::{IpcError, IpcResult, StateError};
use tutti_units::ParamId;

/// The maximum block size the plugin is told (`Prepare::max_block`). A graph
/// block longer than this (the engine renders up to 8192, more with
/// `Engine::with_capacity`) is split by the Processor into calls of at most
/// this length, so the plugin is never entered with more frames than it was
/// told. Splitting rather than re-announcing the graph's maximum: the
/// announcement is a reconfiguration that belongs on the main thread and
/// reaches a live node only through the editor pump, so it cannot be relied
/// on to have happened before the first block.
const MAX_BLOCK_SIZE: usize = 4096;

/// Why [`load_client`] returned no client. `Debug` is written by hand so that
/// the format `L` need not be `Debug`.
#[derive(thiserror::Error)]
pub enum LoadClientError<L: Loaded + 'static> {
    /// The plugin did not load.
    #[error(transparent)]
    Failed(#[from] IpcError),
    /// The plugin does not process the width asked for. It is loaded and
    /// activated at 32-bit: [`WidthError::into_inner`] returns it, with its
    /// handle, to host in a 32-bit graph (boxed, to keep the error small).
    #[error(transparent)]
    Width(Box<WidthError<(InProcessClient<L, f32>, PluginHandle)>>),
}

impl<L: Loaded + 'static> std::fmt::Debug for LoadClientError<L> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(e) => f.debug_tuple("Failed").field(e).finish(),
            Self::Width(e) => f.debug_tuple("Width").field(e).finish(),
        }
    }
}

impl<L: Loaded + 'static> LoadClientError<L> {
    /// The failure as a [`IpcError`], for a host of one width, where a
    /// width refusal cannot come: the load's own failure, or the width refusal.
    #[must_use]
    pub fn into_bridge(self) -> IpcError {
        match self {
            Self::Failed(e) => e,
            Self::Width(e) => IpcError::WidthRefused {
                requested: e.requested(),
                widths: e.widths(),
            },
        }
    }
}

/// Loads a plugin of format `L` in the host process, activated at the width
/// of the graph `F` it will be a node of, returning its client and control
/// handle.
///
/// `tutti-plugin`'s `Plugin::open` calls this for `.vst` files
/// when the `vst2` feature is on; call it directly for another [`InProcess`]
/// format, or to get the concrete [`InProcessClient`] type. `plugin` names the
/// file and, in one that holds several, the plugin in it. Insert the client's node
/// ([`bind`](InProcessClient::bind), then `Editor::insert`). The node and handle
/// share the plugin
/// instance; drop both to drop the plugin. Editors are opened through the
/// handle, and audio runs on whatever thread the graph's executor runs on.
/// `work` is the collector of the plugin's owner: what an off-main call through
/// the handle waits on, and where a drop off the main thread hands its close.
/// Blocks while the plugin loads, so call it off the audio thread.
///
/// The width is [`Widths::negotiate`](tutti_plugin_types::Widths::negotiate) of `F::WIDTH`, once, at the activation.
///
/// # Errors
///
/// [`LoadClientError::Failed`] when the library cannot be opened or the plugin
/// fails to initialize; [`LoadClientError::Width`] when the plugin does not
/// process `F`'s width, which never happens for `f32`.
pub fn load_client<L, F>(
    main: MainThreadMarker,
    work: &MainWorkHandle,
    plugin: &PluginRef,
    sample_rate: impl Into<tutti_units::SampleRate>,
) -> Result<(InProcessClient<L, F>, PluginHandle), LoadClientError<L>>
where
    L: InProcess + 'static,
    L::Processor: 'static,
    F: Float,
{
    load_client_restoring(main, work, plugin, sample_rate, None)
        .map(|(client, handle, _)| (client, handle))
}

/// [`load_client`], with `restore` run on the plugin before it is activated
/// (see the instance's `load`). The outcome is returned as the third value;
/// a width refusal carries the plugin without it.
///
/// # Errors
///
/// As [`load_client`].
pub fn load_client_restoring<L, F>(
    main: MainThreadMarker,
    work: &MainWorkHandle,
    plugin: &PluginRef,
    sample_rate: impl Into<tutti_units::SampleRate>,
    restore: Option<&mut dyn FnMut(&mut L) -> std::result::Result<(), StateError>>,
) -> Result<
    (
        InProcessClient<L, F>,
        PluginHandle,
        Option<std::result::Result<(), StateError>>,
    ),
    LoadClientError<L>,
>
where
    L: InProcess + 'static,
    L::Processor: 'static,
    F: Float,
{
    let sample_rate = sample_rate.into();
    let (inner, rt, restored) = Instance::<L>::load(
        main,
        work,
        plugin,
        sample_rate,
        MAX_BLOCK_SIZE,
        F::WIDTH,
        restore,
    )?;
    let (settled, widths) = (inner.width(), inner.widths());
    let origin = inner.origin().cloned().map(|origin| Reload {
        origin,
        work: work.clone(),
    });
    if settled == F::WIDTH {
        let (client, handle) = assemble::<L, F>(main, work, inner, rt, origin, sample_rate)?;
        return Ok((client, handle, restored));
    }
    // `negotiate` only lowers a request to 32-bit: the plugin runs at `f32`.
    let plugin = assemble::<L, f32>(main, work, inner, rt, origin, sample_rate)?;
    Err(LoadClientError::Width(Box::new(WidthError::new(
        plugin,
        F::WIDTH,
        widths,
    ))))
}

/// The client and handle over a plugin activated at `F`'s width.
fn assemble<L, F>(
    main: MainThreadMarker,
    work: &MainWorkHandle,
    inner: Instance<L>,
    rt: tutti_rt::Rt<L::Processor>,
    origin: Option<Reload>,
    sample_rate: tutti_units::SampleRate,
) -> IpcResult<(InProcessClient<L, F>, PluginHandle)>
where
    L: InProcess + 'static,
    L::Processor: 'static,
    F: Float,
{
    let Wired {
        mut client,
        inner,
        pending_sample_rate,
        post,
        descriptor,
        loaded,
    } = wire(main, inner, rt, origin, sample_rate, 0)?;
    let param_sink = ParameterChangeSink::new();
    let invalidate_sink = crate::InvalidateSink::new();
    let transport_sink = crate::TransportSink::new();

    let reporting = Reporting {
        invalidate: invalidate_sink.clone(),
        transport: transport_sink.clone(),
    };
    // A pump the node's rebinder runs reports through the handle's listeners too.
    client.reporting = Some(reporting.clone());
    let lost = inner.lock().lost_latch();
    let backend = Arc::new(ControlBackend {
        inner: Arc::clone(&inner),
        param_sink: param_sink.clone(),
        pending_sample_rate: Arc::clone(&pending_sample_rate),
        edits: post,
        reporting,
        lost,
        format: descriptor.class.format_name(),
        work: work.clone(),
    });

    // The one backend serves all three slots, so a mode set through the handle
    // reaches the very instance the node renders.
    let control: Arc<dyn crate::MainThreadControl> = backend.clone();
    let handle = PluginHandle::from_backend(backend, Some(control), descriptor, loaded, param_sink)
        .with_invalidate_sink(invalidate_sink)
        .with_transport_sink(transport_sink);

    Ok((client, handle))
}

/// What [`wire`] builds around an activated plugin.
struct Wired<L: Loaded, F: Float> {
    client: InProcessClient<L, F>,
    /// The control side the client and a handle both reach.
    inner: Arc<Mutex<Instance<L>>>,
    /// Where the audio side parks a rate for the main thread's pump; drained
    /// by the handle's backend.
    pending_sample_rate: Arc<PendingSampleRate>,
    /// Where the backend posts the host's parameter writes the client drains.
    post: super::host_edits::EditPost,
    descriptor: PluginDescriptor,
    /// The wiring as the plugin reports it, with the presets it can list.
    loaded: PluginWiring,
}

/// The client over an activated plugin: its declared widths, latency, tail and
/// parameter addresses read now, and the cells the node and a handle share.
/// `spares` is how many parameter addresses beyond the plugin's list the
/// node's delivery may learn.
fn wire<L, F>(
    main: MainThreadMarker,
    mut inner: Instance<L>,
    rt: tutti_rt::Rt<L::Processor>,
    origin: Option<Reload>,
    sample_rate: tutti_units::SampleRate,
    spares: usize,
) -> IpcResult<Wired<L, F>>
where
    L: Loaded + 'static,
    L::Processor: 'static,
    F: Float,
{
    let Some(control) = inner.control() else {
        return Err(IpcError::LostInActivation);
    };
    let descriptor = control.info().clone();
    // What the format's wiring declares, asked of the plugin now: its widths,
    // latency, tail and the capabilities the node gates its per-block sends
    // on. The snapshot of the transport is the host's to give, not the
    // plugin's to request, and the node honours the claim by reading it from
    // the block's `Cx` into each `ProcessContext`.
    let mut loaded = control.wiring(main);
    let parameters = control.parameters(main);
    // A plugin with presets can be listed and sent one. A format whose
    // wiring already says so keeps it; one with none says nothing either
    // way, so a plugin with none is not marked as declining them.
    if !control.presets(main).is_empty() {
        loaded.features.insert(Features::PRESET_LIST);
        loaded.features.insert(Features::PRESET_LOAD);
    }
    // A plugin wider than a block serves is refused here, so no node is built
    // over it.
    let declared = Declared::of(&loaded)?;
    let inner = Arc::new(Mutex::new(inner));
    // Built here rather than inside the node so the node's producer end and the
    // backend's drain end are the same cell: the audio thread parks a rate in
    // it, `editor_idle` dispatches from it.
    //
    // A pending rate change, and the re-reads of latency, tail and pin counts
    // (`Instance::pump`, raised as `PluginInvalidation` through the handle's
    // `on_invalidate`), run on the main thread's tick: `PluginHandle::editor_idle`
    // or the node's `Rebinder::tick`, which the session calls at each update.
    let pending_sample_rate = Arc::new(PendingSampleRate::new());
    // `loaded.features`, not a local copy, so the node gates its per-block
    // transport send on the exact value the handle reports. One value, read
    // twice, cannot drift into a declared-but-undelivered capability.
    let ids: Vec<ParamId> = parameters.iter().map(|p| p.id()).collect();
    let (post, port) = super::host_edits::host_edits(ids.iter().copied());
    let mut client = InProcessClient::new(
        Arc::clone(&inner),
        rt,
        descriptor.clone(),
        origin,
        declared,
        loaded.features,
        sample_rate,
        Arc::clone(&pending_sample_rate),
        None,
        ids.iter().copied(),
        spares,
        None,
        None,
        None,
    );
    client.edits = Some(port);
    Ok(Wired {
        client,
        inner,
        post,
        pending_sample_rate,
        descriptor,
        loaded,
    })
}

/// Why [`client_over`] built no client.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientOverError {
    /// The wiring has more channels in a direction than a block serves.
    #[error(transparent)]
    TooWide(#[from] PluginTooWide),
    /// The plugin was activated at one width and the client is for a graph of
    /// another.
    #[error("the plugin was activated at {activated:?}, not the {hosted:?} of the graph")]
    Width {
        /// The width the Processor was activated at.
        activated: FloatWidth,
        /// The width of the graph the client was asked for.
        hosted: FloatWidth,
    },
}

/// A client over a plugin the caller activated itself, with no main thread
/// ([`Lifecycle::adopt`](tutti_plugin_types::Lifecycle::adopt)): the generic
/// node for a hosting whose activation reads no token, as `tutti-plugin`
/// builds one for an out-of-process plugin (`Ipc::activate_anywhere`). What a graph
/// node's `prepare` then follows is the rate and block it was activated at.
///
/// `wiring` is what the plugin reports now; `parameters` are the addresses it
/// lists, which the node's delivery has a slot for, and `spares` is how many
/// it did not list the delivery may learn. The client has no origin (an out-of-process
/// plugin reopens with `Ipc::reopen_restoring`) and no handle: its control is
/// the hosting's. With `controls`, the node's meter, parameter counters and
/// latency figures are those of the out-of-process plugin's [`PluginControls`], and
/// `retune` keeps its pipeline chunk and rate current.
///
/// `F` is the width of the graph the client's node will join, and `prepare`
/// must have activated the plugin at it ([`Prepare::format`]): the halves are
/// the caller's activation, so a plugin of another width is no `F` client.
///
/// # Errors
///
/// [`ClientOverError::TooWide`] when `wiring` has more channels in a direction
/// than a block serves; [`ClientOverError::Width`] when `prepare` is not at
/// `F`'s width. No node is built over it, and the halves are dropped.
pub fn client_over<L, F>(
    processor: L::Processor,
    controller: L::Controller,
    prepare: Prepare,
    descriptor: PluginDescriptor,
    wiring: &PluginWiring,
    parameters: impl IntoIterator<Item = ParamId>,
    spares: usize,
    controls: Option<PluginControls>,
) -> Result<InProcessClient<L, F>, ClientOverError>
where
    L: Loaded + 'static,
    L::Processor: 'static,
    F: Float,
{
    if prepare.format() != F::WIDTH {
        return Err(ClientOverError::Width {
            activated: prepare.format(),
            hosted: F::WIDTH,
        });
    }
    let declared = Declared::of(wiring)?;
    let (inner, rt) = Instance::<L>::adopt(processor, controller, prepare, wiring);
    Ok(InProcessClient::new(
        Arc::new(Mutex::new(inner)),
        rt,
        descriptor,
        None,
        declared,
        wiring.features,
        prepare.rate(),
        Arc::new(PendingSampleRate::new()),
        None,
        parameters,
        spares,
        controls,
        None,
        None,
    ))
}

impl ClientOverError {
    /// The failure as the [`IpcError`] a load reports.
    #[must_use]
    pub fn into_bridge(self) -> IpcError {
        match self {
            Self::TooWide(wide) => IpcError::TooWide(wide),
            Self::Width { activated, hosted } => IpcError::WidthMismatch { activated, hosted },
        }
    }
}
