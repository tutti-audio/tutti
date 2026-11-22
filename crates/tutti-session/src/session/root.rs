//! The root's declared topology: [`RootTopologyStatus`] says whether the root is as it was asked
//! to be, and the `Session` calls declare it, read it back and place it on the output device.

use tutti_buffers::ChannelTopology;
use tutti_graph::MainOutputError;

use crate::{AppKey, Clock, RootDeclaration, Session};

/// Whether the root is as it was asked to be
/// ([`Session::root_topology_status`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RootTopologyStatus {
    /// In force: the root is what was declared (or nothing was).
    Applied,
    /// Declared and waiting for a running root crossfade to end: the root in
    /// force is the previous declaration, and
    /// [`requested_root_topology`](Session::requested_root_topology) is the
    /// waiting one. It applies in the first update after the fade has ended
    /// (which needs blocks rendered), or is `Refused` if it cannot be placed
    /// then.
    Pending,
    /// Not in force, and why. The previous state stands.
    Refused(MainOutputError),
}

impl<K: AppKey, C: Clock> Session<K, C> {
    /// Declares what the graph root's channels are: the session's output as the
    /// engine and an export place it on the device and in a file.
    /// `Some(`[`ChannelTopology::unassigned`]`)` is independent buses, kept by
    /// index (a 4-wide one on a 6-wide device stays at 0..4, where a bare
    /// 4 is quad and its rears land at 4 and 5); a named topology is speaker
    /// feeds, folded by position. `None` takes the declaration back, and the
    /// root is the engine's order for its width again. **Nothing changes
    /// until one is declared.**
    ///
    /// The declaration states the root, so it **sets the root's width**: the
    /// graph's outputs become exactly as many as `root` has, widened or
    /// narrowed (channels past it are dropped), with no floor of the device's
    /// ([`main_output_width`](crate::main_output_width)). Afterwards the width and the
    /// declaration cannot disagree: [`crate::Graph::widen_outputs`] refuses to widen a
    /// declared root. Declare again to change both.
    ///
    /// A declaration that changes the root crossfades: the old root's outputs
    /// fade out and the new root's fade in over the declick length (5 ms),
    /// both rendering the same nodes, so a change of layout never steps the
    /// sound. [`.unfaded()`](RootDeclaration::unfaded) on the returned declaration
    /// cuts at one block instead.
    ///
    /// A declaration made while a root crossfade runs, and not
    /// [`unfaded`](RootDeclaration::unfaded), waits: it is checked at the call (the
    /// errors below are named here), and held until the running fade ends. The
    /// newest such declaration wins. The first [`update`](Self::update) after
    /// the fade has ended applies it, and the root's width and readers
    /// ([`RootReaderReopened`](crate::SessionEvent::RootReaderReopened)) change in
    /// that update with the audio; until then the root in force, its taps and
    /// [`main_output_topology`](Self::main_output_topology) are the running fade's, and
    /// [`requested_root_topology`](Self::requested_root_topology) is the
    /// waiting one ([`Pending`](RootTopologyStatus::Pending)). It is heard up
    /// to one fade length (5 ms) late, and the fade only ends as blocks render:
    /// with none rendering (a stopped stream, an offline session nobody
    /// pulls) it waits. It stays with the graph through a device restart or a
    /// clock switch, and is checked against the device it finds when it is
    /// applied. A declaration refused at its call drops nothing: the waiting
    /// one still applies. If the
    /// device no longer places it when it is applied,
    /// [`root_topology_status`](Self::root_topology_status) is `Refused` with
    /// the error and the root in force stays. A
    /// [`unfaded`](RootDeclaration::unfaded) declaration applies at once and drops
    /// the waiting one. An export of the master
    /// places the file from the declaration (`tutti_export::Setup::root`, set by the export's fork).
    ///
    /// The device a declaration must place on is the one the session renders
    /// to (the offline `outputs`, or the open device's, and the new one on a
    /// restart); a detached session has none, and the file is checked at the
    /// export.
    ///
    /// # Errors
    ///
    /// [`Error::Output`](crate::Error::Output) when the declaration is empty, wider than the engine
    /// renders ([`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS)), or the
    /// device cannot place it (an unassigned root wider than the device, a channel
    /// no speaker names). Nothing is changed.
    pub fn declare_root_topology(
        &mut self,
        root: Option<ChannelTopology>,
    ) -> crate::Result<RootDeclaration<'_, C::Float>> {
        self.output.declare(&mut self.graph, root)?;
        Ok(RootDeclaration {
            graph: &mut self.graph,
        })
    }

    /// Takes the refusal of a declaration that waited and could not be
    /// applied into `root_refusal`, where a device restart re-attempts it.
    pub(crate) fn sync_root_refusal(&mut self) {
        self.output.sync_refusal(&mut self.graph);
    }

    /// The topology last asked for with
    /// [`declare_root_topology`](Self::declare_root_topology) (or
    /// [`SessionBuilder::main_output_topology`](crate::SessionBuilder::main_output_topology)),
    /// whether or not it is in force: see
    /// [`root_topology_status`](Self::root_topology_status).
    pub fn requested_root_topology(&self) -> Option<&ChannelTopology> {
        self.output.root_request.as_ref()
    }

    /// Whether the root is as it was asked to be. A refusal is a value here,
    /// never only a log line. The first that holds is reported, in this order:
    ///
    /// - [`RootTopologyStatus::Refused`] with the declaration's own
    ///   [`FoldError`](tutti_buffers::FoldError) when the last request could not
    ///   be placed on the device. The previous declaration (the one
    ///   [`main_output_topology`](Self::main_output_topology) reads) stays in force, and a
    ///   **device restart re-attempts the request**: it is
    ///   [`Applied`](RootTopologyStatus::Applied) as soon as the new device
    ///   places it.
    /// - The same when a declaration that waited for a root crossfade
    ///   ([`declare_root_topology`](Self::declare_root_topology)) could not be
    ///   placed once the fade ended; a later widening does not replace it.
    /// - Otherwise `Refused` with [`crate::Graph::widen_outputs`]'s last refusal: a
    ///   widening the declared root could not take
    ///   ([`MainOutputError::Width`]), or
    ///   channels past `MAX_CHANNELS`
    ///   ([`FoldError::Unplaced`](tutti_buffers::FoldError::Unplaced), as a
    ///   [`MainOutputError::Fold`]); that
    ///   is what a host's `MasterSources` longer than the root comes to.
    /// - Otherwise [`Pending`](RootTopologyStatus::Pending) while a declaration
    ///   waits for a root crossfade, and `Applied`.
    ///
    /// "The previous declaration" includes one that is waiting: a declaration
    /// refused at its call drops nothing, so a waiting one still applies.
    pub fn root_topology_status(&self) -> RootTopologyStatus {
        match self
            .output
            .root_refusal
            .or(self.graph.apply_refusal())
            .or(self.graph.widen_refusal())
        {
            Some(e) => RootTopologyStatus::Refused(e),
            None if self.graph.root_pending() => RootTopologyStatus::Pending,
            None => RootTopologyStatus::Applied,
        }
    }

    /// The topology the root was declared with
    /// ([`declare_root_topology`](Self::declare_root_topology)), if one was.
    pub fn main_output_topology(&self) -> Option<&ChannelTopology> {
        self.graph.main_output_topology()
    }

    /// What the output device's channels are as the session folds the root
    /// into them, and whether that was
    /// [`Stated`](tutti_buffers::Placement::Stated)
    /// ([`SessionBuilder::device_topology`](crate::SessionBuilder::device_topology),
    /// `DeviceRestart::topology`) or
    /// [`Default`](tutti_buffers::Placement::Default) from the device's width
    /// alone ([`ChannelTopology::default_for`]). `None` for a session with no
    /// engine (a detached one).
    #[must_use]
    pub fn output_device(&self) -> Option<crate::OutputPlacement> {
        let (_, layout) = self.output.taps.as_ref()?;
        self.graph.output_placement(*layout)
    }
}
