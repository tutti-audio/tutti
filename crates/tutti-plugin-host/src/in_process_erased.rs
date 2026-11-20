//! An in-process plugin's client, node and rebinder behind its format.
//!
//! [`InProcessClient<L>`], [`PluginNode<L>`] and [`Rebinder<L>`] are generic over the
//! format's [`Loaded`](tutti_plugin_types::Loaded). A host that holds a plugin of any format keeps these
//! erased: one box over a private trait, written once here, instead of an enum
//! with an arm per format and a macro that lists them.

use crate::{
    InProcessClient, NodeControls, PluginNode, PluginTimeSignature, RebindError, Rebinder,
    Settlement,
};
use tutti_graph::{IntoNode, NodeParts};
use tutti_plugin_types::{InProcess, Layout, MainThreadMarker, RenderMode};
use tutti_timeline::TempoReader;

/// What a loaded in-process client offers a host, whatever its format.
trait ClientOps: Send {
    fn set_render_mode(&self, mode: RenderMode) -> bool;
    fn controls(&self) -> NodeControls;
    fn bind(self: Box<Self>, tempo: TempoReader) -> (InProcessNode, InProcessRebinder);
}

impl<L> ClientOps for InProcessClient<L>
where
    L: InProcess + 'static,
    L::Processor: 'static,
{
    fn set_render_mode(&self, mode: RenderMode) -> bool {
        InProcessClient::set_render_mode(self, mode)
    }

    fn controls(&self) -> NodeControls {
        InProcessClient::controls(self)
    }

    fn bind(self: Box<Self>, tempo: TempoReader) -> (InProcessNode, InProcessRebinder) {
        let rebinder = InProcessRebinder(Box::new(self.rebinder(tempo.clone())));
        (InProcessNode(Box::new((*self).bind(tempo))), rebinder)
    }
}

/// An in-process plugin's loaded client, whatever its format.
pub struct AnyInProcessClient(Box<dyn ClientOps>);

impl AnyInProcessClient {
    /// Erases the format of `client`.
    #[must_use]
    pub fn new<L>(client: InProcessClient<L>) -> Self
    where
        L: InProcess + 'static,
        L::Processor: 'static,
    {
        Self(Box::new(client))
    }

    /// As [`InProcessClient::set_render_mode`].
    pub fn set_render_mode(&self, mode: RenderMode) -> bool {
        self.0.set_render_mode(mode)
    }

    /// As [`InProcessClient::controls`].
    #[must_use]
    pub fn controls(&self) -> NodeControls {
        self.0.controls()
    }

    /// The node's meter cell.
    #[must_use]
    pub fn meter(&self) -> PluginTimeSignature {
        self.0.controls().meter()
    }

    /// As [`InProcessClient::bind`], and the [`InProcessRebinder`] over the same
    /// plugin, binding its successors to `tempo` too.
    #[must_use]
    pub fn bind(self, tempo: TempoReader) -> (InProcessNode, InProcessRebinder) {
        self.0.bind(tempo)
    }
}

/// An in-process plugin's graph node, whatever its format.
trait NodeOps: Send {
    fn into_parts(self: Box<Self>) -> NodeParts<NodeControls>;
}

impl<L> NodeOps for PluginNode<L>
where
    L: InProcess + 'static,
    L::Processor: 'static,
{
    fn into_parts(self: Box<Self>) -> NodeParts<NodeControls> {
        IntoNode::into_parts(*self)
    }
}

/// A bound in-process plugin: the graph node of [`AnyInProcessClient::bind`], or
/// one a rebinder built.
pub struct InProcessNode(Box<dyn NodeOps>);

impl InProcessNode {
    /// The node, its controls and its fork source, as [`IntoNode::into_parts`]
    /// of a [`PluginNode`].
    #[must_use]
    pub fn into_parts(self) -> NodeParts<NodeControls> {
        self.0.into_parts()
    }
}

/// The move of a plugin's Processor to a node a rebinder built: see
/// [`Pending::commit`](crate::Pending::commit). Dropping it changes nothing.
#[must_use = "a node renders nothing until its move is committed"]
pub struct Commit(Box<dyn FnOnce(MainThreadMarker) + Send>);

impl Commit {
    /// The move that runs `commit`.
    pub fn new(commit: impl FnOnce(MainThreadMarker) + Send + 'static) -> Self {
        Self(Box::new(commit))
    }

    /// Names the node the Processor's way and recalls the Processor from the
    /// node it replaces; see [`Pending::commit`](crate::Pending::commit).
    /// Call it once the node is in the graph: it does not fail.
    pub fn commit(self, main: MainThreadMarker) {
        (self.0)(main);
    }
}

impl std::fmt::Debug for Commit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Commit").finish_non_exhaustive()
    }
}

/// What [`InProcessRebinder::prepare`] built: the node, its controls and the
/// commit.
pub struct InProcessRebuilt {
    /// The node over the plugin's new ports.
    pub node: InProcessNode,
    /// The node's controls.
    pub controls: NodeControls,
    /// The move of the Processor to the node.
    pub commit: Commit,
}

trait RebinderOps: Send {
    fn layout(&self) -> Option<Layout>;
    fn settled(&self) -> Settlement;
    fn prepare(&self) -> Result<InProcessRebuilt, RebindError>;
}

impl<L> RebinderOps for Rebinder<L>
where
    L: InProcess + 'static,
    L::Processor: 'static,
{
    fn layout(&self) -> Option<Layout> {
        Rebinder::layout(self)
    }

    fn settled(&self) -> Settlement {
        Rebinder::settled(self)
    }

    fn prepare(&self) -> Result<InProcessRebuilt, RebindError> {
        let (node, pending) = Rebinder::prepare(self)?;
        let controls = node.controls();
        Ok(InProcessRebuilt {
            node: InProcessNode(Box::new(node)),
            controls,
            commit: Commit::new(move |main| pending.commit(main)),
        })
    }
}

/// An in-process plugin's [`Rebinder`], whatever its format.
pub struct InProcessRebinder(Box<dyn RebinderOps>);

impl InProcessRebinder {
    /// As [`Rebinder::layout`].
    #[must_use]
    pub fn layout(&self) -> Option<Layout> {
        self.0.layout()
    }

    /// As [`Rebinder::settled`].
    #[must_use]
    pub fn settled(&self) -> Settlement {
        self.0.settled()
    }

    /// As [`Rebinder::prepare`].
    ///
    /// # Errors
    ///
    /// The refusals of [`Rebinder::prepare`].
    pub fn prepare(&self) -> Result<InProcessRebuilt, RebindError> {
        self.0.prepare()
    }
}

macro_rules! opaque_debug {
    ($($name:ident),+) => {$(
        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct(stringify!($name)).finish_non_exhaustive()
            }
        }
    )+};
}

opaque_debug!(
    AnyInProcessClient,
    InProcessNode,
    InProcessRebinder,
    InProcessRebuilt
);
